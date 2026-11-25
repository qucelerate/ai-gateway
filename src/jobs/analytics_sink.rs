//! The worker's side of the ClickHouse analytics sink (D-030;
//! `docs/operations.md`, *Analytics in ClickHouse*): applies ClickHouse's
//! schema, turns the sink on, copies history (the backfill) and writes
//! queued requests.
//!
//! Every write is a request read from PostgreSQL just before, versioned by
//! that read's time, so writing a request twice, out of order, or once from
//! the queue and once from the backfill leaves ClickHouse with its newest
//! state. A worker that dies mid-write leaves its claimed requests queued;
//! they are claimed again when the lease ends. ClickHouse being down only
//! delays this loop: dispatch, settlement and the other jobs never wait on
//! it.

use std::{
    collections::HashSet,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::clickhouse::{ClickHouse, ClickHouseError, META_TABLE, REQUESTS_TABLE};
use crate::metrics::{
    ANALYTICS_SINK_ERRORS, ANALYTICS_SINK_LAG, ANALYTICS_SINK_PHASE, ANALYTICS_SINK_QUEUED,
    ANALYTICS_SINK_ROWS, global as metrics,
};
use crate::persistence::analytics_sink::{
    AnalyticsSinkStore, Claimed, ENABLE_SETTLE, Phase, Row, tombstone,
};
use crate::persistence::outbox::backoff;

/// Requests claimed and written per round.
pub const WRITE_BATCH: i64 = 2_000;
/// Reservations copied per backfill page.
pub const BACKFILL_PAGE: i64 = 5_000;
/// How long a claim or the backfill lease lasts: longer than a ClickHouse
/// request may take (`clickhouse::REQUEST_TIMEOUT`).
const LEASE: Duration = Duration::from_secs(5 * 60);
/// Below `clickhouse::REQUEST_TIMEOUT`; see [`AnalyticsSinkRunner::start`].
const TRUNCATE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const STORE_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// A worker without the setting turns the sink off once no configured
/// worker has been seen for this long.
pub const ABANDONED_AFTER: chrono::Duration = chrono::Duration::hours(1);
/// Rounds of writes (or backfill pages) per tick, so one tick is bounded.
const MAX_ROUNDS: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum SinkJobError {
    #[error("analytics sink database error")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    ClickHouse(#[from] ClickHouseError),
}

/// What one tick did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TickReport {
    pub written: u64,
    pub deleted: u64,
    pub backfilled: u64,
    pub failed: u64,
    /// Queue entries claimed.
    pub claimed: u64,
}

impl TickReport {
    fn add(&mut self, other: TickReport) {
        self.written += other.written;
        self.deleted += other.deleted;
        self.backfilled += other.backfilled;
        self.failed += other.failed;
        self.claimed += other.claimed;
    }
}

/// Runs the sink for one worker.
pub struct AnalyticsSinkRunner {
    store: AnalyticsSinkStore,
    /// `None` on a worker without `GATEWAY_CLICKHOUSE_URL`: it only turns
    /// an abandoned sink off.
    clickhouse: Option<ClickHouse>,
    /// This worker's id for the backfill lease.
    owner: Uuid,
    /// Cleared when ClickHouse reports a missing table or database, so the
    /// schema is applied again.
    schema_ready: AtomicBool,
    heartbeat_at: Mutex<Option<Instant>>,
    checked_at: Mutex<Option<Instant>>,
}

impl AnalyticsSinkRunner {
    pub fn new(store: AnalyticsSinkStore, clickhouse: Option<ClickHouse>) -> Self {
        Self {
            store,
            clickhouse,
            owner: Uuid::new_v4(),
            schema_ready: AtomicBool::new(false),
            heartbeat_at: Mutex::new(None),
            checked_at: Mutex::new(None),
        }
    }

    pub fn store(&self) -> &AnalyticsSinkStore {
        &self.store
    }

    pub fn configured(&self) -> bool {
        self.clickhouse.is_some()
    }

    /// One tick: on a configured worker, schema, heartbeat, the phase's
    /// work and the queue; otherwise, turning off an abandoned sink.
    pub async fn tick(&self) -> Result<TickReport, SinkJobError> {
        let Some(clickhouse) = self.clickhouse.clone() else {
            if self
                .store
                .disable_if_abandoned(Utc::now() - ABANDONED_AFTER)
                .await?
            {
                tracing::warn!(
                    "analytics sink turned off: no worker with GATEWAY_CLICKHOUSE_URL for an hour"
                );
            }
            return Ok(TickReport::default());
        };
        // Before the schema: a configured worker keeps the sink on (and
        // requests queued) while ClickHouse is unreachable.
        if due(&self.heartbeat_at, HEARTBEAT_INTERVAL) {
            if self.store.enable().await? {
                tracing::info!("analytics sink turned on; copying history to ClickHouse");
            }
            stamp(&self.heartbeat_at);
        }
        if !self.schema_ready.load(Ordering::Relaxed) {
            let applied = clickhouse.migrate().await.inspect_err(|_| {
                metrics().inc(&ANALYTICS_SINK_ERRORS, &["schema"]);
            })?;
            if applied > 0 {
                tracing::info!(applied, "ClickHouse schema migrations applied");
            }
            self.schema_ready.store(true, Ordering::Relaxed);
            // The schema was (re)applied: check the store's id now rather
            // than within the minute, in case a table had gone.
            *self
                .checked_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
        let mut report = TickReport::default();
        let state = self.store.state().await?;
        if state.is_none() {
            set_phase(None);
        }
        if let Some(state) = &state {
            set_phase(Some(state.phase));
            match state.phase {
                Phase::Starting => self.start(&clickhouse).await.inspect_err(|error| {
                    if let SinkJobError::ClickHouse(error) = error {
                        self.note(error);
                    }
                })?,
                Phase::Backfilling | Phase::Live => {
                    if self.check_store(&clickhouse, state.instance_id).await?
                        && state.phase == Phase::Backfilling
                    {
                        report.add(self.backfill(&clickhouse).await?);
                    }
                }
            }
        }
        // While `starting`, ClickHouse is about to be emptied (and the
        // backfill copies every request anyway): queued requests wait.
        if state
            .as_ref()
            .is_some_and(|state| state.phase != Phase::Starting)
        {
            report.add(self.drain(&clickhouse).await?);
        }
        let stats = self.store.queue_stats().await?;
        metrics().set(&ANALYTICS_SINK_QUEUED, &[], stats.depth);
        metrics().set(&ANALYTICS_SINK_LAG, &[], stats.oldest_seconds);
        Ok(report)
    }

    /// A table or the database is missing (ClickHouse was emptied or
    /// replaced): apply the schema again next tick, after which the store
    /// check finds no id and the sink starts again.
    fn note(&self, error: &ClickHouseError) {
        if crate::clickhouse::missing(error) {
            self.schema_ready.store(false, Ordering::Relaxed);
        }
    }

    /// `starting`: under the lease, makes sure ClickHouse has a store id,
    /// empties the requests table (a fresh copy follows) and moves to the
    /// backfill.
    async fn start(&self, clickhouse: &ClickHouse) -> Result<(), SinkJobError> {
        let now = Utc::now();
        let until = now + chrono::Duration::from_std(LEASE).unwrap_or(chrono::Duration::minutes(5));
        let Some(state) = self.store.claim_lease(self.owner, now, until).await? else {
            return Ok(());
        };
        // Another worker may have moved on meanwhile (its lease ended
        // first): never empty a copy in progress.
        if state.phase != Phase::Starting || now - state.enabled_at < ENABLE_SETTLE {
            return Ok(());
        }
        let instance = match clickhouse.store_id().await? {
            Some(id) => id,
            None => {
                #[derive(serde::Serialize)]
                struct Meta {
                    id: u8,
                    instance_id: Uuid,
                }
                clickhouse
                    .insert(
                        META_TABLE,
                        [Meta {
                            id: 1,
                            instance_id: Uuid::new_v4(),
                        }],
                    )
                    .await?;
                clickhouse
                    .store_id()
                    .await?
                    .ok_or(ClickHouseError::Decode)?
            }
        };
        // The lock wait is capped below the client's timeout, so the
        // TRUNCATE either finishes or fails while this worker still waits
        // for it: one that ran on after the client gave up could empty
        // rows the backfill had copied since.
        clickhouse
            .execute(
                &format!(
                    "TRUNCATE TABLE IF EXISTS {REQUESTS_TABLE} SETTINGS lock_acquire_timeout = {}",
                    TRUNCATE_LOCK_TIMEOUT.as_secs()
                ),
                &[],
            )
            .await?;
        #[derive(Deserialize)]
        struct Count {
            n: u64,
        }
        let left: Vec<Count> = clickhouse
            .query(
                &format!("SELECT count() AS n FROM {REQUESTS_TABLE}"),
                &[],
                Vec::new(),
            )
            .await?;
        if left.first().is_none_or(|count| count.n > 0) {
            // Written to meanwhile (another worker's queue round): try again.
            return Ok(());
        }
        if self.store.start_backfill(self.owner, instance).await? {
            tracing::info!(%instance, "ClickHouse emptied; backfill starting");
        }
        Ok(())
    }

    /// Whether ClickHouse is still the store that was copied to; if not
    /// (emptied, replaced or a different one), the sink starts again.
    /// Checked once a minute.
    async fn check_store(
        &self,
        clickhouse: &ClickHouse,
        expected: Option<Uuid>,
    ) -> Result<bool, SinkJobError> {
        if !due(&self.checked_at, STORE_CHECK_INTERVAL) {
            return Ok(true);
        }
        let found = clickhouse.store_id().await.inspect_err(|error| {
            metrics().inc(&ANALYTICS_SINK_ERRORS, &["check"]);
            self.note(error);
        })?;
        stamp(&self.checked_at);
        if found.is_some() && found == expected {
            return Ok(true);
        }
        if found.is_none() {
            // `gateway_meta` may be gone: make sure it is back first.
            self.schema_ready.store(false, Ordering::Relaxed);
        }
        if self.store.restart(expected).await? {
            tracing::warn!("the ClickHouse store is not the one copied to; copying history again");
        }
        Ok(false)
    }

    /// `backfilling`: under the lease, copies reservations in id order,
    /// page by page, recording the cursor after each page, so a restarted
    /// or replaced worker resumes there. The copy is complete (and reads
    /// may use ClickHouse) after a page shorter than [`BACKFILL_PAGE`].
    async fn backfill(&self, clickhouse: &ClickHouse) -> Result<TickReport, SinkJobError> {
        let mut report = TickReport::default();
        for _ in 0..MAX_ROUNDS {
            let now = Utc::now();
            let until =
                now + chrono::Duration::from_std(LEASE).unwrap_or(chrono::Duration::minutes(5));
            let Some(state) = self.store.claim_lease(self.owner, now, until).await? else {
                return Ok(report);
            };
            if state.phase != Phase::Backfilling {
                return Ok(report);
            }
            let ids = self
                .store
                .backfill_ids(state.backfill_cursor, BACKFILL_PAGE)
                .await?;
            let Some(last) = ids.last().copied() else {
                self.store.finish_empty_backfill(self.owner).await?;
                tracing::info!(rows = state.backfill_rows, "analytics backfill complete");
                return Ok(report);
            };
            let (rows, _) = self.store.read(&ids).await?;
            let count = rows.len() as u64;
            if let Err(error) = clickhouse
                .insert(REQUESTS_TABLE, rows.into_iter().map(|row| row.into_row()))
                .await
            {
                metrics().inc(&ANALYTICS_SINK_ERRORS, &["backfill"]);
                self.note(&error);
                self.store.record_error(&error.to_string()).await?;
                return Err(error.into());
            }
            metrics().add(&ANALYTICS_SINK_ROWS, &["backfilled"], count);
            report.backfilled += count;
            let done = (ids.len() as i64) < BACKFILL_PAGE;
            self.store
                .advance_backfill(self.owner, last, count as i64, done)
                .await?;
            if done {
                tracing::info!(
                    rows = state.backfill_rows + count as i64,
                    "analytics backfill complete"
                );
                return Ok(report);
            }
        }
        Ok(report)
    }

    /// Writes queued requests until a round claims less than a full batch,
    /// a write fails, or [`MAX_ROUNDS`].
    async fn drain(&self, clickhouse: &ClickHouse) -> Result<TickReport, SinkJobError> {
        let mut report = TickReport::default();
        for _ in 0..MAX_ROUNDS {
            let round = self.write_round(clickhouse).await?;
            report.add(round);
            if round.failed > 0 || round.claimed < WRITE_BATCH as u64 {
                break;
            }
        }
        Ok(report)
    }

    /// Claims, reads and writes one batch of queued requests.
    pub async fn write_round(&self, clickhouse: &ClickHouse) -> Result<TickReport, SinkJobError> {
        let now = Utc::now();
        let until = now + chrono::Duration::from_std(LEASE).unwrap_or(chrono::Duration::minutes(5));
        let claimed = self.store.claim(now, until, WRITE_BATCH).await?;
        if claimed.is_empty() {
            return Ok(TickReport::default());
        }
        let read = self.store.read_queued(&claimed).await?;
        let found: HashSet<Uuid> = read.rows.iter().map(|row| row.reservation_id).collect();
        // A request in the queue but not in PostgreSQL was deleted
        // (retention), which is final. One tombstone per request.
        let mut gone: Vec<&Claimed> = Vec::new();
        let mut seen = HashSet::new();
        for entry in &claimed {
            if !found.contains(&entry.reservation_id) && seen.insert(entry.reservation_id) {
                gone.push(entry);
            }
        }
        let version = read.version;
        let lines: Vec<(Uuid, bool, Row)> = read
            .rows
            .into_iter()
            .map(|row| (row.reservation_id, false, row.into_row()))
            .chain(
                gone.iter()
                    .map(|c| (c.reservation_id, true, tombstone(c, version))),
            )
            .collect();
        let (failed, error) = insert_isolating(clickhouse, &lines).await;
        let failed_requests: HashSet<Uuid> = failed.iter().map(|&i| lines[i].0).collect();
        let mut report = TickReport::default();
        for (index, (_, deleted, _)) in lines.iter().enumerate() {
            if failed.contains(&index) {
                continue;
            }
            if *deleted {
                report.deleted += 1;
            } else {
                report.written += 1;
            }
        }
        if let Some(error) = error {
            metrics().inc(&ANALYTICS_SINK_ERRORS, &["write"]);
            self.note(&error);
            tracing::warn!(
                %error,
                requests = failed_requests.len(),
                "analytics sink write failed"
            );
            // Every entry of a failed request backs off, the unclaimed ones
            // the read saw too (at the request's attempts), so a request
            // queued twice is not retried at once through its other entry.
            let mut attempts: std::collections::HashMap<Uuid, i32> =
                std::collections::HashMap::new();
            for entry in claimed
                .iter()
                .filter(|c| failed_requests.contains(&c.reservation_id))
            {
                let count = attempts.entry(entry.reservation_id).or_insert(0);
                *count = (*count).max(entry.attempts);
            }
            let mut retry: std::collections::BTreeMap<i64, i32> = claimed
                .iter()
                .filter(|c| failed_requests.contains(&c.reservation_id))
                .map(|c| (c.id, c.attempts))
                .collect();
            for (id, request) in &read.covered {
                if let Some(&count) = attempts.get(request) {
                    retry.entry(*id).or_insert(count);
                }
            }
            let retry: Vec<(i64, i32)> = retry.into_iter().collect();
            self.store
                .record_failure(&retry, &error.to_string(), |attempts| {
                    Utc::now()
                        + chrono::Duration::from_std(backoff(attempts))
                            .unwrap_or(chrono::Duration::hours(1))
                })
                .await?;
            report.failed = failed_requests.len() as u64;
        }
        // Every entry the read covered, for the requests that were written.
        let done: Vec<i64> = read
            .covered
            .iter()
            .filter(|(_, request)| !failed_requests.contains(request))
            .map(|(id, _)| *id)
            .chain(
                claimed
                    .iter()
                    .filter(|c| !failed_requests.contains(&c.reservation_id))
                    .map(|c| c.id),
            )
            .collect::<std::collections::BTreeSet<i64>>()
            .into_iter()
            .collect();
        if !done.is_empty() {
            self.store.acknowledge(&done).await?;
        }
        metrics().add(&ANALYTICS_SINK_ROWS, &["written"], report.written);
        metrics().add(&ANALYTICS_SINK_ROWS, &["deleted"], report.deleted);
        report.claimed = claimed.len() as u64;
        Ok(report)
    }
}

/// Inserts `lines` as one block. If ClickHouse refuses it (it answered,
/// so it is up), halves it until the refused rows are found, so a row it
/// keeps refusing holds up only itself, not the up to [`WRITE_BATCH`]
/// beside it: about two inserts per halving for one bad row. When both
/// halves are refused the fault is not one row (a missing table, a full
/// disk), so the rest is given up as one. Returns the indexes not written
/// and the last error.
async fn insert_isolating(
    clickhouse: &ClickHouse,
    lines: &[(Uuid, bool, Row)],
) -> (HashSet<usize>, Option<ClickHouseError>) {
    let insert = |range: std::ops::Range<usize>| {
        clickhouse.insert(REQUESTS_TABLE, lines[range].iter().map(|(_, _, row)| row))
    };
    let mut part = 0..lines.len();
    let mut error = match insert(part.clone()).await {
        Ok(()) => return (HashSet::new(), None),
        Err(error) => error,
    };
    loop {
        if part.len() <= 1 || !matches!(error, ClickHouseError::Server { .. }) {
            return (part.collect(), Some(error));
        }
        let middle = part.start + part.len() / 2;
        let (left, right) = (part.start..middle, middle..part.end);
        match (insert(left.clone()).await, insert(right.clone()).await) {
            // Refused as a whole, taken in halves: written now.
            (Ok(()), Ok(())) => return (HashSet::new(), None),
            (Err(e), Ok(())) => (part, error) = (left, e),
            (Ok(()), Err(e)) => (part, error) = (right, e),
            (Err(e), Err(_)) => return (part.collect(), Some(e)),
        }
    }
}

/// Whether `interval` has passed since `at` was stamped (or it never was).
fn due(at: &Mutex<Option<Instant>>, interval: Duration) -> bool {
    at.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_none_or(|at| at.elapsed() >= interval)
}

fn stamp(at: &Mutex<Option<Instant>>) {
    *at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
}

fn set_phase(phase: Option<Phase>) {
    for (name, value) in [
        ("off", None),
        ("starting", Some(Phase::Starting)),
        ("backfilling", Some(Phase::Backfilling)),
        ("live", Some(Phase::Live)),
    ] {
        metrics().set(&ANALYTICS_SINK_PHASE, &[name], i64::from(phase == value));
    }
}
