//! The PostgreSQL side of the optional ClickHouse analytics sink (D-030;
//! `docs/operations.md`, *Analytics in ClickHouse*).
//!
//! ClickHouse holds one row per request: its reservation, its trace (while
//! kept) and its latest usage, as [`snapshot`] reads them here. A row is
//! (re)written whenever the request may have changed, from the queue
//! (`gateway.analytics_sink_queue`):
//!
//! - [`AnalyticsSink`], an outbox sink, queues the request of every
//!   `inference.finalized` event in the dispatch transaction, beside usage
//!   export and webhooks, in the same savepoint: if queueing failed, the
//!   event's webhooks and export would be retried with it. So queueing is
//!   one append-only insert, which never updates, locks or conflicts with
//!   an existing entry; it fails only when the database itself does, and
//!   then the whole dispatch would.
//! - Triggers queue every other change to what a request's row holds,
//!   since not every change has an event (an operator's resolution of a
//!   request whose trace retention deleted has none): batch lines as they
//!   are admitted and change, a reservation closing or changing once
//!   closed, its trace, usage or reconciliation reason changing once it is
//!   closed (0149), and retention's deletes (0148); likewise append only.
//!   An open reservation's changes need none: reads take open requests
//!   from PostgreSQL.
//!
//! The worker claims queue entries with a lease (`SKIP LOCKED`), reads
//! their requests in one snapshot, writes them to ClickHouse and then
//! removes every entry of those requests that the snapshot saw (it covers
//! them); an entry queued after the snapshot stays for the next round.
//! Every row carries the time of the PostgreSQL read it came from as its
//! version, and ClickHouse keeps the newest, so a retry, a duplicate or the
//! backfill can never put an older state over a newer one: delivery is at
//! least once, and idempotent.
//!
//! Reads ([`AnalyticsSinkStore::overlay`]) take, in one snapshot, the
//! requests ClickHouse may not have caught up with (queued, finalized but
//! not yet dispatched, or still in flight) and read those from PostgreSQL;
//! the query then uses ClickHouse for everything else, so its answers are
//! PostgreSQL's whatever the sink's lag.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::clickhouse::format_time;
use crate::domain::{identity::OrganizationId, outbox::OutboxPayload};
use crate::persistence::outbox::{OutboxEvent, OutboxSink, SinkError};

use super::usage_analytics::{DURATION, STATUS};

/// Longest error stored; the columns enforce the same limit.
const MAX_ERROR_CHARS: usize = 200;
/// Requests read from PostgreSQL in place of ClickHouse for one query. A
/// larger backlog (ClickHouse down, a backfill of the queue) sends reads to
/// PostgreSQL until it drains.
pub const MAX_OVERLAY: i64 = 10_000;

/// Where the sink stands; see migration 0148.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Starting,
    Backfilling,
    Live,
}

impl Phase {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "starting" => Some(Self::Starting),
            "backfilling" => Some(Self::Backfilling),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Backfilling => "backfilling",
            Self::Live => "live",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SinkState {
    pub phase: Phase,
    pub enabled_at: DateTime<Utc>,
    pub instance_id: Option<Uuid>,
    pub heartbeat_at: DateTime<Utc>,
    pub backfill_cursor: Option<Uuid>,
    pub backfill_rows: i64,
    pub lease_owner: Option<Uuid>,
    pub lease_until: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct StateRow {
    phase: String,
    enabled_at: DateTime<Utc>,
    instance_id: Option<Uuid>,
    heartbeat_at: DateTime<Utc>,
    backfill_cursor: Option<Uuid>,
    backfill_rows: i64,
    lease_owner: Option<Uuid>,
    lease_until: Option<DateTime<Utc>>,
}

impl StateRow {
    fn into_state(self) -> Option<SinkState> {
        Some(SinkState {
            phase: Phase::parse(&self.phase)?,
            enabled_at: self.enabled_at,
            instance_id: self.instance_id,
            heartbeat_at: self.heartbeat_at,
            backfill_cursor: self.backfill_cursor,
            backfill_rows: self.backfill_rows,
            lease_owner: self.lease_owner,
            lease_until: self.lease_until,
        })
    }
}

const STATE_SELECT: &str = "SELECT phase, enabled_at, instance_id, heartbeat_at, backfill_cursor,
    backfill_rows, lease_owner, lease_until FROM gateway.analytics_sink";

/// A queue entry, claimed by one worker until its lease ends.
#[derive(Clone, Debug, FromRow)]
pub struct Claimed {
    pub id: i64,
    pub organization_id: Uuid,
    pub reservation_id: Uuid,
    pub reservation_created_at: DateTime<Utc>,
    pub attempts: i32,
}

/// A request as PostgreSQL has it now, in the columns of the ClickHouse
/// `requests` table. Each column is what the PostgreSQL report, dashboard
/// and trace queries compute for it, so the same query over these rows
/// gives the same answer.
#[derive(Clone, Debug, FromRow)]
pub struct Snapshot {
    pub organization_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub reservation_id: Uuid,
    /// Microseconds since the epoch of the PostgreSQL statement that read
    /// it: ClickHouse keeps the newest.
    pub version: i64,
    pub trace_id: Option<Uuid>,
    pub recorded_at: Option<DateTime<Utc>>,
    pub correlation_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub service_account_id: Option<Uuid>,
    /// The reservation's own attribution, which the usage report uses.
    pub reservation_team_id: Option<Uuid>,
    pub reservation_service_account_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub end_user: Option<String>,
    pub model: Option<String>,
    pub reservation_model: String,
    pub alias: Option<String>,
    pub deployment: String,
    pub provider_family: Option<String>,
    pub location: Option<String>,
    pub operation: Option<String>,
    pub request_status: Option<String>,
    pub trace_status: Option<String>,
    pub outcome: Option<String>,
    pub error_category: Option<String>,
    pub trace_currency: Option<String>,
    pub trace_amount_minor: Option<i64>,
    pub duration_ms: Option<i64>,
    pub batch_id: Option<Uuid>,
    pub model_master: Option<String>,
    pub has_usage: bool,
    pub usage_currency: Option<String>,
    pub usage_confidence: Option<String>,
    pub usage_completeness: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub other_tokens: Option<i64>,
    pub usage_amount_minor: Option<i64>,
    pub usage_attributed_at: Option<DateTime<Utc>>,
}

/// A row as ClickHouse takes it (JSONEachRow). The overlay external table
/// has the same columns without `version` and `is_deleted`.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub organization_id: Uuid,
    pub created_at: String,
    pub reservation_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_deleted: Option<u8>,
    pub has_trace: u8,
    pub trace_id: Option<Uuid>,
    pub recorded_at: Option<String>,
    pub correlation_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub service_account_id: Option<Uuid>,
    /// The reservation's own attribution, which the usage report uses.
    pub reservation_team_id: Option<Uuid>,
    pub reservation_service_account_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub end_user: Option<String>,
    pub model: Option<String>,
    pub reservation_model: String,
    pub alias: Option<String>,
    pub deployment: String,
    pub provider_family: Option<String>,
    pub location: Option<String>,
    pub operation: Option<String>,
    pub request_status: Option<String>,
    pub trace_status: Option<String>,
    pub outcome: Option<String>,
    pub error_category: Option<String>,
    pub trace_currency: Option<String>,
    pub trace_amount_minor: Option<i64>,
    pub duration_ms: Option<i64>,
    pub batch_id: Option<Uuid>,
    pub model_master: Option<String>,
    pub has_usage: u8,
    pub usage_currency: Option<String>,
    pub usage_confidence: Option<String>,
    pub usage_completeness: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub other_tokens: Option<i64>,
    pub usage_amount_minor: Option<i64>,
    pub usage_attributed_at: Option<String>,
}

/// The overlay external table's structure: the `requests` columns a read
/// uses, in [`Row`]'s order (without `version` and `is_deleted`).
pub const ROW_STRUCTURE: &str = "organization_id UUID, created_at DateTime64(6, 'UTC'), \
    reservation_id UUID, has_trace UInt8, trace_id Nullable(UUID), \
    recorded_at Nullable(DateTime64(6, 'UTC')), correlation_id Nullable(UUID), \
    team_id Nullable(UUID), service_account_id Nullable(UUID), reservation_team_id Nullable(UUID), \
    reservation_service_account_id Nullable(UUID), api_key_id Nullable(UUID), \
    end_user Nullable(String), model Nullable(String), reservation_model String, \
    alias Nullable(String), deployment String, provider_family Nullable(String), \
    location Nullable(String), operation Nullable(String), request_status Nullable(String), \
    trace_status Nullable(String), outcome Nullable(String), error_category Nullable(String), \
    trace_currency Nullable(String), trace_amount_minor Nullable(Int64), \
    duration_ms Nullable(Int64), batch_id Nullable(UUID), model_master Nullable(String), \
    has_usage UInt8, usage_currency Nullable(String), usage_confidence Nullable(String), \
    usage_completeness Nullable(String), input_tokens Nullable(Int64), \
    output_tokens Nullable(Int64), cached_input_tokens Nullable(Int64), \
    reasoning_tokens Nullable(Int64), other_tokens Nullable(Int64), \
    usage_amount_minor Nullable(Int64), usage_attributed_at Nullable(DateTime64(6, 'UTC'))";

/// The same columns, for `SELECT` lists.
pub const ROW_COLUMNS: &str = "organization_id, created_at, reservation_id, has_trace, \
    trace_id, recorded_at, correlation_id, team_id, service_account_id, reservation_team_id, \
    reservation_service_account_id, api_key_id, end_user, \
    model, reservation_model, alias, deployment, provider_family, location, operation, \
    request_status, trace_status, outcome, error_category, trace_currency, trace_amount_minor, \
    duration_ms, batch_id, model_master, has_usage, usage_currency, usage_confidence, \
    usage_completeness, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, \
    other_tokens, usage_amount_minor, usage_attributed_at";

impl Snapshot {
    /// The row to write, versioned.
    pub fn into_row(self) -> Row {
        let version = u64::try_from(self.version).unwrap_or(0);
        let mut row = self.into_overlay_row();
        row.version = Some(version);
        row.is_deleted = Some(0);
        row
    }

    /// The row as an overlay table carries it.
    pub fn into_overlay_row(self) -> Row {
        Row {
            organization_id: self.organization_id,
            created_at: format_time(self.created_at),
            reservation_id: self.reservation_id,
            version: None,
            is_deleted: None,
            has_trace: u8::from(self.trace_id.is_some() && self.recorded_at.is_some()),
            trace_id: self.trace_id,
            recorded_at: self.recorded_at.map(format_time),
            correlation_id: self.correlation_id,
            team_id: self.team_id,
            service_account_id: self.service_account_id,
            reservation_team_id: self.reservation_team_id,
            reservation_service_account_id: self.reservation_service_account_id,
            api_key_id: self.api_key_id,
            end_user: self.end_user,
            model: self.model,
            reservation_model: self.reservation_model,
            alias: self.alias,
            deployment: self.deployment,
            provider_family: self.provider_family,
            location: self.location,
            operation: self.operation,
            request_status: self.request_status,
            trace_status: self.trace_status,
            outcome: self.outcome,
            error_category: self.error_category,
            trace_currency: self.trace_currency,
            trace_amount_minor: self.trace_amount_minor,
            duration_ms: self.duration_ms,
            batch_id: self.batch_id,
            model_master: self.model_master,
            has_usage: u8::from(self.has_usage),
            usage_currency: self.usage_currency,
            usage_confidence: self.usage_confidence,
            usage_completeness: self.usage_completeness,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            reasoning_tokens: self.reasoning_tokens,
            other_tokens: self.other_tokens,
            usage_amount_minor: self.usage_amount_minor,
            usage_attributed_at: self.usage_attributed_at.map(format_time),
        }
    }
}

/// A request PostgreSQL no longer has: written deleted, at `version`.
pub fn tombstone(claimed: &Claimed, version: u64) -> Row {
    Row {
        organization_id: claimed.organization_id,
        created_at: format_time(claimed.reservation_created_at),
        reservation_id: claimed.reservation_id,
        version: Some(version),
        is_deleted: Some(1),
        has_trace: 0,
        trace_id: None,
        recorded_at: None,
        correlation_id: None,
        team_id: None,
        service_account_id: None,
        reservation_team_id: None,
        reservation_service_account_id: None,
        api_key_id: None,
        end_user: None,
        model: None,
        reservation_model: String::new(),
        alias: None,
        deployment: String::new(),
        provider_family: None,
        location: None,
        operation: None,
        request_status: None,
        trace_status: None,
        outcome: None,
        error_category: None,
        trace_currency: None,
        trace_amount_minor: None,
        duration_ms: None,
        batch_id: None,
        model_master: None,
        has_usage: 0,
        usage_currency: None,
        usage_confidence: None,
        usage_completeness: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        reasoning_tokens: None,
        other_tokens: None,
        usage_amount_minor: None,
        usage_attributed_at: None,
    }
}

/// One request per reservation in `$1` (and, when `$2` is set, of that
/// organization only), with the joins and expressions the PostgreSQL
/// reads use: the trace's status and duration as the dashboard computes
/// them, the reservation's attribution as the usage report uses it, and the
/// latest usage row attributed to the time of the first.
fn snapshot_sql() -> String {
    format!(
        "SELECT r.organization_id, r.created_at, r.id AS reservation_id,
                floor(extract(epoch FROM statement_timestamp()) * 1000000)::bigint AS version,
                t.id AS trace_id, t.recorded_at, t.correlation_id,
                CASE WHEN t.id IS NULL THEN r.team_id ELSE t.team_id END AS team_id,
                CASE WHEN t.id IS NULL THEN r.service_account_id ELSE t.service_account_id END
                  AS service_account_id,
                r.team_id AS reservation_team_id,
                r.service_account_id AS reservation_service_account_id,
                t.api_key_id,
                (SELECT eu.external_id FROM gateway.end_user eu WHERE eu.id = t.end_user_id)
                  AS end_user,
                t.model, r.model AS reservation_model, cr.alias, r.deployment,
                r.catalog_family AS provider_family, d.location,
                t.catalog_operation AS operation,
                CASE WHEN t.id IS NOT NULL THEN {STATUS} END AS request_status,
                t.status AS trace_status, t.outcome, t.provider_error_category AS error_category,
                btrim(t.currency) AS trace_currency, t.amount_minor AS trace_amount_minor,
                {DURATION} AS duration_ms, r.batch_id, r.model_master,
                lu.usage_id IS NOT NULL AS has_usage, lu.usage_currency,
                lu.confidence AS usage_confidence, lu.completeness AS usage_completeness,
                lu.input_tokens, lu.output_tokens, lu.cached_input_tokens, lu.reasoning_tokens,
                lu.other_tokens, lu.usage_amount_minor, lu.attributed_at AS usage_attributed_at
         FROM gateway.accounting_reservation r
         LEFT JOIN gateway.inference_trace t
           ON t.organization_id = r.organization_id AND t.reservation_id = r.id
         LEFT JOIN gateway.catalog_deployment d
           ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
          AND d.id = r.catalog_deployment_id
         LEFT JOIN gateway.catalog_route cr
           ON cr.organization_id = t.organization_id AND cr.id = t.catalog_route_id
         LEFT JOIN LATERAL (
           SELECT u.id AS usage_id, btrim(u.currency) AS usage_currency, u.confidence,
                  u.completeness, u.input_tokens, u.output_tokens, u.cached_input_tokens,
                  u.reasoning_tokens, u.other_tokens, u.amount_minor AS usage_amount_minor,
                  (SELECT min(w.recorded_at) FROM gateway.accounting_usage w
                   WHERE w.organization_id = r.organization_id AND w.reservation_id = r.id)
                    AS attributed_at
           FROM gateway.accounting_usage u
           WHERE u.organization_id = r.organization_id AND u.reservation_id = r.id
           ORDER BY u.recorded_at DESC, u.id DESC
           LIMIT 1
         ) lu ON true
         WHERE r.id = ANY($1) AND ($2::uuid IS NULL OR r.organization_id = $2)"
    )
}

/// Reads the requests of `reservations` as PostgreSQL has them now.
/// Reservations that no longer exist are left out.
pub async fn snapshot(
    conn: &mut PgConnection,
    reservations: &[Uuid],
    organization_id: Option<OrganizationId>,
) -> Result<Vec<Snapshot>, sqlx::Error> {
    if reservations.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, Snapshot>(&snapshot_sql())
        .bind(reservations)
        .bind(organization_id.map(|id| id.0))
        .fetch_all(conn)
        .await
}

/// How long a process trusts that the sink is off before looking again.
pub const OFF_CACHE: Duration = Duration::from_secs(1);
/// A new sink waits this long before copying history, so every process
/// has stopped trusting an "off" it saw before and queues every event
/// dispatched after the copy starts.
pub const ENABLE_SETTLE: chrono::Duration = chrono::Duration::seconds(5);

/// When this process last found the sink off; `None` once it found it on.
static SEEN_OFF: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether the sink may exist: a deployment without it pays one lookup a
/// second per process, not one per event.
async fn may_exist(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    {
        let seen = SEEN_OFF
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if seen.is_some_and(|at| at.elapsed() < OFF_CACHE) {
            return Ok(false);
        }
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM gateway.analytics_sink)")
        .fetch_one(conn)
        .await?;
    *SEEN_OFF
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = (!exists).then(Instant::now);
    Ok(exists)
}

/// Queues the request of each `inference.finalized` event while the sink
/// exists. One insert in the dispatch transaction: ClickHouse is written by
/// the worker's own loop, so it being slow or down never holds dispatch.
pub struct AnalyticsSink;

#[async_trait]
impl OutboxSink for AnalyticsSink {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        let OutboxPayload::InferenceFinalized(finalized) = &event.payload else {
            return Ok(());
        };
        let failed = |_| SinkError::new("analytics sink queueing failed");
        if !may_exist(&mut *conn).await.map_err(failed)? {
            return Ok(());
        }
        // Append only: a new entry, never an update of one the worker
        // may hold, so this cannot wait on or deadlock with the worker.
        sqlx::query(
            "INSERT INTO gateway.analytics_sink_queue
                 (organization_id, reservation_id, reservation_created_at)
             SELECT r.organization_id, r.id, r.created_at
             FROM gateway.accounting_reservation r
             WHERE r.organization_id = $1 AND r.id = $2
               AND EXISTS (SELECT 1 FROM gateway.analytics_sink)",
        )
        .bind(event.organization_id.0)
        .bind(finalized.reservation_id)
        .execute(conn)
        .await
        .map_err(failed)?;
        Ok(())
    }
}

/// Requests read from PostgreSQL for one ClickHouse query: `ids` are left
/// out of ClickHouse's rows, and `rows` (those that still exist) stand in
/// for them.
#[derive(Debug, Default)]
pub struct Overlay {
    /// The ClickHouse store the copy was made to (its `gateway_meta` id),
    /// which the read must find before it trusts the copy.
    pub instance_id: Uuid,
    pub ids: Vec<Uuid>,
    pub rows: Vec<Snapshot>,
    /// Each service account's current owner, when the query needs users.
    pub owners: Vec<(Uuid, Option<Uuid>)>,
}

/// Why a read could not use ClickHouse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotReady {
    /// The sink is off or still copying history.
    NotLive,
    /// More than [`MAX_OVERLAY`] requests are not caught up.
    Behind,
    /// The ClickHouse this process reads is not the store the copy was
    /// made to (a URL being moved, a store emptied or replaced).
    OtherStore,
}

impl NotReady {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotLive => "not_live",
            Self::Behind => "behind",
            Self::OtherStore => "other_store",
        }
    }
}

/// [`AnalyticsSinkStore::read_queued`]: the requests as one snapshot has
/// them, the version for those gone, and the queue entries the snapshot
/// covers, with their requests.
#[derive(Debug)]
pub struct QueuedRead {
    pub rows: Vec<Snapshot>,
    pub version: u64,
    pub covered: Vec<(i64, Uuid)>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueStats {
    pub depth: i64,
    pub oldest_seconds: i64,
}

#[derive(Clone)]
pub struct AnalyticsSinkStore {
    pool: PgPool,
}

impl AnalyticsSinkStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn state(&self) -> Result<Option<SinkState>, sqlx::Error> {
        let row: Option<StateRow> = sqlx::query_as(STATE_SELECT)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(StateRow::into_state))
    }

    /// Turns the sink on if it is off (it starts by emptying ClickHouse and
    /// copying history), and records that a configured worker is running.
    /// Returns whether this call turned it on.
    pub async fn enable(&self) -> Result<bool, sqlx::Error> {
        let created = sqlx::query(
            "INSERT INTO gateway.analytics_sink (id) VALUES (true) ON CONFLICT (id) DO NOTHING",
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;
        if !created {
            sqlx::query("UPDATE gateway.analytics_sink SET heartbeat_at = CURRENT_TIMESTAMP")
                .execute(&self.pool)
                .await?;
        }
        Ok(created)
    }

    /// Turns the sink off, discarding its queue, when no configured worker
    /// has been seen since `stale_before`. Run by workers without the
    /// setting, so removing it from every worker stops the queueing.
    pub async fn disable_if_abandoned(
        &self,
        stale_before: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM gateway.analytics_sink WHERE heartbeat_at < $1")
            .bind(stale_before)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        if deleted {
            sqlx::query("DELETE FROM gateway.analytics_sink_queue")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// Takes (or renews) the lease on `starting` or `backfilling` for
    /// `owner`; `None` when another worker holds it or the sink is live.
    pub async fn claim_lease(
        &self,
        owner: Uuid,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<SinkState>, sqlx::Error> {
        let row: Option<StateRow> = sqlx::query_as(
            "UPDATE gateway.analytics_sink SET lease_owner = $1, lease_until = $3
             WHERE phase <> 'live'
               AND (lease_owner IS NULL OR lease_owner = $1 OR lease_until IS NULL
                    OR lease_until <= $2)
             RETURNING phase, enabled_at, instance_id, heartbeat_at, backfill_cursor, backfill_rows,
                       lease_owner, lease_until",
        )
        .bind(owner)
        .bind(now)
        .bind(until)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(StateRow::into_state))
    }

    /// ClickHouse was emptied for store `instance_id`: copy history next.
    pub async fn start_backfill(
        &self,
        owner: Uuid,
        instance_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.analytics_sink
             SET phase = 'backfilling', instance_id = $2, backfill_cursor = NULL,
                 backfill_rows = 0, backfill_started_at = CURRENT_TIMESTAMP,
                 backfill_completed_at = NULL
             WHERE phase = 'starting' AND lease_owner = $1",
        )
        .bind(owner)
        .bind(instance_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// The next `limit` reservations after `cursor`, in id order, across
    /// organizations.
    pub async fn backfill_ids(
        &self,
        cursor: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT id FROM gateway.accounting_reservation
             WHERE ($1::uuid IS NULL OR id > $1)
             ORDER BY id LIMIT $2",
        )
        .bind(cursor)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Requests as they are now, for writing, and a version later than the
    /// read for those that no longer exist (deletion is final).
    pub async fn read(&self, reservations: &[Uuid]) -> Result<(Vec<Snapshot>, u64), sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        let rows = snapshot(&mut conn, reservations, None).await?;
        let now: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM statement_timestamp()) * 1000000)::bigint",
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok((rows, u64::try_from(now).unwrap_or(0)))
    }

    /// Records a backfill page written up to `cursor`; with `done`, the
    /// copy is complete and reads may use ClickHouse. False when this
    /// worker no longer holds the lease (its page was still a valid write).
    pub async fn advance_backfill(
        &self,
        owner: Uuid,
        cursor: Uuid,
        rows: i64,
        done: bool,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.analytics_sink
             SET backfill_cursor = $2, backfill_rows = backfill_rows + $3,
                 phase = CASE WHEN $4 THEN 'live' ELSE phase END,
                 backfill_completed_at = CASE WHEN $4 THEN CURRENT_TIMESTAMP END,
                 lease_owner = CASE WHEN $4 THEN NULL ELSE lease_owner END,
                 lease_until = CASE WHEN $4 THEN NULL ELSE lease_until END
             WHERE phase = 'backfilling' AND lease_owner = $1",
        )
        .bind(owner)
        .bind(cursor)
        .bind(rows)
        .bind(done)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// History is copied with nothing to copy.
    pub async fn finish_empty_backfill(&self, owner: Uuid) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.analytics_sink
             SET phase = 'live', backfill_completed_at = CURRENT_TIMESTAMP,
                 lease_owner = NULL, lease_until = NULL
             WHERE phase = 'backfilling' AND lease_owner = $1",
        )
        .bind(owner)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// The ClickHouse store is not the one copied to (emptied, replaced or
    /// pointed elsewhere): start again. Reads use PostgreSQL meanwhile.
    pub async fn restart(&self, expected: Option<Uuid>) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.analytics_sink
             SET phase = 'starting', instance_id = NULL, backfill_cursor = NULL,
                 backfill_rows = 0, backfill_started_at = NULL, backfill_completed_at = NULL,
                 lease_owner = NULL, lease_until = NULL
             WHERE phase <> 'starting' AND instance_id IS NOT DISTINCT FROM $1",
        )
        .bind(expected)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Claims up to `limit` due queue entries until `lease_until`,
    /// counting the attempt. `SKIP LOCKED`: workers never wait on each
    /// other's entries.
    pub async fn claim(
        &self,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Claimed>, sqlx::Error> {
        sqlx::query_as(
            "UPDATE gateway.analytics_sink_queue q
             SET next_attempt_at = $2, attempts = q.attempts + 1
             FROM (
                 SELECT id FROM gateway.analytics_sink_queue
                 WHERE next_attempt_at <= $1
                 ORDER BY next_attempt_at, id
                 LIMIT $3
                 FOR UPDATE SKIP LOCKED
             ) due
             WHERE q.id = due.id
             RETURNING q.id, q.organization_id, q.reservation_id, q.reservation_created_at,
                       q.attempts",
        )
        .bind(now)
        .bind(lease_until)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// The claimed entries' requests, read in one snapshot, for writing;
    /// a version later than the read for those that no longer exist
    /// (deletion is final); and every queue entry of those requests visible
    /// in that snapshot. An entry commits with the change it stands for (a
    /// trigger's), or after it (the dispatch of the change's event), so a
    /// snapshot that sees the entry sees the change: the read covers every
    /// entry it saw, and once the write succeeds they may all go. An entry
    /// queued after the snapshot stays for the next round.
    pub async fn read_queued(&self, claimed: &[Claimed]) -> Result<QueuedRead, sqlx::Error> {
        let mut requests: Vec<(Uuid, Uuid)> = claimed
            .iter()
            .map(|c| (c.organization_id, c.reservation_id))
            .collect();
        requests.sort_unstable();
        requests.dedup();
        let organizations: Vec<Uuid> = requests.iter().map(|(org, _)| *org).collect();
        let reservations: Vec<Uuid> = requests.iter().map(|(_, id)| *id).collect();
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let rows = snapshot(&mut tx, &reservations, None).await?;
        // By (organization, reservation), the queue's request index.
        let covered: Vec<(i64, Uuid)> = sqlx::query_as(
            "SELECT q.id, q.reservation_id
             FROM unnest($1::uuid[], $2::uuid[]) AS c(o, r)
             JOIN gateway.analytics_sink_queue q
               ON q.organization_id = c.o AND q.reservation_id = c.r",
        )
        .bind(&organizations)
        .bind(&reservations)
        .fetch_all(&mut *tx)
        .await?;
        let now: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM statement_timestamp()) * 1000000)::bigint",
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(QueuedRead {
            rows,
            version: u64::try_from(now).unwrap_or(0),
            covered,
        })
    }

    /// The entries `ids` are written: they leave the queue. Rows are
    /// locked in id order, as [`Self::record_failure`] does, so two
    /// workers whose entries overlap never deadlock. Returns how many left
    /// (fewer when another worker's read covered some first).
    pub async fn acknowledge(&self, ids: &[i64]) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let removed = sqlx::query(
            "DELETE FROM gateway.analytics_sink_queue q
             USING (
                 SELECT id FROM gateway.analytics_sink_queue
                 WHERE id = ANY($1)
                 ORDER BY id
                 FOR UPDATE
             ) c
             WHERE q.id = c.id",
        )
        .bind(ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "UPDATE gateway.analytics_sink
             SET last_success_at = CURRENT_TIMESTAMP, failing_since = NULL, last_error = NULL",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// The write failed: the entries `(id, attempts)` are retried after
    /// `retry_at(attempts)`, and the sink is failing from now unless it
    /// already was.
    pub async fn record_failure(
        &self,
        entries: &[(i64, i32)],
        reason: &str,
        retry_at: impl Fn(i32) -> DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let reason: String = reason.chars().take(MAX_ERROR_CHARS).collect();
        let ids: Vec<i64> = entries.iter().map(|(id, _)| *id).collect();
        let retries: Vec<DateTime<Utc>> = entries
            .iter()
            .map(|(_, attempts)| retry_at(*attempts))
            .collect();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE gateway.analytics_sink_queue q
             SET next_attempt_at = f.at, last_error = $3
             FROM (
                 SELECT x.id, c.at
                 FROM gateway.analytics_sink_queue x
                 JOIN unnest($1::bigint[], $2::timestamptz[]) AS c(id, at) ON c.id = x.id
                 ORDER BY x.id
                 FOR UPDATE OF x
             ) f
             WHERE q.id = f.id",
        )
        .bind(&ids)
        .bind(&retries)
        .bind(&reason)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE gateway.analytics_sink
             SET failing_since = COALESCE(failing_since, CURRENT_TIMESTAMP), last_error = $1",
        )
        .bind(&reason)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// Records an error outside the queue (the backfill, the store check).
    pub async fn record_error(&self, reason: &str) -> Result<(), sqlx::Error> {
        let reason: String = reason.chars().take(MAX_ERROR_CHARS).collect();
        sqlx::query(
            "UPDATE gateway.analytics_sink
             SET failing_since = COALESCE(failing_since, CURRENT_TIMESTAMP), last_error = $1",
        )
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn queue_stats(&self) -> Result<QueueStats, sqlx::Error> {
        let (depth, oldest_seconds): (i64, i64) = sqlx::query_as(
            "SELECT count(*),
                    COALESCE(floor(EXTRACT(EPOCH FROM now() - min(queued_at)))::bigint, 0)
             FROM gateway.analytics_sink_queue",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(QueueStats {
            depth,
            oldest_seconds,
        })
    }

    /// For a read of `organization_id`: whether ClickHouse may serve it,
    /// and the requests to read from PostgreSQL instead, all from one
    /// snapshot. A request is in the overlay while it is queued (its row
    /// may be out of date), while its `inference.finalized` event waits to
    /// be dispatched, and while its reservation is open (in flight; batch
    /// lines are queued instead, since they stay open for hours).
    pub async fn overlay(
        &self,
        organization_id: OrganizationId,
        with_owners: bool,
    ) -> Result<Result<Overlay, NotReady>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let state: Option<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT phase, instance_id FROM gateway.analytics_sink")
                .fetch_optional(&mut *tx)
                .await?;
        let Some((phase, Some(instance_id))) = state else {
            tx.commit().await?;
            return Ok(Err(NotReady::NotLive));
        };
        if phase != Phase::Live.as_str() {
            tx.commit().await?;
            return Ok(Err(NotReady::NotLive));
        }
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT reservation_id FROM gateway.analytics_sink_queue WHERE organization_id = $1
             UNION
             SELECT (payload->>'reservation_id')::uuid FROM gateway.outbox_event
             WHERE dispatched_at IS NULL AND organization_id = $1
               AND event_type = 'inference.finalized'
             UNION
             SELECT id FROM gateway.accounting_reservation
             WHERE organization_id = $1 AND state IN ('reserved', 'claimed') AND batch_id IS NULL
             LIMIT $2",
        )
        .bind(organization_id.0)
        .bind(MAX_OVERLAY + 1)
        .fetch_all(&mut *tx)
        .await?;
        if ids.len() as i64 > MAX_OVERLAY {
            tx.commit().await?;
            return Ok(Err(NotReady::Behind));
        }
        let rows = snapshot(&mut tx, &ids, Some(organization_id)).await?;
        let owners = if with_owners {
            sqlx::query_as(
                "SELECT id, owner_user_id FROM gateway.service_account WHERE organization_id = $1",
            )
            .bind(organization_id.0)
            .fetch_all(&mut *tx)
            .await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        Ok(Ok(Overlay {
            instance_id,
            ids,
            rows,
            owners,
        }))
    }
}
