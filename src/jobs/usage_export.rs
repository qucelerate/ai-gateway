//! Usage export: seals queued records into batches, writes each batch to
//! the organization's storage as one gzip NDJSON object, and reports
//! exports that keep failing (D-014).

use std::{io::Write, sync::Arc, time::Duration};

use chrono::Utc;
use flate2::{Compression, write::GzEncoder};
use futures::future::join_all;

use crate::domain::{
    identity::OrganizationId,
    usage_export::{ObjectWriter, WriteError},
};
use crate::persistence::{
    outbox::backoff,
    usage_export::{ClaimedBatch, UsageExportStore},
};

/// Records are sealed once the oldest has waited this long (or a full
/// batch is waiting).
pub const SEAL_AFTER: Duration = Duration::from_secs(5 * 60);
/// Organizations sealed per run.
const SEAL_ORGANIZATIONS: i64 = 100;
/// Batches written at once.
pub const UPLOAD_BATCH: i64 = 8;
/// How long a claimed batch stays invisible to other workers: longer than
/// credentials plus the request timeout.
const LEASE: Duration = Duration::from_secs(5 * 60);
pub const CONTENT_TYPE: &str = "application/gzip";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportReport {
    pub sealed_batches: u64,
    pub claimed: usize,
    pub written: usize,
    pub records_written: u64,
    pub failed: usize,
    pub reported_failing: u64,
}

enum Outcome {
    Written,
    Failed(WriteError),
    /// Discarded or no longer whole since it was claimed.
    Gone,
}

#[derive(Clone)]
pub struct UsageExportRunner {
    store: UsageExportStore,
    writer: Arc<dyn ObjectWriter>,
}

impl UsageExportRunner {
    pub fn new(store: UsageExportStore, writer: Arc<dyn ObjectWriter>) -> Self {
        Self { store, writer }
    }

    pub fn store(&self) -> &UsageExportStore {
        &self.store
    }

    /// Seals what is due, writes one round of due batches, and reports
    /// exports that have failed for an hour.
    pub async fn run_once(&self) -> Result<ExportReport, sqlx::Error> {
        let mut report = ExportReport::default();
        let now = Utc::now();
        let queued_before = now - chrono::Duration::from_std(SEAL_AFTER).unwrap_or_default();
        for organization in self
            .store
            .due_for_sealing(queued_before, SEAL_ORGANIZATIONS)
            .await?
        {
            report.sealed_batches += self.store.seal(organization).await?.batches;
        }
        let uploads = self.write_due().await?;
        report.claimed = uploads.claimed;
        report.written = uploads.written;
        report.records_written = uploads.records_written;
        report.failed = uploads.failed;
        report.reported_failing = self.store.report_failing(Utc::now()).await?;
        Ok(report)
    }

    /// Claims one round of due batches and writes them concurrently.
    pub async fn write_due(&self) -> Result<ExportReport, sqlx::Error> {
        let now = Utc::now();
        let lease_until = now + chrono::Duration::from_std(LEASE).unwrap_or_default();
        let claimed = self.store.claim_due(now, lease_until, UPLOAD_BATCH).await?;
        let outcomes = join_all(claimed.iter().map(|batch| self.write(batch))).await;
        let mut report = ExportReport {
            claimed: claimed.len(),
            ..ExportReport::default()
        };
        for (batch, outcome) in claimed.iter().zip(outcomes) {
            match outcome? {
                Outcome::Written => {
                    if self.store.record_written(batch, Utc::now()).await? {
                        report.written += 1;
                        report.records_written += u64::try_from(batch.records).unwrap_or(0);
                    }
                }
                Outcome::Failed(error) => {
                    let retry_at = Utc::now()
                        + chrono::Duration::from_std(backoff(batch.attempts))
                            .unwrap_or(chrono::Duration::hours(1));
                    self.store
                        .record_failed(batch, error, retry_at, Utc::now())
                        .await?;
                    report.failed += 1;
                }
                Outcome::Gone => {}
            }
        }
        Ok(report)
    }

    async fn write(&self, batch: &ClaimedBatch) -> Result<Outcome, sqlx::Error> {
        let Some(contents) = self.store.contents(batch).await? else {
            return Ok(Outcome::Gone);
        };
        let Ok(body) = ndjson_gzip(&contents.lines) else {
            return Ok(Outcome::Failed(WriteError::Request));
        };
        Ok(
            match self
                .writer
                .write(
                    OrganizationId(batch.organization_id),
                    &contents.destination,
                    &batch.object_name,
                    CONTENT_TYPE,
                    body,
                )
                .await
            {
                Ok(()) => Outcome::Written,
                Err(error) => Outcome::Failed(error),
            },
        )
    }
}

/// One line per record, each ending in a newline, gzipped.
pub fn ndjson_gzip(lines: &[String]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    for line in lines {
        encoder.write_all(line.as_bytes())?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;

    use super::*;

    #[test]
    fn objects_are_newline_terminated_gzip() {
        let body = ndjson_gzip(&["{\"a\":1}".into(), "{\"b\":2}".into()]).unwrap();
        let mut text = String::new();
        GzDecoder::new(body.as_slice())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "{\"a\":1}\n{\"b\":2}\n");
    }
}
