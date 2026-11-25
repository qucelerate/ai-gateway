//! Usage export queue (D-014).
//!
//! [`UsageExportSink`] is an outbox sink: inside the dispatch transaction it
//! queues one record per `inference.finalized` event for an organization
//! whose export was enabled by the time the inference finalized. The worker
//! seals queued records into batches, one or more per hour of
//! `finalized_at`, each with the object name it is uploaded under; uploads
//! one batch at a time outside any transaction; and deletes a batch and its
//! records once written. A retry rewrites the same object with the same
//! records.
//!
//! Lock order: the organization row, then the export row, then records,
//! then batches. A queued record holds a key-share lock on its export row
//! through its foreign key until dispatch commits, so sealing (which locks
//! the export row for update) never misses a record committed under it,
//! and deleting the export waits for dispatch rather than orphaning one.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, UserAccountId},
    outbox::{OutboxPayload, UsageExportFailing},
    usage_export::{
        DestinationInput, FAILING_AFTER, MAX_BATCH_RECORDS, RECORD_SCHEMA, StorageKind,
        UsageExportView, UsageRecord, WriteError, batch_name, object_name,
    },
};
use crate::persistence::outbox::{self, OutboxEvent, OutboxSink, SinkError};

#[derive(Clone)]
pub struct UsageExportStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct ViewRow {
    id: Uuid,
    kind: String,
    connection: String,
    bucket: String,
    account: Option<String>,
    region: Option<String>,
    prefix: String,
    enabled_at: DateTime<Utc>,
    verified_at: DateTime<Utc>,
    last_success_at: Option<DateTime<Utc>>,
    failing_since: Option<DateTime<Utc>>,
    last_error: Option<String>,
    queued_records: i64,
    oldest_queued_at: Option<DateTime<Utc>>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

const VIEW_SELECT: &str = "
    SELECT x.id, x.kind, x.connection, x.bucket, x.account, x.region, x.prefix, x.enabled_at,
           x.verified_at, x.last_success_at, x.failing_since, x.last_error,
           (SELECT count(*) FROM gateway.usage_export_record r
            WHERE r.organization_id = x.organization_id) AS queued_records,
           (SELECT min(r.queued_at) FROM gateway.usage_export_record r
            WHERE r.organization_id = x.organization_id) AS oldest_queued_at,
           v.version, x.created_at, x.updated_at
    FROM gateway.usage_export x
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = x.organization_id AND v.resource_type = 'usage_export'
     AND v.resource_id = x.id
    WHERE x.organization_id = $1";

impl ViewRow {
    fn into_view(self, now: DateTime<Utc>) -> Option<UsageExportView> {
        Some(UsageExportView {
            id: self.id,
            kind: StorageKind::parse(&self.kind)?,
            connection: self.connection,
            bucket: self.bucket,
            account: self.account,
            region: self.region,
            prefix: self.prefix,
            enabled_at: self.enabled_at,
            verified_at: self.verified_at,
            last_success_at: self.last_success_at,
            failing: self
                .failing_since
                .is_some_and(|since| since <= now - FAILING_AFTER),
            failing_since: self.failing_since,
            last_error: self.last_error,
            queued_records: self.queued_records,
            oldest_queued_at: self.oldest_queued_at,
            version: self
                .version
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(1),
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(FromRow)]
struct DestinationRow {
    kind: String,
    connection: String,
    bucket: String,
    account: Option<String>,
    region: Option<String>,
    prefix: String,
}

impl DestinationRow {
    fn into_destination(self) -> Option<DestinationInput> {
        Some(DestinationInput {
            kind: StorageKind::parse(&self.kind)?,
            connection: self.connection,
            bucket: self.bucket,
            account: self.account,
            region: self.region,
            prefix: self.prefix,
        })
    }
}

/// A batch claimed for upload. Its lease keeps other workers off it until
/// `next_attempt_at`.
#[derive(Clone, Debug, FromRow)]
pub struct ClaimedBatch {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub object_name: String,
    pub records: i32,
    pub attempts: i32,
}

/// A claimed batch's destination and records, in sequence order, each one
/// line of JSON.
pub struct BatchContents {
    pub destination: DestinationInput,
    pub lines: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SealReport {
    pub batches: u64,
    pub records: u64,
}

#[derive(FromRow)]
struct RecordRow {
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    end_user: Option<String>,
    operation: String,
    deployment: Option<String>,
    provider_family: Option<String>,
    location: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    other_tokens: Option<i64>,
    usage_confidence: Option<String>,
    usage_completeness: Option<String>,
    pricing_confidence: Option<String>,
    recorded_at: DateTime<Utc>,
    batch_id: Option<Uuid>,
    custom_id: Option<String>,
    model_master: Option<String>,
}

/// Queues `inference.finalized` events for organizations with an export.
pub struct UsageExportSink;

#[async_trait]
impl OutboxSink for UsageExportSink {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        let OutboxPayload::InferenceFinalized(finalized) = &event.payload else {
            return Ok(());
        };
        let failed = |_| SinkError::new("usage export queueing failed");
        // From the reservation, which is kept as long as usage (a trace may
        // be purged sooner, while the event still waits); outcome, amount
        // and time from the event, which a later finalization of the trace
        // does not change.
        let row: Option<RecordRow> = sqlx::query_as(
            "SELECT r.team_id, r.service_account_id,
                    (SELECT eu.external_id FROM gateway.end_user eu
                     WHERE eu.id = r.end_user_id) AS end_user,
                    COALESCE(r.catalog_operation, 'inference') AS operation, r.deployment,
                    r.catalog_family AS provider_family, d.location,
                    lu.input_tokens, lu.output_tokens, lu.cached_input_tokens,
                    lu.reasoning_tokens, lu.other_tokens, lu.confidence AS usage_confidence,
                    lu.completeness AS usage_completeness, r.pricing_confidence,
                    r.created_at AS recorded_at, r.batch_id, bl.custom_id,
                    r.model_master
             FROM gateway.usage_export x
             JOIN gateway.accounting_reservation r
               ON r.organization_id = x.organization_id AND r.id = $2
             LEFT JOIN gateway.catalog_deployment d
               ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
              AND d.id = r.catalog_deployment_id
             LEFT JOIN LATERAL (
               SELECT u.input_tokens, u.output_tokens, u.cached_input_tokens,
                      u.reasoning_tokens, u.other_tokens, u.confidence, u.completeness
               FROM gateway.accounting_usage u
               WHERE u.organization_id = r.organization_id AND u.reservation_id = r.id
               ORDER BY u.recorded_at DESC, u.id DESC
               LIMIT 1
             ) lu ON true
             LEFT JOIN gateway.batch_line bl
               ON bl.organization_id = r.organization_id AND bl.reservation_id = r.id
             WHERE x.organization_id = $1 AND x.enabled_at <= $3",
        )
        .bind(event.organization_id.0)
        .bind(finalized.reservation_id)
        .bind(finalized.finalized_at)
        .fetch_optional(&mut *conn)
        .await
        .map_err(failed)?;
        // No export, usage from before it, or usage already past the
        // organization's retention period.
        let Some(row) = row else {
            return Ok(());
        };
        let record = UsageRecord {
            schema: RECORD_SCHEMA.to_owned(),
            event_id: event.id,
            trace_id: finalized.trace_id,
            reservation_id: finalized.reservation_id,
            organization_id: event.organization_id.0,
            team_id: row.team_id,
            service_account_id: row.service_account_id,
            end_user: row.end_user,
            operation: row.operation,
            deployment: row.deployment,
            model: finalized.model.clone(),
            provider_family: row.provider_family,
            location: row.location,
            outcome: finalized.outcome.clone(),
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cached_input_tokens: row.cached_input_tokens,
            reasoning_tokens: row.reasoning_tokens,
            other_tokens: row.other_tokens,
            usage_confidence: row.usage_confidence,
            usage_completeness: row.usage_completeness,
            pricing_confidence: row.pricing_confidence,
            amount_minor: finalized.amount_minor,
            currency: finalized.currency.trim().to_owned(),
            recorded_at: row.recorded_at,
            finalized_at: finalized.finalized_at,
            batch_id: row.batch_id.map(crate::domain::batch::public_batch_id),
            custom_id: row.custom_id,
            model_master: row.model_master,
        };
        let line = serde_json::to_string(&record)
            .map_err(|_| SinkError::new("usage export record could not be encoded"))?;
        sqlx::query(
            "INSERT INTO gateway.usage_export_record
                 (organization_id, event_id, sequence, finalized_at, record)
             VALUES ($1, $2, $3, $4, $5::json)
             ON CONFLICT (organization_id, event_id) DO NOTHING",
        )
        .bind(event.organization_id.0)
        .bind(event.id)
        .bind(event.sequence)
        .bind(finalized.finalized_at)
        .bind(line)
        .execute(conn)
        .await
        .map_err(failed)?;
        Ok(())
    }
}

impl UsageExportStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's export, if it has one.
    pub async fn get(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<UsageExportView>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::view(&mut conn, organization_id).await
    }

    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<UsageExportView>, sqlx::Error> {
        let row = sqlx::query_as::<_, ViewRow>(VIEW_SELECT)
            .bind(organization_id.0)
            .fetch_optional(conn)
            .await?;
        Ok(row.and_then(|row| row.into_view(Utc::now())))
    }

    /// Sets the destination, verified at `now`. A new export covers usage
    /// finalized from `now`; one that already existed keeps its start. A
    /// changed destination unseals the queue, so every queued record is
    /// written under the new one, and starts with a clean health record.
    /// Returns whether the export was created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        destination: &DestinationInput,
        actor: UserAccountId,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let existing: Option<DestinationRow> = sqlx::query_as(
            "SELECT kind, connection, bucket, account, region, prefix
             FROM gateway.usage_export WHERE organization_id = $1 FOR UPDATE",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(existing) = existing else {
            sqlx::query(
                "INSERT INTO gateway.usage_export
                     (organization_id, id, kind, connection, bucket, account, region, prefix,
                      enabled_at, verified_at, created_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $10)",
            )
            .bind(organization_id.0)
            .bind(id)
            .bind(destination.kind.as_str())
            .bind(&destination.connection)
            .bind(&destination.bucket)
            .bind(&destination.account)
            .bind(&destination.region)
            .bind(&destination.prefix)
            .bind(now)
            .bind(actor.0)
            .execute(&mut *conn)
            .await?;
            return Ok(true);
        };
        let changed = existing.into_destination().as_ref() != Some(destination);
        sqlx::query(
            "UPDATE gateway.usage_export
             SET kind = $2, connection = $3, bucket = $4, account = $5, region = $6,
                 prefix = $7, verified_at = $8, updated_at = CURRENT_TIMESTAMP,
                 failing_since = CASE WHEN $9 THEN NULL ELSE failing_since END,
                 failing_reported_at = CASE WHEN $9 THEN NULL ELSE failing_reported_at END,
                 last_error = CASE WHEN $9 THEN NULL ELSE last_error END
             WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .bind(destination.kind.as_str())
        .bind(&destination.connection)
        .bind(&destination.bucket)
        .bind(&destination.account)
        .bind(&destination.region)
        .bind(&destination.prefix)
        .bind(now)
        .bind(changed)
        .execute(&mut *conn)
        .await?;
        if changed {
            // Their records return to the queue.
            sqlx::query("DELETE FROM gateway.usage_export_batch WHERE organization_id = $1")
                .bind(organization_id.0)
                .execute(&mut *conn)
                .await?;
        }
        Ok(false)
    }

    /// Deletes the export and discards its queue.
    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM gateway.usage_export WHERE organization_id = $1")
            .bind(organization_id.0)
            .execute(conn)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Organizations whose unsealed records are due: the oldest was queued
    /// at or before `queued_before`, or `MAX_BATCH_RECORDS` are waiting.
    pub async fn due_for_sealing(
        &self,
        queued_before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<OrganizationId>, sqlx::Error> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT organization_id FROM gateway.usage_export_record
             WHERE batch_id IS NULL
             GROUP BY organization_id
             HAVING min(queued_at) <= $1 OR count(*) >= $2
             LIMIT $3",
        )
        .bind(queued_before)
        .bind(MAX_BATCH_RECORDS)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().map(OrganizationId).collect())
    }

    /// Seals every unsealed record of the organization into batches of at
    /// most `MAX_BATCH_RECORDS`, one or more per hour of `finalized_at`, in
    /// one transaction.
    pub async fn seal(&self, organization_id: OrganizationId) -> Result<SealReport, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Waits for dispatch transactions still queueing records here.
        let prefix: Option<String> = sqlx::query_scalar(
            "SELECT prefix FROM gateway.usage_export WHERE organization_id = $1 FOR UPDATE",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(prefix) = prefix else {
            return Ok(SealReport::default());
        };
        let records: Vec<(Uuid, i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT event_id, sequence, date_trunc('hour', finalized_at, 'UTC') AS hour
             FROM gateway.usage_export_record
             WHERE organization_id = $1 AND batch_id IS NULL
             ORDER BY hour, sequence",
        )
        .bind(organization_id.0)
        .fetch_all(&mut *tx)
        .await?;
        let mut report = SealReport::default();
        let max = usize::try_from(MAX_BATCH_RECORDS).unwrap_or(usize::MAX);
        for hour in records.chunk_by(|a, b| a.2 == b.2) {
            for batch in hour.chunks(max) {
                let (Some(first), Some(last)) = (batch.first(), batch.last()) else {
                    continue;
                };
                let id = Uuid::new_v4();
                let name = object_name(
                    &prefix,
                    &batch_name(organization_id, first.2, first.1, last.1),
                );
                sqlx::query(
                    "INSERT INTO gateway.usage_export_batch
                         (id, organization_id, object_name, hour, first_sequence, last_sequence,
                          records)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(id)
                .bind(organization_id.0)
                .bind(name)
                .bind(first.2)
                .bind(first.1)
                .bind(last.1)
                .bind(i32::try_from(batch.len()).unwrap_or(i32::MAX))
                .execute(&mut *tx)
                .await?;
                let events: Vec<Uuid> = batch.iter().map(|record| record.0).collect();
                sqlx::query(
                    "UPDATE gateway.usage_export_record SET batch_id = $3
                     WHERE organization_id = $1 AND event_id = ANY($2)",
                )
                .bind(organization_id.0)
                .bind(&events)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                report.batches += 1;
                report.records += batch.len() as u64;
            }
        }
        tx.commit().await?;
        Ok(report)
    }

    /// Claims up to `limit` due batches until `lease_until`, counting the
    /// attempt.
    pub async fn claim_due(
        &self,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ClaimedBatch>, sqlx::Error> {
        sqlx::query_as(
            "UPDATE gateway.usage_export_batch
             SET next_attempt_at = $2, attempts = attempts + 1
             WHERE id IN (
                 SELECT b.id FROM gateway.usage_export_batch b
                 -- A suspended (or deleted) organization's batches wait:
                 -- its connections are not used.
                 JOIN gateway.organization o
                   ON o.id = b.organization_id AND o.suspended_at IS NULL
                 WHERE b.next_attempt_at <= $1
                 ORDER BY b.next_attempt_at
                 LIMIT $3
                 FOR UPDATE OF b SKIP LOCKED
             )
             RETURNING id, organization_id, object_name, records, attempts",
        )
        .bind(now)
        .bind(lease_until)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// The batch's destination and records; `None` when the batch is gone
    /// or no longer whole (its destination changed, or retention removed
    /// its records).
    pub async fn contents(
        &self,
        batch: &ClaimedBatch,
    ) -> Result<Option<BatchContents>, sqlx::Error> {
        let destination: Option<DestinationRow> = sqlx::query_as(
            "SELECT x.kind, x.connection, x.bucket, x.account, x.region, x.prefix
             FROM gateway.usage_export x
             JOIN gateway.usage_export_batch b ON b.organization_id = x.organization_id
             WHERE b.organization_id = $1 AND b.id = $2",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(destination) = destination.and_then(DestinationRow::into_destination) else {
            return Ok(None);
        };
        let lines: Vec<String> = sqlx::query_scalar(
            "SELECT record::text FROM gateway.usage_export_record
             WHERE organization_id = $1 AND batch_id = $2
             ORDER BY sequence",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .fetch_all(&self.pool)
        .await?;
        if i32::try_from(lines.len()).ok() != Some(batch.records) {
            return Ok(None);
        }
        Ok(Some(BatchContents { destination, lines }))
    }

    /// The batch was written: its records leave the queue and the export is
    /// healthy. Nothing changes if the batch was discarded meanwhile.
    pub async fn record_written(
        &self,
        batch: &ClaimedBatch,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_export(&mut tx, batch.organization_id).await?;
        sqlx::query(
            "DELETE FROM gateway.usage_export_record WHERE organization_id = $1 AND batch_id = $2",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query(
            "DELETE FROM gateway.usage_export_batch WHERE organization_id = $1 AND id = $2",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if deleted {
            sqlx::query(
                "UPDATE gateway.usage_export
                 SET last_success_at = $2, failing_since = NULL, failing_reported_at = NULL,
                     last_error = NULL
                 WHERE organization_id = $1",
            )
            .bind(batch.organization_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// The write failed: the batch is retried at `retry_at`, and the export
    /// is failing from now unless it already was.
    pub async fn record_failed(
        &self,
        batch: &ClaimedBatch,
        error: WriteError,
        retry_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let reason = error.to_string();
        let mut tx = self.pool.begin().await?;
        lock_export(&mut tx, batch.organization_id).await?;
        let kept = sqlx::query(
            "UPDATE gateway.usage_export_batch SET next_attempt_at = $3, last_error = $4
             WHERE organization_id = $1 AND id = $2",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .bind(retry_at)
        .bind(&reason)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if kept {
            sqlx::query(
                "UPDATE gateway.usage_export
                 SET failing_since = COALESCE(failing_since, $2), last_error = $3
                 WHERE organization_id = $1",
            )
            .bind(batch.organization_id)
            .bind(now)
            .bind(&reason)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// Emits `usage_export.failing` once for each export that has been
    /// failing for [`FAILING_AFTER`], until it next succeeds or its
    /// destination changes. Returns how many were reported.
    pub async fn report_failing(&self, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
        let organizations: Vec<Uuid> = sqlx::query_scalar(
            "SELECT organization_id FROM gateway.usage_export
             WHERE failing_since <= $1 AND failing_reported_at IS NULL",
        )
        .bind(now - FAILING_AFTER)
        .fetch_all(&self.pool)
        .await?;
        let mut reported = 0;
        for organization_id in organizations {
            let mut tx = self.pool.begin().await?;
            // The event's row locks the organization; take it before the
            // export, as admin mutations do.
            sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR KEY SHARE")
                .bind(organization_id)
                .execute(&mut *tx)
                .await?;
            let row: Option<(Uuid, String, DateTime<Utc>, Option<String>, i64)> = sqlx::query_as(
                "UPDATE gateway.usage_export x SET failing_reported_at = $2
                     WHERE organization_id = $1 AND failing_since <= $3
                       AND failing_reported_at IS NULL
                     RETURNING id, kind, failing_since, last_error,
                               (SELECT count(*) FROM gateway.usage_export_record r
                                WHERE r.organization_id = x.organization_id)",
            )
            .bind(organization_id)
            .bind(now)
            .bind(now - FAILING_AFTER)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((export_id, kind, failing_since, last_error, queued_records)) = row else {
                continue;
            };
            let Some(kind) = StorageKind::parse(&kind) else {
                continue;
            };
            let payload = OutboxPayload::UsageExportFailing(UsageExportFailing {
                export_id,
                kind,
                failing_since,
                last_error,
                queued_records,
            });
            outbox::emit(&mut tx, OrganizationId(organization_id), &payload)
                .await
                .map_err(|error| match error {
                    outbox::OutboxError::Database(error) => error,
                    outbox::OutboxError::Encode(error) => sqlx::Error::Encode(Box::new(error)),
                })?;
            tx.commit().await?;
            reported += 1;
        }
        Ok(reported)
    }

    /// Drops up to `max_rows` queued records past the organization's usage
    /// retention period, whole hours at a time so a batch goes all at once,
    /// and then batches left empty. Returns the records dropped.
    pub async fn sweep_expired(
        &self,
        now: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let records = sqlx::query(
            "DELETE FROM gateway.usage_export_record r
             USING (
                 SELECT r.organization_id, r.event_id
                 FROM gateway.usage_export_record r
                 JOIN gateway.organization_retention_policy p
                   ON p.organization_id = r.organization_id
                 WHERE p.usage_days IS NOT NULL
                   AND r.finalized_at
                       < date_trunc('hour', $1 - make_interval(days => p.usage_days), 'UTC')
                 LIMIT $2
                 FOR UPDATE OF r SKIP LOCKED
             ) expired
             WHERE r.organization_id = expired.organization_id
               AND r.event_id = expired.event_id",
        )
        .bind(now)
        .bind(max_rows)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "DELETE FROM gateway.usage_export_batch b
             USING gateway.organization_retention_policy p
             WHERE p.organization_id = b.organization_id AND p.usage_days IS NOT NULL
               AND b.hour < date_trunc('hour', $1 - make_interval(days => p.usage_days), 'UTC')
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.usage_export_record r
                   WHERE r.organization_id = b.organization_id AND r.batch_id = b.id
               )",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(records)
    }
}

/// Serializes outcome writes with sealing and destination changes, without
/// blocking dispatch's key-share locks.
async fn lock_export(conn: &mut PgConnection, organization_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM gateway.usage_export WHERE organization_id = $1 FOR NO KEY UPDATE")
        .bind(organization_id)
        .execute(conn)
        .await?;
    Ok(())
}
