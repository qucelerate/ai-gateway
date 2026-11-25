use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use futures::{Stream, StreamExt};
use gateway::{
    domain::{
        audit::{
            AdminAuditDraft, AuditAction, AuditActor, AuditDetails, AuditOutcome, AuditTarget,
        },
        identity::{OrganizationId, TeamId},
        inference::*,
    },
    jobs::{
        CredentialId, CredentialReference, IdempotencyKey, JobEnvelope, JobPayload,
        PayloadSchemaVersion,
    },
    provider::{ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream},
};

fn context() -> InferenceContext {
    InferenceContext::new(
        CorrelationId::new(),
        ProviderAttemptIdempotencyKey::new("attempt-1").unwrap(),
        DeploymentModelReference::new("deployment-a", "model-a").unwrap(),
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "do not print me".into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        temperature: None,
        max_tokens: None,
        ..Default::default()
    }
}

fn provider_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Unavailable)
        .with_partial_usage(Usage::partial(
            Some(TokenCounter::estimated(2)),
            None,
            None,
            None,
            None,
        ))
        .unwrap()
}

struct ObservedStream {
    events: VecDeque<Result<ChatStreamEvent, ProviderError>>,
    drops: Arc<AtomicUsize>,
}

impl Stream for ObservedStream {
    type Item = Result<ChatStreamEvent, ProviderError>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.events.pop_front())
    }
}
impl Drop for ObservedStream {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeAdapter {
    drops: Arc<AtomicUsize>,
    abrupt: bool,
    error: bool,
}

#[async_trait::async_trait]
impl InferenceProviderAdapter for FakeAdapter {
    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse::new(
            Some("req-1".into()),
            "returned-model".into(),
            vec![
                ChatChoice {
                    index: 0,
                    message: ChatMessage {
                        role: ChatRole::Assistant,
                        content: "first".into(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    },
                    finish_reason: Some(FinishReason::new("provider-future-reason")),
                    tool_calls: vec![],
                    refusal: None,
                    safety: vec![],
                    annotations: vec![],
                },
                ChatChoice {
                    index: 1,
                    message: ChatMessage {
                        role: ChatRole::Assistant,
                        content: "second".into(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    },
                    finish_reason: None,
                    tool_calls: vec![ToolCall {
                        id: Some("call-1".into()),
                        name: Some("lookup".into()),
                        arguments: "{\"x\":1}".into(),
                    }],
                    refusal: Some("blocked".into()),
                    safety: vec![],
                    annotations: vec![],
                },
            ],
            None,
            SafeMetadata::default(),
        )
        .unwrap())
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let mut events = VecDeque::from([
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(
                    Some("req-1".into()),
                    "returned-model".into(),
                    SafeMetadata::default(),
                )
                .unwrap(),
            )),
            Ok(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                choice_index: 1,
                content_delta: Some("one".into()),
                refusal_delta: None,
                annotations: vec![],
            })),
            Ok(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                choice_index: 1,
                tool_call_index: 0,
                id: Some("call-1".into()),
                name: Some("lookup".into()),
                arguments_delta: "{\"x\":".into(),
            })),
        ]);
        if self.abrupt {
            return Ok(ValidatingChatEventStream::new(ObservedStream {
                events,
                drops: self.drops.clone(),
            }));
        }
        events.push_back(Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            Usage::unavailable(),
        ))));
        if self.error {
            events.push_back(Err(provider_error()));
            return Ok(ValidatingChatEventStream::new(ObservedStream {
                events,
                drops: self.drops.clone(),
            }));
        }
        events.push_back(Ok(ChatStreamEvent::ChoiceFinished(
            ChoiceFinished::new(1, FinishReason::new("tool_calls")).unwrap(),
        )));
        let final_usage = Usage::complete(
            TokenCounter::exact(3),
            TokenCounter::exact(4),
            None,
            None,
            None,
        )
        .unwrap();
        events.push_back(Ok(ChatStreamEvent::ResponseCompleted(
            ResponseCompletion::new(
                Some("req-1".into()),
                "returned-model".into(),
                Some(UsageSnapshot::final_(final_usage).unwrap()),
            )
            .unwrap(),
        )));
        Ok(ValidatingChatEventStream::new(ObservedStream {
            events,
            drops: self.drops.clone(),
        }))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Ok(EmbeddingResponse::new(
            Some("embed-1".into()),
            "embedding-model".into(),
            vec![
                Embedding::new(0, vec![1.0, 2.0]).unwrap(),
                Embedding::new(1, vec![3.0, 4.0]).unwrap(),
            ],
            Some(
                UsageSnapshot::final_(
                    Usage::complete(
                        TokenCounter::exact(1),
                        TokenCounter::exact(1),
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                )
                .unwrap(),
            ),
        )
        .unwrap())
    }
}

#[tokio::test]
async fn dynamic_adapter_covers_all_operations_and_ordered_stream() {
    let drops = Arc::new(AtomicUsize::new(0));
    let adapter: Arc<dyn InferenceProviderAdapter> = Arc::new(FakeAdapter {
        drops: drops.clone(),
        abrupt: false,
        error: false,
    });
    let response = adapter.chat(context(), request()).await.unwrap();
    assert_eq!(response.model(), "returned-model");
    assert_eq!(response.choices()[0].index, 0);
    assert_eq!(
        response.choices()[1].tool_calls[0].name.as_deref(),
        Some("lookup")
    );
    assert_eq!(
        response.choices()[0]
            .finish_reason
            .as_ref()
            .unwrap()
            .as_str(),
        "provider-future-reason"
    );
    let response_debug = format!("{response:?}");
    assert!(!response_debug.contains("first"));
    assert!(!response_debug.contains("{\"x\":1}"));

    let mut stream = adapter.chat_stream(context(), request()).await.unwrap();
    let events: Vec<_> = stream.by_ref().collect().await;
    assert_eq!(events.len(), 6);
    assert!(matches!(events[3], Ok(ChatStreamEvent::Usage(_))));
    if let Ok(ChatStreamEvent::Usage(snapshot)) = &events[3] {
        assert!(snapshot.cumulative());
        assert_eq!(snapshot.finality(), UsageFinality::Interim);
        assert_eq!(snapshot.usage().quality(), UsageQuality::Unknown);
    }
    assert!(matches!(events[4], Ok(ChatStreamEvent::ChoiceFinished(..))));
    assert!(matches!(
        events.last(),
        Some(Ok(ChatStreamEvent::ResponseCompleted(..)))
    ));
    assert!(
        stream.next().await.is_none(),
        "events are forbidden after completion"
    );
    drop(stream);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let mut cancelled = adapter.chat_stream(context(), request()).await.unwrap();
    assert!(cancelled.next().await.is_some());
    drop(cancelled);
    assert_eq!(drops.load(Ordering::SeqCst), 2);

    let embeddings = adapter
        .embeddings(
            context(),
            EmbeddingRequest {
                input: vec!["a".into(), "b".into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        embeddings
            .data()
            .iter()
            .map(|item| item.index())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn stream_error_and_abrupt_eof_are_not_synthesized() {
    let drops = Arc::new(AtomicUsize::new(0));
    let adapter: Arc<dyn InferenceProviderAdapter> = Arc::new(FakeAdapter {
        drops: drops.clone(),
        abrupt: true,
        error: false,
    });
    let mut stream = adapter.chat_stream(context(), request()).await.unwrap();
    let values: Vec<_> = stream.by_ref().collect().await;
    assert_eq!(values.len(), 4);
    assert!(
        !values
            .iter()
            .any(|event| matches!(event, Ok(ChatStreamEvent::ResponseCompleted(..))))
    );
    assert!(values.last().is_some_and(|event| event.is_err()));
    assert!(validate_chat_stream(&values, true).is_ok());
    drop(stream);

    let error_adapter: Arc<dyn InferenceProviderAdapter> = Arc::new(FakeAdapter {
        drops: drops.clone(),
        abrupt: false,
        error: true,
    });
    let mut provider_error_stream = error_adapter
        .chat_stream(context(), request())
        .await
        .unwrap();
    let values: Vec<_> = provider_error_stream.by_ref().collect().await;
    assert!(values.last().is_some_and(|event| event.is_err()));
    assert!(provider_error_stream.next().await.is_none());
    assert!(validate_chat_stream(&values, true).is_ok());

    let mut error_stream = ObservedStream {
        events: VecDeque::from([
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "m".into(), SafeMetadata::default()).unwrap(),
            )),
            Err(provider_error()),
        ]),
        drops: drops.clone(),
    };
    assert!(error_stream.next().await.is_some());
    assert!(error_stream.next().await.unwrap().is_err());
    assert!(
        error_stream.next().await.is_none(),
        "terminal error must be the last event"
    );
    let mut after_error = vec![Ok(ChatStreamEvent::ResponseStarted(
        ResponseStarted::new(None, "m".into(), SafeMetadata::default()).unwrap(),
    ))];
    after_error.push(Err(provider_error()));
    after_error.push(Ok(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
        choice_index: 0,
        content_delta: Some("forbidden".into()),
        refusal_delta: None,
        annotations: vec![],
    })));
    assert_eq!(
        validate_chat_stream(&after_error, true),
        Err(StreamLifecycleError::EventAfterTerminal)
    );

    let decreasing_usage = ValidatingChatEventStream::new(ObservedStream {
        events: VecDeque::from([
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "m".into(), SafeMetadata::default()).unwrap(),
            )),
            Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
                Usage::partial(Some(TokenCounter::exact(5)), None, None, None, None),
            ))),
            Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
                Usage::partial(Some(TokenCounter::exact(3)), None, None, None, None),
            ))),
        ]),
        drops: drops.clone(),
    });
    let mut decreasing_usage = Box::pin(decreasing_usage);
    assert!(decreasing_usage.next().await.is_some());
    assert!(decreasing_usage.next().await.is_some());
    assert!(decreasing_usage.next().await.unwrap().is_err());
    assert!(decreasing_usage.next().await.is_none());
}

#[tokio::test]
async fn initial_provider_error_matches_public_stream_validation() {
    let initial_error = vec![Err(provider_error())];
    assert!(validate_chat_stream(&initial_error, true).is_ok());

    let drops = Arc::new(AtomicUsize::new(0));
    let mut runtime = ValidatingChatEventStream::new(ObservedStream {
        events: VecDeque::from([Err(provider_error())]),
        drops: drops.clone(),
    });
    assert!(runtime.next().await.is_some_and(|event| event.is_err()));
    assert!(runtime.next().await.is_none());

    let after_error = vec![
        Err(provider_error()),
        Ok(ChatStreamEvent::ResponseStarted(
            ResponseStarted::new(None, "m".into(), SafeMetadata::default()).unwrap(),
        )),
    ];
    assert_eq!(
        validate_chat_stream(&after_error, true),
        Err(StreamLifecycleError::EventAfterTerminal)
    );

    let mut runtime_with_following_event = ValidatingChatEventStream::new(ObservedStream {
        events: VecDeque::from([
            Err(provider_error()),
            Ok(ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "m".into(), SafeMetadata::default()).unwrap(),
            )),
        ]),
        drops,
    });
    assert!(
        runtime_with_following_event
            .next()
            .await
            .is_some_and(|event| event.is_err())
    );
    assert!(runtime_with_following_event.next().await.is_none());
}

#[test]
fn public_stream_validator_rejects_provider_lifecycle_and_usage_violations() {
    let started = || {
        Ok::<ChatStreamEvent, ProviderError>(ChatStreamEvent::ResponseStarted(
            ResponseStarted::new(None, "m".into(), SafeMetadata::default()).unwrap(),
        ))
    };
    let completion = || {
        Ok::<ChatStreamEvent, ProviderError>(ChatStreamEvent::ResponseCompleted(
            ResponseCompletion::new(None, "m".into(), None).unwrap(),
        ))
    };
    let finished = || {
        Ok::<ChatStreamEvent, ProviderError>(ChatStreamEvent::ChoiceFinished(
            ChoiceFinished::new(0, FinishReason::new("stop")).unwrap(),
        ))
    };
    let delta = || {
        Ok::<ChatStreamEvent, ProviderError>(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
            choice_index: 0,
            content_delta: Some("late".into()),
            refusal_delta: None,
            annotations: vec![],
        }))
    };

    let repeated_start = vec![started(), started(), completion()];
    assert_eq!(
        validate_chat_stream(&repeated_start, true),
        Err(StreamLifecycleError::DuplicateResponseStarted)
    );

    let delta_after_finish = vec![started(), finished(), delta()];
    assert_eq!(
        validate_chat_stream(&delta_after_finish, true),
        Err(StreamLifecycleError::ChoiceEventAfterFinished)
    );

    let completion_before_finish = vec![started(), delta(), completion()];
    assert_eq!(
        validate_chat_stream(&completion_before_finish, true),
        Err(StreamLifecycleError::MissingChoiceFinished)
    );

    let final_usage = Usage::complete(
        TokenCounter::exact(1),
        TokenCounter::exact(1),
        None,
        None,
        None,
    )
    .unwrap();
    let standalone_final_usage = vec![
        started(),
        Ok(ChatStreamEvent::Usage(
            UsageSnapshot::final_(final_usage).unwrap(),
        )),
    ];
    assert_eq!(
        validate_chat_stream(&standalone_final_usage, true),
        Err(StreamLifecycleError::StandaloneFinalUsage)
    );

    let terminal_then_event = vec![started(), completion(), delta()];
    assert_eq!(
        validate_chat_stream(&terminal_then_event, true),
        Err(StreamLifecycleError::EventAfterTerminal)
    );

    let concrete_usage = Usage::partial(
        Some(TokenCounter::exact(1)),
        Some(TokenCounter::exact(1)),
        Some(TokenCounter::exact(2)),
        Some(TokenCounter::exact(3)),
        Some(TokenCounter::exact(4)),
    );
    let dropped_cached_usage = Usage::partial(
        Some(TokenCounter::exact(1)),
        Some(TokenCounter::exact(2)),
        None,
        Some(TokenCounter::exact(3)),
        Some(TokenCounter::exact(4)),
    );
    let decreasing_usage = vec![
        started(),
        Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            concrete_usage,
        ))),
        Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            dropped_cached_usage,
        ))),
    ];
    assert_eq!(
        validate_chat_stream(&decreasing_usage, true),
        Err(StreamLifecycleError::NonCumulativeUsage)
    );

    let dropped_reasoning_usage = Usage::partial(
        Some(TokenCounter::exact(1)),
        Some(TokenCounter::exact(2)),
        Some(TokenCounter::exact(2)),
        Some(TokenCounter::unknown()),
        Some(TokenCounter::exact(4)),
    );
    let reasoning_drop = vec![
        started(),
        Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            concrete_usage,
        ))),
        Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            dropped_reasoning_usage,
        ))),
    ];
    assert_eq!(
        validate_chat_stream(&reasoning_drop, true),
        Err(StreamLifecycleError::NonCumulativeUsage)
    );

    let decreased_other_usage = Usage::partial(
        Some(TokenCounter::exact(1)),
        Some(TokenCounter::exact(2)),
        Some(TokenCounter::exact(2)),
        Some(TokenCounter::exact(3)),
        Some(TokenCounter::exact(3)),
    );
    let other_decrease = vec![
        started(),
        Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            concrete_usage,
        ))),
        Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(
            decreased_other_usage,
        ))),
    ];
    assert_eq!(
        validate_chat_stream(&other_decrease, true),
        Err(StreamLifecycleError::NonCumulativeUsage)
    );
}

#[test]
fn provider_error_debug_and_serialization_redact_raw_material() {
    let error = provider_error();
    let debug = format!("{error:?}");
    let json = serde_json::to_string(&error).unwrap();
    assert!(!debug.contains("secret"));
    assert!(!json.contains("authorization"));
    assert_eq!(TokenCounter::unknown().value(), None);
    assert_eq!(
        error.partial_usage().unwrap().completeness(),
        UsageCompleteness::Partial
    );
}

#[test]
fn admin_audit_is_scalar_safe_and_secret_free() {
    let details = AuditDetails::ApiKeyCreated {
        key_id: gateway::domain::identity::ApiKeyId::new(),
    };
    let draft = AdminAuditDraft::new(
        OrganizationId::new(),
        AuditActor::System,
        AuditTarget::ApiKey(gateway::domain::identity::ApiKeyId::new()),
        AuditAction::new("api-key.created").unwrap(),
        AuditOutcome::Succeeded,
        CorrelationId::new(),
        Some(TeamId::new()),
        details,
    );
    let json = serde_json::to_string(&draft).unwrap();
    assert!(!json.contains("Bearer"));
    assert!(AuditAction::new("Bearer secret").is_err());
    assert!(serde_json::from_str::<AuditAction>(r#""Not An Action""#).is_err());
    assert!(serde_json::from_str::<AuditDetails>(r#"{"arbitrary":"secret"}"#).is_err());
    assert!(!format!("{:?}", SafeMetadata::default()).contains("prompt"));
}

#[test]
fn opaque_structured_metadata_round_trips_without_debug_or_audit_leakage() {
    let metadata = serde_json::json!({
        "future_object": {
            "nested": [true, 7, {"unknown": "provider-value"}]
        },
        "future_number": 12.5,
        "future_boolean": false
    });
    let response = serde_json::json!({
        "provider_request_id": "provider-request",
        "model": "model",
        "choices": [],
        "usage": null,
        "metadata": metadata.clone()
    });
    assert!(serde_json::from_value::<ChatResponse>(response).is_err());
    assert!(
        !serde_json::to_string(&AdminAuditDraft::new(
            OrganizationId::new(),
            AuditActor::System,
            AuditTarget::Organization(OrganizationId::new()),
            AuditAction::new("metadata.observed").unwrap(),
            AuditOutcome::Succeeded,
            CorrelationId::new(),
            None,
            AuditDetails::None,
        ))
        .unwrap()
        .contains("future_object")
    );
}

#[test]
fn job_envelope_round_trips_typed_payload_without_credentials() {
    let envelope = JobEnvelope::new(
        JobPayload::CatalogRefresh {
            credential: Some(CredentialReference {
                id: CredentialId::new(),
            }),
        },
        PayloadSchemaVersion::new(1).unwrap(),
        IdempotencyKey::new("job-attempt-1").unwrap(),
        CorrelationId::new(),
    );
    let id = envelope.id();
    let json = serde_json::to_string(&envelope).unwrap();
    let round_trip: JobEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(round_trip.id(), id);
    assert_eq!(round_trip.payload(), envelope.payload());
    assert_eq!(
        round_trip.credential_references(),
        envelope.credential_references()
    );
    assert!(!json.contains("bearer"));
    assert_eq!(round_trip.kind().as_str(), "catalog.refresh");
    assert_eq!(round_trip.organization_id(), None);
    assert_eq!(round_trip.credential_references().len(), 1);
    assert!(!json.contains("kind"));
    assert!(!json.contains("organization_id"));
    assert!(!json.contains("credential_references"));
    let mut invalid = serde_json::to_value(&envelope).unwrap();
    invalid["payload_schema_version"] = serde_json::json!(0);
    assert!(serde_json::from_value::<JobEnvelope>(invalid).is_err());
    let invalid_payload = serde_json::json!({
        "id": envelope.id(), "payload_schema_version": 1, "idempotency_key": "x",
        "payload": {"InferenceDispatch": {}}, "correlation_id": envelope.correlation_id()
    });
    assert!(serde_json::from_value::<JobEnvelope>(invalid_payload).is_err());
}

#[test]
fn deserialization_rejects_malformed_indices_usage_and_lifecycle() {
    let choices = serde_json::json!([
        {"index": 0, "message": {"role": "Assistant", "content": "a"}, "finish_reason": null, "tool_calls": [], "refusal": null, "safety": [], "annotations": []},
        {"index": 0, "message": {"role": "Assistant", "content": "b"}, "finish_reason": null, "tool_calls": [], "refusal": null, "safety": [], "annotations": []}
    ]);
    let response = serde_json::json!({"provider_request_id": null, "model": "m", "choices": choices, "usage": null, "metadata": {}});
    assert!(serde_json::from_value::<ChatResponse>(response).is_err());

    let embeddings = serde_json::json!({"provider_request_id": null, "model": "m", "data": [{"index": 1, "vector": [1.0]}, {"index": 0, "vector": [2.0]}], "usage": null});
    assert!(serde_json::from_value::<EmbeddingResponse>(embeddings).is_err());

    let malformed_counter = serde_json::json!({"value": null, "quality": "Exact"});
    assert!(serde_json::from_value::<TokenCounter>(malformed_counter).is_err());
    let malformed_snapshot = serde_json::json!({"usage": serde_json::to_value(Usage::partial(Some(TokenCounter::exact(1)), None, None, None, None)).unwrap(), "finality": "Final"});
    assert!(serde_json::from_value::<UsageSnapshot>(malformed_snapshot).is_err());
    let malformed_error = serde_json::json!({"category": "Unavailable", "provider": null, "provider_request_id": null, "http_status": null, "provider_code": null, "retry_after": null, "partial_usage": serde_json::to_value(Usage::unavailable()).unwrap()});
    assert!(serde_json::from_value::<ProviderError>(malformed_error).is_err());
    let malformed_context = serde_json::json!({"correlation_id": CorrelationId::new(), "provider_attempt_idempotency_key": " ", "deployment": {"deployment": "d", "model": "m"}});
    assert!(serde_json::from_value::<InferenceContext>(malformed_context).is_err());
    let malformed_reference = serde_json::json!({"deployment": "", "model": "m"});
    assert!(serde_json::from_value::<DeploymentModelReference>(malformed_reference).is_err());
}
