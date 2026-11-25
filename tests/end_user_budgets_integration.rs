#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Budgets per end user of an app (D-028): the allowance and credits a
//! request draws from, alongside the service account's budget.

use std::{
    env,
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use chrono::{Duration, Utc};
use gateway::{
    AccountedChatRequest, AccountingPolicy, AccountingService, AccountingServiceError,
    AccountingStore, AccountingStoreError, ApiKeyPepper, BudgetAmount, Currency, IdentityStore,
    MembershipRole, PriceSnapshot, QuotaScope, VerifiedIdentity,
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

    async fn plan(&self, name: &str, allowance: i64) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.end_user_plan
                 (id, organization_id, service_account_id, name, allowance_minor)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(self.organization)
        .bind(self.service_account)
        .bind(name)
        .bind(allowance)
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

    /// A grant of `amount` expiring in `days` (or never), as its quota bucket.
    async fn grant(&self, end_user: Uuid, amount: i64, days: Option<i64>) -> Uuid {
        let (grant, bucket) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Utc::now();
        let expires = days.map(|days| now + Duration::days(days));
        sqlx::query(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, service_account_id, scope_kind, end_user_id,
                  credit_grant_id, period_start, period_end, currency, limit_minor)
             VALUES ($1, $2, $3, 'end-user', $4, $5, $6, $7, 'CRD', $8)",
        )
        .bind(bucket)
        .bind(self.organization)
        .bind(self.service_account)
        .bind(end_user)
        .bind(grant)
        .bind(now - Duration::seconds(1))
        .bind(expires.unwrap_or_else(gateway::domain::end_user::never_expires))
        .bind(amount)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.end_user_credit_grant
                 (id, organization_id, service_account_id, end_user_id, reference,
                  amount_minor, currency, expires_at, quota_bucket_id)
             VALUES ($1, $2, $3, $4, $5, $6, 'CRD', $7, $8)",
        )
        .bind(grant)
        .bind(self.organization)
        .bind(self.service_account)
        .bind(end_user)
        .bind(format!("order-{grant}"))
        .bind(amount)
        .bind(expires)
        .bind(bucket)
        .execute(&self.pool)
        .await
        .unwrap();
        bucket
    }

    /// `(consumed, reserved, limit)` of the user's month allowance.
    async fn allowance(&self, end_user: Uuid) -> (i64, i64, i64) {
        sqlx::query_as(
            "SELECT consumed_minor, reserved_minor, limit_minor FROM gateway.quota_bucket
             WHERE end_user_id = $1 AND credit_grant_id IS NULL",
        )
        .bind(end_user)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// `(consumed, reserved)` of one credit bucket.
    async fn credit(&self, bucket: Uuid) -> (i64, i64) {
        sqlx::query_as(
            "SELECT consumed_minor, reserved_minor FROM gateway.quota_bucket WHERE id = $1",
        )
        .bind(bucket)
        .fetch_one(&self.pool)
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

fn refusal(error: AccountingServiceError) -> gateway::domain::end_user::EndUserRefusal {
    match error {
        AccountingServiceError::Store(AccountingStoreError::EndUserQuotaExceeded(refusal)) => {
            refusal
        }
        other => panic!("expected an end-user refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn requests_spend_the_allowance_then_the_soonest_expiring_credits() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    let pro = app.plan("pro", 6).await;
    let alice = app.user("alice", Some(pro)).await;
    let later = app.grant(alice, 10, None).await;
    let sooner = app.grant(alice, 10, Some(30)).await;

    // Each request reserves 5 and settles at 2.
    app.chat(Some("alice")).await.unwrap();
    assert_eq!(app.allowance(alice).await, (2, 0, 6));
    // 4 left: reserves 4 of allowance and 1 of credit, settles 2 from the
    // allowance and gives the credit back.
    app.chat(Some("alice")).await.unwrap();
    assert_eq!(app.allowance(alice).await, (4, 0, 6));
    assert_eq!(app.credit(sooner).await, (0, 0));
    app.chat(Some("alice")).await.unwrap();
    assert_eq!(app.allowance(alice).await, (6, 0, 6));
    // Allowance gone: the grant that expires sooner pays, never the other.
    app.chat(Some("alice")).await.unwrap();
    assert_eq!(app.credit(sooner).await, (2, 0));
    assert_eq!(app.credit(later).await, (0, 0));
    // Every request was charged to the service account's budget as well.
    assert_eq!(app.service_account_consumed().await, 8);
    let traced: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.inference_trace WHERE end_user_id = $1")
            .bind(alice)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(traced, 4);
    // Listed with this month's spend, from allowance and credits alike.
    let (users, _) = EndUserStore::new(app.pool.clone())
        .end_users(app.organization, app.service_account, 10, None, Utc::now())
        .await
        .unwrap();
    let listed = users.iter().find(|user| user.id == "alice").unwrap();
    assert_eq!(listed.spent_minor, Some(8));
    assert_eq!(listed.spent_currency.as_deref(), Some("CRD"));
    // Read from the ledger: traces deleted by retention leave it as it was.
    sqlx::query("DELETE FROM gateway.inference_trace WHERE end_user_id = $1")
        .bind(alice)
        .execute(&app.pool)
        .await
        .unwrap();
    let (users, _) = EndUserStore::new(app.pool.clone())
        .end_users(app.organization, app.service_account, 10, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(users[0].spent_minor, Some(8));
    // A request still awaiting reconciliation (a charge observed, above
    // its reservation, but not settled) is not counted.
    let reservation = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.accounting_reservation
             (id, organization_id, service_account_id, idempotency_key, fingerprint, state,
              reserved_minor, expires_at, output_token_cap, input_price_minor,
              output_price_minor, cached_input_price_minor, reasoning_price_minor,
              other_price_minor, price_version, pricing_confidence, currency, deployment,
              model, trace_id, end_user_id)
         VALUES ($1, $2, $3, $4, $5, 'unknown', 5, now() + interval '5 minutes', 4, 1, 1,
                 1, 1, 1, 'v1', 'known', 'CRD', 'deployment', 'model', $6, $7)",
    )
    .bind(reservation)
    .bind(app.organization)
    .bind(app.service_account)
    .bind(reservation.to_string())
    .bind(vec![7_u8; 32])
    .bind(Uuid::new_v4())
    .bind(alice)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.accounting_usage
             (id, reservation_id, organization_id, currency, confidence, amount_minor,
              completeness, finality)
         VALUES ($1, $2, $3, 'CRD', 'exact', 50, 'complete', 'final')",
    )
    .bind(Uuid::new_v4())
    .bind(reservation)
    .bind(app.organization)
    .execute(&app.pool)
    .await
    .unwrap();
    let (users, _) = EndUserStore::new(app.pool.clone())
        .end_users(app.organization, app.service_account, 10, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(users[0].spent_minor, Some(8));
}

#[tokio::test]
async fn a_user_past_their_allowance_and_credits_is_refused_while_others_are_served() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    let free = app.plan("free", 3).await;
    app.default_plan(Some(free)).await;
    // The first request fits only with a smaller output cap (1 input and 1
    // output, at 2 a token for output and reasoning), and settles at 2; the
    // 1 left cannot cover another.
    app.chat(Some("bob")).await.unwrap();
    let refused = refusal(app.chat(Some("bob")).await.unwrap_err());
    assert_eq!(refused.end_user, "bob");
    assert_eq!(refused.allowance_remaining, 1);
    assert_eq!(refused.credit_balance, 0);
    assert_eq!(refused.currency, "CRD");
    // Another user of the same app, on the same default plan, still fits,
    // and so does a request naming no user.
    app.chat(Some("carol")).await.unwrap();
    app.chat(None).await.unwrap();
    // Credits granted to bob let him through again.
    let bob = app.user("bob", None).await;
    app.grant(bob, 50, None).await;
    app.chat(Some("bob")).await.unwrap();
}

#[tokio::test]
async fn required_mode_and_unregistered_users_are_refused_before_any_charge() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("required").await;
    assert!(matches!(
        app.chat(None).await,
        Err(AccountingServiceError::Store(
            AccountingStoreError::EndUserRequired
        ))
    ));
    // No default plan: a user the app never assigned is refused.
    assert!(matches!(
        app.chat(Some("dave")).await,
        Err(AccountingServiceError::Store(
            AccountingStoreError::EndUserNotRegistered
        ))
    ));
    let pro = app.plan("pro", 100).await;
    app.user("dave", Some(pro)).await;
    app.chat(Some("dave")).await.unwrap();
    assert_eq!(app.service_account_consumed().await, 2);
}

#[tokio::test]
async fn a_service_account_without_settings_records_its_end_users_without_limiting_them() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.chat(Some("erin")).await.unwrap();
    let (users, buckets): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM gateway.end_user WHERE service_account_id = $1),
                (SELECT count(*) FROM gateway.quota_bucket WHERE end_user_id IS NOT NULL
                   AND service_account_id = $1)",
    )
    .bind(app.service_account)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!((users, buckets), (1, 0));
    // Its spend is reported in the currency its requests used.
    let (listed, _) = EndUserStore::new(app.pool.clone())
        .end_users(app.organization, app.service_account, 10, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(listed[0].spent_minor, Some(2));
    assert_eq!(listed[0].spent_currency.as_deref(), Some("CRD"));
}

#[tokio::test]
async fn a_plan_change_applies_from_the_next_request_and_keeps_the_months_spend() {
    let _guard = database_guard();
    let Some(app) = app(1_000).await else {
        return;
    };
    app.settings("optional").await;
    let free = app.plan("free", 3).await;
    let pro = app.plan("pro", 10).await;
    app.default_plan(Some(free)).await;
    app.chat(Some("frank")).await.unwrap();
    assert!(app.chat(Some("frank")).await.is_err());
    // An upgrade: 10 less the 2 already spent this month.
    let frank = app.user("frank", Some(pro)).await;
    app.chat(Some("frank")).await.unwrap();
    assert_eq!(app.allowance(frank).await, (4, 0, 10));
    // A downgrade below the spend leaves nothing until next month.
    app.user("frank", Some(free)).await;
    let refused = refusal(app.chat(Some("frank")).await.unwrap_err());
    assert_eq!(refused.allowance_remaining, 0);
    assert_eq!(app.allowance(frank).await.0, 4);
}

#[tokio::test]
async fn concurrent_requests_of_one_user_never_overspend_their_credits() {
    let _guard = database_guard();
    let Some(app) = app(1_000_000).await else {
        return;
    };
    app.settings("optional").await;
    let credits_only = app.plan("credits", 0).await;
    app.default_plan(Some(credits_only)).await;
    let gina = app.user("gina", None).await;
    let bucket = app.grant(gina, 13, None).await;
    let results = futures::future::join_all((0..50).map(|_| app.chat(Some("gina")))).await;
    let served = results.iter().filter(|result| result.is_ok()).count();
    for result in results {
        if let Err(error) = result {
            refusal(error);
        }
    }
    let (consumed, reserved) = app.credit(bucket).await;
    assert_eq!(reserved, 0);
    assert!(consumed <= 13, "credits overspent: {consumed}");
    assert_eq!(consumed, 2 * served as i64);
    assert!(served >= 1);
    // The allowance of a credits-only plan is never spent.
    assert_eq!(app.allowance(gina).await.0, 0);
}
