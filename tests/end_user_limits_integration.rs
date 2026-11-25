#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Plans of an app's end users (D-028): per-user request and token rate
//! limits, allowance periods other than the month, and spend over the
//! plan's period.

use std::{
    env,
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use chrono::{DateTime, Duration, TimeZone, Utc};
use gateway::{
    AccountedChatRequest, AccountingPolicy, AccountingService, AccountingServiceError,
    AccountingStore, AccountingStoreError, ApiKeyPepper, BudgetAmount, Currency, IdentityStore,
    MembershipRole, PriceSnapshot, QuotaScope, VerifiedIdentity,
    domain::end_user::{EndUserRateLimitKind, EndUserRateRefusal, PlanPeriod},
    domain::inference::{
        ChatMessage, ChatRequest, ChatResponse, ChatRole, CorrelationId, DeploymentModelReference,
        EmbeddingRequest, EmbeddingResponse, InferenceContext, ProviderAttemptIdempotencyKey,
        ProviderError, ProviderErrorCategory, SafeMetadata, TokenCounter, Usage, UsageSnapshot,
    },
    persistence::end_users::EndUserStore,
    provider::{ChatEventStream, InferenceProviderAdapter},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn pool() -> Option<PgPool> {
    let url = env::var("GATEWAY_TEST_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(&url)
        .await
        .unwrap_or_else(|error| {
            panic!("GATEWAY_TEST_DATABASE_URL is configured but unreachable: {error}")
        });
    gateway::persistence::migrations::run(&pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
    Some(pool)
}

/// Uses 1 input and 1 output token: each request reserves 5 (1 input
/// counted, 4 output) at a price of 1 per token and settles at 2.
struct TwoTokenProvider;

#[async_trait::async_trait]
impl InferenceProviderAdapter for TwoTokenProvider {
    async fn count_chat_input_tokens(
        &self,
        _: &InferenceContext,
        _: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        Ok(1)
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        // Hold the reservation a moment, so concurrent requests overlap.
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(1),
            None,
            None,
            None,
        )
        .unwrap();
        Ok(ChatResponse::new(
            None,
            "model".into(),
            vec![],
            Some(UsageSnapshot::final_(usage).unwrap()),
            SafeMetadata::default(),
        )
        .unwrap())
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

struct App {
    pool: PgPool,
    service: AccountingService,
    identity: VerifiedIdentity,
    organization: Uuid,
    service_account: Uuid,
}

impl App {
    /// The same key, for `end_user`.
    fn as_user(&self, end_user: Option<&str>) -> VerifiedIdentity {
        VerifiedIdentity {
            end_user: end_user.map(str::to_owned),
            ..self.identity.clone()
        }
    }

    async fn chat(&self, end_user: Option<&str>) -> Result<(), AccountingServiceError> {
        self.service
            .chat(
                &self.as_user(end_user),
                context(),
                request(),
                AccountedChatRequest::new(Uuid::new_v4().to_string()),
            )
            .await
            .map(|_| ())
    }

    async fn settings(&self, requirement: &str) {
        sqlx::query(
            "INSERT INTO gateway.end_user_setting
                 (service_account_id, organization_id, id, requirement, currency)
             VALUES ($1, $2, $3, $4, 'CRD')
             ON CONFLICT (service_account_id) DO UPDATE SET requirement = EXCLUDED.requirement",
        )
        .bind(self.service_account)
        .bind(self.organization)
        .bind(Uuid::new_v4())
        .bind(requirement)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn default_plan(&self, default_plan: Option<Uuid>) {
        sqlx::query(
            "UPDATE gateway.end_user_setting SET default_plan_id = $2 WHERE service_account_id = $1",
        )
        .bind(self.service_account)
        .bind(default_plan)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// A plan with `period` (and its day count) and per-user limits.
    async fn plan(
        &self,
        name: &str,
        allowance: i64,
        period: (&str, Option<i32>),
        requests_per_minute: Option<i32>,
        tokens_per_minute: Option<i32>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.end_user_plan
                 (id, organization_id, service_account_id, name, allowance_minor, period,
                  period_days, requests_per_minute, tokens_per_minute)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(self.organization)
        .bind(self.service_account)
        .bind(name)
        .bind(allowance)
        .bind(period.0)
        .bind(period.1)
        .bind(requests_per_minute)
        .bind(tokens_per_minute)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    async fn user(&self, external_id: &str, plan: Option<Uuid>) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id, plan_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (service_account_id, external_id) DO UPDATE SET plan_id = EXCLUDED.plan_id
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(self.organization)
        .bind(self.service_account)
        .bind(external_id)
        .bind(plan)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Each allowance bucket of the user, oldest first: `(period_start,
    /// period_end, consumed)`.
    async fn allowances(&self, end_user: Uuid) -> Vec<(DateTime<Utc>, DateTime<Utc>, i64)> {
        sqlx::query_as(
            "SELECT period_start, period_end, consumed_minor FROM gateway.quota_bucket
             WHERE end_user_id = $1 AND credit_grant_id IS NULL ORDER BY created_at, id",
        )
        .bind(end_user)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// The user's reservations in order: `(output_token_cap, token_bound)`.
    async fn reservations(&self, end_user: Uuid) -> Vec<(Option<i64>, Option<i64>)> {
        sqlx::query_as(
            "SELECT output_token_cap, token_bound FROM gateway.accounting_reservation
             WHERE end_user_id = $1 ORDER BY created_at, id",
        )
        .bind(end_user)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn service_account_consumed(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT consumed_minor FROM gateway.quota_bucket
             WHERE organization_id = $1 AND scope_kind = 'service-account'",
        )
        .bind(self.organization)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

async fn app(budget: i64) -> Option<App> {
    let pool = pool().await?;
    let identities = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"end-user-test-pepper").unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identities
        .create_organization(&format!("end-users-{suffix}"), "End users test")
        .await
        .unwrap();
    let user = identities
        .create_user(org.id, None, None, None, Some("test"))
        .await
        .unwrap();
    identities
        .add_organization_membership(org.id, user.id, MembershipRole::Owner)
        .await
        .unwrap();
    let account = identities
        .create_service_account(org.id, user.id, None, "app")
        .await
        .unwrap();
    let key = identities
        .create_api_key(
            org.id,
            account.id,
            &[gateway::domain::identity::ApiKeyScope::Inference],
            None,
            user.id,
        )
        .await
        .unwrap();
    let store = AccountingStore::new(pool.clone());
    let now = Utc::now();
    store
        .create_quota_bucket(
            &QuotaScope {
                organization_id: org.id,
                team_id: None,
                service_account_id: Some(account.id),
                api_key_id: None,
            },
            BudgetAmount::new(budget).unwrap(),
            &Currency::new("CRD").unwrap(),
            now - Duration::hours(1),
            now + Duration::hours(1),
        )
        .await
        .unwrap();
    let policy = AccountingPolicy::new(
        1024,
        std::num::NonZeroU32::new(4).unwrap(),
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 1, 0).unwrap(),
        Currency::new("CRD").unwrap(),
        [7; 32],
    )
    .unwrap()
    .with_chat_input_count_mode(gateway::domain::catalog::ChatInputCountMode::ProviderApiExact);
    Some(App {
        pool,
        service: AccountingService::new(store, std::sync::Arc::new(TwoTokenProvider), policy),
        identity: VerifiedIdentity {
            organization_id: org.id,
            service_account_id: account.id,
            team_id: None,
            api_key_id: Some(key.id),
            scopes: vec![],
            end_user: None,
        },
        organization: org.id.0,
        service_account: account.id.0,
    })
}

fn context() -> InferenceContext {
    InferenceContext::new(
        CorrelationId::new(),
        ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
        DeploymentModelReference::new("deployment", "model").unwrap(),
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        temperature: None,
        max_tokens: Some(4),
        ..Default::default()
    }
}

fn rate_refusal(error: AccountingServiceError) -> EndUserRateRefusal {
    match error {
        AccountingServiceError::Store(AccountingStoreError::EndUserRateLimited(refusal)) => refusal,
        other => panic!("expected an end-user rate refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_plans_requests_per_minute_limit_each_of_its_users_alone() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    let pro = app
        .plan("pro", 1_000, ("monthly", None), Some(2), None)
        .await;
    app.user("alice", Some(pro)).await;
    // Bob is on the plan as the default.
    app.default_plan(Some(pro)).await;
    app.chat(Some("alice")).await.unwrap();
    app.chat(Some("alice")).await.unwrap();
    let refused = rate_refusal(app.chat(Some("alice")).await.unwrap_err());
    assert_eq!(refused.end_user, "alice");
    assert_eq!(refused.limit, EndUserRateLimitKind::RequestsPerMinute);
    assert!((1..=60).contains(&refused.retry_after_seconds));
    assert!(!refused.never_fits);
    // Another user of the plan, and a request naming no user, still fit.
    app.chat(Some("bob")).await.unwrap();
    app.chat(None).await.unwrap();
    // Nothing was reserved for the refused request.
    assert_eq!(app.service_account_consumed().await, 8);
    // Bob's second request takes his last one; its replay is not a new
    // admission, so it finds the request already served rather than a limit.
    let key = Uuid::new_v4().to_string();
    let bob = app.as_user(Some("bob"));
    let send = || {
        app.service.chat(
            &bob,
            context(),
            request(),
            AccountedChatRequest::new(key.clone()),
        )
    };
    send().await.unwrap();
    assert!(matches!(
        send().await,
        Err(AccountingServiceError::Store(
            AccountingStoreError::NotClaimable
        ))
    ));
    rate_refusal(app.chat(Some("bob")).await.unwrap_err());
}

#[tokio::test]
async fn a_plans_tokens_per_minute_lowers_the_output_cap_and_then_refuses() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    // Each request holds 1 input and up to 4 output tokens and reports 2.
    let metered = app
        .plan("metered", 1_000, ("monthly", None), None, Some(12))
        .await;
    let carol = app.user("carol", Some(metered)).await;
    for _ in 0..6 {
        app.chat(Some("carol")).await.unwrap();
    }
    // Settled requests count what they reported (2 each); the last two fit
    // only with a smaller output cap.
    let caps: Vec<_> = app.reservations(carol).await;
    assert_eq!(
        caps,
        [
            (Some(4), Some(5)),
            (Some(4), Some(5)),
            (Some(4), Some(5)),
            (Some(4), Some(5)),
            (Some(3), Some(4)),
            (Some(1), Some(2)),
        ]
    );
    let refused = rate_refusal(app.chat(Some("carol")).await.unwrap_err());
    assert_eq!(refused.limit, EndUserRateLimitKind::TokensPerMinute);
    assert!((1..=60).contains(&refused.retry_after_seconds));
    assert!(!refused.never_fits);
    // A request whose input alone is over the limit never fits.
    let tiny = app
        .plan("tiny", 1_000, ("monthly", None), None, Some(1))
        .await;
    app.user("dave", Some(tiny)).await;
    let refused = rate_refusal(app.chat(Some("dave")).await.unwrap_err());
    assert!(refused.never_fits);
    assert_eq!(refused.retry_after_seconds, 60);
}

#[tokio::test]
async fn concurrent_requests_of_one_user_never_pass_their_plan_limits() {
    let _guard = database_guard();
    let Some(app) = app(1_000_000).await else {
        return;
    };
    app.settings("optional").await;
    let seven = app
        .plan("seven", 1_000_000, ("monthly", None), Some(7), None)
        .await;
    app.user("erin", Some(seven)).await;
    let results = futures::future::join_all((0..50).map(|_| app.chat(Some("erin")))).await;
    let served = results.iter().filter(|result| result.is_ok()).count();
    for result in results {
        if let Err(error) = result {
            assert_eq!(
                rate_refusal(error).limit,
                EndUserRateLimitKind::RequestsPerMinute
            );
        }
    }
    assert_eq!(served, 7);

    // Each served request counts at least the 2 tokens it reported, so no
    // more than 10 fit 20 tokens a minute, however they overlap.
    let twenty = app
        .plan("twenty", 1_000_000, ("monthly", None), None, Some(20))
        .await;
    let frank = app.user("frank", Some(twenty)).await;
    let results = futures::future::join_all((0..50).map(|_| app.chat(Some("frank")))).await;
    let served = results.iter().filter(|result| result.is_ok()).count();
    for result in results {
        if let Err(error) = result {
            assert_eq!(
                rate_refusal(error).limit,
                EndUserRateLimitKind::TokensPerMinute
            );
        }
    }
    assert!((1..=10).contains(&served), "served {served}");
    let held: i64 = app
        .reservations(frank)
        .await
        .iter()
        .map(|(_, bound)| bound.unwrap())
        .max()
        .unwrap();
    assert!(held <= 20);
}

#[tokio::test]
async fn a_plans_max_concurrent_limits_each_users_requests_in_flight() {
    let _guard = database_guard();
    let Some(app) = app(1_000_000).await else {
        return;
    };
    app.settings("optional").await;
    let two = app
        .plan("two", 1_000_000, ("monthly", None), None, None)
        .await;
    sqlx::query("UPDATE gateway.end_user_plan SET max_concurrent = 2 WHERE id = $1")
        .bind(two)
        .execute(&app.pool)
        .await
        .unwrap();
    let hana = app.user("hana", Some(two)).await;
    app.user("ivan", Some(two)).await;
    // Each request stays in flight 20 ms; admission takes the user's row
    // in turn, so most of twenty at once find two already in flight.
    let results = futures::future::join_all((0..20).map(|_| app.chat(Some("hana")))).await;
    let served = results.iter().filter(|result| result.is_ok()).count();
    let mut refused = 0;
    for result in results {
        if let Err(error) = result {
            let refusal = rate_refusal(error);
            assert_eq!(refusal.limit, EndUserRateLimitKind::Concurrency);
            assert_eq!(refusal.retry_after_seconds, 1);
            assert!(!refusal.never_fits);
            refused += 1;
        }
    }
    assert!(
        served >= 2 && refused > 0,
        "served {served}, refused {refused}"
    );
    assert_eq!(app.reservations(hana).await.len(), served);
    // Another user on the plan has slots of their own, and finished
    // requests free theirs.
    app.chat(Some("ivan")).await.unwrap();
    app.chat(Some("hana")).await.unwrap();
}

#[tokio::test]
async fn an_allowance_can_repeat_daily_and_spend_is_reported_over_the_plans_period() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    let daily = app.plan("daily", 6, ("daily", None), None, None).await;
    let gina = app.user("gina", Some(daily)).await;
    app.chat(Some("gina")).await.unwrap();
    let now = Utc::now();
    let (day_start, day_end) = PlanPeriod::Daily.window(now);
    assert_eq!(app.allowances(gina).await, [(day_start, day_end, 2)]);
    let store = EndUserStore::new(app.pool.clone());
    let (users, _) = store
        .end_users(app.organization, app.service_account, 10, None, now)
        .await
        .unwrap();
    assert_eq!(
        (users[0].period_start, users[0].period_end),
        (day_start, day_end)
    );
    assert_eq!(users[0].allowance_spent_minor, 2);
    assert_eq!(users[0].spent_minor, Some(2));
    // Moving the plan to the month starts the month's allowance afresh; the
    // month's spend, read from the ledger, still counts both requests.
    sqlx::query("UPDATE gateway.end_user_plan SET period = 'monthly' WHERE id = $1")
        .bind(daily)
        .execute(&app.pool)
        .await
        .unwrap();
    app.chat(Some("gina")).await.unwrap();
    let (month_start, month_end) = PlanPeriod::Monthly.window(now);
    assert_eq!(
        app.allowances(gina).await,
        [(day_start, day_end, 2), (month_start, month_end, 2)]
    );
    let (users, _) = store
        .end_users(app.organization, app.service_account, 10, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        (users[0].period_start, users[0].period_end),
        (month_start, month_end)
    );
    assert_eq!(users[0].allowance_spent_minor, 2);
    assert_eq!(users[0].spent_minor, Some(4));
}

#[tokio::test]
async fn the_database_and_the_gateway_agree_on_every_periods_window() {
    let _guard = database_guard();
    let Some(pool) = pool().await else {
        return;
    };
    let instants = [
        Utc.with_ymd_and_hms(2026, 11, 17, 13, 5, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap(),
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2028, 2, 29, 12, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 3, 29, 1, 30, 0).unwrap(),
    ];
    let periods = [
        PlanPeriod::Monthly,
        PlanPeriod::Weekly,
        PlanPeriod::Daily,
        PlanPeriod::Days(1),
        PlanPeriod::Days(7),
        PlanPeriod::Days(30),
        PlanPeriod::Days(366),
    ];
    // In a session whose zone has daylight saving, which must not move them.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET TIME ZONE 'Europe/London'")
        .execute(&mut *conn)
        .await
        .unwrap();
    for at in instants {
        for period in periods {
            let window: (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
                "SELECT gateway.end_user_period_start($1, $2, $3),
                        gateway.end_user_period_end($1, $2, $3)",
            )
            .bind(period.as_str())
            .bind(period.days().map(|days| days as i32))
            .bind(at)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
            assert_eq!(window, period.window(at), "{period:?} at {at}");
        }
    }
    sqlx::query("RESET TIME ZONE")
        .execute(&mut *conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failover_holds_no_more_than_the_users_window_has_left_now() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    let metered = app
        .plan("metered", 1_000, ("monthly", None), None, Some(12))
        .await;
    let frank = app.user("frank", Some(metered)).await;
    // A slow first request, admitted long enough ago to have left the
    // window...
    app.chat(Some("frank")).await.unwrap();
    let first: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.accounting_reservation WHERE end_user_id = $1")
            .bind(frank)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE gateway.accounting_reservation
         SET created_at = created_at - INTERVAL '2 minutes' WHERE id = $1",
    )
    .bind(first)
    .execute(&app.pool)
    .await
    .unwrap();
    // ...while others filled it: five settled requests of 2 tokens each.
    for _ in 0..5 {
        app.chat(Some("frank")).await.unwrap();
    }
    let mut conn = app.pool.acquire().await.unwrap();
    // Failing over the first request's 5 tokens gets only the 2 left.
    let room =
        gateway::persistence::end_users::failover_token_room(&mut conn, frank, first, 5).await;
    assert_eq!(room.unwrap(), Some(2));
    // A user without a tokens-per-minute limit is not limited.
    let free = app.plan("free", 1_000, ("monthly", None), None, None).await;
    let gina = app.user("gina", Some(free)).await;
    let room =
        gateway::persistence::end_users::failover_token_room(&mut conn, gina, first, 5).await;
    assert_eq!(room.unwrap(), None);
}
