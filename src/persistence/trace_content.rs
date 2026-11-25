//! Rows of traced content (`inference_trace_content`): what each traced
//! request kept and where its encrypted object is, and the expiry of stored
//! content.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::admin::{AdminStoreError, insert_audit};
use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    identity::OrganizationId,
    trace_policy::DEFAULT_CONTENT_DAYS,
};

/// What became of a traced request's content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentState {
    /// Recorded before its object is written; see the migration.
    Writing,
    Stored,
    Off,
    Unavailable,
    Failed,
    Expired,
}

impl ContentState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Writing => "writing",
            Self::Stored => "stored",
            Self::Off => "off",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }
}

/// The encrypted object of stored content.
#[derive(Clone, Debug)]
pub struct StoredObject {
    pub name: String,
    pub key_id: String,
    pub sealed_key: Vec<u8>,
}

/// A row to record.
#[derive(Clone, Debug)]
pub struct NewTraceContent {
    pub organization_id: OrganizationId,
    pub trace_id: Uuid,
    pub state: ContentState,
    /// Set exactly when `state` is `Writing` or `Stored`.
    pub object: Option<StoredObject>,
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub request_truncated: bool,
    pub response_truncated: bool,
    pub started_at: DateTime<Utc>,
    pub provider_started_at: Option<DateTime<Utc>>,
    pub first_event_at: Option<DateTime<Utc>>,
    pub finished_at: DateTime<Utc>,
}

/// How long a row may stay `writing` before the worker takes it as
/// abandoned. Writing an object is bounded well within it (credentials 15 s,
/// the upload 60 s).
pub const STALE_WRITE_MINUTES: i32 = 60;

/// Stored content past its period.
#[derive(Clone, Debug, FromRow)]
pub struct ExpiredContent {
    pub trace_id: Uuid,
    pub object_name: String,
}

#[derive(Clone)]
pub struct TraceContentStore {
    pool: PgPool,
}

impl TraceContentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records a traced request's content. Returns false when the trace
    /// already has a row, which is kept.
    pub async fn insert(&self, row: &NewTraceContent) -> Result<bool, sqlx::Error> {
        let bytes = |bytes: Option<u64>| bytes.and_then(|bytes| i64::try_from(bytes).ok());
        Ok(sqlx::query(
            "INSERT INTO gateway.inference_trace_content
                 (trace_id, organization_id, state, object_name, key_id, sealed_key,
                  request_bytes, response_bytes, request_truncated, response_truncated,
                  started_at, provider_started_at, first_event_at, finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (trace_id) DO NOTHING",
        )
        .bind(row.trace_id)
        .bind(row.organization_id.0)
        .bind(row.state.as_str())
        .bind(row.object.as_ref().map(|object| object.name.as_str()))
        .bind(row.object.as_ref().map(|object| object.key_id.as_str()))
        .bind(
            row.object
                .as_ref()
                .map(|object| object.sealed_key.as_slice()),
        )
        .bind(bytes(row.request_bytes))
        .bind(bytes(row.response_bytes))
        .bind(row.request_truncated)
        .bind(row.response_truncated)
        .bind(row.started_at)
        .bind(row.provider_started_at)
        .bind(row.first_event_at)
        .bind(row.finished_at)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Records how writing a `writing` row's object ended: `stored`, or
    /// `failed` with its object (deleted) no longer named. Returns false
    /// when the row was no longer being written.
    pub async fn finish_write(&self, trace_id: Uuid, stored: bool) -> Result<bool, sqlx::Error> {
        let query = if stored {
            "UPDATE gateway.inference_trace_content SET state = 'stored'
             WHERE trace_id = $1 AND state = 'writing'"
        } else {
            "UPDATE gateway.inference_trace_content
             SET state = 'failed', object_name = NULL, key_id = NULL, sealed_key = NULL
             WHERE trace_id = $1 AND state = 'writing'"
        };
        Ok(sqlx::query(query)
            .bind(trace_id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// Organizations with content stored or being written.
    pub async fn organizations_with_content(&self) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT DISTINCT organization_id FROM gateway.inference_trace_content
             WHERE state IN ('writing', 'stored') ORDER BY organization_id",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Up to `limit` of the organization's stored content that is past its
    /// period at `now`: the tracing policy's content period (30 days without
    /// a policy), or sooner when its request record's trace or usage period
    /// under the retention policy has passed, so content never outlives the
    /// record it belongs to. A row still `writing` after
    /// [`STALE_WRITE_MINUTES`] is included too: its writer is gone, and its
    /// object, if any, is removed.
    pub async fn expired(
        &self,
        organization_id: Uuid,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ExpiredContent>, sqlx::Error> {
        sqlx::query_as(
            "SELECT c.trace_id, c.object_name
             FROM gateway.inference_trace_content c
             JOIN gateway.inference_trace t ON t.id = c.trace_id
             JOIN gateway.accounting_reservation r
               ON r.organization_id = t.organization_id AND r.id = t.reservation_id
             LEFT JOIN gateway.organization_trace_policy p
               ON p.organization_id = c.organization_id
             LEFT JOIN gateway.organization_retention_policy rp
               ON rp.organization_id = c.organization_id
             WHERE c.organization_id = $1 AND c.state IN ('writing', 'stored')
               AND ((c.state = 'writing' AND c.recorded_at < $2 - make_interval(mins => $5))
                    OR c.recorded_at < $2 - make_interval(days => coalesce(p.content_days, $4))
                    OR t.recorded_at < $2 - make_interval(days => rp.trace_days)
                    OR r.created_at < $2 - make_interval(days => rp.usage_days))
             ORDER BY c.recorded_at
             LIMIT $3",
        )
        .bind(organization_id)
        .bind(now)
        .bind(limit)
        .bind(i32::try_from(DEFAULT_CONTENT_DAYS).unwrap_or(30))
        .bind(STALE_WRITE_MINUTES)
        .fetch_all(&self.pool)
        .await
    }

    /// Marks content whose objects are deleted as expired (a stale `writing`
    /// row as failed), dropping where the objects were and their keys, and audits the batch by the system
    /// (`retention.trace_content_purged`, naming the tracing policy) in the
    /// same transaction. Returns how many rows changed.
    pub async fn mark_expired(
        &self,
        organization_id: Uuid,
        trace_ids: &[Uuid],
        now: DateTime<Utc>,
        run_id: Uuid,
    ) -> Result<u64, AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        let expired = sqlx::query(
            "UPDATE gateway.inference_trace_content
             SET state = CASE state WHEN 'stored' THEN 'expired' ELSE 'failed' END,
                 expired_at = CASE state WHEN 'stored' THEN $3::timestamptz END,
                 object_name = NULL, key_id = NULL, sealed_key = NULL
             WHERE organization_id = $1 AND trace_id = ANY($2)
               AND state IN ('writing', 'stored')",
        )
        .bind(organization_id)
        .bind(trace_ids)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let policy: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM gateway.organization_trace_policy WHERE organization_id = $1",
        )
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let (true, Some(policy)) = (expired > 0, policy) {
            insert_audit(
                &mut tx,
                &AdminAuditEvent::new(
                    AuditActor::System,
                    OrganizationId(organization_id),
                    None,
                    AuditTargetResource::TracePolicy,
                    AuditAction::new("retention.trace_content_purged")
                        .expect("audit action constant is valid"),
                    policy,
                    AuditOutcome::Succeeded,
                    run_id,
                    None,
                    None,
                ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(expired)
    }
}

/// One trace's content row as read back, with its record's team.
#[derive(Clone, Debug, FromRow)]
pub struct ContentRecord {
    pub team_id: Option<Uuid>,
    /// When the request record was finalized; null while it is in flight.
    pub trace_finalized_at: Option<DateTime<Utc>>,
    pub state: Option<String>,
    pub object_name: Option<String>,
    pub key_id: Option<String>,
    pub sealed_key: Option<Vec<u8>>,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    pub request_truncated: Option<bool>,
    pub response_truncated: Option<bool>,
    pub started_at: Option<DateTime<Utc>>,
    pub provider_started_at: Option<DateTime<Utc>>,
    pub first_event_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub expired_at: Option<DateTime<Utc>>,
}

impl TraceContentStore {
    /// The organization's trace `trace_id` with its content row, if any
    /// (every content column null when the request was not traced). `None`
    /// when the organization has no such trace.
    pub async fn read(
        &self,
        organization_id: OrganizationId,
        trace_id: Uuid,
    ) -> Result<Option<ContentRecord>, sqlx::Error> {
        sqlx::query_as(
            "SELECT t.team_id, t.finalized_at AS trace_finalized_at, c.state, c.object_name, c.key_id, c.sealed_key,
                    c.request_bytes, c.response_bytes, c.request_truncated,
                    c.response_truncated, c.started_at, c.provider_started_at,
                    c.first_event_at, c.finished_at, c.expired_at
             FROM gateway.inference_trace t
             LEFT JOIN gateway.inference_trace_content c ON c.trace_id = t.id
             WHERE t.organization_id = $1 AND t.id = $2",
        )
        .bind(organization_id.0)
        .bind(trace_id)
        .fetch_optional(&self.pool)
        .await
    }
}
