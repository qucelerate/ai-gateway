//! Provider adapter contracts.
//!
//! This module deliberately contains no provider SDK or HTTP implementation. An
//! adapter owns authentication, network retries, and provider-specific decoding;
//! callers see only the normalized domain types. A method returning an error
//! before a stream is created is a setup error. Once a stream is returned, an
//! item containing `Err(ProviderError)` is the terminal in-stream error.
//!
//! Streams must emit at most one terminal completion or error and must not emit
//! further items afterwards. Dropping a stream is cancellation owned by the
//! caller: adapters should cancel their provider request and release resources
//! when the returned stream is dropped. This contract does not perform
//! settlement, billing, or finalization.
//!
//! One adapter method invocation represents one logical provider attempt. An
//! adapter may retry only before sending, or within a provider's explicitly
//! supported idempotency boundary; retryability is not retry authorization.
//! Adapters mark an error whose request never left the gateway (a connection
//! that could not be opened) with `ProviderError::not_sent`; the gateway's
//! own retries (`application::provider_retry`) rely on it.

use async_trait::async_trait;
use futures::Stream;
use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use crate::domain::discovery::DiscoveredModel;
use crate::domain::inference::{
    ChatRequest, ChatResponse, ChatStreamEvent, EmbeddingRequest, EmbeddingResponse,
    InferenceContext, ProviderError,
};

pub(crate) mod anthropic;
pub(crate) mod azure_ai_foundry;
pub(crate) mod azure_management;
pub(crate) mod azure_openai;
pub(crate) mod batch;
pub(crate) mod bedrock;
#[cfg(test)]
mod compatibility_matrix;
pub(crate) mod gemini;
mod model_list;
pub(crate) mod openai;
pub(crate) mod openai_compatible;
pub(crate) use tools::tool_arguments;
pub(crate) mod tools;

/// Trusted adapter registry of process-configured adapters. A catalog can
/// select an adapter instance but cannot provide credentials or an endpoint;
/// organization credentials stored through the admin API are applied by
/// `application::provider_resolver`, never through the catalog.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    adapters: std::collections::HashMap<(String, String), Arc<dyn InferenceProviderAdapter>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
    pub(crate) fn insert(
        &mut self,
        key: impl Into<String>,
        family: impl Into<String>,
        adapter: Arc<dyn InferenceProviderAdapter>,
    ) {
        self.adapters.insert((key.into(), family.into()), adapter);
    }
    fn adapter(&self, context: &InferenceContext) -> Option<Arc<dyn InferenceProviderAdapter>> {
        context
            .adapter_instance_key
            .as_ref()
            .and_then(|key| {
                context
                    .provider_family
                    .as_ref()
                    .and_then(|family| self.adapters.get(&(key.clone(), family.clone())))
            })
            .cloned()
    }
}

#[async_trait]
impl InferenceProviderAdapter for ProviderRegistry {
    fn supports(&self, context: &InferenceContext) -> bool {
        self.adapter(context)
            .is_some_and(|adapter| adapter.supports(context))
    }

    fn validate_chat_request(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        self.adapter(context).map_or_else(
            || {
                Err(ProviderError::new(
                    crate::domain::inference::ProviderErrorCategory::Unavailable,
                ))
            },
            |adapter| adapter.validate_chat_request(context, request),
        )
    }
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        match self.adapter(context) {
            Some(adapter) => adapter.count_chat_input_tokens(context, request).await,
            None => Err(ProviderError::new(
                crate::domain::inference::ProviderErrorCategory::Unavailable,
            )),
        }
    }
    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        match self.adapter(&context) {
            Some(adapter) => adapter.chat(context, request).await,
            None => Err(ProviderError::new(
                crate::domain::inference::ProviderErrorCategory::Unavailable,
            )),
        }
    }
    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        match self.adapter(&context) {
            Some(adapter) => adapter.chat_stream(context, request).await,
            None => Err(ProviderError::new(
                crate::domain::inference::ProviderErrorCategory::Unavailable,
            )),
        }
    }
    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        match self.adapter(&context) {
            Some(adapter) => adapter.embeddings(context, request).await,
            None => Err(ProviderError::new(
                crate::domain::inference::ProviderErrorCategory::Unavailable,
            )),
        }
    }
}

/// An Azure resource's key, or an Entra token, as a bearer `Authorization`
/// value.
pub(crate) async fn azure_bearer(
    auth: &crate::config::AzureAuth,
    timeout: std::time::Duration,
) -> Result<reqwest::header::HeaderValue, ProviderError> {
    let token = match auth {
        crate::config::AzureAuth::ApiKey(api_key) => zeroize::Zeroizing::new(api_key.clone()),
        crate::config::AzureAuth::Entra(source) => source
            .get_within(timeout)
            .await
            .map_err(credentials_error)?,
    };
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
        .map_err(|_| {
        ProviderError::new(crate::domain::inference::ProviderErrorCategory::Internal)
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// OpenAI-style usage reports reasoning tokens under
/// `completion_tokens_details.reasoning_tokens`, but providers disagree on
/// whether `completion_tokens` includes them: OpenAI's models count them in
/// it (and in the output cap), while xAI's Grok reports them on top (and does
/// not cap them). `total_tokens` tells them apart. The gateway's usage
/// counts output without reasoning, and charges reasoning on its own, so
/// this returns the output to record and the reasoning counter.
pub(crate) fn split_reasoning(
    usage: &serde_json::Value,
    prompt: u64,
    completion: u64,
) -> (u64, Option<crate::domain::inference::TokenCounter>) {
    let Some(reasoning) = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64)
    else {
        return (completion, None);
    };
    let total = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64);
    let on_top = reasoning > completion
        || (reasoning > 0
            && total
                == prompt
                    .checked_add(completion)
                    .and_then(|sum| sum.checked_add(reasoning)));
    let output = if on_top {
        completion
    } else {
        completion - reasoning
    };
    (
        output,
        Some(crate::domain::inference::TokenCounter::exact(reasoning)),
    )
}

/// The error of a request not sent because its cloud credentials could not
/// be had. Refused credentials are an authentication failure, which a retry
/// cannot cure.
pub(crate) fn credentials_error(
    error: crate::cloud_credentials::CredentialsError,
) -> ProviderError {
    use crate::{cloud_credentials::CredentialsError, domain::inference::ProviderErrorCategory};
    match error {
        CredentialsError::Refused(400 | 401 | 403) => {
            ProviderError::not_sent(ProviderErrorCategory::Authentication)
        }
        // The credential wait counts toward the adapter's request timeout.
        CredentialsError::TimedOut => ProviderError::not_sent(ProviderErrorCategory::Timeout),
        _ => ProviderError::not_sent(ProviderErrorCategory::Unavailable),
    }
}

/// The wait a response's `Retry-After` asks for, in either form HTTP allows:
/// delay-seconds, or an HTTP date (a date already past means no wait).
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    )
}

/// A dynamically dispatchable provider adapter.
///
/// `InferenceContext` contains correlation and provider-attempt identifiers and
/// a resolved deployment reference, never a bearer token or provider secret.
type RawChatEventStream =
    Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, ProviderError>> + Send + 'static>>;

/// Stream returned by adapters. It enforces start, terminal, ordered-choice,
/// and cumulative-usage invariants as items are observed.
pub struct ValidatingChatEventStream {
    inner: RawChatEventStream,
    started: bool,
    terminal: bool,
    observed_choices: HashSet<u32>,
    finished_choices: HashSet<u32>,
    last_usage: Option<crate::domain::inference::Usage>,
}

pub type ChatEventStream = ValidatingChatEventStream;
pub type InferenceEventStream = ChatEventStream;

impl ValidatingChatEventStream {
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<ChatStreamEvent, ProviderError>> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
            started: false,
            terminal: false,
            observed_choices: HashSet::new(),
            finished_choices: HashSet::new(),
            last_usage: None,
        }
    }
    fn protocol_error(&self) -> ProviderError {
        ProviderError::new(crate::domain::inference::ProviderErrorCategory::Protocol)
    }
}

impl Unpin for ValidatingChatEventStream {}

impl Stream for ValidatingChatEventStream {
    type Item = Result<ChatStreamEvent, ProviderError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.terminal {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.terminal = true;
                Poll::Ready(Some(Err(this.protocol_error())))
            }
            Poll::Ready(Some(Err(error))) => {
                this.terminal = true;
                if error.validate().is_err() {
                    Poll::Ready(Some(Err(this.protocol_error())))
                } else {
                    Poll::Ready(Some(Err(error)))
                }
            }
            Poll::Ready(Some(Ok(event))) => {
                if !this.started {
                    if !matches!(&event, ChatStreamEvent::ResponseStarted(_)) {
                        this.terminal = true;
                        return Poll::Ready(Some(Err(this.protocol_error())));
                    }
                    this.started = true;
                    return Poll::Ready(Some(Ok(event)));
                }
                let invalid = match &event {
                    ChatStreamEvent::ResponseStarted(_) => true,
                    ChatStreamEvent::ChoiceDelta(delta) => {
                        this.observed_choices.insert(delta.choice_index);
                        this.finished_choices.contains(&delta.choice_index)
                    }
                    ChatStreamEvent::ToolCallDelta(delta) => {
                        this.observed_choices.insert(delta.choice_index);
                        this.finished_choices.contains(&delta.choice_index)
                    }
                    ChatStreamEvent::ChoiceFinished(finished) => {
                        !this.finished_choices.insert(finished.choice_index())
                    }
                    ChatStreamEvent::Usage(snapshot) => {
                        if snapshot.finality() == crate::domain::inference::UsageFinality::Final {
                            true
                        } else if let Some(previous) = this.last_usage {
                            !cumulative_not_decreasing(previous, snapshot.usage())
                        } else {
                            false
                        }
                    }
                    ChatStreamEvent::SafetyAnnotation(_) => false,
                    ChatStreamEvent::ResponseCompleted(completion) => {
                        !this.observed_choices.is_subset(&this.finished_choices)
                            || if let Some(snapshot) = completion.usage() {
                                snapshot.finality()
                                    != crate::domain::inference::UsageFinality::Final
                                    || this.last_usage.is_some_and(|previous| {
                                        !cumulative_not_decreasing(previous, snapshot.usage())
                                    })
                            } else {
                                false
                            }
                    }
                };
                if invalid {
                    this.terminal = true;
                    Poll::Ready(Some(Err(this.protocol_error())))
                } else {
                    if let ChatStreamEvent::Usage(snapshot) = &event {
                        this.last_usage = Some(snapshot.usage());
                    }
                    if event.is_terminal() {
                        this.terminal = true;
                    }
                    Poll::Ready(Some(Ok(event)))
                }
            }
        }
    }
}

fn cumulative_not_decreasing(
    previous: crate::domain::inference::Usage,
    next: crate::domain::inference::Usage,
) -> bool {
    fn okay(
        before: Option<crate::domain::inference::TokenCounter>,
        after: Option<crate::domain::inference::TokenCounter>,
    ) -> bool {
        match (
            before.and_then(|counter| counter.value()),
            after.and_then(|counter| counter.value()),
        ) {
            (Some(before), Some(after)) => after >= before,
            (Some(_), None) => false,
            (None, Some(_)) | (None, None) => true,
        }
    }
    okay(previous.input_tokens(), next.input_tokens())
        && okay(previous.output_tokens(), next.output_tokens())
        && okay(previous.cached_input_tokens(), next.cached_input_tokens())
        && okay(previous.reasoning_tokens(), next.reasoning_tokens())
        && okay(previous.other_tokens(), next.other_tokens())
}

#[cfg(test)]
mod cumulative_usage_tests {
    use super::*;
    use crate::domain::inference::TokenCounter;

    fn usage(
        cached: Option<TokenCounter>,
        reasoning: Option<TokenCounter>,
        other: Option<TokenCounter>,
    ) -> crate::domain::inference::Usage {
        crate::domain::inference::Usage::partial(
            Some(TokenCounter::exact(1)),
            Some(TokenCounter::exact(1)),
            cached,
            reasoning,
            other,
        )
    }

    #[test]
    fn concrete_counters_cannot_disappear_or_decrease() {
        let concrete = usage(
            Some(TokenCounter::exact(2)),
            Some(TokenCounter::exact(3)),
            Some(TokenCounter::exact(4)),
        );
        let missing_cached = usage(
            None,
            Some(TokenCounter::exact(3)),
            Some(TokenCounter::exact(4)),
        );
        let unknown_reasoning = usage(
            Some(TokenCounter::exact(2)),
            Some(TokenCounter::unknown()),
            Some(TokenCounter::exact(4)),
        );
        let decreased_other = usage(
            Some(TokenCounter::exact(2)),
            Some(TokenCounter::exact(3)),
            Some(TokenCounter::exact(3)),
        );
        assert!(!cumulative_not_decreasing(concrete, missing_cached));
        assert!(!cumulative_not_decreasing(concrete, unknown_reasoning));
        assert!(!cumulative_not_decreasing(concrete, decreased_other));
    }

    #[test]
    fn unknown_or_absent_counters_may_become_concrete() {
        let unknown = usage(None, Some(TokenCounter::unknown()), None);
        let concrete = usage(
            Some(TokenCounter::exact(2)),
            Some(TokenCounter::exact(3)),
            Some(TokenCounter::exact(4)),
        );
        assert!(cumulative_not_decreasing(unknown, concrete));
    }
}

#[cfg(test)]
mod validating_stream_tests {
    use super::*;
    use crate::domain::inference::{
        ChoiceFinished, FinishReason, ProviderErrorCategory, ResponseCompletion, ResponseStarted,
        SafeMetadata, StreamChoiceDelta,
    };
    use futures::{StreamExt, stream};

    fn response_started() -> ChatStreamEvent {
        ChatStreamEvent::ResponseStarted(
            ResponseStarted::new(None, "test-model".to_owned(), SafeMetadata::new()).unwrap(),
        )
    }

    fn response_completed() -> ChatStreamEvent {
        ChatStreamEvent::ResponseCompleted(
            ResponseCompletion::new(None, "test-model".to_owned(), None).unwrap(),
        )
    }

    fn choice_delta(choice_index: u32) -> ChatStreamEvent {
        ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
            choice_index,
            content_delta: Some("content".to_owned()),
            refusal_delta: None,
            annotations: Vec::new(),
        })
    }

    fn choice_finished(choice_index: u32) -> ChatStreamEvent {
        ChatStreamEvent::ChoiceFinished(
            ChoiceFinished::new(choice_index, FinishReason::new("stop")).unwrap(),
        )
    }

    fn collect(events: Vec<ChatStreamEvent>) -> Vec<Result<ChatStreamEvent, ProviderError>> {
        futures::executor::block_on(
            ValidatingChatEventStream::new(stream::iter(
                events.into_iter().map(Ok::<_, ProviderError>),
            ))
            .collect(),
        )
    }

    #[test]
    fn response_completed_requires_finish_for_each_observed_choice() {
        let events = collect(vec![
            response_started(),
            choice_delta(0),
            response_completed(),
        ]);

        assert!(matches!(
            events.last(),
            Some(Err(error)) if error.category() == ProviderErrorCategory::Protocol
        ));
    }

    #[test]
    fn response_completed_is_valid_after_each_observed_choice_finishes() {
        let events = collect(vec![
            response_started(),
            choice_delta(0),
            choice_finished(0),
            response_completed(),
        ]);

        assert!(events.iter().all(Result::is_ok));
    }
}

#[async_trait]
pub trait InferenceProviderAdapter: Send + Sync {
    /// Pure admission checks run before an accounting reservation exists.
    fn supports(&self, _context: &InferenceContext) -> bool {
        true
    }
    fn validate_chat_request(
        &self,
        _context: &InferenceContext,
        _request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
    /// Count the exact input tokens for a chat request without generating a
    /// response. Implementations must not perform network I/O unless this
    /// operation is explicitly supported by the provider.
    async fn count_chat_input_tokens(
        &self,
        _context: &InferenceContext,
        _request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        Err(ProviderError::new(
            crate::domain::inference::ProviderErrorCategory::UnsupportedFeature,
        ))
    }
    /// Whether `error`, with a status that is not proof of anything by
    /// itself (a 5xx), is one this provider documents as refusing the
    /// request before any inference, so a route may fail it over to another
    /// deployment without risking a second charge (D-023). No adapter claims
    /// one yet; wrappers ask the adapter they wrap.
    fn refused_before_inference(&self, _error: &ProviderError) -> bool {
        false
    }
    /// Execute a complete chat request. Errors here are setup/request errors.
    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError>;

    /// Start a typed chat event stream. Errors before the stream starts are
    /// returned from this method; provider errors after that point are items.
    /// EOF before `ResponseCompleted` is an interrupted, non-successful
    /// response; adapters must not synthesize completion on EOF.
    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError>;

    /// Execute an embedding request. Embeddings are intentionally not modeled
    /// as chat choices.
    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError>;

    /// The models this provider account reports it can call, following every
    /// page. Nothing is generated or billed. Families without a listing API
    /// usable with inference credentials report `UnsupportedFeature`.
    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        Err(ProviderError::new(
            crate::domain::inference::ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    /// The models the provider's catalog lets this account deploy (D-018),
    /// following every page, or `None` when the family has no such catalog.
    /// Read only after a successful [`Self::list_models`]; a failure leaves
    /// the previous list.
    async fn list_deployable_models(
        &self,
    ) -> Result<Option<Vec<crate::domain::discovery::DeployableModel>>, ProviderError> {
        Ok(None)
    }

    /// The family's native batch API (D-021), for adapters that have one.
    fn batches(&self) -> Option<&dyn batch::BatchProvider> {
        None
    }

    /// The family's native batch API staged through the tenant's bucket
    /// (Bedrock, Gemini on Agent Platform), for adapters whose jobs read and
    /// write one. `None` when the adapter runs no staged jobs.
    fn staged_batches(
        &self,
        _staging: &batch::BatchStaging,
    ) -> Option<Box<dyn batch::BatchProvider + '_>> {
        None
    }

    /// The provider's model agreements (D-035), for adapters whose models
    /// can require one (Bedrock's AWS Marketplace offers).
    fn model_agreements(&self) -> Option<&dyn ModelAgreements> {
        None
    }

    /// Whether [`Self::probe_model`] calls the provider.
    fn probes_models(&self) -> bool {
        false
    }

    /// Whether [`Self::probe_model`] can call this listed model. A model
    /// listed only for what the tenant would need to do (Agent Platform's
    /// partner models) is never probed.
    fn probes_model(&self, _model: &str) -> bool {
        true
    }

    /// Whether probing this listed model costs the tenant and so is done
    /// only once a release deploys it, never for automatic offers alone
    /// (Agent Platform's partner models without a free call, and endpoints).
    fn probes_only_when_deployed(&self, _model: &str) -> bool {
        false
    }

    /// Whether a listed model is probed only once the listing's own check
    /// settled its entitlement. On Bedrock a first call to a Marketplace
    /// model whose agreement is not in place can subscribe the account by
    /// itself when the role may subscribe (D-035), and only the
    /// availability check tells such a model apart; a model the check did
    /// not answer for (it ended early, or the role may not make it) waits
    /// for a listing that does.
    fn probes_need_entitlement(&self) -> bool {
        false
    }

    /// Calls `model` as cheaply as the provider allows, to learn whether
    /// the account can call it (D-018): `Ok` when the call succeeded, else
    /// the provider's error. Families that do not probe report
    /// `UnsupportedFeature` without calling anything.
    async fn probe_model(&self, _model: &str) -> Result<(), ProviderError> {
        Err(ProviderError::new(
            crate::domain::inference::ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    /// Proves the credential can reach the account the way inference will,
    /// without generating or billing anything, and reports what it can call.
    /// By default the model listing; families whose listing does not prove
    /// it (or that cannot list) check another way.
    async fn check_access(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        self.list_models().await
    }
}

/// A provider's model agreements (D-035): reading a model's offers and its
/// agreement's state, and accepting an offer. Nothing here calls a model.
#[async_trait]
pub trait ModelAgreements: Send + Sync {
    /// Each listed model identifier (foundation models and their inference
    /// profiles) and the foundation model it calls, whose agreement it
    /// needs.
    async fn agreement_bases(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, ProviderError>;

    /// The offers for `model`, a foundation model.
    async fn agreement_offers(
        &self,
        model: &str,
    ) -> Result<Vec<crate::domain::model_agreement::AgreementOffer>, ProviderError>;

    /// What the provider says about `model`'s agreement now; `None` when its
    /// answer is not one this build knows.
    async fn agreement_availability(
        &self,
        model: &str,
    ) -> Result<Option<crate::domain::model_agreement::AgreementAvailability>, ProviderError>;

    /// Accepts `token`'s offer for `model`. The provider makes the agreement
    /// asynchronously: [`Self::agreement_availability`] reports it pending,
    /// then available.
    async fn create_agreement(
        &self,
        model: &str,
        token: &crate::domain::model_agreement::OfferToken,
    ) -> Result<(), ProviderError>;
}

/// Most pages one model listing may follow.
pub(crate) const MAX_MODEL_LIST_PAGES: usize = 100;

pub(crate) struct UnavailableProviderAdapter;

#[async_trait]
impl InferenceProviderAdapter for UnavailableProviderAdapter {
    fn supports(&self, _context: &InferenceContext) -> bool {
        false
    }

    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::new(
            crate::domain::inference::ProviderErrorCategory::Unavailable,
        ))
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        Err(ProviderError::new(
            crate::domain::inference::ProviderErrorCategory::Unavailable,
        ))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(
            crate::domain::inference::ProviderErrorCategory::Unavailable,
        ))
    }
}

#[cfg(test)]
mod retry_after_tests {
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

    fn parse(value: &str) -> Option<Duration> {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        super::retry_after(&headers)
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        assert_eq!(parse("7"), Some(Duration::from_secs(7)));
        let later = (chrono::Utc::now() + chrono::Duration::seconds(30))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let wait = parse(&later).unwrap();
        assert!(
            wait > Duration::from_secs(25) && wait <= Duration::from_secs(30),
            "{wait:?}"
        );
        assert_eq!(parse("Wed, 21 Oct 2015 07:28:00 GMT"), Some(Duration::ZERO));
        assert_eq!(parse("soon"), None);
        assert_eq!(super::retry_after(&HeaderMap::new()), None);
    }
}

#[cfg(test)]
mod split_reasoning_tests {
    use serde_json::json;

    fn split(usage: serde_json::Value) -> (u64, Option<u64>) {
        let prompt = usage["prompt_tokens"].as_u64().unwrap();
        let completion = usage["completion_tokens"].as_u64().unwrap();
        let (output, reasoning) = super::split_reasoning(&usage, prompt, completion);
        (output, reasoning.and_then(|counter| counter.value()))
    }

    #[test]
    fn openai_counts_reasoning_inside_completion_tokens() {
        // GPT-6 on Foundry: total = prompt + completion.
        let usage = json!({"prompt_tokens": 16, "completion_tokens": 17, "total_tokens": 33,
            "completion_tokens_details": {"reasoning_tokens": 7}});
        assert_eq!(split(usage), (10, Some(7)));
        // All of a small cap spent reasoning.
        let usage = json!({"prompt_tokens": 16, "completion_tokens": 5, "total_tokens": 21,
            "completion_tokens_details": {"reasoning_tokens": 5}});
        assert_eq!(split(usage), (0, Some(5)));
        // No total: OpenAI's convention.
        let usage = json!({"prompt_tokens": 16, "completion_tokens": 17,
            "completion_tokens_details": {"reasoning_tokens": 7}});
        assert_eq!(split(usage), (10, Some(7)));
    }

    #[test]
    fn grok_reports_reasoning_on_top_of_completion_tokens() {
        // Grok 4.7 on Foundry: total = prompt + completion + reasoning.
        let usage = json!({"prompt_tokens": 14, "completion_tokens": 1, "total_tokens": 43,
            "completion_tokens_details": {"reasoning_tokens": 28}});
        assert_eq!(split(usage), (1, Some(28)));
        // Less reasoning than completion: only the total tells the
        // conventions apart.
        let on_top = json!({"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 35,
            "completion_tokens_details": {"reasoning_tokens": 5}});
        assert_eq!(split(on_top), (20, Some(5)));
        let inside = json!({"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30,
            "completion_tokens_details": {"reasoning_tokens": 5}});
        assert_eq!(split(inside), (15, Some(5)));
        // More reasoning than completion cannot be inside it.
        let usage = json!({"prompt_tokens": 14, "completion_tokens": 1,
            "completion_tokens_details": {"reasoning_tokens": 28}});
        assert_eq!(split(usage), (1, Some(28)));
    }

    #[test]
    fn without_reasoning_details_the_completion_is_the_output() {
        let usage = json!({"prompt_tokens": 16, "completion_tokens": 4, "total_tokens": 20});
        assert_eq!(split(usage), (4, None));
        let usage = json!({"prompt_tokens": 16, "completion_tokens": 4, "total_tokens": 20,
            "completion_tokens_details": {"reasoning_tokens": 0}});
        assert_eq!(split(usage), (4, Some(0)));
    }
}
