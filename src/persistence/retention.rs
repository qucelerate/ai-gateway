//! Retention policies, the retention purge, and export queries.
//!
//! The purge deletes in batches, each in its own transaction marked with
//! `gateway.retention_purge`, which the append-only triggers of usage, price
//! snapshot and audit rows require before they allow a delete. Each batch
//! reads its period from the policy row under `FOR SHARE`, so it waits for a
//! policy write in progress and never deletes under a period that has since
//! been lengthened or removed. Rows are locked with `SKIP LOCKED`, so
//! concurrent workers share the work.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::admin::{AdminStoreError, insert_audit};
use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    retention::{
        ExportDataset, ExportQuery, ExportRecord, ExportStatus, PurgedCounts, RetentionKind,
        RetentionPolicyInput, RetentionPolicyView,
    },
};

#[derive(Clone)]
pub struct RetentionStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct PolicyRow {
    trace_days: Option<i32>,
    usage_days: Option<i32>,
    audit_days: Option<i32>,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
    last_purged_at: Option<DateTime<Utc>>,
    purged_traces: i64,
    purged_reservations: i64,
    purged_audit_events: i64,
}

impl From<PolicyRow> for RetentionPolicyView {
    fn from(row: PolicyRow) -> Self {
        let days = |days: Option<i32>| days.and_then(|days| u32::try_from(days).ok());
        let count = |count: i64| u64::try_from(count).unwrap_or(0);
        Self {
            trace_days: days(row.trace_days),
            usage_days: days(row.usage_days),
            audit_days: days(row.audit_days),
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            updated_at: Some(row.updated_at),
            last_purged_at: row.last_purged_at,
            purged: PurgedCounts {
                traces: count(row.purged_traces),
                reservations: count(row.purged_reservations),
                audit_events: count(row.purged_audit_events),
            },
        }
    }
}

const POLICY_SELECT: &str = "SELECT p.trace_days, p.usage_days, p.audit_days, v.version,
        p.updated_at, p.last_purged_at, p.purged_traces, p.purged_reservations,
        p.purged_audit_events
    FROM gateway.organization_retention_policy p
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = p.organization_id
     AND v.resource_type = 'retention_policy' AND v.resource_id = p.id
    WHERE p.organization_id = $1";

/// A batch line's batch, as the batches APIs name it; `null` otherwise.
const BATCH_ID: &str = "CASE WHEN r.batch_id IS NOT NULL THEN 'batch_' || r.batch_id::text END";

impl RetentionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn policy(
        &self,
        organization_id: OrganizationId,
    ) -> Result<RetentionPolicyView, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map_or_else(RetentionPolicyView::unset, Into::into))
    }

    /// The policy on the caller's connection, so a mutation reads what its
    /// own transaction wrote.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<RetentionPolicyView>, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(row.map(Into::into))
    }

    /// Insert or replace the policy. Returns true when a row was created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        input: RetentionPolicyInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        let days = |days: Option<u32>| days.and_then(|days| i32::try_from(days).ok());
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.organization_retention_policy
                 (organization_id, id, trace_days, usage_days, audit_days, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (organization_id) DO UPDATE
             SET trace_days = EXCLUDED.trace_days,
                 usage_days = EXCLUDED.usage_days,
                 audit_days = EXCLUDED.audit_days,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(days(input.trace_days))
        .bind(days(input.usage_days))
        .bind(days(input.audit_days))
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }

    /// Organizations with any retention period set.
    pub async fn organizations_with_policy(&self) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT organization_id FROM gateway.organization_retention_policy
             WHERE trace_days IS NOT NULL OR usage_days IS NOT NULL OR audit_days IS NOT NULL
             ORDER BY organization_id",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Deletes up to `limit` rows of `kind` older than the organization's
    /// current period for it, read in the batch's own transaction, and adds
    /// what it deleted to the policy's totals in the same transaction, so
    /// the totals never miss a committed batch. A batch that deleted
    /// anything is audited by the system (`retention.{kind}_purged`) in that
    /// transaction too, so no deletion commits without its event; `run_id`
    /// is the event's request id. Deletes nothing when no period is set any
    /// more.
    pub async fn purge(
        &self,
        kind: RetentionKind,
        organization_id: Uuid,
        now: DateTime<Utc>,
        limit: i64,
        run_id: Uuid,
    ) -> Result<PurgedCounts, AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET LOCAL gateway.retention_purge = 'on'")
            .execute(&mut *tx)
            .await?;
        let days: Option<i32> = sqlx::query_scalar::<_, Option<i32>>(&format!(
            "SELECT {} FROM gateway.organization_retention_policy
             WHERE organization_id = $1 FOR NO KEY UPDATE",
            kind.column()
        ))
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let Some(days) = days else {
            return Ok(PurgedCounts::default());
        };
        let before = now - Duration::days(i64::from(days));
        let purged = match kind {
            RetentionKind::Traces => PurgedCounts {
                traces: purge_traces(&mut tx, organization_id, before, limit).await?,
                ..PurgedCounts::default()
            },
            RetentionKind::Usage => {
                purge_reservations(&mut tx, organization_id, before, limit).await?
            }
            RetentionKind::AuditEvents => PurgedCounts {
                audit_events: purge_audit_events(&mut tx, organization_id, before, limit).await?,
                ..PurgedCounts::default()
            },
        };
        let count = |count: u64| i64::try_from(count).unwrap_or(i64::MAX);
        let policy: Uuid = sqlx::query_scalar(
            "UPDATE gateway.organization_retention_policy
             SET purged_traces = purged_traces + $2,
                 purged_reservations = purged_reservations + $3,
                 purged_audit_events = purged_audit_events + $4
             WHERE organization_id = $1
             RETURNING id",
        )
        .bind(organization_id)
        .bind(count(purged.traces))
        .bind(count(purged.reservations))
        .bind(count(purged.audit_events))
        .fetch_one(&mut *tx)
        .await?;
        if purged.traces + purged.reservations + purged.audit_events > 0 {
            let action = match kind {
                RetentionKind::Traces => "retention.traces_purged",
                RetentionKind::Usage => "retention.usage_purged",
                RetentionKind::AuditEvents => "retention.audit_events_purged",
            };
            insert_audit(
                &mut tx,
                &purge_event(
                    organization_id,
                    policy,
                    action,
                    AuditOutcome::Succeeded,
                    run_id,
                ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(purged)
    }

    /// Records that the purge ran for the organization. A run that failed
    /// is audited as `retention.purge_failed` by the system, in the same
    /// transaction; what it did delete was audited by each batch.
    pub async fn record_purge(
        &self,
        organization_id: Uuid,
        at: DateTime<Utc>,
        failed: bool,
        run_id: Uuid,
    ) -> Result<(), AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        let policy: Option<Uuid> = sqlx::query_scalar(
            "UPDATE gateway.organization_retention_policy SET last_purged_at = $2
             WHERE organization_id = $1 RETURNING id",
        )
        .bind(organization_id)
        .bind(at)
        .fetch_optional(&mut *tx)
        .await?;
        if let (Some(policy), true) = (policy, failed) {
            insert_audit(
                &mut tx,
                &purge_event(
                    organization_id,
                    policy,
                    "retention.purge_failed",
                    AuditOutcome::Failed,
                    run_id,
                ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Up to `limit` export rows after `after`, in time order. Each row's
    /// values are in `query.dataset.columns()` order; it comes with its
    /// keyset.
    pub async fn export_page(
        &self,
        organization_id: OrganizationId,
        query: &ExportQuery,
        after: Option<(DateTime<Utc>, Uuid)>,
        limit: i64,
    ) -> Result<Vec<(DateTime<Utc>, Uuid, Vec<Value>)>, sqlx::Error> {
        let sql = match query.dataset {
            ExportDataset::Usage => format!(
                "SELECT u.recorded_at AS at, u.id, jsonb_build_array(
                     u.id, {}, u.reservation_id, r.team_id, r.service_account_id,
                     r.deployment, r.model, btrim(u.currency), u.confidence, u.completeness,
                     u.finality, u.input_tokens, u.output_tokens, u.cached_input_tokens,
                     u.reasoning_tokens, u.other_tokens, u.amount_minor, {batch},
                     bl.custom_id, r.model_master) AS row
                 FROM gateway.accounting_usage u
                 JOIN gateway.accounting_reservation r
                   ON r.organization_id = u.organization_id AND r.id = u.reservation_id
                 LEFT JOIN gateway.batch_line bl
                   ON bl.organization_id = r.organization_id AND bl.reservation_id = r.id
                 WHERE u.organization_id = $1 AND u.recorded_at >= $2 AND u.recorded_at < $3
                   AND ($4::timestamptz IS NULL OR (u.recorded_at, u.id) > ($4, $5))
                   AND ($7::uuid IS NULL OR r.batch_id = $7)
                   AND ($8::text IS NULL OR r.model_master = $8)
                 ORDER BY u.recorded_at, u.id
                 LIMIT $6",
                utc("u.recorded_at"),
                batch = BATCH_ID,
            ),
            ExportDataset::Traces => format!(
                "SELECT t.recorded_at AS at, t.id, jsonb_build_array(
                     t.id, {}, {}, t.reservation_id, t.correlation_id, t.team_id,
                     t.service_account_id, t.status, t.outcome, t.provider_request_id,
                     r.deployment, t.model, t.catalog_family, t.input_bound, t.output_cap,
                     t.price_version, t.pricing_confidence, btrim(t.currency),
                     t.amount_minor, {batch}, bl.custom_id, r.model_master) AS row
                 FROM gateway.inference_trace t
                 JOIN gateway.accounting_reservation r
                   ON r.organization_id = t.organization_id AND r.id = t.reservation_id
                 LEFT JOIN gateway.batch_line bl
                   ON bl.organization_id = r.organization_id AND bl.reservation_id = r.id
                 WHERE t.organization_id = $1 AND t.recorded_at >= $2 AND t.recorded_at < $3
                   AND ($4::timestamptz IS NULL OR (t.recorded_at, t.id) > ($4, $5))
                   AND ($7::uuid IS NULL OR r.batch_id = $7)
                   AND ($8::text IS NULL OR r.model_master = $8)
                 ORDER BY t.recorded_at, t.id
                 LIMIT $6",
                utc("t.recorded_at"),
                utc("t.finalized_at"),
                batch = BATCH_ID,
            ),
            ExportDataset::AuditEvents => format!(
                "SELECT e.occurred_at AS at, e.id, jsonb_build_array(
                     e.id, {}, e.actor_type, e.actor_user_id, e.actor_service_account_id,
                     e.team_id, e.action, e.entity_type, e.entity_id, e.outcome, e.request_id,
                     e.metadata, e.before_metadata, e.after_metadata) AS row
                 FROM gateway.audit_event e
                 WHERE e.organization_id = $1 AND e.occurred_at >= $2 AND e.occurred_at < $3
                   AND ($4::timestamptz IS NULL OR (e.occurred_at, e.id) > ($4, $5))
                   AND $7::uuid IS NULL
                   AND $8::text IS NULL
                 ORDER BY e.occurred_at, e.id
                 LIMIT $6",
                utc("e.occurred_at")
            ),
        };
        let rows = sqlx::query_as::<_, (DateTime<Utc>, Uuid, sqlx::types::Json<Value>)>(&sql)
            .bind(organization_id.0)
            .bind(query.from)
            .bind(query.to)
            .bind(after.map(|(at, _)| at))
            .bind(after.map_or(Uuid::nil(), |(_, id)| id))
            .bind(limit)
            .bind(query.batch)
            .bind(&query.model_master)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(at, id, row)| {
                let values = match row.0 {
                    Value::Array(values) => values,
                    other => vec![other],
                };
                (at, id, values)
            })
            .collect())
    }
}

/// A purge's audit event. Row counts are not audit metadata (its keys are
/// an allowlist); they are in the policy's totals, updated in the same
/// transaction.
fn purge_event(
    organization_id: Uuid,
    policy: Uuid,
    action: &str,
    outcome: AuditOutcome,
    run_id: Uuid,
) -> AdminAuditEvent {
    AdminAuditEvent::new(
        AuditActor::System,
        OrganizationId(organization_id),
        None,
        AuditTargetResource::RetentionPolicy,
        AuditAction::new(action).expect("audit action constant is valid"),
        policy,
        outcome,
        run_id,
        None,
        None,
    )
}

/// Finalized traces recorded before `before`, of reservations that are
/// final: settlement still writes to the trace of one in progress. A trace
/// whose content is still stored waits for the content to expire first
/// (`jobs::trace_content`, which runs before this), so its object is never
/// left without a row.
async fn purge_traces(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    before: DateTime<Utc>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM gateway.inference_trace WHERE id IN (
             SELECT t.id FROM gateway.inference_trace t
             JOIN gateway.accounting_reservation r
               ON r.organization_id = t.organization_id AND r.id = t.reservation_id
             WHERE t.organization_id = $1 AND t.recorded_at < $2 AND t.status = 'finalized'
               AND r.state IN ('settled', 'released', 'unknown', 'expired')
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.inference_trace_content c
                   WHERE c.trace_id = t.id AND c.state IN ('writing', 'stored'))
             ORDER BY t.recorded_at
             LIMIT $3
             FOR UPDATE OF t SKIP LOCKED)",
    )
    .bind(organization_id)
    .bind(before)
    .bind(limit)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

/// Reservations created before `before`, with everything recorded about
/// them. Only final reservations go, and none whose reconciliation item is
/// still open, whose trace's content is still stored, or whose batch has not
/// ended. Their traces are
/// counted as traces.
async fn purge_reservations(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    before: DateTime<Utc>,
    limit: i64,
) -> Result<PurgedCounts, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM gateway.accounting_reservation r
         WHERE r.organization_id = $1 AND r.created_at < $2
           AND r.state IN ('settled', 'released', 'unknown', 'expired')
           AND NOT EXISTS (
               SELECT 1 FROM gateway.accounting_reconciliation c
               WHERE c.reservation_id = r.id AND c.status = 'open')
           AND NOT EXISTS (
               SELECT 1 FROM gateway.inference_trace t
               JOIN gateway.inference_trace_content tc ON tc.trace_id = t.id
               WHERE t.reservation_id = r.id AND tc.state IN ('writing', 'stored'))
           AND NOT EXISTS (
               SELECT 1 FROM gateway.batch b
               WHERE b.organization_id = r.organization_id AND b.id = r.batch_id
                 AND b.status NOT IN ('completed', 'failed', 'expired', 'cancelled'))
         ORDER BY r.created_at
         LIMIT $3
         FOR UPDATE OF r SKIP LOCKED",
    )
    .bind(organization_id)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await?;
    if ids.is_empty() {
        return Ok(PurgedCounts::default());
    }
    let traces = sqlx::query("DELETE FROM gateway.inference_trace WHERE reservation_id = ANY($1)")
        .bind(&ids)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let items: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM gateway.accounting_reconciliation
         WHERE reservation_id = ANY($1) RETURNING id",
    )
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'reconciliation_item'
           AND resource_id = ANY($2)",
    )
    .bind(organization_id)
    .bind(&items)
    .execute(&mut **tx)
    .await?;
    // A batch line is part of its request's record, and goes with it; the
    // batch keeps its counts.
    for table in [
        "batch_line",
        "accounting_allocation",
        "accounting_usage",
        "accounting_price_snapshot",
    ] {
        sqlx::query(&format!(
            "DELETE FROM gateway.{table} WHERE reservation_id = ANY($1)"
        ))
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    }
    // Settlement records cascade.
    let reservations = sqlx::query("DELETE FROM gateway.accounting_reservation WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    Ok(PurgedCounts {
        traces,
        reservations,
        audit_events: 0,
    })
}

/// Audit events that occurred before `before`, and the export records of
/// the same age that no remaining audit event names. An export still
/// `streaming` that old was cut off by a crash; it goes too.
async fn purge_audit_events(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    before: DateTime<Utc>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM gateway.audit_event WHERE id IN (
             SELECT id FROM gateway.audit_event
             WHERE organization_id = $1 AND occurred_at < $2
             ORDER BY occurred_at
             LIMIT $3
             FOR UPDATE SKIP LOCKED)",
    )
    .bind(organization_id)
    .bind(before)
    .bind(limit)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    sqlx::query(
        "DELETE FROM gateway.data_export WHERE id IN (
             SELECT x.id FROM gateway.data_export x
             WHERE x.organization_id = $1 AND x.started_at < $2
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.audit_event e
                   WHERE e.organization_id = x.organization_id
                     AND e.entity_type = 'data_export' AND e.entity_id = x.id)
             ORDER BY x.started_at
             LIMIT $3
             FOR UPDATE SKIP LOCKED)",
    )
    .bind(organization_id)
    .bind(before)
    .bind(limit)
    .execute(&mut **tx)
    .await?;
    Ok(deleted)
}

#[derive(FromRow)]
struct ExportRow {
    id: Uuid,
    dataset: String,
    format: String,
    range_from: DateTime<Utc>,
    range_to: DateTime<Utc>,
    actor_user_id: Option<Uuid>,
    request_id: Uuid,
    status: String,
    rows: i64,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    batch_id: Option<Uuid>,
    model_master: Option<String>,
}

impl From<ExportRow> for ExportRecord {
    fn from(row: ExportRow) -> Self {
        Self {
            id: row.id,
            dataset: row.dataset,
            format: row.format,
            from: row.range_from,
            to: row.range_to,
            actor_user_id: row.actor_user_id,
            request_id: row.request_id,
            status: match row.status.as_str() {
                "completed" => ExportStatus::Completed,
                "failed" => ExportStatus::Failed,
                "abandoned" => ExportStatus::Abandoned,
                _ => ExportStatus::Streaming,
            },
            rows: u64::try_from(row.rows).unwrap_or(0),
            started_at: row.started_at,
            finished_at: row.finished_at,
            batch_id: row.batch_id.map(crate::domain::batch::public_batch_id),
            model_master: row.model_master,
        }
    }
}

impl RetentionStore {
    /// Records an export as it starts, together with its audit event: both
    /// commit or neither does.
    pub async fn start_export(
        &self,
        id: Uuid,
        organization_id: OrganizationId,
        actor: UserAccountId,
        query: &ExportQuery,
        request_id: Uuid,
        audit: &AdminAuditEvent,
    ) -> Result<(), AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO gateway.data_export
                 (id, organization_id, actor_user_id, dataset, format, range_from, range_to,
                  request_id, batch_id, model_master)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(actor.0)
        .bind(query.dataset.as_str())
        .bind(query.format.as_str())
        .bind(query.from)
        .bind(query.to)
        .bind(request_id)
        .bind(query.batch)
        .bind(&query.model_master)
        .execute(&mut *tx)
        .await?;
        insert_audit(&mut tx, audit).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records how a streaming export ended; a finished export is final.
    pub async fn finish_export(
        &self,
        id: Uuid,
        status: ExportStatus,
        rows: u64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE gateway.data_export
             SET status = $2, rows = $3, finished_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND status = 'streaming'",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(i64::try_from(rows).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One page of the organization's exports, newest first, after the
    /// `after` keyset. The flag reports whether more rows follow.
    pub async fn list_exports(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<(Vec<ExportRecord>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, ExportRow>(
            "SELECT id, dataset, format, range_from, range_to, actor_user_id, request_id,
                    status, rows, started_at, finished_at, batch_id, model_master
             FROM gateway.data_export
             WHERE organization_id = $1
               AND ($2::timestamptz IS NULL OR (started_at, id) < ($2, $3))
             ORDER BY started_at DESC, id DESC
             LIMIT $4",
        )
        .bind(organization_id.0)
        .bind(after.map(|(at, _)| at))
        .bind(after.map_or(Uuid::nil(), |(_, id)| id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok((
            rows.into_iter()
                .take(page_size as usize)
                .map(Into::into)
                .collect(),
            has_more,
        ))
    }
}

/// A timestamp column as RFC 3339 UTC text with microseconds, whatever the
/// session time zone.
fn utc(column: &str) -> String {
    format!("to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
}
