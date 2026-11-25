//! Gateway-side provider retries.
//!
//! A provider call is tried again only when the failed attempt cannot have
//! been processed:
//!
//! - the connection was never opened, so nothing reached the provider; or
//! - the provider refused the request with 429 (rate limited), which every
//!   supported provider documents as rejected before processing. A 429 that
//!   reports exhausted quota (`insufficient_quota`,
//!   `serviceQuotaExceededException`) is not transient and is not retried.
//!
//! Timeouts, dropped connections and 5xx responses are never retried: the
//! provider may have run the request, and a second run would be billed
//! twice. Nor is an error carrying partial usage, whatever its status: an
//! adapter that splits a request into several provider calls reports so
//! the calls that already ran and were billed. A stream is retried only before it is returned, never once events
//! flow. Each retry reuses the request's provider idempotency key.
//!
//! Attempts are bounded (three in all) and so is the waiting between them:
//! backoff with jitter, or the provider's `Retry-After` when it gives one,
//! and at most [`RetrySettings::max_wait`] in total. A `Retry-After` longer
//! than the remaining wait ends the retries, so the caller sees the 429 and
//! its `Retry-After` at once rather than after a long stall.
//!
//! The decorator sits inside the circuit guard, so a deployment's circuit
//! sees one outcome per request: the last attempt's.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;

use crate::{
    domain::inference::{
        ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, InferenceContext,
        ProviderError, ProviderErrorCategory,
    },
    metrics::{PROVIDER_RETRIES, global as metrics, provider_family_label},
    provider::{ChatEventStream, InferenceProviderAdapter},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct RetrySettings {
    /// Attempts in all, the first included.
    pub max_attempts: u32,
    /// Backoff before the first retry; doubled for each later one.
    pub base_delay: Duration,
    /// Longest the gateway waits across all retries of one call.
    pub max_wait: Duration,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_wait: Duration::from_secs(5),
        }
    }
}

/// Provider codes of a 429 that means a quota is used up, not a momentary
/// limit: retrying cannot help.
fn exhausted(code: &str) -> bool {
    ["insufficient_quota", "serviceQuotaExceededException"]
        .iter()
        .any(|quota| code.eq_ignore_ascii_case(quota))
}

/// Why a failed attempt may be tried again; `None` when it may not. A
/// request not sent because its credentials were refused fails again.
fn retry_reason(error: &ProviderError) -> Option<&'static str> {
    if error.partial_usage().is_some() {
        None
    } else if error.was_not_sent() && error.category() != ProviderErrorCategory::Authentication {
        Some("not_sent")
    } else if error.http_status() == Some(429) && !error.provider_code().is_some_and(exhausted) {
        Some("rate_limited")
    } else {
        None
    }
}

impl RetrySettings {
    /// How long to wait before retry number `retry` (1-based), given what
    /// was already waited; `None` to stop.
    fn delay(&self, retry: u32, error: &ProviderError, waited: Duration) -> Option<Duration> {
        if retry >= self.max_attempts {
            return None;
        }
        let delay = match error.retry_after() {
            Some(after) => after,
            None => {
                let backoff = self.base_delay.saturating_mul(1 << (retry - 1).min(16));
                // Full jitter in [backoff/2, backoff], so callers retrying
                // together spread out.
                let half = backoff / 2;
                half + half.mul_f64(rand::random::<f64>())
            }
        };
        (waited + delay <= self.max_wait).then_some(delay)
    }
}

/// Adapter decorator that retries attempts that provably did nothing.
pub(crate) struct RetryingAdapter {
    inner: Arc<dyn InferenceProviderAdapter>,
    settings: RetrySettings,
}

impl RetryingAdapter {
    pub(crate) fn new(inner: Arc<dyn InferenceProviderAdapter>) -> Self {
        Self::with_settings(inner, RetrySettings::default())
    }

    pub(crate) fn with_settings(
        inner: Arc<dyn InferenceProviderAdapter>,
        settings: RetrySettings,
    ) -> Self {
        Self { inner, settings }
    }

    async fn run<T, F, Fut>(
        &self,
        context: &InferenceContext,
        operation: &'static str,
        mut attempt: F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut + Send,
        Fut: Future<Output = Result<T, ProviderError>> + Send,
    {
        let mut waited = Duration::ZERO;
        let mut retry = 1;
        loop {
            let error = match attempt().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let Some(reason) = retry_reason(&error) else {
                return Err(error);
            };
            let Some(delay) = self.settings.delay(retry, &error, waited) else {
                return Err(error);
            };
            metrics().inc(
                &PROVIDER_RETRIES,
                &[
                    provider_family_label(context.provider_family.as_deref()),
                    operation,
                    reason,
                ],
            );
            tokio::time::sleep(delay).await;
            waited += delay;
            retry += 1;
        }
    }
}

#[async_trait]
impl InferenceProviderAdapter for RetryingAdapter {
    fn refused_before_inference(&self, error: &ProviderError) -> bool {
        self.inner.refused_before_inference(error)
    }

    fn supports(&self, context: &InferenceContext) -> bool {
        self.inner.supports(context)
    }

    fn validate_chat_request(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        self.inner.validate_chat_request(context, request)
    }

    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        self.run(context, "count_tokens", || {
            self.inner.count_chat_input_tokens(context, request)
        })
        .await
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.run(&context, "chat", || {
            self.inner.chat(context.clone(), request.clone())
        })
        .await
    }

    /// Retried only while setting the stream up; once returned, it is the
    /// caller's.
    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        self.run(&context, "chat_stream", || {
            self.inner.chat_stream(context.clone(), request.clone())
        })
        .await
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        self.run(&context, "embeddings", || {
            self.inner.embeddings(context.clone(), request.clone())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    };

    use super::*;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, ProviderAttemptIdempotencyKey,
        ProviderErrorCategory,
    };

    /// Fails with each scripted error in turn, then fails with `Internal`
    /// forever; counts calls and records each call's idempotency key.
    struct Scripted {
        errors: Mutex<Vec<ProviderError>>,
        calls: AtomicU32,
        keys: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(errors: Vec<ProviderError>) -> Arc<Self> {
            Arc::new(Self {
                errors: Mutex::new(errors.into_iter().rev().collect()),
                calls: AtomicU32::new(0),
                keys: Mutex::new(Vec::new()),
            })
        }

        fn next(&self, context: &InferenceContext) -> ProviderError {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.keys
                .lock()
                .unwrap()
                .push(context.provider_attempt_idempotency_key.as_str().to_owned());
            self.errors
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| ProviderError::new(ProviderErrorCategory::Internal))
        }
    }

    #[async_trait]
    impl InferenceProviderAdapter for Scripted {
        async fn chat(
            &self,
            context: InferenceContext,
            _: ChatRequest,
        ) -> Result<ChatResponse, ProviderError> {
            Err(self.next(&context))
        }

        async fn chat_stream(
            &self,
            context: InferenceContext,
            _: ChatRequest,
        ) -> Result<ChatEventStream, ProviderError> {
            Err(self.next(&context))
        }

        async fn embeddings(
            &self,
            context: InferenceContext,
            _: EmbeddingRequest,
        ) -> Result<EmbeddingResponse, ProviderError> {
            Err(self.next(&context))
        }
    }

    fn context() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt-1").unwrap(),
            DeploymentModelReference::new("chat", "model").unwrap(),
        )
        .with_provider_family("gemini")
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![],
            temperature: None,
            max_tokens: None,
            ..Default::default()
        }
    }

    fn status(code: u16, retry_after: Option<u64>, provider_code: Option<&str>) -> ProviderError {
        ProviderError::from_parts(
            if code == 429 {
                ProviderErrorCategory::RateLimited
            } else {
                ProviderErrorCategory::Unavailable
            },
            None,
            None,
            Some(code),
            provider_code.map(ToOwned::to_owned),
            retry_after.map(Duration::from_secs),
            None,
        )
        .unwrap()
    }

    fn not_sent() -> ProviderError {
        ProviderError::not_sent(ProviderErrorCategory::Unavailable)
    }

    async fn calls(errors: Vec<ProviderError>) -> (u32, ProviderError) {
        let inner = Scripted::new(errors);
        let adapter = RetryingAdapter::new(inner.clone());
        let error = adapter.chat(context(), request()).await.unwrap_err();
        (inner.calls.load(Ordering::SeqCst), error)
    }

    #[tokio::test(start_paused = true)]
    async fn only_attempts_that_did_nothing_are_tried_again() {
        // Unsent and rate-limited attempts are retried, up to three in all.
        let (count, error) = calls(vec![not_sent(), status(429, None, None)]).await;
        assert_eq!(count, 3);
        assert_eq!(error.category(), ProviderErrorCategory::Internal);
        let (count, error) = calls(vec![not_sent(), not_sent(), not_sent(), not_sent()]).await;
        assert_eq!(count, 3);
        assert!(error.was_not_sent());
        // Credentials refused before sending fail again: not retried.
        let (count, error) = calls(vec![ProviderError::not_sent(
            ProviderErrorCategory::Authentication,
        )])
        .await;
        assert_eq!(count, 1);
        assert!(error.was_not_sent());

        // Anything the provider may have run is returned at once.
        for error in [
            ProviderError::new(ProviderErrorCategory::Timeout),
            ProviderError::new(ProviderErrorCategory::Provider),
            status(500, None, None),
            status(503, Some(1), None),
            status(429, None, Some("insufficient_quota")),
            status(429, None, Some("serviceQuotaExceededException")),
            ProviderError::new(ProviderErrorCategory::InvalidRequest),
            // A 429 after earlier calls of the same request were billed.
            status(429, None, None)
                .with_partial_usage(crate::domain::inference::Usage::partial(
                    Some(crate::domain::inference::TokenCounter::exact(5)),
                    Some(crate::domain::inference::TokenCounter::exact(0)),
                    None,
                    None,
                    None,
                ))
                .unwrap(),
        ] {
            let (count, returned) = calls(vec![error.clone()]).await;
            assert_eq!((count, returned), (1, error));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_honored_within_the_wait_budget() {
        let started = tokio::time::Instant::now();
        let (count, _) = calls(vec![status(429, Some(2), None), status(429, Some(2), None)]).await;
        assert_eq!(count, 3);
        assert_eq!(started.elapsed(), Duration::from_secs(4));

        // A wait longer than what remains returns the 429 at once.
        let started = tokio::time::Instant::now();
        let (count, error) =
            calls(vec![status(429, Some(3), None), status(429, Some(3), None)]).await;
        assert_eq!(count, 2);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(started.elapsed(), Duration::from_secs(3));
        let (count, _) = calls(vec![status(429, Some(60), None)]).await;
        assert_eq!(count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_jittered_and_retries_reuse_the_idempotency_key() {
        let inner = Scripted::new(vec![not_sent(), not_sent()]);
        let adapter = RetryingAdapter::new(inner.clone());
        let started = tokio::time::Instant::now();
        assert!(adapter.chat_stream(context(), request()).await.is_err());
        let waited = started.elapsed();
        // 125-250ms, then 250-500ms.
        assert!(
            waited >= Duration::from_millis(375) && waited <= Duration::from_millis(750),
            "{waited:?}"
        );
        assert_eq!(*inner.keys.lock().unwrap(), ["attempt-1"; 3]);

        let before = metrics()
            .value(&PROVIDER_RETRIES, &["gemini", "embeddings", "not_sent"])
            .unwrap_or(0);
        let inner = Scripted::new(vec![not_sent()]);
        let adapter = RetryingAdapter::new(inner.clone());
        let request = EmbeddingRequest {
            input: vec!["x".into()],
        };
        assert!(adapter.embeddings(context(), request).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(
            metrics()
                .value(&PROVIDER_RETRIES, &["gemini", "embeddings", "not_sent"])
                .unwrap_or(0)
                > before
        );
    }
}
