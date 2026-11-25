//! Rate-limit policy rows and the admission check the reservation path runs.
//!
//! Admission reads a counter per policy (migration 0121) that triggers on
//! the reservations and batches keep in the same transaction as the row
//! they count: admissions per second over the last 61 seconds, the single
//! requests reserved or claimed, and, for a policy limiting tokens (0155),
//! tokens per second over the same seconds (each request its bound until it
//! settles, then what it reported, the difference kept on release rows). A request is in flight exactly
//! while its reservation is reserved or claimed, so one a crash abandoned
//! holds its slot until the expiry worker closes it. The rate window runs
//! on the database clock, which is also the clock that stamps `created_at`
//! when a reservation is admitted.
//!
//! With `GATEWAY_RATE_LIMIT_LEASES` on, most admissions go through a lease
//! instead (`persistence::rate_leases`, migration 0145): the lease counted
//! them on the same counters when it was taken, and this check is the path
//! for requests no lease covers.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    accounting::QuotaScope,
    end_user::{WindowEntry, token_admission},
    identity::{OrganizationId, UserAccountId},
    rate_limit::{
        RATE_WINDOW_SECONDS, RateLimitInput, RateLimitKind, RateLimitPolicyView, RateLimitScope,
    },
};

#[derive(Clone)]
pub struct RateLimitStore {
    pool: PgPool,
}

/// A request that would exceed a limit, whose policy it is, and how long
/// until it could be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateLimitBreach {
    pub kind: RateLimitKind,
    /// The level of the policy: `organization`, `team`, `service-account`
    /// or `api-key`.
    pub scope: &'static str,
    pub retry_after_seconds: u32,
    /// The fewest tokens the request can be served with (its input bound,
    /// reasoning allowance and one output token where it has output) are
    /// more than a `tokens_per_minute` limit: it never fits, whatever the
    /// wait.
    pub never_fits: bool,
}

/// What a new admission asks of the tokens-per-minute limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TokenDemand {
    /// A chat or embeddings request: the fewest tokens it can be served
    /// with (its input bound and reasoning allowance, and one output token
    /// where it reserves output).
    Request { min_tokens: u64 },
    /// A batch: one admission, refused while a token window is used up; its
    /// lines are not counted.
    Batch,
}

/// The outcome of [`RateLimitStore::check_admission`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    /// Admitted; under a tokens-per-minute limit, the fewest tokens any of
    /// them has left for this request, which caps its output.
    Admitted {
        token_room: Option<u64>,
    },
    Refused(RateLimitBreach),
}

#[derive(FromRow)]
struct PolicyRow {
    id: Uuid,
    scope_kind: String,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    requests_per_minute: Option<i32>,
    tokens_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<PolicyRow> for RateLimitPolicyView {
    fn from(row: PolicyRow) -> Self {
        Self {
            id: row.id,
            scope: row.scope_kind,
            team_id: row.team_id,
            service_account_id: row.service_account_id,
            api_key_id: row.api_key_id,
            requests_per_minute: row.requests_per_minute.and_then(|v| u32::try_from(v).ok()),
            tokens_per_minute: row.tokens_per_minute.and_then(|v| u32::try_from(v).ok()),
            max_concurrent: row.max_concurrent.and_then(|v| u32::try_from(v).ok()),
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

const POLICY_SELECT: &str = "SELECT p.id, p.scope_kind, p.team_id, p.service_account_id,
        p.api_key_id, p.requests_per_minute, p.tokens_per_minute, p.max_concurrent, v.version,
        p.created_at, p.updated_at
    FROM gateway.rate_limit_policy p
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = p.organization_id
     AND v.resource_type = 'rate_limit_policy' AND v.resource_id = p.id";

impl RateLimitStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One page of policies ordered by id, after the `after` keyset. The
    /// flag reports whether more rows follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<Uuid>,
    ) -> Result<(Vec<RateLimitPolicyView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, PolicyRow>(&format!(
            "{POLICY_SELECT}
             WHERE p.organization_id = $1 AND ($2::uuid IS NULL OR p.id > $2)
             ORDER BY p.id
             LIMIT $3"
        ))
        .bind(organization_id.0)
        .bind(after)
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

    /// One policy on the caller's connection, so a mutation can read the row
    /// and version its own transaction wrote.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<RateLimitPolicyView>, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(&format!(
            "{POLICY_SELECT} WHERE p.organization_id = $1 AND p.id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Whether the team, service account or API key a policy names belongs
    /// to the organization. The row is share-locked so it cannot be deleted
    /// before the policy referencing it commits. A revoked key still exists:
    /// its policy can be read, changed and removed, as its budget can.
    pub async fn scope_exists(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        scope: RateLimitScope,
    ) -> Result<bool, sqlx::Error> {
        let sql = match scope {
            RateLimitScope::Organization => return Ok(true),
            RateLimitScope::Team(_) => {
                "SELECT id FROM gateway.team WHERE organization_id = $1 AND id = $2 FOR SHARE"
            }
            RateLimitScope::ServiceAccount(_) => {
                "SELECT id FROM gateway.service_account
                 WHERE organization_id = $1 AND id = $2 FOR SHARE"
            }
            RateLimitScope::ApiKey(_) => {
                "SELECT id FROM gateway.api_key WHERE organization_id = $1 AND id = $2 FOR SHARE"
            }
        };
        let subject = scope
            .team_id()
            .map(|team| team.0)
            .or(scope.service_account_id().map(|account| account.0))
            .or(scope.api_key_id().map(|key| key.0));
        Ok(sqlx::query_scalar::<_, Uuid>(sql)
            .bind(organization_id.0)
            .bind(subject)
            .fetch_optional(&mut *conn)
            .await?
            .is_some())
    }

    /// Insert or replace the policy, and recount its counter from the
    /// reservations and batches themselves. Returns true when a row was
    /// created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        scope: RateLimitScope,
        input: RateLimitInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        // Admission takes a key-share lock on the organization's row before
        // the policy's, as every reservation and batch insert does. Locked
        // first here, it waits for those being written and holds off new
        // ones until this transaction ends, so the recount below neither
        // misses nor double counts one.
        sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
            .bind(organization_id.0)
            .execute(&mut *conn)
            .await?;
        let created = sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.rate_limit_policy
                 (id, organization_id, scope_kind, team_id, service_account_id, api_key_id,
                  requests_per_minute, tokens_per_minute, max_concurrent, created_by,
                  updated_by)
             VALUES ($1, $2, $3, $4, $5, $9, $6, $10, $7, $8, $8)
             ON CONFLICT (id) DO UPDATE
             SET requests_per_minute = EXCLUDED.requests_per_minute,
                 tokens_per_minute = EXCLUDED.tokens_per_minute,
                 max_concurrent = EXCLUDED.max_concurrent,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(scope.kind())
        .bind(scope.team_id().map(|team| team.0))
        .bind(scope.service_account_id().map(|account| account.0))
        .bind(input.requests_per_minute.map(limit))
        .bind(input.max_concurrent.map(limit))
        .bind(actor.0)
        .bind(scope.api_key_id().map(|key| key.0))
        .bind(input.tokens_per_minute.map(limit))
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        Ok(created)
    }

    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "DELETE FROM gateway.rate_limit_policy WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }

    /// Check every policy covering `scope` (its key's, service account's,
    /// team's and organization's) before a new reservation is inserted in
    /// the same transaction. The policy rows stay locked until that
    /// transaction ends, so concurrent admissions to one scope are counted
    /// one after another. Each policy's counter (kept by triggers on the
    /// reservations and batches themselves, migrations 0121 and 0155) is
    /// one fixed-size row, so the locked section does not grow with
    /// traffic. Refuses with the breach that waits longest when more than
    /// one limit is reached, or one that never fits.
    pub(crate) async fn check_admission(
        conn: &mut PgConnection,
        scope: &QuotaScope,
        demand: TokenDemand,
    ) -> Result<Admission, sqlx::Error> {
        // Locks the organization (as the reservation insert's foreign key
        // will) and then the policies, the order of an admin write, so the
        // two cannot deadlock. A function, so that a data plane's role can
        // take the locks without the right to update either table.
        let counters = sqlx::query_as::<_, CounterRow>(
            "SELECT scope_kind, requests_per_minute, max_concurrent, tokens_per_minute,
                    in_flight, last_second, slots, token_window, now_epoch
             FROM gateway.lock_rate_limit_counters($1, $2, $3, $4)",
        )
        .bind(scope.organization_id.0)
        .bind(scope.team_id.map(|team| team.0))
        .bind(scope.service_account_id.map(|account| account.0))
        .bind(scope.api_key_id.map(|key| key.0))
        .fetch_all(&mut *conn)
        .await?;
        let mut breach: Option<RateLimitBreach> = None;
        let mut token_room: Option<u64> = None;
        for counter in counters {
            let level = level(&counter.scope_kind);
            if let Some(limit) = counter.requests_per_minute {
                // A batch is one admission, whatever its lines (D-021).
                let window = Window::new(&counter.slots, counter.last_second, counter.now_epoch);
                if let Some(wait) = window.wait_below(i64::from(limit)) {
                    breach = worst(
                        breach,
                        refusal(RateLimitKind::RequestsPerMinute, level, retry_after(wait)),
                    );
                }
            }
            // Concurrency counts single requests only, since a batch runs
            // for up to a day. In-flight requests usually finish within
            // seconds; there is no better estimate of when one will.
            if let Some(limit) = counter.max_concurrent {
                if counter.in_flight >= i64::from(limit) {
                    breach = worst(breach, refusal(RateLimitKind::Concurrency, level, 1));
                }
            }
            if let (Some(limit), Some(window)) = (counter.tokens_per_minute, &counter.token_window)
            {
                let limit = u64::try_from(limit).unwrap_or(0);
                let needed = match demand {
                    TokenDemand::Request { min_tokens } => min_tokens,
                    TokenDemand::Batch => 1,
                };
                match token_admission(limit, needed, &token_entries(window, counter.now_epoch)) {
                    Ok(room) => token_room = Some(token_room.map_or(room, |r| r.min(room))),
                    Err(retry_after) => {
                        let mut refused = refusal(
                            RateLimitKind::TokensPerMinute,
                            level,
                            retry_after.unwrap_or(RATE_WINDOW_SECONDS as u32),
                        );
                        refused.never_fits = retry_after.is_none();
                        breach = worst(breach, refused);
                    }
                }
            }
        }
        Ok(match breach {
            Some(breach) => Admission::Refused(breach),
            None => Admission::Admitted { token_room },
        })
    }

    /// Whether a policy limiting tokens covers `scope`, with the
    /// organization's row key-share locked, as admission locks it, so none
    /// can be set or removed before the caller's transaction ends.
    pub(crate) async fn tokens_limited(
        conn: &mut PgConnection,
        scope: &QuotaScope,
    ) -> Result<bool, sqlx::Error> {
        // A function, so that a data plane's role can take the lock.
        sqlx::query_scalar("SELECT gateway.lock_rate_limit_tokens_covered($1, $2, $3, $4)")
            .bind(scope.organization_id.0)
            .bind(scope.team_id.map(|team| team.0))
            .bind(scope.service_account_id.map(|account| account.0))
            .bind(scope.api_key_id.map(|key| key.0))
            .fetch_one(&mut *conn)
            .await
    }
}

/// A token window's seconds as end-user limits read their requests: oldest
/// first, each with the seconds until it leaves the window. `window` holds
/// the 61 seconds up to `now`.
fn token_entries(window: &[i64], now: f64) -> Vec<WindowEntry> {
    let first = now.floor() as i64 - RATE_WINDOW_SECONDS;
    window
        .iter()
        .enumerate()
        .map(|(offset, &tokens)| WindowEntry {
            // Second `s` stops being counted once `now` reaches s + 61.
            leaves_in_seconds: (first + offset as i64 + RING) as f64 - now,
            tokens: u64::try_from(tokens).unwrap_or(0),
        })
        .collect()
}

/// A policy's level as a refusal names it.
fn level(scope_kind: &str) -> &'static str {
    match scope_kind {
        "team" => "team",
        "service-account" => "service-account",
        "api-key" => "api-key",
        _ => "organization",
    }
}

fn refusal(kind: RateLimitKind, scope: &'static str, retry_after_seconds: u32) -> RateLimitBreach {
    RateLimitBreach {
        kind,
        scope,
        retry_after_seconds,
        never_fits: false,
    }
}

#[derive(FromRow)]
struct CounterRow {
    scope_kind: String,
    requests_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
    tokens_per_minute: Option<i32>,
    in_flight: i64,
    last_second: i64,
    slots: Vec<i32>,
    token_window: Option<Vec<i64>>,
    now_epoch: f64,
}

/// A policy's admissions per second over the last 61 seconds (a ring
/// indexed by the epoch second mod 61, `newest` the latest second written),
/// read at `now`.
struct Window<'a> {
    slots: &'a [i32],
    newest: i64,
    now: f64,
}

impl<'a> Window<'a> {
    fn new(slots: &'a [i32], newest: i64, now: f64) -> Self {
        Self { slots, newest, now }
    }

    /// Admissions in `second`, while the ring still holds it.
    fn at(&self, second: i64) -> i64 {
        if second > self.newest || second <= self.newest - RING || self.slots.len() != RING_LEN {
            return 0;
        }
        i64::from(self.slots[second.rem_euclid(RING) as usize])
    }

    /// The seconds counted at `now`: every one overlapping the minute
    /// before it, so the count is never below the admissions of that
    /// minute (it may include up to one second more).
    fn seconds(&self) -> std::ops::RangeInclusive<i64> {
        let now = self.now.floor() as i64;
        (now - RATE_WINDOW_SECONDS)..=now.max(self.newest)
    }

    fn count(&self) -> i64 {
        self.seconds().map(|second| self.at(second)).sum()
    }

    /// Seconds until fewer than `limit` admissions are counted, if there
    /// are `limit` or more now: until the oldest seconds whose leaving
    /// brings the count under the limit have left the window.
    fn wait_below(&self, limit: i64) -> Option<f64> {
        let mut count = self.count();
        if count < limit {
            return None;
        }
        for second in self.seconds() {
            count -= self.at(second);
            if count < limit {
                // Second `s` stops being counted once `now` reaches s + 61.
                return Some((second + RING) as f64 - self.now);
            }
        }
        Some(RATE_WINDOW_SECONDS as f64)
    }
}

/// The admissions a counter's ring counts at `now`, as admission reads them.
pub(crate) fn window_count(slots: &[i32], last_second: i64, now: f64) -> i64 {
    Window::new(slots, last_second, now).count()
}

/// Seconds the ring keeps: the window and the second it partly covers.
const RING: i64 = RATE_WINDOW_SECONDS + 1;
const RING_LEN: usize = RING as usize;

fn limit(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Whole seconds until a request can be admitted: at least 1, at most the
/// window.
fn retry_after(wait: f64) -> u32 {
    wait.ceil().clamp(1.0, RATE_WINDOW_SECONDS as f64) as u32
}

/// The breach a refusal reports: one that never fits, else the longest
/// wait (the first found on a tie).
fn worst(current: Option<RateLimitBreach>, next: RateLimitBreach) -> Option<RateLimitBreach> {
    match current {
        Some(current)
            if current.never_fits
                || (!next.never_fits
                    && current.retry_after_seconds >= next.retry_after_seconds) =>
        {
            Some(current)
        }
        _ => Some(next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_rounds_up_within_the_window() {
        assert_eq!(retry_after(0.2), 1);
        assert_eq!(retry_after(-3.0), 1);
        assert_eq!(retry_after(12.01), 13);
        assert_eq!(retry_after(90.0), 60);
    }

    /// A ring with `counts` admissions at the given seconds, the newest
    /// written at `newest`.
    fn ring(counts: &[(i64, i32)]) -> Vec<i32> {
        let mut slots = vec![0; RING_LEN];
        for (second, count) in counts {
            slots[second.rem_euclid(RING) as usize] += count;
        }
        slots
    }

    #[test]
    fn the_window_counts_every_second_overlapping_the_last_minute() {
        let slots = ring(&[(1_000, 3), (1_040, 2), (1_060, 1)]);
        // At 1060.5 the minute before reaches back to 1000.5: second 1000
        // overlaps it, so it counts (never fewer than were admitted).
        let window = Window::new(&slots, 1_060, 1_060.5);
        assert_eq!(window.count(), 6);
        assert_eq!(window.wait_below(7), None);
        // Under 6: second 1000 leaves at 1061, 0.5 s from now.
        assert_eq!(window.wait_below(6), Some(0.5));
        // Under 3: 1040 must leave too, at 1101.
        assert_eq!(window.wait_below(3), Some(40.5));
        // Under 1: everything, the last at 1121.
        assert_eq!(window.wait_below(1), Some(60.5));
        // A minute and a second on, only 1060 is left.
        assert_eq!(Window::new(&slots, 1_060, 1_101.0).count(), 1);
        // Past the ring: nothing.
        assert_eq!(Window::new(&slots, 1_060, 1_200.0).count(), 0);
    }

    #[test]
    fn the_ring_holds_only_its_last_61_seconds() {
        // Slot 1000 mod 61 was reused by second 1061: it is not counted for
        // 1000 again.
        let slots = ring(&[(1_061, 4)]);
        let window = Window::new(&slots, 1_061, 1_061.2);
        assert_eq!(window.at(1_000), 0);
        assert_eq!(window.at(1_061), 4);
        assert_eq!(window.count(), 4);
        // A malformed ring counts nothing rather than panicking.
        assert_eq!(Window::new(&[1, 2], 1_061, 1_061.2).count(), 0);
    }

    #[test]
    fn the_longest_wait_wins_unless_a_request_never_fits() {
        let breach = worst(None, refusal(RateLimitKind::Concurrency, "api-key", 1));
        let breach = worst(
            breach,
            refusal(RateLimitKind::RequestsPerMinute, "organization", 30),
        );
        let breach = worst(breach, refusal(RateLimitKind::Concurrency, "team", 1));
        assert_eq!(
            breach,
            Some(refusal(
                RateLimitKind::RequestsPerMinute,
                "organization",
                30
            ))
        );
        let never = RateLimitBreach {
            never_fits: true,
            ..refusal(RateLimitKind::TokensPerMinute, "api-key", 60)
        };
        let breach = worst(breach, never);
        assert_eq!(breach, Some(never));
        assert_eq!(
            worst(
                breach,
                refusal(RateLimitKind::RequestsPerMinute, "team", 60)
            ),
            Some(never)
        );
    }

    #[test]
    fn token_window_seconds_leave_in_order() {
        let mut window = vec![0_i64; RING_LEN];
        window[0] = 500;
        window[60] = 100;
        let entries = token_entries(&window, 1_060.25);
        assert_eq!(entries.len(), RING_LEN);
        // The oldest second (1000) leaves at 1061, 0.75 s from now; the
        // newest (1060) at 1121.
        assert_eq!(entries[0].leaves_in_seconds, 0.75);
        assert_eq!(entries[0].tokens, 500);
        assert_eq!(entries[60].leaves_in_seconds, 60.75);
        // 600 of 1000 used: 400 left; 450 fit once second 1000 has left.
        assert_eq!(token_admission(1_000, 400, &entries), Ok(400));
        assert_eq!(token_admission(1_000, 450, &entries), Err(Some(1)));
        assert_eq!(token_admission(1_000, 1_001, &entries), Err(None));
    }
}
