//! Accounting is the only application boundary allowed to invoke inference.
//! It derives all budget inputs from verified identity, bounded request data,
//! routing limits, and an immutable policy price.
use crate::{
    domain::{
        accounting::{
            AccountingError, CatalogProvenance, Currency, PriceSnapshot, QuotaScope, Reservation,
            TraceId, TraceMetadata,
        },
        catalog::ChatInputCountMode,
        identity::VerifiedIdentity,
        inference::{
            ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent, EmbeddingRequest,
            EmbeddingResponse, InferenceContext, ProviderError, ProviderErrorCategory, ToolChoice,
            ToolDefinition, Usage,
        },
        model_card::{RequestTimings, finish_reason_code, millis},
    },
    persistence::{
        AccountingStore, AccountingStoreError, ReserveResult,
        accounting::{ReservationRequest, Settlement},
    },
    provider::{ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream},
};
use chrono::Utc;
use futures::{Future, Stream};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{
    num::NonZeroU32,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

use super::settlement_retry;
use super::trace_capture::{Capture, TraceCapture};

type HmacSha256 = Hmac<Sha256>;
const ACCOUNTING_FINGERPRINT_KEY_INFO: &[u8] =
    b"qucelerate-gateway/accounting/request-fingerprint-key/v1";
// Conservative, fixed per-message byte allowance covering chat wire-format
// framing (role delimiters and other structural tokens) that real provider
// usage can include but that contributes no bytes to `ChatMessage` itself.
// Comfortably exceeds documented per-message chat-template overhead (a
// handful of tokens) across current provider families even at the smallest
// configured `max_tokens_per_byte`.
const CHAT_MESSAGE_FRAMING_OVERHEAD_BYTES: usize = 64;
/// Framing a provider wraps around each tool definition and each tool call
/// beyond its own bytes (D-032), as for messages.
const TOOL_FRAMING_OVERHEAD_BYTES: usize = 64;
/// The instructions providers add to a request that uses tools (Anthropic's
/// tool-use system prompt is several hundred tokens), bounded as bytes at
/// the route's tokens per byte, like everything else (D-032).
const TOOL_USE_OVERHEAD_BYTES: usize = 2048;

fn derive_accounting_fingerprint_key(secret: &[u8]) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(None, secret);
    let mut key = [0_u8; 32];
    hkdf.expand(ACCOUNTING_FINGERPRINT_KEY_INFO, &mut key)
        .expect("HKDF-SHA256 supports a 32-byte output");
    key
}

pub trait TokenizerBound: Send + Sync {
    fn upper_bound(&self, request: &ChatRequest) -> Result<u64, AccountingServiceError>;
    fn upper_bound_embeddings(
        &self,
        request: &EmbeddingRequest,
    ) -> Result<u64, AccountingServiceError> {
        let _ = request;
        Err(AccountingServiceError::InputBoundExceeded)
    }
}

#[derive(Clone)]
pub struct ConfiguredTokenizerBound {
    max_tokens_per_byte: NonZeroU32,
}
impl ConfiguredTokenizerBound {
    pub fn new(max_tokens_per_byte: NonZeroU32) -> Self {
        Self {
            max_tokens_per_byte,
        }
    }
}
impl TokenizerBound for ConfiguredTokenizerBound {
    fn upper_bound(&self, request: &ChatRequest) -> Result<u64, AccountingServiceError> {
        // Unlike embeddings' flat input strings, chat messages are framed by
        // the provider's wire format with per-message role delimiters and
        // other structural tokens that contribute to real usage but have no
        // corresponding bytes in `ChatMessage`. A fixed conservative
        // allowance per message keeps this an actual upper bound instead of
        // just a bound on content bytes.
        let framing_bytes = request
            .messages
            .len()
            .checked_mul(CHAT_MESSAGE_FRAMING_OVERHEAD_BYTES)
            .ok_or(AccountingServiceError::InputBoundExceeded)?;
        // Tools add their definitions and calls, each framed, and the
        // providers' own tool instructions (D-032).
        let calls = request
            .messages
            .iter()
            .map(|message| message.tool_calls.len())
            .sum::<usize>();
        let tool_framing = if request.uses_tools() {
            request
                .tools
                .len()
                .checked_add(calls)
                .and_then(|count| count.checked_mul(TOOL_FRAMING_OVERHEAD_BYTES))
                .and_then(|bytes| bytes.checked_add(TOOL_USE_OVERHEAD_BYTES))
                .ok_or(AccountingServiceError::InputBoundExceeded)?
        } else {
            0
        };
        let bytes = request_bytes(request)?
            .checked_add(framing_bytes)
            .and_then(|bytes| bytes.checked_add(tool_framing))
            .ok_or(AccountingServiceError::InputBoundExceeded)?;
        let bytes = u64::try_from(bytes).map_err(|_| AccountingServiceError::InputBoundExceeded)?;
        bytes
            .checked_mul(u64::from(self.max_tokens_per_byte.get()))
            .ok_or(AccountingServiceError::InputBoundExceeded)
    }

    fn upper_bound_embeddings(
        &self,
        request: &EmbeddingRequest,
    ) -> Result<u64, AccountingServiceError> {
        let bytes = request
            .input
            .iter()
            .map(String::len)
            .try_fold(0usize, usize::checked_add)
            .ok_or(AccountingServiceError::InputBoundExceeded)?;
        let bytes = u64::try_from(bytes.saturating_add(request.input.len()))
            .map_err(|_| AccountingServiceError::InputBoundExceeded)?;
        bytes
            .checked_mul(u64::from(self.max_tokens_per_byte.get()))
            .ok_or(AccountingServiceError::InputBoundExceeded)
    }
}

#[derive(Clone)]
pub struct AccountingPolicy {
    max_input_bytes: usize,
    max_output_tokens: NonZeroU32,
    reservation_ttl: Duration,
    price: PriceSnapshot,
    currency: Currency,
    fingerprint_key: [u8; 32],
    tokenizer: Arc<dyn TokenizerBound>,
    tokenizer_name: String,
    chat_input_count_mode: Option<ChatInputCountMode>,
    route: Option<(String, String)>,
    catalog_provenance: Option<CatalogProvenance>,
    reasoning_allowance: u32,
    /// The route's highest price, held when the request may fail over to a
    /// dearer deployment than the one it is first sent to (D-023).
    hold_price: Option<PriceSnapshot>,
}
impl AccountingPolicy {
    pub fn new(
        max_input_bytes: usize,
        max_output_tokens: NonZeroU32,
        reservation_ttl: Duration,
        price: PriceSnapshot,
        currency: Currency,
        fingerprint_key: [u8; 32],
    ) -> Result<Self, AccountingServiceError> {
        if max_input_bytes == 0 || reservation_ttl.is_zero() {
            return Err(AccountingServiceError::InvalidPolicy);
        }
        Ok(Self {
            max_input_bytes,
            max_output_tokens,
            reservation_ttl,
            price,
            currency,
            fingerprint_key,
            tokenizer: Arc::new(ConfiguredTokenizerBound::new(NonZeroU32::new(1).unwrap())),
            tokenizer_name: "configured".into(),
            chat_input_count_mode: None,
            route: None,
            catalog_provenance: None,
            reasoning_allowance: 0,
            hold_price: None,
        })
    }
    pub fn with_tokenizer(mut self, tokenizer: Arc<dyn TokenizerBound>) -> Self {
        self.tokenizer = tokenizer;
        self
    }
    pub fn with_chat_input_count_mode(mut self, mode: ChatInputCountMode) -> Self {
        self.chat_input_count_mode = Some(mode);
        self
    }
    pub fn with_route(mut self, deployment: impl Into<String>, model: impl Into<String>) -> Self {
        self.route = Some((deployment.into(), model.into()));
        self
    }

    pub(crate) fn with_catalog(
        mut self,
        deployment: impl Into<String>,
        model: impl Into<String>,
        price: PriceSnapshot,
        currency: Currency,
        provenance: CatalogProvenance,
    ) -> Self {
        self.route = Some((deployment.into(), model.into()));
        self.price = price;
        self.currency = currency;
        self.catalog_provenance = Some(provenance);
        self.chat_input_count_mode = self.catalog_provenance.as_ref().map(|p| {
            p.chat_input_count_mode
                .parse()
                .unwrap_or(ChatInputCountMode::Unsupported)
        });
        self
    }

    pub(crate) fn with_capability_policy(
        mut self,
        max_input_bytes: usize,
        max_output_tokens: NonZeroU32,
        max_tokens_per_byte: NonZeroU32,
        tokenizer: &str,
        reservation_ttl: Duration,
    ) -> Self {
        self.max_input_bytes = max_input_bytes;
        self.max_output_tokens = max_output_tokens;
        self.tokenizer = Arc::new(ConfiguredTokenizerBound::new(max_tokens_per_byte));
        self.tokenizer_name = tokenizer.to_owned();
        self.reservation_ttl = reservation_ttl;
        self
    }

    /// Reasoning tokens to reserve beyond each chat request's output cap,
    /// for a model whose reasoning the cap does not bound.
    pub fn with_reasoning_allowance(mut self, tokens: u32) -> Self {
        self.reasoning_allowance = tokens;
        self
    }

    /// Holds requests at `price`, a route's highest, while settling them
    /// at the serving deployment's own (D-023).
    pub(crate) fn with_hold_price(mut self, price: Option<PriceSnapshot>) -> Self {
        self.hold_price = price;
        self
    }

    pub fn for_tests() -> Self {
        Self::new(
            128 * 1024,
            NonZeroU32::new(4096).unwrap(),
            Duration::from_secs(300),
            PriceSnapshot::new(1, 1, 1).unwrap(),
            Currency::new("CRD").unwrap(),
            [7; 32],
        )
        .unwrap()
    }

    pub(crate) fn for_runtime(secret: &[u8]) -> Self {
        let key = derive_accounting_fingerprint_key(secret);
        Self::new(
            128 * 1024,
            NonZeroU32::new(4096).unwrap(),
            Duration::from_secs(300),
            PriceSnapshot::new(1, 1, 1).unwrap(),
            Currency::new("CRD").unwrap(),
            key,
        )
        .unwrap()
    }
}

#[derive(Debug, Error)]
pub enum AccountingServiceError {
    #[error(transparent)]
    Store(#[from] AccountingStoreError),
    #[error(transparent)]
    Domain(#[from] AccountingError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("accounting policy is invalid")]
    InvalidPolicy,
    #[error("missing accounting configuration: {0}")]
    Configuration(String),
    #[error("request exceeds the finite accounting input bound")]
    InputBoundExceeded,
    #[error("provider output exceeded the routed output cap")]
    OutputCapExceeded,
    #[error("accounting finalization failed")]
    Finalization,
    #[error("idempotent request is already claimed by another invocation")]
    AlreadyClaimed,
    #[error("route has no configured accounting policy")]
    RouteNotConfigured,
}

#[derive(Clone, Debug)]
pub struct AccountedChatRequest {
    pub idempotency_key: String,
}

impl AccountedChatRequest {
    pub fn new(idempotency_key: impl Into<String>) -> Self {
        Self {
            idempotency_key: idempotency_key.into(),
        }
    }
}

#[derive(Clone)]
pub struct AccountingService {
    store: AccountingStore,
    provider: Arc<dyn InferenceProviderAdapter>,
    policy: AccountingPolicy,
    /// Records traced chat requests; none means nothing is traced.
    capture: Option<TraceCapture>,
}

impl AccountingService {
    pub fn new(
        store: AccountingStore,
        provider: Arc<dyn InferenceProviderAdapter>,
        policy: AccountingPolicy,
    ) -> Self {
        Self {
            store,
            provider,
            policy,
            capture: None,
        }
    }

    /// The same service, tracing chat and embeddings requests through
    /// `capture`.
    pub fn with_capture(mut self, capture: TraceCapture) -> Self {
        self.capture = Some(capture);
        self
    }

    pub fn store(&self) -> &AccountingStore {
        &self.store
    }

    pub(crate) fn with_policy(&self, policy: AccountingPolicy) -> Self {
        Self {
            store: self.store.clone(),
            provider: self.provider.clone(),
            policy,
            capture: self.capture.clone(),
        }
    }

    pub(crate) fn with_provider(&self, provider: Arc<dyn InferenceProviderAdapter>) -> Self {
        Self {
            store: self.store.clone(),
            provider,
            policy: self.policy.clone(),
            capture: self.capture.clone(),
        }
    }

    pub(crate) fn provider(&self) -> Arc<dyn InferenceProviderAdapter> {
        self.provider.clone()
    }

    pub(crate) fn policy(&self) -> AccountingPolicy {
        self.policy.clone()
    }

    pub(crate) fn preflight(
        &self,
        context: &InferenceContext,
        request: Option<&ChatRequest>,
    ) -> Result<(), AccountingServiceError> {
        self.ensure_supported(context)?;
        if let Some(request) = request {
            self.provider.validate_chat_request(context, request)?;
        }
        Ok(())
    }

    fn ensure_supported(&self, context: &InferenceContext) -> Result<(), AccountingServiceError> {
        if !self.provider.supports(context) {
            return Err(AccountingServiceError::Provider(ProviderError::new(
                ProviderErrorCategory::Unavailable,
            )));
        }
        Ok(())
    }

    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(u64, Option<ChatInputCountMode>), AccountingServiceError> {
        match self.policy.chat_input_count_mode {
            // A request with tools is reserved against the byte bound even
            // where the route counts input with the provider: a count may
            // leave out the instructions a provider adds for tools, and the
            // bound includes an allowance for them (D-032). Settled against
            // reported usage, as `Unsupported` is.
            Some(ChatInputCountMode::ProviderApiExact) if request.uses_tools() => {
                let count = self.policy.tokenizer.upper_bound(request)?;
                Ok((count, Some(ChatInputCountMode::Unsupported)))
            }
            Some(ChatInputCountMode::ProviderApiExact) => {
                let count = self
                    .provider
                    .count_chat_input_tokens(context, request)
                    .await?;
                i64::try_from(count)
                    .map_err(|_| AccountingServiceError::Domain(AccountingError::Overflow))?;
                Ok((count, Some(ChatInputCountMode::ProviderApiExact)))
            }
            // No provider-side exact counting endpoint. Reserve against the
            // catalog-configured, conservative bytes-per-token upper bound
            // instead, the same mechanism `plan_embedding` already uses for
            // embeddings. This is a true upper bound, not a guess, so it is
            // safe to use for the hard budget reservation; settlement still
            // charges the provider's actual reported usage rather than
            // requiring it to match this bound exactly (see `chat`'s and
            // `AccountedStream`'s use of `count_mode` to skip the
            // exact-match drift check for this mode).
            Some(ChatInputCountMode::Unsupported) => {
                let count = self.policy.tokenizer.upper_bound(request)?;
                Ok((count, Some(ChatInputCountMode::Unsupported)))
            }
            // No catalog policy at all (e.g. a non-catalog-backed route).
            // Unlike `Unsupported`, there is no configured tokenizer bound
            // to trust here, so chat accounting cannot proceed.
            None => Err(AccountingServiceError::Provider(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ))),
        }
    }

    fn prepare_chat(
        &self,
        identity: &VerifiedIdentity,
        context: &InferenceContext,
        request: &ChatRequest,
        idempotency_key: &str,
    ) -> Result<(QuotaScope, NonZeroU32), AccountingServiceError> {
        if idempotency_key.trim().is_empty() || idempotency_key.len() > 255 {
            return Err(AccountingServiceError::Store(AccountingStoreError::Domain(
                crate::domain::accounting::AccountingError::InvalidIdempotency,
            )));
        }
        let input_bytes = request_bytes(request)?;
        if input_bytes > self.policy.max_input_bytes {
            return Err(AccountingServiceError::InputBoundExceeded);
        }
        if let Some((deployment, model)) = &self.policy.route {
            if deployment != context.deployment.deployment() || model != context.deployment.model()
            {
                return Err(AccountingServiceError::RouteNotConfigured);
            }
        }
        let cap = request
            .max_tokens
            .map_or(self.policy.max_output_tokens.get(), |value| {
                value.min(self.policy.max_output_tokens.get())
            });
        let output_cap = NonZeroU32::new(cap).ok_or(AccountingServiceError::OutputCapExceeded)?;
        Ok((QuotaScope::from_identity(identity)?, output_cap))
    }

    fn plan(
        &self,
        identity: &VerifiedIdentity,
        context: &InferenceContext,
        request: &ChatRequest,
        input_bound: u64,
        count_mode: Option<ChatInputCountMode>,
        idempotency_key: &str,
        scope: QuotaScope,
        output_cap: NonZeroU32,
    ) -> Result<ReservationRequest, AccountingServiceError> {
        let trace_id = TraceId::new();
        // A route that may fail over holds the input any of its deployments
        // may count: the byte bound, which every count is under (its
        // tokens-per-byte is the route's largest), above an exact count.
        let hold_input_bound = match self.policy.hold_price {
            Some(_) if count_mode == Some(ChatInputCountMode::ProviderApiExact) => {
                Some(self.policy.tokenizer.upper_bound(request)?.max(input_bound))
            }
            _ => None,
        };
        Ok(ReservationRequest {
            scope,
            idempotency_key: idempotency_key.to_owned(),
            fingerprint: self.fingerprint(
                identity,
                context,
                request,
                output_cap,
                input_bound,
                count_mode,
                idempotency_key,
            )?,
            price: self.policy.price.clone(),
            currency: self.policy.currency.clone(),
            deployment: context.deployment.deployment().to_owned(),
            model: context.deployment.model().to_owned(),
            input_bound,
            output_token_cap: Some(output_cap),
            reserve_output: true,
            reasoning_allowance: self.policy.reasoning_allowance,
            ttl: self.policy.reservation_ttl,
            trace_id,
            correlation_id: context.correlation_id.0,
            trace_metadata: TraceMetadata::new(
                input_bound,
                Some(output_cap.get()),
                self.policy.currency.clone(),
            ),
            catalog_provenance: self.policy.catalog_provenance.clone(),
            api_key_id: identity.api_key_id.map(|id| id.0),
            end_user: identity.end_user.clone(),
            hold_price: self.policy.hold_price.clone(),
            hold_input_bound,
        })
    }

    fn plan_embedding(
        &self,
        identity: &VerifiedIdentity,
        context: &InferenceContext,
        request: &EmbeddingRequest,
        idempotency_key: &str,
    ) -> Result<ReservationRequest, AccountingServiceError> {
        if idempotency_key.trim().is_empty() || idempotency_key.len() > 255 {
            return Err(AccountingServiceError::Store(AccountingStoreError::Domain(
                AccountingError::InvalidIdempotency,
            )));
        }
        let input_bytes = request
            .input
            .iter()
            .map(String::len)
            .try_fold(0usize, usize::checked_add)
            .ok_or(AccountingServiceError::InputBoundExceeded)?;
        if input_bytes > self.policy.max_input_bytes {
            return Err(AccountingServiceError::InputBoundExceeded);
        }
        if let Some((deployment, model)) = &self.policy.route {
            if deployment != context.deployment.deployment() || model != context.deployment.model()
            {
                return Err(AccountingServiceError::RouteNotConfigured);
            }
        }
        let input_bound = self.policy.tokenizer.upper_bound_embeddings(request)?;
        let mut mac = HmacSha256::new_from_slice(&self.policy.fingerprint_key)
            .map_err(|_| AccountingServiceError::InvalidPolicy)?;
        frame(&mut mac, b"gateway-accounting-embedding-v1");
        frame(&mut mac, identity.organization_id.0.as_bytes());
        frame(&mut mac, identity.service_account_id.0.as_bytes());
        // Only where named, so requests without one keep their fingerprints.
        if let Some(end_user) = &identity.end_user {
            frame(&mut mac, b"end-user");
            frame(&mut mac, end_user.as_bytes());
        }
        if let Some(team) = identity.team_id {
            frame(&mut mac, team.0.as_bytes());
        } else {
            frame(&mut mac, b"-");
        }
        frame(&mut mac, context.deployment.deployment().as_bytes());
        frame(&mut mac, context.deployment.model().as_bytes());
        frame(&mut mac, idempotency_key.as_bytes());
        frame(&mut mac, &input_bound.to_be_bytes());
        frame(&mut mac, self.policy.price.version.as_bytes());
        frame(
            &mut mac,
            match self.policy.price.confidence {
                crate::domain::accounting::PricingConfidence::Known => b"known",
                crate::domain::accounting::PricingConfidence::Conservative => b"conservative",
            },
        );
        frame(&mut mac, self.policy.currency.as_str().as_bytes());
        frame(&mut mac, self.policy.tokenizer_name.as_bytes());
        if let Some(provenance) = &self.policy.catalog_provenance {
            frame(&mut mac, provenance.release_id.as_bytes());
            frame(&mut mac, provenance.route_id.as_bytes());
            frame(&mut mac, provenance.deployment_id.as_bytes());
            frame(&mut mac, provenance.price_id.as_bytes());
            frame(&mut mac, &provenance.digest);
            frame(&mut mac, provenance.operation.as_bytes());
            frame(&mut mac, provenance.family.as_bytes());
            frame(&mut mac, provenance.price_source.as_bytes());
        }
        for input in &request.input {
            frame(&mut mac, input.as_bytes());
        }
        let trace_id = TraceId::new();
        Ok(ReservationRequest {
            scope: QuotaScope::from_identity(identity)?,
            idempotency_key: idempotency_key.to_owned(),
            fingerprint: mac.finalize().into_bytes().into(),
            price: self.policy.price.clone(),
            currency: self.policy.currency.clone(),
            deployment: context.deployment.deployment().to_owned(),
            model: context.deployment.model().to_owned(),
            input_bound,
            output_token_cap: None,
            reserve_output: false,
            reasoning_allowance: 0,
            ttl: self.policy.reservation_ttl,
            trace_id,
            correlation_id: context.correlation_id.0,
            trace_metadata: TraceMetadata::new(input_bound, None, self.policy.currency.clone()),
            catalog_provenance: self.policy.catalog_provenance.clone(),
            api_key_id: identity.api_key_id.map(|id| id.0),
            end_user: identity.end_user.clone(),
            hold_price: self.policy.hold_price.clone(),
            hold_input_bound: None,
        })
    }

    /// The reservation of one batch line: the single request's own bounds
    /// (the strict input upper bound, never a provider count, and the
    /// output cap), held for `ttl`, the batch's completion window and
    /// grace.
    pub(crate) fn plan_batch_line(
        &self,
        identity: &VerifiedIdentity,
        context: &InferenceContext,
        request: &crate::domain::batch::LineRequest,
        idempotency_key: &str,
        ttl: Duration,
    ) -> Result<ReservationRequest, AccountingServiceError> {
        let mut plan = match request {
            crate::domain::batch::LineRequest::Chat(request) => {
                let (scope, cap) =
                    self.prepare_chat(identity, context, request, idempotency_key)?;
                let input_bound = self.policy.tokenizer.upper_bound(request)?;
                self.plan(
                    identity,
                    context,
                    request,
                    input_bound,
                    Some(ChatInputCountMode::Unsupported),
                    idempotency_key,
                    scope,
                    cap,
                )?
            }
            crate::domain::batch::LineRequest::Embeddings(request) => {
                self.plan_embedding(identity, context, request, idempotency_key)?
            }
        };
        // The bound is the tokenizer's, whatever the route counts singly.
        if let Some(provenance) = plan.catalog_provenance.as_mut() {
            provenance.chat_input_count_mode = ChatInputCountMode::Unsupported.as_str().to_owned();
        }
        plan.ttl = ttl;
        // A batch line is sent to one deployment and never fails over.
        plan.hold_price = None;
        plan.hold_input_bound = None;
        Ok(plan)
    }

    pub async fn embeddings(
        &self,
        identity: &VerifiedIdentity,
        context: InferenceContext,
        request: EmbeddingRequest,
        accounted: AccountedChatRequest,
    ) -> Result<EmbeddingResponse, AccountingServiceError> {
        self.embeddings_routed(identity, context, request, accounted, None)
            .await
    }

    /// [`Self::embeddings`], failing over to `fallbacks` (D-023).
    pub(crate) async fn embeddings_routed(
        &self,
        identity: &VerifiedIdentity,
        mut context: InferenceContext,
        request: EmbeddingRequest,
        accounted: AccountedChatRequest,
        mut fallbacks: Option<Box<dyn Fallbacks>>,
    ) -> Result<EmbeddingResponse, AccountingServiceError> {
        self.ensure_supported(&context)?;
        let plan = self.plan_embedding(identity, &context, &request, &accounted.idempotency_key)?;
        let mut capture = match &self.capture {
            Some(capture) => capture.begin_embeddings(identity, &request).await,
            None => None,
        };
        let mut reservation = self.reserve_claim(plan).await?;
        if let Some(capture) = &mut capture {
            capture.provider_started();
        }
        let mut service = self.clone();
        let mut failed_over = false;
        loop {
            let started = Instant::now();
            let result = within(
                deadline(&reservation, failed_over),
                service
                    .provider
                    .embeddings(context.clone(), request.clone()),
            )
            .await;
            let timings = RequestTimings {
                provider_latency_ms: Some(millis(started.elapsed())),
                ..RequestTimings::default()
            };
            match result {
                Ok(response) => {
                    if let Some(capture) = capture.take() {
                        capture.finish_embeddings(reservation.trace_id.0, Some(&response));
                    }
                    settle(
                        &service.store,
                        &reservation,
                        response.usage().map(|s| s.usage()),
                        response.provider_request_id().map(str::to_owned),
                        None,
                        timings,
                    )
                    .await?;
                    return Ok(response);
                }
                Err(error) => {
                    if let Some(next) = service
                        .fail_over(
                            identity,
                            &mut fallbacks,
                            &reservation,
                            &error,
                            timings.clone(),
                            Attempt::Embeddings(&request),
                        )
                        .await?
                    {
                        (service, context, reservation) =
                            (next.service, next.context, next.reservation);
                        failed_over = true;
                        continue;
                    }
                    if let Some(capture) = capture.take() {
                        capture.finish_embeddings(reservation.trace_id.0, None);
                    }
                    service.failed(&reservation, &error, timings).await?;
                    return Err(error.into());
                }
            }
        }
    }

    fn fingerprint(
        &self,
        identity: &VerifiedIdentity,
        context: &InferenceContext,
        request: &ChatRequest,
        cap: NonZeroU32,
        input_bound: u64,
        count_mode: Option<ChatInputCountMode>,
        idempotency_key: &str,
    ) -> Result<[u8; 32], AccountingServiceError> {
        let mut mac = HmacSha256::new_from_slice(&self.policy.fingerprint_key)
            .map_err(|_| AccountingServiceError::InvalidPolicy)?;
        frame(&mut mac, b"gateway-accounting-v1");
        frame(&mut mac, identity.organization_id.0.as_bytes());
        frame(&mut mac, identity.service_account_id.0.as_bytes());
        // Only where named, so requests without one keep their fingerprints.
        if let Some(end_user) = &identity.end_user {
            frame(&mut mac, b"end-user");
            frame(&mut mac, end_user.as_bytes());
        }
        if let Some(team) = identity.team_id {
            frame(&mut mac, team.0.as_bytes());
        } else {
            frame(&mut mac, b"-");
        }
        frame(&mut mac, context.deployment.deployment().as_bytes());
        frame(&mut mac, context.deployment.model().as_bytes());
        frame(&mut mac, idempotency_key.as_bytes());
        frame(&mut mac, &cap.get().to_be_bytes());
        frame(&mut mac, &input_bound.to_be_bytes());
        frame(&mut mac, self.policy.price.version.as_bytes());
        frame(
            &mut mac,
            match self.policy.price.confidence {
                crate::domain::accounting::PricingConfidence::Known => &b"known"[..],
                crate::domain::accounting::PricingConfidence::Conservative => &b"conservative"[..],
            },
        );
        frame(&mut mac, self.policy.currency.as_str().as_bytes());
        frame(&mut mac, self.policy.tokenizer_name.as_bytes());
        frame(
            &mut mac,
            count_mode.map_or(b"legacy-estimate".as_slice(), |mode| {
                mode.as_str().as_bytes()
            }),
        );
        if let Some(provenance) = &self.policy.catalog_provenance {
            frame(&mut mac, provenance.release_id.as_bytes());
            frame(&mut mac, provenance.route_id.as_bytes());
            frame(&mut mac, provenance.deployment_id.as_bytes());
            frame(&mut mac, provenance.price_id.as_bytes());
            frame(&mut mac, &provenance.digest);
            frame(&mut mac, provenance.operation.as_bytes());
            frame(&mut mac, provenance.family.as_bytes());
            frame(&mut mac, provenance.price_source.as_bytes());
        }
        if let Some(temp) = request.temperature {
            frame(&mut mac, &temp.to_bits().to_be_bytes());
        } else {
            frame(&mut mac, b"-");
        }
        for message in &request.messages {
            frame(&mut mac, &[role_tag(message.role)]);
            frame(&mut mac, message.content.as_bytes());
            frame(&mut mac, message.name.as_deref().unwrap_or("-").as_bytes());
            frame(
                &mut mac,
                message.tool_call_id.as_deref().unwrap_or("-").as_bytes(),
            );
            // Only where present, so requests without tools keep their
            // fingerprints (D-032).
            for call in &message.tool_calls {
                frame(&mut mac, b"tool-call");
                frame(&mut mac, call.id.as_deref().unwrap_or("-").as_bytes());
                frame(&mut mac, call.name.as_deref().unwrap_or("-").as_bytes());
                frame(&mut mac, call.arguments.as_bytes());
            }
        }
        frame_tools(&mut mac, request);
        Ok(mac.finalize().into_bytes().into())
    }

    pub async fn chat(
        &self,
        identity: &VerifiedIdentity,
        context: InferenceContext,
        request: ChatRequest,
        accounted: AccountedChatRequest,
    ) -> Result<ChatResponse, AccountingServiceError> {
        self.chat_routed(identity, context, request, accounted, None)
            .await
    }

    /// [`Self::chat`], failing over to `fallbacks` (D-023).
    pub(crate) async fn chat_routed(
        &self,
        identity: &VerifiedIdentity,
        mut context: InferenceContext,
        mut request: ChatRequest,
        accounted: AccountedChatRequest,
        mut fallbacks: Option<Box<dyn Fallbacks>>,
    ) -> Result<ChatResponse, AccountingServiceError> {
        self.ensure_supported(&context)?;
        self.provider.validate_chat_request(&context, &request)?;
        let (scope, output_cap) =
            self.prepare_chat(identity, &context, &request, &accounted.idempotency_key)?;
        let (mut input_bound, mut count_mode) =
            self.count_chat_input_tokens(&context, &request).await?;
        let plan = self.plan(
            identity,
            &context,
            &request,
            input_bound,
            count_mode,
            &accounted.idempotency_key,
            scope,
            output_cap,
        )?;
        let mut capture = self.begin_capture(identity, &request).await;
        let mut reservation = self.reserve_claim(plan).await?;
        // A later attempt is planned (and fingerprinted) from the request as
        // it came, so a retry of it replays rather than conflicts; only what
        // is sent carries the cap its reservation granted.
        let original = request.clone();
        request.max_tokens = Some(
            reservation
                .output_token_cap
                .ok_or(AccountingServiceError::OutputCapExceeded)?
                .get(),
        );
        if let Some(capture) = &mut capture {
            capture.provider_started();
        }
        let mut service = self.clone();
        let mut failed_over = false;
        let (result, timings) = loop {
            let started = Instant::now();
            let result = within(
                deadline(&reservation, failed_over),
                service.provider.chat(context.clone(), request.clone()),
            )
            .await;
            let timings = RequestTimings {
                provider_latency_ms: Some(millis(started.elapsed())),
                first_event_ms: None,
                finish_reason: result
                    .as_ref()
                    .ok()
                    .and_then(|response| response.choices().first())
                    .and_then(|choice| choice.finish_reason.as_ref())
                    .map(|reason| finish_reason_code(reason.as_str())),
            };
            if let Err(error) = &result {
                if let Some(next) = service
                    .fail_over(
                        identity,
                        &mut fallbacks,
                        &reservation,
                        error,
                        timings.clone(),
                        Attempt::Chat(&original),
                    )
                    .await?
                {
                    (service, context, reservation) =
                        (next.service, next.context, next.reservation);
                    if let Some(bound) = next.input_bound {
                        (input_bound, count_mode) = bound;
                    }
                    request.max_tokens = reservation.output_token_cap.map(NonZeroU32::get);
                    failed_over = true;
                    continue;
                }
            }
            break (result, timings);
        };
        if let Some(capture) = capture {
            capture.finish(reservation.trace_id.0, result.as_ref().ok());
        }
        match result {
            Ok(response) => {
                let usage = response.usage().map(|s| s.usage());
                // Exact drift-detection only applies when `input_bound` came
                // from the provider's own counting endpoint (`ProviderApiExact`),
                // where it should match subsequent actual usage exactly. A
                // `Unsupported`-mode `input_bound` is a deliberately
                // conservative upper bound, not expected to match, so it
                // settles against actual usage instead (like embeddings).
                if count_mode == Some(ChatInputCountMode::ProviderApiExact)
                    && usage.as_ref().is_some_and(|usage| {
                        counted_prompt(usage).is_some_and(|value| value != input_bound)
                    })
                {
                    reconcile_input_drift(
                        &service.store,
                        &reservation,
                        usage,
                        response.provider_request_id().map(str::to_owned),
                        None,
                        timings,
                    )
                    .await?;
                    return Err(AccountingServiceError::Finalization);
                }
                settle(
                    &service.store,
                    &reservation,
                    usage,
                    response.provider_request_id().map(str::to_owned),
                    None,
                    timings,
                )
                .await?;
                Ok(response)
            }
            Err(error) => {
                service.failed(&reservation, &error, timings).await?;
                Err(error.into())
            }
        }
    }

    pub async fn chat_stream(
        &self,
        identity: &VerifiedIdentity,
        context: InferenceContext,
        request: ChatRequest,
        accounted: AccountedChatRequest,
    ) -> Result<ChatEventStream, AccountingServiceError> {
        self.chat_stream_routed(identity, context, request, accounted, None)
            .await
    }

    /// [`Self::chat_stream`], failing over to `fallbacks` until a stream
    /// has started, never after (D-023).
    pub(crate) async fn chat_stream_routed(
        &self,
        identity: &VerifiedIdentity,
        mut context: InferenceContext,
        mut request: ChatRequest,
        accounted: AccountedChatRequest,
        mut fallbacks: Option<Box<dyn Fallbacks>>,
    ) -> Result<ChatEventStream, AccountingServiceError> {
        self.ensure_supported(&context)?;
        self.provider.validate_chat_request(&context, &request)?;
        let (scope, output_cap) =
            self.prepare_chat(identity, &context, &request, &accounted.idempotency_key)?;
        let (mut input_bound, mut count_mode) =
            self.count_chat_input_tokens(&context, &request).await?;
        let plan = self.plan(
            identity,
            &context,
            &request,
            input_bound,
            count_mode,
            &accounted.idempotency_key,
            scope,
            output_cap,
        )?;
        let mut capture = self.begin_capture(identity, &request).await;
        let mut reservation = self.reserve_claim(plan).await?;
        // A later attempt is planned (and fingerprinted) from the request as
        // it came, so a retry of it replays rather than conflicts; only what
        // is sent carries the cap its reservation granted.
        let original = request.clone();
        request.max_tokens = Some(
            reservation
                .output_token_cap
                .ok_or(AccountingServiceError::OutputCapExceeded)?
                .get(),
        );
        if let Some(capture) = &mut capture {
            capture.provider_started();
        }
        let mut service = self.clone();
        let mut failed_over = false;
        let (inner, started) = loop {
            let started = Instant::now();
            match within(
                deadline(&reservation, failed_over),
                service
                    .provider
                    .chat_stream(context.clone(), request.clone()),
            )
            .await
            {
                Ok(stream) => break (stream, started),
                Err(error) => {
                    let timings = RequestTimings {
                        provider_latency_ms: Some(millis(started.elapsed())),
                        ..RequestTimings::default()
                    };
                    if let Some(next) = service
                        .fail_over(
                            identity,
                            &mut fallbacks,
                            &reservation,
                            &error,
                            timings.clone(),
                            Attempt::Chat(&original),
                        )
                        .await?
                    {
                        (service, context, reservation) =
                            (next.service, next.context, next.reservation);
                        if let Some(bound) = next.input_bound {
                            (input_bound, count_mode) = bound;
                        }
                        request.max_tokens = reservation.output_token_cap.map(NonZeroU32::get);
                        failed_over = true;
                        continue;
                    }
                    if let Some(capture) = capture {
                        capture.finish(reservation.trace_id.0, None);
                    }
                    service.failed(&reservation, &error, timings).await?;
                    return Err(error.into());
                }
            }
        };
        Ok(ValidatingChatEventStream::new(AccountedStream {
            inner,
            store: service.store.clone(),
            reservation,
            // Only exact-mode bounds are expected to match reported usage;
            // see the comment in `chat` above.
            expected_input_bound: (count_mode == Some(ChatInputCountMode::ProviderApiExact))
                .then_some(input_bound),
            pending: None,
            finalization: None,
            finalized: false,
            capture,
            started,
            first_event_ms: None,
            finish_reason: None,
        }))
    }

    /// The reservation `attempt` takes on this deployment, under `key`,
    /// and for a chat request the input bound it was planned with.
    async fn plan_attempt(
        &self,
        identity: &VerifiedIdentity,
        context: &InferenceContext,
        attempt: &Attempt<'_>,
        key: &str,
    ) -> Result<
        (
            ReservationRequest,
            Option<(u64, Option<ChatInputCountMode>)>,
        ),
        AccountingServiceError,
    > {
        self.ensure_supported(context)?;
        match attempt {
            Attempt::Chat(request) => {
                self.provider.validate_chat_request(context, request)?;
                let (scope, cap) = self.prepare_chat(identity, context, request, key)?;
                let (bound, mode) = self.count_chat_input_tokens(context, request).await?;
                let plan = self.plan(identity, context, request, bound, mode, key, scope, cap)?;
                Ok((plan, Some((bound, mode))))
            }
            Attempt::Embeddings(request) => {
                Ok((self.plan_embedding(identity, context, request, key)?, None))
            }
        }
    }

    /// The next attempt of a request whose attempt `failed`, on this
    /// service's deployment, ended in `error`: claimed on the first of
    /// `fallbacks` that can take it, when the provider cannot have started
    /// the work (D-023). `None` when `error` is final: it may follow work,
    /// there is nowhere else to go, or the attempts or the deadline are
    /// spent.
    async fn fail_over(
        &self,
        identity: &VerifiedIdentity,
        fallbacks: &mut Option<Box<dyn Fallbacks>>,
        failed: &Reservation,
        error: &ProviderError,
        timings: RequestTimings,
        attempt: Attempt<'_>,
    ) -> Result<Option<Failover>, AccountingServiceError> {
        let Some(fallbacks) = fallbacks.as_mut() else {
            return Ok(None);
        };
        let Some(reason) = failover_reason(self.provider.as_ref(), error) else {
            return Ok(None);
        };
        // The attempt's own key: unique per attempt, and found again from
        // the request's through `failover_of`.
        let key = format!("failover:{}", failed.id.0);
        // Finding and planning the next attempt (which may count its input
        // with its provider) stays within the request's deadline too.
        let found = tokio::time::timeout(remaining(failed), async {
            while let Some((next, context)) = fallbacks.next().await {
                // A deployment that cannot take this request is passed over.
                if let Ok((plan, input_bound)) =
                    next.plan_attempt(identity, &context, &attempt, &key).await
                {
                    return Some((next, context, plan, input_bound));
                }
            }
            None
        })
        .await;
        if let Ok(Some((next, context, plan, input_bound))) = found {
            let reservation = match self
                .store
                .fail_over(failed, error, plan, MAX_ROUTE_ATTEMPTS, Utc::now())
                .await
            {
                Ok(Some(reservation)) => reservation,
                Ok(None) => return Ok(None),
                // The database is unreachable for now: the failover rolled
                // back, so the failed attempt is finalized as final, through
                // the settlement retry, rather than left claimed until it
                // expires into reconciliation.
                Err(store) if settlement_retry::is_transient(&store) => {
                    tracing::warn!(
                        trace_id = %failed.trace_id.0,
                        error = %store,
                        "failover not recorded; finalizing the failed attempt"
                    );
                    return Ok(None);
                }
                Err(store) => return Err(store.into()),
            };
            log_provider_failure(failed, error);
            record_timings(&self.store, failed, timings);
            crate::metrics::global().inc(
                &crate::metrics::ROUTE_FAILOVERS,
                &[
                    crate::metrics::provider_family_label(
                        failed
                            .catalog_provenance
                            .as_ref()
                            .map(|p| p.family.as_str()),
                    ),
                    reason,
                ],
            );
            tracing::info!(
                organization_id = %failed.scope.organization_id.0,
                trace_id = %failed.trace_id.0,
                next_trace_id = %reservation.trace_id.0,
                from = %failed.deployment,
                to = %reservation.deployment,
                reason,
                "request failed over"
            );
            return Ok(Some(Failover {
                service: next,
                context,
                reservation,
                input_bound,
            }));
        }
        Ok(None)
    }

    /// The capture of `request` when its organization traces it.
    async fn begin_capture(
        &self,
        identity: &VerifiedIdentity,
        request: &ChatRequest,
    ) -> Option<Capture> {
        match &self.capture {
            Some(capture) => capture.begin(identity, request).await,
            None => None,
        }
    }

    async fn reserve_claim(
        &self,
        plan: ReservationRequest,
    ) -> Result<Reservation, AccountingServiceError> {
        // A new reservation is written claimed, in the same commit; only a
        // replay of an existing one is claimed (or found claimed) apart.
        let reservation = match self.store.reserve(plan, Utc::now(), true).await? {
            ReserveResult::Created(reservation) => return Ok(reservation),
            ReserveResult::Existing(reservation) => reservation,
        };
        match self
            .store
            .claim_exclusive(
                reservation.scope.organization_id,
                reservation.id,
                Utc::now(),
            )
            .await?
        {
            crate::persistence::accounting::ClaimOutcome::Won(reservation) => Ok(reservation),
            crate::persistence::accounting::ClaimOutcome::AlreadyClaimed(_) => {
                Err(AccountingServiceError::AlreadyClaimed)
            }
        }
    }
    /// Finalizes a failed provider request, keeping why it failed: released,
    /// charging nothing, when the provider refused it before any work (the
    /// same proof a failover needs, D-023); otherwise settled as unknown.
    async fn failed(
        &self,
        reservation: &Reservation,
        error: &ProviderError,
        timings: RequestTimings,
    ) -> Result<(), AccountingServiceError> {
        if failover_reason(self.provider.as_ref(), error).is_some() {
            release_refused(&self.store, reservation, error.clone(), timings).await?;
            return Ok(());
        }
        settle(
            &self.store,
            reservation,
            error.partial_usage(),
            error.provider_request_id().map(str::to_owned),
            Some(error.clone()),
            timings,
        )
        .await?;
        Ok(())
    }
}

/// What is left of `reservation`'s deadline.
fn remaining(reservation: &Reservation) -> Duration {
    (reservation.expires_at - Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO)
}

/// The deadline a call on `reservation` must end within: a failover
/// attempt's is what is left of the deadline its request's first
/// reservation set, which it shares (D-023); a first attempt has its
/// provider's own timeouts.
fn deadline(reservation: &Reservation, failed_over: bool) -> Option<Duration> {
    failed_over.then(|| remaining(reservation))
}

/// `call`, ended with a timeout at `deadline`. The provider may have
/// started the work by then, so the attempt is settled as any timeout is.
async fn within<T>(
    deadline: Option<Duration>,
    call: impl Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    match deadline {
        None => call.await,
        Some(deadline) => tokio::time::timeout(deadline, call)
            .await
            .unwrap_or_else(|_| Err(ProviderError::new(ProviderErrorCategory::Timeout))),
    }
}

/// Most deployments one request is tried on, its first included (D-023).
pub(crate) const MAX_ROUTE_ATTEMPTS: u8 = 3;

/// The other deployments of a request's route, in the order to fail over
/// to them (D-023).
#[async_trait::async_trait]
pub(crate) trait Fallbacks: Send {
    /// The next deployment that admits the request now, with accounting
    /// bound to it and the context to call it in; `None` when none is left.
    async fn next(&mut self) -> Option<(AccountingService, InferenceContext)>;
}

/// What an attempt sends: the chat request (with the output cap its first
/// reservation granted) or the embeddings request.
enum Attempt<'a> {
    Chat(&'a ChatRequest),
    Embeddings(&'a EmbeddingRequest),
}

/// The attempt a request failed over to.
struct Failover {
    service: AccountingService,
    context: InferenceContext,
    reservation: Reservation,
    input_bound: Option<(u64, Option<ChatInputCountMode>)>,
}

/// Why `error` may fail over, when the provider cannot have started the
/// work: the request was never sent, the provider refused it with 429, or
/// the adapter documents its status as refused before inference. Never
/// after usage was reported, a timeout, or another 5xx, since the provider
/// may have done the work and would charge twice (D-023).
fn failover_reason(
    provider: &dyn InferenceProviderAdapter,
    error: &ProviderError,
) -> Option<&'static str> {
    if error.partial_usage().is_some() || error.category() == ProviderErrorCategory::Cancelled {
        None
    } else if error.was_not_sent() {
        Some("not_sent")
    } else if error.http_status() == Some(429) {
        Some("rate_limited")
    } else if provider.refused_before_inference(error) {
        Some("refused")
    } else {
        None
    }
}

/// Logs a failed provider request with what identifies it and why it failed,
/// never the provider's message or body, which may echo the request.
fn log_provider_failure(reservation: &Reservation, error: &ProviderError) {
    tracing::warn!(
        organization_id = %reservation.scope.organization_id.0,
        trace_id = %reservation.trace_id.0,
        deployment = %reservation.deployment,
        provider = error.provider().unwrap_or("unknown"),
        category = error.category().as_str(),
        http_status = error.http_status(),
        error_type = error.error_type(),
        provider_request_id = error.provider_request_id(),
        "provider request failed"
    );
}

/// Most timing writes in flight at once; past it, timings are dropped.
const MAX_PENDING_TIMING_WRITES: usize = 256;

/// Records how a request went on its trace, off the request's path: the
/// write runs in the background, so a slow database never delays the
/// response or its settlement. Best effort: when too many writes are
/// pending, or one fails, the timings are dropped and the request is
/// unaffected (the card counts only requests with timings).
fn record_timings(store: &AccountingStore, reservation: &Reservation, timings: RequestTimings) {
    static PENDING: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    if timings.is_empty() {
        return;
    }
    let pending =
        PENDING.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_TIMING_WRITES)));
    let Ok(permit) = pending.clone().try_acquire_owned() else {
        tracing::debug!(trace_id = %reservation.trace_id.0, "request timings dropped: too many pending");
        return;
    };
    let store = store.clone();
    let organization_id = reservation.scope.organization_id;
    let trace_id = reservation.trace_id;
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = store
            .record_request_timings(organization_id, trace_id, &timings)
            .await
        {
            tracing::warn!(error = ?error, trace_id = %trace_id.0, "request timings not recorded");
        }
    });
}

/// Settles `reservation`, retrying transient failures under one
/// finalization id; see [`settlement_retry`].
async fn settle(
    store: &AccountingStore,
    reservation: &Reservation,
    usage: Option<Usage>,
    provider_request_id: Option<String>,
    provider_error: Option<ProviderError>,
    timings: RequestTimings,
) -> Result<(), AccountingStoreError> {
    if let Some(error) = &provider_error {
        log_provider_failure(reservation, error);
    }
    let store = store.clone();
    let (organization_id, reservation_id) = (reservation.scope.organization_id, reservation.id);
    let settlement = Settlement {
        finalization_id: Uuid::new_v4(),
        usage,
        trace_id: reservation.trace_id,
        provider_request_id,
        provider_error,
        timings,
    };
    settlement_retry::finalize(reservation_id, move || {
        let (store, settlement) = (store.clone(), settlement.clone());
        async move {
            // Counted when this finalization commits or replays, which is
            // once: an attempt whose reply was lost is replayed by the next.
            let result = store
                .settle(organization_id, reservation_id, settlement, Utc::now())
                .await;
            let outcome = match &result {
                Ok(crate::domain::accounting::ReservationState::Unknown) => Some("unknown"),
                Ok(_) => Some("settled"),
                Err(AccountingStoreError::BoundExceeded) => Some("bound_breach"),
                Err(_) => None,
            };
            if let Some(outcome) = outcome {
                crate::metrics::global().inc(&crate::metrics::SETTLEMENTS, &[outcome]);
            }
            result.map(|_| ())
        }
    })
    .await
}

/// Releases `reservation`, which its provider refused before any work,
/// retrying transient failures under one finalization id.
async fn release_refused(
    store: &AccountingStore,
    reservation: &Reservation,
    error: ProviderError,
    timings: RequestTimings,
) -> Result<(), AccountingStoreError> {
    log_provider_failure(reservation, &error);
    let store = store.clone();
    let (organization_id, reservation_id) = (reservation.scope.organization_id, reservation.id);
    let settlement = Settlement {
        finalization_id: Uuid::new_v4(),
        usage: None,
        trace_id: reservation.trace_id,
        provider_request_id: error.provider_request_id().map(str::to_owned),
        provider_error: Some(error),
        timings,
    };
    settlement_retry::finalize(reservation_id, move || {
        let (store, settlement) = (store.clone(), settlement.clone());
        async move {
            let result = store
                .release_refused(organization_id, reservation_id, settlement)
                .await;
            let outcome = match &result {
                Ok(crate::domain::accounting::ReservationState::Unknown) => Some("unknown"),
                Ok(_) => Some("refused"),
                Err(_) => None,
            };
            if let Some(outcome) = outcome {
                crate::metrics::global().inc(&crate::metrics::SETTLEMENTS, &[outcome]);
            }
            result.map(|_| ())
        }
    })
    .await
}

/// The prompt a response reports, to compare with a provider's own count
/// of it (`ProviderApiExact`): the input, which includes cache reads, plus
/// cache writes, which Anthropic and Bedrock report apart as `other` (the
/// only adapters that report `other`). The counting endpoints count the
/// whole prompt, cached or not.
fn counted_prompt(usage: &crate::domain::inference::Usage) -> Option<u64> {
    let input = usage.input_tokens()?.value()?;
    let writes = match usage.other_tokens() {
        Some(counter) => counter.value()?,
        None => 0,
    };
    input.checked_add(writes)
}

/// Records input-count drift for `reservation`, retrying transient failures
/// under one finalization id.
async fn reconcile_input_drift(
    store: &AccountingStore,
    reservation: &Reservation,
    usage: Option<Usage>,
    provider_request_id: Option<String>,
    provider_error: Option<ProviderError>,
    timings: RequestTimings,
) -> Result<(), AccountingStoreError> {
    if let Some(error) = &provider_error {
        log_provider_failure(reservation, error);
    }
    record_timings(store, reservation, timings);
    let store = store.clone();
    let (organization_id, reservation_id) = (reservation.scope.organization_id, reservation.id);
    let (finalization_id, trace_id) = (Uuid::new_v4(), reservation.trace_id);
    settlement_retry::finalize(reservation_id, move || {
        let (store, usage, provider_request_id, provider_error) = (
            store.clone(),
            usage.clone(),
            provider_request_id.clone(),
            provider_error.clone(),
        );
        async move {
            store
                .reconcile_input_drift(
                    organization_id,
                    reservation_id,
                    finalization_id,
                    trace_id,
                    usage,
                    provider_request_id.as_deref(),
                    provider_error.as_ref(),
                )
                .await
                .inspect(|()| {
                    crate::metrics::global()
                        .inc(&crate::metrics::SETTLEMENTS, &["input_count_drift"]);
                })
        }
    })
    .await
}

fn frame(mac: &mut HmacSha256, bytes: &[u8]) {
    mac.update(&(bytes.len() as u64).to_be_bytes());
    mac.update(bytes);
}
fn role_tag(role: ChatRole) -> u8 {
    match role {
        ChatRole::System => 0,
        ChatRole::Developer => 1,
        ChatRole::User => 2,
        ChatRole::Assistant => 3,
        ChatRole::Tool => 4,
    }
}
fn message_bytes(message: &ChatMessage) -> usize {
    message
        .content
        .len()
        .saturating_add(message.name.as_ref().map_or(0, String::len))
        .saturating_add(message.tool_call_id.as_ref().map_or(0, String::len))
        .saturating_add(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    call.id
                        .as_ref()
                        .map_or(0, String::len)
                        .saturating_add(call.name.as_ref().map_or(0, String::len))
                        .saturating_add(call.arguments.len())
                })
                .fold(0usize, usize::saturating_add),
        )
}

/// Frames a chat request's tools, tool choice and parallel calls into a
/// fingerprint, each only where present, so requests without tools keep
/// their fingerprints (D-032).
fn frame_tools(mac: &mut HmacSha256, request: &ChatRequest) {
    for tool in &request.tools {
        frame(mac, b"tool");
        frame(mac, tool.name.as_bytes());
        frame(mac, tool.description.as_deref().unwrap_or("-").as_bytes());
        frame(
            mac,
            serde_json::to_string(&tool.parameters)
                .unwrap_or_default()
                .as_bytes(),
        );
    }
    if let Some(choice) = &request.tool_choice {
        frame(mac, b"tool-choice");
        match choice {
            ToolChoice::Auto => frame(mac, b"auto"),
            ToolChoice::None => frame(mac, b"none"),
            ToolChoice::Required => frame(mac, b"required"),
            ToolChoice::Function(name) => {
                frame(mac, b"function");
                frame(mac, name.as_bytes());
            }
        }
    }
    if let Some(parallel) = request.parallel_tool_calls {
        frame(mac, b"parallel-tool-calls");
        frame(mac, &[u8::from(parallel)]);
    }
}

/// The bytes a chat request carries: its messages, with their tool calls,
/// and its tool definitions (D-032). Never framing or allowances.
fn request_bytes(request: &ChatRequest) -> Result<usize, AccountingServiceError> {
    request
        .messages
        .iter()
        .map(message_bytes)
        .chain(request.tools.iter().map(ToolDefinition::bytes))
        .chain(match &request.tool_choice {
            Some(ToolChoice::Function(name)) => Some(name.len()),
            _ => None,
        })
        .try_fold(0usize, usize::checked_add)
        .ok_or(AccountingServiceError::InputBoundExceeded)
}

struct AccountedStream {
    inner: ChatEventStream,
    store: AccountingStore,
    reservation: Reservation,
    expected_input_bound: Option<u64>,
    pending: Option<Result<ChatStreamEvent, ProviderError>>,
    finalization: Option<Pin<Box<dyn Future<Output = Result<(), AccountingStoreError>> + Send>>>,
    finalized: bool,
    /// The trace being captured, until the stream ends.
    capture: Option<Capture>,
    /// When the request was sent to the provider.
    started: Instant,
    first_event_ms: Option<u32>,
    /// The first choice's finish reason, as recorded.
    finish_reason: Option<String>,
}
impl AccountedStream {
    /// Records the captured trace with the response streamed so far.
    fn finish_capture(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.finish(self.reservation.trace_id.0, None);
        }
    }

    /// What the stream showed so far about how the request went.
    fn timings(&self) -> RequestTimings {
        RequestTimings {
            provider_latency_ms: Some(millis(self.started.elapsed())),
            first_event_ms: self.first_event_ms,
            finish_reason: self.finish_reason.clone(),
        }
    }

    /// Notes the first content event and the first choice's finish reason.
    fn observe(&mut self, event: &ChatStreamEvent) {
        match event {
            ChatStreamEvent::ChoiceDelta(_) | ChatStreamEvent::ToolCallDelta(_) => {
                self.first_event_ms
                    .get_or_insert_with(|| millis(self.started.elapsed()));
            }
            // The first choice's reason wins over any other's.
            ChatStreamEvent::ChoiceFinished(finished)
                if finished.choice_index() == 0 || self.finish_reason.is_none() =>
            {
                self.finish_reason = Some(finish_reason_code(finished.finish_reason().as_str()));
            }
            _ => {}
        }
    }

    fn begin(
        &mut self,
        event: Result<ChatStreamEvent, ProviderError>,
        usage: Option<crate::domain::inference::Usage>,
        provider_request_id: Option<String>,
    ) {
        if self.finalization.is_some() {
            return;
        }
        self.finish_capture();
        let timings = self.timings();
        let drift = self.expected_input_bound.is_some_and(|expected| {
            usage.as_ref().is_some_and(|usage| {
                counted_prompt(usage).is_some_and(|observed| observed != expected)
            })
        });
        let store = self.store.clone();
        let reservation = self.reservation.clone();
        let error = event.as_ref().err().cloned();
        let provider_request_id = provider_request_id.or_else(|| {
            error
                .as_ref()
                .and_then(|error| error.provider_request_id().map(str::to_owned))
        });
        self.pending = Some(event);
        self.finalization = Some(Box::pin(async move {
            if drift {
                reconcile_input_drift(
                    &store,
                    &reservation,
                    usage,
                    provider_request_id,
                    error,
                    timings,
                )
                .await
            } else {
                settle(
                    &store,
                    &reservation,
                    usage,
                    provider_request_id,
                    error,
                    timings,
                )
                .await
            }
        }));
    }
}

impl Drop for AccountedStream {
    fn drop(&mut self) {
        // A client that leaves early still has what it was sent recorded.
        self.finish_capture();
        if self.finalized {
            return;
        }
        let future = self.finalization.take().unwrap_or_else(|| {
            let store = self.store.clone();
            let reservation = self.reservation.clone();
            let timings = self.timings();
            Box::pin(async move { settle(&store, &reservation, None, None, None, timings).await })
        });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(future);
        }
    }
}

impl Stream for AccountedStream {
    type Item = Result<ChatStreamEvent, ProviderError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(future) = self.finalization.as_mut() {
            match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => {
                    self.finalized = true;
                    self.finalization = None;
                    return Poll::Ready(self.pending.take());
                }
                Poll::Ready(Err(error)) => {
                    // A transient failure is still being retried in the
                    // background, so dropping the stream must not also
                    // finalize it as unknown.
                    self.finalized = settlement_retry::is_transient(&error);
                    self.finalization = None;
                    self.pending = None;
                    return Poll::Ready(Some(Err(ProviderError::new(
                        ProviderErrorCategory::Internal,
                    ))));
                }
            }
        }
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => {
                if let Some(capture) = &mut self.capture {
                    capture.event(&event);
                }
                self.observe(&event);
                let usage = match &event {
                    ChatStreamEvent::ResponseCompleted(completion) => {
                        completion.usage().map(|s| s.usage())
                    }
                    _ => return Poll::Ready(Some(Ok(event))),
                };
                let provider_request_id = match &event {
                    ChatStreamEvent::ResponseCompleted(completion) => {
                        completion.provider_request_id().map(str::to_owned)
                    }
                    _ => None,
                };
                self.begin(Ok(event), usage, provider_request_id);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Some(Err(error))) => {
                let partial_usage = error.partial_usage();
                self.begin(Err(error), partial_usage, None);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(None) => {
                self.begin(
                    Err(ProviderError::new(ProviderErrorCategory::Protocol)),
                    None,
                    None,
                );
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counted_prompt_includes_cache_reads_and_writes() {
        use crate::domain::inference::{TokenCounter, Usage};
        // A provider's count of the whole prompt: 30 uncached, 1000 read
        // (inside the input), 200 written (`other`).
        let cached = Usage::complete(
            TokenCounter::exact(1030),
            TokenCounter::exact(12),
            Some(TokenCounter::exact(1000)),
            None,
            Some(TokenCounter::exact(200)),
        )
        .unwrap();
        assert_eq!(counted_prompt(&cached), Some(1230));
        // No cache writes reported: the input alone.
        let plain = Usage::complete(
            TokenCounter::exact(7),
            TokenCounter::exact(1),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(counted_prompt(&plain), Some(7));
    }

    /// Refuses with 403 as a documented refusal before inference.
    struct Refusing;

    #[async_trait::async_trait]
    impl InferenceProviderAdapter for Refusing {
        fn refused_before_inference(&self, error: &ProviderError) -> bool {
            error.http_status() == Some(403)
        }
        async fn chat(
            &self,
            _: InferenceContext,
            _: ChatRequest,
        ) -> Result<ChatResponse, ProviderError> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: InferenceContext,
            _: ChatRequest,
        ) -> Result<ChatEventStream, ProviderError> {
            unreachable!()
        }
        async fn embeddings(
            &self,
            _: InferenceContext,
            _: EmbeddingRequest,
        ) -> Result<EmbeddingResponse, ProviderError> {
            unreachable!()
        }
    }

    fn status(category: ProviderErrorCategory, status: u16) -> ProviderError {
        ProviderError::from_parts(category, None, None, Some(status), None, None, None).unwrap()
    }

    /// What a final failure is released for (charging nothing) is exactly
    /// what may fail over: proof that the provider did no work.
    #[test]
    fn only_a_refusal_before_work_is_released() {
        use crate::domain::inference::TokenCounter;
        let refused = |error: &ProviderError| failover_reason(&Refusing, error).is_some();
        assert!(refused(&status(ProviderErrorCategory::RateLimited, 429)));
        assert!(refused(&status(ProviderErrorCategory::Authorization, 403)));
        assert!(refused(&ProviderError::not_sent(
            ProviderErrorCategory::Unavailable
        )));
        assert!(!refused(&status(ProviderErrorCategory::Unavailable, 503)));
        assert!(!refused(&status(ProviderErrorCategory::Provider, 500)));
        assert!(!refused(&ProviderError::new(
            ProviderErrorCategory::Timeout
        )));
        assert!(!refused(&ProviderError::new(
            ProviderErrorCategory::Cancelled
        )));
        let partial = Usage::partial(Some(TokenCounter::exact(3)), None, None, None, None);
        let after_work = status(ProviderErrorCategory::RateLimited, 429)
            .with_partial_usage(partial)
            .unwrap();
        assert!(!refused(&after_work));
    }

    #[test]
    fn runtime_accounting_fingerprint_key_uses_stable_hkdf_vector() {
        let policy = AccountingPolicy::for_runtime(b"accounting-runtime-secret-v1");

        assert_eq!(
            policy.fingerprint_key,
            [
                0x0d, 0x1d, 0x94, 0x7d, 0xc0, 0xe0, 0x71, 0x08, 0x73, 0x6f, 0x08, 0xbe, 0x0f, 0x33,
                0xea, 0x63, 0x3d, 0x8b, 0xad, 0x4a, 0x33, 0xc0, 0x6f, 0xfd, 0xe2, 0x10, 0x13, 0x52,
                0x74, 0x36, 0x4d, 0x31,
            ]
        );
    }

    #[test]
    fn the_byte_bound_covers_tool_definitions_calls_and_their_allowance() {
        use crate::domain::inference::{ToolCall, ToolDefinition};
        let bound = ConfiguredTokenizerBound::new(NonZeroU32::new(1).unwrap());
        let plain = ChatRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "hello")],
            ..Default::default()
        };
        let plain_bound = bound.upper_bound(&plain).unwrap();
        assert_eq!(
            plain_bound,
            (5 + CHAT_MESSAGE_FRAMING_OVERHEAD_BYTES) as u64
        );
        let tool = ToolDefinition {
            name: "lookup".into(),
            description: Some("Looks it up".into()),
            parameters: serde_json::json!({"type": "object"}),
        };
        let with_tool = ChatRequest {
            tools: vec![tool.clone()],
            ..plain.clone()
        };
        assert_eq!(
            bound.upper_bound(&with_tool).unwrap(),
            plain_bound
                + (tool.bytes() + TOOL_FRAMING_OVERHEAD_BYTES + TOOL_USE_OVERHEAD_BYTES) as u64
        );
        let call = ToolCall {
            id: Some("call_1".into()),
            name: Some("lookup".into()),
            arguments: r#"{"q":"x"}"#.into(),
        };
        let mut with_call = with_tool.clone();
        with_call.messages.push(ChatMessage {
            tool_calls: vec![call],
            ..ChatMessage::text(ChatRole::Assistant, "")
        });
        assert_eq!(
            bound.upper_bound(&with_call).unwrap(),
            bound.upper_bound(&with_tool).unwrap()
                + (CHAT_MESSAGE_FRAMING_OVERHEAD_BYTES
                    + "call_1".len()
                    + "lookup".len()
                    + r#"{"q":"x"}"#.len()
                    + TOOL_FRAMING_OVERHEAD_BYTES) as u64
        );
    }

    #[test]
    fn fingerprints_change_with_tools_and_stay_put_without_them() {
        use crate::domain::inference::{ToolChoice, ToolDefinition};
        let policy = AccountingPolicy::for_tests();
        let plain = ChatRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "hello")],
            ..Default::default()
        };
        let with_tool = ChatRequest {
            tools: vec![ToolDefinition {
                name: "lookup".into(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
            }],
            ..plain.clone()
        };
        let forced = ChatRequest {
            tool_choice: Some(ToolChoice::Required),
            ..with_tool.clone()
        };
        let digest = |request: &ChatRequest| {
            let mut mac = HmacSha256::new_from_slice(&policy.fingerprint_key).unwrap();
            frame_tools(&mut mac, request);
            mac.finalize().into_bytes()
        };
        let empty = HmacSha256::new_from_slice(&policy.fingerprint_key)
            .unwrap()
            .finalize()
            .into_bytes();
        // Nothing is framed for a request without tools.
        assert_eq!(digest(&plain), empty);
        assert_ne!(digest(&with_tool), empty);
        assert_ne!(digest(&with_tool), digest(&forced));
    }
}
