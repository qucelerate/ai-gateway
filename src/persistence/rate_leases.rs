//! Rate-limit leases (D-030, `GATEWAY_RATE_LIMIT_LEASES`, migration 0145).
//!
//! Without them every new reservation locks its scope's rate-limit policy
//! rows and counts itself on each policy's counter row, so one
//! organization's admissions pass one at a time through that row. With
//! leases on, a gateway process takes a *lease* of N admissions for a scope
//! (organization, team, service account) in one transaction, under the same
//! locks and checks as an admission, and then admits up to N of the scope's
//! requests in memory, writing nothing on the policies' rows.
//!
//! The limits stay exact, independent of this process's memory or timing:
//!
//! * Taking a lease counts all N on every policy covering the scope: N
//!   admissions in the current second, and N requests in flight. It is
//!   taken only if each limit has room for all N in the window admission
//!   reads during that second, read in the same second with the policies
//!   locked.
//! * A lease admits only until that second ends, and each reservation under
//!   it is stamped with the time it was admitted, so every request it admits
//!   falls in the second it was counted in, as if admitted on the policy's
//!   row then. Nothing is counted in a second ahead of the clock: the ring
//!   holds 61 seconds, so writing the next second would clear the oldest
//!   one while it is still in the window.
//! * Requests admitted under a lease leave flight as any other (on the
//!   policies' release rows). Returning the lease counts the reservations
//!   it admitted (the database's, not memory's) and gives back the rest of
//!   its N. A transaction that admits under a lease takes the lease's
//!   advisory lock shared and checks it is still usable (not returned, not
//!   past its end, the policies and their limits unchanged); a return takes
//!   it exclusively, so it sees every reservation made under the lease.
//! * The database caps a lease at N whatever this process's memory says:
//!   each admission carries one of the lease's tickets (0 to N-1), and a
//!   unique index allows one reservation per ticket. A reservation whose
//!   ticket the lease does not have, or whose admission time falls outside
//!   its own transaction, is counted on the policies as any other.
//! * Memory hands out each ticket once: it is taken just before the
//!   database checks the lease and given back only when the reservation's
//!   transaction did not commit.
//!
//! Leases cover requests per minute and concurrency of the organization, a
//! team and a service account, and nothing else (migration 0155):
//!
//! * A lease is never granted for a scope a `tokens_per_minute` policy
//!   covers, and is not used once one does: a request's tokens are known
//!   only at admission, so such a scope is admitted on the policies' rows.
//! * A request of an API key with a policy of its own is never admitted
//!   under a lease: the database says so when the lease is used, the ticket
//!   goes back to the lease for other keys' requests, and the request is
//!   admitted on the policies' rows, which lock and count every policy
//!   covering it, the key's among them. This process then leaves that key's
//!   requests out of its leases for a few seconds.
//!
//! So a lease can only refuse a request another process could have
//! admitted, while its own N are unused: for at most a second, after
//! which its holder returns it. A crashed process's leases count their N
//! admissions until those leave the minute's window, and their N in flight
//! until the worker returns them ([`EXPIRE_AFTER`] past their end).
//!
//! Size: the smaller of what this process admitted for the scope in the
//! last second (a lease's life is at most a second), and a share of each limit's room
//! ([`SHARE_PERCENT`], divided among the processes holding leases in the
//! organization). A limit with room for fewer than [`MIN_ROOM`] more, or a
//! lease that would cover fewer than [`MIN_LEASE`], is not leased: near its
//! limit a scope is admitted row by row, exactly as without leases.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::accounting::QuotaScope;

/// How often a process returns its ended leases.
pub const TICK: Duration = Duration::from_millis(250);
/// The worker returns a lease this long after its end, whoever holds it.
pub const EXPIRE_AFTER: Duration = Duration::from_secs(10);
/// How many leases a process returns at once.
const RETURNS_AT_ONCE: usize = 8;
/// Returned leases are kept this long, then deleted.
const KEEP_RETURNED: Duration = Duration::from_secs(3600);
/// A lease must cover at least this many admissions to be worth a commit.
const MIN_LEASE: i64 = 10;
/// A limit is leased only while it has room for this many more.
const MIN_ROOM: i64 = 100;
/// The share of a limit's room one round of leases takes.
const SHARE_PERCENT: i64 = 10;
/// A lease is taken only with this much of the current second left; nearer
/// its end, requests are admitted on the policies' rows until the next one.
const MIN_LIFE: Duration = Duration::from_millis(200);
/// A lease admits only while it ends more than this from now: the
/// reservation's transaction checks it in the database a little later.
const MARGIN: Duration = Duration::from_millis(50);
/// After a scope could not be leased, it is not tried again for this long.
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// After a scope was found to have no policy, it is not tried for this long.
const NO_POLICY_RETRY: Duration = Duration::from_secs(5);
/// After a key was found to have a policy of its own, its requests skip
/// leases for this long.
const KEYED_RETRY: Duration = Duration::from_secs(5);
/// A scope with nothing leased and no request for this long is forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(60);

/// Why a lease was returned (`rate_limit_lease.return_reason`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReturnReason {
    /// Past its end, by its holder.
    Ended,
    Shutdown,
    /// Past its end by [`EXPIRE_AFTER`], by the worker.
    Expired,
}

impl ReturnReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Shutdown => "shutdown",
            Self::Expired => "expired",
        }
    }
}

type Key = (Uuid, Option<Uuid>, Option<Uuid>);

fn key(scope: &QuotaScope) -> Key {
    (
        scope.organization_id.0,
        scope.team_id.map(|team| team.0),
        scope.service_account_id.map(|account| account.0),
    )
}

#[derive(Debug)]
struct Lease {
    id: Uuid,
    admissions: i64,
    /// Its tickets, 0 to N-1, one per admission: the next never handed out,
    /// and those given back by reservations never made. The database holds
    /// one reservation per ticket, so a lease admits at most N there too.
    next: i64,
    free: Vec<i64>,
    /// When it stops admitting, on this process's clock (never later than
    /// its end in the database).
    deadline: Instant,
}

#[derive(Debug, Default)]
struct Scope {
    current: Option<Lease>,
    /// Leases that take nothing new, and when to return them.
    ended: Vec<(Uuid, Instant)>,
    taking: bool,
    retry_at: Option<Instant>,
    /// Admissions this second and the second before.
    second: u64,
    this_second: i64,
    last_second: i64,
    touched: Option<Instant>,
}

impl Lease {
    fn new(id: Uuid, admissions: i64, deadline: Instant) -> Self {
        Self {
            id,
            admissions,
            next: 0,
            free: Vec::new(),
            deadline,
        }
    }

    /// What this process may still admit under it.
    fn left(&self) -> i64 {
        (self.admissions - self.next).saturating_add(self.free.len() as i64)
    }
}

impl Scope {
    /// Takes nothing new under the current lease; it is returned at `at`,
    /// or at its deadline if that comes first.
    fn end_current(&mut self, at: Option<Instant>) {
        if let Some(lease) = self.current.take() {
            let at = at.map_or(lease.deadline, |at| at.min(lease.deadline));
            self.ended.push((lease.id, at));
        }
    }

    fn rate(&self, second: u64) -> i64 {
        match second.saturating_sub(self.second) {
            0 => self.this_second.max(self.last_second),
            1 => self.this_second,
            _ => 0,
        }
    }
}

/// Clears a scope's `taking` if taking its lease is dropped before it
/// finishes (the request cancelled), and waits [`RETRY_AFTER`].
struct Taking<'a> {
    leases: &'a RateLimitLeases,
    key: Key,
}

impl Drop for Taking<'_> {
    fn drop(&mut self) {
        if let Some(scope) = self.leases.scopes().get_mut(&self.key) {
            scope.taking = false;
            scope.retry_at = Some(Instant::now() + RETRY_AFTER);
        }
    }
}

/// This process's rate-limit leases.
pub struct RateLimitLeases {
    holder: Uuid,
    pool: PgPool,
    started: Instant,
    scopes: Mutex<HashMap<Key, Scope>>,
    /// API keys with a policy of their own, and until when their requests
    /// skip leases.
    keyed: Mutex<HashMap<Uuid, Instant>>,
}

/// What admitting a request under a lease found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseUse {
    /// Admitted at this time.
    Admitted(DateTime<Utc>),
    /// The request's key has a policy of its own: the lease is still usable
    /// for other requests.
    Keyed,
    /// The lease is no longer usable.
    Unusable,
}

impl std::fmt::Debug for RateLimitLeases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimitLeases")
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

/// The outcome of taking a lease.
#[derive(Debug, PartialEq)]
enum Taken {
    Lease {
        id: Uuid,
        admissions: i64,
        /// When it stops admitting, on this process's clock.
        deadline: Instant,
    },
    /// No policy covers the scope.
    NoPolicy,
    /// A limit is too near, or the lease would be too small.
    NoRoom,
    /// Too little of the current second is left (or the second changed while
    /// the room was read): try again after this.
    Later(Duration),
}

#[derive(FromRow)]
struct CounterRow {
    requests_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
    in_flight: i64,
    last_second: i64,
    slots: Vec<i32>,
    now_epoch: f64,
    holders: i64,
}

impl RateLimitLeases {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self {
            holder: Uuid::new_v4(),
            pool,
            started: Instant::now(),
            scopes: Mutex::new(HashMap::new()),
            keyed: Mutex::new(HashMap::new()),
        })
    }

    pub fn holder(&self) -> Uuid {
        self.holder
    }

    fn scopes(&self) -> MutexGuard<'_, HashMap<Key, Scope>> {
        self.scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn keys(&self) -> MutexGuard<'_, HashMap<Uuid, Instant>> {
        self.keyed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether `scope`'s key was lately found to have a policy of its own.
    fn skips(&self, scope: &QuotaScope) -> bool {
        scope.api_key_id.is_some_and(|key| {
            self.keys()
                .get(&key.0)
                .is_some_and(|until| Instant::now() < *until)
        })
    }

    /// The database found a policy of `scope`'s key: its requests skip
    /// leases for [`KEYED_RETRY`].
    pub(crate) fn keyed(&self, scope: &QuotaScope) {
        if let Some(key) = scope.api_key_id {
            self.keys().insert(key.0, Instant::now() + KEYED_RETRY);
        }
    }

    /// Counts one admission wanted for `scope` and takes a new lease when
    /// the current one is missing, nearly used or nearly ended, and one is
    /// worth it. Never fails a request: an error only means no lease.
    pub(crate) async fn prepare(&self, scope: &QuotaScope) {
        // Requests of a key with a policy of its own are never leased, and
        // are not counted towards the scope's leases either.
        if self.skips(scope) {
            return;
        }
        let key = key(scope);
        let now = Instant::now();
        let second = self.started.elapsed().as_secs();
        let want = {
            let mut scopes = self.scopes();
            let state = scopes.entry(key).or_default();
            state.touched = Some(now);
            match second.saturating_sub(state.second) {
                0 => {}
                1 => {
                    state.last_second = state.this_second;
                    state.this_second = 0;
                }
                _ => {
                    state.last_second = 0;
                    state.this_second = 0;
                }
            }
            state.second = second;
            state.this_second = state.this_second.saturating_add(1);
            let rate = state.rate(second);
            // Renewed before it runs out (under half a second's admissions
            // left), unless its second is nearly over: a lease admits only
            // within the second it is taken in, so the next one waits for
            // the next second.
            let lasting = state.current.as_ref().is_some_and(|lease| {
                now + MARGIN < lease.deadline
                    && (lease.left().saturating_mul(2) >= rate || now + MIN_LIFE >= lease.deadline)
            });
            if lasting || state.taking || state.retry_at.is_some_and(|retry| now < retry) {
                return;
            }
            let want = rate;
            if want < MIN_LEASE {
                return;
            }
            state.taking = true;
            want
        };
        // The request may be cancelled while the lease is taken: the guard
        // then clears `taking`. A lease that committed unseen is returned by
        // the worker once it has ended.
        let taking = Taking { leases: self, key };
        let taken = take(&self.pool, self.holder, key, want).await;
        std::mem::forget(taking);
        let mut scopes = self.scopes();
        let state = scopes.entry(key).or_default();
        state.taking = false;
        match taken {
            Ok(Taken::Lease {
                id,
                admissions,
                deadline,
            }) => {
                let metrics = crate::metrics::global();
                metrics.inc(&crate::metrics::RATE_LIMIT_LEASES, &["taken", ""]);
                metrics.add(
                    &crate::metrics::RATE_LIMIT_LEASE_ADMISSIONS,
                    &["leased"],
                    u64::try_from(admissions).unwrap_or(0),
                );
                // The lease it replaces still admits what was taken from it
                // until its own deadline, and is returned then.
                state.end_current(None);
                state.current = Some(Lease::new(id, admissions, deadline));
                state.retry_at = None;
            }
            Ok(Taken::NoPolicy) => state.retry_at = Some(Instant::now() + NO_POLICY_RETRY),
            Ok(Taken::NoRoom) => state.retry_at = Some(Instant::now() + RETRY_AFTER),
            Ok(Taken::Later(after)) => state.retry_at = Some(Instant::now() + after),
            Err(error) => {
                tracing::warn!(%error, organization = %key.0, "rate-limit lease not taken");
                state.retry_at = Some(Instant::now() + RETRY_AFTER);
            }
        }
    }

    /// One admission from the scope's current lease, if it has one left and
    /// is not about to end; the lease's id.
    pub(crate) fn hold(&self, scope: &QuotaScope) -> Option<(Uuid, i32)> {
        if self.skips(scope) {
            return None;
        }
        let mut scopes = self.scopes();
        let state = scopes.get_mut(&key(scope))?;
        let now = Instant::now();
        let lease = state.current.as_mut()?;
        if now + MARGIN >= lease.deadline {
            state.end_current(Some(now));
            return None;
        }
        let ticket = match lease.free.pop() {
            Some(ticket) => ticket,
            None if lease.next < lease.admissions => {
                lease.next += 1;
                lease.next - 1
            }
            None => return None,
        };
        Some((lease.id, i32::try_from(ticket).ok()?))
    }

    /// Gives back an admission whose reservation was never made.
    pub(crate) fn give_back(&self, scope: &QuotaScope, lease: Uuid, ticket: i32) {
        let mut scopes = self.scopes();
        if let Some(current) = scopes
            .get_mut(&key(scope))
            .and_then(|state| state.current.as_mut())
            .filter(|current| current.id == lease)
        {
            let ticket = i64::from(ticket);
            if (0..current.next).contains(&ticket) && !current.free.contains(&ticket) {
                current.free.push(ticket);
            }
        }
    }

    /// Stops admitting under a lease the database says is no longer usable;
    /// it is returned on the next tick.
    pub(crate) fn retire(&self, scope: &QuotaScope, lease: Uuid) {
        let mut scopes = self.scopes();
        if let Some(state) = scopes.get_mut(&key(scope))
            && state
                .current
                .as_ref()
                .is_some_and(|current| current.id == lease)
        {
            state.end_current(Some(Instant::now()));
        }
    }

    /// Returns the leases that have ended. Run every [`TICK`].
    pub async fn tick(&self) {
        let now = Instant::now();
        let mut due = Vec::new();
        {
            let mut scopes = self.scopes();
            for state in scopes.values_mut() {
                if state
                    .current
                    .as_ref()
                    .is_some_and(|lease| lease.deadline <= now)
                {
                    state.end_current(Some(now));
                }
                state.ended.retain(|&(lease, at)| {
                    if at <= now {
                        due.push(lease);
                        false
                    } else {
                        true
                    }
                });
            }
            scopes.retain(|_, state| {
                state.current.is_some()
                    || !state.ended.is_empty()
                    || state.taking
                    || state.touched.is_some_and(|at| at.elapsed() < FORGET_AFTER)
            });
        }
        self.keys().retain(|_, until| now < *until);
        self.return_each(due, ReturnReason::Ended).await;
    }

    /// Returns every lease this process holds (on shutdown).
    pub async fn return_all(&self) {
        let leases: Vec<Uuid> = {
            let mut scopes = self.scopes();
            let now = Instant::now();
            scopes
                .values_mut()
                .flat_map(|state| {
                    state.end_current(Some(now));
                    std::mem::take(&mut state.ended)
                })
                .map(|(lease, _)| lease)
                .collect()
        };
        self.return_each(leases, ReturnReason::Shutdown).await;
    }

    /// Returns `leases` a few at a time, so one return waiting on a lease's
    /// lock (for a reservation's transaction still admitting under it) does
    /// not hold up the others.
    async fn return_each(&self, leases: Vec<Uuid>, reason: ReturnReason) {
        use futures::StreamExt;
        futures::stream::iter(leases)
            .for_each_concurrent(RETURNS_AT_ONCE, |lease| self.return_one(lease, reason))
            .await;
    }

    async fn return_one(&self, lease: Uuid, reason: ReturnReason) {
        match return_lease(&self.pool, lease, reason).await {
            Ok(Some(admitted)) => record_return(reason, admitted),
            Ok(None) => {}
            // The worker returns it once it has expired.
            Err(error) => tracing::warn!(%error, %lease, "rate-limit lease not returned"),
        }
    }
}

fn record_return(reason: ReturnReason, admitted: i64) {
    let metrics = crate::metrics::global();
    metrics.inc(
        &crate::metrics::RATE_LIMIT_LEASES,
        &["returned", reason.as_str()],
    );
    metrics.add(
        &crate::metrics::RATE_LIMIT_LEASE_ADMISSIONS,
        &["used"],
        u64::try_from(admitted).unwrap_or(0),
    );
}

/// Takes a lease for `holder` on the scope: at most `want` admissions, sized
/// against each covering policy's room (with the policies locked, as an
/// admission locks them), counted on their counters in the same
/// transaction.
async fn take(pool: &PgPool, holder: Uuid, key: Key, want: i64) -> Result<Taken, sqlx::Error> {
    let (organization, team, account) = key;
    let mut tx = pool.begin().await?;
    let counters = sqlx::query_as::<_, CounterRow>(
        "SELECT requests_per_minute, max_concurrent, in_flight, last_second, slots, now_epoch,
                holders
         FROM gateway.lock_rate_limit_lease_counters($1, $2, $3, $4)",
    )
    .bind(organization)
    .bind(team)
    .bind(account)
    .bind(holder)
    .fetch_all(&mut *tx)
    .await?;
    if counters.is_empty() {
        return Ok(Taken::NoPolicy);
    }
    // Every room is read for the window admission reads in one second; the
    // lease is counted in that second or not at all.
    let room_second = counters[0].now_epoch.floor() as i64;
    if counters
        .iter()
        .any(|counter| counter.now_epoch.floor() as i64 != room_second)
    {
        return Ok(Taken::Later(Duration::ZERO));
    }
    let rooms: Vec<(i64, i64)> = counters
        .iter()
        .flat_map(|counter| {
            let rate = counter.requests_per_minute.map(|limit| {
                i64::from(limit).saturating_sub(super::rate_limits::window_count(
                    &counter.slots,
                    counter.last_second,
                    counter.now_epoch,
                ))
            });
            let flight = counter
                .max_concurrent
                .map(|limit| i64::from(limit).saturating_sub(counter.in_flight));
            [rate, flight]
                .into_iter()
                .flatten()
                .map(|room| (room, counter.holders))
        })
        .collect();
    let Some(admissions) = lease_size(&rooms, want) else {
        return Ok(Taken::NoRoom);
    };
    let id = Uuid::new_v4();
    // The grant reads the database's clock after this, so its time left,
    // counted from here, never ends later than the lease does. (Counted
    // from before the locks above, a wait for them would come off the
    // lease's life.)
    let asked = Instant::now();
    let granted: Option<(bool, f64)> = sqlx::query_as(
        "SELECT granted, seconds_left
         FROM gateway.grant_rate_limit_lease($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(holder)
    .bind(organization)
    .bind(team)
    .bind(account)
    .bind(i32::try_from(admissions).unwrap_or(i32::MAX))
    .bind(room_second)
    .bind(MIN_LIFE.as_secs_f64())
    .fetch_optional(&mut *tx)
    .await?;
    let left = match granted {
        None => return Ok(Taken::NoPolicy),
        Some((false, left)) => {
            return Ok(Taken::Later(
                Duration::try_from_secs_f64(left).unwrap_or(Duration::ZERO),
            ));
        }
        Some((true, left)) => left,
    };
    tx.commit().await?;
    Ok(Taken::Lease {
        id,
        admissions: admissions.min(i64::from(i32::MAX)),
        deadline: asked + Duration::try_from_secs_f64(left).unwrap_or(Duration::ZERO),
    })
}

/// How many admissions one lease takes: `want`, but no more than an even
/// share (among the other holders and this one) of [`SHARE_PERCENT`] of each
/// limit's room; `None` when a limit is too near or the lease too small.
/// `rooms` is each limit's room with the number of other holders.
pub(crate) fn lease_size(rooms: &[(i64, i64)], want: i64) -> Option<i64> {
    let mut size = want;
    for &(room, others) in rooms {
        if room < MIN_ROOM {
            return None;
        }
        let share = room / 100 * SHARE_PERCENT / others.saturating_add(1).max(1);
        size = size.min(share);
    }
    (size >= MIN_LEASE).then_some(size)
}

/// Admits a request under `lease` in the reservation's transaction: takes
/// the organization's row (key share) and the lease's advisory lock shared,
/// until the transaction ends, and returns the time of admission if the
/// lease is still usable for `scope` and `scope`'s key has no policy of its
/// own.
pub(crate) async fn use_lease(
    conn: &mut PgConnection,
    lease: Uuid,
    ticket: i32,
    scope: &QuotaScope,
) -> Result<LeaseUse, sqlx::Error> {
    let (admitted, keyed): (Option<DateTime<Utc>>, bool) = sqlx::query_as(
        "SELECT admitted_at, keyed FROM gateway.use_rate_limit_lease($1, $2, $3, $4, $5, $6)",
    )
    .bind(lease)
    .bind(ticket)
    .bind(scope.organization_id.0)
    .bind(scope.team_id.map(|team| team.0))
    .bind(scope.service_account_id.map(|account| account.0))
    .bind(scope.api_key_id.map(|key| key.0))
    .fetch_one(&mut *conn)
    .await?;
    Ok(match (admitted, keyed) {
        (_, true) => LeaseUse::Keyed,
        (Some(at), false) => LeaseUse::Admitted(at),
        (None, false) => LeaseUse::Unusable,
    })
}

/// Returns `lease`: what it did not admit is given back on its policies'
/// counters. The number it admitted, or `None` if it was already returned.
pub async fn return_lease(
    pool: &PgPool,
    lease: Uuid,
    reason: ReturnReason,
) -> Result<Option<i64>, sqlx::Error> {
    let admitted: Option<i32> =
        sqlx::query_scalar("SELECT gateway.return_rate_limit_lease($1, $2)")
            .bind(lease)
            .bind(reason.as_str())
            .fetch_one(pool)
            .await?;
    Ok(admitted.map(i64::from))
}

/// Returns up to `max` leases `after` past their end ([`EXPIRE_AFTER`] in
/// the worker), whoever holds them, so a crashed process's leases come back,
/// and deletes leases returned more than an hour ago. Each return is its own
/// transaction.
pub async fn return_expired(pool: &PgPool, max: i64, after: Duration) -> Result<u64, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gateway.rate_limit_lease
         WHERE returned_at IS NULL AND valid_until <= clock_timestamp() - make_interval(secs => $1)
         ORDER BY valid_until LIMIT $2",
    )
    .bind(after.as_secs_f64())
    .bind(max.clamp(1, 1_000))
    .fetch_all(pool)
    .await?;
    let mut count = 0;
    for lease in due {
        if let Some(admitted) = return_lease(pool, lease, ReturnReason::Expired).await? {
            record_return(ReturnReason::Expired, admitted);
            count += 1;
        }
    }
    sqlx::query(
        "DELETE FROM gateway.rate_limit_lease WHERE id IN (
             SELECT id FROM gateway.rate_limit_lease
             WHERE returned_at < clock_timestamp() - make_interval(secs => $1)
             LIMIT 10000)",
    )
    .bind(KEEP_RETURNED.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::identity::{OrganizationId, ServiceAccountId};

    #[test]
    fn a_lease_is_the_smaller_of_its_share_and_the_recent_rate() {
        // 10% of 50,000 is 5,000; the recent rate asks for 500.
        assert_eq!(lease_size(&[(50_000, 0)], 500), Some(500));
        // Shared with three other holders, and the tightest limit wins.
        assert_eq!(lease_size(&[(50_000, 3), (8_000, 3)], 5_000), Some(200));
        // A limit with room for fewer than 100 more is never leased.
        assert_eq!(lease_size(&[(50_000, 0), (99, 0)], 500), None);
        // Too small to be worth a commit.
        assert_eq!(lease_size(&[(50_000, 0)], 9), None);
        assert_eq!(lease_size(&[(100, 0)], 500), Some(10));
        assert_eq!(lease_size(&[(100, 1)], 500), None);
        // No limit with a room (a policy limiting nothing): the rate decides.
        assert_eq!(lease_size(&[], 40), Some(40));
    }

    fn leases() -> RateLimitLeases {
        RateLimitLeases {
            holder: Uuid::new_v4(),
            pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            started: Instant::now(),
            scopes: Mutex::new(HashMap::new()),
            keyed: Mutex::new(HashMap::new()),
        }
    }

    fn scope() -> QuotaScope {
        QuotaScope {
            organization_id: OrganizationId::new(),
            team_id: None,
            service_account_id: Some(ServiceAccountId(Uuid::new_v4())),
            api_key_id: None,
        }
    }

    fn install(
        leases: &RateLimitLeases,
        scope: &QuotaScope,
        admissions: i64,
        life: Duration,
    ) -> Uuid {
        let id = Uuid::new_v4();
        leases.scopes().entry(key(scope)).or_default().current =
            Some(Lease::new(id, admissions, Instant::now() + life));
        id
    }

    #[tokio::test]
    async fn memory_never_admits_more_than_the_lease() {
        let leases = leases();
        let scope = scope();
        let id = install(&leases, &scope, 2, Duration::from_secs(5));
        assert_eq!(leases.hold(&scope), Some((id, 0)));
        assert_eq!(leases.hold(&scope), Some((id, 1)));
        assert_eq!(leases.hold(&scope), None, "used up");
        // A reservation that was never made gives its ticket back, but never
        // one the lease does not have, nor the same one twice.
        leases.give_back(&scope, id, 1);
        leases.give_back(&scope, id, 1);
        leases.give_back(&scope, id, 2);
        leases.give_back(&scope, id, -1);
        assert_eq!(leases.hold(&scope), Some((id, 1)));
        assert_eq!(leases.hold(&scope), None);
        // Another scope has its own leases.
        assert_eq!(leases.hold(&self::scope()), None);
    }

    #[tokio::test]
    async fn a_key_with_a_policy_of_its_own_skips_leases_for_a_while() {
        let leases = leases();
        let keyed = QuotaScope {
            api_key_id: Some(crate::domain::identity::ApiKeyId(Uuid::new_v4())),
            ..scope()
        };
        let other = QuotaScope {
            api_key_id: Some(crate::domain::identity::ApiKeyId(Uuid::new_v4())),
            ..keyed.clone()
        };
        let id = install(&leases, &keyed, 10, Duration::from_secs(5));
        assert_eq!(leases.hold(&keyed), Some((id, 0)));
        // The database said the key has a policy: the ticket goes back and
        // the key's requests skip the lease, which the other key still uses.
        leases.give_back(&keyed, id, 0);
        leases.keyed(&keyed);
        assert_eq!(leases.hold(&keyed), None);
        assert_eq!(leases.hold(&other), Some((id, 0)));
        // Nor do they count towards the scope's next lease.
        leases.prepare(&keyed).await;
        assert_eq!(leases.scopes()[&key(&keyed)].this_second, 0);
        // Once the while is over, they try again.
        leases
            .keys()
            .insert(keyed.api_key_id.unwrap().0, Instant::now());
        assert_eq!(leases.hold(&keyed), Some((id, 1)));
        leases.tick().await;
        assert!(leases.keys().is_empty());
    }

    #[tokio::test]
    async fn a_lease_admits_nothing_near_its_end_or_once_retired() {
        let leases = leases();
        let scope = scope();
        let id = install(&leases, &scope, 100, MARGIN / 2);
        assert_eq!(leases.hold(&scope), None, "too near its end");
        {
            let scopes = leases.scopes();
            let state = &scopes[&key(&scope)];
            assert!(state.current.is_none());
            assert_eq!(state.ended.len(), 1);
            assert_eq!(state.ended[0].0, id);
        }
        let id = install(&leases, &scope, 100, Duration::from_secs(5));
        assert_eq!(leases.hold(&scope), Some((id, 0)));
        leases.retire(&scope, id);
        assert_eq!(leases.hold(&scope), None);
        // Giving back to a retired lease changes nothing.
        leases.give_back(&scope, id, 0);
        assert_eq!(leases.hold(&scope), None);
        let scopes = leases.scopes();
        assert_eq!(scopes[&key(&scope)].ended.len(), 2);
    }
}
