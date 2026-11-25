//! Expires stored trace content: for every organization with content kept,
//! deletes the objects past their period (see
//! `TraceContentStore::expired`) and marks their rows expired. It runs with
//! the hourly sweep, before the retention purge, so a request record is
//! never removed while its content is still stored. A backlog larger than
//! one run's batches drains over later runs.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::{
    application::trace_objects::TraceObjects,
    metrics::{SWEPT_ROWS, global as metrics},
    persistence::trace_content::TraceContentStore,
};

const BATCH: i64 = 100;
/// Batches per organization in one run.
const MAX_BATCHES: usize = 100;

/// What one run did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExpiryReport {
    pub expired: u64,
    /// Organizations whose expiry stopped on an error.
    pub failed: u64,
}

/// Expires content past its period. Without a trace store nothing can be
/// deleted, so nothing is marked: the content stays until a store is
/// configured again.
pub async fn expire_content(
    store: &TraceContentStore,
    objects: Option<&Arc<dyn TraceObjects>>,
    now: DateTime<Utc>,
) -> Result<ExpiryReport, sqlx::Error> {
    let mut report = ExpiryReport::default();
    let organizations = store.organizations_with_content().await?;
    let Some(objects) = objects else {
        if !organizations.is_empty() {
            tracing::warn!(
                "trace content is stored but no trace store is configured; none expired"
            );
        }
        return Ok(report);
    };
    // The request id of every audit event this run writes.
    let run_id = uuid::Uuid::new_v4();
    for organization in organizations {
        report.add(expire_with(store, objects.as_ref(), organization, now, run_id).await);
    }
    if report.expired > 0 {
        metrics().add(&SWEPT_ROWS, &["inference_trace_content"], report.expired);
    }
    Ok(report)
}

/// Expires one organization's content past its period, as
/// [`expire_content`] does for each organization with content.
pub async fn expire_organization_content(
    store: &TraceContentStore,
    objects: &Arc<dyn TraceObjects>,
    organization: uuid::Uuid,
    now: DateTime<Utc>,
) -> ExpiryReport {
    let report = expire_with(
        store,
        objects.as_ref(),
        organization,
        now,
        uuid::Uuid::new_v4(),
    )
    .await;
    if report.expired > 0 {
        metrics().add(&SWEPT_ROWS, &["inference_trace_content"], report.expired);
    }
    report
}

async fn expire_with(
    store: &TraceContentStore,
    objects: &dyn TraceObjects,
    organization: uuid::Uuid,
    now: DateTime<Utc>,
    run_id: uuid::Uuid,
) -> ExpiryReport {
    match drain(store, objects, organization, now, run_id).await {
        Ok(expired) => ExpiryReport { expired, failed: 0 },
        Err(expired) => {
            tracing::warn!(organization_id = %organization, "trace content expiry incomplete");
            ExpiryReport { expired, failed: 1 }
        }
    }
}

impl ExpiryReport {
    fn add(&mut self, other: Self) {
        self.expired += other.expired;
        self.failed += other.failed;
    }
}

/// Expires the organization's content until a short batch, an error, or
/// [`MAX_BATCHES`]. Returns how many were expired, as an error when a batch
/// failed.
async fn drain(
    store: &TraceContentStore,
    objects: &dyn TraceObjects,
    organization: uuid::Uuid,
    now: DateTime<Utc>,
    run_id: uuid::Uuid,
) -> Result<u64, u64> {
    let mut expired = 0;
    for _ in 0..MAX_BATCHES {
        let batch = store
            .expired(organization, now, BATCH)
            .await
            .map_err(|error| {
                tracing::warn!(error = ?error, "trace content expiry query failed");
                expired
            })?;
        let mut deleted = Vec::with_capacity(batch.len());
        let mut failed = false;
        for row in &batch {
            match objects.delete(&row.object_name).await {
                Ok(()) => deleted.push(row.trace_id),
                Err(error) => {
                    tracing::warn!(trace_id = %row.trace_id, error = %error, "trace content object not deleted");
                    failed = true;
                }
            }
        }
        if !deleted.is_empty() {
            expired += store
                .mark_expired(organization, &deleted, now, run_id)
                .await
                .map_err(|error| {
                    tracing::warn!(error = ?error, "trace content not marked expired");
                    expired
                })?;
        }
        // A failed delete stays stored and is retried next run, so the
        // batch would come back: stop here.
        if failed {
            return Err(expired);
        }
        if batch.len() < BATCH as usize {
            break;
        }
    }
    Ok(expired)
}

/// What one run of [`expire_batch_files`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchFileExpiry {
    pub purged: u64,
    /// Files whose content the store refused to delete: still stored, and
    /// retried next run.
    pub failed: u64,
}

/// Removes batch files' content past its period (D-021): the trace content
/// period, since a batch file holds requests and responses just as trace
/// content does, and sooner for an upload that failed. A file an open batch
/// still reads is kept. The file records stay.
pub async fn expire_batch_files(
    store: &crate::persistence::batches::BatchStore,
    objects: Option<&Arc<dyn TraceObjects>>,
    now: DateTime<Utc>,
) -> Result<BatchFileExpiry, crate::persistence::batches::BatchStoreError> {
    let mut report = BatchFileExpiry::default();
    let Some(objects) = objects else {
        return Ok(report);
    };
    let days = i32::try_from(crate::domain::trace_policy::DEFAULT_CONTENT_DAYS).unwrap_or(30);
    for _ in 0..MAX_BATCHES {
        let due = store.expired_files(now, days, BATCH).await?;
        let mut kept = 0;
        for (organization_id, id, key) in &due {
            let refused = std::cell::Cell::new(false);
            // Deleting a missing object succeeds, so a repeat is harmless.
            let delete = || async {
                match objects.delete(key).await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(%organization_id, file_id = %id, error = %error, "batch file content not deleted");
                        refused.set(true);
                        false
                    }
                }
            };
            if store.purge_file(*organization_id, *id, now, delete).await? {
                report.purged += 1;
            } else {
                kept += 1;
            }
            if refused.get() {
                report.failed += 1;
            }
        }
        // A file kept this run would be selected again: stop rather than
        // loop on it.
        if (due.len() as i64) < BATCH || kept > 0 {
            break;
        }
    }
    if report.purged > 0 {
        metrics().add(&SWEPT_ROWS, &["batch_file"], report.purged);
    }
    Ok(report)
}
