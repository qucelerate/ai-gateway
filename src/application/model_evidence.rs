//! Records what inference traffic shows about whether an account can call a
//! model (D-018): a call that succeeds verifies the model, and one the
//! provider refuses for the model (403, 404) refuses it.
//!
//! Recording never delays or fails a request. Observations go through a
//! bounded queue to one writer task, and a process writes the same verdict
//! for the same model at most once per [`REFRESH`], so steady traffic costs
//! no database writes; a changed verdict is written at once. A write that
//! fails, or an observation the full queue drops, is forgotten, so the next
//! observation tries again.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::Utc;
use futures::Stream;
use tokio::sync::mpsc;

use crate::{
    domain::{
        identity::OrganizationId,
        inference::{
            ChatRequest, ChatResponse, ChatStreamEvent, EmbeddingRequest, EmbeddingResponse,
            InferenceContext, ProviderError,
        },
        model_evidence::{EvidenceKey, Observation, Verdict},
    },
    persistence::model_evidence::ModelEvidenceStore,
    provider::{ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream},
};

/// How often an unchanged verdict is written again, so `observed_at` stays
/// recent without a write per request.
pub(crate) const REFRESH: Duration = Duration::from_secs(600);
/// Observations waiting for the writer; more are dropped (and counted).
const QUEUE: usize = 1024;
/// Models whose last written verdict a process remembers; past this it
/// forgets them all, which costs at most one extra write each.
const MAX_REMEMBERED: usize = 10_000;

/// The verdict last queued per model, and when.
type Written = Arc<Mutex<HashMap<EvidenceKey, (Verdict, Instant)>>>;

/// Forgets `observation`'s verdict, unless a newer verdict has replaced it,
/// so the next observation is written.
fn forget(written: &Written, observation: &Observation) {
    let mut written = written
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if written
        .get(&observation.key)
        .is_some_and(|(verdict, _)| *verdict == observation.verdict)
    {
        written.remove(&observation.key);
    }
}

#[derive(Clone)]
pub(crate) struct EvidenceRecorder {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    queue: mpsc::Sender<Observation>,
    written: Written,
}

impl EvidenceRecorder {
    /// Records nothing.
    pub(crate) fn disabled() -> Self {
        Self { inner: None }
    }

    /// Writes observations to `store` from a task on the current runtime.
    pub(crate) fn new(store: ModelEvidenceStore) -> Self {
        let (queue, mut observations) = mpsc::channel::<Observation>(QUEUE);
        let written: Written = Arc::default();
        let remembered = written.clone();
        tokio::spawn(async move {
            while let Some(observation) = observations.recv().await {
                let result = store.record(&observation).await;
                let outcome = if result.is_ok() { "written" } else { "failed" };
                crate::metrics::global().inc(&crate::metrics::MODEL_EVIDENCE, &[outcome]);
                if let Err(error) = result {
                    // The next observation tries again rather than waiting
                    // out the refresh.
                    forget(&remembered, &observation);
                    tracing::warn!(
                        organization_id = %observation.key.organization_id.0,
                        adapter_instance_key = %observation.key.adapter_instance_key,
                        model = %observation.key.model,
                        error = %error,
                        "recording model evidence failed"
                    );
                }
            }
        });
        Self {
            inner: Some(Arc::new(Inner { queue, written })),
        }
    }

    pub(crate) fn observe(&self, observation: Observation) {
        self.observe_at(observation, Instant::now());
    }

    fn observe_at(&self, observation: Observation, now: Instant) {
        let Some(inner) = &self.inner else {
            return;
        };
        {
            let mut written = inner
                .written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if written.get(&observation.key).is_some_and(|(verdict, at)| {
                *verdict == observation.verdict && now.duration_since(*at) < REFRESH
            }) {
                return;
            }
            if written.len() >= MAX_REMEMBERED && !written.contains_key(&observation.key) {
                written.clear();
            }
            written.insert(observation.key.clone(), (observation.verdict, now));
        }
        if let Err(error) = inner.queue.try_send(observation) {
            let observation = match error {
                mpsc::error::TrySendError::Full(observation)
                | mpsc::error::TrySendError::Closed(observation) => observation,
            };
            forget(&inner.written, &observation);
            crate::metrics::global().inc(&crate::metrics::MODEL_EVIDENCE, &["dropped"]);
        }
    }
}

/// Adapter decorator for one request: reports what the call showed about the
/// model to the recorder. It wraps the adapter outside its own rerouting and
/// the gateway's retries, so only the final outcome counts.
pub(crate) struct EvidenceReportingAdapter {
    inner: Arc<dyn InferenceProviderAdapter>,
    recorder: EvidenceRecorder,
    organization_id: OrganizationId,
}

impl EvidenceReportingAdapter {
    pub(crate) fn new(
        inner: Arc<dyn InferenceProviderAdapter>,
        recorder: EvidenceRecorder,
        organization_id: OrganizationId,
    ) -> Self {
        Self {
            inner,
            recorder,
            organization_id,
        }
    }
}

/// Reports one call's result for the model in `context`. Contexts without an
/// adapter instance and family (tests, the process adapter) say nothing.
fn report(
    recorder: &EvidenceRecorder,
    organization_id: OrganizationId,
    context: &InferenceContext,
    result: Result<(), &ProviderError>,
) {
    let (Some(adapter_instance_key), Some(family)) = (
        context.adapter_instance_key.clone(),
        context.provider_family.clone(),
    ) else {
        return;
    };
    let key = EvidenceKey {
        organization_id,
        adapter_instance_key,
        family,
        model: context.deployment.model().to_owned(),
    };
    if let Some(observation) =
        Observation::of_call(key, context.deployment.deployment(), result, Utc::now())
    {
        recorder.observe(observation);
    }
}

#[async_trait]
impl InferenceProviderAdapter for EvidenceReportingAdapter {
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

    /// Counting precedes the call, so only a refusal is reported: counting
    /// tokens does not prove the model can be called.
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        let result = self.inner.count_chat_input_tokens(context, request).await;
        if let Err(error) = &result {
            report(&self.recorder, self.organization_id, context, Err(error));
        }
        result
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let reported = context.clone();
        let result = self.inner.chat(context, request).await;
        report(
            &self.recorder,
            self.organization_id,
            &reported,
            result.as_ref().map(|_| ()),
        );
        result
    }

    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let reported = context.clone();
        match self.inner.chat_stream(context, request).await {
            Ok(inner) => Ok(ValidatingChatEventStream::new(ReportingStream {
                inner,
                pending: Some((self.recorder.clone(), self.organization_id, reported)),
            })),
            Err(error) => {
                report(&self.recorder, self.organization_id, &reported, Err(&error));
                Err(error)
            }
        }
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        let reported = context.clone();
        let result = self.inner.embeddings(context, request).await;
        report(
            &self.recorder,
            self.organization_id,
            &reported,
            result.as_ref().map(|_| ()),
        );
        result
    }
}

/// A stream's first item tells: an event means the provider is serving the
/// model, an error may refuse it. Later items say nothing more.
struct ReportingStream {
    inner: ChatEventStream,
    pending: Option<(EvidenceRecorder, OrganizationId, InferenceContext)>,
}

impl Stream for ReportingStream {
    type Item = Result<ChatStreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let item = Pin::new(&mut this.inner).poll_next(cx);
        if let Poll::Ready(Some(result)) = &item {
            if let Some((recorder, organization_id, context)) = this.pending.take() {
                report(
                    &recorder,
                    organization_id,
                    &context,
                    result.as_ref().map(|_| ()),
                );
            }
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        inference::ProviderErrorCategory,
        model_evidence::{EvidenceReason, EvidenceSource},
    };
    use uuid::Uuid;

    fn observation(verdict: Verdict) -> Observation {
        Observation {
            key: EvidenceKey {
                organization_id: OrganizationId(Uuid::nil()),
                adapter_instance_key: "bedrock-1".into(),
                family: "bedrock".into(),
                model: "m".into(),
            },
            verdict,
            source: EvidenceSource::Traffic,
            deployment: Some("d".into()),
            provider_status: None,
            provider_code: None,
            observed_at: Utc::now(),
        }
    }

    fn recorder_with(capacity: usize) -> (EvidenceRecorder, mpsc::Receiver<Observation>) {
        let (queue, observations) = mpsc::channel(capacity);
        (
            EvidenceRecorder {
                inner: Some(Arc::new(Inner {
                    queue,
                    written: Arc::default(),
                })),
            },
            observations,
        )
    }

    fn recorder() -> (EvidenceRecorder, mpsc::Receiver<Observation>) {
        recorder_with(QUEUE)
    }

    fn written(recorder: &EvidenceRecorder) -> &Written {
        &recorder.inner.as_ref().unwrap().written
    }

    #[test]
    fn an_unchanged_verdict_is_written_once_per_refresh() {
        let (recorder, mut queue) = recorder();
        let start = Instant::now();
        recorder.observe_at(observation(Verdict::Verified), start);
        recorder.observe_at(
            observation(Verdict::Verified),
            start + Duration::from_secs(1),
        );
        assert!(queue.try_recv().is_ok());
        assert!(queue.try_recv().is_err());
        recorder.observe_at(observation(Verdict::Verified), start + REFRESH);
        assert!(queue.try_recv().is_ok());
    }

    #[test]
    fn a_changed_verdict_is_written_at_once() {
        let (recorder, mut queue) = recorder();
        let start = Instant::now();
        recorder.observe_at(observation(Verdict::Verified), start);
        recorder.observe_at(
            observation(Verdict::Refused(EvidenceReason::AccessDenied)),
            start + Duration::from_secs(1),
        );
        assert_eq!(queue.try_recv().unwrap().verdict, Verdict::Verified);
        assert_eq!(
            queue.try_recv().unwrap().verdict,
            Verdict::Refused(EvidenceReason::AccessDenied)
        );
    }

    #[test]
    fn a_disabled_recorder_records_nothing() {
        EvidenceRecorder::disabled().observe(observation(Verdict::Verified));
    }

    #[test]
    fn a_full_queue_forgets_the_verdict_so_it_is_tried_again() {
        let (recorder, mut observations) = recorder_with(1);
        let mut other = observation(Verdict::Verified);
        other.key.model = "other".into();
        let now = Instant::now();
        recorder.observe_at(other, now);
        recorder.observe_at(observation(Verdict::Verified), now);
        assert_eq!(observations.try_recv().unwrap().key.model, "other");
        recorder.observe_at(observation(Verdict::Verified), now);
        assert_eq!(observations.try_recv().unwrap().key.model, "m");
    }

    #[test]
    fn a_failed_write_is_tried_again_by_the_next_observation() {
        let (recorder, mut queue) = recorder();
        let now = Instant::now();
        recorder.observe_at(observation(Verdict::Verified), now);
        let failed = queue.try_recv().unwrap();
        forget(written(&recorder), &failed);
        recorder.observe_at(observation(Verdict::Verified), now);
        assert!(queue.try_recv().is_ok());
    }

    #[test]
    fn a_failed_write_does_not_forget_a_newer_verdict() {
        let (recorder, mut queue) = recorder();
        let now = Instant::now();
        recorder.observe_at(observation(Verdict::Verified), now);
        let failed = queue.try_recv().unwrap();
        let refused = Verdict::Refused(EvidenceReason::NotFound);
        recorder.observe_at(observation(refused), now);
        assert!(queue.try_recv().is_ok());
        forget(written(&recorder), &failed);
        recorder.observe_at(observation(refused), now);
        assert!(queue.try_recv().is_err(), "the refusal is still remembered");
    }

    #[test]
    fn errors_that_say_nothing_are_not_reported() {
        let (recorder, mut queue) = recorder();
        let context = InferenceContext::new(
            crate::domain::inference::CorrelationId::new(),
            crate::domain::inference::ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            crate::domain::inference::DeploymentModelReference::new("dep", "m").unwrap(),
        )
        .with_adapter_instance_key("bedrock-1")
        .with_provider_family("bedrock");
        let throttled = ProviderError::new(ProviderErrorCategory::RateLimited);
        report(
            &recorder,
            OrganizationId(Uuid::nil()),
            &context,
            Err(&throttled),
        );
        assert!(queue.try_recv().is_err());
        let denied = ProviderError::new(ProviderErrorCategory::Authorization);
        report(
            &recorder,
            OrganizationId(Uuid::nil()),
            &context,
            Err(&denied),
        );
        let observed = queue.try_recv().unwrap();
        assert_eq!(observed.key.model, "m");
        assert_eq!(observed.key.family, "bedrock");
        assert_eq!(observed.deployment.as_deref(), Some("dep"));
    }
}
