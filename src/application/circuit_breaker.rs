//! Circuit breaking per organization, adapter instance, and deployment.
//!
//! After `failure_threshold` consecutive provider failures a circuit opens and
//! requests fail fast, before anything is reserved, until the open period
//! ends. Then one probe request is let through: success closes the circuit,
//! failure reopens it for twice as long, up to `max_open_for`. Circuits are
//! keyed by organization so one tenant's credential or quota never trips
//! another's, and state is per process: each gateway instance learns a
//! deployment's health from its own traffic.
//!
//! Only failures that say the provider is unhealthy count (unavailable,
//! timeout, throttled, provider or protocol errors). A rejected request, a
//! bad credential, or a cancelled call proves the provider answered, or
//! proves nothing, and never trips a circuit.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::Stream;

use crate::{
    domain::{
        identity::OrganizationId,
        inference::{
            ChatRequest, ChatResponse, ChatStreamEvent, EmbeddingRequest, EmbeddingResponse,
            InferenceContext, ProviderError, ProviderErrorCategory,
        },
    },
    metrics::{CIRCUIT_TRANSITIONS, PROVIDER_CALLS, PROVIDER_DURATION, global as metrics},
    provider::{ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct CircuitBreakerSettings {
    pub failure_threshold: u32,
    pub open_for: Duration,
    pub max_open_for: Duration,
    /// A probe holds the circuit's probe slot for at most this long; after
    /// that the next request probes instead.
    pub probe_lease: Duration,
    /// A circuit no request touched for this long is forgotten.
    pub forget_after: Duration,
}

impl Default for CircuitBreakerSettings {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_for: Duration::from_secs(30),
            max_open_for: Duration::from_secs(300),
            probe_lease: Duration::from_secs(120),
            forget_after: Duration::from_secs(600),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CircuitKey {
    organization_id: OrganizationId,
    adapter_instance_key: String,
    deployment: String,
}

impl CircuitKey {
    fn new(organization_id: OrganizationId, context: &InferenceContext) -> Self {
        Self::of(
            organization_id,
            context.adapter_instance_key.as_deref(),
            context.deployment.deployment(),
        )
    }

    fn of(
        organization_id: OrganizationId,
        adapter_instance_key: Option<&str>,
        deployment: &str,
    ) -> Self {
        Self {
            organization_id,
            adapter_instance_key: adapter_instance_key.unwrap_or("-").to_owned(),
            deployment: deployment.to_owned(),
        }
    }
}

/// Why a route skips a deployment it would otherwise pick (D-023).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Unhealthy {
    /// Its circuit is open, or its one probe is in flight.
    CircuitOpen,
    /// The provider refused a request with 429 and asked for a wait that has
    /// not passed.
    RateLimited,
}

/// Closed circuits with no recent failure have no entry, so the map only
/// holds deployments that are failing or recovering. `open_for` is the last
/// wait, which a failed probe doubles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Circuit {
    Closed {
        failures: u32,
    },
    Open {
        until: Instant,
        open_for: Duration,
    },
    HalfOpen {
        probe: Option<Probe>,
        open_for: Duration,
    },
}

/// The request probing a half-open circuit, and when its lease ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Probe {
    id: u64,
    until: Instant,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    circuit: Circuit,
    /// Outcomes of requests admitted before this generation belong to an
    /// earlier state of the circuit and are ignored.
    since: u64,
    /// An idle entry is forgotten after this instant.
    expires: Instant,
}

#[derive(Debug)]
struct Circuits {
    entries: HashMap<CircuitKey, Entry>,
    /// Deployments whose provider answered 429 with a retry-after, until
    /// that wait ends. Routing skips them meanwhile; a request that reaches
    /// one anyway is sent as before.
    throttled: HashMap<CircuitKey, Instant>,
    /// Advances whenever a circuit opens or recovers.
    generation: u64,
    probes: u64,
    swept: Option<Instant>,
}

/// A request refused because its deployment's circuit is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CircuitOpen {
    pub retry_after_seconds: u32,
}

/// What a provider call says about the provider's health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Healthy,
    Failed { retry_after: Option<Duration> },
    Inconclusive,
}

impl Outcome {
    fn of(error: &ProviderError) -> Self {
        match error.category() {
            ProviderErrorCategory::Unavailable
            | ProviderErrorCategory::Timeout
            | ProviderErrorCategory::RateLimited
            | ProviderErrorCategory::Provider
            | ProviderErrorCategory::Protocol => Self::Failed {
                retry_after: error.retry_after(),
            },
            ProviderErrorCategory::Authentication
            | ProviderErrorCategory::Authorization
            | ProviderErrorCategory::InvalidRequest
            | ProviderErrorCategory::UnsupportedFeature => Self::Healthy,
            ProviderErrorCategory::Cancelled | ProviderErrorCategory::Internal => {
                Self::Inconclusive
            }
        }
    }
}

/// Throttled deployments a process remembers; past this, new ones are not
/// remembered until old waits end.
const MAX_THROTTLED: usize = 10_000;
/// The longest provider wait routing honours: a day.
const MAX_THROTTLE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub(crate) struct CircuitBreakers {
    settings: CircuitBreakerSettings,
    circuits: Arc<Mutex<Circuits>>,
}

impl Default for CircuitBreakers {
    fn default() -> Self {
        Self::new(CircuitBreakerSettings::default())
    }
}

impl CircuitBreakers {
    pub(crate) fn new(settings: CircuitBreakerSettings) -> Self {
        Self {
            settings,
            circuits: Arc::new(Mutex::new(Circuits {
                entries: HashMap::new(),
                throttled: HashMap::new(),
                generation: 0,
                probes: 0,
                swept: None,
            })),
        }
    }

    fn circuits(&self) -> MutexGuard<'_, Circuits> {
        self.circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Admit a request to the deployment in `context`, or refuse it while
    /// the circuit is open or its single probe is in flight.
    pub(crate) fn admit(
        &self,
        organization_id: OrganizationId,
        context: &InferenceContext,
    ) -> Result<CircuitPermit, CircuitOpen> {
        self.admit_at(CircuitKey::new(organization_id, context), Instant::now())
    }

    /// Whether a route should skip this deployment now, without admitting
    /// a request to it. A half-open circuit with no probe in flight is
    /// healthy: the request it gets is its probe.
    pub(crate) fn health(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: Option<&str>,
        deployment: &str,
    ) -> Option<Unhealthy> {
        self.health_at(
            &CircuitKey::of(organization_id, adapter_instance_key, deployment),
            Instant::now(),
        )
    }

    fn health_at(&self, key: &CircuitKey, now: Instant) -> Option<Unhealthy> {
        let mut circuits = self.circuits();
        let open = match circuits.entries.get(key).map(|entry| entry.circuit) {
            Some(Circuit::Open { until, .. }) => now < until,
            Some(Circuit::HalfOpen {
                probe: Some(probe), ..
            }) => now < probe.until,
            _ => false,
        };
        if open {
            return Some(Unhealthy::CircuitOpen);
        }
        match circuits.throttled.get(key) {
            Some(until) if now < *until => Some(Unhealthy::RateLimited),
            Some(_) => {
                circuits.throttled.remove(key);
                None
            }
            None => None,
        }
    }

    /// Remembers that the provider asked for `retry_after` before this
    /// deployment is called again, the whole wait (not the circuit's own
    /// backoff limit), bounded only by [`MAX_THROTTLE`] against absurd
    /// values.
    fn throttle_at(&self, key: &CircuitKey, retry_after: Duration, now: Instant) {
        let until = now + retry_after.min(MAX_THROTTLE);
        let mut circuits = self.circuits();
        let throttled = &mut circuits.throttled;
        if throttled.len() >= MAX_THROTTLED && !throttled.contains_key(key) {
            throttled.retain(|_, until| now < *until);
            if throttled.len() >= MAX_THROTTLED {
                return;
            }
        }
        let entry = throttled.entry(key.clone()).or_insert(until);
        *entry = (*entry).max(until);
    }

    /// Open the circuit of the deployment in `context`.
    #[cfg(test)]
    pub(crate) fn trip(&self, organization_id: OrganizationId, context: &InferenceContext) {
        let key = CircuitKey::new(organization_id, context);
        for _ in 0..self.settings.failure_threshold {
            self.admit_at(key.clone(), Instant::now())
                .unwrap()
                .record(Outcome::Failed { retry_after: None });
        }
    }

    fn admit_at(&self, key: CircuitKey, now: Instant) -> Result<CircuitPermit, CircuitOpen> {
        let mut circuits = self.circuits();
        self.sweep(&mut circuits, now);
        let generation = circuits.generation;
        let lease = Probe {
            id: circuits.probes,
            until: now + self.settings.probe_lease,
        };
        let probe = match circuits.entries.get_mut(&key) {
            None
            | Some(Entry {
                circuit: Circuit::Closed { .. },
                ..
            }) => None,
            Some(entry) => {
                // Refused requests keep the circuit from being forgotten.
                entry.expires = now + self.settings.forget_after;
                let open_for = match entry.circuit {
                    Circuit::Open { until, .. } if now < until => {
                        return Err(CircuitOpen {
                            retry_after_seconds: seconds_until(until, now),
                        });
                    }
                    Circuit::HalfOpen {
                        probe: Some(probe), ..
                    } if now < probe.until => {
                        // The probe usually answers within seconds.
                        return Err(CircuitOpen {
                            retry_after_seconds: 1,
                        });
                    }
                    Circuit::Open { open_for, .. } | Circuit::HalfOpen { open_for, .. } => open_for,
                    Circuit::Closed { .. } => unreachable!("closed circuits have no probe"),
                };
                entry.circuit = Circuit::HalfOpen {
                    probe: Some(lease),
                    open_for,
                };
                Some(lease.id)
            }
        };
        if probe.is_some() {
            circuits.probes += 1;
        }
        drop(circuits);
        Ok(CircuitPermit {
            breakers: self.clone(),
            key,
            probe,
            generation,
            recorded: false,
        })
    }

    fn record_at(&self, permit: &CircuitPermit, outcome: Outcome, now: Instant) {
        let key = &permit.key;
        let mut circuits = self.circuits();
        let current = circuits.entries.get(key).copied();
        if current.is_some_and(|entry| permit.generation < entry.since) {
            // Admitted before the circuit last opened or recovered.
            return;
        }
        let generation = circuits.generation;
        let circuit = current.map_or(Circuit::Closed { failures: 0 }, |entry| entry.circuit);
        let next = match (circuit, outcome) {
            // A recovered circuit keeps its entry to mark the recovery.
            (Circuit::Closed { .. }, Outcome::Healthy) => current
                .filter(|entry| entry.since > 0)
                .map(|_| Circuit::Closed { failures: 0 }),
            (Circuit::Closed { failures }, Outcome::Failed { retry_after }) => {
                let failures = failures.saturating_add(1);
                if failures >= self.settings.failure_threshold {
                    tracing::warn!(
                        organization_id = %key.organization_id.0,
                        adapter_instance_key = %key.adapter_instance_key,
                        deployment = %key.deployment,
                        failures,
                        "provider circuit opened"
                    );
                    metrics().inc(&CIRCUIT_TRANSITIONS, &["opened"]);
                    circuits.generation += 1;
                    Some(self.open(self.settings.open_for, retry_after, now))
                } else {
                    Some(Circuit::Closed { failures })
                }
            }
            // Any probe of this opening that answers proves recovery, even
            // one whose lease has passed to a later probe.
            (Circuit::HalfOpen { .. }, Outcome::Healthy) if permit.probe.is_some() => {
                tracing::info!(
                    organization_id = %key.organization_id.0,
                    adapter_instance_key = %key.adapter_instance_key,
                    deployment = %key.deployment,
                    "provider circuit closed"
                );
                metrics().inc(&CIRCUIT_TRANSITIONS, &["closed"]);
                // Remember the recovery so that late failures of
                // requests admitted before it cannot reopen the circuit.
                circuits.generation += 1;
                Some(Circuit::Closed { failures: 0 })
            }
            (
                Circuit::HalfOpen {
                    probe: Some(probe),
                    open_for,
                },
                Outcome::Failed { retry_after },
            ) if permit.probe == Some(probe.id) => {
                let open_for = open_for.saturating_mul(2).min(self.settings.max_open_for);
                tracing::warn!(
                    organization_id = %key.organization_id.0,
                    adapter_instance_key = %key.adapter_instance_key,
                    deployment = %key.deployment,
                    open_for_seconds = open_for.as_secs(),
                    "provider circuit probe failed"
                );
                metrics().inc(&CIRCUIT_TRANSITIONS, &["reopened"]);
                circuits.generation += 1;
                Some(self.open(open_for, retry_after, now))
            }
            (
                Circuit::HalfOpen {
                    probe: Some(probe),
                    open_for,
                },
                Outcome::Inconclusive,
            ) if permit.probe == Some(probe.id) => Some(Circuit::HalfOpen {
                probe: None,
                open_for,
            }),
            // Inconclusive outcomes, and outcomes of requests that are not
            // the circuit's current probe, leave the state as it is and
            // never create an entry.
            (state, _) => current.map(|_| state),
        };
        let since = if circuits.generation == generation {
            current.map_or(0, |entry| entry.since)
        } else {
            circuits.generation
        };
        match next {
            Some(circuit) => {
                let expires = now + self.settings.forget_after;
                circuits.entries.insert(
                    key.clone(),
                    Entry {
                        circuit,
                        since,
                        expires,
                    },
                );
            }
            None => {
                circuits.entries.remove(key);
            }
        }
    }

    /// A probe that never reached the provider frees the probe slot, unless
    /// its lease has passed to a later probe.
    fn release(&self, permit: &CircuitPermit) {
        if let Some(Entry {
            circuit: Circuit::HalfOpen { probe, .. },
            ..
        }) = self.circuits().entries.get_mut(&permit.key)
        {
            if probe.is_some_and(|probe| permit.probe == Some(probe.id)) {
                *probe = None;
            }
        }
    }

    /// Forget circuits no request has touched for `forget_after`, at most
    /// once per `forget_after`. A circuit whose probe holds its lease is
    /// kept.
    fn sweep(&self, circuits: &mut Circuits, now: Instant) {
        if circuits
            .swept
            .is_some_and(|swept| now < swept + self.settings.forget_after)
        {
            return;
        }
        circuits.swept = Some(now);
        circuits.entries.retain(|_, entry| {
            now < entry.expires
                || matches!(
                    entry.circuit,
                    Circuit::HalfOpen { probe: Some(probe), .. } if now < probe.until
                )
        });
    }

    /// Open for `open_for`, or longer when the provider asked for it, but
    /// never longer than `max_open_for`. The wait is what a failed probe
    /// doubles.
    fn open(&self, open_for: Duration, retry_after: Option<Duration>, now: Instant) -> Circuit {
        let open_for = retry_after
            .map_or(open_for, |retry_after| open_for.max(retry_after))
            .min(self.settings.max_open_for);
        Circuit::Open {
            until: now + open_for,
            open_for,
        }
    }
}

fn seconds_until(until: Instant, now: Instant) -> u32 {
    let wait = until.saturating_duration_since(now);
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    u32::try_from(seconds.max(1)).unwrap_or(u32::MAX)
}

/// One admitted request. It reports the provider outcome once; dropping it
/// unreported means the request never reached the provider, or ended
/// without an answer, and says nothing about the provider's health.
pub(crate) struct CircuitPermit {
    breakers: CircuitBreakers,
    key: CircuitKey,
    /// The probe id when this request probes a half-open circuit.
    probe: Option<u64>,
    generation: u64,
    recorded: bool,
}

impl CircuitPermit {
    fn record(self, outcome: Outcome) {
        self.record_at(outcome, Instant::now());
    }

    fn record_at(mut self, outcome: Outcome, now: Instant) {
        self.recorded = true;
        self.breakers.record_at(&self, outcome, now);
    }
}

impl Drop for CircuitPermit {
    fn drop(&mut self) {
        if !self.recorded && self.probe.is_some() {
            self.breakers.release(self);
        }
    }
}

/// Adapter decorator for one request: reports the provider outcome of the
/// request's call to its permit, and to the provider call metrics.
pub(crate) struct CircuitGuardedAdapter {
    inner: Arc<dyn InferenceProviderAdapter>,
    permit: Arc<Mutex<Option<CircuitPermit>>>,
}

impl CircuitGuardedAdapter {
    pub(crate) fn new(inner: Arc<dyn InferenceProviderAdapter>, permit: CircuitPermit) -> Self {
        Self {
            inner,
            permit: Arc::new(Mutex::new(Some(permit))),
        }
    }

    fn record(&self, outcome: Outcome) {
        record(&self.permit, outcome);
    }

    fn record_result<T>(&self, result: &Result<T, ProviderError>) {
        if let Err(error) = result {
            throttle_on(&self.permit, error);
        }
        self.record(match result {
            Ok(_) => Outcome::Healthy,
            Err(error) => Outcome::of(error),
        });
    }
}

/// One provider call's metric labels and start. A call that is dropped
/// unfinished (its future cancelled, or its stream dropped before the end,
/// usually because the client went away) is counted as `abandoned`.
struct CallTimer {
    family: &'static str,
    operation: &'static str,
    started: Instant,
    finished: bool,
}

impl CallTimer {
    fn start(context: &InferenceContext, operation: &'static str) -> Self {
        Self {
            family: crate::metrics::provider_family_label(context.provider_family.as_deref()),
            operation,
            started: Instant::now(),
            finished: false,
        }
    }

    fn finish(mut self, outcome: &'static str) {
        self.record(outcome);
    }

    fn record(&mut self, outcome: &'static str) {
        self.finished = true;
        let registry = metrics();
        registry.inc(&PROVIDER_CALLS, &[self.family, self.operation, outcome]);
        registry.observe(
            &PROVIDER_DURATION,
            &[self.family, self.operation],
            self.started.elapsed(),
        );
    }

    fn finish_result<T>(self, result: &Result<T, ProviderError>) {
        self.finish(match result {
            Ok(_) => "ok",
            Err(error) => category_label(error.category()),
        });
    }
}

impl Drop for CallTimer {
    fn drop(&mut self) {
        if !self.finished {
            self.record("abandoned");
        }
    }
}

fn category_label(category: ProviderErrorCategory) -> &'static str {
    match category {
        ProviderErrorCategory::Authentication => "authentication",
        ProviderErrorCategory::Authorization => "authorization",
        ProviderErrorCategory::InvalidRequest => "invalid_request",
        ProviderErrorCategory::UnsupportedFeature => "unsupported_feature",
        ProviderErrorCategory::RateLimited => "rate_limited",
        ProviderErrorCategory::Unavailable => "unavailable",
        ProviderErrorCategory::Timeout => "timeout",
        ProviderErrorCategory::Cancelled => "cancelled",
        ProviderErrorCategory::Provider => "provider",
        ProviderErrorCategory::Protocol => "protocol",
        ProviderErrorCategory::Internal => "internal",
    }
}

/// A 429 with a retry-after marks the permit's deployment throttled, so
/// routing prefers its other deployments until the wait ends.
fn throttle_on(permit: &Mutex<Option<CircuitPermit>>, error: &ProviderError) {
    if error.category() != ProviderErrorCategory::RateLimited {
        return;
    }
    let Some(retry_after) = error.retry_after() else {
        return;
    };
    let permit = permit
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(permit) = permit.as_ref() {
        permit
            .breakers
            .throttle_at(&permit.key, retry_after, Instant::now());
    }
}

fn record(permit: &Mutex<Option<CircuitPermit>>, outcome: Outcome) {
    let permit = permit
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(permit) = permit {
        permit.record(outcome);
    }
}

#[async_trait]
impl InferenceProviderAdapter for CircuitGuardedAdapter {
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

    /// Counting precedes the call itself, so only a failure is reported;
    /// success leaves the outcome to the call.
    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        let timer = CallTimer::start(context, "count_tokens");
        let result = self.inner.count_chat_input_tokens(context, request).await;
        timer.finish_result(&result);
        if let Err(error) = &result {
            throttle_on(&self.permit, error);
            if matches!(Outcome::of(error), Outcome::Failed { .. }) {
                self.record(Outcome::of(error));
            }
        }
        result
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let timer = CallTimer::start(&context, "chat");
        let result = self.inner.chat(context, request).await;
        timer.finish_result(&result);
        self.record_result(&result);
        result
    }

    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let timer = CallTimer::start(&context, "chat_stream");
        match self.inner.chat_stream(context, request).await {
            Ok(inner) => Ok(ValidatingChatEventStream::new(GuardedStream {
                inner,
                permit: self.permit.clone(),
                timer: Some(timer),
            })),
            Err(error) => {
                timer.finish(category_label(error.category()));
                throttle_on(&self.permit, &error);
                self.record(Outcome::of(&error));
                Err(error)
            }
        }
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        let timer = CallTimer::start(&context, "embeddings");
        let result = self.inner.embeddings(context, request).await;
        timer.finish_result(&result);
        self.record_result(&result);
        result
    }
}

/// A stream is healthy once it completes, and failed on a provider error or
/// an early end. A stream dropped midway, usually because the client went
/// away, reports nothing to its circuit, and its timer counts it abandoned.
struct GuardedStream {
    inner: ChatEventStream,
    permit: Arc<Mutex<Option<CircuitPermit>>>,
    /// Taken when the call's outcome is known.
    timer: Option<CallTimer>,
}

impl Stream for GuardedStream {
    type Item = Result<ChatStreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let item = Pin::new(&mut this.inner).poll_next(cx);
        let outcome = match &item {
            Poll::Ready(Some(Ok(ChatStreamEvent::ResponseCompleted(_)))) => {
                record(&this.permit, Outcome::Healthy);
                Some("ok")
            }
            Poll::Ready(Some(Err(error))) => {
                record(&this.permit, Outcome::of(error));
                Some(category_label(error.category()))
            }
            Poll::Ready(None) => {
                record(&this.permit, Outcome::Failed { retry_after: None });
                Some("incomplete")
            }
            _ => None,
        };
        if let Some(outcome) = outcome {
            if let Some(timer) = this.timer.take() {
                timer.finish(outcome);
            }
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, ProviderAttemptIdempotencyKey, ResponseCompletion,
        ResponseStarted, SafeMetadata,
    };
    use futures::{StreamExt, stream};

    const FAILED: Outcome = Outcome::Failed { retry_after: None };

    fn settings() -> CircuitBreakerSettings {
        CircuitBreakerSettings {
            failure_threshold: 3,
            open_for: Duration::from_secs(10),
            max_open_for: Duration::from_secs(25),
            probe_lease: Duration::from_secs(30),
            forget_after: Duration::from_secs(60),
        }
    }

    fn key(organization_id: OrganizationId, deployment: &str) -> CircuitKey {
        CircuitKey {
            organization_id,
            adapter_instance_key: "primary".into(),
            deployment: deployment.into(),
        }
    }

    fn fail(breakers: &CircuitBreakers, key: &CircuitKey, times: u32, now: Instant) {
        for _ in 0..times {
            breakers
                .admit_at(key.clone(), now)
                .unwrap()
                .record_at(FAILED, now);
        }
    }

    fn state(breakers: &CircuitBreakers, key: &CircuitKey) -> Option<Circuit> {
        breakers
            .circuits()
            .entries
            .get(key)
            .map(|entry| entry.circuit)
    }

    #[test]
    fn routing_sees_open_circuits_probes_in_flight_and_throttled_deployments() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        assert_eq!(breakers.health_at(&deployment, now), None);
        fail(&breakers, &deployment, 3, now);
        assert_eq!(
            breakers.health_at(&deployment, now),
            Some(Unhealthy::CircuitOpen)
        );
        // Past the open period the next request probes: healthy until one
        // does, then skipped while the probe is in flight.
        let later = now + Duration::from_secs(11);
        assert_eq!(breakers.health_at(&deployment, later), None);
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        assert_eq!(
            breakers.health_at(&deployment, later),
            Some(Unhealthy::CircuitOpen)
        );
        probe.record_at(Outcome::Healthy, later);
        assert_eq!(breakers.health_at(&deployment, later), None);

        // A 429's retry-after, all of it: longer than the circuit's own
        // `max_open_for` (25 seconds here).
        breakers.throttle_at(&deployment, Duration::from_secs(600), later);
        assert_eq!(
            breakers.health_at(&deployment, later + Duration::from_secs(599)),
            Some(Unhealthy::RateLimited)
        );
        assert_eq!(
            breakers.health_at(&deployment, later + Duration::from_secs(601)),
            None
        );
        assert!(breakers.circuits().throttled.is_empty());
    }

    #[test]
    fn consecutive_failures_open_the_circuit_and_success_resets_the_count() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 2, now);
        let permit = breakers.admit_at(deployment.clone(), now).unwrap();
        permit.record(Outcome::Healthy);
        assert_eq!(state(&breakers, &deployment), None);

        fail(&breakers, &deployment, 3, now);
        assert_eq!(
            breakers.admit_at(deployment.clone(), now).err(),
            Some(CircuitOpen {
                retry_after_seconds: 10
            })
        );
        assert_eq!(
            breakers
                .admit_at(deployment, now + Duration::from_millis(9_500))
                .err(),
            Some(CircuitOpen {
                retry_after_seconds: 1
            })
        );
    }

    #[test]
    fn healthy_answers_and_inconclusive_outcomes_never_open_a_circuit() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        for category in [
            ProviderErrorCategory::InvalidRequest,
            ProviderErrorCategory::Authentication,
            ProviderErrorCategory::Cancelled,
            ProviderErrorCategory::Internal,
        ] {
            for _ in 0..5 {
                let permit = breakers.admit_at(deployment.clone(), now).unwrap();
                permit.record(Outcome::of(&ProviderError::new(category)));
            }
        }
        assert!(breakers.admit_at(deployment, now).is_ok());
        for category in [
            ProviderErrorCategory::Unavailable,
            ProviderErrorCategory::Timeout,
            ProviderErrorCategory::RateLimited,
            ProviderErrorCategory::Provider,
            ProviderErrorCategory::Protocol,
        ] {
            assert!(matches!(
                Outcome::of(&ProviderError::new(category)),
                Outcome::Failed { .. }
            ));
        }
    }

    #[test]
    fn circuits_are_isolated_per_tenant_and_deployment() {
        let breakers = CircuitBreakers::new(settings());
        let org = OrganizationId::new();
        let now = Instant::now();
        fail(&breakers, &key(org, "chat"), 3, now);
        assert!(breakers.admit_at(key(org, "chat"), now).is_err());
        assert!(breakers.admit_at(key(org, "other"), now).is_ok());
        assert!(
            breakers
                .admit_at(key(OrganizationId::new(), "chat"), now)
                .is_ok()
        );
    }

    #[test]
    fn one_probe_closes_the_circuit_or_reopens_it_for_longer() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 3, now);

        // After the open period one probe goes through at a time.
        let later = now + Duration::from_secs(10);
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        assert!(probe.probe.is_some());
        assert_eq!(
            breakers.admit_at(deployment.clone(), later).err(),
            Some(CircuitOpen {
                retry_after_seconds: 1
            })
        );
        // A probe failure doubles the open period, up to the maximum.
        probe.record_at(FAILED, later);
        assert!(matches!(
            state(&breakers, &deployment),
            Some(Circuit::Open { open_for, .. }) if open_for == Duration::from_secs(20)
        ));
        let later = later + Duration::from_secs(20);
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        probe.record_at(FAILED, later);
        assert!(matches!(
            state(&breakers, &deployment),
            Some(Circuit::Open { open_for, .. }) if open_for == Duration::from_secs(25)
        ));

        let later = later + Duration::from_secs(25);
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        probe.record(Outcome::Healthy);
        assert_eq!(
            state(&breakers, &deployment),
            Some(Circuit::Closed { failures: 0 })
        );
        assert!(breakers.admit_at(deployment, later).is_ok());
    }

    #[test]
    fn a_probe_that_never_answers_frees_the_probe_slot() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 3, now);
        let later = now + Duration::from_secs(10);
        drop(breakers.admit_at(deployment.clone(), later).unwrap());
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        probe.record(Outcome::Inconclusive);
        assert!(
            breakers
                .admit_at(deployment, later)
                .unwrap()
                .probe
                .is_some()
        );
    }

    #[test]
    fn a_probe_holds_its_slot_only_for_its_lease() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 3, now);
        let later = now + Duration::from_secs(10);
        let stuck = breakers.admit_at(deployment.clone(), later).unwrap();
        // Sweeps keep the half-open circuit however long the probe takes.
        let later = later + Duration::from_secs(29);
        assert!(breakers.admit_at(deployment.clone(), later).is_err());
        let later = later + Duration::from_secs(1);
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        assert!(probe.probe.is_some());
        assert!(breakers.admit_at(deployment.clone(), later).is_err());

        // The superseded probe cannot free the slot or reopen the circuit,
        // but its success still proves recovery.
        stuck.record_at(Outcome::Healthy, later);
        assert!(breakers.admit_at(deployment.clone(), later).is_ok());
        probe.record_at(FAILED, later);
        assert!(breakers.admit_at(deployment, later).is_ok());
    }

    #[test]
    fn a_failed_superseded_probe_does_not_reopen_the_circuit() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 3, now);
        let later = now + Duration::from_secs(10);
        let stuck = breakers.admit_at(deployment.clone(), later).unwrap();
        let later = later + Duration::from_secs(30);
        let probe = breakers.admit_at(deployment.clone(), later).unwrap();
        stuck.record_at(FAILED, later);
        drop(probe);
        // Only the current probe's failure would have reopened the circuit.
        assert!(matches!(
            state(&breakers, &deployment),
            Some(Circuit::HalfOpen { probe: None, .. })
        ));
    }

    #[test]
    fn a_circuit_is_forgotten_only_once_nothing_touches_it() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 3, now);
        let later = now + Duration::from_secs(10);
        let abandoned = breakers.admit_at(deployment.clone(), later).unwrap();
        // Refused requests keep the half-open circuit, and its gate, alive.
        let mut later = later;
        for _ in 0..3 {
            later += Duration::from_secs(40);
            let probe = breakers.admit_at(deployment.clone(), later).unwrap();
            assert!(probe.probe.is_some());
            assert!(breakers.admit_at(deployment.clone(), later).is_err());
            std::mem::forget(probe);
        }
        drop(abandoned);
        // Once no request touches it for `forget_after` and no probe holds a
        // lease, it is forgotten.
        let later = later + Duration::from_secs(90);
        drop(breakers.admit_at(key(OrganizationId::new(), "other"), later));
        assert_eq!(state(&breakers, &deployment), None);
    }

    #[test]
    fn a_stale_probe_does_not_free_a_later_probe_slot() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 3, now);
        let later = now + Duration::from_secs(10);
        let stale = breakers.admit_at(deployment.clone(), later).unwrap();
        // The circuit is forgotten under the stale probe, then trips and
        // admits a new probe.
        breakers.circuits().entries.clear();
        fail(&breakers, &deployment, 3, later);
        let later = later + Duration::from_secs(10);
        let _probe = breakers.admit_at(deployment.clone(), later).unwrap();
        drop(stale);
        assert!(breakers.admit_at(deployment, later).is_err());
    }

    #[test]
    fn late_answers_from_before_the_trip_do_not_close_the_circuit() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        let early = breakers.admit_at(deployment.clone(), now).unwrap();
        fail(&breakers, &deployment, 3, now);
        early.record(Outcome::Healthy);
        assert!(breakers.admit_at(deployment, now).is_err());
    }

    #[test]
    fn provider_retry_after_extends_the_open_period_within_the_maximum() {
        let breakers = CircuitBreakers::new(settings());
        let now = Instant::now();
        for (retry_after, expected) in [(5, 10), (18, 18), (600, 25)] {
            let deployment = key(OrganizationId::new(), "chat");
            fail(&breakers, &deployment, 2, now);
            breakers
                .admit_at(deployment.clone(), now)
                .unwrap()
                .record_at(
                    Outcome::Failed {
                        retry_after: Some(Duration::from_secs(retry_after)),
                    },
                    now,
                );
            assert_eq!(
                breakers.admit_at(deployment, now).err(),
                Some(CircuitOpen {
                    retry_after_seconds: expected
                })
            );
        }
    }

    #[test]
    fn a_failed_probe_doubles_the_wait_the_provider_asked_for() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        fail(&breakers, &deployment, 2, now);
        breakers
            .admit_at(deployment.clone(), now)
            .unwrap()
            .record_at(
                Outcome::Failed {
                    retry_after: Some(Duration::from_secs(12)),
                },
                now,
            );
        let later = now + Duration::from_secs(12);
        breakers
            .admit_at(deployment.clone(), later)
            .unwrap()
            .record_at(FAILED, later);
        assert_eq!(
            breakers.admit_at(deployment, later).err(),
            Some(CircuitOpen {
                retry_after_seconds: 24
            })
        );
    }

    #[test]
    fn late_failures_from_before_a_recovery_do_not_reopen_the_circuit() {
        let breakers = CircuitBreakers::new(settings());
        let deployment = key(OrganizationId::new(), "chat");
        let now = Instant::now();
        let slow: Vec<_> = (0..3)
            .map(|_| breakers.admit_at(deployment.clone(), now).unwrap())
            .collect();
        fail(&breakers, &deployment, 3, now);
        let later = now + Duration::from_secs(10);
        breakers
            .admit_at(deployment.clone(), later)
            .unwrap()
            .record_at(Outcome::Healthy, later);
        // A healthy answer after the recovery keeps the recovery marker.
        breakers
            .admit_at(deployment.clone(), later)
            .unwrap()
            .record_at(Outcome::Healthy, later);
        for permit in slow {
            permit.record_at(FAILED, later);
        }
        assert_eq!(
            state(&breakers, &deployment),
            Some(Circuit::Closed { failures: 0 })
        );

        // Failures of requests admitted after the recovery still count.
        fail(&breakers, &deployment, 3, later);
        assert!(breakers.admit_at(deployment, later).is_err());
    }

    #[test]
    fn idle_circuits_are_forgotten() {
        let breakers = CircuitBreakers::new(settings());
        let now = Instant::now();
        let failing = key(OrganizationId::new(), "chat");
        let open = key(OrganizationId::new(), "chat");
        fail(&breakers, &failing, 1, now);
        fail(&breakers, &open, 3, now);
        breakers
            .admit_at(key(OrganizationId::new(), "chat"), now)
            .unwrap()
            .record_at(Outcome::Inconclusive, now);
        assert_eq!(breakers.circuits().entries.len(), 2);

        // Sweeps run at most once per `forget_after`.
        let later = now + Duration::from_secs(59);
        drop(breakers.admit_at(key(OrganizationId::new(), "other"), later));
        assert!(state(&breakers, &failing).is_some());
        let later = later + Duration::from_secs(1);
        drop(breakers.admit_at(key(OrganizationId::new(), "other"), later));
        assert_eq!(state(&breakers, &failing), None);
        assert_eq!(state(&breakers, &open), None);
    }

    struct Scripted {
        chat: ProviderErrorCategory,
        stream: Vec<Result<ChatStreamEvent, ProviderError>>,
    }

    #[async_trait]
    impl InferenceProviderAdapter for Scripted {
        async fn chat(
            &self,
            _: InferenceContext,
            _: ChatRequest,
        ) -> Result<ChatResponse, ProviderError> {
            Err(ProviderError::new(self.chat))
        }

        async fn chat_stream(
            &self,
            _: InferenceContext,
            _: ChatRequest,
        ) -> Result<ChatEventStream, ProviderError> {
            Ok(ValidatingChatEventStream::new(stream::iter(
                self.stream.clone(),
            )))
        }

        async fn embeddings(
            &self,
            _: InferenceContext,
            _: EmbeddingRequest,
        ) -> Result<EmbeddingResponse, ProviderError> {
            Err(ProviderError::new(self.chat))
        }
    }

    fn context() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            DeploymentModelReference::new("chat", "model").unwrap(),
        )
        .with_adapter_instance_key("primary")
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            messages: vec![],
            temperature: None,
            max_tokens: None,
            ..Default::default()
        }
    }

    fn started() -> ChatStreamEvent {
        ChatStreamEvent::ResponseStarted(
            ResponseStarted::new(None, "model".to_owned(), SafeMetadata::new()).unwrap(),
        )
    }

    fn completed() -> ChatStreamEvent {
        ChatStreamEvent::ResponseCompleted(
            ResponseCompletion::new(None, "model".to_owned(), None).unwrap(),
        )
    }

    fn guarded(
        breakers: &CircuitBreakers,
        org: OrganizationId,
        adapter: Scripted,
    ) -> CircuitGuardedAdapter {
        CircuitGuardedAdapter::new(Arc::new(adapter), breakers.admit(org, &context()).unwrap())
    }

    fn failures(breakers: &CircuitBreakers, org: OrganizationId) -> Option<Circuit> {
        state(breakers, &CircuitKey::new(org, &context()))
    }

    #[tokio::test]
    async fn guarded_calls_report_the_provider_outcome() {
        let breakers = CircuitBreakers::new(settings());
        let org = OrganizationId::new();
        let adapter = guarded(
            &breakers,
            org,
            Scripted {
                chat: ProviderErrorCategory::Unavailable,
                stream: vec![],
            },
        );
        assert!(adapter.chat(context(), chat_request()).await.is_err());
        // Only the first outcome of a request counts.
        assert!(adapter.chat(context(), chat_request()).await.is_err());
        assert_eq!(
            failures(&breakers, org),
            Some(Circuit::Closed { failures: 1 })
        );

        let stream = |events| Scripted {
            chat: ProviderErrorCategory::Unavailable,
            stream: events,
        };
        // A stream that ends early fails.
        let adapter = guarded(&breakers, org, stream(vec![Ok(started())]));
        let events: Vec<_> = adapter
            .chat_stream(context(), chat_request())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.last().unwrap().is_err());
        assert_eq!(
            failures(&breakers, org),
            Some(Circuit::Closed { failures: 2 })
        );
        // A stream dropped before its end reports nothing.
        let adapter = guarded(&breakers, org, stream(vec![Ok(started()), Ok(completed())]));
        let mut events = adapter
            .chat_stream(context(), chat_request())
            .await
            .unwrap();
        assert!(events.next().await.unwrap().is_ok());
        drop(events);
        assert_eq!(
            failures(&breakers, org),
            Some(Circuit::Closed { failures: 2 })
        );
        // A completed stream is healthy and resets the count.
        let adapter = guarded(&breakers, org, stream(vec![Ok(started()), Ok(completed())]));
        let events: Vec<_> = adapter
            .chat_stream(context(), chat_request())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(failures(&breakers, org), None);
    }

    #[tokio::test]
    async fn guarded_calls_are_counted_by_family_operation_and_outcome() {
        // No other test calls this family, so the counts are this test's.
        let family = "azure-ai-foundry";
        let calls = |operation, outcome| {
            metrics()
                .value(&PROVIDER_CALLS, &[family, operation, outcome])
                .unwrap_or(0)
        };
        let breakers = CircuitBreakers::new(settings());
        let org = OrganizationId::new();
        let context = || context().with_provider_family(family);
        let adapter = |events| {
            CircuitGuardedAdapter::new(
                Arc::new(Scripted {
                    chat: ProviderErrorCategory::Timeout,
                    stream: events,
                }),
                breakers.admit(org, &context()).unwrap(),
            )
        };

        assert!(
            adapter(vec![])
                .chat(context(), chat_request())
                .await
                .is_err()
        );
        assert_eq!(calls("chat", "timeout"), 1);
        assert_eq!(
            metrics().value(&PROVIDER_DURATION, &[family, "chat"]),
            Some(1)
        );

        let complete = vec![Ok(started()), Ok(completed())];
        let events: Vec<_> = adapter(complete.clone())
            .chat_stream(context(), chat_request())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(calls("chat_stream", "ok"), 1);

        let mut events = adapter(complete)
            .chat_stream(context(), chat_request())
            .await
            .unwrap();
        assert!(events.next().await.unwrap().is_ok());
        drop(events);
        assert_eq!(calls("chat_stream", "abandoned"), 1);
        assert_eq!(calls("chat_stream", "ok"), 1);

        // A call cancelled while awaiting the provider is abandoned too.
        struct Pending;
        #[async_trait]
        impl InferenceProviderAdapter for Pending {
            async fn chat(
                &self,
                _: InferenceContext,
                _: ChatRequest,
            ) -> Result<ChatResponse, ProviderError> {
                std::future::pending().await
            }
            async fn chat_stream(
                &self,
                _: InferenceContext,
                _: ChatRequest,
            ) -> Result<ChatEventStream, ProviderError> {
                std::future::pending().await
            }
            async fn embeddings(
                &self,
                _: InferenceContext,
                _: EmbeddingRequest,
            ) -> Result<EmbeddingResponse, ProviderError> {
                std::future::pending().await
            }
        }
        let pending =
            CircuitGuardedAdapter::new(Arc::new(Pending), breakers.admit(org, &context()).unwrap());
        let call = pending.chat(context(), chat_request());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), call)
                .await
                .is_err()
        );
        assert_eq!(calls("chat", "abandoned"), 1);

        // A family the catalog does not know is never a label value.
        let unknown = CircuitGuardedAdapter::new(
            Arc::new(Scripted {
                chat: ProviderErrorCategory::Timeout,
                stream: vec![],
            }),
            breakers.admit(org, &context()).unwrap(),
        );
        let before = metrics()
            .value(&PROVIDER_CALLS, &["unknown", "embeddings", "timeout"])
            .unwrap_or(0);
        let request = EmbeddingRequest {
            input: vec!["x".into()],
        };
        let strange = context().with_provider_family("tenant-chosen-name");
        assert!(unknown.embeddings(strange, request).await.is_err());
        assert_eq!(
            metrics().value(&PROVIDER_CALLS, &["unknown", "embeddings", "timeout"]),
            Some(before + 1)
        );
        assert_eq!(
            metrics().value(
                &PROVIDER_CALLS,
                &["tenant-chosen-name", "embeddings", "timeout"]
            ),
            None
        );
    }

    #[test]
    fn circuit_transitions_are_counted() {
        let transitions = |name| metrics().value(&CIRCUIT_TRANSITIONS, &[name]).unwrap_or(0);
        let (opened, closed, reopened) = (
            transitions("opened"),
            transitions("closed"),
            transitions("reopened"),
        );
        let breakers = CircuitBreakers::new(settings());
        let key = key(OrganizationId::new(), "counted");
        let now = Instant::now();
        fail(&breakers, &key, 3, now);
        let later = now + Duration::from_secs(11);
        breakers
            .admit_at(key.clone(), later)
            .unwrap()
            .record_at(Outcome::Failed { retry_after: None }, later);
        let latest = later + Duration::from_secs(30);
        breakers
            .admit_at(key.clone(), latest)
            .unwrap()
            .record_at(Outcome::Healthy, latest);
        // Other tests open circuits concurrently, so these are lower bounds.
        assert!(transitions("opened") > opened);
        assert!(transitions("reopened") > reopened);
        assert!(transitions("closed") > closed);
    }
}
