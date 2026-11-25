//! Money correctness after a run, for the run's organizations only:
//!
//! - nothing is left reserved or claimed once the worker has had time to
//!   expire what a failure abandoned;
//! - every reservation in `unknown` has a reconciliation item;
//! - no reservation has more than one final usage record (no double
//!   charge), and no request (correlation id) more than one reservation
//!   that is not released (a failover releases the attempt it replaces);
//! - every budget's `reserved` equals what its held allocations hold and
//!   its `consumed` what its settled allocations charged, and no budget's
//!   consumed plus reserved passes its limit (no overspend); with budget
//!   leases, an active lease's amount is part of `reserved` and the
//!   allocations under it are checked against the lease, never more than
//!   its amount;
//! - every rate-limit policy's in-flight count (an API key's policy's
//!   too), as admission reads it, is what is in flight outside active
//!   rate-limit leases plus what those leases still count, and no
//!   rate-limit lease admitted more than it counted;
//! - every tokens-per-minute policy's window, as admission reads it (the
//!   counter's ring and the release rows'), is what the reservations in it
//!   count (each its bound until settled, then what it reported), and no
//!   rate-limit lease admitted a request under a policy that limits tokens
//!   or a request of a key with a policy of its own;
//! - for each audited request: an answered one is settled once at exactly
//!   the price of the usage the gateway reported; any other is in a final
//!   state, charged no more than it reserved, and one every deployment
//!   refused with 429 is released.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    directive::Fault,
    generator::{AuditRecord, Kind},
};

#[derive(Debug, Default, Serialize)]
pub struct Accounting {
    pub settle_wait_s: f64,
    pub reservations: HashMap<String, i64>,
    pub reconciliation_open: i64,
    pub reconciliation_by_reason: HashMap<String, i64>,
    pub violations: Vec<String>,
    pub violation_count: u64,
    pub audited: u64,
    /// Audited requests by fault and the reservation's final state.
    pub audit_states: HashMap<String, u64>,
    pub charged_minor: i64,
    pub tight_budgets: Vec<Value>,
    /// Budget leases taken during the run, and those still active.
    pub leases_taken: i64,
    pub leases_active: i64,
    /// Rate-limit leases taken during the run, and those still active.
    pub rate_leases_taken: i64,
    pub rate_leases_active: i64,
}

impl Accounting {
    fn violation(&mut self, message: String) {
        self.violation_count += 1;
        if self.violations.len() < 50 {
            self.violations.push(message);
        }
    }

    pub fn exact(&self) -> bool {
        self.violation_count == 0
    }
}

async fn open_reservations(pool: &PgPool, orgs: &[Uuid]) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = ANY($1) AND state IN ('reserved', 'claimed')",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    correlation_id: Uuid,
    reservations: i64,
    state: String,
    reserved_minor: i64,
    usage_rows: i64,
    amount: Option<i64>,
    reconciliation: Option<String>,
}

pub async fn check(
    pool: &PgPool,
    orgs: &[Uuid],
    tight: &[Uuid],
    audit: &[AuditRecord],
    requests_sent: u64,
    sole_generator: bool,
    settle_timeout: Duration,
) -> Result<Accounting, sqlx::Error> {
    let mut result = Accounting::default();

    // Wait for in-flight work and the worker's expiry of abandoned
    // reservations (it runs every 30 s, after each reservation's TTL).
    let started = Instant::now();
    loop {
        match open_reservations(pool, orgs).await {
            Ok(0) => break,
            Ok(_) | Err(_) if started.elapsed() >= settle_timeout => break,
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    result.settle_wait_s = started.elapsed().as_secs_f64();

    let states: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, count(*) FROM gateway.accounting_reservation
         WHERE organization_id = ANY($1) GROUP BY state",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    result.reservations = states.into_iter().collect();
    // A failover's next attempt has its own reservation, keyed after the
    // one it replaces; each request has one first reservation.
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = ANY($1) AND idempotency_key NOT LIKE 'failover:%'",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await?;
    for state in ["reserved", "claimed"] {
        if let Some(count) = result.reservations.get(state).filter(|count| **count > 0) {
            result.violation(format!(
                "{count} reservations still {state} after {:.0} s",
                result.settle_wait_s
            ));
        }
    }
    if sole_generator && total as u64 > requests_sent {
        result.violation(format!(
            "{total} reservations for {requests_sent} requests sent"
        ));
    }

    let unreconciled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation r
         WHERE r.organization_id = ANY($1) AND r.state = 'unknown'
           AND NOT EXISTS (SELECT 1 FROM gateway.accounting_reconciliation c
                           WHERE c.reservation_id = r.id)",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await?;
    if unreconciled > 0 {
        result.violation(format!(
            "{unreconciled} unknown reservations without a reconciliation item"
        ));
    }
    let reasons: Vec<(String, i64)> = sqlx::query_as(
        "SELECT reason || ':' || status, count(*) FROM gateway.accounting_reconciliation
         WHERE organization_id = ANY($1) GROUP BY 1",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    result.reconciliation_open = reasons
        .iter()
        .filter(|(reason, _)| reason.ends_with(":open"))
        .map(|(_, count)| count)
        .sum();
    result.reconciliation_by_reason = reasons.into_iter().collect();

    let doubled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT reservation_id FROM gateway.accounting_usage
             WHERE organization_id = ANY($1) AND finality = 'final'
             GROUP BY reservation_id HAVING count(*) > 1) d",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await?;
    if doubled > 0 {
        result.violation(format!(
            "{doubled} reservations with more than one final usage record"
        ));
    }
    let charged: Option<i64> = sqlx::query_scalar(
        "SELECT sum(amount_minor)::BIGINT FROM gateway.accounting_usage
         WHERE organization_id = ANY($1) AND finality = 'final'",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await?;
    result.charged_minor = charged.unwrap_or(0);

    // Every budget against its allocations. With budget leases on, an
    // active lease's amount is reserved on its bucket and the allocations
    // made under it move nothing there until it is returned, so they are
    // checked against the lease instead; returned leases' allocations are
    // the bucket's own.
    let buckets: Vec<(Uuid, String, i64, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT b.id, b.scope_kind, b.limit_minor, b.reserved_minor, b.consumed_minor,
                b.leased_minor,
                COALESCE(sum(a.reserved_minor) FILTER (WHERE a.state = 'held' AND l.id IS NULL), 0)::BIGINT,
                COALESCE(sum(a.settled_minor) FILTER (WHERE a.state = 'settled' AND l.id IS NULL), 0)::BIGINT,
                (SELECT COALESCE(sum(x.amount_minor), 0)::BIGINT FROM gateway.budget_lease x
                 WHERE x.quota_bucket_id = b.id AND x.returned_at IS NULL)
         FROM gateway.quota_bucket b
         LEFT JOIN gateway.accounting_allocation a ON a.quota_bucket_id = b.id
         LEFT JOIN gateway.budget_lease l ON l.id = a.lease_id AND l.returned_at IS NULL
         WHERE b.organization_id = ANY($1)
         GROUP BY b.id",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    for (id, scope, limit, reserved, consumed, leased, held, settled, active) in buckets {
        if leased != active {
            result.violation(format!(
                "{scope} budget {id}: leased {leased}, its active leases {active}"
            ));
        }
        if reserved != held + leased {
            result.violation(format!(
                "{scope} budget {id}: reserved {reserved}, its held allocations {held} and leases {leased}"
            ));
        }
        if consumed != settled {
            result.violation(format!(
                "{scope} budget {id}: consumed {consumed}, its settled allocations {settled}"
            ));
        }
        if consumed + reserved > limit {
            result.violation(format!(
                "{scope} budget {id} overspent: consumed {consumed} + reserved {reserved} > limit {limit}"
            ));
        }
    }
    // Each active lease against the allocations made under it: they never
    // hold and charge more than it has.
    let overdrawn: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "SELECT l.id, l.amount_minor,
                (COALESCE(sum(a.reserved_minor) FILTER (WHERE a.state = 'held'), 0)
                 + COALESCE(sum(a.settled_minor) FILTER (WHERE a.state = 'settled'), 0))::BIGINT
         FROM gateway.budget_lease l
         LEFT JOIN gateway.accounting_allocation a ON a.lease_id = l.id
         WHERE l.organization_id = ANY($1) AND l.returned_at IS NULL
         GROUP BY l.id
         HAVING (COALESCE(sum(a.reserved_minor) FILTER (WHERE a.state = 'held'), 0)
                 + COALESCE(sum(a.settled_minor) FILTER (WHERE a.state = 'settled'), 0))
                > l.amount_minor",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    for (lease, amount, used) in overdrawn {
        result.violation(format!(
            "budget lease {lease} overdrawn: its allocations used {used} of {amount}"
        ));
    }
    // How many leases were taken, and how many are still out.
    let leases: (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE returned_at IS NULL), count(*)
         FROM gateway.budget_lease WHERE organization_id = ANY($1)",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await?;
    result.leases_active = leases.0;
    result.leases_taken = leases.1;
    // Each rate-limit policy's in-flight count against the reservations, in
    // one statement (one snapshot): an active rate-limit lease counted all
    // its admissions in flight when it was taken, and its reservations leave
    // flight on the policy's release rows as any other.
    let policies: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "WITH active AS (
             SELECT l.*
             FROM gateway.rate_limit_lease l
             WHERE l.organization_id = ANY($1) AND l.returned_at IS NULL)
         SELECT p.id,
                COALESCE((SELECT c.in_flight - COALESCE((
                              SELECT sum(x.released) FROM gateway.rate_limit_release x
                              WHERE x.policy_id = p.id), 0)
                          FROM gateway.rate_limit_counter c WHERE c.policy_id = p.id), 0)::BIGINT,
                ((SELECT count(*) FROM gateway.accounting_reservation r
                  WHERE r.organization_id = p.organization_id AND r.batch_id IS NULL
                    AND r.state IN ('reserved', 'claimed')
                    AND (p.scope_kind = 'organization'
                         OR (p.scope_kind = 'team' AND r.team_id = p.team_id)
                         OR (p.scope_kind = 'service-account'
                             AND r.service_account_id = p.service_account_id)
                         OR (p.scope_kind = 'api-key' AND r.api_key_id = p.api_key_id))
                    AND NOT EXISTS (
                        SELECT 1 FROM active a
                        WHERE a.id = r.rate_lease_id AND p.id = ANY (a.policy_ids)
                          AND gateway.rate_lease_counts(
                              a.organization_id, a.team_id, a.service_account_id, a.admissions,
                              a.created_at, a.valid_until, r.organization_id, r.team_id,
                              r.service_account_id, r.batch_id, r.rate_lease_ticket,
                              r.rate_admitted_at)))
                 + (SELECT COALESCE(sum(a.admissions - (
                        SELECT count(*) FROM gateway.accounting_reservation r
                        WHERE r.rate_lease_id = a.id AND gateway.rate_lease_counts(
                              a.organization_id, a.team_id, a.service_account_id, a.admissions,
                              a.created_at, a.valid_until, r.organization_id, r.team_id,
                              r.service_account_id, r.batch_id, r.rate_lease_ticket,
                              r.rate_admitted_at)
                          AND r.state NOT IN ('reserved', 'claimed'))), 0)
                    FROM active a WHERE p.id = ANY (a.policy_ids)))::BIGINT
         FROM gateway.rate_limit_policy p
         WHERE p.organization_id = ANY($1)",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    for (policy, counted, expected) in policies {
        if counted != expected {
            result.violation(format!(
                "rate-limit policy {policy}: {counted} counted in flight, {expected} expected"
            ));
        }
    }
    let overdrawn: Vec<(Uuid, i32, i64)> = sqlx::query_as(
        "SELECT l.id, l.admissions, count(*)
         FROM gateway.rate_limit_lease l
         JOIN gateway.accounting_reservation r
           ON r.rate_lease_id = l.id
          AND gateway.rate_lease_counts(
              l.organization_id, l.team_id, l.service_account_id, l.admissions, l.created_at,
              l.valid_until, r.organization_id, r.team_id, r.service_account_id, r.batch_id,
              r.rate_lease_ticket, r.rate_admitted_at)
         WHERE l.organization_id = ANY($1)
         GROUP BY l.id HAVING count(*) > l.admissions",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    for (lease, admissions, used) in overdrawn {
        result.violation(format!(
            "rate-limit lease {lease} admitted {used} of its {admissions}"
        ));
    }
    // Each tokens-per-minute policy's window against the reservations, in
    // one statement: the counter's ring plus the release rows' rings, second
    // by second, as admission reads them, and each reservation in those
    // seconds at what it counts now. Exact once nothing is settling.
    let windows: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "WITH clock AS (SELECT floor(extract(epoch FROM clock_timestamp()))::BIGINT AS top)
         SELECT p.id,
             COALESCE((SELECT sum(GREATEST(0,
                  gateway.rate_limit_ring_at(c.token_slots, c.token_last_second, g.second)
                  + COALESCE((SELECT sum(gateway.rate_limit_ring_at(
                                  x.token_slots, x.token_last_second, g.second))
                              FROM gateway.rate_limit_release x WHERE x.policy_id = p.id), 0)))
              FROM gateway.rate_limit_counter c, clock,
                   generate_series(clock.top - 60, clock.top) AS g(second)
              WHERE c.policy_id = p.id), 0)::BIGINT,
             COALESCE((SELECT sum(gateway.rate_limit_reservation_tokens(
                         r.organization_id, r.id, r.state, r.token_bound))
              FROM gateway.accounting_reservation r, clock
              WHERE r.organization_id = p.organization_id AND r.batch_id IS NULL
                AND (p.scope_kind = 'organization'
                     OR (p.scope_kind = 'team' AND r.team_id = p.team_id)
                     OR (p.scope_kind = 'service-account'
                         AND r.service_account_id = p.service_account_id)
                     OR (p.scope_kind = 'api-key' AND r.api_key_id = p.api_key_id))
                AND r.created_at >= to_timestamp(clock.top - 60)
                AND floor(extract(epoch FROM COALESCE(r.tokens_counted_at, r.created_at)))
                    >= clock.top - 60), 0)::BIGINT
         FROM gateway.rate_limit_policy p
         WHERE p.organization_id = ANY($1) AND p.tokens_per_minute IS NOT NULL",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    for (policy, counted, expected) in windows {
        if counted != expected {
            result.violation(format!(
                "rate-limit policy {policy}: {counted} tokens counted in its window, {expected} expected"
            ));
        }
    }
    // Leases cover neither: none admitted a request once a policy it names
    // limited tokens, nor a request of a key with a policy of its own.
    let uncovered: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT r.rate_lease_id, 'admitted a request under a tokens-per-minute policy'
         FROM gateway.accounting_reservation r
         JOIN gateway.rate_limit_lease l
           ON l.organization_id = r.organization_id AND l.id = r.rate_lease_id
         JOIN gateway.rate_limit_policy p ON p.id = ANY (l.policy_ids)
         WHERE r.organization_id = ANY($1) AND p.tokens_per_minute IS NOT NULL
           AND r.created_at > p.updated_at
         UNION ALL
         SELECT DISTINCT r.rate_lease_id, 'admitted a request of a key with a policy'
         FROM gateway.accounting_reservation r
         JOIN gateway.rate_limit_policy p
           ON p.organization_id = r.organization_id AND p.scope_kind = 'api-key'
          AND p.api_key_id = r.api_key_id
         WHERE r.organization_id = ANY($1) AND r.rate_lease_id IS NOT NULL
           AND r.created_at > p.updated_at",
    )
    .bind(orgs)
    .fetch_all(pool)
    .await?;
    for (lease, what) in uncovered {
        result.violation(format!("rate-limit lease {lease} {what}"));
    }
    let rate_leases: (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE returned_at IS NULL), count(*)
         FROM gateway.rate_limit_lease WHERE organization_id = ANY($1)",
    )
    .bind(orgs)
    .fetch_one(pool)
    .await?;
    result.rate_leases_active = rate_leases.0;
    result.rate_leases_taken = rate_leases.1;
    let tight_budgets: Vec<(Uuid, i64, i64, i64)> = sqlx::query_as(
        "SELECT organization_id, limit_minor, consumed_minor, reserved_minor
         FROM gateway.quota_bucket WHERE organization_id = ANY($1) AND scope_kind = 'organization'",
    )
    .bind(tight)
    .fetch_all(pool)
    .await?;
    result.tight_budgets = tight_budgets
        .into_iter()
        .map(|(org, limit, consumed, reserved)| {
            json!({"organization": org, "limit_minor": limit, "consumed_minor": consumed,
                   "reserved_minor": reserved,
                   "used_share": (consumed + reserved) as f64 / limit.max(1) as f64})
        })
        .collect();

    // The audit: what each request the client saw was charged.
    let by_id: HashMap<Uuid, &AuditRecord> = audit
        .iter()
        .map(|record| (record.correlation, record))
        .collect();
    let mut found: HashMap<Uuid, AuditRow> = HashMap::new();
    for chunk in audit.chunks(20_000) {
        let ids: Vec<Uuid> = chunk.iter().map(|record| record.correlation).collect();
        // A failover settles on a new reservation and releases the one
        // before it, so a request may have several; at most one may be
        // anything but released.
        let rows: Vec<AuditRow> = sqlx::query_as(
            "WITH request AS (
                 SELECT DISTINCT t.correlation_id, t.reservation_id
                 FROM gateway.inference_trace t
                 WHERE t.organization_id = ANY($1) AND t.correlation_id = ANY($2))
             SELECT q.correlation_id,
                    count(*) FILTER (WHERE r.state <> 'released') AS reservations,
                    COALESCE(max(r.state) FILTER (WHERE r.state <> 'released'), 'released')
                        AS state,
                    COALESCE(max(r.reserved_minor) FILTER (WHERE r.state <> 'released'),
                             max(r.reserved_minor)) AS reserved_minor,
                    COALESCE(sum(u.rows), 0)::BIGINT AS usage_rows,
                    sum(u.amount)::BIGINT AS amount,
                    max(c.status) AS reconciliation
             FROM request q
             JOIN gateway.accounting_reservation r ON r.id = q.reservation_id
             LEFT JOIN LATERAL (
                 SELECT count(*) AS rows, sum(amount_minor)::BIGINT AS amount
                 FROM gateway.accounting_usage
                 WHERE reservation_id = r.id AND finality = 'final') u ON true
             LEFT JOIN gateway.accounting_reconciliation c ON c.reservation_id = r.id
             GROUP BY q.correlation_id",
        )
        .bind(orgs)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        for row in rows {
            found.insert(row.correlation_id, row);
        }
    }
    for (id, record) in &by_id {
        result.audited += 1;
        let row = found.get(id);
        let state = row.map_or("none".to_owned(), |row| match &row.reconciliation {
            Some(status) => format!("{}+reconciliation_{status}", row.state),
            None => row.state.clone(),
        });
        *result
            .audit_states
            .entry(format!(
                "{} fault={} {:?}{}{} -> {state}",
                record.class.as_str(),
                record.fault,
                record.kind,
                record
                    .code
                    .as_deref()
                    .map_or(String::new(), |code| format!(" {} {code}", record.status)),
                if record.disrupted { " (disrupted)" } else { "" },
            ))
            .or_default() += 1;
        if let Some(row) = row {
            if row.reservations > 1 {
                result.violation(format!("request {id}: {} reservations", row.reservations));
            }
            if row.amount.unwrap_or(0) > row.reserved_minor && row.reconciliation.is_none() {
                result.violation(format!(
                    "request {id}: charged {:?}, more than its reservation {}",
                    row.amount, row.reserved_minor
                ));
            }
        }
        if record.kind == Kind::Ok {
            match row {
                None => result.violation(format!(
                    "answered request {id} ({}) has no reservation",
                    record.class.as_str()
                )),
                Some(row) => {
                    let expected = record.expected_minor;
                    if row.state != "settled" || row.usage_rows != 1 || row.amount != expected {
                        result.violation(format!(
                            "answered request {id} ({} fault={}): {} with {} usage records charging {:?}, expected {:?}",
                            record.class.as_str(),
                            record.fault,
                            row.state,
                            row.usage_rows,
                            row.amount,
                            expected
                        ));
                    }
                }
            }
        } else if let Some(row) = row {
            if !["settled", "released", "expired", "unknown"].contains(&row.state.as_str()) {
                result.violation(format!(
                    "failed request {id} ({:?} {}): reservation left {}",
                    record.kind, record.status, row.state
                ));
            }
            // Every deployment refused it before any work: it used nothing,
            // so nothing may stay held for reconciliation.
            if record.fault == Fault::RateLimitedAll
                && record.status == 429
                && !record.disrupted
                && row.state != "released"
            {
                result.violation(format!(
                    "refused request {id}: reservation {}, not released",
                    row.state
                ));
            }
        }
    }
    Ok(result)
}
