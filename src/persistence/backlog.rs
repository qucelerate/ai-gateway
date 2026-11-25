//! Gateway-wide backlog counts for the operational metrics. Each reads a
//! partial index of rows awaiting work, so sampling cost follows the backlog,
//! not the history the tables hold. No organization is identified.

use sqlx::{FromRow, PgPool};

/// What is waiting, across all organizations.
#[derive(Clone, Copy, Debug, Default, Eq, FromRow, PartialEq)]
pub struct Backlog {
    pub outbox_pending: i64,
    /// Seconds since the oldest undispatched event occurred; 0 when none.
    pub outbox_oldest_seconds: i64,
    pub webhooks_pending: i64,
    pub reconciliation_open: i64,
    /// Reservations past expiry that recovery has not processed yet.
    pub reservations_overdue: i64,
    pub price_sources_failing: i64,
    pub usage_export_queued: i64,
    /// Exports whose writes have failed for an hour.
    pub usage_exports_failing: i64,
}

#[derive(Clone)]
pub struct BacklogStore {
    pool: PgPool,
}

impl BacklogStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn sample(&self) -> Result<Backlog, sqlx::Error> {
        sqlx::query_as(
            "SELECT
                 (SELECT count(*) FROM gateway.outbox_event
                  WHERE dispatched_at IS NULL) AS outbox_pending,
                 COALESCE((SELECT floor(EXTRACT(EPOCH FROM now() - min(occurred_at)))::BIGINT
                           FROM gateway.outbox_event WHERE dispatched_at IS NULL), 0)
                     AS outbox_oldest_seconds,
                 (SELECT count(*) FROM gateway.webhook_delivery
                  WHERE status = 'pending') AS webhooks_pending,
                 (SELECT count(*) FROM gateway.accounting_reconciliation
                  WHERE status = 'open') AS reconciliation_open,
                 (SELECT count(*) FROM gateway.accounting_reservation
                  WHERE state IN ('reserved', 'claimed') AND expires_at <= now())
                     AS reservations_overdue,
                 (SELECT count(*) FROM gateway.price_source
                  WHERE retired_at IS NULL AND last_error_code IS NOT NULL)
                     AS price_sources_failing,
                 (SELECT count(*) FROM gateway.usage_export_record) AS usage_export_queued,
                 (SELECT count(*) FROM gateway.usage_export
                  WHERE failing_since <= now() - interval '1 hour')
                     AS usage_exports_failing",
        )
        .fetch_one(&self.pool)
        .await
    }
}
