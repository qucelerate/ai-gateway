//! End users of an app, their plans and credits (D-028), as reservations
//! see them, and as the admin lane writes them.
//!
//! An end user's allowance for one period of their plan and each credit
//! grant are quota buckets of scope `end-user`. A reservation admits the end
//! user ([`admit`]): it finds or creates the user, works out their plan,
//! creates this period's allowance bucket from it, and returns the buckets to
//! draw from, allowance first and then credits soonest-expiring first, and
//! the plan's rate limits, which [`check_rate`] applies while the user's row
//! is locked. The reservation then locks the buckets together with the
//! budgets', in id order, as settlement locks them.
//!
//! Lock order: the end user's row, then buckets in id order. An admin write
//! to an end user (a plan change, a grant, a reversal) locks the same way.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::domain::{
    accounting::QuotaScope,
    end_user::{
        self, CreditGrantInput, CreditGrantView, CreditReversalInput, CreditReversalView,
        END_USER_RATE_WINDOW_SECONDS, EndUserRateLimitKind, EndUserRateRefusal, EndUserRefusal,
        EndUserRequirement, EndUserSettingsInput, EndUserSettingsView, EndUserView, PlanPeriod,
        PlanRateLimits, PlanSpec, PlanView, WindowEntry, never_expires, token_admission,
    },
};
use crate::persistence::AccountingStoreError;

/// What a reservation charges for its end user.
#[derive(Clone, Debug)]
pub(crate) struct EndUserAdmission {
    pub end_user_id: Uuid,
    pub external_id: String,
    /// `None` where the service account sets no plans: the user is recorded,
    /// not limited.
    pub draw: Option<EndUserDraw>,
    /// The rate limits of the plan in force; none without one.
    pub limits: PlanRateLimits,
}

/// The buckets a request draws from, in draw order.
#[derive(Clone, Debug)]
pub(crate) struct EndUserDraw {
    pub allowance: Uuid,
    /// The period's allowance under the plan in force now. The bucket keeps
    /// the limit it was created with until a reservation brings it in line
    /// (under the bucket's lock), so a plan change applies from the next
    /// request and keeps the period's spend.
    pub allowance_limit: i64,
    pub credits: Vec<Uuid>,
    pub currency: String,
}

impl EndUserDraw {
    pub fn bucket_ids(&self) -> Vec<Uuid> {
        std::iter::once(self.allowance)
            .chain(self.credits.iter().copied())
            .collect()
    }
}

#[derive(FromRow)]
struct SettingRow {
    requirement: String,
    currency: String,
    default_plan_id: Option<Uuid>,
}

/// Find or create the end user named by a request, and what it draws from.
///
/// - No setting on the service account: the user, if named, is recorded.
/// - A setting, no user named: refused when the setting requires one.
/// - A user named: their plan, or the default plan, or refused when there
///   is no default.
/// - A request priced in another currency than the setting's is refused for
///   that user, never converted.
pub(crate) async fn admit(
    conn: &mut PgConnection,
    scope: &QuotaScope,
    end_user: Option<&str>,
    currency: &str,
    now: DateTime<Utc>,
) -> Result<Option<EndUserAdmission>, AccountingStoreError> {
    let Some(service_account_id) = scope.service_account_id.map(|id| id.0) else {
        return Ok(None);
    };
    let setting = sqlx::query_as::<_, SettingRow>(
        "SELECT requirement, currency, default_plan_id FROM gateway.end_user_setting
         WHERE service_account_id = $1",
    )
    .bind(service_account_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(external_id) = end_user else {
        return match setting {
            Some(setting)
                if EndUserRequirement::parse(&setting.requirement)
                    == Some(EndUserRequirement::Required) =>
            {
                Err(AccountingStoreError::EndUserRequired)
            }
            _ => Ok(None),
        };
    };
    let (end_user_id, plan_id, _) = find_or_create(
        conn,
        scope.organization_id.0,
        service_account_id,
        external_id,
    )
    .await?;
    let Some(setting) = setting else {
        return Ok(Some(EndUserAdmission {
            end_user_id,
            external_id: external_id.to_owned(),
            draw: None,
            limits: PlanRateLimits::default(),
        }));
    };
    let Some(plan_id) = plan_id.or(setting.default_plan_id) else {
        return Err(AccountingStoreError::EndUserNotRegistered);
    };
    let plan = sqlx::query_as::<_, PlanRow>(
        "SELECT allowance_minor, period, period_days, requests_per_minute, tokens_per_minute,
                max_concurrent
         FROM gateway.end_user_plan WHERE id = $1",
    )
    .bind(plan_id)
    .fetch_one(&mut *conn)
    .await?;
    let period = PlanPeriod::from_row(&plan.period, plan.period_days);
    if setting.currency != currency {
        let (allowance_remaining, credit_balance) =
            balances(conn, end_user_id, &setting.currency, period, now).await?;
        return Err(AccountingStoreError::EndUserQuotaExceeded(EndUserRefusal {
            end_user: external_id.to_owned(),
            allowance_remaining,
            credit_balance,
            currency: setting.currency,
        }));
    }
    let allowance_limit = plan.allowance_minor;
    let allowance = materialize_allowance(
        conn,
        scope.organization_id.0,
        service_account_id,
        end_user_id,
        plan_id,
        &setting.currency,
        period,
        now,
    )
    .await?;
    let credits = sqlx::query_scalar::<_, Uuid>(
        "SELECT b.id FROM gateway.quota_bucket b
         JOIN gateway.end_user_credit_grant g ON g.id = b.credit_grant_id
         WHERE b.end_user_id = $1 AND b.currency = $2 AND b.retired_at IS NULL
           AND b.period_start <= $3 AND b.period_end > $3
           AND b.limit_minor > b.consumed_minor + b.reserved_minor
         ORDER BY g.expires_at ASC NULLS LAST, g.granted_at, g.id",
    )
    .bind(end_user_id)
    .bind(&setting.currency)
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(EndUserAdmission {
        end_user_id,
        external_id: external_id.to_owned(),
        draw: Some(EndUserDraw {
            allowance,
            allowance_limit,
            credits,
            currency: setting.currency,
        }),
        limits: plan.limits(),
    }))
}

#[derive(FromRow)]
struct PlanRow {
    allowance_minor: i64,
    period: String,
    period_days: Option<i32>,
    requests_per_minute: Option<i32>,
    tokens_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
}

impl PlanRow {
    fn limits(&self) -> PlanRateLimits {
        PlanRateLimits {
            requests_per_minute: self.requests_per_minute.and_then(|v| u32::try_from(v).ok()),
            tokens_per_minute: self.tokens_per_minute.and_then(|v| u32::try_from(v).ok()),
            max_concurrent: self.max_concurrent.and_then(|v| u32::try_from(v).ok()),
        }
    }
}

/// What a new admission asks of its end user's plan limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RateDemand {
    /// A chat or embeddings request: one request, and the fewest tokens it
    /// can be served with (its input bound and reasoning allowance, and one
    /// output token where it reserves output).
    Request { min_tokens: u64 },
    /// A batch: one request, refused while the user's tokens for the minute
    /// are used up; its lines are not counted.
    Batch,
}

/// Apply the plan's rate limits to a new admission of `admission`'s user,
/// whose row the caller's transaction holds locked (so one user's
/// admissions are counted one after another). Counts the user's
/// reservations in flight and in the last minute on the database clock,
/// which stamps their `created_at`. Returns the tokens left for this request where the
/// plan limits tokens.
pub(crate) async fn check_rate(
    conn: &mut PgConnection,
    admission: &EndUserAdmission,
    demand: RateDemand,
) -> Result<Option<u64>, AccountingStoreError> {
    let limits = admission.limits;
    let refuse = |limit, retry_after: Option<u32>| {
        AccountingStoreError::EndUserRateLimited(EndUserRateRefusal {
            end_user: admission.external_id.clone(),
            limit,
            retry_after_seconds: retry_after.unwrap_or(END_USER_RATE_WINDOW_SECONDS as u32),
            never_fits: retry_after.is_none(),
        })
    };
    // The database's clock, read once: a bound fixed for the statements
    // below lets the `(end_user_id, created_at)` index limit them to the
    // window (`clock_timestamp()` in a filter would scan the user's history).
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    // The user's requests in flight, as a policy's `max_concurrent` counts
    // them: single requests reserved or claimed, a batch's lines not. Read
    // up to the limit only, with the user's row locked.
    if let Some(limit) = limits.max_concurrent {
        let in_flight: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (
                 SELECT 1 FROM gateway.accounting_reservation
                 WHERE end_user_id = $1 AND batch_id IS NULL
                   AND state IN ('reserved', 'claimed')
                 LIMIT $2) AS flight",
        )
        .bind(admission.end_user_id)
        .bind(i64::from(limit))
        .fetch_one(&mut *conn)
        .await?;
        if in_flight >= i64::from(limit) {
            return Err(refuse(EndUserRateLimitKind::Concurrency, Some(1)));
        }
    }
    if let Some(limit) = limits.requests_per_minute {
        // The `limit`-th newest admission in the window: while it exists the
        // user is at their limit, and it leaves the window first among those
        // that matter. A batch is one admission (its lines' earliest); a
        // failover attempt is the request it continues.
        let wait = sqlx::query_scalar::<_, f64>(
            "SELECT EXTRACT(EPOCH FROM admitted_at + make_interval(secs => $2)
                                    - $4::timestamptz)::float8
             FROM (SELECT created_at AS admitted_at FROM gateway.accounting_reservation
                   WHERE end_user_id = $1 AND batch_id IS NULL AND failover_of IS NULL
                     AND created_at > $4::timestamptz - make_interval(secs => $2)
                   UNION ALL
                   SELECT min(created_at) FROM gateway.accounting_reservation
                   WHERE end_user_id = $1 AND batch_id IS NOT NULL
                     AND created_at > $4::timestamptz - make_interval(secs => $2)
                   GROUP BY batch_id) AS admitted
             ORDER BY admitted_at DESC
             OFFSET $3 LIMIT 1",
        )
        .bind(admission.end_user_id)
        .bind(END_USER_RATE_WINDOW_SECONDS as f64)
        .bind(i64::from(limit) - 1)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(wait) = wait {
            return Err(refuse(
                EndUserRateLimitKind::RequestsPerMinute,
                Some(crate::domain::end_user::rate_retry_after(wait)),
            ));
        }
    }
    let Some(limit) = limits.tokens_per_minute else {
        return Ok(None);
    };
    let needed = match demand {
        RateDemand::Request { min_tokens } => min_tokens,
        RateDemand::Batch => 1,
    };
    let limit = u64::from(limit);
    // What the window holds: the sum on every request, each request (oldest
    // first) only to work out the wait of one that does not fit.
    let used = window_tokens(conn, admission.end_user_id, now, None).await?;
    let room = limit.saturating_sub(used);
    if needed <= room {
        return Ok(Some(room));
    }
    let entries = sqlx::query_as::<_, (f64, i64)>(&format!(
        "SELECT leaves_in_seconds, tokens FROM ({TOKEN_WINDOW}) AS w
         ORDER BY created_at, id"
    ))
    .bind(admission.end_user_id)
    .bind(END_USER_RATE_WINDOW_SECONDS as f64)
    .bind(now)
    .bind(None::<Uuid>)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(leaves_in_seconds, tokens)| WindowEntry {
        leaves_in_seconds,
        tokens: u64::try_from(tokens).unwrap_or(0),
    })
    .collect::<Vec<_>>();
    // Refused on the sum; the entries, read a moment later, only say when.
    let retry_after = match token_admission(limit, needed, &entries) {
        Ok(_) => Some(1),
        Err(retry_after) => retry_after,
    };
    Err(refuse(EndUserRateLimitKind::TokensPerMinute, retry_after))
}

/// An end user's (`$1`) requests in the token window of `$2` seconds before
/// `$3` (the database's clock, read once), leaving out the reservation `$4`
/// if any: each
/// counts its upper bound while in flight or unresolved, and what its usage
/// reported once settled or released (a released attempt that reported
/// nothing used nothing). Batch lines are not counted.
const TOKEN_WINDOW: &str = "SELECT r.created_at, r.id,
        EXTRACT(EPOCH FROM r.created_at + make_interval(secs => $2)
                           - $3::timestamptz)::float8 AS leaves_in_seconds,
        (CASE
             WHEN r.state IN ('settled', 'released') AND u.reported THEN u.tokens
             WHEN r.state = 'released' THEN 0
             ELSE COALESCE(r.token_bound, 0)
         END)::BIGINT AS tokens
     FROM gateway.accounting_reservation r
     LEFT JOIN LATERAL (
         SELECT (a.input_tokens IS NOT NULL OR a.output_tokens IS NOT NULL) AS reported,
                COALESCE(a.input_tokens, 0) + COALESCE(a.output_tokens, 0)
                    + COALESCE(a.reasoning_tokens, 0) + COALESCE(a.other_tokens, 0) AS tokens
         FROM gateway.accounting_usage a
         WHERE a.organization_id = r.organization_id AND a.reservation_id = r.id
         ORDER BY a.recorded_at DESC, a.id DESC
         LIMIT 1
     ) u ON r.state IN ('settled', 'released')
     WHERE r.end_user_id = $1 AND r.batch_id IS NULL
       AND r.created_at > $3::timestamptz - make_interval(secs => $2)
       AND r.id IS DISTINCT FROM $4::uuid";

/// The tokens an end user's window holds at `now`, leaving out `except`.
async fn window_tokens(
    conn: &mut PgConnection,
    end_user_id: Uuid,
    now: DateTime<Utc>,
    except: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    let used: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(SUM(tokens), 0)::BIGINT FROM ({TOKEN_WINDOW}) AS w"
    ))
    .bind(end_user_id)
    .bind(END_USER_RATE_WINDOW_SECONDS as f64)
    .bind(now)
    .bind(except)
    .fetch_one(&mut *conn)
    .await?;
    Ok(u64::try_from(used).unwrap_or(0))
}

/// The tokens a failover attempt of `failed` (which held `bound`) may hold
/// under its end user's plan: no more than the failed attempt, nor than the
/// user's window has left now without it (the window may have moved on and
/// filled since the failed attempt was admitted). `None` when the plan sets
/// no tokens per minute. Locks the user's row, as new admissions do, so it
/// is counted in turn with them.
pub async fn failover_token_room(
    conn: &mut PgConnection,
    end_user_id: Uuid,
    failed: Uuid,
    bound: u64,
) -> Result<Option<u64>, sqlx::Error> {
    let limit: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT p.tokens_per_minute
         FROM gateway.end_user u
         JOIN gateway.end_user_setting s ON s.service_account_id = u.service_account_id
         JOIN gateway.end_user_plan p ON p.id = COALESCE(u.plan_id, s.default_plan_id)
         WHERE u.id = $1
         FOR UPDATE OF u",
    )
    .bind(end_user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(limit) = limit.flatten().and_then(|v| u64::try_from(v).ok()) else {
        return Ok(None);
    };
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    let used = window_tokens(conn, end_user_id, now, Some(failed)).await?;
    Ok(Some(bound.min(limit.saturating_sub(used))))
}

/// The end user's id and assigned plan, and whether this call created
/// them, creating the user on first sight; their row stays locked.
pub(crate) async fn find_or_create(
    conn: &mut PgConnection,
    organization_id: Uuid,
    service_account_id: Uuid,
    external_id: &str,
) -> Result<(Uuid, Option<Uuid>, bool), sqlx::Error> {
    let created = sqlx::query(
        "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (service_account_id, external_id) DO NOTHING",
    )
    .bind(end_user::end_user_id(service_account_id, external_id))
    .bind(organization_id)
    .bind(service_account_id)
    .bind(external_id)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    let (id, plan_id) = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT id, plan_id FROM gateway.end_user
         WHERE service_account_id = $1 AND external_id = $2 FOR UPDATE",
    )
    .bind(service_account_id)
    .bind(external_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok((id, plan_id, created))
}

/// This period's allowance bucket of the user, created from their plan by
/// the first request that needs it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn materialize_allowance(
    conn: &mut PgConnection,
    organization_id: Uuid,
    service_account_id: Uuid,
    end_user_id: Uuid,
    plan_id: Uuid,
    currency: &str,
    period: PlanPeriod,
    now: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let (start, end) = period.window(now);
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, team_id, service_account_id, scope_kind, end_user_id,
              period_start, period_end, currency, limit_minor)
         SELECT $1, $2, NULL, $3, 'end-user', $4, $5, $6, $7, p.allowance_minor
         FROM gateway.end_user_plan p WHERE p.id = $8
         ON CONFLICT (end_user_id, period_start, period_end, currency)
             WHERE end_user_id IS NOT NULL AND credit_grant_id IS NULL
         DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id)
    .bind(service_account_id)
    .bind(end_user_id)
    .bind(start)
    .bind(end)
    .bind(currency)
    .bind(plan_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query_scalar(
        "SELECT id FROM gateway.quota_bucket
         WHERE end_user_id = $1 AND credit_grant_id IS NULL
           AND period_start = $2 AND period_end = $3 AND currency = $4",
    )
    .bind(end_user_id)
    .bind(start)
    .bind(end)
    .bind(currency)
    .fetch_one(&mut *conn)
    .await
}

/// This period's allowance left (never negative) and the credit balance
/// (what no request has spent or holds), in `currency`.
pub(crate) async fn balances(
    conn: &mut PgConnection,
    end_user_id: Uuid,
    currency: &str,
    period: PlanPeriod,
    now: DateTime<Utc>,
) -> Result<(i64, i64), sqlx::Error> {
    let (start, end) = period.window(now);
    let (allowance, credits): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT
             SUM(GREATEST(limit_minor - consumed_minor - reserved_minor, 0))
                 FILTER (WHERE credit_grant_id IS NULL
                           AND period_start = $4 AND period_end = $5)::BIGINT,
             SUM(GREATEST(limit_minor - consumed_minor - reserved_minor, 0))
                 FILTER (WHERE credit_grant_id IS NOT NULL)::BIGINT
         FROM gateway.quota_bucket
         WHERE end_user_id = $1 AND currency = $2 AND retired_at IS NULL
           AND period_start <= $3 AND period_end > $3",
    )
    .bind(end_user_id)
    .bind(currency)
    .bind(now)
    .bind(start)
    .bind(end)
    .fetch_one(&mut *conn)
    .await?;
    Ok((allowance.unwrap_or(0), credits.unwrap_or(0)))
}

/// Why an admin write is refused once it reads the database.
#[derive(Debug)]
pub(crate) enum EndUserWriteError {
    /// The service account, user or grant is not in the organization.
    NotFound,
    /// The service account has no end-user settings yet.
    SettingsMissing,
    UnknownPlan,
    /// A new grant's expiry is not in the future.
    Expired,
    CurrencyLocked,
    PlanInUse,
    CreditReferenceConflict,
    ReversalReferenceConflict,
    Store(sqlx::Error),
}

impl From<sqlx::Error> for EndUserWriteError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error)
    }
}

/// A write that may find its result already made by an earlier request.
#[derive(Debug)]
pub(crate) enum Written<T> {
    Created(T),
    Updated(T),
    Existing(T),
}

#[derive(Clone)]
pub struct EndUserStore {
    pool: sqlx::PgPool,
}

#[derive(FromRow)]
struct SettingViewRow {
    service_account_id: Uuid,
    requirement: String,
    currency: String,
    default_plan: Option<String>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct PlanViewRow {
    name: String,
    allowance_minor: i64,
    period: String,
    period_days: Option<i32>,
    requests_per_minute: Option<i32>,
    tokens_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
    currency: String,
    users: i64,
    spent_minor: i64,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct EndUserViewRow {
    external_id: String,
    plan: Option<String>,
    effective_plan: Option<String>,
    currency: Option<String>,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
    allowance_minor: i64,
    allowance_spent_minor: i64,
    allowance_reserved_minor: i64,
    credit_balance_minor: i64,
    spent_minor: Option<i64>,
    spent_currency: Option<String>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct GrantViewRow {
    id: Uuid,
    end_user: String,
    reference: String,
    amount_minor: i64,
    currency: String,
    reversed_minor: i64,
    limit_minor: i64,
    consumed_minor: i64,
    reserved_minor: i64,
    period_end: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    granted_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct ReversalViewRow {
    id: Uuid,
    grant_id: Uuid,
    reference: String,
    requested_minor: i64,
    reversed_minor: i64,
    reason: String,
    created_at: DateTime<Utc>,
}

fn version(value: Option<i64>) -> u64 {
    value.and_then(|v| u64::try_from(v).ok()).unwrap_or(0)
}

impl SettingViewRow {
    fn view(self) -> EndUserSettingsView {
        EndUserSettingsView {
            service_account_id: self.service_account_id,
            requirement: EndUserRequirement::parse(&self.requirement).unwrap_or_default(),
            currency: self.currency,
            default_plan: self.default_plan,
            version: version(self.version),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

impl PlanViewRow {
    fn view(self) -> PlanView {
        let period = PlanPeriod::from_row(&self.period, self.period_days);
        let limit = |value: Option<i32>| value.and_then(|v| u32::try_from(v).ok());
        PlanView {
            name: self.name,
            allowance_minor: self.allowance_minor,
            period: period.kind(),
            period_days: period.days(),
            requests_per_minute: limit(self.requests_per_minute),
            tokens_per_minute: limit(self.tokens_per_minute),
            max_concurrent: limit(self.max_concurrent),
            currency: self.currency,
            users: u64::try_from(self.users).unwrap_or(0),
            spent_minor: self.spent_minor,
            period_start: self.period_start,
            period_end: self.period_end,
            version: version(self.version),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

impl EndUserViewRow {
    fn view(self) -> EndUserView {
        let remaining = self
            .allowance_minor
            .saturating_sub(self.allowance_spent_minor)
            .saturating_sub(self.allowance_reserved_minor)
            .max(0);
        EndUserView {
            id: self.external_id,
            plan: self.plan,
            effective_plan: self.effective_plan,
            currency: self.currency,
            period_start: self.period_start,
            period_end: self.period_end,
            allowance_minor: self.allowance_minor,
            allowance_spent_minor: self.allowance_spent_minor,
            allowance_reserved_minor: self.allowance_reserved_minor,
            allowance_remaining_minor: remaining,
            credit_balance_minor: self.credit_balance_minor,
            spent_minor: self.spent_minor,
            spent_currency: self.spent_currency,
            version: version(self.version),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

impl GrantViewRow {
    fn view(self, now: DateTime<Utc>) -> CreditGrantView {
        let expired = self.period_end <= now;
        let remaining = if expired {
            0
        } else {
            self.limit_minor
                .saturating_sub(self.consumed_minor)
                .saturating_sub(self.reserved_minor)
                .max(0)
        };
        CreditGrantView {
            id: self.id,
            end_user: self.end_user,
            reference: self.reference,
            amount_minor: self.amount_minor,
            currency: self.currency,
            reversed_minor: self.reversed_minor,
            spent_minor: self.consumed_minor,
            reserved_minor: self.reserved_minor,
            remaining_minor: remaining,
            expires_at: self.expires_at,
            expired,
            granted_at: self.granted_at,
        }
    }
}

impl ReversalViewRow {
    fn view(self) -> CreditReversalView {
        CreditReversalView {
            id: self.id,
            grant_id: self.grant_id,
            reference: self.reference,
            requested_minor: self.requested_minor,
            reversed_minor: self.reversed_minor,
            reason: self.reason,
            created_at: self.created_at,
        }
    }
}

const SETTING_SELECT: &str = "SELECT s.service_account_id, s.requirement, s.currency,
        p.name AS default_plan, v.version, s.created_at, s.updated_at
     FROM gateway.end_user_setting s
     LEFT JOIN gateway.end_user_plan p ON p.id = s.default_plan_id
     LEFT JOIN gateway.admin_resource_version v
         ON v.organization_id = s.organization_id
         AND v.resource_type = 'end_user_setting' AND v.resource_id = s.id
     WHERE s.organization_id = $1 AND s.service_account_id = $2";

/// `$1` organization, `$2` service account, `$3` now. With each plan, its
/// totals: the users it is in force for and what their requests settled in
/// its current period were charged (from the ledger, as each user's spend).
const PLAN_SELECT: &str = "SELECT p.name, p.allowance_minor, p.period, p.period_days,
        p.requests_per_minute, p.tokens_per_minute, p.max_concurrent,
        s.currency::TEXT AS currency,
        totals.users, totals.spent_minor, w.period_start, w.period_end,
        v.version, p.created_at, p.updated_at
     FROM gateway.end_user_plan p
     JOIN gateway.end_user_setting s ON s.service_account_id = p.service_account_id
     CROSS JOIN LATERAL (
         SELECT gateway.end_user_period_start(p.period, p.period_days, $3) AS period_start,
                gateway.end_user_period_end(p.period, p.period_days, $3) AS period_end
     ) w
     CROSS JOIN LATERAL (
         SELECT count(*) AS users,
                COALESCE(SUM(spend.amount_minor), 0)::BIGINT AS spent_minor
         FROM gateway.end_user u
         LEFT JOIN LATERAL (
             SELECT SUM(lu.amount_minor) AS amount_minor
             FROM gateway.accounting_reservation r
             CROSS JOIN LATERAL (
                 SELECT a.amount_minor, btrim(a.currency) AS currency
                 FROM gateway.accounting_usage a
                 WHERE a.organization_id = r.organization_id AND a.reservation_id = r.id
                 ORDER BY a.recorded_at DESC, a.id DESC
                 LIMIT 1
             ) lu
             WHERE r.end_user_id = u.id AND r.state = 'settled'
               AND r.settled_at >= w.period_start AND r.settled_at < w.period_end
               AND lu.currency = btrim(s.currency::TEXT)
         ) spend ON true
         WHERE u.service_account_id = p.service_account_id
           AND COALESCE(u.plan_id, s.default_plan_id) = p.id
     ) totals
     LEFT JOIN gateway.admin_resource_version v
         ON v.organization_id = p.organization_id
         AND v.resource_type = 'end_user_plan' AND v.resource_id = p.id
     WHERE p.organization_id = $1 AND p.service_account_id = $2 AND p.deleted_at IS NULL";

/// `$1` organization, `$2` service account, `$3` now. The period is the
/// plan in force's; without one, the UTC calendar month.
const END_USER_SELECT: &str = "SELECT u.external_id, ap.name AS plan, ep.name AS effective_plan,
        s.currency::TEXT AS currency, w.period_start, w.period_end,
        COALESCE(ep.allowance_minor, 0) AS allowance_minor,
        COALESCE(b.consumed_minor, 0) AS allowance_spent_minor,
        COALESCE(b.reserved_minor, 0) AS allowance_reserved_minor,
        COALESCE(c.balance, 0) AS credit_balance_minor,
        spend.spent_minor, spend.spent_currency,
        v.version, u.created_at, u.updated_at
     FROM gateway.end_user u
     LEFT JOIN gateway.end_user_setting s ON s.service_account_id = u.service_account_id
     LEFT JOIN gateway.end_user_plan ap ON ap.id = u.plan_id
     LEFT JOIN gateway.end_user_plan ep ON ep.id = COALESCE(u.plan_id, s.default_plan_id)
     CROSS JOIN LATERAL (
         SELECT gateway.end_user_period_start(COALESCE(ep.period, 'monthly'), ep.period_days, $3)
                    AS period_start,
                gateway.end_user_period_end(COALESCE(ep.period, 'monthly'), ep.period_days, $3)
                    AS period_end
     ) w
     LEFT JOIN gateway.quota_bucket b
         ON b.end_user_id = u.id AND b.credit_grant_id IS NULL AND b.currency = s.currency
         AND b.period_start = w.period_start AND b.period_end = w.period_end
     LEFT JOIN LATERAL (
         SELECT SUM(GREATEST(g.limit_minor - g.consumed_minor - g.reserved_minor, 0))::BIGINT
             AS balance
         FROM gateway.quota_bucket g
         WHERE g.end_user_id = u.id AND g.credit_grant_id IS NOT NULL
           AND g.currency = s.currency AND g.retired_at IS NULL
           AND g.period_start <= $3 AND g.period_end > $3
     ) c ON true
     LEFT JOIN LATERAL (
         -- What its requests settled this period were charged, from allowance
         -- or credits alike, read from the ledger (traces may be deleted
         -- sooner): each settled reservation's latest usage, in the app's
         -- currency; without end-user settings, in the one currency its
         -- requests used (none when they used several). A request awaiting
         -- reconciliation is not counted until it is settled.
         SELECT CASE WHEN count(DISTINCT lu.currency) <= 1
                     THEN COALESCE(SUM(lu.amount_minor), 0)::BIGINT END AS spent_minor,
                COALESCE(btrim(s.currency::TEXT), min(lu.currency)) AS spent_currency
         FROM gateway.accounting_reservation r
         CROSS JOIN LATERAL (
             SELECT a.amount_minor, btrim(a.currency) AS currency
             FROM gateway.accounting_usage a
             WHERE a.organization_id = r.organization_id AND a.reservation_id = r.id
             ORDER BY a.recorded_at DESC, a.id DESC
             LIMIT 1
         ) lu
         WHERE r.end_user_id = u.id AND r.state = 'settled'
           AND r.settled_at >= w.period_start AND r.settled_at < w.period_end
           AND lu.currency = COALESCE(btrim(s.currency::TEXT), lu.currency)
     ) spend ON true
     LEFT JOIN gateway.admin_resource_version v
         ON v.organization_id = u.organization_id
         AND v.resource_type = 'end_user' AND v.resource_id = u.id
     WHERE u.organization_id = $1 AND u.service_account_id = $2";

/// `$1` organization, `$2` service account.
const GRANT_SELECT: &str = "SELECT g.id, u.external_id AS end_user, g.reference, g.amount_minor,
        g.currency::TEXT AS currency,
        COALESCE((SELECT SUM(r.reversed_minor) FROM gateway.end_user_credit_reversal r
                  WHERE r.grant_id = g.id), 0)::BIGINT AS reversed_minor,
        b.limit_minor, b.consumed_minor, b.reserved_minor, b.period_end,
        g.expires_at, g.granted_at
     FROM gateway.end_user_credit_grant g
     JOIN gateway.end_user u ON u.id = g.end_user_id
     JOIN gateway.quota_bucket b ON b.id = g.quota_bucket_id
     WHERE g.organization_id = $1 AND g.service_account_id = $2";

const REVERSAL_SELECT: &str = "SELECT id, grant_id, reference, requested_minor, reversed_minor,
        reason, created_at
     FROM gateway.end_user_credit_reversal";

impl EndUserStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn connection(
        &self,
    ) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>, sqlx::Error> {
        self.pool.acquire().await
    }

    pub(crate) async fn service_account_exists(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.service_account
                            WHERE organization_id = $1 AND id = $2)",
        )
        .bind(organization_id)
        .bind(service_account_id)
        .fetch_one(&mut *conn)
        .await
    }

    pub(crate) async fn settings(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
    ) -> Result<Option<EndUserSettingsView>, sqlx::Error> {
        Ok(sqlx::query_as::<_, SettingViewRow>(SETTING_SELECT)
            .bind(organization_id)
            .bind(service_account_id)
            .fetch_optional(&mut *conn)
            .await?
            .map(SettingViewRow::view))
    }

    pub(crate) async fn plans(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Vec<PlanView>, sqlx::Error> {
        Ok(
            sqlx::query_as::<_, PlanViewRow>(&format!("{PLAN_SELECT} ORDER BY p.name"))
                .bind(organization_id)
                .bind(service_account_id)
                .bind(now)
                .fetch_all(&mut *conn)
                .await?
                .into_iter()
                .map(PlanViewRow::view)
                .collect(),
        )
    }

    pub(crate) async fn plan(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PlanView>, sqlx::Error> {
        Ok(
            sqlx::query_as::<_, PlanViewRow>(&format!("{PLAN_SELECT} AND p.name = $4"))
                .bind(organization_id)
                .bind(service_account_id)
                .bind(now)
                .bind(name)
                .fetch_optional(&mut *conn)
                .await?
                .map(PlanViewRow::view),
        )
    }

    pub(crate) async fn end_user(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        external_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<EndUserView>, sqlx::Error> {
        Ok(sqlx::query_as::<_, EndUserViewRow>(&format!(
            "{END_USER_SELECT} AND u.external_id = $4"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(now)
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?
        .map(EndUserViewRow::view))
    }

    /// A page of end users by ID, after `after`; and whether more follow.
    pub async fn end_users(
        &self,
        organization_id: Uuid,
        service_account_id: Uuid,
        page_size: u32,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(Vec<EndUserView>, bool), sqlx::Error> {
        let mut rows = sqlx::query_as::<_, EndUserViewRow>(&format!(
            "{END_USER_SELECT} AND ($4::TEXT IS NULL OR u.external_id COLLATE \"C\" > $4)
             ORDER BY u.external_id COLLATE \"C\" LIMIT $5"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(now)
        .bind(after)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        rows.truncate(page_size as usize);
        Ok((
            rows.into_iter().map(EndUserViewRow::view).collect(),
            has_more,
        ))
    }

    pub(crate) async fn grants(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        external_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<CreditGrantView>, sqlx::Error> {
        Ok(sqlx::query_as::<_, GrantViewRow>(&format!(
            "{GRANT_SELECT} AND u.external_id = $3 ORDER BY g.granted_at, g.id"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(external_id)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| row.view(now))
        .collect())
    }

    async fn grant_view(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        grant_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<CreditGrantView>, sqlx::Error> {
        Ok(
            sqlx::query_as::<_, GrantViewRow>(&format!("{GRANT_SELECT} AND g.id = $3"))
                .bind(organization_id)
                .bind(service_account_id)
                .bind(grant_id)
                .fetch_optional(&mut *conn)
                .await?
                .map(|row| row.view(now)),
        )
    }

    async fn plan_id_by_name(
        conn: &mut PgConnection,
        service_account_id: Uuid,
        name: &str,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT id FROM gateway.end_user_plan
             WHERE service_account_id = $1 AND name = $2 AND deleted_at IS NULL",
        )
        .bind(service_account_id)
        .bind(name)
        .fetch_optional(&mut *conn)
        .await
    }

    async fn setting_currency(
        conn: &mut PgConnection,
        service_account_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT currency::TEXT FROM gateway.end_user_setting WHERE service_account_id = $1",
        )
        .bind(service_account_id)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Create or replace a service account's settings. The currency cannot
    /// change once a plan or a grant exists.
    pub(crate) async fn put_settings(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        input: &EndUserSettingsInput,
    ) -> Result<Written<EndUserSettingsView>, EndUserWriteError> {
        if !Self::service_account_exists(conn, organization_id, service_account_id).await? {
            return Err(EndUserWriteError::NotFound);
        }
        let default_plan = match input.default_plan.as_deref() {
            Some(name) => Some(
                Self::plan_id_by_name(conn, service_account_id, name)
                    .await?
                    .ok_or(EndUserWriteError::UnknownPlan)?,
            ),
            None => None,
        };
        let current = Self::setting_currency(conn, service_account_id).await?;
        if current.as_deref().is_some_and(|c| c != input.currency) {
            let in_use: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM gateway.end_user_plan WHERE service_account_id = $1)
                     OR EXISTS (SELECT 1 FROM gateway.end_user_credit_grant
                                WHERE service_account_id = $1)",
            )
            .bind(service_account_id)
            .fetch_one(&mut *conn)
            .await?;
            if in_use {
                return Err(EndUserWriteError::CurrencyLocked);
            }
        }
        sqlx::query(
            "INSERT INTO gateway.end_user_setting
                 (service_account_id, organization_id, id, requirement, currency, default_plan_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (service_account_id) DO UPDATE SET
                 requirement = EXCLUDED.requirement, currency = EXCLUDED.currency,
                 default_plan_id = EXCLUDED.default_plan_id, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(service_account_id)
        .bind(organization_id)
        .bind(end_user::setting_id(service_account_id))
        .bind(input.requirement.as_str())
        .bind(&input.currency)
        .bind(default_plan)
        .execute(&mut *conn)
        .await?;
        let view = Self::settings(conn, organization_id, service_account_id)
            .await?
            .ok_or(EndUserWriteError::NotFound)?;
        Ok(if current.is_some() {
            Written::Updated(view)
        } else {
            Written::Created(view)
        })
    }

    /// Create or replace a plan; a removed plan of the same name comes back.
    pub(crate) async fn put_plan(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        name: &str,
        plan: &PlanSpec,
        now: DateTime<Utc>,
    ) -> Result<Written<PlanView>, EndUserWriteError> {
        if !Self::service_account_exists(conn, organization_id, service_account_id).await? {
            return Err(EndUserWriteError::NotFound);
        }
        if Self::setting_currency(conn, service_account_id)
            .await?
            .is_none()
        {
            return Err(EndUserWriteError::SettingsMissing);
        }
        let existed = Self::plan_id_by_name(conn, service_account_id, name)
            .await?
            .is_some();
        let limit = |value: Option<u32>| value.map(|v| i32::try_from(v).unwrap_or(i32::MAX));
        sqlx::query(
            "INSERT INTO gateway.end_user_plan
                 (id, organization_id, service_account_id, name, allowance_minor, period,
                  period_days, requests_per_minute, tokens_per_minute, max_concurrent)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET
                 allowance_minor = EXCLUDED.allowance_minor, period = EXCLUDED.period,
                 period_days = EXCLUDED.period_days,
                 requests_per_minute = EXCLUDED.requests_per_minute,
                 tokens_per_minute = EXCLUDED.tokens_per_minute,
                 max_concurrent = EXCLUDED.max_concurrent, deleted_at = NULL,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(end_user::plan_id(service_account_id, name))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(name)
        .bind(plan.allowance_minor)
        .bind(plan.period.as_str())
        .bind(limit(plan.period.days()))
        .bind(limit(plan.limits.requests_per_minute))
        .bind(limit(plan.limits.tokens_per_minute))
        .bind(limit(plan.limits.max_concurrent))
        .execute(&mut *conn)
        .await?;
        let view = Self::plan(conn, organization_id, service_account_id, name, now)
            .await?
            .ok_or(EndUserWriteError::NotFound)?;
        Ok(if existed {
            Written::Updated(view)
        } else {
            Written::Created(view)
        })
    }

    /// Hide a plan no user is on and that is not the default.
    pub(crate) async fn delete_plan(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        name: &str,
    ) -> Result<(), EndUserWriteError> {
        let id = Self::plan_id_by_name(conn, service_account_id, name)
            .await?
            .ok_or(EndUserWriteError::NotFound)?;
        let in_use: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.end_user_setting WHERE default_plan_id = $1)
                 OR EXISTS (SELECT 1 FROM gateway.end_user WHERE plan_id = $1)",
        )
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        if in_use {
            return Err(EndUserWriteError::PlanInUse);
        }
        sqlx::query(
            "UPDATE gateway.end_user_plan SET deleted_at = CURRENT_TIMESTAMP,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND organization_id = $2",
        )
        .bind(id)
        .bind(organization_id)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Put an end user on a plan (`None`: the default), creating them if
    /// new. Takes effect from their next request.
    pub(crate) async fn put_end_user(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        external_id: &str,
        plan: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Written<EndUserView>, EndUserWriteError> {
        if !Self::service_account_exists(conn, organization_id, service_account_id).await? {
            return Err(EndUserWriteError::NotFound);
        }
        let plan_id = match plan {
            Some(name) => {
                if Self::setting_currency(conn, service_account_id)
                    .await?
                    .is_none()
                {
                    return Err(EndUserWriteError::SettingsMissing);
                }
                Some(
                    Self::plan_id_by_name(conn, service_account_id, name)
                        .await?
                        .ok_or(EndUserWriteError::UnknownPlan)?,
                )
            }
            None => None,
        };
        let (id, _, created) =
            find_or_create(conn, organization_id, service_account_id, external_id).await?;
        sqlx::query(
            "UPDATE gateway.end_user SET plan_id = $1, updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(plan_id)
        .bind(id)
        .execute(&mut *conn)
        .await?;
        let view = Self::end_user(conn, organization_id, service_account_id, external_id, now)
            .await?
            .ok_or(EndUserWriteError::NotFound)?;
        Ok(if created {
            Written::Created(view)
        } else {
            Written::Updated(view)
        })
    }

    /// Grant credits, once per reference: the same reference with the same
    /// user, amount and expiry is the grant already made.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn grant(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        external_id: &str,
        input: &CreditGrantInput,
        actor: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Written<CreditGrantView>, EndUserWriteError> {
        if !Self::service_account_exists(conn, organization_id, service_account_id).await? {
            return Err(EndUserWriteError::NotFound);
        }
        let currency = Self::setting_currency(conn, service_account_id)
            .await?
            .ok_or(EndUserWriteError::SettingsMissing)?;
        let (end_user_id, _, _) =
            find_or_create(conn, organization_id, service_account_id, external_id).await?;
        let existing = sqlx::query_as::<_, (Uuid, Uuid, i64, Option<DateTime<Utc>>)>(
            "SELECT id, end_user_id, amount_minor, expires_at FROM gateway.end_user_credit_grant
             WHERE service_account_id = $1 AND reference = $2",
        )
        .bind(service_account_id)
        .bind(&input.reference)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((id, user, amount, expires_at)) = existing {
            if user != end_user_id || amount != input.amount_minor || expires_at != input.expires_at
            {
                return Err(EndUserWriteError::CreditReferenceConflict);
            }
            let view = Self::grant_view(conn, organization_id, service_account_id, id, now)
                .await?
                .ok_or(EndUserWriteError::NotFound)?;
            return Ok(Written::Existing(view));
        }
        if input.expired(now) {
            return Err(EndUserWriteError::Expired);
        }
        let id = end_user::grant_id(service_account_id, &input.reference);
        let bucket = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, team_id, service_account_id, scope_kind, end_user_id,
                  credit_grant_id, period_start, period_end, currency, limit_minor)
             VALUES ($1, $2, NULL, $3, 'end-user', $4, $5, $6, $7, $8, $9)",
        )
        .bind(bucket)
        .bind(organization_id)
        .bind(service_account_id)
        .bind(end_user_id)
        .bind(id)
        .bind(now)
        .bind(input.expires_at.unwrap_or_else(never_expires))
        .bind(&currency)
        .bind(input.amount_minor)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO gateway.end_user_credit_grant
                 (id, organization_id, service_account_id, end_user_id, reference, amount_minor,
                  currency, expires_at, quota_bucket_id, granted_at, granted_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(id)
        .bind(organization_id)
        .bind(service_account_id)
        .bind(end_user_id)
        .bind(&input.reference)
        .bind(input.amount_minor)
        .bind(&currency)
        .bind(input.expires_at)
        .bind(bucket)
        .bind(now)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        let view = Self::grant_view(conn, organization_id, service_account_id, id, now)
            .await?
            .ok_or(EndUserWriteError::NotFound)?;
        Ok(Written::Created(view))
    }

    /// Take back up to what is unspent and unreserved of a grant, once per
    /// reference.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn reverse(
        conn: &mut PgConnection,
        organization_id: Uuid,
        service_account_id: Uuid,
        external_id: &str,
        grant_id: Uuid,
        input: &CreditReversalInput,
        actor: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Written<CreditReversalView>, EndUserWriteError> {
        // The user's row, then the grant's bucket: the order reservations
        // lock them in.
        let end_user_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM gateway.end_user
             WHERE organization_id = $1 AND service_account_id = $2 AND external_id = $3
             FOR UPDATE",
        )
        .bind(organization_id)
        .bind(service_account_id)
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(EndUserWriteError::NotFound)?;
        let bucket = sqlx::query_as::<_, (Uuid, i64, i64, i64, DateTime<Utc>)>(
            "SELECT b.id, b.limit_minor, b.consumed_minor, b.reserved_minor, b.period_end
             FROM gateway.end_user_credit_grant g
             JOIN gateway.quota_bucket b ON b.id = g.quota_bucket_id
             WHERE g.id = $1 AND g.organization_id = $2 AND g.end_user_id = $3
             FOR UPDATE OF b",
        )
        .bind(grant_id)
        .bind(organization_id)
        .bind(end_user_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(EndUserWriteError::NotFound)?;
        let existing = sqlx::query_as::<_, ReversalViewRow>(&format!(
            "{REVERSAL_SELECT} WHERE grant_id = $1 AND reference = $2"
        ))
        .bind(grant_id)
        .bind(&input.reference)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(row) = existing {
            if row.requested_minor != input.amount_minor || row.reason != input.reason {
                return Err(EndUserWriteError::ReversalReferenceConflict);
            }
            return Ok(Written::Existing(row.view()));
        }
        let (bucket_id, limit, consumed, reserved, period_end) = bucket;
        let unspent = if period_end <= now {
            0
        } else {
            limit
                .saturating_sub(consumed)
                .saturating_sub(reserved)
                .max(0)
        };
        let taken = unspent.min(input.amount_minor);
        sqlx::query("UPDATE gateway.quota_bucket SET limit_minor = limit_minor - $1 WHERE id = $2")
            .bind(taken)
            .bind(bucket_id)
            .execute(&mut *conn)
            .await?;
        let row = sqlx::query_as::<_, ReversalViewRow>(
            "INSERT INTO gateway.end_user_credit_reversal
                 (id, organization_id, grant_id, reference, requested_minor, reversed_minor,
                  reason, created_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING id, grant_id, reference, requested_minor, reversed_minor, reason,
                       created_at",
        )
        .bind(end_user::reversal_id(grant_id, &input.reference))
        .bind(organization_id)
        .bind(grant_id)
        .bind(&input.reference)
        .bind(input.amount_minor)
        .bind(taken)
        .bind(&input.reason)
        .bind(now)
        .bind(actor)
        .fetch_one(&mut *conn)
        .await?;
        Ok(Written::Created(row.view()))
    }
}
