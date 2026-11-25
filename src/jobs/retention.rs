//! The retention purge: for every organization with a retention period set,
//! deletes traces, reservations (with their usage and accounting records)
//! and audit events older than the period, in bounded batches, and adds the
//! counts to the policy. It runs with the hourly sweep; a backlog larger
//! than one run's batches drains over later runs. Each batch re-reads its
//! period, so a policy changed mid-run governs the batches after it.

use chrono::{DateTime, Utc};

use crate::{
    domain::retention::{PurgedCounts, RetentionKind},
    metrics::{SWEPT_ROWS, global as metrics},
    persistence::retention::RetentionStore,
};

const BATCH: i64 = 1_000;
/// Batches per kind and organization in one run.
const MAX_BATCHES: usize = 100;

/// What one purge run did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PurgeReport {
    pub purged: PurgedCounts,
    /// Organizations whose purge stopped on an error; what was deleted
    /// before it is still counted.
    pub failed: u64,
}

/// Purges every organization with a policy and returns the totals. An
/// organization whose purge fails is counted, logged and retried next run.
/// The policy's totals are updated with each batch, in its transaction.
pub async fn purge_expired(
    store: &RetentionStore,
    now: DateTime<Utc>,
) -> Result<PurgeReport, sqlx::Error> {
    let mut report = PurgeReport::default();
    // The request id of every audit event this run writes.
    let run_id = uuid::Uuid::new_v4();
    for organization in store.organizations_with_policy().await? {
        let mut failed = false;
        for kind in RetentionKind::ALL {
            let (purged, ok) = drain(store, kind, organization, now, run_id).await;
            add(&mut report.purged, purged);
            failed |= !ok;
        }
        if failed {
            report.failed += 1;
            tracing::warn!(organization_id = %organization, "retention purge incomplete");
        }
        if let Err(error) = store.record_purge(organization, now, failed, run_id).await {
            report.failed += u64::from(!failed);
            tracing::warn!(organization_id = %organization, error = ?error, "retention purge time not recorded");
        }
    }
    Ok(report)
}

fn add(total: &mut PurgedCounts, batch: PurgedCounts) {
    total.traces += batch.traces;
    total.reservations += batch.reservations;
    total.audit_events += batch.audit_events;
}

/// Purges `kind` until a short batch, an error, or [`MAX_BATCHES`].
/// Returns what was deleted, including before an error, and whether no
/// batch failed.
async fn drain(
    store: &RetentionStore,
    kind: RetentionKind,
    organization: uuid::Uuid,
    now: DateTime<Utc>,
    run_id: uuid::Uuid,
) -> (PurgedCounts, bool) {
    let mut deleted = PurgedCounts::default();
    for _ in 0..MAX_BATCHES {
        match store.purge(kind, organization, now, BATCH, run_id).await {
            Ok(batch) => {
                add(&mut deleted, batch);
                // Traces removed with their usage count as traces.
                for (table, count) in [
                    ("inference_trace", batch.traces),
                    ("accounting_reservation", batch.reservations),
                    ("audit_event", batch.audit_events),
                ] {
                    if count > 0 {
                        metrics().add(&SWEPT_ROWS, &[table], count);
                    }
                }
                let primary = match kind {
                    RetentionKind::Traces => batch.traces,
                    RetentionKind::Usage => batch.reservations,
                    RetentionKind::AuditEvents => batch.audit_events,
                };
                if primary < BATCH as u64 {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(table = kind.table(), error = ?error, "retention purge batch failed");
                return (deleted, false);
            }
        }
    }
    (deleted, true)
}
