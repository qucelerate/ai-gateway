//! Captures traced requests (D-017).
//!
//! Before a chat or embeddings request goes to its provider, the
//! organization's tracing policy (read at most once a minute per
//! organization and process) says whether it is traced and whether its
//! content is kept. Once the request is finished, its content is encrypted
//! under a new key, written to the trace store, and recorded with its
//! timing beside the request record; all of that happens in the background,
//! so tracing never delays a response, and a failure only marks the trace
//! (`failed`). At most [`MAX_WRITES`] writes are in flight per process; a
//! request finishing beyond that is not recorded at all (counted as
//! `dropped`), so a slow database or store never queues captured content.
//! Without a trace store content is marked `unavailable`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    application::trace_objects::TraceObjects,
    domain::{
        credentials::CredentialCipher,
        identity::{OrganizationId, VerifiedIdentity},
        inference::{
            ChatRequest, ChatResponse, ChatStreamEvent, EmbeddingRequest, EmbeddingResponse,
        },
        trace_content::{
            StreamAssembler, TraceDocument, TracedEmbeddingRequest, TracedEmbeddingResponse,
            TracedRequest, TracedResponse, content_aad, new_content_key, seal_content,
        },
        trace_policy::TracePolicyView,
    },
    persistence::{
        trace_content::{ContentState, NewTraceContent, StoredObject, TraceContentStore},
        trace_policy::TracePolicyStore,
    },
};

/// How long a process uses a policy it read.
pub const POLICY_TTL: Duration = Duration::from_secs(60);
/// Content writes in flight per process.
pub const MAX_WRITES: usize = 64;

#[derive(Clone)]
pub struct TraceCapture {
    inner: Arc<Inner>,
}

struct Inner {
    policies: TracePolicyStore,
    rows: TraceContentStore,
    objects: Option<Arc<dyn TraceObjects>>,
    cipher: Option<CredentialCipher>,
    /// The object name prefix is the store's; names here are
    /// `{organization}/{trace}`.
    max_bytes: usize,
    cache: Mutex<HashMap<Uuid, (Instant, Arc<TracePolicyView>)>>,
    writes: Arc<Semaphore>,
}

impl TraceCapture {
    pub fn new(
        policies: TracePolicyStore,
        rows: TraceContentStore,
        objects: Option<Arc<dyn TraceObjects>>,
        cipher: Option<CredentialCipher>,
        max_bytes: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                policies,
                rows,
                objects,
                cipher,
                max_bytes,
                cache: Mutex::new(HashMap::new()),
                writes: Arc::new(Semaphore::new(MAX_WRITES)),
            }),
        }
    }

    async fn policy(&self, organization_id: OrganizationId) -> Option<Arc<TracePolicyView>> {
        let now = Instant::now();
        if let Some((read, policy)) = self.lock_cache().get(&organization_id.0) {
            if now.duration_since(*read) < POLICY_TTL {
                return Some(policy.clone());
            }
        }
        match self.inner.policies.policy(organization_id).await {
            Ok(policy) => {
                let policy = Arc::new(policy);
                self.lock_cache()
                    .insert(organization_id.0, (now, policy.clone()));
                Some(policy)
            }
            // A request is never refused for tracing's sake: it goes
            // untraced and the next one reads again.
            Err(error) => {
                tracing::warn!(error = ?error, "tracing policy unreadable; request not traced");
                None
            }
        }
    }

    fn lock_cache(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<Uuid, (Instant, Arc<TracePolicyView>)>> {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The capture of chat `request` when its organization traces it.
    pub async fn begin(
        &self,
        identity: &VerifiedIdentity,
        request: &ChatRequest,
    ) -> Option<Capture> {
        self.begin_traced(identity, true, || {
            serde_json::to_value(TracedRequest::from(request)).unwrap_or(Value::Null)
        })
        .await
    }

    /// The capture of embeddings `request` when its organization traces it.
    pub async fn begin_embeddings(
        &self,
        identity: &VerifiedIdentity,
        request: &EmbeddingRequest,
    ) -> Option<Capture> {
        self.begin_traced(identity, false, || {
            serde_json::to_value(TracedEmbeddingRequest::from(request)).unwrap_or(Value::Null)
        })
        .await
    }

    /// The capture of a request, its traced form made by `request` only when
    /// content is kept. `streams` says its response may arrive as events.
    async fn begin_traced(
        &self,
        identity: &VerifiedIdentity,
        streams: bool,
        request: impl FnOnce() -> Value,
    ) -> Option<Capture> {
        let started_at = Utc::now();
        let policy = self.policy(identity.organization_id).await?;
        if !policy.traces(
            identity.team_id.map(|team| team.0),
            Some(identity.service_account_id.0),
        ) {
            return None;
        }
        Some(Capture {
            capture: self.clone(),
            organization_id: identity.organization_id,
            request: policy.content.then(request),
            started_at,
            provider_started_at: None,
            first_event_at: None,
            assembler: (policy.content && streams)
                .then(|| StreamAssembler::new(self.inner.max_bytes)),
        })
    }

    /// Records `capture`. Its content, when kept, is recorded as `writing`
    /// with the object's name before the object is written, so the object
    /// is never without a row naming it: an object written but not
    /// confirmed (a timed-out reply, a lost update) is deleted by the
    /// worker later rather than leaked.
    async fn write(
        self,
        capture: Capture,
        trace_id: Uuid,
        response: Option<Value>,
        response_truncated: bool,
        finished_at: DateTime<Utc>,
    ) {
        let organization_id = capture.organization_id;
        let mut row = NewTraceContent {
            organization_id,
            trace_id,
            state: ContentState::Off,
            object: None,
            request_bytes: None,
            response_bytes: None,
            request_truncated: false,
            response_truncated: false,
            started_at: capture.started_at,
            provider_started_at: capture.provider_started_at,
            first_event_at: capture.first_event_at,
            finished_at,
        };
        let mut sealed = None;
        if let Some(request) = capture.request {
            let document =
                TraceDocument::of(request, response, response_truncated, self.inner.max_bytes);
            row.request_truncated = document.truncated.request;
            row.response_truncated = document.truncated.response;
            row.request_bytes = serde_json::to_vec(&document.request)
                .ok()
                .map(|bytes| bytes.len() as u64);
            row.response_bytes = serde_json::to_vec(&document.response)
                .ok()
                .map(|bytes| bytes.len() as u64);
            row.state = ContentState::Unavailable;
            if let (Some(objects), Some(cipher)) = (&self.inner.objects, &self.inner.cipher) {
                match seal(cipher, organization_id, trace_id, &document) {
                    Ok((object, bytes)) => {
                        row.state = ContentState::Writing;
                        row.object = Some(object);
                        sealed = Some((objects.clone(), bytes));
                    }
                    Err(()) => row.state = ContentState::Failed,
                }
            }
        }
        match self.inner.rows.insert(&row).await {
            Ok(true) => {}
            // Already recorded: nothing is written over it.
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(trace_id = %trace_id, error = ?error, "trace content not recorded");
                return;
            }
        }
        let state = match (sealed, &row.object) {
            (Some((objects, bytes)), Some(object)) => {
                let stored = match objects.put(&object.name, bytes).await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(trace_id = %trace_id, error = %error, "trace content not written");
                        // Left `writing` when the object may remain, for the
                        // worker to delete.
                        if objects.delete(&object.name).await.is_err() {
                            return;
                        }
                        false
                    }
                };
                match self.inner.rows.finish_write(trace_id, stored).await {
                    Ok(true) if stored => ContentState::Stored,
                    Ok(true) => ContentState::Failed,
                    Ok(false) => return,
                    Err(error) => {
                        tracing::warn!(trace_id = %trace_id, error = ?error, "trace content outcome not recorded");
                        return;
                    }
                }
            }
            _ => row.state,
        };
        crate::metrics::global().inc(&crate::metrics::TRACE_CONTENT, &[state.as_str()]);
    }
}

/// `document` encrypted under a new key: where it goes, with the key
/// sealed, and the bytes to write.
fn seal(
    cipher: &CredentialCipher,
    organization_id: OrganizationId,
    trace_id: Uuid,
    document: &TraceDocument,
) -> Result<(StoredObject, Vec<u8>), ()> {
    let plaintext = zeroize::Zeroizing::new(serde_json::to_vec(document).map_err(|_| ())?);
    let key = new_content_key();
    let aad = content_aad(organization_id, trace_id);
    let sealed = seal_content(&key, &aad, &plaintext).map_err(|_| ())?;
    let sealed_key = cipher
        .seal_trace_content_key(organization_id, trace_id, &key)
        .map_err(|_| ())?;
    Ok((
        StoredObject {
            name: format!("{}/{}", organization_id.0, trace_id),
            key_id: sealed_key.key_id,
            sealed_key: sealed_key.ciphertext,
        },
        sealed,
    ))
}

/// One traced request in progress.
pub struct Capture {
    capture: TraceCapture,
    organization_id: OrganizationId,
    /// The request as the client sent it, in its traced form, when content
    /// is kept.
    request: Option<Value>,
    started_at: DateTime<Utc>,
    provider_started_at: Option<DateTime<Utc>>,
    first_event_at: Option<DateTime<Utc>>,
    /// A streamed response so far, when content is kept.
    assembler: Option<StreamAssembler>,
}

impl Capture {
    /// The request goes to its provider now.
    pub fn provider_started(&mut self) {
        self.provider_started_at = Some(Utc::now());
    }

    /// One event of a streamed response.
    pub fn event(&mut self, event: &ChatStreamEvent) {
        self.first_event_at.get_or_insert_with(Utc::now);
        if let Some(assembler) = &mut self.assembler {
            assembler.push(event);
        }
    }

    /// Records a finished request with `response`, if any. A streamed
    /// response is the one assembled from its events.
    pub fn finish(mut self, trace_id: Uuid, response: Option<&ChatResponse>) {
        let finished_at = Utc::now();
        let (response, truncated) = match (response, self.assembler.take()) {
            (Some(response), _) => (Some(TracedResponse::from(response)), false),
            (None, Some(assembler)) if self.first_event_at.is_some() => {
                let (response, truncated) = assembler.finish();
                (Some(response), truncated)
            }
            (None, _) => (None, false),
        };
        let response = response.and_then(|response| serde_json::to_value(response).ok());
        self.record(trace_id, response, truncated, finished_at);
    }

    /// Records a finished embeddings request with `response`, if any.
    pub fn finish_embeddings(self, trace_id: Uuid, response: Option<&EmbeddingResponse>) {
        let finished_at = Utc::now();
        let response = response.and_then(|response| {
            serde_json::to_value(TracedEmbeddingResponse::from(response)).ok()
        });
        self.record(trace_id, response, false, finished_at);
    }

    fn record(
        self,
        trace_id: Uuid,
        response: Option<Value>,
        truncated: bool,
        finished_at: DateTime<Utc>,
    ) {
        let capture = self.capture.clone();
        // Every background write holds a slot, so what waits on the database
        // or the store is bounded. Without one the trace is dropped here,
        // content and all, rather than queued.
        let Ok(permit) = capture.inner.writes.clone().try_acquire_owned() else {
            tracing::warn!(trace_id = %trace_id, "trace dropped: writes saturated");
            crate::metrics::global().inc(&crate::metrics::TRACE_CONTENT, &["dropped"]);
            return;
        };
        // A stream dropped outside a runtime (at shutdown) is not recorded.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            capture
                .write(self, trace_id, response, truncated, finished_at)
                .await;
            drop(permit);
        });
    }
}
