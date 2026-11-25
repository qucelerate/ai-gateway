//! Conditions the worker notices by looking rather than by being told
//! (Phase 10, milestone 4): budgets past their threshold, API keys and
//! agent tokens near expiry, and end users low on credits (D-028). Each run raises a notification for every
//! subject in the condition now, and resolves the open ones whose subject no
//! longer is (a new month, a raised limit, a revoked or extended key, a
//! token that expired). It also resolves `webhook.failing` for
//! endpoints since deleted or disabled; delivery raises and resolves the
//! rest.
//!
//! One kind is checked per transaction, under a transaction-scoped advisory
//! lock, so two workers never interleave a kind's raises and resolutions.

use chrono::{DateTime, Duration, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    domain::{
        identity::OrganizationId,
        notifications::{
            BUDGET_THRESHOLD_PERCENT, CREDITS_LOW_PERCENT, EXPIRY_NOTICE_DAYS, NewNotification,
            NotificationKind,
        },
    },
    persistence::notifications::NotificationStore,
};

const BUDGETS_LINK: &str = "/admin/policies/budgets";
const SERVICE_ACCOUNTS_LINK: &str = "/admin/identities/service-accounts";
const AGENTS_LINK: &str = "/admin/identities/agents";
const RECONCILIATION_LINK: &str = "/admin/usage/reconciliation";

/// What one run did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConditionReport {
    /// Subjects found in a condition (raised, or kept open).
    pub open: u64,
    pub resolved: u64,
}

impl ConditionReport {
    fn add(&mut self, other: Self) {
        self.open += other.open;
        self.resolved += other.resolved;
    }
}

#[derive(Clone)]
pub struct NotificationConditions {
    pool: PgPool,
}

/// A subject in a condition, with the notification that reports it.
struct Found {
    organization_id: Uuid,
    notification: NewNotification,
}

impl NotificationConditions {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Checks every condition once.
    pub async fn check(&self, now: DateTime<Utc>) -> Result<ConditionReport, sqlx::Error> {
        let mut report = ConditionReport::default();
        for kind in [
            NotificationKind::BudgetThreshold,
            NotificationKind::ApiKeyExpiring,
            NotificationKind::AgentTokenExpiring,
            NotificationKind::EndUserCreditsLow,
            NotificationKind::ReconciliationOpen,
        ] {
            report.add(self.check_kind(kind, now).await?);
        }
        report.resolved += self.resolve_gone_webhooks(now).await?;
        Ok(report)
    }

    async fn check_kind(
        &self,
        kind: NotificationKind,
        now: DateTime<Utc>,
    ) -> Result<ConditionReport, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext('notification:' || $1))")
                .bind(kind.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if !locked {
            // Another worker is checking this kind now.
            return Ok(ConditionReport::default());
        }
        let found = match kind {
            NotificationKind::BudgetThreshold => budgets(&mut tx, now).await?,
            NotificationKind::ApiKeyExpiring => api_keys(&mut tx, now).await?,
            NotificationKind::AgentTokenExpiring => agent_tokens(&mut tx, now).await?,
            NotificationKind::EndUserCreditsLow => end_user_credits(&mut tx, now).await?,
            NotificationKind::ReconciliationOpen => reconciliation(&mut tx).await?,
            _ => Vec::new(),
        };
        let mut organizations = Vec::with_capacity(found.len());
        let mut subjects = Vec::with_capacity(found.len());
        for item in found {
            let notification = item.notification.bounded();
            organizations.push(item.organization_id);
            subjects.push(notification.subject.clone());
            NotificationStore::raise_on(
                &mut tx,
                OrganizationId(item.organization_id),
                notification,
                now,
            )
            .await?;
        }
        let resolved = sqlx::query(
            "UPDATE gateway.notification n SET resolved_at = $2, updated_at = $2
             WHERE n.kind = $1 AND n.resolvable AND n.resolved_at IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM unnest($3::uuid[], $4::text[]) AS c (organization_id, subject)
                   WHERE c.organization_id = n.organization_id AND c.subject = n.subject)",
        )
        .bind(kind.as_str())
        .bind(now)
        .bind(&organizations)
        .bind(&subjects)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(ConditionReport {
            open: organizations.len() as u64,
            resolved,
        })
    }

    /// Resolves `webhook.failing` for endpoints deleted or disabled since.
    async fn resolve_gone_webhooks(&self, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gateway.notification n SET resolved_at = $2, updated_at = $2
             WHERE n.kind = $1 AND n.resolvable AND n.resolved_at IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.webhook_endpoint e
                   WHERE e.organization_id = n.organization_id AND e.id::text = n.subject
                     AND e.enabled)",
        )
        .bind(NotificationKind::WebhookFailing.as_str())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Organizations conditions are raised in.
const LIVE_ORGANIZATION: &str = "JOIN gateway.organization o ON o.id = {org} AND o.suspended_at IS NULL AND o.deleted_at IS NULL";

fn live(column: &str) -> String {
    LIVE_ORGANIZATION.replace("{org}", column)
}

/// Budgets whose current window has used the threshold share of their
/// limit, by settled spend (reservations in flight are not counted, so a
/// burst of large reservations does not raise and resolve it).
async fn budgets(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<Vec<Found>, sqlx::Error> {
    type Row = (Uuid, Uuid, String, Option<String>, String, i64, String, i64);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT p.organization_id, p.id, p.scope_kind,
                COALESCE(t.name, sa.name), p.currency, p.limit_minor, p.period,
                b.consumed_minor
         FROM gateway.budget_policy p
         {}
         JOIN LATERAL (
             SELECT b.consumed_minor FROM gateway.quota_bucket b
             WHERE b.organization_id = p.organization_id AND b.scope_kind = p.scope_kind
               AND (p.scope_kind = 'organization'
                    OR (p.scope_kind = 'team' AND b.team_id = p.team_id
                        AND b.service_account_id IS NULL)
                    OR (p.scope_kind = 'service-account'
                        AND b.service_account_id = p.service_account_id))
               AND b.currency = p.currency AND b.retired_at IS NULL
               AND b.period_start <= $1 AND b.period_end > $1
             ORDER BY b.period_start DESC LIMIT 1
         ) b ON true
         LEFT JOIN gateway.team t
           ON t.organization_id = p.organization_id AND t.id = p.team_id
         LEFT JOIN gateway.service_account sa
           ON sa.organization_id = p.organization_id AND sa.id = p.service_account_id
         WHERE p.limit_minor > 0
           AND b.consumed_minor::numeric * 100 >= p.limit_minor::numeric * $2",
        live("p.organization_id")
    ))
    .bind(now)
    .bind(BUDGET_THRESHOLD_PERCENT)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(organization_id, policy_id, scope, name, currency, limit, period, consumed)| {
                let percent = percent_of(consumed, limit);
                let whose = match (scope.as_str(), name.as_deref()) {
                    ("team", Some(name)) => format!("Team {name}'s budget"),
                    ("service-account", Some(name)) => {
                        format!("Service account {name}'s budget")
                    }
                    _ => "The organization's budget".to_string(),
                };
                let window = if period == "monthly" {
                    "this month's"
                } else {
                    "its"
                };
                let (title, body) = if consumed >= limit {
                    (
                        format!("{whose} is used up"),
                        format!(
                            "{whose} has used all of {window} {currency} limit. Requests it \
                             covers are refused until the window ends or the limit is raised."
                        ),
                    )
                } else {
                    (
                        format!("{whose} is at {percent}%"),
                        format!(
                            "{whose} has used {percent}% of {window} {currency} limit. \
                             Requests it covers are refused once it is used up."
                        ),
                    )
                };
                Found {
                    organization_id,
                    notification: NewNotification::new(
                        NotificationKind::BudgetThreshold,
                        policy_id.to_string(),
                        title,
                        body,
                    )
                    .with_link(BUDGETS_LINK)
                    .with_data(serde_json::json!({
                        "budget_id": policy_id,
                        "scope": scope,
                        "currency": currency,
                        "period": period,
                        "limit_minor": limit,
                        "consumed_minor": consumed,
                        "percent": percent,
                        "threshold_percent": BUDGET_THRESHOLD_PERCENT,
                    })),
                }
            },
        )
        .collect())
}

/// End users of enabled service accounts with [`CREDITS_LOW_PERCENT`] or
/// less left of their live credits (granted, not reversed, not expired), by
/// settled spend as budgets are. Users whose grants all expired, or who
/// never had any, are not reported: they are on their plan's allowance.
async fn end_user_credits(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<Vec<Found>, sqlx::Error> {
    type Row = (Uuid, Uuid, String, Uuid, String, String, i64, i64);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT u.organization_id, u.id, u.external_id, sa.id, sa.name, g.currency::TEXT,
                SUM(g.limit_minor)::BIGINT,
                SUM(GREATEST(g.limit_minor - g.consumed_minor, 0))::BIGINT
         FROM gateway.quota_bucket g
         JOIN gateway.end_user u ON u.organization_id = g.organization_id AND u.id = g.end_user_id
         JOIN gateway.service_account sa
           ON sa.organization_id = u.organization_id AND sa.id = u.service_account_id
         {}
         WHERE g.credit_grant_id IS NOT NULL AND g.retired_at IS NULL
           AND g.period_start <= $1 AND g.period_end > $1 AND sa.enabled
         GROUP BY u.organization_id, u.id, u.external_id, sa.id, sa.name, g.currency
         HAVING SUM(g.limit_minor) > 0
            AND SUM(GREATEST(g.limit_minor - g.consumed_minor, 0))::numeric * 100
                <= SUM(g.limit_minor)::numeric * $2",
        live("g.organization_id")
    ))
    .bind(now)
    .bind(CREDITS_LOW_PERCENT)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                organization_id,
                end_user_id,
                end_user,
                account_id,
                account,
                currency,
                granted,
                left,
            )| {
                let percent = percent_of(left, granted);
                let (title, body) = if left == 0 {
                    (
                        format!("End user {end_user} has no credits left"),
                        format!(
                            "{account}'s end user {end_user} has spent all of their {currency} \
                             credits. Their requests are refused once their plan's allowance \
                             for the month is used up, until they are granted more."
                        ),
                    )
                } else {
                    (
                        format!("End user {end_user} has {percent}% of their credits left"),
                        format!(
                            "{account}'s end user {end_user} has {percent}% left of the \
                             {currency} credits granted to them. Their requests are refused \
                             once these and their plan's allowance are used up."
                        ),
                    )
                };
                Found {
                    organization_id,
                    notification: NewNotification::new(
                        NotificationKind::EndUserCreditsLow,
                        end_user_id.to_string(),
                        title,
                        body,
                    )
                    .with_link(SERVICE_ACCOUNTS_LINK)
                    .with_data(serde_json::json!({
                        "service_account_id": account_id,
                        "end_user": end_user,
                        "currency": currency,
                        "granted_minor": granted,
                        "balance_minor": left,
                        "percent_left": percent,
                        "threshold_percent": CREDITS_LOW_PERCENT,
                    })),
                }
            },
        )
        .collect())
}

/// Organizations with reconciliation items open: one notification each,
/// counting them and naming the oldest, until none are left.
async fn reconciliation(conn: &mut PgConnection) -> Result<Vec<Found>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, DateTime<Utc>)> = sqlx::query_as(&format!(
        "SELECT c.organization_id, count(*)::BIGINT, min(c.created_at)
         FROM gateway.accounting_reconciliation c
         {}
         WHERE c.status = 'open'
         GROUP BY c.organization_id",
        live("c.organization_id")
    ))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(organization_id, open, oldest)| {
            let title = if open == 1 {
                "1 request's cost needs the provider's amount".to_owned()
            } else {
                format!("{open} requests' costs need the provider's amount")
            };
            Found {
                organization_id,
                notification: NewNotification::new(
                    NotificationKind::ReconciliationOpen,
                    "reconciliation".to_owned(),
                    title,
                    "The gateway could not settle what these requests cost (the provider \
                     timed out after accepting them, or charged more than was reserved), so \
                     their budgets still hold the reservation. Resolve each with the amount \
                     on the provider's bill."
                        .to_owned(),
                )
                .with_link(RECONCILIATION_LINK)
                .with_data(serde_json::json!({ "open": open, "oldest_at": oldest })),
            }
        })
        .collect())
}

/// Whole percent, rounded down, without overflowing.
fn percent_of(consumed: i64, limit: i64) -> i64 {
    if limit <= 0 {
        return 0;
    }
    i64::try_from(i128::from(consumed) * 100 / i128::from(limit)).unwrap_or(i64::MAX)
}

/// "in 3 days", "in 5 hours", "within the hour".
fn expires_in(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let left = expires_at - now;
    match (left.num_days(), left.num_hours()) {
        (days, _) if days >= 2 => format!("in {days} days"),
        (1, _) => "in 1 day".into(),
        (_, hours) if hours >= 2 => format!("in {hours} hours"),
        (_, 1) => "in 1 hour".into(),
        _ => "within the hour".into(),
    }
}

/// Active API keys of enabled service accounts that expire within the
/// notice period and have not expired yet.
async fn api_keys(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<Vec<Found>, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, String, DateTime<Utc>, Uuid, String)> = sqlx::query_as(&format!(
        "SELECT k.organization_id, k.id, k.public_prefix, k.expires_at, sa.id, sa.name
             FROM gateway.api_key k
             JOIN gateway.service_account sa
               ON sa.organization_id = k.organization_id AND sa.id = k.service_account_id
             {}
             WHERE k.status = 'active' AND sa.enabled
               AND k.expires_at > $1 AND k.expires_at <= $2",
        live("k.organization_id")
    ))
    .bind(now)
    .bind(now + Duration::days(EXPIRY_NOTICE_DAYS))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(organization_id, key_id, prefix, expires_at, account_id, account)| Found {
                organization_id,
                notification: NewNotification::new(
                    NotificationKind::ApiKeyExpiring,
                    key_id.to_string(),
                    format!("API key {prefix}… expires {}", expires_in(expires_at, now)),
                    format!(
                        "Service account {account}'s key {prefix}… expires at {}. Requests with \
                         it are refused from then on. A rotated key keeps this expiry: create \
                         a key with a later one, move clients to it, then revoke this one.",
                        expires_at.format("%Y-%m-%d %H:%M UTC")
                    ),
                )
                .with_link(SERVICE_ACCOUNTS_LINK)
                .with_data(serde_json::json!({
                    "api_key_id": key_id,
                    "public_prefix": prefix,
                    "service_account_id": account_id,
                    "expires_at": expires_at,
                })),
            },
        )
        .collect())
}

/// Agent tokens of enabled agents that expire within the notice period and
/// have not expired or been revoked.
async fn agent_tokens(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<Vec<Found>, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, String, String, DateTime<Utc>, Uuid, String)> =
        sqlx::query_as(&format!(
            "SELECT t.organization_id, t.id, t.name, t.public_prefix, t.expires_at,
                    a.user_id, a.name
             FROM gateway.agent_token t
             JOIN gateway.agent a ON a.user_id = t.agent_user_id
             {}
             WHERE t.revoked_at IS NULL AND a.disabled_at IS NULL
               AND t.expires_at > $1 AND t.expires_at <= $2",
            live("t.organization_id")
        ))
        .bind(now)
        .bind(now + Duration::days(EXPIRY_NOTICE_DAYS))
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(organization_id, token_id, name, prefix, expires_at, agent_id, agent)| Found {
                organization_id,
                notification: NewNotification::new(
                    NotificationKind::AgentTokenExpiring,
                    token_id.to_string(),
                    format!("Agent token {name} expires {}", expires_in(expires_at, now)),
                    format!(
                        "Agent {agent}'s token {name} ({prefix}…) expires at {}. The agent \
                         is refused with it from then on; issue it a new token.",
                        expires_at.format("%Y-%m-%d %H:%M UTC")
                    ),
                )
                .with_link(AGENTS_LINK)
                .with_data(serde_json::json!({
                    "agent_token_id": token_id,
                    "public_prefix": prefix,
                    "agent_id": agent_id,
                    "expires_at": expires_at,
                })),
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_rounds_down_and_never_overflows() {
        assert_eq!(percent_of(79, 100), 79);
        assert_eq!(percent_of(80, 100), 80);
        assert_eq!(percent_of(i64::MAX, 1), i64::MAX);
        assert_eq!(percent_of(5, 0), 0);
    }

    #[test]
    fn expiry_reads_in_days_then_hours() {
        let now = Utc::now();
        assert_eq!(expires_in(now + Duration::days(6), now), "in 6 days");
        assert_eq!(expires_in(now + Duration::hours(30), now), "in 1 day");
        assert_eq!(expires_in(now + Duration::hours(5), now), "in 5 hours");
        assert_eq!(expires_in(now + Duration::minutes(90), now), "in 1 hour");
        assert_eq!(
            expires_in(now + Duration::minutes(20), now),
            "within the hour"
        );
    }
}
