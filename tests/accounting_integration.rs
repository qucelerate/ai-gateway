#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
use std::{
    env,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration as StdDuration,
};

use chrono::{Duration, Utc};
use futures::{StreamExt, stream};
use gateway::{
    AccountedChatRequest, AccountingPolicy, AccountingService, AccountingServiceError,
    AccountingStore, ApiKeyPepper, BudgetAmount, Currency, IdentityStore, MembershipRole,
    PriceSnapshot, QuotaScope, Settlement, VerifiedIdentity,
    domain::catalog::ChatInputCountMode,
    domain::inference::{
        ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent, ChoiceFinished,
        CorrelationId, DeploymentModelReference, Embedding, EmbeddingRequest, EmbeddingResponse,
        FinishReason, InferenceContext, ProviderAttemptIdempotencyKey, ProviderError,
        ProviderErrorCategory, ResponseCompletion, ResponseStarted, SafeMetadata, TokenCounter,
        Usage, UsageSnapshot,
    },
    provider::{ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

async fn pool() -> Option<PgPool> {
    let url = env::var("GATEWAY_TEST_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
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

/// Outbox events recorded for an organization, oldest first.
async fn outbox_events(
    pool: &PgPool,
    organization: gateway::OrganizationId,
) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT event_type, payload FROM gateway.outbox_event
         WHERE organization_id = $1 ORDER BY sequence",
    )
    .bind(organization.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

struct CountingProvider {
    count_calls: Arc<AtomicUsize>,
    generation_calls: Arc<AtomicUsize>,
    fail_count: bool,
    reported_input_tokens: u64,
    /// Cache writes reported apart from the input (Anthropic's and
    /// Bedrock's `other`).
    reported_cache_writes: u64,
    order: Arc<Mutex<Vec<&'static str>>>,
    observation: Arc<Mutex<Option<(PgPool, Uuid)>>>,
    claimed_seen: Arc<AtomicUsize>,
}

impl CountingProvider {
    async fn observe_claimed_reservation(&self) {
        let Some((pool, organization_id)) = self.observation.lock().unwrap().clone() else {
            return;
        };
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM gateway.accounting_reservation WHERE organization_id=$1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(organization_id)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if state.as_deref() == Some("claimed") {
            self.claimed_seen.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for CountingProvider {
    async fn count_chat_input_tokens(
        &self,
        _: &InferenceContext,
        _: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        self.count_calls.fetch_add(1, Ordering::SeqCst);
        self.order.lock().unwrap().push("count");
        if self.fail_count {
            return Err(ProviderError::new(ProviderErrorCategory::Provider));
        }
        Ok(1)
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.generation_calls.fetch_add(1, Ordering::SeqCst);
        self.order.lock().unwrap().push("generation");
        self.observe_claimed_reservation().await;
        let usage = Usage::complete(
            TokenCounter::exact(self.reported_input_tokens),
            TokenCounter::exact(self.reported_input_tokens),
            None,
            None,
            (self.reported_cache_writes > 0)
                .then(|| TokenCounter::exact(self.reported_cache_writes)),
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
        self.generation_calls.fetch_add(1, Ordering::SeqCst);
        self.order.lock().unwrap().push("generation");
        self.observe_claimed_reservation().await;
        let usage = Usage::complete(
            TokenCounter::exact(self.reported_input_tokens),
            TokenCounter::exact(1),
            None,
            None,
            (self.reported_cache_writes > 0)
                .then(|| TokenCounter::exact(self.reported_cache_writes)),
        )
        .unwrap();
        Ok(ValidatingChatEventStream::new(stream::iter(vec![
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model".into(), SafeMetadata::default()).unwrap(),
            )),
            Ok(ChatStreamEvent::ChoiceFinished(
                ChoiceFinished::new(0, FinishReason::new("stop")).unwrap(),
            )),
            Ok(ChatStreamEvent::ResponseCompleted(
                ResponseCompletion::new(
                    Some("counting-stream".into()),
                    "model".into(),
                    Some(UsageSnapshot::final_(usage).unwrap()),
                )
                .unwrap(),
            )),
        ])))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

struct SuccessfulProvider;

async fn exact_chat_count(_: &InferenceContext, _: &ChatRequest) -> Result<u64, ProviderError> {
    Ok(1)
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for SuccessfulProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
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

struct OverboundProvider;

/// Reasons past the output cap, as Grok does: 2 output tokens within the
/// request's cap of 4, and 20 reasoning tokens on top.
struct UncappedReasoningProvider;

#[async_trait::async_trait]
impl InferenceProviderAdapter for UncappedReasoningProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(2),
            None,
            Some(TokenCounter::exact(20)),
            None,
        )
        .unwrap();
        Ok(ChatResponse::new(
            Some("reasoned".into()),
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

struct PostClaimTimeoutProvider;

struct UnknownStreamProvider;

struct EmbeddingProvider;

#[async_trait::async_trait]
impl InferenceProviderAdapter for EmbeddingProvider {
    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
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
        let usage = Usage::embedding(TokenCounter::exact(2)).unwrap();
        Ok(EmbeddingResponse::new(
            Some("embedding-request".into()),
            "embedding-model".into(),
            vec![Embedding::new(0, vec![0.1, 0.2]).unwrap()],
            Some(UsageSnapshot::final_(usage).unwrap()),
        )
        .unwrap())
    }
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for OverboundProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(100),
            None,
            None,
            None,
        )
        .unwrap();
        Ok(ChatResponse::new(
            Some("overbound".into()),
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

#[async_trait::async_trait]
impl InferenceProviderAdapter for PostClaimTimeoutProvider {
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
        Err(ProviderError::from_parts(
            ProviderErrorCategory::Timeout,
            Some("bedrock".into()),
            Some("req-timeout-1".into()),
            Some(408),
            Some("ModelTimeoutException".into()),
            None,
            None,
        )
        .unwrap())
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Timeout))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for UnknownStreamProvider {
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
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let concrete = Usage::partial(
            Some(TokenCounter::exact(1)),
            Some(TokenCounter::exact(1)),
            Some(TokenCounter::exact(2)),
            None,
            None,
        );
        let missing_cached = Usage::partial(
            Some(TokenCounter::exact(1)),
            Some(TokenCounter::exact(2)),
            None,
            None,
            None,
        );
        Ok(ValidatingChatEventStream::new(stream::iter(vec![
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model".into(), SafeMetadata::default()).unwrap(),
            )),
            Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(concrete))),
            Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
                missing_cached,
            ))),
        ])))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

struct StreamingProvider;

#[async_trait::async_trait]
impl InferenceProviderAdapter for StreamingProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(1),
            None,
            None,
            None,
        )
        .unwrap();
        let events = vec![
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model".into(), SafeMetadata::default()).unwrap(),
            )),
            Ok(ChatStreamEvent::ChoiceFinished(
                ChoiceFinished::new(0, FinishReason::new("stop")).unwrap(),
            )),
            Ok(ChatStreamEvent::ResponseCompleted(
                ResponseCompletion::new(
                    Some("stream-request".into()),
                    "model".into(),
                    Some(UsageSnapshot::final_(usage).unwrap()),
                )
                .unwrap(),
            )),
        ];
        Ok(ValidatingChatEventStream::new(stream::iter(events)))
    }
    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

/// Holds every chat call open until the test releases it, so the claim stays
/// observable for as long as the test needs.
struct GatedProvider {
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for GatedProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.release.notified().await;
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(1),
            None,
            None,
            None,
        )
        .unwrap();
        Ok(ChatResponse::new(
            Some("slow".into()),
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

struct PartialFailureProvider;

#[async_trait::async_trait]
impl InferenceProviderAdapter for PartialFailureProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable)
            .with_partial_usage(Usage::partial(
                Some(TokenCounter::exact(2)),
                None,
                None,
                None,
                None,
            ))
            .unwrap())
    }
    /// Fails after the response starts, with partial usage whose input
    /// count differs from the exact count admission used.
    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let failure = ProviderError::from_parts(
            ProviderErrorCategory::Provider,
            Some("bedrock".into()),
            Some("req-stream-1".into()),
            Some(500),
            Some("ModelStreamErrorException".into()),
            None,
            Some(Usage::partial(
                Some(TokenCounter::exact(2)),
                None,
                None,
                None,
                None,
            )),
        )
        .unwrap();
        Ok(ValidatingChatEventStream::new(stream::iter(vec![
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model".into(), SafeMetadata::default()).unwrap(),
            )),
            Err(failure),
        ])))
    }
    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

struct UnsupportedCountProvider {
    reported_input_tokens: u64,
    reported_output_tokens: u64,
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for UnsupportedCountProvider {
    async fn count_chat_input_tokens(
        &self,
        _: &InferenceContext,
        _: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        panic!(
            "count_chat_input_tokens must not be called for ChatInputCountMode::Unsupported routes"
        );
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let usage = Usage::complete(
            TokenCounter::exact(self.reported_input_tokens),
            TokenCounter::exact(self.reported_output_tokens),
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
        let usage = Usage::complete(
            TokenCounter::exact(self.reported_input_tokens),
            TokenCounter::exact(self.reported_output_tokens),
            None,
            None,
            None,
        )
        .unwrap();
        Ok(ValidatingChatEventStream::new(stream::iter(vec![
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model".into(), SafeMetadata::default()).unwrap(),
            )),
            Ok(ChatStreamEvent::ChoiceFinished(
                ChoiceFinished::new(0, FinishReason::new("stop")).unwrap(),
            )),
            Ok(ChatStreamEvent::ResponseCompleted(
                ResponseCompletion::new(
                    Some("unsupported-count-stream".into()),
                    "model".into(),
                    Some(UsageSnapshot::final_(usage).unwrap()),
                )
                .unwrap(),
            )),
        ])))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

async fn setup_with(
    provider: std::sync::Arc<dyn InferenceProviderAdapter>,
    limit: i64,
) -> Option<(PgPool, AccountingService, VerifiedIdentity)> {
    setup_with_ttl(provider, limit, StdDuration::from_secs(60)).await
}

async fn setup_with_ttl(
    provider: std::sync::Arc<dyn InferenceProviderAdapter>,
    limit: i64,
    ttl: StdDuration,
) -> Option<(PgPool, AccountingService, VerifiedIdentity)> {
    setup_with_ttl_and_price(provider, limit, ttl, PriceSnapshot::new(1, 1, 0).unwrap()).await
}

async fn setup_with_ttl_and_price(
    provider: std::sync::Arc<dyn InferenceProviderAdapter>,
    limit: i64,
    ttl: StdDuration,
    price: PriceSnapshot,
) -> Option<(PgPool, AccountingService, VerifiedIdentity)> {
    setup_with_ttl_and_price_and_mode(
        provider,
        limit,
        ttl,
        price,
        ChatInputCountMode::ProviderApiExact,
    )
    .await
}

async fn setup_with_ttl_and_price_and_mode(
    provider: std::sync::Arc<dyn InferenceProviderAdapter>,
    limit: i64,
    ttl: StdDuration,
    price: PriceSnapshot,
    mode: ChatInputCountMode,
) -> Option<(PgPool, AccountingService, VerifiedIdentity)> {
    setup_with_reasoning_allowance(provider, limit, ttl, price, mode, 0).await
}

async fn setup_with_reasoning_allowance(
    provider: std::sync::Arc<dyn InferenceProviderAdapter>,
    limit: i64,
    ttl: StdDuration,
    price: PriceSnapshot,
    mode: ChatInputCountMode,
    reasoning_allowance: u32,
) -> Option<(PgPool, AccountingService, VerifiedIdentity)> {
    let pool = pool().await?;
    let identity_store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"accounting-test-pepper").unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity_store
        .create_organization(&format!("accounting-{suffix}"), "Accounting test")
        .await
        .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"));
    let user = identity_store
        .create_user(org.id, None, None, None, Some("test"))
        .await
        .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"));
    identity_store
        .add_organization_membership(org.id, user.id, MembershipRole::Owner)
        .await
        .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"));
    let service_account = identity_store
        .create_service_account(org.id, user.id, None, "test-service")
        .await
        .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"));
    let key = identity_store
        .create_api_key(
            org.id,
            service_account.id,
            &[gateway::domain::identity::ApiKeyScope::Inference],
            None,
            user.id,
        )
        .await
        .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"));
    let identity = VerifiedIdentity {
        organization_id: org.id,
        service_account_id: service_account.id,
        team_id: None,
        api_key_id: Some(key.id),
        scopes: vec![],
        end_user: None,
    };
    let store = AccountingStore::new(pool.clone());
    let scope = QuotaScope {
        organization_id: org.id,
        team_id: None,
        service_account_id: Some(service_account.id),
        api_key_id: None,
    };
    let now = Utc::now();
    store
        .create_quota_bucket(
            &scope,
            BudgetAmount::new(limit).unwrap(),
            &Currency::new("CRD").unwrap(),
            now - Duration::hours(1),
            now + Duration::hours(1),
        )
        .await
        .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"));
    let policy = AccountingPolicy::new(
        1024,
        std::num::NonZeroU32::new(4).unwrap(),
        ttl,
        price,
        Currency::new("CRD").unwrap(),
        [9; 32],
    )
    .unwrap_or_else(|error| panic!("configured accounting test setup failed: {error}"))
    .with_chat_input_count_mode(mode)
    .with_reasoning_allowance(reasoning_allowance);
    Some((
        pool,
        AccountingService::new(store, provider, policy),
        identity,
    ))
}

async fn setup() -> Option<(PgPool, AccountingService, VerifiedIdentity)> {
    setup_with(std::sync::Arc::new(SuccessfulProvider), 10).await
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

#[tokio::test]
async fn local_chat_rejection_happens_before_count_generation_or_rows() {
    let _guard = database_guard();
    let count_calls = Arc::new(AtomicUsize::new(0));
    let generation_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        count_calls: count_calls.clone(),
        generation_calls: generation_calls.clone(),
        fail_count: false,
        reported_input_tokens: 1,
        reported_cache_writes: 0,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    let Some((pool, service, identity)) = setup_with(provider, 100).await else {
        return;
    };
    let mut invalid = request();
    invalid.max_tokens = Some(0);
    let result = service
        .chat(
            &identity,
            context(),
            invalid,
            AccountedChatRequest::new("local-rejection"),
        )
        .await;
    assert!(matches!(
        result,
        Err(gateway::AccountingServiceError::OutputCapExceeded)
    ));
    assert_eq!(count_calls.load(Ordering::SeqCst), 0);
    assert_eq!(generation_calls.load(Ordering::SeqCst), 0);
    for table in [
        "accounting_reservation",
        "inference_trace",
        "accounting_allocation",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM gateway.{table} WHERE organization_id=$1"
        ))
        .bind(identity.organization_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "unexpected rows in {table}");
    }
}

#[tokio::test]
async fn an_api_key_budget_refuses_before_generation_while_its_service_account_has_room() {
    let _guard = database_guard();
    let count_calls = Arc::new(AtomicUsize::new(0));
    let generation_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        count_calls: count_calls.clone(),
        generation_calls: generation_calls.clone(),
        fail_count: false,
        reported_input_tokens: 1,
        reported_cache_writes: 0,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    // The service account's budget has plenty of room; the key's has none.
    let Some((pool, service, identity)) = setup_with(provider, 1_000_000).await else {
        return;
    };
    let now = Utc::now();
    let key_scope = QuotaScope {
        api_key_id: identity.api_key_id,
        ..QuotaScope::from_identity(&identity).unwrap()
    };
    let key_bucket = AccountingStore::new(pool.clone())
        .create_quota_bucket(
            &key_scope,
            BudgetAmount::new(1).unwrap(),
            &Currency::new("CRD").unwrap(),
            now - Duration::hours(1),
            now + Duration::hours(1),
        )
        .await
        .unwrap();
    let result = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("key-budget"),
        )
        .await;
    assert!(
        matches!(
            result,
            Err(AccountingServiceError::Store(
                gateway::AccountingStoreError::ApiKeyBudgetExceeded
            ))
        ),
        "{result:?}"
    );
    assert_eq!(generation_calls.load(Ordering::SeqCst), 0);
    let reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation WHERE organization_id=$1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reservations, 0);

    // The same service account without that key is served, and charged only
    // to its own buckets.
    let without_key = VerifiedIdentity {
        api_key_id: None,
        ..identity.clone()
    };
    service
        .chat(
            &without_key,
            context(),
            request(),
            AccountedChatRequest::new("service-account-budget"),
        )
        .await
        .unwrap();
    assert_eq!(generation_calls.load(Ordering::SeqCst), 1);
    let key_reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_minor + consumed_minor FROM gateway.quota_bucket WHERE id=$1",
    )
    .bind(key_bucket)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(key_reserved, 0);

    // A key budget with room is charged alongside the service account's.
    sqlx::query("UPDATE gateway.quota_bucket SET limit_minor = 1000000 WHERE id=$1")
        .bind(key_bucket)
        .execute(&pool)
        .await
        .unwrap();
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("key-budget-room"),
        )
        .await
        .unwrap();
    let allocations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_allocation WHERE quota_bucket_id=$1",
    )
    .bind(key_bucket)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(allocations, 1);
}

#[tokio::test]
async fn chat_count_failure_happens_before_reservation_trace_or_generation() {
    let _guard = database_guard();
    let count_calls = Arc::new(AtomicUsize::new(0));
    let generation_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        count_calls: count_calls.clone(),
        generation_calls: generation_calls.clone(),
        fail_count: true,
        reported_input_tokens: 1,
        reported_cache_writes: 0,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    let Some((pool, service, identity)) = setup_with(provider, 100).await else {
        return;
    };
    let result = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("count-failure"),
        )
        .await;
    assert!(matches!(
        result,
        Err(gateway::AccountingServiceError::Provider(_))
    ));
    assert_eq!(count_calls.load(Ordering::SeqCst), 1);
    assert_eq!(generation_calls.load(Ordering::SeqCst), 0);
    for table in [
        "accounting_reservation",
        "inference_trace",
        "accounting_allocation",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM gateway.{table} WHERE organization_id=$1"
        ))
        .bind(identity.organization_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "unexpected rows in {table}");
    }
}

#[tokio::test]
async fn exact_count_precedes_claimed_chat_and_stream_generation() {
    let _guard = database_guard();
    let provider = Arc::new(CountingProvider {
        count_calls: Arc::new(AtomicUsize::new(0)),
        generation_calls: Arc::new(AtomicUsize::new(0)),
        fail_count: false,
        reported_input_tokens: 1,
        reported_cache_writes: 0,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    let Some((pool, service, identity)) = setup_with(provider.clone(), 100).await else {
        return;
    };
    *provider.observation.lock().unwrap() = Some((pool.clone(), identity.organization_id.0));

    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("ordered-chat"),
        )
        .await
        .unwrap();
    service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("ordered-stream"),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        *provider.order.lock().unwrap(),
        vec!["count", "generation", "count", "generation"]
    );
    assert_eq!(provider.claimed_seen.load(Ordering::SeqCst), 2);
    let input_bounds: Vec<i64> = sqlx::query_scalar(
        "SELECT input_bound FROM gateway.inference_trace WHERE organization_id=$1 ORDER BY recorded_at",
    )
    .bind(identity.organization_id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(input_bounds, vec![1, 1]);
}

#[tokio::test]
async fn cache_writes_count_toward_the_provider_counted_prompt() {
    let _guard = database_guard();
    // The provider counted 1 prompt token; the response reports it as a
    // cache write (other), with no uncached input: no drift, both ways.
    let provider = Arc::new(CountingProvider {
        count_calls: Arc::new(AtomicUsize::new(0)),
        generation_calls: Arc::new(AtomicUsize::new(0)),
        fail_count: false,
        reported_input_tokens: 0,
        reported_cache_writes: 1,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    let Some((_pool, service, identity)) = setup_with(provider, 100).await else {
        return;
    };
    let unary = format!("cache-write-{}", Uuid::new_v4());
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&unary),
        )
        .await
        .unwrap();
    let streamed = format!("cache-write-stream-{}", Uuid::new_v4());
    let events = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&streamed),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    for key in [&unary, &streamed] {
        let reservation = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                key,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reservation.state,
            gateway::ReservationState::Settled,
            "{key}"
        );
    }
}

#[tokio::test]
async fn input_count_drift_retains_holds_and_opens_reconciliation() {
    let _guard = database_guard();
    let provider = Arc::new(CountingProvider {
        count_calls: Arc::new(AtomicUsize::new(0)),
        generation_calls: Arc::new(AtomicUsize::new(0)),
        fail_count: false,
        reported_input_tokens: 2,
        reported_cache_writes: 0,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    let Some((pool, service, identity)) = setup_with(provider, 100).await else {
        return;
    };
    let key = format!("input-drift-{}", Uuid::new_v4());
    let stream = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let events = stream.collect::<Vec<_>>().await;
    assert!(events.iter().all(Result::is_ok));
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
    let held: i64 = sqlx::query_scalar(
        "SELECT reserved_minor FROM gateway.accounting_allocation WHERE reservation_id=$1 AND state='held'",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(held, reservation.reserved.value());
    let usage: (i64, Option<i64>) = sqlx::query_as(
        "SELECT input_tokens, amount_minor FROM gateway.accounting_usage WHERE reservation_id=$1 ORDER BY recorded_at DESC LIMIT 1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage, (2, None));
    let reconciliation: (String, String) = sqlx::query_as(
        "SELECT reason, status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reconciliation, ("input-count-drift".into(), "open".into()));
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome, "input-count-drift");
}

#[tokio::test]
async fn unsupported_count_mode_reserves_against_tokenizer_bound_and_settles_chat() {
    let _guard = database_guard();
    let provider = std::sync::Arc::new(UnsupportedCountProvider {
        reported_input_tokens: 3,
        reported_output_tokens: 2,
    });
    let Some((pool, service, identity)) = setup_with_ttl_and_price_and_mode(
        provider,
        100,
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 1, 0).unwrap(),
        ChatInputCountMode::Unsupported,
    )
    .await
    else {
        return;
    };
    let key = format!("unsupported-count-chat-{}", Uuid::new_v4());
    // Reaching this response at all proves `count_chat_input_tokens` was never
    // called (`UnsupportedCountProvider` panics if it is), since the
    // catalog-configured tokenizer upper bound is used for reservation
    // instead.
    let response = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    assert_eq!(response.model(), "model");
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Settled);
    let usage: (i64, Option<i64>) = sqlx::query_as(
        "SELECT input_tokens, amount_minor FROM gateway.accounting_usage WHERE reservation_id=$1 ORDER BY recorded_at DESC LIMIT 1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage.0, 3);
    assert!(usage.1.is_some());
    let reconciliation: Option<(String, String)> = sqlx::query_as(
        "SELECT reason, status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(reconciliation.is_none());
    let (metadata, outcome): (serde_json::Value, String) =
        sqlx::query_as("SELECT metadata, outcome FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome, "settled");
    assert_eq!(
        metadata,
        serde_json::json!({"input_bound": 69, "output_cap": 4, "currency": "CRD"})
    );
}

#[tokio::test]
async fn unsupported_count_mode_reserves_against_tokenizer_bound_and_settles_stream() {
    let _guard = database_guard();
    let provider = std::sync::Arc::new(UnsupportedCountProvider {
        reported_input_tokens: 3,
        reported_output_tokens: 2,
    });
    let Some((pool, service, identity)) = setup_with_ttl_and_price_and_mode(
        provider,
        100,
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 1, 0).unwrap(),
        ChatInputCountMode::Unsupported,
    )
    .await
    else {
        return;
    };
    let key = format!("unsupported-count-stream-{}", Uuid::new_v4());
    let events = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Settled);
    let usage: (i64, Option<i64>) = sqlx::query_as(
        "SELECT input_tokens, amount_minor FROM gateway.accounting_usage WHERE reservation_id=$1 ORDER BY recorded_at DESC LIMIT 1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage.0, 3);
    assert!(usage.1.is_some());
    let reconciliation: Option<(String, String)> = sqlx::query_as(
        "SELECT reason, status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(reconciliation.is_none());
}

#[tokio::test]
async fn postgres_accounting_is_scoped_idempotent_and_trace_safe() {
    let _guard = database_guard();
    let Some((pool, service, identity)) = setup().await else {
        return;
    };
    let key = format!("accounting-{}", Uuid::new_v4());
    let response = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    assert_eq!(response.model(), "model");
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Settled);
    assert_eq!(
        service
            .store()
            .settle(
                identity.organization_id,
                reservation.id,
                Settlement {
                    finalization_id: Uuid::new_v4(),
                    usage: None,
                    trace_id: reservation.trace_id,
                    provider_request_id: None,
                    provider_error: None,
                    timings: Default::default(),
                },
                Utc::now()
            )
            .await
            .unwrap(),
        gateway::ReservationState::Settled
    );
    let conflicting_usage = Usage::complete(
        TokenCounter::exact(2),
        TokenCounter::exact(2),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(matches!(
        service
            .store()
            .settle(
                identity.organization_id,
                reservation.id,
                Settlement {
                    finalization_id: Uuid::new_v4(),
                    usage: Some(conflicting_usage),
                    trace_id: reservation.trace_id,
                    provider_request_id: None,
                    provider_error: None,
                    timings: Default::default(),
                },
                Utc::now(),
            )
            .await,
        Err(gateway::AccountingStoreError::SettlementConflict)
    ));
    assert!(
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(&key)
            )
            .await
            .is_err()
    );
    let other_org = gateway::OrganizationId::new();
    assert!(
        service
            .store()
            .find_reservation(other_org, reservation.id)
            .await
            .unwrap()
            .is_none()
    );
    let metadata: serde_json::Value =
        sqlx::query_scalar("SELECT metadata FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        metadata,
        serde_json::json!({"input_bound": 1, "output_cap": 4, "currency": "CRD"})
    );
    // The key that made the request is on its trace, and stays there.
    let api_key_id: Option<Uuid> =
        sqlx::query_scalar("SELECT api_key_id FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(api_key_id, identity.api_key_id.map(|id| id.0));
    assert!(
        sqlx::query("UPDATE gateway.inference_trace SET api_key_id = NULL WHERE id=$1")
            .bind(reservation.trace_id.0)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn postgres_accounting_rejects_concurrent_overcommit() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 6).await
    else {
        return;
    };
    let first = service.clone();
    let second = service.clone();
    let a = identity.clone();
    let b = identity.clone();
    let (left, right) = tokio::join!(
        first.chat(
            &a,
            context(),
            request(),
            AccountedChatRequest::new("concurrent-a")
        ),
        second.chat(
            &b,
            context(),
            request(),
            AccountedChatRequest::new("concurrent-b")
        ),
    );
    assert!(left.is_ok() ^ right.is_ok());
    let reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_minor FROM gateway.quota_bucket WHERE organization_id=$1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reserved, 0);
}

#[tokio::test]
async fn postgres_first_use_idempotency_race_has_one_reservation_and_one_winner() {
    let _guard = database_guard();
    let Some((pool, service, identity)) = setup().await else {
        return;
    };
    let left = service.clone();
    let right = service.clone();
    let left_identity = identity.clone();
    let right_identity = identity.clone();
    let (first, second) = tokio::join!(
        left.chat(
            &left_identity,
            context(),
            request(),
            AccountedChatRequest::new("same-first-use-key"),
        ),
        right.chat(
            &right_identity,
            context(),
            request(),
            AccountedChatRequest::new("same-first-use-key"),
        ),
    );
    assert!(first.is_ok() ^ second.is_ok());
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation WHERE organization_id=$1 AND idempotency_key=$2",
    )
    .bind(identity.organization_id.0)
    .bind("same-first-use-key")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn bound_breach_is_observed_and_reconciled_before_returning() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(OverboundProvider), 10).await
    else {
        return;
    };
    let key = format!("overbound-{}", Uuid::new_v4());
    let result = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await;
    assert!(matches!(
        result,
        Err(gateway::AccountingServiceError::Store(
            gateway::AccountingStoreError::BoundExceeded
        ))
    ));
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
    let reconciliation: (String, String) = sqlx::query_as(
        "SELECT reason,status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reconciliation, ("bound-breach".into(), "open".into()));
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome, "bound-breach");
    let events = outbox_events(&pool, identity.organization_id).await;
    let types: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(types, ["reconciliation.opened", "inference.finalized"]);
    assert_eq!(events[0].1["reason"], "bound-breach");
    assert_eq!(events[0].1["reservation_id"], reservation.id.0.to_string());
    assert_eq!(events[1].1["outcome"], "bound-breach");
    assert_eq!(events[1].1["trace_id"], reservation.trace_id.0.to_string());
}

/// With no reasoning price, reasoning is reserved at the output price, and
/// the output cap a nearly spent budget affords is sized at that same rate:
/// the hold never exceeds what is left.
#[tokio::test]
async fn an_affordable_cap_is_sized_at_the_effective_reasoning_rate() {
    let _guard = database_guard();
    let Some((_pool, service, identity)) = setup_with_ttl_and_price(
        std::sync::Arc::new(SuccessfulProvider),
        6,
        StdDuration::from_secs(60),
        PriceSnapshot::new(0, 1, 0).unwrap(),
    )
    .await
    else {
        return;
    };
    let key = format!("affordable-{}", Uuid::new_v4());
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    // 6 left, 2 per output token (output plus reasoning at the output
    // price): the request's cap of 4 is cut to 3, holding exactly 6.
    assert_eq!(reservation.output_token_cap.map(|cap| cap.get()), Some(3));
    assert_eq!(reservation.reserved.value(), 6);
}

/// A model that reasons past the output cap breaches a reservation sized by
/// the cap alone; its capability's reasoning allowance reserves room for that
/// reasoning, charged at the output price when none is published, and the
/// request settles.
#[tokio::test]
async fn a_reasoning_allowance_covers_reasoning_past_the_output_cap() {
    let _guard = database_guard();
    for (allowance, settles) in [(0, false), (20, true)] {
        let Some((_pool, service, identity)) = setup_with_reasoning_allowance(
            std::sync::Arc::new(UncappedReasoningProvider),
            1000,
            StdDuration::from_secs(60),
            PriceSnapshot::new(1, 1, 0).unwrap(),
            ChatInputCountMode::ProviderApiExact,
            allowance,
        )
        .await
        else {
            return;
        };
        let key = format!("reasoning-{}", Uuid::new_v4());
        let result = service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(&key),
            )
            .await;
        let reservation = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
            .unwrap();
        // Input 1 + cap 4 x (output + reasoning at the output price), plus
        // the allowance at the reasoning (here output) price.
        assert_eq!(reservation.reserved.value(), 9 + i64::from(allowance));
        if settles {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(reservation.state, gateway::ReservationState::Settled);
        } else {
            assert!(matches!(
                result,
                Err(gateway::AccountingServiceError::Store(
                    gateway::AccountingStoreError::BoundExceeded
                ))
            ));
        }
    }
}

#[tokio::test]
async fn output_cap_breach_is_rejected_even_when_output_is_free() {
    let _guard = database_guard();
    let Some((pool, service, identity)) = setup_with_ttl_and_price(
        std::sync::Arc::new(OverboundProvider),
        100,
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 0, 0).unwrap(),
    )
    .await
    else {
        return;
    };
    let key = format!("free-output-overbound-{}", Uuid::new_v4());
    let result = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await;
    assert!(matches!(
        result,
        Err(gateway::AccountingServiceError::Store(
            gateway::AccountingStoreError::BoundExceeded
        ))
    ));
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
    let allocation: (String, i64) = sqlx::query_as(
        "SELECT state, reserved_minor FROM gateway.accounting_allocation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(allocation, ("held".into(), 1));
    let usage: (i64, Option<i64>) = sqlx::query_as(
        "SELECT output_tokens, amount_minor FROM gateway.accounting_usage WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage, (100, None));
    let reconciliation: (String, String) = sqlx::query_as(
        "SELECT reason, status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reconciliation, ("bound-breach".into(), "open".into()));
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome, "bound-breach");
}

#[tokio::test]
async fn post_claim_timeout_marks_reservation_unknown_and_keeps_hold() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(PostClaimTimeoutProvider), 10).await
    else {
        return;
    };
    let key = format!("post-claim-timeout-{}", Uuid::new_v4());
    let result = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await;
    assert!(matches!(
        result,
        Err(gateway::AccountingServiceError::Provider(error))
            if error.category() == ProviderErrorCategory::Timeout
    ));
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
    let allocation_state: String = sqlx::query_scalar(
        "SELECT state FROM gateway.accounting_allocation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(allocation_state, "held");
    let usage_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.accounting_usage WHERE reservation_id=$1")
            .bind(reservation.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(usage_count, 0);
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome, "unknown");
    // The trace keeps why the provider failed, and its request id.
    let failure: (Option<String>, Option<i16>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT provider_error_category, provider_error_status, provider_error_code,
                provider_request_id
         FROM gateway.inference_trace WHERE id=$1",
    )
    .bind(reservation.trace_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        failure,
        (
            Some("timeout".into()),
            Some(408),
            Some("ModelTimeoutException".into()),
            Some("req-timeout-1".into())
        )
    );
}

#[tokio::test]
async fn stream_usage_counter_disappearance_is_accounted_as_unknown() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(UnknownStreamProvider), 10).await
    else {
        return;
    };
    let key = format!("stream-usage-unknown-{}", Uuid::new_v4());
    let response_stream = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let events = response_stream.collect::<Vec<_>>().await;
    assert!(
        matches!(events.last(), Some(Err(error)) if error.category() == ProviderErrorCategory::Protocol)
    );
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
    let allocation_state: String = sqlx::query_scalar(
        "SELECT state FROM gateway.accounting_allocation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(allocation_state, "held");
    let reconciliation: (String, String) = sqlx::query_as(
        "SELECT reason, status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reconciliation, ("provider-unknown".into(), "open".into()));
}

#[tokio::test]
async fn stream_completion_finalizes_accounting_durably() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(StreamingProvider), 10).await
    else {
        return;
    };
    let key = format!("stream-{}", Uuid::new_v4());
    let response_stream = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let events: Vec<_> = response_stream.collect().await;
    assert!(matches!(
        events.last(),
        Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
    ));
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Settled);
    let settlement_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.accounting_usage WHERE reservation_id=$1")
            .bind(reservation.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settlement_count, 1);
    let events = outbox_events(&pool, identity.organization_id).await;
    assert_eq!(events.len(), 1);
    let (kind, payload) = &events[0];
    assert_eq!(kind, "inference.finalized");
    assert_eq!(payload["outcome"], "settled");
    assert_eq!(payload["currency"], "CRD");
    assert_eq!(
        payload["service_account_id"],
        identity.service_account_id.0.to_string()
    );
    assert!(payload["amount_minor"].is_i64());
    // Metadata only: nothing from the prompt or completion.
    assert!(!payload.to_string().contains("hello"));
}

#[tokio::test]
async fn dropping_stream_before_completion_triggers_fallback_settlement() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(StreamingProvider), 10).await
    else {
        return;
    };
    let key = format!("stream-cancel-{}", Uuid::new_v4());
    let mut response_stream = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    assert!(matches!(
        response_stream.next().await,
        Some(Ok(ChatStreamEvent::ResponseStarted(_)))
    ));
    drop(response_stream);
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
    loop {
        let reservation = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
            .unwrap();
        let fallback_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.accounting_reconciliation WHERE reservation_id=$1 AND reason='provider-unknown'",
        )
        .bind(reservation.id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        if reservation.state == gateway::ReservationState::Unknown && fallback_count == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fallback settlement did not complete: state={:?}, fallbacks={fallback_count}",
            reservation.state,
        );
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn worker_recovery_marks_expired_claim_unknown_then_reconciles() {
    let _guard = database_guard();
    let release = Arc::new(tokio::sync::Notify::new());
    let Some((pool, service, identity)) = setup_with_ttl(
        std::sync::Arc::new(GatedProvider {
            release: release.clone(),
        }),
        10,
        // Long enough for reserve and claim on a slow runner; the claim
        // still expires well before the test gives up.
        StdDuration::from_millis(750),
    )
    .await
    else {
        return;
    };
    let key = format!("recovery-{}", Uuid::new_v4());
    let task_key = key.clone();
    let worker_service = service.clone();
    let worker_identity = identity.clone();
    let task = tokio::spawn(async move {
        worker_service
            .chat(
                &worker_identity,
                context(),
                request(),
                AccountedChatRequest::new(&task_key),
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    let reservation = loop {
        if task.is_finished() {
            panic!(
                "chat ended before its reservation was claimed: {:?}",
                task.await.unwrap().err()
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reservation was never claimed"
        );
        if let Some(reservation) = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
        {
            if reservation.state == gateway::ReservationState::Claimed {
                break reservation;
            }
        }
        tokio::time::sleep(StdDuration::from_millis(2)).await;
    };
    tokio::time::sleep(StdDuration::from_millis(800)).await;
    assert!(
        service
            .store()
            .recover_expired(Utc::now(), 100)
            .await
            .unwrap()
            >= 1
    );
    assert_eq!(
        service
            .store()
            .find_reservation(identity.organization_id, reservation.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        gateway::ReservationState::Unknown
    );
    release.notify_one();
    assert!(task.await.unwrap().is_ok());
    let status: String = sqlx::query_scalar(
        "SELECT status FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "resolved");
    let events = outbox_events(&pool, identity.organization_id).await;
    let summary: Vec<(&str, &str)> = events
        .iter()
        .map(|(kind, payload)| {
            let detail = match kind.as_str() {
                "inference.finalized" => payload["outcome"].as_str(),
                "reconciliation.opened" => payload["reason"].as_str(),
                _ => payload["resolution"].as_str(),
            };
            (kind.as_str(), detail.unwrap())
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("reconciliation.opened", "expired-after-claim"),
            ("inference.finalized", "unknown"),
            ("reconciliation.resolved", "settlement"),
            ("inference.finalized", "settled"),
        ]
    );
    assert_eq!(events[0].1["item_id"], events[2].1["item_id"]);
}

#[tokio::test]
async fn partial_provider_usage_is_appended_without_hard_charge() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(PartialFailureProvider), 10).await
    else {
        return;
    };
    let key = format!("partial-{}", Uuid::new_v4());
    assert!(
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(&key),
            )
            .await
            .is_err()
    );
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    let usage: (Option<i64>, String, String) = sqlx::query_as(
        "SELECT amount_minor,completeness,finality FROM gateway.accounting_usage WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage, (None, "partial".into(), "partial".into()));
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
}

#[tokio::test]
async fn a_failed_stream_whose_input_count_drifted_keeps_the_provider_error() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(PartialFailureProvider), 10).await
    else {
        return;
    };
    let key = format!("partial-stream-{}", Uuid::new_v4());
    let events = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.last().unwrap().is_err());
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    let trace: (
        String,
        Option<String>,
        Option<i16>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT outcome, provider_error_category, provider_error_status,
                    provider_error_code, provider_request_id
             FROM gateway.inference_trace WHERE id=$1",
    )
    .bind(reservation.trace_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        trace,
        (
            "input-count-drift".into(),
            Some("provider".into()),
            Some(500),
            Some("ModelStreamErrorException".into()),
            Some("req-stream-1".into())
        )
    );
}

#[tokio::test]
async fn embeddings_use_input_only_reservation_and_settle() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(EmbeddingProvider), 100).await
    else {
        return;
    };
    let key = format!("embedding-{}", Uuid::new_v4());
    let response = service
        .embeddings(
            &identity,
            context(),
            EmbeddingRequest {
                input: vec!["hello".into()],
            },
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    assert_eq!(response.data().len(), 1);
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Settled);
    assert!(reservation.output_token_cap.is_none());
    let output_cap: Option<i64> =
        sqlx::query_scalar("SELECT output_cap FROM gateway.inference_trace WHERE id=$1")
            .bind(reservation.trace_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(output_cap, None);
}

#[tokio::test]
async fn same_idempotency_key_is_isolated_between_service_accounts() {
    let _guard = database_guard();
    let Some((pool, service, first)) = setup().await else {
        return;
    };
    let owner: Uuid =
        sqlx::query_scalar("SELECT owner_user_id FROM gateway.service_account WHERE id=$1")
            .bind(first.service_account_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let identity_store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"accounting-test-pepper").unwrap(),
    );
    let second_account = identity_store
        .create_service_account(
            first.organization_id,
            gateway::UserAccountId(owner),
            None,
            "second-service",
        )
        .await
        .unwrap();
    let second = VerifiedIdentity {
        organization_id: first.organization_id,
        service_account_id: second_account.id,
        team_id: None,
        api_key_id: None,
        scopes: vec![],
        end_user: None,
    };
    let now = Utc::now();
    service
        .store()
        .create_quota_bucket(
            &QuotaScope {
                organization_id: second.organization_id,
                team_id: None,
                service_account_id: Some(second.service_account_id),
                api_key_id: None,
            },
            BudgetAmount::new(100).unwrap(),
            &Currency::new("CRD").unwrap(),
            now - Duration::hours(1),
            now + Duration::hours(1),
        )
        .await
        .unwrap();
    let key = format!("cross-account-{}", Uuid::new_v4());
    let left = service.clone();
    let right = service.clone();
    let (left, right) = tokio::join!(
        left.chat(
            &first,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        ),
        right.chat(
            &second,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        ),
    );
    assert!(left.is_ok());
    assert!(right.is_ok());
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation WHERE organization_id=$1 AND idempotency_key=$2",
    )
    .bind(first.organization_id.0)
    .bind(&key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

/// A cached-input price (auto-offer copies the input price there when the
/// source states none) charges nothing for embeddings, which cache no input:
/// the request settles instead of landing in reconciliation.
#[tokio::test]
async fn embeddings_settle_under_a_price_that_charges_cached_input() {
    let _guard = database_guard();
    let Some((_pool, service, identity)) = setup_with_ttl_and_price(
        std::sync::Arc::new(EmbeddingProvider),
        100,
        StdDuration::from_secs(60),
        PriceSnapshot::new(2, 0, 2).unwrap(),
    )
    .await
    else {
        return;
    };
    let key = format!("embedding-{}", Uuid::new_v4());
    service
        .embeddings(
            &identity,
            context(),
            EmbeddingRequest {
                input: vec!["hello".into()],
            },
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Settled);
}

#[tokio::test]
async fn embeddings_reject_negative_available_before_reserving() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(EmbeddingProvider), 10).await
    else {
        return;
    };
    sqlx::query(
        "UPDATE gateway.quota_bucket SET reserved_minor=limit_minor+1 WHERE organization_id=$1 AND service_account_id=$2",
    )
    .bind(identity.organization_id.0)
    .bind(identity.service_account_id.0)
    .execute(&pool)
    .await
    .unwrap();

    let key = format!("negative-embedding-{}", Uuid::new_v4());
    assert!(
        service
            .embeddings(
                &identity,
                context(),
                EmbeddingRequest {
                    input: vec!["hello".into()],
                },
                AccountedChatRequest::new(&key),
            )
            .await
            .is_err()
    );
    let reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation WHERE organization_id=$1 AND service_account_id=$2 AND idempotency_key=$3",
    )
    .bind(identity.organization_id.0)
    .bind(identity.service_account_id.0)
    .bind(&key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reservations, 0);
}

#[tokio::test]
async fn nullable_accounting_fields_are_immutable() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(EmbeddingProvider), 100).await
    else {
        return;
    };
    let key = format!("nullable-{}", Uuid::new_v4());
    service
        .embeddings(
            &identity,
            context(),
            EmbeddingRequest {
                input: vec!["hello".into()],
            },
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    let reservation_mutation =
        sqlx::query("UPDATE gateway.accounting_reservation SET output_token_cap=1 WHERE id=$1")
            .bind(reservation.id.0)
            .execute(&pool)
            .await;
    assert!(reservation_mutation.is_err());
    let trace_mutation = sqlx::query("UPDATE gateway.inference_trace SET output_cap=1 WHERE id=$1")
        .bind(reservation.trace_id.0)
        .execute(&pool)
        .await;
    assert!(trace_mutation.is_err());

    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 100).await
    else {
        return;
    };
    let key = format!("nullable-chat-{}", Uuid::new_v4());
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    let reservation_mutation =
        sqlx::query("UPDATE gateway.accounting_reservation SET output_token_cap=NULL WHERE id=$1")
            .bind(reservation.id.0)
            .execute(&pool)
            .await;
    assert!(reservation_mutation.is_err());
    let trace_mutation =
        sqlx::query("UPDATE gateway.inference_trace SET output_cap=NULL WHERE id=$1")
            .bind(reservation.trace_id.0)
            .execute(&pool)
            .await;
    assert!(trace_mutation.is_err());
}

/// Holds every chat call in the provider until released, so its reservation
/// stays claimed.
struct BlockingProvider {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for BlockingProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.entered.notify_one();
        self.release.notified().await;
        SuccessfulProvider.chat(context, request).await
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

/// Replaces the policy of one scope; `None` limits are unlimited.
async fn set_rate_limit(
    pool: &PgPool,
    organization: gateway::OrganizationId,
    team: Option<gateway::TeamId>,
    service_account: Option<gateway::ServiceAccountId>,
    requests_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
) {
    let kind = match (team, service_account) {
        (_, Some(_)) => "service-account",
        (Some(_), None) => "team",
        (None, None) => "organization",
    };
    sqlx::query(
        "DELETE FROM gateway.rate_limit_policy
         WHERE organization_id = $1 AND scope_kind = $2
           AND team_id IS NOT DISTINCT FROM $3 AND service_account_id IS NOT DISTINCT FROM $4",
    )
    .bind(organization.0)
    .bind(kind)
    .bind(team.map(|team| team.0))
    .bind(service_account.map(|account| account.0))
    .execute(pool)
    .await
    .unwrap();
    if requests_per_minute.is_none() && max_concurrent.is_none() {
        return;
    }
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, team_id, service_account_id,
              requests_per_minute, max_concurrent)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::new_v4())
    .bind(organization.0)
    .bind(kind)
    .bind(team.map(|team| team.0))
    .bind(service_account.map(|account| account.0))
    .bind(requests_per_minute)
    .bind(max_concurrent)
    .execute(pool)
    .await
    .unwrap();
}

fn rate_limited(
    result: &Result<gateway::domain::inference::ChatResponse, AccountingServiceError>,
) -> Option<gateway::persistence::rate_limits::RateLimitBreach> {
    match result {
        Err(AccountingServiceError::Store(gateway::AccountingStoreError::RateLimited(breach))) => {
            Some(*breach)
        }
        _ => None,
    }
}

async fn reservations(pool: &PgPool, organization: gateway::OrganizationId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation WHERE organization_id=$1",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn rate_limits_count_new_reservations_per_scope_and_never_replays() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 1000).await
    else {
        return;
    };
    let org = identity.organization_id;
    let chat = |key: &'static str, identity: VerifiedIdentity| {
        let service = service.clone();
        async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new(key),
                )
                .await
        }
    };

    set_rate_limit(
        &pool,
        org,
        None,
        Some(identity.service_account_id),
        Some(2),
        None,
    )
    .await;
    assert!(chat("rate-1", identity.clone()).await.is_ok());
    assert!(chat("rate-2", identity.clone()).await.is_ok());
    let breach = rate_limited(&chat("rate-3", identity.clone()).await).expect("third is limited");
    assert_eq!(breach.kind, RateLimitKind::RequestsPerMinute);
    assert!((1..=60).contains(&breach.retry_after_seconds), "{breach:?}");
    assert_eq!(
        reservations(&pool, org).await,
        2,
        "a rejection reserves nothing"
    );
    // A retry of an admitted request replays its reservation; it is never
    // rate limited, whatever the replay itself returns.
    assert!(rate_limited(&chat("rate-1", identity.clone()).await).is_none());

    // The organization's policy counts every reservation of the tenant.
    set_rate_limit(
        &pool,
        org,
        None,
        Some(identity.service_account_id),
        None,
        None,
    )
    .await;
    set_rate_limit(&pool, org, None, None, Some(3), None).await;
    assert!(chat("rate-3", identity.clone()).await.is_ok());
    assert!(rate_limited(&chat("rate-4", identity.clone()).await).is_some());

    // A team's policy covers its service accounts and nobody else.
    set_rate_limit(&pool, org, None, None, None, None).await;
    let identities = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"accounting-test-pepper").unwrap(),
    );
    let team = identities.create_team(org, "limited").await.unwrap().id;
    let user = identities
        .create_user(org, None, None, None, Some("team-owner"))
        .await
        .unwrap()
        .id;
    identities
        .add_organization_membership(org, user, MembershipRole::Member)
        .await
        .unwrap();
    identities
        .add_team_membership(org, team, user, MembershipRole::Admin)
        .await
        .unwrap();
    let team_account = identities
        .create_service_account(org, user, Some(team), "team-service")
        .await
        .unwrap();
    let now = Utc::now();
    AccountingStore::new(pool.clone())
        .create_quota_bucket(
            &QuotaScope::organization(org),
            BudgetAmount::new(1000).unwrap(),
            &Currency::new("CRD").unwrap(),
            now - Duration::hours(1),
            now + Duration::hours(1),
        )
        .await
        .unwrap();
    let team_identity = VerifiedIdentity {
        organization_id: org,
        service_account_id: team_account.id,
        team_id: Some(team),
        api_key_id: None,
        scopes: vec![],
        end_user: None,
    };
    set_rate_limit(&pool, org, Some(team), None, Some(1), None).await;
    assert!(chat("team-1", team_identity.clone()).await.is_ok());
    assert!(rate_limited(&chat("team-2", team_identity.clone()).await).is_some());
    assert!(chat("rate-5", identity.clone()).await.is_ok());
}

#[tokio::test]
async fn concurrency_limits_count_in_flight_reservations_until_they_finish() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = BlockingProvider {
        entered: entered.clone(),
        release: release.clone(),
    };
    let Some((pool, service, identity)) = setup_with(std::sync::Arc::new(provider), 1000).await
    else {
        return;
    };
    set_rate_limit(
        &pool,
        identity.organization_id,
        None,
        Some(identity.service_account_id),
        None,
        Some(1),
    )
    .await;

    let first = {
        let service = service.clone();
        let identity = identity.clone();
        tokio::spawn(async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new("slot-1"),
                )
                .await
        })
    };
    entered.notified().await;
    let second = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("slot-2"),
        )
        .await;
    let breach = rate_limited(&second).expect("a second in-flight request is limited");
    assert_eq!(breach.kind, RateLimitKind::Concurrency);
    assert_eq!(breach.retry_after_seconds, 1);

    release.notify_one();
    assert!(first.await.unwrap().is_ok());
    // The finished request frees its slot.
    release.notify_one();
    assert!(
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new("slot-2")
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn admission_and_policy_writes_lock_in_the_same_order() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 1000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(100), None).await;

    // An admin write locks the organization, then the policy it changes.
    let mut admin = pool.begin().await.unwrap();
    let admin_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *admin)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gateway.organization WHERE id = $1 FOR UPDATE")
        .bind(org.0)
        .execute(&mut *admin)
        .await
        .unwrap();
    let chat = {
        let service = service.clone();
        let identity = identity.clone();
        tokio::spawn(async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new("lock-order"),
                )
                .await
        })
    };
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    loop {
        // Only this admission can be blocked by the admin transaction.
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE $1 = ANY(pg_blocking_pids(pid))",
        )
        .bind(admin_pid)
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            !chat.is_finished(),
            "admission did not wait for the admin write"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "admission never waited"
        );
        tokio::time::sleep(StdDuration::from_millis(5)).await;
    }
    // Admission waits before taking the policy, so the admin write gets it
    // without a deadlock, and admission proceeds once the write commits.
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *admin)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gateway.rate_limit_policy WHERE organization_id = $1 FOR UPDATE")
        .bind(org.0)
        .execute(&mut *admin)
        .await
        .expect("the admin write takes the policy lock");
    admin.commit().await.unwrap();
    assert!(chat.await.unwrap().is_ok());
    assert_eq!(reservations(&pool, org).await, 1);
}

#[tokio::test]
async fn retried_finalizations_replay_without_writing_twice() {
    let _guard = database_guard();
    let release = Arc::new(tokio::sync::Notify::new());
    let Some((pool, service, identity)) = setup_with_ttl(
        std::sync::Arc::new(GatedProvider {
            release: release.clone(),
        }),
        10,
        StdDuration::from_secs(60),
    )
    .await
    else {
        return;
    };
    let key = format!("finalization-replay-{}", Uuid::new_v4());
    let task_key = key.clone();
    let task_service = service.clone();
    let task_identity = identity.clone();
    let task = tokio::spawn(async move {
        task_service
            .chat(
                &task_identity,
                context(),
                request(),
                AccountedChatRequest::new(&task_key),
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    let reservation = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "reservation was never claimed"
        );
        if let Some(reservation) = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
        {
            if reservation.state == gateway::ReservationState::Claimed {
                break reservation;
            }
        }
        tokio::time::sleep(StdDuration::from_millis(2)).await;
    };
    // The request never finalizes; this test finalizes the reservation.
    task.abort();

    let store = service.store();
    let (org, id, trace) = (
        identity.organization_id,
        reservation.id,
        reservation.trace_id,
    );
    let snapshot = || async {
        let usage: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.accounting_usage WHERE reservation_id=$1",
        )
        .bind(id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        let consumed: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(consumed_minor), 0)::BIGINT FROM gateway.quota_bucket WHERE organization_id=$1",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        (usage, consumed, outbox_events(&pool, org).await.len())
    };
    let settlement = |finalization_id: Uuid, usage: Option<Usage>| Settlement {
        finalization_id,
        usage,
        trace_id: trace,
        provider_request_id: None,
        provider_error: None,
        timings: Default::default(),
    };

    // Input-count drift: the replay writes no second usage row.
    let drift_id = Uuid::new_v4();
    let observed = Usage::complete(
        TokenCounter::exact(9),
        TokenCounter::exact(1),
        None,
        None,
        None,
    )
    .unwrap();
    store
        .reconcile_input_drift(org, id, drift_id, trace, Some(observed.clone()), None, None)
        .await
        .unwrap();
    let after_drift = snapshot().await;
    assert_eq!(after_drift.0, 1);
    store
        .reconcile_input_drift(org, id, drift_id, trace, Some(observed), None, None)
        .await
        .unwrap();
    assert_eq!(snapshot().await, after_drift);

    // Partial usage on an unknown reservation: the replay adds nothing.
    let partial_id = Uuid::new_v4();
    let partial = Usage::partial(Some(TokenCounter::exact(1)), None, None, None, None);
    for _ in 0..2 {
        assert_eq!(
            store
                .settle(
                    org,
                    id,
                    settlement(partial_id, Some(partial.clone())),
                    Utc::now()
                )
                .await
                .unwrap(),
            gateway::ReservationState::Unknown
        );
    }
    let after_partial = snapshot().await;
    assert_eq!(after_partial.0, 2);

    // Exact usage settles once: quota, usage and events are not repeated.
    let exact_id = Uuid::new_v4();
    let exact = Usage::complete(
        TokenCounter::exact(1),
        TokenCounter::exact(1),
        None,
        None,
        None,
    )
    .unwrap();
    store
        .settle(
            org,
            id,
            settlement(exact_id, Some(exact.clone())),
            Utc::now(),
        )
        .await
        .unwrap();
    let settled = snapshot().await;
    assert_eq!(settled.0, 3);
    assert!(settled.1 > after_partial.1);
    for replay in [
        settlement(exact_id, Some(exact)),
        // A replay returns the recorded outcome even when retried with
        // different usage, since only the first attempt's inputs committed.
        settlement(exact_id, None),
    ] {
        assert_eq!(
            store.settle(org, id, replay, Utc::now()).await.unwrap(),
            gateway::ReservationState::Settled
        );
    }
    assert_eq!(snapshot().await, settled);
    // Earlier finalizations replay their own outcome after settlement.
    assert_eq!(
        store
            .settle(org, id, settlement(partial_id, Some(partial)), Utc::now())
            .await
            .unwrap(),
        gateway::ReservationState::Unknown
    );
    store
        .reconcile_input_drift(org, id, drift_id, trace, None, None, None)
        .await
        .unwrap();
    assert_eq!(snapshot().await, settled);

    let outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome FROM gateway.accounting_finalization WHERE reservation_id=$1 ORDER BY created_at, outcome",
    )
    .bind(id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(
        service
            .store()
            .sweep_finalizations(Utc::now() + chrono::Duration::seconds(1), 1_000)
            .await
            .unwrap()
            >= 3
    );
    release.notify_one();
}

type RecoveredClaim = (
    PgPool,
    AccountingService,
    gateway::OrganizationId,
    gateway::ReservationId,
    gateway::TraceId,
    Arc<tokio::sync::Notify>,
);

/// A chat request whose reservation is claimed and then recovered by expiry,
/// which opens an `expired-after-claim` item. The request never finalizes;
/// the caller does, then notifies the returned gate.
async fn claimed_then_recovered() -> Option<RecoveredClaim> {
    let release = Arc::new(tokio::sync::Notify::new());
    let (pool, service, identity) = setup_with_ttl(
        std::sync::Arc::new(GatedProvider {
            release: release.clone(),
        }),
        10,
        StdDuration::from_secs(60),
    )
    .await?;
    let key = format!("late-reason-{}", Uuid::new_v4());
    let task_key = key.clone();
    let task_service = service.clone();
    let task_identity = identity.clone();
    let task = tokio::spawn(async move {
        task_service
            .chat(
                &task_identity,
                context(),
                request(),
                AccountedChatRequest::new(&task_key),
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    let reservation = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "reservation was never claimed"
        );
        if let Some(reservation) = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
        {
            if reservation.state == gateway::ReservationState::Claimed {
                break reservation;
            }
        }
        tokio::time::sleep(StdDuration::from_millis(2)).await;
    };
    task.abort();
    // Recovery runs as if the reservation's 60-second lease had lapsed.
    assert!(
        service
            .store()
            .recover_expired(Utc::now() + chrono::Duration::minutes(2), 100)
            .await
            .unwrap()
            >= 1
    );
    Some((
        pool,
        service,
        identity.organization_id,
        reservation.id,
        reservation.trace_id,
        release,
    ))
}

async fn reconciliation_item(pool: &PgPool, id: gateway::ReservationId) -> (Uuid, String, String) {
    sqlx::query_as(
        "SELECT id, reason, status FROM gateway.accounting_reconciliation
         WHERE reservation_id = $1",
    )
    .bind(id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn reconciliation_events(
    pool: &PgPool,
    org: gateway::OrganizationId,
) -> Vec<(String, serde_json::Value)> {
    outbox_events(pool, org)
        .await
        .into_iter()
        .filter(|(kind, _)| kind.starts_with("reconciliation."))
        .collect()
}

#[tokio::test]
async fn a_late_finalization_updates_the_reason_of_an_item_opened_by_recovery() {
    let _guard = database_guard();
    let Some((pool, service, org, id, trace, release)) = claimed_then_recovered().await else {
        return;
    };
    let store = service.store();
    let item = || reconciliation_item(&pool, id);
    let (item_id, reason, _) = item().await;
    assert_eq!(reason, "expired-after-claim");

    // The late drift names the real cause; its replay emits nothing more.
    let drift_id = Uuid::new_v4();
    for _ in 0..2 {
        store
            .reconcile_input_drift(org, id, drift_id, trace, None, None, None)
            .await
            .unwrap();
    }
    assert_eq!(
        item().await,
        (item_id, "input-count-drift".to_owned(), "open".to_owned())
    );
    // Only the recovery reason is replaced: a later unknown outcome keeps
    // the drift.
    let settlement = Settlement {
        finalization_id: Uuid::new_v4(),
        usage: None,
        trace_id: trace,
        provider_request_id: None,
        provider_error: None,
        timings: Default::default(),
    };
    assert_eq!(
        store.settle(org, id, settlement, Utc::now()).await.unwrap(),
        gateway::ReservationState::Unknown
    );
    assert_eq!(item().await.1, "input-count-drift");

    let events = reconciliation_events(&pool, org).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].0, "reconciliation.opened");
    assert_eq!(events[1].0, "reconciliation.updated");
    let updated = &events[1].1;
    assert_eq!(updated["item_id"], serde_json::json!(item_id));
    assert_eq!(updated["reservation_id"], serde_json::json!(id.0));
    assert_eq!(updated["previous_reason"], "expired-after-claim");
    assert_eq!(updated["reason"], "input-count-drift");
    assert!(updated["updated_at"].is_string());
    release.notify_one();
}

#[tokio::test]
async fn a_late_unknown_settlement_updates_the_reason_of_an_item_opened_by_recovery() {
    let _guard = database_guard();
    let Some((pool, service, org, id, trace, release)) = claimed_then_recovered().await else {
        return;
    };
    let (item_id, reason, _) = reconciliation_item(&pool, id).await;
    assert_eq!(reason, "expired-after-claim");

    let settlement = Settlement {
        finalization_id: Uuid::new_v4(),
        usage: None,
        trace_id: trace,
        provider_request_id: None,
        provider_error: None,
        timings: Default::default(),
    };
    for _ in 0..2 {
        assert_eq!(
            service
                .store()
                .settle(org, id, settlement.clone(), Utc::now())
                .await
                .unwrap(),
            gateway::ReservationState::Unknown
        );
    }
    assert_eq!(
        reconciliation_item(&pool, id).await,
        (item_id, "provider-unknown".to_owned(), "open".to_owned())
    );
    let events = reconciliation_events(&pool, org).await;
    let kinds: Vec<_> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds, ["reconciliation.opened", "reconciliation.updated"]);
    assert_eq!(events[1].1["previous_reason"], "expired-after-claim");
    assert_eq!(events[1].1["reason"], "provider-unknown");
    release.notify_one();
}

#[tokio::test]
async fn stream_settlement_retries_in_the_background_after_a_transient_failure() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(std::sync::Arc::new(StreamingProvider), 10).await
    else {
        return;
    };
    // A one-connection pool that the test can exhaust, so settlement fails
    // with a pool timeout, a transient failure.
    let small = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(StdDuration::from_millis(200))
        .connect(&env::var("GATEWAY_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    let policy = AccountingPolicy::new(
        1024,
        std::num::NonZeroU32::new(4).unwrap(),
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 1, 0).unwrap(),
        Currency::new("CRD").unwrap(),
        [9; 32],
    )
    .unwrap()
    .with_chat_input_count_mode(ChatInputCountMode::ProviderApiExact);
    let service = AccountingService::new(
        AccountingStore::new(small.clone()),
        std::sync::Arc::new(StreamingProvider),
        policy,
    );
    let key = format!("stream-retry-{}", Uuid::new_v4());
    let response_stream = service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    let held = small.acquire().await.unwrap();
    let events: Vec<_> = response_stream.collect().await;
    assert!(matches!(events.last(), Some(Err(_))), "{events:?}");
    // The stream is dropped; releasing the connection lets the background
    // retry settle it.
    drop(held);
    let store = AccountingStore::new(pool.clone());
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    let reservation = loop {
        let reservation = store
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
            .unwrap();
        if reservation.state == gateway::ReservationState::Settled {
            break reservation;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "settlement was never retried: {:?}",
            reservation.state
        );
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    };
    // Settled exactly once, and never finalized as unknown by the drop.
    let usage: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.accounting_usage WHERE reservation_id=$1")
            .bind(reservation.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(usage, 1);
    let reconciliation: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reconciliation WHERE reservation_id=$1",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reconciliation, 0);
    let events = outbox_events(&pool, identity.organization_id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "inference.finalized");
    assert_eq!(events[0].1["outcome"], "settled");
}

fn quota_exceeded(
    result: &Result<gateway::domain::inference::ChatResponse, AccountingServiceError>,
) -> bool {
    matches!(
        result,
        Err(AccountingServiceError::Store(
            gateway::AccountingStoreError::QuotaExceeded
        ))
    )
}

fn no_budget(
    result: &Result<gateway::domain::inference::ChatResponse, AccountingServiceError>,
) -> bool {
    matches!(
        result,
        Err(AccountingServiceError::Store(
            gateway::AccountingStoreError::NoBudget
        ))
    )
}

#[tokio::test]
async fn budgets_admit_requests_through_their_current_window() {
    use gateway::domain::budget::{BudgetInput, BudgetPeriod, BudgetScope};
    use gateway::persistence::budgets::BudgetStore;

    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 1_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    let chat = |key: &'static str| {
        let service = service.clone();
        let identity = identity.clone();
        async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new(key),
                )
                .await
        }
    };
    // Without the setup's own bucket, no budget covers the request.
    sqlx::query("UPDATE gateway.quota_bucket SET retired_at = now() WHERE organization_id = $1")
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    assert!(no_budget(&chat("budget-none").await));

    // A monthly organization budget: its month's bucket is created by the
    // first request that needs it.
    let monthly = |limit_minor| BudgetInput {
        currency: "CRD".into(),
        limit_minor,
        period: BudgetPeriod::Monthly,
        starts_at: None,
        ends_at: None,
    };
    let actor = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"accounting-test-pepper").unwrap(),
    )
    .create_user(org, None, None, None, Some("budget-admin"))
    .await
    .unwrap()
    .id;
    let policy_id = Uuid::new_v4();
    let set = |input: BudgetInput| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            BudgetStore::upsert(
                &mut tx,
                org,
                policy_id,
                BudgetScope::Organization,
                &input,
                actor,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
    };
    set(monthly(1000)).await;
    assert!(chat("budget-1").await.is_ok());
    let month = gateway::domain::budget::month_of(Utc::now());
    let (consumed, period_start, period_end): (i64, chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
        sqlx::query_as(
            "SELECT consumed_minor, period_start, period_end FROM gateway.quota_bucket
             WHERE organization_id = $1 AND scope_kind = 'organization' AND retired_at IS NULL",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(consumed > 0);
    assert_eq!((period_start, period_end), month);

    // Lowering the limit applies at once and keeps what was spent.
    let apply = |input: BudgetInput| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            BudgetStore::upsert(
                &mut tx,
                org,
                policy_id,
                BudgetScope::Organization,
                &input,
                actor,
            )
            .await
            .unwrap();
            BudgetStore::apply(&mut tx, org, BudgetScope::Organization, &input, Utc::now())
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    apply(monthly(consumed)).await;
    assert!(quota_exceeded(&chat("budget-2").await));
    apply(monthly(1000)).await;
    assert!(chat("budget-3").await.is_ok());
    let buckets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'organization'",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buckets, 1, "one bucket per window, whatever the limit");

    // Removed, the budget covers nothing new.
    let mut tx = pool.begin().await.unwrap();
    let stored = BudgetStore::view(&mut tx, org, policy_id, Utc::now())
        .await
        .unwrap()
        .unwrap();
    assert!(stored.current.as_ref().unwrap().consumed_minor > consumed);
    BudgetStore::retire_scope(&mut tx, org, BudgetScope::Organization, &stored, Utc::now())
        .await
        .unwrap();
    assert!(BudgetStore::delete(&mut tx, org, policy_id).await.unwrap());
    tx.commit().await.unwrap();
    assert!(no_budget(&chat("budget-4").await));

    // A budget in another currency covers nothing in this one.
    set(BudgetInput {
        currency: "USD".into(),
        ..monthly(1_000_000)
    })
    .await;
    assert!(no_budget(&chat("budget-5").await));
}

/// Answers "hi there", in one piece or streamed in two.
struct TracedProvider;

#[async_trait::async_trait]
impl InferenceProviderAdapter for TracedProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(2),
            None,
            None,
            None,
        )
        .unwrap();
        Ok(ChatResponse::new(
            None,
            "model".into(),
            vec![gateway::domain::inference::ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: "hi there".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                finish_reason: Some(FinishReason::new("stop")),
                tool_calls: Vec::new(),
                refusal: None,
                safety: Vec::new(),
                annotations: Vec::new(),
            }],
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
        let usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(2),
            None,
            None,
            None,
        )
        .unwrap();
        let delta = |text: &str| {
            Ok(ChatStreamEvent::ChoiceDelta(
                gateway::domain::inference::StreamChoiceDelta {
                    choice_index: 0,
                    content_delta: Some(text.into()),
                    refusal_delta: None,
                    annotations: Vec::new(),
                },
            ))
        };
        Ok(ValidatingChatEventStream::new(stream::iter(vec![
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model".into(), SafeMetadata::default()).unwrap(),
            )),
            delta("hi "),
            delta("there"),
            Ok(ChatStreamEvent::ChoiceFinished(
                ChoiceFinished::new(0, FinishReason::new("stop")).unwrap(),
            )),
            Ok(ChatStreamEvent::ResponseCompleted(
                ResponseCompletion::new(
                    None,
                    "model".into(),
                    Some(UsageSnapshot::final_(usage).unwrap()),
                )
                .unwrap(),
            )),
        ])))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Unavailable))
    }
}

#[derive(sqlx::FromRow)]
struct ContentRow {
    trace_id: Uuid,
    state: String,
    object_name: Option<String>,
    key_id: Option<String>,
    sealed_key: Option<Vec<u8>>,
    provider_started_at: Option<chrono::DateTime<Utc>>,
    first_event_at: Option<chrono::DateTime<Utc>>,
}

async fn content_rows(pool: &PgPool, organization: gateway::OrganizationId) -> Vec<ContentRow> {
    sqlx::query_as(
        "SELECT trace_id, state, object_name, key_id, sealed_key, provider_started_at,
                first_event_at
         FROM gateway.inference_trace_content WHERE organization_id = $1
         ORDER BY started_at",
    )
    .bind(organization.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The rows once `count` are recorded; capture writes in the background.
async fn wait_for_content(
    pool: &PgPool,
    organization: gateway::OrganizationId,
    count: usize,
) -> Vec<ContentRow> {
    for _ in 0..100 {
        // Rows still being written are not yet counted.
        let rows = content_rows(pool, organization).await;
        if rows.iter().filter(|row| row.state != "writing").count() >= count {
            return rows;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    panic!("trace content was not recorded");
}

#[tokio::test]
async fn traced_requests_keep_encrypted_content_until_it_expires() {
    use gateway::{
        application::{
            trace_capture::TraceCapture,
            trace_objects::{MemoryTraceObjects, TraceObjectError, TraceObjects},
        },
        domain::{
            credentials::CredentialCipher,
            trace_content::{DEFAULT_MAX_BYTES, TraceDocument, content_aad, open_content},
        },
        persistence::{trace_content::TraceContentStore, trace_policy::TracePolicyStore},
    };

    let _guard = database_guard();
    let Some((pool, service, identity)) = setup_with(Arc::new(TracedProvider), 100).await else {
        return;
    };
    let org = identity.organization_id;
    let objects = Arc::new(MemoryTraceObjects::default());
    let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
    let content = TraceContentStore::new(pool.clone());
    let capture = |objects: Arc<MemoryTraceObjects>| {
        TraceCapture::new(
            TracePolicyStore::new(pool.clone()),
            content.clone(),
            Some(objects as Arc<dyn TraceObjects>),
            Some(cipher.clone()),
            DEFAULT_MAX_BYTES,
        )
    };

    // Tracing off: nothing is recorded.
    let untraced = service.clone().with_capture(capture(objects.clone()));
    untraced
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("untraced"),
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.organization_trace_policy (organization_id, id, mode)
         VALUES ($1, gen_random_uuid(), 'organization')",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    // A new capture: the first read the policy before it was set.
    let traced = service.with_capture(capture(objects.clone()));
    traced
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("traced-chat"),
        )
        .await
        .unwrap();
    let events = traced
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("traced-stream"),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));

    let rows = wait_for_content(&pool, org, 2).await;
    assert_eq!(rows.len(), 2, "the untraced request has no row");
    for (row, streamed) in rows.iter().zip([false, true]) {
        assert_eq!(row.state, "stored");
        assert!(row.provider_started_at.is_some());
        assert_eq!(row.first_event_at.is_some(), streamed);
        let name = row.object_name.as_deref().unwrap();
        assert_eq!(name, format!("{}/{}", org.0, row.trace_id));
        let sealed = objects.object(name).unwrap();
        assert!(!sealed.windows(5).any(|window| window == b"hello"));
        let key = cipher
            .open_trace_content_key(
                org,
                row.trace_id,
                row.key_id.as_deref().unwrap(),
                row.sealed_key.as_deref().unwrap(),
            )
            .unwrap();
        let plaintext = open_content(&key, &content_aad(org, row.trace_id), &sealed).unwrap();
        let document: TraceDocument = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(
            document.request,
            serde_json::json!({
                "messages": [{"role": "user", "content": "hello"}],
                "max_tokens": 4,
            })
        );
        assert_eq!(
            document.response,
            serde_json::json!({
                "model": "model",
                "choices": [{"index": 0, "content": "hi there", "finish_reason": "stop"}],
            })
        );
    }
    let (chat, stream) = (rows[0].trace_id, rows[1].trace_id);

    // Content past its 30 days goes; its record stays.
    sqlx::query(
        "UPDATE gateway.inference_trace_content
         SET recorded_at = recorded_at - interval '31 days' WHERE trace_id = $1",
    )
    .bind(chat)
    .execute(&pool)
    .await
    .unwrap();
    let as_objects: Arc<dyn TraceObjects> = objects.clone();
    let report = gateway::jobs::trace_content::expire_organization_content(
        &content,
        &as_objects,
        org.0,
        Utc::now(),
    )
    .await;
    assert_eq!((report.expired, report.failed), (1, 0));
    let rows = content_rows(&pool, org).await;
    assert_eq!(
        rows.iter()
            .map(|row| (row.trace_id, row.state.as_str(), row.sealed_key.is_some()))
            .collect::<Vec<_>>(),
        [(chat, "expired", false), (stream, "stored", true)]
    );
    assert_eq!(objects.names(), [format!("{}/{stream}", org.0)]);
    let purged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'retention.trace_content_purged'",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(purged, 1);

    // A record past its retention period waits for its content to expire
    // first, which that period also brings forward.
    sqlx::query(
        "INSERT INTO gateway.organization_retention_policy (organization_id, id, trace_days)
         VALUES ($1, gen_random_uuid(), 7)",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE gateway.inference_trace SET recorded_at = recorded_at - interval '8 days'
         WHERE id = $1",
    )
    .bind(stream)
    .execute(&pool)
    .await
    .unwrap();
    let retention = gateway::persistence::retention::RetentionStore::new(pool.clone());
    gateway::jobs::retention::purge_expired(&retention, Utc::now())
        .await
        .unwrap();
    let kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.inference_trace WHERE id = $1")
            .bind(stream)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept, 1, "a record with stored content is kept");
    gateway::jobs::trace_content::expire_organization_content(
        &content,
        &as_objects,
        org.0,
        Utc::now(),
    )
    .await;
    assert!(objects.names().is_empty());
    gateway::jobs::retention::purge_expired(&retention, Utc::now())
        .await
        .unwrap();
    let left: Vec<Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM gateway.inference_trace_content WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(left, [chat], "the purged record took its content row");

    // A write whose outcome is unknown (the store failed, and so did the
    // delete) leaves its row `writing`, naming the object; the worker
    // removes whatever landed once the write is stale.
    *objects.failing.lock().unwrap() = Some(TraceObjectError::Unreachable);
    traced
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("traced-unconfirmed"),
        )
        .await
        .unwrap();
    let mut unconfirmed = None;
    for _ in 0..100 {
        unconfirmed = content_rows(&pool, org)
            .await
            .into_iter()
            .find(|row| row.trace_id != chat);
        if unconfirmed.is_some() {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    let unconfirmed = unconfirmed.expect("the unconfirmed write has a row");
    assert_eq!(unconfirmed.state, "writing");
    let name = unconfirmed.object_name.clone().unwrap();
    *objects.failing.lock().unwrap() = None;
    // As if the upload had landed before its reply was lost.
    objects.put(&name, b"sealed".to_vec()).await.unwrap();
    gateway::jobs::trace_content::expire_organization_content(
        &content,
        &as_objects,
        org.0,
        Utc::now(),
    )
    .await;
    assert_eq!(
        objects.names(),
        std::slice::from_ref(&name),
        "a fresh write is left alone"
    );
    sqlx::query(
        "UPDATE gateway.inference_trace_content
         SET recorded_at = recorded_at - interval '2 hours' WHERE trace_id = $1",
    )
    .bind(unconfirmed.trace_id)
    .execute(&pool)
    .await
    .unwrap();
    gateway::jobs::trace_content::expire_organization_content(
        &content,
        &as_objects,
        org.0,
        Utc::now(),
    )
    .await;
    assert!(objects.names().is_empty());
    let row = content_rows(&pool, org)
        .await
        .into_iter()
        .find(|row| row.trace_id == unconfirmed.trace_id)
        .unwrap();
    assert_eq!((row.state.as_str(), row.object_name), ("failed", None));
}

#[tokio::test]
async fn traced_embeddings_keep_their_input_and_each_vectors_start() {
    use gateway::{
        application::{
            trace_capture::TraceCapture,
            trace_objects::{MemoryTraceObjects, TraceObjects},
        },
        domain::{
            credentials::CredentialCipher,
            trace_content::{DEFAULT_MAX_BYTES, TraceDocument, content_aad, open_content},
        },
        persistence::{trace_content::TraceContentStore, trace_policy::TracePolicyStore},
    };

    let _guard = database_guard();
    let Some((pool, service, identity)) = setup_with(Arc::new(EmbeddingProvider), 100).await else {
        return;
    };
    // The provider that fails embeddings serves another organization.
    let (_, failing, failing_identity) = setup_with(Arc::new(TracedProvider), 100).await.unwrap();
    for org in [identity.organization_id, failing_identity.organization_id] {
        sqlx::query(
            "INSERT INTO gateway.organization_trace_policy (organization_id, id, mode)
             VALUES ($1, gen_random_uuid(), 'organization')",
        )
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    }
    let objects = Arc::new(MemoryTraceObjects::default());
    let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
    let capture = TraceCapture::new(
        TracePolicyStore::new(pool.clone()),
        TraceContentStore::new(pool.clone()),
        Some(objects.clone() as Arc<dyn TraceObjects>),
        Some(cipher.clone()),
        DEFAULT_MAX_BYTES,
    );
    let input = || EmbeddingRequest {
        input: vec!["hello".into()],
    };
    service
        .clone()
        .with_capture(capture.clone())
        .embeddings(
            &identity,
            context(),
            input(),
            AccountedChatRequest::new("traced-embedding"),
        )
        .await
        .unwrap();
    // A request the provider fails keeps what was sent.
    failing
        .with_capture(capture)
        .embeddings(
            &failing_identity,
            context(),
            input(),
            AccountedChatRequest::new("failed-embedding"),
        )
        .await
        .unwrap_err();

    let mut rows = wait_for_content(&pool, identity.organization_id, 1).await;
    rows.extend(wait_for_content(&pool, failing_identity.organization_id, 1).await);
    let documents: Vec<TraceDocument> = rows
        .iter()
        .zip([identity.organization_id, failing_identity.organization_id])
        .map(|(row, org)| {
            assert_eq!(row.state, "stored");
            assert!(row.provider_started_at.is_some());
            assert!(row.first_event_at.is_none());
            let sealed = objects.object(row.object_name.as_deref().unwrap()).unwrap();
            let key = cipher
                .open_trace_content_key(
                    org,
                    row.trace_id,
                    row.key_id.as_deref().unwrap(),
                    row.sealed_key.as_deref().unwrap(),
                )
                .unwrap();
            let plaintext = open_content(&key, &content_aad(org, row.trace_id), &sealed).unwrap();
            serde_json::from_slice(&plaintext).unwrap()
        })
        .collect();
    for document in &documents {
        assert_eq!(document.request, serde_json::json!({"input": ["hello"]}));
    }
    assert_eq!(
        documents[0].response,
        serde_json::json!({
            "model": "embedding-model",
            "embeddings": [{"index": 0, "dimensions": 2, "preview": [0.1_f32, 0.2_f32]}],
        })
    );
    assert_eq!(documents[1].response, serde_json::Value::Null);
}

#[tokio::test]
async fn requests_record_latency_and_finish_reason_on_their_trace() {
    let _guard = database_guard();
    let provider = Arc::new(CountingProvider {
        count_calls: Arc::new(AtomicUsize::new(0)),
        generation_calls: Arc::new(AtomicUsize::new(0)),
        fail_count: false,
        reported_input_tokens: 1,
        reported_cache_writes: 0,
        order: Arc::new(Mutex::new(Vec::new())),
        observation: Arc::new(Mutex::new(None)),
        claimed_seen: Arc::new(AtomicUsize::new(0)),
    });
    let Some((pool, service, identity)) = setup_with(provider.clone(), 100).await else {
        return;
    };
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("timed-chat"),
        )
        .await
        .unwrap();
    service
        .chat_stream(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("timed-stream"),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;

    // Timings are written in the background, off the request's path.
    let mut rows: Vec<(Option<i32>, Option<i32>, Option<String>)> = Vec::new();
    for _ in 0..100 {
        rows = sqlx::query_as(
            "SELECT provider_latency_ms, first_event_ms, finish_reason
             FROM gateway.inference_trace WHERE organization_id=$1 ORDER BY recorded_at",
        )
        .bind(identity.organization_id.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        if rows.iter().all(|row| row.0.is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(rows.len(), 2);
    // The chat response has no choices, so no finish reason; the stream's
    // first choice finished with `stop` and sent no content event.
    assert!(rows[0].0.is_some_and(|ms| ms >= 0));
    assert_eq!((rows[0].1, rows[0].2.as_deref()), (None, None));
    assert!(rows[1].0.is_some_and(|ms| ms >= 0));
    assert_eq!((rows[1].1, rows[1].2.as_deref()), (None, Some("stop")));
}

/// Refuses every request with 429 before any work, or with 503 when
/// `unavailable` (which may follow work).
struct RefusingProvider {
    unavailable: bool,
}

impl RefusingProvider {
    fn error(&self) -> ProviderError {
        let (category, status) = if self.unavailable {
            (ProviderErrorCategory::Unavailable, 503)
        } else {
            (ProviderErrorCategory::RateLimited, 429)
        };
        ProviderError::from_parts(
            category,
            Some("openai".into()),
            Some("req-refused".into()),
            Some(status),
            Some("rate_limit_exceeded".into()),
            Some(StdDuration::from_secs(1)),
            None,
        )
        .unwrap()
    }
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for RefusingProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Err(self.error())
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        Err(self.error())
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(self.error())
    }
}

/// A provider 429 is a refusal before any work: the request's reservation
/// is released, its budget holds nothing and was charged nothing, its trace
/// closes as `refused` at zero with the provider's status, and no
/// reconciliation item opens; for chat and a stream that never started.
/// A 503, which may follow work, still settles as unknown.
#[tokio::test]
async fn a_provider_refusal_releases_the_reservation_at_zero() {
    let _guard = database_guard();
    let Some((pool, service, identity)) = setup_with(
        std::sync::Arc::new(RefusingProvider { unavailable: false }),
        10,
    )
    .await
    else {
        return;
    };
    let chat_key = format!("refused-chat-{}", Uuid::new_v4());
    let stream_key = format!("refused-stream-{}", Uuid::new_v4());
    let result = service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&chat_key),
        )
        .await;
    assert!(matches!(
        result,
        Err(AccountingServiceError::Provider(error)) if error.http_status() == Some(429)
    ));
    assert!(
        service
            .chat_stream(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(&stream_key),
            )
            .await
            .is_err()
    );
    for key in [&chat_key, &stream_key] {
        let reservation = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                key,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reservation.state, gateway::ReservationState::Released);
        let trace: (Option<String>, Option<i64>, Option<i16>, Option<String>) = sqlx::query_as(
            "SELECT outcome, amount_minor, provider_error_status, provider_request_id
             FROM gateway.inference_trace WHERE id=$1",
        )
        .bind(reservation.trace_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            trace,
            (
                Some("refused".into()),
                Some(0),
                Some(429),
                Some("req-refused".into())
            )
        );
        let allocations: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM gateway.accounting_allocation WHERE reservation_id=$1",
        )
        .bind(reservation.id.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(allocations, vec!["released".to_owned()]);
    }
    let (reserved, consumed, items): (i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(reserved_minor), 0)::BIGINT, COALESCE(sum(consumed_minor), 0)::BIGINT,
                (SELECT count(*) FROM gateway.accounting_reconciliation WHERE organization_id=$1)
         FROM gateway.quota_bucket WHERE organization_id=$1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed, items), (0, 0, 0));
    let events = outbox_events(&pool, identity.organization_id).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(
        events
            .iter()
            .all(|(kind, body)| kind == "inference.finalized"
                && body["outcome"] == "refused"
                && body["amount_minor"] == 0),
        "{events:?}"
    );

    // A 503 is not proof that nothing was done.
    let Some((_, service, identity)) = setup_with(
        std::sync::Arc::new(RefusingProvider { unavailable: true }),
        10,
    )
    .await
    else {
        return;
    };
    let key = format!("unavailable-{}", Uuid::new_v4());
    assert!(
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(&key)
            )
            .await
            .is_err()
    );
    let reservation = service
        .store()
        .find_reservation_by_service_account_idempotency(
            identity.organization_id,
            identity.service_account_id,
            &key,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, gateway::ReservationState::Unknown);
}

/// Releasing a refused request is a finalization: a retry under the same id
/// replays it without releasing twice, another finalization of the released
/// reservation is refused, and a reservation the expiry worker made unknown
/// first keeps its hold and reconciliation item.
#[tokio::test]
async fn a_released_refusal_replays_and_never_releases_twice() {
    let _guard = database_guard();
    let Some((pool, service, org, id, trace, release)) = claimed_then_recovered().await else {
        return;
    };
    let refusal = |finalization_id: Uuid| Settlement {
        finalization_id,
        usage: None,
        trace_id: trace,
        provider_request_id: None,
        provider_error: Some(ProviderError::new(ProviderErrorCategory::RateLimited)),
        timings: Default::default(),
    };
    let store = service.store();
    assert_eq!(
        store
            .release_refused(org, id, refusal(Uuid::new_v4()))
            .await
            .unwrap(),
        gateway::ReservationState::Unknown
    );
    assert_eq!(reconciliation_item(&pool, id).await.2, "open");
    release.notify_one();

    let release = Arc::new(tokio::sync::Notify::new());
    let Some((pool, service, identity)) = setup_with(
        std::sync::Arc::new(GatedProvider {
            release: release.clone(),
        }),
        10,
    )
    .await
    else {
        return;
    };
    let key = format!("refusal-replay-{}", Uuid::new_v4());
    let task = {
        let (service, identity, key) = (service.clone(), identity.clone(), key.clone());
        tokio::spawn(async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new(&key),
                )
                .await
        })
    };
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    let reservation = loop {
        assert!(tokio::time::Instant::now() < deadline, "never claimed");
        if let Some(reservation) = service
            .store()
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
            .filter(|reservation| reservation.state == gateway::ReservationState::Claimed)
        {
            break reservation;
        }
        tokio::time::sleep(StdDuration::from_millis(2)).await;
    };
    task.abort();
    let (org, id) = (identity.organization_id, reservation.id);
    let bucket = || async {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket
             WHERE organization_id=$1",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(bucket().await.0 > 0);
    let trace = reservation.trace_id;
    let refusal = |finalization_id: Uuid| Settlement {
        finalization_id,
        usage: None,
        trace_id: trace,
        provider_request_id: None,
        provider_error: Some(ProviderError::new(ProviderErrorCategory::RateLimited)),
        timings: Default::default(),
    };
    let finalization = Uuid::new_v4();
    for _ in 0..2 {
        assert_eq!(
            service
                .store()
                .release_refused(org, id, refusal(finalization))
                .await
                .unwrap(),
            gateway::ReservationState::Released
        );
    }
    assert_eq!(bucket().await, (0, 0));
    assert!(matches!(
        service
            .store()
            .release_refused(org, id, refusal(Uuid::new_v4()))
            .await,
        Err(gateway::AccountingStoreError::NotSettleable)
    ));
    let finalizations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_finalization WHERE reservation_id=$1",
    )
    .bind(id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(finalizations, 1);
    assert_eq!(
        outbox_events(&pool, org)
            .await
            .iter()
            .filter(|(kind, _)| kind == "inference.finalized")
            .count(),
        1
    );
    release.notify_one();
}

/// A TCP proxy to the test database that can be taken down as a restart
/// does: open connections are reset, and a new one is answered with the
/// single byte a PostgreSQL still starting up sends instead of answering
/// TLS negotiation (which the driver reports as a protocol error).
struct RestartingDatabase {
    port: u16,
    down: Arc<std::sync::atomic::AtomicBool>,
    reset: tokio::sync::broadcast::Sender<()>,
}

impl RestartingDatabase {
    async fn start(upstream: (String, u16)) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (reset, _) = tokio::sync::broadcast::channel(4);
        let (accept_down, accept_reset) = (down.clone(), reset.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                let (down, mut reset, upstream) = (
                    accept_down.clone(),
                    accept_reset.subscribe(),
                    upstream.clone(),
                );
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    if down.load(Ordering::SeqCst) {
                        let _ = client.write_all(&[0]).await;
                        return;
                    }
                    let Ok(mut server) = tokio::net::TcpStream::connect(upstream).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = reset.recv() => {}
                    }
                });
            }
        });
        Self { port, down, reset }
    }

    fn stop(&self) {
        self.down.store(true, Ordering::SeqCst);
        let _ = self.reset.send(());
    }

    fn start_again(&self) {
        self.down.store(false, Ordering::SeqCst);
    }
}

/// A settlement that falls in a database restart is completed by the
/// settlement retry once the database is back: settled once, at its usage,
/// with no reconciliation item, though the restart answered the retries
/// with reset connections and protocol errors for two seconds.
#[tokio::test]
async fn a_settlement_during_a_database_restart_completes_once_it_is_back() {
    let _guard = database_guard();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(GatedProvider {
        release: release.clone(),
    });
    let Some((pool, _, identity)) = setup_with(provider.clone(), 10).await else {
        return;
    };
    let options: sqlx::postgres::PgConnectOptions = env::var("GATEWAY_TEST_DATABASE_URL")
        .unwrap()
        .parse()
        .unwrap();
    let database =
        RestartingDatabase::start((options.get_host().to_owned(), options.get_port())).await;
    let proxied = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(StdDuration::from_millis(500))
        .connect_with(options.host("127.0.0.1").port(database.port))
        .await
        .unwrap();
    let policy = AccountingPolicy::new(
        1024,
        std::num::NonZeroU32::new(4).unwrap(),
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 1, 0).unwrap(),
        Currency::new("CRD").unwrap(),
        [9; 32],
    )
    .unwrap()
    .with_chat_input_count_mode(ChatInputCountMode::ProviderApiExact);
    let service = AccountingService::new(AccountingStore::new(proxied), provider, policy);
    let key = format!("restart-{}", Uuid::new_v4());
    let task = {
        let (service, identity, key) = (service.clone(), identity.clone(), key.clone());
        tokio::spawn(async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new(&key),
                )
                .await
        })
    };
    let store = AccountingStore::new(pool.clone());
    let find = || async {
        store
            .find_reservation_by_service_account_idempotency(
                identity.organization_id,
                identity.service_account_id,
                &key,
            )
            .await
            .unwrap()
    };
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
    while find()
        .await
        .is_none_or(|reservation| reservation.state != gateway::ReservationState::Claimed)
    {
        assert!(tokio::time::Instant::now() < deadline, "never claimed");
        tokio::time::sleep(StdDuration::from_millis(5)).await;
    }

    // The database goes down as the provider answers.
    database.stop();
    release.notify_one();
    // The caller is told settlement failed, since it is not yet final.
    assert!(task.await.unwrap().is_err());
    tokio::time::sleep(StdDuration::from_secs(2)).await;
    assert_eq!(
        find().await.unwrap().state,
        gateway::ReservationState::Claimed
    );
    database.start_again();

    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(20);
    let reservation = loop {
        let reservation = find().await.unwrap();
        if reservation.state == gateway::ReservationState::Settled {
            break reservation;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the retry never settled it: {:?}",
            reservation.state
        );
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    };
    let (usage, items): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM gateway.accounting_usage WHERE reservation_id=$1),
                (SELECT count(*) FROM gateway.accounting_reconciliation WHERE reservation_id=$1)",
    )
    .bind(reservation.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((usage, items), (1, 0));
    let (reserved, consumed): (i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket WHERE organization_id=$1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed), (0, 2));
}

/// Answers once `open` has permits; until then every call waits.
struct HeldProvider {
    open: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for HeldProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let _permit = self.open.acquire().await.unwrap();
        SuccessfulProvider.chat(context, request).await
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

/// At high limits, hundreds of requests racing one organization's policy
/// are admitted exactly up to each limit, never past it: 250 of 500 held
/// in flight under `max_concurrent` 250, and, once those finish, 150 more
/// of 300 under 400 requests a minute. The counters admission reads end
/// equal to what the reservations themselves say.
#[tokio::test]
async fn high_limits_admit_exactly_up_to_the_limit_under_contention() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    let Some((pool, service, identity)) =
        setup_with(Arc::new(HeldProvider { open: open.clone() }), 1_000_000_000).await
    else {
        return;
    };
    set_rate_limit(
        &pool,
        identity.organization_id,
        None,
        None,
        Some(400),
        Some(250),
    )
    .await;
    let burst = |count: usize, prefix: &'static str| {
        (0..count)
            .map(|index| {
                let (service, identity) = (service.clone(), identity.clone());
                tokio::spawn(async move {
                    service
                        .chat(
                            &identity,
                            context(),
                            request(),
                            AccountedChatRequest::new(format!("{prefix}-{index}")),
                        )
                        .await
                })
            })
            .collect::<Vec<_>>()
    };
    let in_flight = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.accounting_reservation
             WHERE organization_id=$1 AND state IN ('reserved', 'claimed')",
        )
        .bind(identity.organization_id.0)
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    let held = burst(500, "held");
    let mut refused = 0;
    let mut waiting = Vec::new();
    for task in held {
        if task.is_finished() {
            let breach = rate_limited(&task.await.unwrap()).expect("only refusals finish");
            assert_eq!(breach.kind, RateLimitKind::Concurrency);
            refused += 1;
        } else {
            waiting.push(task);
        }
    }
    // Every request has either been refused or holds a slot.
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(60);
    loop {
        let finished: Vec<_> = waiting.iter().map(|task| task.is_finished()).collect();
        let (done, pending): (Vec<_>, Vec<_>) = waiting
            .into_iter()
            .zip(finished)
            .partition(|(_, finished)| *finished);
        for (task, _) in done {
            let breach = rate_limited(&task.await.unwrap()).expect("only refusals finish");
            assert_eq!(breach.kind, RateLimitKind::Concurrency);
            refused += 1;
        }
        waiting = pending.into_iter().map(|(task, _)| task).collect();
        if refused + in_flight().await as usize == 500 && waiting.len() == 250 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{refused} refused, {} waiting",
            waiting.len()
        );
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    assert_eq!((refused, in_flight().await), (250, 250));

    open.add_permits(10_000);
    for task in waiting {
        assert!(task.await.unwrap().is_ok());
    }
    assert_eq!(in_flight().await, 0);

    // 250 admitted this minute: 150 of the next 300 fit under 400.
    let mut admitted = 0;
    let mut limited = 0;
    for task in burst(300, "rated") {
        match task.await.unwrap() {
            Ok(_) => admitted += 1,
            result => {
                let breach = rate_limited(&result).expect("only rate limits refuse");
                assert_eq!(breach.kind, RateLimitKind::RequestsPerMinute);
                assert!((1..=60).contains(&breach.retry_after_seconds));
                limited += 1;
            }
        }
    }
    assert_eq!((admitted, limited), (150, 150));
    assert_eq!(reservations(&pool, identity.organization_id).await, 400);
    let (counted_flight, counted_admissions): (i64, i64) = sqlx::query_as(
        "SELECT c.in_flight - COALESCE((SELECT sum(r.released) FROM gateway.rate_limit_release r
                                         WHERE r.policy_id = c.policy_id), 0)::BIGINT,
                (SELECT sum(s)::BIGINT FROM unnest(c.slots) s)
         FROM gateway.rate_limit_counter c
         JOIN gateway.rate_limit_policy p ON p.id = c.policy_id
         WHERE p.organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((counted_flight, counted_admissions), (0, 400));
}

/// Write transactions (each takes one transaction id) since `before`, less
/// the one that reads the counter.
async fn writes_since(pool: &PgPool, before: i64) -> i64 {
    let after: i64 = sqlx::query_scalar("SELECT pg_current_xact_id()::text::BIGINT")
        .fetch_one(pool)
        .await
        .unwrap();
    after - before - 1
}

#[tokio::test]
async fn a_request_writes_in_two_commits_its_reservation_and_its_settlement() {
    let _guard = database_guard();
    let Some((pool, service, identity)) = setup().await else {
        return;
    };
    let before: i64 = sqlx::query_scalar("SELECT pg_current_xact_id()::text::BIGINT")
        .fetch_one(&pool)
        .await
        .unwrap();
    let key = format!("two-commits-{}", Uuid::new_v4());
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new(&key),
        )
        .await
        .unwrap();
    // The reservation is written claimed, and the request's timings go on
    // its trace in the settlement's transaction.
    assert_eq!(writes_since(&pool, before).await, 2);
    let (state, latency): (String, Option<i32>) = sqlx::query_as(
        "SELECT r.state, t.provider_latency_ms FROM gateway.accounting_reservation r
         JOIN gateway.inference_trace t ON t.id = r.trace_id
         WHERE r.organization_id = $1 AND r.idempotency_key = $2",
    )
    .bind(identity.organization_id.0)
    .bind(&key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "settled");
    assert!(
        latency.is_some(),
        "the timings are recorded with the settlement"
    );
}

/// Admission holds a scope's rate-limit policies for update while it waits
/// on budget rows that settlements hold, so a settlement (counting its
/// request out of flight) must never wait on a policy's row, or the two
/// deadlock.
#[tokio::test]
async fn a_settlement_never_waits_on_the_policies_admission_holds() {
    let _guard = database_guard();
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    let Some((pool, service, identity)) =
        setup_with(Arc::new(HeldProvider { open: open.clone() }), 1_000_000).await
    else {
        return;
    };
    set_rate_limit(
        &pool,
        identity.organization_id,
        None,
        None,
        Some(1_000),
        Some(100),
    )
    .await;
    let request = {
        let (service, identity) = (service.clone(), identity.clone());
        tokio::spawn(async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new(format!("held-{}", Uuid::new_v4())),
                )
                .await
        })
    };
    // Admitted and waiting on the provider.
    for _ in 0..200 {
        let claimed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.accounting_reservation
             WHERE organization_id=$1 AND state='claimed'",
        )
        .bind(identity.organization_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        if claimed == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Another admission holds the organization's policies.
    let mut admission = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.rate_limit_policy WHERE organization_id=$1 FOR UPDATE")
        .bind(identity.organization_id.0)
        .execute(&mut *admission)
        .await
        .unwrap();
    open.add_permits(1);
    let settled = tokio::time::timeout(std::time::Duration::from_secs(10), request)
        .await
        .expect("the settlement finishes while admission holds the policies")
        .unwrap();
    assert!(settled.is_ok(), "{:?}", settled.err());
    admission.rollback().await.unwrap();
}

/// What the organization's policy counts in flight: its counter less its
/// releases, as admission reads it.
async fn policy_in_flight(pool: &PgPool, policy: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT c.in_flight - COALESCE((SELECT sum(r.released) FROM gateway.rate_limit_release r
                                        WHERE r.policy_id = c.policy_id), 0)::BIGINT
         FROM gateway.rate_limit_counter c WHERE c.policy_id = $1",
    )
    .bind(policy)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A release (a refused, settled or expired request) updates its policy's
/// release shard and then writes rows whose foreign keys take a key-share
/// lock on the organization's row, while a policy write locks that row and
/// then recounts the policy or deletes it with its release rows. Neither
/// may wait on the other's row, or they deadlock; and the recount stays
/// exact with releases already written.
async fn a_policy_change_and_a_release_never_deadlock(delete: bool) {
    let _guard = database_guard();
    let open = Arc::new(tokio::sync::Semaphore::new(1));
    let Some((pool, service, identity)) =
        setup_with(Arc::new(HeldProvider { open: open.clone() }), 1_000_000).await
    else {
        return;
    };
    let organization = identity.organization_id.0;
    set_rate_limit(
        &pool,
        identity.organization_id,
        None,
        None,
        Some(1_000),
        Some(100),
    )
    .await;
    let policy: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.rate_limit_policy WHERE organization_id = $1")
            .bind(organization)
            .fetch_one(&pool)
            .await
            .unwrap();
    // One request finishes (writing a release); two more stay in flight at
    // the provider, which lets a request through while it has a permit.
    service
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("deadlock-first"),
        )
        .await
        .unwrap();
    open.acquire().await.unwrap().forget();
    let requests: Vec<_> = (0..2)
        .map(|index| {
            let (service, identity) = (service.clone(), identity.clone());
            tokio::spawn(async move {
                service
                    .chat(
                        &identity,
                        context(),
                        request(),
                        AccountedChatRequest::new(format!("deadlock-{index}")),
                    )
                    .await
            })
        })
        .collect();
    let count = |state: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gateway.accounting_reservation
                 WHERE organization_id = $1 AND state = $2",
            )
            .bind(organization)
            .bind(state)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    for _ in 0..500 {
        if count("claimed").await == 2 {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    assert_eq!(count("claimed").await, 2);
    assert_eq!(count("settled").await, 1);
    assert_eq!(policy_in_flight(&pool, policy).await, 2);
    sqlx::query(
        "INSERT INTO gateway.rate_limit_release (policy_id, shard, released)
         SELECT $1, s, 0 FROM generate_series(0, 15) s ON CONFLICT DO NOTHING",
    )
    .bind(policy)
    .execute(&pool)
    .await
    .unwrap();
    // A release in progress: its shards updated (here all of them), its
    // usage and outbox rows not yet written.
    let mut release = pool.begin().await.unwrap();
    sqlx::query("UPDATE gateway.rate_limit_release SET released = released WHERE policy_id = $1")
        .bind(policy)
        .execute(&mut *release)
        .await
        .unwrap();
    // An administrator changes the policy: the organization's row first.
    let change = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
                .bind(organization)
                .execute(&mut *tx)
                .await?;
            if delete {
                sqlx::query("DELETE FROM gateway.rate_limit_policy WHERE id = $1")
                    .bind(policy)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query(
                    "UPDATE gateway.rate_limit_policy SET max_concurrent = 50 WHERE id = $1",
                )
                .bind(policy)
                .execute(&mut *tx)
                .await?;
                sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
                    .bind(policy)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await
        })
    };
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    // The release goes on to the rows that reference the organization.
    let finished = tokio::time::timeout(
        StdDuration::from_secs(10),
        sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR KEY SHARE")
            .bind(organization)
            .execute(&mut *release),
    )
    .await
    .expect("the release is not left waiting");
    assert!(finished.is_ok(), "the release failed: {:?}", finished.err());
    release.commit().await.unwrap();
    let changed = tokio::time::timeout(StdDuration::from_secs(10), change)
        .await
        .expect("the policy change is not left waiting")
        .unwrap();
    assert!(
        changed.is_ok(),
        "the policy change failed: {:?}",
        changed.err()
    );
    if delete {
        let counters: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.rate_limit_counter WHERE policy_id = $1",
        )
        .bind(policy)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(counters, 0);
    } else {
        // Recounted with the releases left in place: still exactly the two
        // in flight, and none once they finish.
        assert_eq!(policy_in_flight(&pool, policy).await, 2);
    }
    open.add_permits(2);
    for request in requests {
        assert!(request.await.unwrap().is_ok());
    }
    if !delete {
        assert_eq!(policy_in_flight(&pool, policy).await, 0);
    }
}

#[tokio::test]
async fn a_policy_recount_and_a_release_never_deadlock() {
    a_policy_change_and_a_release_never_deadlock(false).await;
}

#[tokio::test]
async fn a_policy_deletion_and_a_release_never_deadlock() {
    a_policy_change_and_a_release_never_deadlock(true).await;
}

/// A service for `pool` reserving against budget leases of its own, as one
/// gateway process with `GATEWAY_BUDGET_LEASES` on.
fn leased_service(
    pool: &PgPool,
    provider: Arc<dyn InferenceProviderAdapter>,
) -> (
    AccountingService,
    Arc<gateway::persistence::leases::BudgetLeases>,
) {
    leased_service_with_ttl(pool, provider, StdDuration::from_secs(60))
}

fn leased_service_with_ttl(
    pool: &PgPool,
    provider: Arc<dyn InferenceProviderAdapter>,
    ttl: StdDuration,
) -> (
    AccountingService,
    Arc<gateway::persistence::leases::BudgetLeases>,
) {
    let leases = gateway::persistence::leases::BudgetLeases::new(pool.clone());
    (leased_with(pool, provider, ttl, &leases), leases)
}

/// Another service in the same process: sharing `leases`.
fn leased_with(
    pool: &PgPool,
    provider: Arc<dyn InferenceProviderAdapter>,
    ttl: StdDuration,
    leases: &Arc<gateway::persistence::leases::BudgetLeases>,
) -> AccountingService {
    let policy = AccountingPolicy::new(
        1024,
        std::num::NonZeroU32::new(4).unwrap(),
        ttl,
        PriceSnapshot::new(1, 1, 0).unwrap(),
        Currency::new("CRD").unwrap(),
        [9; 32],
    )
    .unwrap()
    .with_chat_input_count_mode(ChatInputCountMode::ProviderApiExact);
    let store = AccountingStore::new(pool.clone()).with_leases(leases.clone());
    AccountingService::new(store, provider, policy)
}

/// The budget checks the load harness makes (`gateway-load` check.rs), for
/// one organization: with leases, every budget's `reserved` is what its
/// allocations outside active leases hold plus the active leases' amounts,
/// its `consumed` what those allocations were charged, nothing passes the
/// limit, and no active lease's allocations use more than it has.
async fn assert_budgets_exact(pool: &PgPool, organization: gateway::OrganizationId) {
    let buckets: Vec<(i64, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT b.limit_minor, b.reserved_minor, b.consumed_minor, b.leased_minor,
                COALESCE(sum(a.reserved_minor) FILTER (WHERE a.state = 'held' AND l.id IS NULL), 0)::BIGINT,
                COALESCE(sum(a.settled_minor) FILTER (WHERE a.state = 'settled' AND l.id IS NULL), 0)::BIGINT,
                (SELECT COALESCE(sum(x.amount_minor), 0)::BIGINT FROM gateway.budget_lease x
                 WHERE x.quota_bucket_id = b.id AND x.returned_at IS NULL)
         FROM gateway.quota_bucket b
         LEFT JOIN gateway.accounting_allocation a ON a.quota_bucket_id = b.id
         LEFT JOIN gateway.budget_lease l ON l.id = a.lease_id AND l.returned_at IS NULL
         WHERE b.organization_id = $1 GROUP BY b.id",
    )
    .bind(organization.0)
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(!buckets.is_empty());
    for (limit, reserved, consumed, leased, held, settled, active) in buckets {
        assert_eq!(leased, active, "leased_minor is the active leases' amounts");
        assert_eq!(
            reserved,
            held + leased,
            "reserved is held allocations plus leases"
        );
        assert_eq!(
            consumed, settled,
            "consumed is what settled allocations were charged"
        );
        assert!(
            consumed + reserved <= limit,
            "overspent: {consumed} + {reserved} > {limit}"
        );
    }
    let overdrawn: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT l.id FROM gateway.budget_lease l
             LEFT JOIN gateway.accounting_allocation a ON a.lease_id = l.id
             WHERE l.organization_id = $1 AND l.returned_at IS NULL
             GROUP BY l.id
             HAVING COALESCE(sum(a.reserved_minor) FILTER (WHERE a.state = 'held'), 0)
                  + COALESCE(sum(a.settled_minor) FILTER (WHERE a.state = 'settled'), 0)
                  > max(l.amount_minor)) o",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        overdrawn, 0,
        "an active lease's allocations used more than it has"
    );
}

/// Usage charged per settled reservation, from the usage records.
async fn charged(pool: &PgPool, organization: gateway::OrganizationId) -> (i64, i64) {
    sqlx::query_as(
        "SELECT count(*), COALESCE(sum(amount_minor), 0)::BIGINT FROM gateway.accounting_usage
         WHERE organization_id = $1",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn leased_allocations(pool: &PgPool, organization: gateway::OrganizationId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_allocation
         WHERE organization_id = $1 AND lease_id IS NOT NULL",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn leased_requests_settle_exactly_and_returning_the_lease_folds_them() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000).await
    else {
        return;
    };
    let (service, leases) = leased_service(&pool, Arc::new(SuccessfulProvider));
    for index in 0..60 {
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("leased-{index}")),
            )
            .await
            .unwrap();
        assert_budgets_exact(&pool, identity.organization_id).await;
    }
    // The first few warm the rate; then requests reserve against leases.
    let leased = leased_allocations(&pool, identity.organization_id).await;
    assert!(leased >= 40, "only {leased} of 60 under a lease");
    let (count, amount) = charged(&pool, identity.organization_id).await;
    assert_eq!(count, 60, "each request settled once");
    leases.return_all().await;
    assert_budgets_exact(&pool, identity.organization_id).await;
    let (reserved, consumed, leased): (i64, i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, consumed_minor, leased_minor FROM gateway.quota_bucket
         WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed, leased), (0, amount, 0));
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.budget_lease
         WHERE organization_id = $1 AND (returned_at IS NULL OR return_reason IS NULL)",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 0, "every lease returned");
}

/// Three processes' leases and many racing requests against one budget too
/// small for them all: nothing past the limit, each admitted request settled
/// once, and once the leases are returned the budget is exactly what was
/// charged.
#[tokio::test]
async fn leases_of_many_processes_never_overspend_a_budget() {
    let _guard = database_guard();
    let Some((pool, plain, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000).await
    else {
        return;
    };
    // One output token: each request holds what it is charged, so 3,000
    // requests run out a budget with room for 2,500, which is large enough
    // to be leased until it nears its limit.
    let one = || ChatRequest {
        max_tokens: Some(1),
        ..request()
    };
    plain
        .chat(
            &identity,
            context(),
            one(),
            AccountedChatRequest::new("size"),
        )
        .await
        .unwrap();
    let per_request = charged(&pool, identity.organization_id).await.1;
    assert!(per_request > 0);
    sqlx::query(
        "UPDATE gateway.quota_bucket SET limit_minor = consumed_minor + $2 WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .bind(per_request * 2_500)
    .execute(&pool)
    .await
    .unwrap();
    let processes: Vec<_> = (0..3)
        .map(|_| leased_service(&pool, Arc::new(SuccessfulProvider)))
        .collect();
    let requests = (0..3_000).map(|index| {
        let (service, _) = &processes[index % 3];
        let (service, identity) = (service.clone(), identity.clone());
        async move {
            service
                .chat(
                    &identity,
                    context(),
                    one(),
                    AccountedChatRequest::new(format!("race-{index}")),
                )
                .await
        }
    });
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (pool, organization, done) = (pool.clone(), identity.organization_id, done.clone());
        tokio::spawn(async move {
            while !done.load(Ordering::SeqCst) {
                let (limit, used): (i64, i64) = sqlx::query_as(
                    "SELECT limit_minor, consumed_minor + reserved_minor FROM gateway.quota_bucket
                     WHERE organization_id = $1",
                )
                .bind(organization.0)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert!(used <= limit, "overspent mid-run: {used} > {limit}");
                tokio::time::sleep(StdDuration::from_millis(20)).await;
            }
        })
    };
    let results: Vec<_> = stream::iter(requests).buffer_unordered(48).collect().await;
    done.store(true, Ordering::SeqCst);
    sampler.await.unwrap();
    let ok = results.iter().filter(|result| result.is_ok()).count();
    let refused = results
        .iter()
        .filter(|result| {
            matches!(
                result,
                Err(AccountingServiceError::Store(
                    gateway::AccountingStoreError::QuotaExceeded
                ))
            )
        })
        .count();
    assert_eq!(ok + refused, 3_000, "only admitted or refused for quota");
    assert!(refused > 0, "the budget ran out");
    assert!(leased_allocations(&pool, identity.organization_id).await > 0);
    assert_budgets_exact(&pool, identity.organization_id).await;
    for (_, leases) in &processes {
        leases.return_all().await;
    }
    assert_budgets_exact(&pool, identity.organization_id).await;
    let (count, amount) = charged(&pool, identity.organization_id).await;
    assert_eq!(count, ok as i64 + 1, "each admitted request settled once");
    let (limit, reserved, consumed): (i64, i64, i64) = sqlx::query_as(
        "SELECT limit_minor, reserved_minor, consumed_minor FROM gateway.quota_bucket
         WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed), (0, amount));
    assert!(consumed <= limit);
}

/// A process holding requests under a lease dies: the worker makes its
/// requests unknown and returns its lease, which moves their holds to the
/// bucket; a settlement that still arrives afterwards finalizes against the
/// bucket, and a new process takes leases of its own.
#[tokio::test]
async fn a_crashed_processs_lease_is_returned_by_the_worker_and_its_requests_stay_exact() {
    let _guard = database_guard();
    let open = Arc::new(tokio::sync::Semaphore::new(1));
    let Some((pool, _, identity)) =
        setup_with(Arc::new(HeldProvider { open: open.clone() }), 1_000_000).await
    else {
        return;
    };
    let (crashed, _crashed_leases) = leased_service_with_ttl(
        &pool,
        Arc::new(HeldProvider { open: open.clone() }),
        StdDuration::from_secs(2),
    );
    for index in 0..10 {
        crashed
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("warm-{index}")),
            )
            .await
            .unwrap();
    }
    // Five more admitted under the lease, stuck at the provider.
    open.acquire().await.unwrap().forget();
    let stuck: Vec<_> = (0..5)
        .map(|index| {
            let (service, identity) = (crashed.clone(), identity.clone());
            tokio::spawn(async move {
                service
                    .chat(
                        &identity,
                        context(),
                        request(),
                        AccountedChatRequest::new(format!("stuck-{index}")),
                    )
                    .await
            })
        })
        .collect();
    for _ in 0..500 {
        let claimed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.accounting_reservation r
             JOIN gateway.accounting_allocation a ON a.reservation_id = r.id
             WHERE r.organization_id = $1 AND r.state = 'claimed' AND a.lease_id IS NOT NULL",
        )
        .bind(identity.organization_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        if claimed == 5 {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    let under_lease: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation r
         JOIN gateway.accounting_allocation a ON a.reservation_id = r.id
         WHERE r.organization_id = $1 AND r.state = 'claimed' AND a.lease_id IS NOT NULL",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(under_lease, 5, "the stuck requests hold under a lease");
    assert_budgets_exact(&pool, identity.organization_id).await;
    // The process is gone: its requests and its lease outlive their expiry.
    tokio::time::sleep(StdDuration::from_millis(2_500)).await;
    sqlx::query(
        "UPDATE gateway.budget_lease SET expires_at = now() - interval '1 second'
         WHERE organization_id = $1 AND returned_at IS NULL",
    )
    .bind(identity.organization_id.0)
    .execute(&pool)
    .await
    .unwrap();
    let worker = AccountingStore::new(pool.clone());
    assert_eq!(worker.recover_expired(Utc::now(), 100).await.unwrap(), 5);
    assert_budgets_exact(&pool, identity.organization_id).await;
    assert!(
        gateway::persistence::leases::return_expired(&pool, 100)
            .await
            .unwrap()
            >= 1
    );
    assert_budgets_exact(&pool, identity.organization_id).await;
    let (held, leased): (i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, leased_minor FROM gateway.quota_bucket WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        held > 0,
        "the unknown requests' holds stay, on the bucket now"
    );
    assert_eq!(leased, 0);
    let reasons: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT return_reason FROM gateway.budget_lease WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(reasons, vec!["expired".to_owned()]);
    // Their answers arrive after all: each settles once, on the bucket.
    open.add_permits(5);
    for request in stuck {
        let _ = request.await.unwrap();
    }
    assert_budgets_exact(&pool, identity.organization_id).await;
    // A process started afterwards reserves as usual, under leases of its own.
    let (restarted, restarted_leases) =
        leased_service(&pool, Arc::new(HeldProvider { open: open.clone() }));
    for index in 0..20 {
        restarted
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("after-{index}")),
            )
            .await
            .unwrap();
    }
    restarted_leases.return_all().await;
    assert_budgets_exact(&pool, identity.organization_id).await;
    let (usage_count, usage_amount) = charged(&pool, identity.organization_id).await;
    let (reserved, consumed, unknown): (i64, i64, i64) = sqlx::query_as(
        "SELECT b.reserved_minor, b.consumed_minor,
                (SELECT count(*) FROM gateway.accounting_reservation r
                 WHERE r.organization_id = b.organization_id AND r.state = 'unknown')
         FROM gateway.quota_bucket b WHERE b.organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unknown, 0, "the late answers settled the unknown requests");
    assert_eq!((reserved, consumed), (0, usage_amount));
    assert_eq!(usage_count, 35);
}

/// An operator resolving a request held under an active lease may charge
/// more than it held, which the lease's amount cannot take: the lease is
/// returned first, and the budget stays exact.
#[tokio::test]
async fn an_operator_resolution_returns_the_lease_its_request_was_held_under() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000).await
    else {
        return;
    };
    let (service, leases) = leased_service(&pool, Arc::new(SuccessfulProvider));
    let overbound = leased_with(
        &pool,
        Arc::new(OverboundProvider),
        StdDuration::from_secs(60),
        &leases,
    );
    for index in 0..10 {
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("warm-{index}")),
            )
            .await
            .unwrap();
    }
    let breach = overbound
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("overbound"),
        )
        .await;
    assert!(matches!(
        breach,
        Err(AccountingServiceError::Store(
            gateway::AccountingStoreError::BoundExceeded
        ))
    ));
    let (item, lease): (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT c.id, a.lease_id FROM gateway.accounting_reconciliation c
         JOIN gateway.accounting_reservation r ON r.id = c.reservation_id
         JOIN gateway.accounting_allocation a ON a.reservation_id = r.id
         WHERE r.organization_id = $1 AND r.idempotency_key = 'overbound'",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let lease = lease.expect("the unknown request is held under a lease");
    assert_budgets_exact(&pool, identity.organization_id).await;
    let actor: Uuid = sqlx::query_scalar(
        "SELECT user_id FROM gateway.organization_membership WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
        .bind(identity.organization_id.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    let resolved = AccountingStore::resolve_by_operator(
        &mut tx,
        identity.organization_id,
        item,
        500,
        gateway::UserAccountId(actor),
    )
    .await
    .unwrap();
    assert!(matches!(
        resolved,
        gateway::persistence::accounting::OperatorResolution::Resolved
    ));
    tx.commit().await.unwrap();
    let reason: Option<String> =
        sqlx::query_scalar("SELECT return_reason FROM gateway.budget_lease WHERE id = $1")
            .bind(lease)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("operator"));
    assert_budgets_exact(&pool, identity.organization_id).await;
    // The process goes on, past the returned lease.
    for index in 0..10 {
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("after-{index}")),
            )
            .await
            .unwrap();
    }
    leases.return_all().await;
    assert_budgets_exact(&pool, identity.organization_id).await;
    let (reserved, consumed): (i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket WHERE organization_id = $1",
    )
    .bind(identity.organization_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (reserved, consumed),
        (0, charged(&pool, identity.organization_id).await.1)
    );
}

/// A request cancelled while its process takes a lease (its client gone,
/// its task aborted) must not stop the process leasing that budget again.
#[tokio::test]
async fn a_request_cancelled_while_taking_a_lease_leaves_leasing_working() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000).await
    else {
        return;
    };
    let organization = identity.organization_id.0;
    let (service, leases) = leased_service(&pool, Arc::new(SuccessfulProvider));
    let taken = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gateway.budget_lease WHERE organization_id = $1",
            )
            .bind(organization)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let chat = |name: String| {
        let (service, identity) = (service.clone(), identity.clone());
        async move {
            service
                .chat(
                    &identity,
                    context(),
                    request(),
                    AccountedChatRequest::new(name),
                )
                .await
        }
    };
    // Warm the budget's rate until a lease is taken, then give it back, so
    // the next request takes a new one.
    for index in 0..200 {
        chat(format!("warm-{index}")).await.unwrap();
        if taken().await > 0 {
            break;
        }
    }
    assert_eq!(taken().await, 1);
    leases.return_all().await;
    // The budget's row held elsewhere: the next request's lease waits on it.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.quota_bucket WHERE organization_id = $1 FOR UPDATE")
        .bind(organization)
        .execute(&mut *holder)
        .await
        .unwrap();
    let cancelled = tokio::spawn(chat("cancelled".to_owned()));
    let mut waiting = false;
    for _ in 0..500 {
        waiting = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                            WHERE wait_event_type = 'Lock'
                              AND query LIKE '%count(DISTINCT l.holder)%')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    assert!(waiting, "the request waits while taking its lease");
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    holder.rollback().await.unwrap();
    // After the retry interval, requests lease the budget again.
    tokio::time::sleep(StdDuration::from_millis(1_100)).await;
    for index in 0..200 {
        chat(format!("after-{index}")).await.unwrap();
        if taken().await > 1 {
            break;
        }
    }
    assert!(taken().await > 1, "leasing resumed after the cancellation");
    leases.return_all().await;
    assert_budgets_exact(&pool, identity.organization_id).await;
}

/// A service for `pool` admitting against rate-limit leases of its own, as
/// one gateway process with `GATEWAY_RATE_LIMIT_LEASES` on.
fn rate_leased_service(
    pool: &PgPool,
    provider: Arc<dyn InferenceProviderAdapter>,
) -> (
    AccountingService,
    Arc<gateway::persistence::rate_leases::RateLimitLeases>,
) {
    let leases = gateway::persistence::rate_leases::RateLimitLeases::new(pool.clone());
    let policy = AccountingPolicy::new(
        1024,
        std::num::NonZeroU32::new(4).unwrap(),
        StdDuration::from_secs(60),
        PriceSnapshot::new(1, 1, 0).unwrap(),
        Currency::new("CRD").unwrap(),
        [9; 32],
    )
    .unwrap()
    .with_chat_input_count_mode(ChatInputCountMode::ProviderApiExact);
    let store = AccountingStore::new(pool.clone()).with_rate_leases(leases.clone());
    (AccountingService::new(store, provider, policy), leases)
}

/// The organization's policy as admission reads it: in flight (its counter
/// less its releases) and the admissions its ring counts.
async fn counted(pool: &PgPool, organization: gateway::OrganizationId) -> (i64, i64) {
    sqlx::query_as(
        "SELECT c.in_flight - COALESCE((SELECT sum(r.released) FROM gateway.rate_limit_release r
                                        WHERE r.policy_id = c.policy_id), 0)::BIGINT,
                (SELECT sum(s)::BIGINT FROM unnest(c.slots) s)
         FROM gateway.rate_limit_counter c
         JOIN gateway.rate_limit_policy p ON p.id = c.policy_id
         WHERE p.organization_id = $1",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn in_flight_reservations(pool: &PgPool, organization: gateway::OrganizationId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND state IN ('reserved', 'claimed')",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Reservations made under a rate-limit lease, and leases still out.
async fn rate_lease_use(pool: &PgPool, organization: gateway::OrganizationId) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM gateway.accounting_reservation
                 WHERE organization_id = $1 AND rate_lease_id IS NOT NULL),
                (SELECT count(*) FROM gateway.rate_limit_lease
                 WHERE organization_id = $1 AND returned_at IS NULL)",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `count` requests from `service`, one every 2 ms (500 a second, as
/// steady traffic arrives: a lease is sized by the last second's rate),
/// keyed `prefix-<n>`, each saying `content`.
async fn paced_from(
    service: &AccountingService,
    identity: &VerifiedIdentity,
    count: usize,
    prefix: &str,
    content: &'static str,
) -> Vec<tokio::task::JoinHandle<Result<ChatResponse, AccountingServiceError>>> {
    let mut tasks = Vec::with_capacity(count);
    let mut pace = tokio::time::interval(StdDuration::from_millis(2));
    for index in 0..count {
        pace.tick().await;
        let (service, identity, key) = (
            service.clone(),
            identity.clone(),
            format!("{prefix}-{index}"),
        );
        tasks.push(tokio::spawn(async move {
            service
                .chat(
                    &identity,
                    context(),
                    ChatRequest {
                        messages: vec![ChatMessage {
                            role: ChatRole::User,
                            content: content.into(),
                            name: None,
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                        }],
                        ..request()
                    },
                    AccountedChatRequest::new(key),
                )
                .await
        }));
    }
    tasks
}

/// `count` requests from each of two services at once, paced.
async fn paced_from_both(
    services: [&AccountingService; 2],
    identity: &VerifiedIdentity,
    count: usize,
    prefix: &str,
) -> Vec<tokio::task::JoinHandle<Result<ChatResponse, AccountingServiceError>>> {
    let (a, b) = (format!("{prefix}a"), format!("{prefix}b"));
    let (mut first, second) = tokio::join!(
        paced_from(services[0], identity, count, &a, "hello"),
        paced_from(services[1], identity, count, &b, "hello"),
    );
    first.extend(second);
    first
}

/// Two gateway processes (each its own pool and leases) racing one
/// organization's requests-per-minute limit: never more admitted than the
/// limit, at any point; and once their leases are returned, what they did
/// not use is back, so requests are admitted up to exactly the limit and
/// the counter says exactly what the reservations say.
#[tokio::test]
async fn rate_limit_leases_of_two_processes_never_pass_the_limit() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let Some((pool, plain, identity)) =
        setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(600), None).await;
    let other_pool = self::pool().await.unwrap();
    let (first, first_leases) = rate_leased_service(&pool, Arc::new(SuccessfulProvider));
    let (second, second_leases) = rate_leased_service(&other_pool, Arc::new(SuccessfulProvider));
    let mut admitted = 0;
    for round in 0..3 {
        let tasks = paced_from_both([&first, &second], &identity, 200, &format!("r{round}")).await;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => admitted += 1,
                result => {
                    let breach = rate_limited(&result).expect("only rate limits refuse");
                    assert_eq!(breach.kind, RateLimitKind::RequestsPerMinute);
                }
            }
        }
        assert!(admitted <= 600, "{admitted} admitted past the limit");
        assert_eq!(reservations(&pool, org).await, admitted);
    }
    let (leased, _) = rate_lease_use(&pool, org).await;
    assert!(leased > 0, "no request was admitted under a lease");
    first_leases.return_all().await;
    second_leases.return_all().await;
    assert_eq!(
        rate_lease_use(&pool, org).await.1,
        0,
        "every lease returned"
    );
    // The window still holds every admission; what is left of it is
    // admitted on the policy's row, up to exactly the limit.
    loop {
        let result = plain
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("plain-{admitted}")),
            )
            .await;
        if rate_limited(&result).is_some() {
            break;
        }
        result.unwrap();
        admitted += 1;
        assert!(admitted <= 600, "{admitted} admitted past the limit");
    }
    assert_eq!(admitted, 600);
    assert_eq!(reservations(&pool, org).await, 600);
    assert_eq!(counted(&pool, org).await, (0, 600));
    // A leased reservation keeps its insert time in `created_at` (end-user
    // limits count by it); its admission time is a column of its own.
    let backdated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND rate_lease_id IS NOT NULL
           AND NOT (rate_admitted_at <= created_at)",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(backdated, 0);
}

/// The same for a concurrency limit, with requests held at the provider:
/// never more in flight than the limit across both processes; with their
/// leases returned, the count is exactly what is in flight, the rest of the
/// limit is admitted, and it all comes back once the requests finish.
#[tokio::test]
async fn rate_limit_leases_of_two_processes_never_pass_a_concurrency_limit() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = || Arc::new(HeldProvider { open: open.clone() });
    let Some((pool, plain, identity)) = setup_with(provider(), 1_000_000_000).await else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, None, Some(400)).await;
    let other_pool = self::pool().await.unwrap();
    let (first, first_leases) = rate_leased_service(&pool, provider());
    let (second, second_leases) = rate_leased_service(&other_pool, provider());
    // A gentle start until a lease is in use: a burst from nothing queues
    // every request on the policy's row, and the lease's take behind them.
    let mut waiting = Vec::new();
    for index in 0..200 {
        for (name, service) in [("a", &first), ("b", &second)] {
            waiting.extend(
                paced_from(
                    service,
                    &identity,
                    1,
                    &format!("warm-{name}{index}"),
                    "hello",
                )
                .await,
            );
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
        if rate_lease_use(&pool, org).await.0 > 0 {
            break;
        }
    }
    let warm = waiting.len();
    waiting.extend(paced_from_both([&first, &second], &identity, 300, "held").await);
    // Every request of `waiting` is either refused or held in flight, beside
    // the `already` held before.
    let settle = |mut waiting: Vec<
        tokio::task::JoinHandle<Result<ChatResponse, AccountingServiceError>>,
    >,
                  already: usize| {
        let pool = pool.clone();
        async move {
            let mut refused = 0;
            let deadline = tokio::time::Instant::now() + StdDuration::from_secs(60);
            loop {
                let (done, pending): (Vec<_>, Vec<_>) =
                    waiting.into_iter().partition(|task| task.is_finished());
                for task in done {
                    let breach = rate_limited(&task.await.unwrap()).expect("only refusals finish");
                    assert_eq!(breach.kind, RateLimitKind::Concurrency);
                    refused += 1;
                }
                waiting = pending;
                let held = in_flight_reservations(&pool, org).await as usize;
                assert!(held <= 400, "{held} in flight past the limit");
                if waiting.len() + already == held {
                    return (waiting, refused);
                }
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(StdDuration::from_millis(20)).await;
            }
        }
    };
    let (mut held, refused) = settle(waiting, 0).await;
    assert_eq!(held.len() + refused, warm + 600);
    assert!(held.len() <= 400);
    assert!(
        rate_lease_use(&pool, org).await.0 > 0,
        "no request was leased"
    );
    first_leases.return_all().await;
    second_leases.return_all().await;
    assert_eq!(counted(&pool, org).await.0, held.len() as i64);
    // The rest of the limit, and no more, on the policy's row.
    let more = paced_from(&plain, &identity, 400, "plain", "hello").await;
    let previous = held.len();
    let (more, refused) = settle(more, previous).await;
    assert_eq!(more.len() + refused, 400);
    assert_eq!(previous + more.len(), 400);
    held.extend(more);
    assert_eq!(counted(&pool, org).await.0, 400);
    open.add_permits(10_000);
    for task in held {
        assert!(task.await.unwrap().is_ok());
    }
    assert_eq!(counted(&pool, org).await.0, 0);
}

/// A process that takes a lease and dies never returns it: its admissions
/// still count, and its in-flight count holds capacity until the worker
/// returns the lease once it has ended. Then the counter says exactly what
/// the reservations say.
#[tokio::test]
async fn an_abandoned_rate_limit_lease_expires_and_gives_its_capacity_back() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(100_000), Some(100_000)).await;
    let mut sent = 0;
    {
        let (service, leases) = rate_leased_service(&pool, Arc::new(SuccessfulProvider));
        // Rounds until requests were admitted under a lease (none is taken
        // in the last 200 ms of a second, which a short round can fall in).
        for round in 0..10 {
            for task in paced_from(
                &service,
                &identity,
                50,
                &format!("crashed-{round}"),
                "hello",
            )
            .await
            {
                task.await.unwrap().unwrap();
            }
            sent += 50;
            if rate_lease_use(&pool, org).await.0 > 0 {
                break;
            }
        }
        // It crashes: nothing is returned.
        drop(leases);
    }
    // And another process crashed right after taking a lease.
    while !matches!(
        grant_lease_now(&pool, org, 30, None, None).await,
        Some((true, _))
    ) {}
    let (leased, out) = rate_lease_use(&pool, org).await;
    assert!(leased > 0 && out > 0, "{leased} leased, {out} leases out");
    // The leases' unused admissions hold in-flight capacity and count in the
    // window until they are returned.
    let unused: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(l.admissions - (SELECT count(*) FROM gateway.accounting_reservation r
                                             WHERE r.rate_lease_id = l.id)), 0)::BIGINT
         FROM gateway.rate_limit_lease l WHERE l.organization_id = $1 AND l.returned_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(unused >= 30, "{unused} unused");
    assert_eq!(counted(&pool, org).await, (unused, sent + unused));
    // Not before the lease has ended.
    assert_eq!(
        gateway::persistence::rate_leases::return_expired(&pool, 100, StdDuration::from_secs(3600))
            .await
            .unwrap(),
        0
    );
    tokio::time::sleep(StdDuration::from_millis(2_100)).await;
    let returned = gateway::persistence::rate_leases::return_expired(&pool, 100, StdDuration::ZERO)
        .await
        .unwrap();
    // Other tests' abandoned leases may come back too.
    assert!(returned >= out as u64);
    assert_eq!(rate_lease_use(&pool, org).await.1, 0);
    let reasons: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT return_reason FROM gateway.rate_limit_lease WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(reasons, vec!["expired".to_owned()]);
    assert_eq!(counted(&pool, org).await, (0, sent));
}

/// Answers by what the request says: `hold` waits at the provider, `refuse`
/// is a 429 before any work, `fail` a 503; anything else succeeds.
struct MixedProvider {
    open: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for MixedProvider {
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        exact_chat_count(context, request).await
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        match request.messages[0].content.as_str() {
            "hold" => {
                let _permit = self.open.acquire().await.unwrap();
                SuccessfulProvider.chat(context, request).await
            }
            "refuse" => Err(RefusingProvider { unavailable: false }.error()),
            "fail" => Err(RefusingProvider { unavailable: true }.error()),
            _ => SuccessfulProvider.chat(context, request).await,
        }
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

/// A request admitted under a lease leaves flight on every path: settled,
/// refused (released), failed (unknown), and cancelled mid-request and then
/// expired by the worker (as a crashed process's would be).
#[tokio::test]
async fn leased_requests_leave_flight_on_every_path() {
    let _guard = database_guard();
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = || Arc::new(MixedProvider { open: open.clone() });
    let Some((pool, _, identity)) = setup_with(provider(), 1_000_000_000).await else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(100_000), Some(100_000)).await;
    let (service, leases) = rate_leased_service(&pool, provider());
    // Steady traffic first, until leases are taken: requests arriving while
    // the first lease is taken are admitted on the policy's row.
    for index in 0..1_000 {
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("warm-{index}")),
            )
            .await
            .unwrap();
        if rate_lease_use(&pool, org).await.0 >= 20 {
            break;
        }
    }
    let warm = reservations(&pool, org).await;
    // Interleaved, so that each kind is admitted under leases.
    let (ok, refuse, fail, hold) = tokio::join!(
        paced_from(&service, &identity, 40, "ok", "ok"),
        paced_from(&service, &identity, 40, "refuse", "refuse"),
        paced_from(&service, &identity, 40, "fail", "fail"),
        paced_from(&service, &identity, 40, "hold", "hold"),
    );
    let mut tasks = Vec::new();
    for (kind, kind_tasks) in [
        ("ok", ok),
        ("refuse", refuse),
        ("fail", fail),
        ("hold", hold),
    ] {
        tasks.extend(kind_tasks.into_iter().map(|task| (kind, task)));
    }
    let (held, finished): (Vec<_>, Vec<_>) =
        tasks.into_iter().partition(|(kind, _)| *kind == "hold");
    for (kind, task) in finished {
        let result = task.await.unwrap();
        assert_eq!(result.is_ok(), kind == "ok", "{kind}: {:?}", result.err());
    }
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(30);
    while in_flight_reservations(&pool, org).await < 40 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held requests never claimed"
        );
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    // The held requests are cancelled: their reservations stay claimed.
    for (_, task) in held {
        task.abort();
    }
    // Every path ran under a lease.
    let leased: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT split_part(idempotency_key, '-', 1), state, count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND rate_lease_id IS NOT NULL GROUP BY 1, 2 ORDER BY 1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    let states: Vec<(&str, &str)> = leased
        .iter()
        .map(|(kind, state, _)| (kind.as_str(), state.as_str()))
        .collect();
    for path in [
        ("fail", "unknown"),
        ("hold", "claimed"),
        ("ok", "settled"),
        ("refuse", "released"),
    ] {
        assert!(states.contains(&path), "no leased {path:?} in {leased:?}");
    }
    leases.return_all().await;
    assert_eq!(counted(&pool, org).await, (40, warm + 160));
    // The worker expires what the cancelled requests left (with other
    // tests' leftovers, oldest first).
    for _ in 0..100 {
        let expired = service
            .store()
            .recover_expired(Utc::now() + Duration::hours(1), 1_000)
            .await
            .unwrap();
        if expired == 0 || in_flight_reservations(&pool, org).await == 0 {
            break;
        }
    }
    assert_eq!(in_flight_reservations(&pool, org).await, 0);
    assert_eq!(counted(&pool, org).await, (0, warm + 160));
}

/// A policy changed while a lease is out: the recount keeps what the lease
/// counted, the lease admits nothing more (its limits are not the
/// policy's), and once it is returned the counter is exact.
#[tokio::test]
async fn a_policy_change_retires_rate_limit_leases_and_recounts_them_exactly() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(100_000), Some(100_000)).await;
    let policy: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.rate_limit_policy WHERE organization_id = $1")
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (service, leases) = rate_leased_service(&pool, Arc::new(SuccessfulProvider));
    // Rounds until a lease is out: none is taken in the last 200 ms of a
    // second, which a short round can fall within.
    let mut sent = 0;
    let mut out: Vec<Uuid> = Vec::new();
    for round in 0..10 {
        for task in paced_from(&service, &identity, 50, &format!("before-{round}"), "hello").await {
            task.await.unwrap().unwrap();
        }
        sent += 50;
        out = sqlx::query_scalar(
            "SELECT id FROM gateway.rate_limit_lease
             WHERE organization_id = $1 AND returned_at IS NULL",
        )
        .bind(org.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        if !out.is_empty() {
            break;
        }
    }
    assert!(!out.is_empty(), "no lease taken");
    let before = counted(&pool, org).await;
    // An administrator changes the policy, as RateLimitStore::upsert does.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.rate_limit_policy SET max_concurrent = 50000 WHERE id = $1")
        .bind(policy)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
        .bind(policy)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        counted(&pool, org).await,
        before,
        "the recount keeps what the leases counted"
    );
    for index in 0..20 {
        service
            .chat(
                &identity,
                context(),
                request(),
                AccountedChatRequest::new(format!("after-{index}")),
            )
            .await
            .unwrap();
    }
    let reused: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND rate_lease_id = ANY($2) AND idempotency_key LIKE 'after-%'",
    )
    .bind(org.0)
    .bind(&out)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        reused, 0,
        "a lease taken under the old limits admitted more"
    );
    leases.return_all().await;
    assert_eq!(rate_lease_use(&pool, org).await.1, 0);
    assert_eq!(counted(&pool, org).await, (0, sent + 20));
}

/// With leases off (the default), admission is as before: nothing is
/// leased, and every reservation is counted on the policy's row.
#[tokio::test]
async fn without_rate_limit_leases_nothing_is_leased() {
    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(100_000), Some(100_000)).await;
    assert!(service.store().rate_leases().is_none());
    for task in paced_from(&service, &identity, 60, "plain", "hello").await {
        task.await.unwrap().unwrap();
    }
    assert_eq!(rate_lease_use(&pool, org).await, (0, 0));
    let leases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.rate_limit_lease WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leases, 0);
    assert_eq!(counted(&pool, org).await, (0, 60));
}

/// The database's clock, in epoch seconds.
async fn database_epoch(pool: &PgPool) -> f64 {
    sqlx::query_scalar("SELECT extract(epoch FROM clock_timestamp())::DOUBLE PRECISION")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// What admission reads of the organization's policy right now (the window
/// `rate_limits::Window` counts: every second from a minute ago to the
/// newest the ring holds), with the second it was read in.
async fn window_now(pool: &PgPool, organization: gateway::OrganizationId) -> (i64, i64) {
    sqlx::query_as(
        "WITH t AS (SELECT floor(extract(epoch FROM clock_timestamp()))::BIGINT AS now)
         SELECT t.now,
                (SELECT COALESCE(sum(c.slots[(g % 61)::INTEGER + 1]), 0)::BIGINT
                 FROM generate_series(t.now - 60, GREATEST(t.now, c.last_second)) g
                 WHERE g > c.last_second - 61 AND g <= c.last_second)
         FROM t, gateway.rate_limit_counter c
         JOIN gateway.rate_limit_policy p ON p.id = c.policy_id
         WHERE p.organization_id = $1",
    )
    .bind(organization.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Takes a lease of `admissions` for the organization as a gateway does
/// (its locks, then the grant), with the room read in `room_second`.
async fn grant_lease_now(
    pool: &PgPool,
    organization: gateway::OrganizationId,
    admissions: i32,
    room_second: Option<i64>,
    between: Option<StdDuration>,
) -> Option<(bool, f64)> {
    let holder = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    let epochs: Vec<f64> = sqlx::query_scalar(
        "SELECT now_epoch FROM gateway.lock_rate_limit_lease_counters($1, NULL, NULL, $2)",
    )
    .bind(organization.0)
    .bind(holder)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    if let Some(between) = between {
        tokio::time::sleep(between).await;
    }
    let granted = sqlx::query_as(
        "SELECT granted, seconds_left
         FROM gateway.grant_rate_limit_lease($1, $2, $3, NULL, NULL, $4, $5, 0)",
    )
    .bind(Uuid::new_v4())
    .bind(holder)
    .bind(organization.0)
    .bind(admissions)
    .bind(room_second.unwrap_or(epochs[0].floor() as i64))
    .fetch_optional(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    granted
}

/// A lease is counted in the second it is taken in. Counting it in the next
/// second would clear the 61-second ring's slot for the oldest second still
/// in the window: 890 admissions late in second S-60 would vanish while a
/// request in S still counts them, and about 1,890 would pass a limit of
/// 1,000 a minute.
#[tokio::test]
async fn a_rate_limit_lease_never_clears_admissions_still_in_the_window() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(1_000), None).await;
    sqlx::query(
        "SELECT gateway.seed_rate_limit_counter(id) FROM gateway.rate_limit_policy
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    let mut checked = false;
    for _ in 0..20 {
        // Early in a second, so setting up, taking the lease and reading the
        // window all happen in it.
        let now = database_epoch(&pool).await;
        if now.fract() > 0.3 {
            tokio::time::sleep(StdDuration::from_secs_f64(1.02 - now.fract())).await;
            continue;
        }
        let second = now.floor() as i64;
        // 890 admitted in second S-60, the newest second written then.
        sqlx::query(
            "UPDATE gateway.rate_limit_counter c
             SET slots = (SELECT array_agg(CASE WHEN i = (($2 - 60) % 61) + 1 THEN 890 ELSE 0 END
                                           ORDER BY i)
                          FROM generate_series(1, 61) i),
                 last_second = $2 - 60
             FROM gateway.rate_limit_policy p
             WHERE p.id = c.policy_id AND p.organization_id = $1",
        )
        .bind(org.0)
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(window_now(&pool, org).await, (second, 890));
        let Some((true, _)) = grant_lease_now(&pool, org, 50, None, None).await else {
            continue;
        };
        let (read_in, count) = window_now(&pool, org).await;
        if read_in != second {
            continue;
        }
        // Still in S: the window holds S-60's admissions and the lease, so
        // only 60 more can be admitted this second.
        assert_eq!(count, 940, "the lease cleared a second still in the window");
        checked = true;
        break;
    }
    assert!(checked, "never set up within one second");
    sqlx::query("UPDATE gateway.rate_limit_lease SET returned_at = clock_timestamp(), return_reason = 'ended', returned_admitted = 0 WHERE organization_id = $1")
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// A lease's room is read in one second and it is counted in that second or
/// not at all: read at the end of S and granted in S+1, the room was for
/// another window, so nothing is granted or counted.
#[tokio::test]
async fn a_rate_limit_lease_is_not_granted_in_a_later_second_than_its_room() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(1_000), None).await;
    let now = database_epoch(&pool).await;
    let granted = grant_lease_now(
        &pool,
        org,
        50,
        Some(now.floor() as i64),
        Some(StdDuration::from_secs_f64(1.05 - now.fract())),
    )
    .await;
    assert_eq!(granted, Some((false, 0.0)));
    assert_eq!(rate_lease_use(&pool, org).await, (0, 0));
    assert_eq!(counted(&pool, org).await, (0, 0));
    // Too little of the second left: not granted either.
    let granted: Option<(bool, f64)> = {
        let holder = Uuid::new_v4();
        let mut tx = pool.begin().await.unwrap();
        let epochs: Vec<f64> = sqlx::query_scalar(
            "SELECT now_epoch FROM gateway.lock_rate_limit_lease_counters($1, NULL, NULL, $2)",
        )
        .bind(org.0)
        .bind(holder)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let granted = sqlx::query_as(
            "SELECT granted, seconds_left
             FROM gateway.grant_rate_limit_lease($1, $2, $3, NULL, NULL, 50, $4, 1.5)",
        )
        .bind(Uuid::new_v4())
        .bind(holder)
        .bind(org.0)
        .bind(epochs[0].floor() as i64)
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        granted
    };
    assert!(matches!(granted, Some((false, left)) if left > 0.0 && left <= 1.0));
    assert_eq!(rate_lease_use(&pool, org).await, (0, 0));
    assert_eq!(counted(&pool, org).await, (0, 0));
}

/// Inserts a copy of reservation `like` under `lease` with `ticket`,
/// admitted `admitted_ago` before its transaction's own time, as a holder
/// with a broken memory or a direct writer could.
async fn insert_under_lease(
    pool: &PgPool,
    like: Uuid,
    lease: Uuid,
    ticket: i32,
    admitted_ago: StdDuration,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TEMP TABLE copy ON COMMIT DROP AS
         SELECT * FROM gateway.accounting_reservation WHERE id = $1",
    )
    .bind(like)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE copy SET id = gen_random_uuid(), idempotency_key = gen_random_uuid()::text,
                         rate_lease_id = $1, rate_lease_ticket = $2,
                         rate_admitted_at = clock_timestamp() - make_interval(secs => $3)",
    )
    .bind(lease)
    .bind(ticket)
    .bind(admitted_ago.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO gateway.accounting_reservation SELECT * FROM copy")
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// The database caps a lease at its N, whatever writes reservations under
/// it: one reservation per ticket, a ticket past N or an admission stamped
/// outside its own transaction is counted on the policy as any other, and
/// the return counts at most N.
#[tokio::test]
async fn the_database_caps_a_rate_limit_lease_at_its_admissions() {
    let _guard = database_guard();
    let Some((pool, plain, identity)) =
        setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(1_000), None).await;
    plain
        .chat(
            &identity,
            context(),
            request(),
            AccountedChatRequest::new("first".to_owned()),
        )
        .await
        .unwrap();
    let (like, team, account): (Uuid, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT id, team_id, service_account_id FROM gateway.accounting_reservation
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let admissions = || async { counted(&pool, org).await.1 };
    assert_eq!(admissions().await, 1);
    // A lease of 2 for that scope, taken as a gateway takes it.
    let lease = Uuid::new_v4();
    loop {
        let holder = Uuid::new_v4();
        let mut tx = pool.begin().await.unwrap();
        let epochs: Vec<f64> = sqlx::query_scalar(
            "SELECT now_epoch FROM gateway.lock_rate_limit_lease_counters($1, $2, $3, $4)",
        )
        .bind(org.0)
        .bind(team)
        .bind(account)
        .bind(holder)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let granted: Option<(bool, f64)> = sqlx::query_as(
            "SELECT granted, seconds_left
             FROM gateway.grant_rate_limit_lease($1, $2, $3, $4, $5, 2, $6, 0.5)",
        )
        .bind(lease)
        .bind(holder)
        .bind(org.0)
        .bind(team)
        .bind(account)
        .bind(epochs[0].floor() as i64)
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        if matches!(granted, Some((true, _))) {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
    assert_eq!(admissions().await, 3);
    // Its two tickets: the lease counted them already.
    for ticket in [0, 1] {
        insert_under_lease(&pool, like, lease, ticket, StdDuration::ZERO)
            .await
            .unwrap();
    }
    assert_eq!(admissions().await, 3);
    // The same ticket twice: refused.
    let error = insert_under_lease(&pool, like, lease, 1, StdDuration::ZERO)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23505")
    );
    // A ticket the lease does not have, or an admission stamped before its
    // own transaction began: counted on the policy, not skipped.
    insert_under_lease(&pool, like, lease, 2, StdDuration::ZERO)
        .await
        .unwrap();
    assert_eq!(admissions().await, 4);
    insert_under_lease(&pool, like, lease, 3, StdDuration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(admissions().await, 5);
    // The return counts the two it had tickets for.
    assert_eq!(
        gateway::persistence::rate_leases::return_lease(
            &pool,
            lease,
            gateway::persistence::rate_leases::ReturnReason::Ended,
        )
        .await
        .unwrap(),
        Some(2)
    );
    assert_eq!(admissions().await, 5);
}

/// A policy deleted and created again (same id) while a lease naming it is
/// returned: the recount and the return are ordered, so nothing the lease
/// did not use stays counted. Before, the recreated policy had no counter
/// row to lock; a return committing during the recount updated no counter,
/// and the recount kept the lease's unused admissions in flight for good.
#[tokio::test]
async fn a_lease_returned_while_its_policy_is_recreated_leaves_nothing_counted() {
    let _guard = database_guard();
    let Some((pool, _, identity)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = identity.organization_id;
    set_rate_limit(&pool, org, None, None, Some(10_000), Some(10_000)).await;
    let policy: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.rate_limit_policy WHERE organization_id = $1")
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    while !matches!(
        grant_lease_now(&pool, org, 30, None, None).await,
        Some((true, _))
    ) {}
    let lease: Uuid = sqlx::query_scalar(
        "SELECT id FROM gateway.rate_limit_lease WHERE organization_id = $1 AND returned_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counted(&pool, org).await, (30, 30));
    // Deleted: its counter goes with it.
    sqlx::query("DELETE FROM gateway.rate_limit_policy WHERE id = $1")
        .bind(policy)
        .execute(&pool)
        .await
        .unwrap();
    // Created again with the same id, as RateLimitStore::upsert does, its
    // transaction held open after the recount.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, requests_per_minute, max_concurrent)
         VALUES ($1, $2, 'organization', 10000, 10000)",
    )
    .bind(policy)
    .bind(org.0)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
        .bind(policy)
        .execute(&mut *tx)
        .await
        .unwrap();
    // The lease is returned meanwhile: it waits for the recount.
    let returning = tokio::spawn({
        let pool = pool.clone();
        async move {
            gateway::persistence::rate_leases::return_lease(
                &pool,
                lease,
                gateway::persistence::rate_leases::ReturnReason::Ended,
            )
            .await
            .unwrap()
        }
    });
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert!(
        !returning.is_finished(),
        "the return did not wait for the recount"
    );
    tx.commit().await.unwrap();
    assert_eq!(returning.await.unwrap(), Some(0));
    assert_eq!(counted(&pool, org).await, (0, 0));
}

/// A policy of any level, with any of its limits; its id. Its counter is
/// seeded on the first admission that reads it, as for a policy written by
/// a process from before counters.
#[allow(clippy::too_many_arguments)]
async fn insert_rate_limit(
    pool: &PgPool,
    organization: gateway::OrganizationId,
    team: Option<gateway::TeamId>,
    service_account: Option<gateway::ServiceAccountId>,
    api_key: Option<gateway::domain::identity::ApiKeyId>,
    requests_per_minute: Option<i32>,
    tokens_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
) -> Uuid {
    let kind = match (team, service_account, api_key) {
        (_, _, Some(_)) => "api-key",
        (_, Some(_), None) => "service-account",
        (Some(_), None, None) => "team",
        (None, None, None) => "organization",
    };
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, team_id, service_account_id, api_key_id,
              requests_per_minute, tokens_per_minute, max_concurrent)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(organization.0)
    .bind(kind)
    .bind(team.map(|team| team.0))
    .bind(service_account.map(|account| account.0))
    .bind(api_key.map(|key| key.0))
    .bind(requests_per_minute)
    .bind(tokens_per_minute)
    .bind(max_concurrent)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Another key of `identity`'s service account, as an identity.
async fn second_key(pool: &PgPool, identity: &VerifiedIdentity) -> VerifiedIdentity {
    let identities = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"accounting-test-pepper").unwrap(),
    );
    let user = identities
        .create_user(identity.organization_id, None, None, None, Some("test"))
        .await
        .unwrap()
        .id;
    identities
        .add_organization_membership(identity.organization_id, user, MembershipRole::Owner)
        .await
        .unwrap();
    let key = identities
        .create_api_key(
            identity.organization_id,
            identity.service_account_id,
            &[gateway::domain::identity::ApiKeyScope::Inference],
            None,
            user,
        )
        .await
        .unwrap();
    VerifiedIdentity {
        api_key_id: Some(key.id),
        ..identity.clone()
    }
}

async fn chat_as(
    service: &AccountingService,
    identity: &VerifiedIdentity,
    key: &str,
) -> Result<ChatResponse, AccountingServiceError> {
    service
        .chat(
            identity,
            context(),
            request(),
            AccountedChatRequest::new(key),
        )
        .await
}

/// Each tokens-per-minute policy of the organization: the tokens its
/// window counts as admission reads it (the counter's ring and the release
/// rows', second by second), and what the reservations themselves count in
/// the same seconds, in one statement.
async fn token_windows(pool: &PgPool, organization: gateway::OrganizationId) -> Vec<(i64, i64)> {
    sqlx::query_as(
        "WITH clock AS (SELECT floor(extract(epoch FROM clock_timestamp()))::BIGINT AS top)
         SELECT
             (SELECT COALESCE(sum(GREATEST(0,
                  gateway.rate_limit_ring_at(c.token_slots, c.token_last_second, g.second)
                  + COALESCE((SELECT sum(gateway.rate_limit_ring_at(
                                  x.token_slots, x.token_last_second, g.second))
                              FROM gateway.rate_limit_release x WHERE x.policy_id = p.id), 0))),
                  0)::BIGINT
              FROM gateway.rate_limit_counter c, clock,
                   generate_series(clock.top - 60, clock.top) AS g(second)
              WHERE c.policy_id = p.id),
             (SELECT COALESCE(sum(gateway.rate_limit_reservation_tokens(
                         r.organization_id, r.id, r.state, r.token_bound)), 0)::BIGINT
              FROM gateway.accounting_reservation r, clock
              WHERE r.organization_id = p.organization_id AND r.batch_id IS NULL
                AND (p.scope_kind = 'organization'
                     OR (p.scope_kind = 'team' AND r.team_id = p.team_id)
                     OR (p.scope_kind = 'service-account'
                         AND r.service_account_id = p.service_account_id)
                     OR (p.scope_kind = 'api-key' AND r.api_key_id = p.api_key_id))
                AND floor(extract(epoch FROM COALESCE(r.tokens_counted_at, r.created_at)))
                    >= clock.top - 60)
         FROM gateway.rate_limit_policy p
         WHERE p.organization_id = $1 AND p.tokens_per_minute IS NOT NULL
         ORDER BY p.id",
    )
    .bind(organization.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Output caps and token bounds of the organization's reservations, oldest
/// first.
async fn caps_and_bounds(
    pool: &PgPool,
    organization: gateway::OrganizationId,
) -> Vec<(Option<i64>, Option<i64>)> {
    sqlx::query_as(
        "SELECT output_token_cap, token_bound FROM gateway.accounting_reservation
         WHERE organization_id = $1 ORDER BY created_at, id",
    )
    .bind(organization.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// An API key's policy limits that key's requests and no other key's; every
/// policy covering a request applies, and the refusal names the level whose
/// limit was reached.
#[tokio::test]
async fn an_api_keys_policy_limits_its_own_requests_with_the_others() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let Some((pool, service, first)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 1000).await
    else {
        return;
    };
    let org = first.organization_id;
    let second = second_key(&pool, &first).await;
    insert_rate_limit(
        &pool,
        org,
        None,
        None,
        first.api_key_id,
        Some(2),
        None,
        None,
    )
    .await;
    chat_as(&service, &first, "key-1").await.unwrap();
    chat_as(&service, &first, "key-2").await.unwrap();
    let breach = rate_limited(&chat_as(&service, &first, "key-3").await).expect("limited");
    assert_eq!(
        (breach.kind, breach.scope, breach.never_fits),
        (RateLimitKind::RequestsPerMinute, "api-key", false)
    );
    assert!((1..=60).contains(&breach.retry_after_seconds));
    // The other key has no policy of its own.
    chat_as(&service, &second, "other-1").await.unwrap();
    // The service account's policy counts both keys' requests.
    insert_rate_limit(
        &pool,
        org,
        None,
        Some(first.service_account_id),
        None,
        Some(4),
        None,
        None,
    )
    .await;
    chat_as(&service, &second, "other-2").await.unwrap();
    let breach = rate_limited(&chat_as(&service, &second, "other-3").await).expect("limited");
    assert_eq!(breach.scope, "service-account");
    // Each reservation names its key; the key's counter counted its own.
    let keys: Vec<(Option<Uuid>, i64)> = sqlx::query_as(
        "SELECT api_key_id, count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 GROUP BY api_key_id ORDER BY api_key_id",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected = vec![
        (first.api_key_id.map(|key| key.0), 2),
        (second.api_key_id.map(|key| key.0), 2),
    ];
    expected.sort();
    assert_eq!(keys, expected);
}

/// A key's concurrency limit counts its requests in flight.
#[tokio::test]
async fn an_api_keys_concurrency_limit_counts_its_requests_in_flight() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = BlockingProvider {
        entered: entered.clone(),
        release: release.clone(),
    };
    let Some((pool, service, first)) = setup_with(std::sync::Arc::new(provider), 1000).await else {
        return;
    };
    let second = second_key(&pool, &first).await;
    insert_rate_limit(
        &pool,
        first.organization_id,
        None,
        None,
        first.api_key_id,
        None,
        None,
        Some(1),
    )
    .await;
    let held = {
        let (service, first) = (service.clone(), first.clone());
        tokio::spawn(async move { chat_as(&service, &first, "held").await })
    };
    entered.notified().await;
    let breach = rate_limited(&chat_as(&service, &first, "second").await).expect("limited");
    assert_eq!(
        (breach.kind, breach.scope, breach.retry_after_seconds),
        (RateLimitKind::Concurrency, "api-key", 1)
    );
    // Another key's request is not counted against it.
    let other = {
        let (service, second) = (service.clone(), second.clone());
        tokio::spawn(async move { chat_as(&service, &second, "other").await })
    };
    entered.notified().await;
    release.notify_one();
    release.notify_one();
    held.await.unwrap().unwrap();
    other.await.unwrap().unwrap();
    // Finished, the slot is free again.
    release.notify_one();
    chat_as(&service, &first, "second").await.unwrap();
}

/// A tokens-per-minute policy counts each request's bound while it runs
/// and what it reported once settled, lowers the output cap of a request
/// only a smaller cap fits, and refuses one whose input alone is more than
/// the limit; the counter agrees with the reservations throughout.
#[tokio::test]
async fn a_policys_tokens_per_minute_lowers_the_output_cap_and_then_refuses() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let Some((pool, service, identity)) =
        setup_with(std::sync::Arc::new(SuccessfulProvider), 1000).await
    else {
        return;
    };
    let org = identity.organization_id;
    // Each request holds 1 input and up to 4 output tokens and reports 2.
    insert_rate_limit(
        &pool,
        org,
        None,
        Some(identity.service_account_id),
        None,
        None,
        Some(12),
        None,
    )
    .await;
    for index in 0..6 {
        chat_as(&service, &identity, &format!("tokens-{index}"))
            .await
            .unwrap();
        let windows = token_windows(&pool, org).await;
        assert_eq!(windows[0].0, windows[0].1, "after request {index}");
    }
    assert_eq!(
        caps_and_bounds(&pool, org).await,
        [
            (Some(4), Some(5)),
            (Some(4), Some(5)),
            (Some(4), Some(5)),
            (Some(4), Some(5)),
            (Some(3), Some(4)),
            (Some(1), Some(2)),
        ]
    );
    assert_eq!(token_windows(&pool, org).await, [(12, 12)]);
    let breach = rate_limited(&chat_as(&service, &identity, "tokens-6").await).expect("limited");
    assert_eq!(
        (breach.kind, breach.scope, breach.never_fits),
        (RateLimitKind::TokensPerMinute, "service-account", false)
    );
    assert!((1..=60).contains(&breach.retry_after_seconds));
    // A key's limit below what the input alone needs never fits.
    insert_rate_limit(
        &pool,
        org,
        None,
        None,
        identity.api_key_id,
        None,
        Some(1),
        None,
    )
    .await;
    let breach = rate_limited(&chat_as(&service, &identity, "tokens-7").await).expect("limited");
    assert_eq!(
        (breach.kind, breach.scope, breach.never_fits),
        (RateLimitKind::TokensPerMinute, "api-key", true)
    );
    assert_eq!(breach.retry_after_seconds, 60);
    assert_eq!(reservations(&pool, org).await, 6);
}

/// A request in flight counts its whole bound until it settles.
#[tokio::test]
async fn a_request_in_flight_counts_its_token_bound_until_it_settles() {
    use gateway::domain::rate_limit::RateLimitKind;

    let _guard = database_guard();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = BlockingProvider {
        entered: entered.clone(),
        release: release.clone(),
    };
    let Some((pool, service, identity)) = setup_with(std::sync::Arc::new(provider), 1000).await
    else {
        return;
    };
    let org = identity.organization_id;
    insert_rate_limit(&pool, org, None, None, None, None, Some(6), None).await;
    let held = {
        let (service, identity) = (service.clone(), identity.clone());
        tokio::spawn(async move { chat_as(&service, &identity, "held").await })
    };
    entered.notified().await;
    // Its bound of 5 leaves 1, less than the 2 the next one needs.
    assert_eq!(token_windows(&pool, org).await, [(5, 5)]);
    let breach = rate_limited(&chat_as(&service, &identity, "next").await).expect("limited");
    assert_eq!(
        (breach.kind, breach.scope),
        (RateLimitKind::TokensPerMinute, "organization")
    );
    release.notify_one();
    held.await.unwrap().unwrap();
    // Settled, it counts the 2 it reported: the next fits with 3 output.
    assert_eq!(token_windows(&pool, org).await, [(2, 2)]);
    release.notify_one();
    chat_as(&service, &identity, "next").await.unwrap();
    assert_eq!(
        caps_and_bounds(&pool, org).await,
        [(Some(4), Some(5)), (Some(3), Some(4))]
    );
    assert_eq!(token_windows(&pool, org).await, [(4, 4)]);
}

/// A policy that starts limiting tokens is recounted from the reservations
/// already in its window, whatever they hold or reported.
#[tokio::test]
async fn a_policy_starting_to_limit_tokens_is_recounted_exactly() {
    let _guard = database_guard();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = BlockingProvider {
        entered: entered.clone(),
        release: release.clone(),
    };
    let Some((pool, service, identity)) = setup_with(std::sync::Arc::new(provider), 1000).await
    else {
        return;
    };
    let org = identity.organization_id;
    let policy = insert_rate_limit(&pool, org, None, None, None, Some(1000), None, None).await;
    for index in 0..3 {
        release.notify_one();
        chat_as(&service, &identity, &format!("before-{index}"))
            .await
            .unwrap();
    }
    // The permit the last request left.
    entered.notified().await;
    let held = {
        let (service, identity) = (service.clone(), identity.clone());
        tokio::spawn(async move { chat_as(&service, &identity, "held").await })
    };
    entered.notified().await;
    // As a policy write recounts it.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.rate_limit_policy SET tokens_per_minute = 100 WHERE id = $1")
        .bind(policy)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
        .bind(policy)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Three settled at 2 and one in flight at 5.
    assert_eq!(token_windows(&pool, org).await, [(11, 11)]);
    release.notify_one();
    held.await.unwrap().unwrap();
    assert_eq!(token_windows(&pool, org).await, [(8, 8)]);
}

/// Leases cover neither a key's own policy nor tokens per minute: a key
/// with a policy is admitted on the policies' rows (its ticket going back
/// to the lease, which other keys' requests keep using), and no lease is
/// granted or used once a policy limits tokens. The counters stay exact.
#[tokio::test]
async fn rate_limit_leases_leave_out_keyed_requests_and_token_limits() {
    let _guard = database_guard();
    let Some((pool, _, first)) = setup_with(Arc::new(SuccessfulProvider), 1_000_000_000).await
    else {
        return;
    };
    let org = first.organization_id;
    let second = second_key(&pool, &first).await;
    let policy = insert_rate_limit(&pool, org, None, None, None, Some(100_000), None, None).await;
    let key_policy = insert_rate_limit(
        &pool,
        org,
        None,
        None,
        first.api_key_id,
        Some(100_000),
        None,
        None,
    )
    .await;
    let (service, leases) = rate_leased_service(&pool, Arc::new(SuccessfulProvider));
    let leased_of = |identity: &VerifiedIdentity| {
        let (pool, key) = (pool.clone(), identity.api_key_id.map(|key| key.0));
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gateway.accounting_reservation
                 WHERE api_key_id = $1 AND rate_lease_id IS NOT NULL",
            )
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    // The second key's traffic is leased; the first key's, mixed in, is not.
    let (keyless, keyed) = tokio::join!(
        paced_from(&service, &second, 300, "plain", "hello"),
        paced_from(&service, &first, 100, "keyed", "hello"),
    );
    for task in keyless.into_iter().chain(keyed) {
        task.await.unwrap().unwrap();
    }
    assert!(leased_of(&second).await > 0, "the other key was not leased");
    assert_eq!(leased_of(&first).await, 0, "a keyed request was leased");
    leases.return_all().await;
    let counts: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "SELECT c.policy_id,
                c.in_flight - COALESCE((SELECT sum(r.released) FROM gateway.rate_limit_release r
                                        WHERE r.policy_id = c.policy_id), 0)::BIGINT,
                (SELECT sum(s)::BIGINT FROM unnest(c.slots) s)
         FROM gateway.rate_limit_counter c WHERE c.policy_id = ANY($1) ORDER BY c.policy_id",
    )
    .bind(vec![policy, key_policy])
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected = vec![(policy, 0, 400), (key_policy, 0, 100)];
    expected.sort();
    assert_eq!(counts, expected);

    // Once the organization's policy limits tokens, nothing is leased.
    sqlx::query(
        "UPDATE gateway.rate_limit_policy
         SET tokens_per_minute = 1000000000, updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(policy)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
        .bind(policy)
        .execute(&pool)
        .await
        .unwrap();
    let taken = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.rate_limit_lease WHERE organization_id = $1",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = taken(pool.clone()).await;
    let leased_before = leased_of(&second).await;
    for task in paced_from(&service, &second, 300, "tokens", "hello").await {
        task.await.unwrap().unwrap();
    }
    assert_eq!(taken(pool.clone()).await, before, "a lease was granted");
    assert_eq!(leased_of(&second).await, leased_before);
    let windows = token_windows(&pool, org).await;
    assert_eq!(windows.len(), 1);
    assert!(windows[0].1 >= 600, "{windows:?}");
    assert_eq!(windows[0].0, windows[0].1);
}
