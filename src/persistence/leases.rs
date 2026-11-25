//! Budget leases (D-030, `GATEWAY_BUDGET_LEASES`, migration 0122).
//!
//! Every reservation used to lock each budget's bucket row and every
//! settlement to update it, so one budget's requests ran one at a time
//! through that row. With leases on, a gateway process takes a *lease* on a
//! busy budget: one transaction moves part of the bucket's remaining
//! allowance into the lease (reserved on the bucket, and counted in
//! `leased_minor`), and the process then reserves requests against the lease
//! in memory. Each request still writes its reservation and allocations, and
//! is still finalized exactly once; an allocation made under a lease names it
//! and moves nothing on the bucket while the lease is active. Returning the
//! lease moves what its allocations still hold into `reserved_minor`, what
//! they were charged into `consumed_minor`, and gives back the rest.
//!
//! Money correctness does not depend on this process's memory or timing:
//!
//! * A lease's amount is reserved on its bucket, so consumed plus reserved
//!   never passes the limit; the most a lease can do is refuse a request
//!   another process could have admitted.
//! * Memory never counts more room than the lease has: a hold is taken
//!   before the reservation's transaction, given back only when that
//!   transaction did not commit, and a finalization's change comes back only
//!   once it has committed. Anything unknown (a lost commit reply, a
//!   finalization by another process) stays used until the lease returns,
//!   when the database's allocations, not memory, decide the amounts.
//! * Every transaction that writes or finalizes an allocation under a lease
//!   takes the lease's advisory lock shared and checks the lease is still
//!   active; a return takes it exclusively. So a return sees every allocation
//!   made under the lease, nothing moves while it folds them, and any
//!   process (the holder, the worker, an operator) may return a lease at any
//!   time. Advisory locks are taken before any bucket row on every path, so
//!   the two cannot deadlock.
//!
//! Lease size is the smaller of a share of what remains (2%, divided among
//! the processes holding leases on the budget) and what this process
//! reserved on the budget in the last second times the lease interval (5
//! seconds). A budget with room for fewer than 100 of the request's
//! reservations, or a lease that would cover fewer than 10, is not leased:
//! near its limit a budget is enforced row by row, exactly as without
//! leases. A lease takes new requests for 20 seconds, is returned once idle
//! 5 seconds or drained of its requests, and the worker returns any lease 60
//! seconds old (its holder may have crashed).
//!
//! End users' allowances and credits are never leased: admitting an end
//! user's request already locks that user's row.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// The advisory lock class of leases; the two-key form keeps lease locks
/// apart from the single-key ones taken elsewhere.
const LOCK_CLASS: i32 = 0x0010_ea5e;

/// Past this the worker returns a lease, whether or not its holder lives.
pub const LEASE_TTL: Duration = Duration::from_secs(60);
/// A lease takes new requests for this long, then drains.
const ROTATE_AFTER: Duration = Duration::from_secs(20);
/// A lease nothing was reserved against for this long is returned.
const IDLE_AFTER: Duration = Duration::from_secs(5);
/// A lease holds at most what this process reserved on the budget in the
/// last second times this.
const INTERVAL_SECONDS: i64 = 5;
/// A holder returns a draining lease this long before the worker would.
const RETURN_MARGIN: Duration = Duration::from_secs(10);
/// A draining lease is returned once its requests are finalized, or after
/// this long: a request whose outcome this process never learns (one the
/// worker or an operator finalizes) must not keep it out for its whole life.
/// Returning early is always safe; what it still holds moves to the bucket.
const DRAIN_GRACE: Duration = Duration::from_secs(30);
/// A budget is leased only while it has room for this many reservations.
const MIN_REQUESTS_LEFT: i64 = 100;
/// A lease must cover at least this many reservations to be worth a commit.
const MIN_LEASE_REQUESTS: i64 = 10;
/// The share of a budget's remaining allowance one round of leases takes.
const SHARE_PERCENT: i64 = 2;
/// After a budget could not be leased, it is not tried again for this long.
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// Which budgets cover a scope, remembered this long.
const SCOPE_TTL: Duration = Duration::from_secs(5);

/// Why a lease was returned (`budget_lease.return_reason`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReturnReason {
    Idle,
    Drained,
    Shutdown,
    Expired,
    Limit,
    Operator,
}

impl ReturnReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Drained => "drained",
            Self::Shutdown => "shutdown",
            Self::Expired => "expired",
            Self::Limit => "limit",
            Self::Operator => "operator",
        }
    }
}

/// What a returned lease's allocations held and were charged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Returned {
    pub amount: i64,
    pub held: i64,
    pub settled: i64,
}

#[derive(Debug)]
struct Lease {
    id: Uuid,
    amount: i64,
    /// What this process may still reserve against it.
    available: i64,
    /// Reservations made under it whose finalization this process awaits.
    in_flight: u32,
    taken: Instant,
    used: Instant,
    /// When it stopped taking new requests.
    draining: Option<Instant>,
}

#[derive(Debug, Default)]
struct Bucket {
    organization_id: Uuid,
    current: Option<Lease>,
    draining: Vec<Lease>,
    taking: bool,
    retry_at: Option<Instant>,
    /// Amounts reserved this second and the second before.
    second: u64,
    this_second: i64,
    last_second: i64,
    touched: Option<Instant>,
}

impl Bucket {
    fn lease_mut(&mut self, id: Uuid) -> Option<&mut Lease> {
        self.current
            .as_mut()
            .filter(|lease| lease.id == id)
            .or_else(|| self.draining.iter_mut().find(|lease| lease.id == id))
    }

    fn drain_current(&mut self) {
        if let Some(mut lease) = self.current.take() {
            lease.draining = Some(Instant::now());
            self.draining.push(lease);
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

/// Clears a bucket's `taking` if the lease's acquisition is dropped before
/// it finishes, and waits [`RETRY_AFTER`] before the next try.
struct Taking<'a> {
    leases: &'a BudgetLeases,
    bucket_id: Uuid,
}

impl Drop for Taking<'_> {
    fn drop(&mut self) {
        if let Some(bucket) = self.leases.buckets().get_mut(&self.bucket_id) {
            bucket.taking = false;
            bucket.retry_at = Some(Instant::now() + RETRY_AFTER);
        }
    }
}

/// This process's budget leases.
pub struct BudgetLeases {
    holder: Uuid,
    pool: PgPool,
    started: Instant,
    buckets: Mutex<HashMap<Uuid, Bucket>>,
    scopes: Mutex<HashMap<String, (Instant, Vec<(Uuid, Uuid)>)>>,
}

impl std::fmt::Debug for BudgetLeases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetLeases")
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

impl BudgetLeases {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self {
            holder: Uuid::new_v4(),
            pool,
            started: Instant::now(),
            buckets: Mutex::new(HashMap::new()),
            scopes: Mutex::new(HashMap::new()),
        })
    }

    pub fn holder(&self) -> Uuid {
        self.holder
    }

    fn buckets(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Bucket>> {
        self.buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn cached_scope(&self, key: &str) -> Option<Vec<(Uuid, Uuid)>> {
        let scopes = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scopes
            .get(key)
            .filter(|(at, _)| at.elapsed() < SCOPE_TTL)
            .map(|(_, buckets)| buckets.clone())
    }

    pub(crate) fn cache_scope(&self, key: String, buckets: Vec<(Uuid, Uuid)>) {
        let mut scopes = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if scopes.len() > 10_000 {
            scopes.retain(|_, (at, _)| at.elapsed() < SCOPE_TTL);
        }
        scopes.insert(key, (Instant::now(), buckets));
    }

    /// Counts `amount` reserved on each of `buckets` (organization, bucket)
    /// and takes a lease on each that has none able to hold `amount`, where
    /// one is worth it. Never fails a request: an error only means no lease.
    pub(crate) async fn prepare(&self, buckets: &[(Uuid, Uuid)], amount: i64) {
        if amount <= 0 {
            return;
        }
        let now = Instant::now();
        let second = self.started.elapsed().as_secs();
        for &(organization_id, bucket_id) in buckets {
            let want = {
                let mut all = self.buckets();
                let bucket = all.entry(bucket_id).or_default();
                bucket.organization_id = organization_id;
                bucket.touched = Some(now);
                match second.saturating_sub(bucket.second) {
                    0 => {}
                    1 => {
                        bucket.last_second = bucket.this_second;
                        bucket.this_second = 0;
                    }
                    _ => {
                        bucket.last_second = 0;
                        bucket.this_second = 0;
                    }
                }
                bucket.second = second;
                bucket.this_second = bucket.this_second.saturating_add(amount);
                let usable = bucket.current.as_ref().is_some_and(|lease| {
                    lease.available >= amount && lease.taken.elapsed() < ROTATE_AFTER
                });
                if usable || bucket.taking || bucket.retry_at.is_some_and(|retry| now < retry) {
                    continue;
                }
                let want = bucket.rate(second).saturating_mul(INTERVAL_SECONDS);
                if want < amount.saturating_mul(MIN_LEASE_REQUESTS) {
                    continue;
                }
                bucket.taking = true;
                want
            };
            // The request may be cancelled while the lease is taken (its
            // client gone, its task aborted): the guard then clears `taking`
            // so later requests try again. A lease that committed unseen is
            // returned by the worker once it expires.
            let taking = Taking {
                leases: self,
                bucket_id,
            };
            let taken = take(
                &self.pool,
                self.holder,
                organization_id,
                bucket_id,
                amount,
                want,
            )
            .await;
            std::mem::forget(taking);
            let mut all = self.buckets();
            let bucket = all.entry(bucket_id).or_default();
            bucket.taking = false;
            match taken {
                Ok(Some((id, size))) => {
                    crate::metrics::global().inc(&crate::metrics::BUDGET_LEASES, &["taken", ""]);
                    crate::metrics::global().add(
                        &crate::metrics::BUDGET_LEASE_MINOR,
                        &["leased"],
                        u64::try_from(size).unwrap_or(0),
                    );
                    bucket.drain_current();
                    bucket.current = Some(Lease {
                        id,
                        amount: size,
                        available: size,
                        in_flight: 0,
                        taken: Instant::now(),
                        used: Instant::now(),
                        draining: None,
                    });
                    bucket.retry_at = None;
                }
                Ok(None) => bucket.retry_at = Some(Instant::now() + RETRY_AFTER),
                Err(error) => {
                    tracing::warn!(%error, bucket = %bucket_id, "budget lease not taken");
                    bucket.retry_at = Some(Instant::now() + RETRY_AFTER);
                }
            }
        }
    }

    /// Holds `amount` of the bucket's current lease for one reservation, if
    /// it has that much; the lease's id.
    pub(crate) fn hold(&self, bucket_id: Uuid, amount: i64) -> Option<Uuid> {
        let mut all = self.buckets();
        let bucket = all.get_mut(&bucket_id)?;
        let lease = bucket.current.as_mut()?;
        if lease.available < amount || lease.taken.elapsed() >= ROTATE_AFTER {
            bucket.drain_current();
            return None;
        }
        lease.available -= amount;
        lease.in_flight += 1;
        lease.used = Instant::now();
        Some(lease.id)
    }

    /// Gives `amount` back to a lease; `finished` when the reservation it
    /// held for is done with the lease (or was never made).
    pub(crate) fn give_back(&self, bucket_id: Uuid, lease: Uuid, amount: i64, finished: bool) {
        let mut all = self.buckets();
        if let Some(lease) = all
            .get_mut(&bucket_id)
            .and_then(|bucket| bucket.lease_mut(lease))
        {
            lease.available = lease
                .available
                .saturating_add(amount.max(0))
                .min(lease.amount);
            if finished {
                lease.in_flight = lease.in_flight.saturating_sub(1);
            }
        }
    }

    /// Stops reserving against a lease the database says is returned or no
    /// longer usable; a still-active one drains and is returned as usual.
    pub(crate) fn retire(&self, bucket_id: Uuid, lease: Uuid) {
        let mut all = self.buckets();
        if let Some(bucket) = all.get_mut(&bucket_id)
            && bucket
                .current
                .as_ref()
                .is_some_and(|current| current.id == lease)
        {
            bucket.drain_current();
        }
    }

    /// Returns idle and drained leases, and moves old ones to draining.
    /// Run about every second.
    pub async fn tick(&self) {
        let mut due = Vec::new();
        {
            let mut all = self.buckets();
            for bucket in all.values_mut() {
                if let Some(current) = &bucket.current {
                    if current.used.elapsed() >= IDLE_AFTER
                        || current.taken.elapsed() >= ROTATE_AFTER
                    {
                        bucket.drain_current();
                    }
                }
                bucket.draining.retain(|lease| {
                    if lease.in_flight == 0 {
                        due.push((lease.id, ReturnReason::Drained));
                        false
                    } else if lease.taken.elapsed() + RETURN_MARGIN >= LEASE_TTL
                        || lease.draining.is_some_and(|at| at.elapsed() >= DRAIN_GRACE)
                    {
                        due.push((lease.id, ReturnReason::Expired));
                        false
                    } else {
                        true
                    }
                });
            }
            all.retain(|_, bucket| {
                bucket.current.is_some()
                    || !bucket.draining.is_empty()
                    || bucket.taking
                    || bucket
                        .touched
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
            });
        }
        for (lease, reason) in due {
            self.return_one(lease, reason).await;
        }
    }

    /// Returns every lease this process holds (on shutdown).
    pub async fn return_all(&self) {
        let leases: Vec<Uuid> = {
            let mut all = self.buckets();
            all.values_mut()
                .flat_map(|bucket| {
                    bucket.drain_current();
                    std::mem::take(&mut bucket.draining)
                })
                .map(|lease| lease.id)
                .collect()
        };
        for lease in leases {
            self.return_one(lease, ReturnReason::Shutdown).await;
        }
    }

    async fn return_one(&self, lease: Uuid, reason: ReturnReason) {
        let result = async {
            let mut tx = self.pool.begin().await?;
            let returned = return_lease(&mut tx, lease, reason).await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(returned)
        }
        .await;
        match result {
            Ok(Some(returned)) => record_return(reason, returned),
            Ok(None) => {}
            // The worker returns it once it expires.
            Err(error) => tracing::warn!(%error, %lease, "budget lease not returned"),
        }
    }
}

pub(crate) fn record_return(reason: ReturnReason, returned: Returned) {
    let metrics = crate::metrics::global();
    metrics.inc(
        &crate::metrics::BUDGET_LEASES,
        &["returned", reason.as_str()],
    );
    metrics.add(
        &crate::metrics::BUDGET_LEASE_MINOR,
        &["used"],
        u64::try_from(returned.held.saturating_add(returned.settled)).unwrap_or(0),
    );
}

/// Takes a lease on a bucket for `holder`: the smaller of its share of what
/// remains and `want`, if that holds at least [`MIN_LEASE_REQUESTS`] of
/// `amount` and the bucket has room for [`MIN_REQUESTS_LEFT`].
async fn take(
    pool: &PgPool,
    holder: Uuid,
    organization_id: Uuid,
    bucket_id: Uuid,
    amount: i64,
    want: i64,
) -> Result<Option<(Uuid, i64)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT b.limit_minor, b.consumed_minor, b.reserved_minor,
                (SELECT count(DISTINCT l.holder) FROM gateway.budget_lease l
                 WHERE l.quota_bucket_id = b.id AND l.returned_at IS NULL AND l.holder <> $3)
         FROM gateway.quota_bucket b
         WHERE b.organization_id = $1 AND b.id = $2 AND b.end_user_id IS NULL
           AND b.retired_at IS NULL AND b.period_start <= now() AND b.period_end > now()
         FOR UPDATE OF b",
    )
    .bind(organization_id)
    .bind(bucket_id)
    .bind(holder)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((limit, consumed, reserved, others)) = row else {
        return Ok(None);
    };
    let remaining = limit.saturating_sub(consumed).saturating_sub(reserved);
    let Some(size) = lease_size(remaining, others, amount, want) else {
        return Ok(None);
    };
    sqlx::query(
        "UPDATE gateway.quota_bucket
         SET reserved_minor = reserved_minor + $2, leased_minor = leased_minor + $2
         WHERE id = $1",
    )
    .bind(bucket_id)
    .bind(size)
    .execute(&mut *tx)
    .await?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.budget_lease
             (id, organization_id, quota_bucket_id, holder, amount_minor, limit_minor, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp() + make_interval(secs => $7))",
    )
    .bind(id)
    .bind(organization_id)
    .bind(bucket_id)
    .bind(holder)
    .bind(size)
    .bind(limit)
    .bind(LEASE_TTL.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some((id, size)))
}

/// How much one lease takes: the smaller of an even share (among `others`
/// holders and this one) of [`SHARE_PERCENT`] of `remaining`, and `want`;
/// `None` when the budget is too near its limit or the lease too small.
pub(crate) fn lease_size(remaining: i64, others: i64, amount: i64, want: i64) -> Option<i64> {
    if amount <= 0 || remaining < amount.saturating_mul(MIN_REQUESTS_LEFT) {
        return None;
    }
    let share = remaining / 100 * SHARE_PERCENT / others.saturating_add(1).max(1);
    let size = share.min(want);
    (size >= amount.saturating_mul(MIN_LEASE_REQUESTS)).then_some(size)
}

/// Takes the advisory locks of `leases` shared and returns those still
/// active (not returned): what a finalization of their allocations may
/// leave inside the lease.
pub(crate) async fn lock_active(
    conn: &mut PgConnection,
    leases: &[Uuid],
) -> Result<HashSet<Uuid>, sqlx::Error> {
    lock_shared(conn, leases, false).await
}

/// Takes the advisory locks of `leases` shared and returns those a new
/// reservation may use: active, not near expiry, on a bucket still in force
/// with the limit it had when leased.
pub(crate) async fn lock_usable(
    conn: &mut PgConnection,
    leases: &[Uuid],
) -> Result<HashSet<Uuid>, sqlx::Error> {
    lock_shared(conn, leases, true).await
}

async fn lock_shared(
    conn: &mut PgConnection,
    leases: &[Uuid],
    usable: bool,
) -> Result<HashSet<Uuid>, sqlx::Error> {
    if leases.is_empty() {
        return Ok(HashSet::new());
    }
    let mut leases = leases.to_vec();
    leases.sort_unstable();
    leases.dedup();
    sqlx::query(
        "SELECT pg_advisory_xact_lock_shared($1, hashtext(t.id::text))
         FROM unnest($2::uuid[]) WITH ORDINALITY AS t(id, n) ORDER BY t.n",
    )
    .bind(LOCK_CLASS)
    .bind(&leases)
    .execute(&mut *conn)
    .await?;
    let active: Vec<Uuid> = if usable {
        sqlx::query_scalar(
            "SELECT l.id FROM gateway.budget_lease l
             JOIN gateway.quota_bucket b ON b.id = l.quota_bucket_id
             WHERE l.id = ANY($1) AND l.returned_at IS NULL
               AND l.expires_at > clock_timestamp() + interval '5 seconds'
               AND b.limit_minor = l.limit_minor AND b.retired_at IS NULL
               AND b.period_start <= now() AND b.period_end > now()",
        )
        .bind(&leases)
        .fetch_all(&mut *conn)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT id FROM gateway.budget_lease WHERE id = ANY($1) AND returned_at IS NULL",
        )
        .bind(&leases)
        .fetch_all(&mut *conn)
        .await?
    };
    Ok(active.into_iter().collect())
}

/// Takes `lease`'s advisory lock exclusively if no transaction is using
/// the lease now; whether it did.
pub(crate) async fn try_lock(conn: &mut PgConnection, lease: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1, hashtext($2::text))")
        .bind(LOCK_CLASS)
        .bind(lease)
        .fetch_one(&mut *conn)
        .await
}

/// Returns `lease` in the caller's transaction: what its allocations hold
/// moves into the bucket's `reserved_minor`, what they were charged into
/// its `consumed_minor`, and the rest is given back. Waits for transactions
/// using the lease; `None` if it was already returned.
pub(crate) async fn return_lease(
    conn: &mut PgConnection,
    lease: Uuid,
    reason: ReturnReason,
) -> Result<Option<Returned>, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::text))")
        .bind(LOCK_CLASS)
        .bind(lease)
        .execute(&mut *conn)
        .await?;
    let row: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT quota_bucket_id, amount_minor FROM gateway.budget_lease
         WHERE id = $1 AND returned_at IS NULL FOR UPDATE",
    )
    .bind(lease)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((bucket, amount)) = row else {
        return Ok(None);
    };
    let (held, settled): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(reserved_minor) FILTER (WHERE state = 'held'), 0)::BIGINT,
                COALESCE(sum(settled_minor) FILTER (WHERE state = 'settled'), 0)::BIGINT
         FROM gateway.accounting_allocation WHERE lease_id = $1",
    )
    .bind(lease)
    .fetch_one(&mut *conn)
    .await?;
    if held.saturating_add(settled) > amount {
        // Never expected: every hold under a lease came out of its amount and
        // no charge passes its hold. The allocations are the truth either way.
        tracing::error!(%lease, amount, held, settled, "budget lease used more than it held");
    }
    sqlx::query(
        "UPDATE gateway.quota_bucket
         SET reserved_minor = reserved_minor - $2 + $3,
             leased_minor = leased_minor - $2,
             consumed_minor = consumed_minor + $4
         WHERE id = $1",
    )
    .bind(bucket)
    .bind(amount)
    .bind(held)
    .bind(settled)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE gateway.budget_lease
         SET returned_at = clock_timestamp(), return_reason = $2,
             returned_held_minor = $3, returned_settled_minor = $4
         WHERE id = $1",
    )
    .bind(lease)
    .bind(reason.as_str())
    .bind(held)
    .bind(settled)
    .execute(&mut *conn)
    .await?;
    Ok(Some(Returned {
        amount,
        held,
        settled,
    }))
}

/// Returns up to `max` leases past their expiry, whoever holds them (the
/// worker runs this, so a crashed process's leases come back). Each in its
/// own transaction.
pub async fn return_expired(pool: &PgPool, max: i64) -> Result<u64, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gateway.budget_lease
         WHERE returned_at IS NULL AND expires_at <= $1 ORDER BY expires_at LIMIT $2",
    )
    .bind(Utc::now())
    .bind(max.clamp(1, 1_000))
    .fetch_all(pool)
    .await?;
    let mut count = 0;
    for lease in due {
        let mut tx = pool.begin().await?;
        let returned = return_lease(&mut tx, lease, ReturnReason::Expired).await?;
        tx.commit().await?;
        if let Some(returned) = returned {
            record_return(ReturnReason::Expired, returned);
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_is_the_smaller_of_its_share_and_the_recent_rate() {
        // 2% of 1,000,000 is 20,000; the recent rate asks for 5,000.
        assert_eq!(lease_size(1_000_000, 0, 100, 5_000), Some(5_000));
        // Shared with three other holders: 20,000 / 4.
        assert_eq!(lease_size(1_000_000, 3, 100, 50_000), Some(5_000));
        // Too near the limit: room for fewer than 100 reservations.
        assert_eq!(lease_size(9_999, 0, 100, 50_000), None);
        // Too small to cover 10 reservations.
        assert_eq!(lease_size(1_000_000, 0, 100, 999), None);
        assert_eq!(lease_size(40_000, 0, 100, 50_000), None);
        assert_eq!(lease_size(1_000_000, 0, 0, 50_000), None);
    }

    fn leases() -> BudgetLeases {
        BudgetLeases {
            holder: Uuid::new_v4(),
            pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            started: Instant::now(),
            buckets: Mutex::new(HashMap::new()),
            scopes: Mutex::new(HashMap::new()),
        }
    }

    fn install(leases: &BudgetLeases, bucket: Uuid, amount: i64) -> Uuid {
        let id = Uuid::new_v4();
        leases.buckets().entry(bucket).or_default().current = Some(Lease {
            id,
            amount,
            available: amount,
            in_flight: 0,
            taken: Instant::now(),
            used: Instant::now(),
            draining: None,
        });
        id
    }

    #[tokio::test]
    async fn memory_never_holds_more_than_the_lease() {
        let leases = leases();
        let bucket = Uuid::new_v4();
        let id = install(&leases, bucket, 250);
        assert_eq!(leases.hold(bucket, 100), Some(id));
        assert_eq!(leases.hold(bucket, 100), Some(id));
        // 50 left: the lease drains and the request goes to the bucket row.
        assert_eq!(leases.hold(bucket, 100), None);
        assert_eq!(
            leases.hold(bucket, 10),
            None,
            "a draining lease takes nothing new"
        );
        // A finalization's change comes back to the lease it was held from,
        // never past its amount.
        leases.give_back(bucket, id, 70, true);
        leases.give_back(bucket, id, 1_000, true);
        let all = leases.buckets();
        let lease = &all[&bucket].draining[0];
        assert_eq!((lease.available, lease.in_flight), (250, 0));
    }

    #[tokio::test]
    async fn a_retired_lease_takes_nothing_new() {
        let leases = leases();
        let bucket = Uuid::new_v4();
        let id = install(&leases, bucket, 1_000);
        assert_eq!(leases.hold(bucket, 100), Some(id));
        leases.retire(bucket, id);
        assert_eq!(leases.hold(bucket, 100), None);
        // What it held is still given back to it, not to a newer lease.
        let newer = install(&leases, bucket, 1_000);
        leases.give_back(bucket, id, 100, true);
        assert_eq!(leases.hold(bucket, 1_000), Some(newer));
    }
}
