//! Budget policy rows, and the quota buckets that enforce them.
//!
//! Reservations charge quota buckets, never policies. A policy keeps its
//! scope's bucket for the window in force: the first reservation of a month
//! creates that month's bucket from the policy ([`BudgetStore::materialize`]),
//! and setting or removing a policy updates or retires the bucket in the same
//! transaction as the policy change. A retired bucket covers no new request
//! but keeps its counters for settlements already in flight.
//!
//! Lock order: an admin write locks the organization, then its scope's
//! buckets in id order; a reservation takes the organization key-share lock,
//! then its buckets in id order. Bucket creation on both sides goes through
//! the bucket unique index, so a policy change and a reservation creating
//! the same month's bucket serialize there.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    accounting::QuotaScope,
    budget::{BudgetInput, BudgetPeriod, BudgetPolicyView, BudgetScope, BudgetUsage},
    identity::{OrganizationId, UserAccountId},
};
use crate::persistence::rate_limits::RateLimitStore;

#[derive(Clone)]
pub struct BudgetStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct PolicyRow {
    id: Uuid,
    scope_kind: String,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    currency: String,
    limit_minor: i64,
    period: String,
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    consumed_minor: Option<i64>,
    reserved_minor: Option<i64>,
}

impl PolicyRow {
    fn view(self, now: DateTime<Utc>) -> BudgetPolicyView {
        let period = if self.period == "fixed" {
            BudgetPeriod::Fixed
        } else {
            BudgetPeriod::Monthly
        };
        let input = BudgetInput {
            currency: self.currency.clone(),
            limit_minor: self.limit_minor,
            period,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
        };
        // Before the window's first request there is no bucket yet: nothing
        // used.
        let current = input
            .window_at(now)
            .map(|(period_start, period_end)| BudgetUsage {
                period_start,
                period_end,
                consumed_minor: self.consumed_minor.unwrap_or(0),
                reserved_minor: self.reserved_minor.unwrap_or(0),
            });
        BudgetPolicyView {
            id: self.id,
            scope: self.scope_kind,
            team_id: self.team_id,
            service_account_id: self.service_account_id,
            api_key_id: self.api_key_id,
            currency: self.currency,
            limit_minor: self.limit_minor,
            period,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            current,
            version: self
                .version
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(1),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// The buckets of exactly the policy's scope (not the scopes it contains).
const EXACT_SCOPE: &str = "b.organization_id = p.organization_id
    AND b.scope_kind = p.scope_kind
    AND (p.scope_kind = 'organization'
         OR (p.scope_kind = 'team' AND b.team_id = p.team_id AND b.service_account_id IS NULL)
         OR (p.scope_kind = 'service-account' AND b.service_account_id = p.service_account_id)
         OR (p.scope_kind = 'api-key' AND b.api_key_id = p.api_key_id))";

/// The budget bucket unique index, as `ON CONFLICT` must name it: partial,
/// since end users' buckets (D-028) have their own.
const BUCKET_KEY: &str = "(organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(api_key_id, '00000000-0000-0000-0000-000000000000'::uuid),
    period_start, period_end, currency) WHERE end_user_id IS NULL";

/// Policies with their versions and what the current window has used, as
/// of the timestamp bound at placeholder `now`.
fn policy_select(now: &str) -> String {
    format!(
        "SELECT p.id, p.scope_kind, p.team_id, p.service_account_id, p.api_key_id, p.currency,
                p.limit_minor, p.period, p.starts_at, p.ends_at, v.version,
                p.created_at, p.updated_at, b.consumed_minor, b.reserved_minor
         FROM gateway.budget_policy p
         LEFT JOIN gateway.admin_resource_version v
           ON v.organization_id = p.organization_id
          AND v.resource_type = 'budget_policy' AND v.resource_id = p.id
         LEFT JOIN LATERAL (
             SELECT b.consumed_minor, b.reserved_minor FROM gateway.quota_bucket b
             WHERE {EXACT_SCOPE} AND b.currency = p.currency AND b.retired_at IS NULL
               AND b.period_start <= {now} AND b.period_end > {now}
             ORDER BY b.period_start DESC LIMIT 1
         ) b ON true"
    )
}

impl BudgetStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One page of policies ordered by id, after the `after` keyset, with
    /// what their current windows have used. The flag reports whether more
    /// rows follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<Uuid>,
    ) -> Result<(Vec<BudgetPolicyView>, bool), sqlx::Error> {
        let now = Utc::now();
        let rows = sqlx::query_as::<_, PolicyRow>(&format!(
            "{}
             WHERE p.organization_id = $1 AND ($2::uuid IS NULL OR p.id > $2)
             ORDER BY p.id
             LIMIT $3",
            policy_select("$4")
        ))
        .bind(organization_id.0)
        .bind(after)
        .bind(i64::from(page_size) + 1)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok((
            rows.into_iter()
                .take(page_size as usize)
                .map(|row| row.view(now))
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
        now: DateTime<Utc>,
    ) -> Result<Option<BudgetPolicyView>, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(&format!(
            "{} WHERE p.organization_id = $1 AND p.id = $2",
            policy_select("$3")
        ))
        .bind(organization_id.0)
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|row| row.view(now)))
    }

    /// Insert or replace the policy. Returns true when a row was created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        scope: BudgetScope,
        input: &BudgetInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.budget_policy
                 (id, organization_id, scope_kind, team_id, service_account_id, api_key_id,
                  currency, limit_minor, period, starts_at, ends_at, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $12, $6, $7, $8, $9, $10, $11, $11)
             ON CONFLICT (id) DO UPDATE
             SET currency = EXCLUDED.currency,
                 limit_minor = EXCLUDED.limit_minor,
                 period = EXCLUDED.period,
                 starts_at = EXCLUDED.starts_at,
                 ends_at = EXCLUDED.ends_at,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(scope.kind())
        .bind(scope.team_id().map(|team| team.0))
        .bind(scope.service_account_id().map(|account| account.0))
        .bind(&input.currency)
        .bind(input.limit_minor)
        .bind(input.period.as_str())
        .bind(input.starts_at)
        .bind(input.ends_at)
        .bind(actor.0)
        .bind(scope.api_key_id().map(|key| key.0))
        .fetch_one(&mut *conn)
        .await
    }

    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM gateway.budget_policy WHERE organization_id = $1 AND id = $2")
                .bind(organization_id.0)
                .bind(id)
                .execute(&mut *conn)
                .await?
                .rows_affected()
                == 1,
        )
    }

    /// Make the scope's buckets match a policy just set: retire any current
    /// or future bucket of another window or currency, and give the window in
    /// force (if any) the new limit, reactivating it if it was retired, so
    /// what the window already spent still counts.
    pub async fn apply(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        scope: BudgetScope,
        input: &BudgetInput,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let window = input.window_at(now);
        let buckets = lock_scope_buckets(conn, organization_id, scope, now).await?;
        for (id, start, end, currency) in buckets {
            if window != Some((start, end)) || currency != input.currency {
                retire(conn, id, now).await?;
            }
        }
        if let Some((start, end)) = window {
            upsert_bucket(
                conn,
                organization_id,
                scope,
                &input.currency,
                input.limit_minor,
                (start, end),
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// Retire the scope's current and future buckets before its policy is
    /// removed. The window the policy has in force is written retired even if
    /// no request has made it yet, so a reservation that read the policy
    /// before this commits cannot create it afterwards.
    pub async fn retire_scope(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        scope: BudgetScope,
        stored: &BudgetPolicyView,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        for (id, ..) in lock_scope_buckets(conn, organization_id, scope, now).await? {
            retire(conn, id, now).await?;
        }
        if let Some(current) = &stored.current {
            upsert_bucket(
                conn,
                organization_id,
                scope,
                &stored.currency,
                stored.limit_minor,
                (current.period_start, current.period_end),
                Some(now),
            )
            .await?;
        }
        Ok(())
    }

    /// Create the buckets that policies covering `scope` have in force at
    /// `now` and that no reservation has created yet. Run by a reservation
    /// before it locks its buckets; an existing bucket, active or retired, is
    /// left as it is.
    pub(crate) async fn materialize(
        conn: &mut PgConnection,
        scope: &QuotaScope,
        currency: &str,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(&format!(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, team_id, service_account_id, api_key_id, scope_kind,
                  period_start, period_end, currency, limit_minor)
             SELECT gen_random_uuid(), p.organization_id,
                    CASE WHEN p.scope_kind = 'service-account' THEN sa.team_id ELSE p.team_id END,
                    p.service_account_id, p.api_key_id, p.scope_kind, w.period_start, w.period_end,
                    p.currency, p.limit_minor
             FROM gateway.budget_policy p
             LEFT JOIN gateway.service_account sa
               ON sa.organization_id = p.organization_id AND sa.id = p.service_account_id
             CROSS JOIN LATERAL (
                 SELECT CASE WHEN p.period = 'monthly'
                             THEN date_trunc('month', $5 AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                             ELSE p.starts_at END AS period_start,
                        CASE WHEN p.period = 'monthly'
                             THEN (date_trunc('month', $5 AT TIME ZONE 'UTC') + interval '1 month')
                                  AT TIME ZONE 'UTC'
                             ELSE p.ends_at END AS period_end
             ) w
             WHERE p.organization_id = $1 AND p.currency = $4
               AND (p.scope_kind = 'organization'
                    OR (p.scope_kind = 'team' AND p.team_id = $2)
                    OR (p.scope_kind = 'service-account' AND p.service_account_id = $3)
                    OR (p.scope_kind = 'api-key' AND p.api_key_id = $6))
               AND w.period_start <= $5 AND w.period_end > $5
             ORDER BY p.id
             ON CONFLICT {BUCKET_KEY} DO NOTHING"
        ))
        .bind(scope.organization_id.0)
        .bind(scope.team_id.map(|team| team.0))
        .bind(scope.service_account_id.map(|account| account.0))
        .bind(currency)
        .bind(now)
        .bind(scope.api_key_id.map(|key| key.0))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Whether the team, service account or API key a policy names belongs
    /// to the organization, share-locked so it cannot go before the policy
    /// referencing it commits. A revoked key still exists: its budget can be
    /// read, changed and removed.
    pub async fn scope_exists(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        scope: BudgetScope,
    ) -> Result<bool, sqlx::Error> {
        RateLimitStore::scope_exists(conn, organization_id, scope.rate_limit_scope()).await
    }
}

/// The scope's active buckets that are current or future, locked in id
/// order: id, window, currency.
async fn lock_scope_buckets(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    scope: BudgetScope,
    now: DateTime<Utc>,
) -> Result<Vec<(Uuid, DateTime<Utc>, DateTime<Utc>, String)>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, DateTime<Utc>, DateTime<Utc>, String)>(
        "SELECT id, period_start, period_end, currency FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = $2
           AND ($2 = 'organization'
                OR ($2 = 'team' AND team_id = $3 AND service_account_id IS NULL)
                OR ($2 = 'service-account' AND service_account_id = $4)
                OR ($2 = 'api-key' AND api_key_id = $6))
           AND retired_at IS NULL AND period_end > $5
         ORDER BY id FOR UPDATE",
    )
    .bind(organization_id.0)
    .bind(scope.kind())
    .bind(scope.team_id().map(|team| team.0))
    .bind(scope.service_account_id().map(|account| account.0))
    .bind(now)
    .bind(scope.api_key_id().map(|key| key.0))
    .fetch_all(&mut *conn)
    .await
}

async fn retire(conn: &mut PgConnection, id: Uuid, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE gateway.quota_bucket SET retired_at = $2 WHERE id = $1")
        .bind(id)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Create or update the scope's bucket for one window: its limit, and
/// whether it is retired.
async fn upsert_bucket(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    scope: BudgetScope,
    currency: &str,
    limit_minor: i64,
    (start, end): (DateTime<Utc>, DateTime<Utc>),
    retired_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    // A service account's bucket carries its team, as reservations match it.
    let team_id = match scope {
        BudgetScope::ServiceAccount(account) => sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT team_id FROM gateway.service_account WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(account.0)
        .fetch_optional(&mut *conn)
        .await?
        .flatten(),
        _ => scope.team_id().map(|team| team.0),
    };
    sqlx::query(&format!(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, team_id, service_account_id, scope_kind,
              period_start, period_end, currency, limit_minor, retired_at, api_key_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT {BUCKET_KEY} DO UPDATE
         SET limit_minor = EXCLUDED.limit_minor, retired_at = EXCLUDED.retired_at"
    ))
    .bind(Uuid::new_v4())
    .bind(organization_id.0)
    .bind(team_id)
    .bind(scope.service_account_id().map(|account| account.0))
    .bind(scope.kind())
    .bind(start)
    .bind(end)
    .bind(currency)
    .bind(limit_minor)
    .bind(retired_at)
    .bind(scope.api_key_id().map(|key| key.0))
    .execute(&mut *conn)
    .await?;
    Ok(())
}
