use axum::{
    Json,
    body::Body,
    extract::{Json as JsonExtractor, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_TYPE, RETRY_AFTER},
    },
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};
use uuid::Uuid;

use crate::{
    AppState,
    application::inference::{AccountedEventStream, InferenceFacadeError},
    domain::{
        identity::{ApiKeyScope, VerifiedIdentity},
        inference::{ChatRequest, ChatStreamEvent, CorrelationId, EmbeddingRequest, ProviderError},
        rate_limit::RateLimitKind,
    },
};

pub(crate) const IDEMPOTENCY_HEADER: &str = "idempotency-key";
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";
pub(crate) const CHAT_PATH: &str = "/api/inference/v1/chat/completions";
pub(crate) const STREAM_PATH: &str = "/api/inference/v1/chat/completions/stream";
pub(crate) const EMBEDDINGS_PATH: &str = "/api/inference/v1/embeddings";
use crate::domain::public_request::RequestBoundError;
pub use crate::domain::public_request::{ChatBody, EmbeddingBody, PublicMessage};
use crate::domain::public_request::{
    ChatBodyResponse, choice_response, embedding_response, usage_response,
};
#[cfg(test)]
use crate::domain::public_request::{MAX_MODEL_ALIAS_CHARS, MAX_TEXT_BYTES};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::FinishReason;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn inference_routes_require_gw_bearer_keys() {
        let response = crate::transport::http::router(AppState::for_test())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/inference/v1/chat/completions")
                    .body(Body::from(r#"{"model":"chat-default","messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "authentication_required");
        assert!(body.get("request_id").is_some());
    }

    #[test]
    fn sse_events_are_named_and_non_resumable() {
        let event = sse_event(
            Ok(ChatStreamEvent::ChoiceFinished(
                crate::domain::inference::ChoiceFinished::new(0, FinishReason::new("stop"))
                    .unwrap(),
            )),
            "chat-default",
        );
        assert!(event.starts_with("event: choice.finished\ndata: "));
        assert!(!event.contains("Last-Event-ID"));
    }

    #[test]
    fn public_dtos_are_strict_and_bounded() {
        assert!(
            serde_json::from_str::<ChatBody>(
                r#"{"model":"chat-default","messages":[],"provider":"openai"}"#
            )
            .is_err()
        );
        let body = |model: &str, max_tokens: &str| {
            serde_json::from_str::<ChatBody>(&format!(
                r#"{{"model":"{model}","messages":[{{"role":"user","content":"hello"}}]{max_tokens}}}"#
            ))
            .unwrap()
        };
        let chat_request = crate::domain::public_request::chat_request;
        assert!(chat_request(body("chat-default", r#","max_tokens":4097"#)).is_err());
        assert_eq!(
            chat_request(body("catalog-alias", "")).unwrap().0,
            "catalog-alias"
        );
        assert_eq!(
            chat_request(body(&"a".repeat(MAX_MODEL_ALIAS_CHARS + 1), "")).map(|_| ()),
            Err(RequestBoundError::ChatRequestOutOfBounds)
        );
        assert!(
            crate::application::inference::PublicModelCatalog::for_tests(
                crate::application::AccountingPolicy::for_tests(),
            )
            .resolve(
                "chat-default",
                crate::application::inference::PublicOperation::Embeddings,
            )
            .is_none()
        );
    }

    #[test]
    fn usage_quality_and_completeness_use_the_contract_values() {
        use crate::domain::inference::{TokenCounter, Usage};
        let spec: Value =
            serde_json::from_str(include_str!("../../openapi/inference-v1.json")).unwrap();
        let usage = &spec["components"]["schemas"]["Usage"]["properties"];
        let quality = usage["quality"]["enum"].as_array().unwrap();
        let completeness = usage["completeness"]["enum"].as_array().unwrap();
        let samples = [
            Usage::complete(
                TokenCounter::exact(1),
                TokenCounter::exact(2),
                None,
                None,
                None,
            )
            .unwrap(),
            Usage::complete(
                TokenCounter::estimated(1),
                TokenCounter::exact(2),
                None,
                None,
                None,
            )
            .unwrap(),
            Usage::partial(Some(TokenCounter::exact(1)), None, None, None, None),
            Usage::unavailable(),
        ];
        let mut seen_quality = std::collections::BTreeSet::new();
        let mut seen_completeness = std::collections::BTreeSet::new();
        for sample in samples {
            let body = serde_json::to_value(usage_response(sample)).unwrap();
            assert!(quality.contains(&body["quality"]), "{body}");
            assert!(completeness.contains(&body["completeness"]), "{body}");
            seen_quality.insert(body["quality"].as_str().unwrap().to_owned());
            seen_completeness.insert(body["completeness"].as_str().unwrap().to_owned());
        }
        assert_eq!(seen_quality.len(), 3, "{seen_quality:?}");
        assert_eq!(seen_completeness.len(), 3, "{seen_completeness:?}");
    }

    #[test]
    fn client_conformance_taxonomy_lists_every_problem_code() {
        // Exhaustive without a wildcard: a new ProblemCode fails to compile
        // here until it is added, and so to clients/conformance/errors.json.
        fn index(code: ProblemCode) -> usize {
            match code {
                ProblemCode::AuthenticationRequired => 0,
                ProblemCode::InvalidApiKey => 1,
                ProblemCode::InsufficientScope => 2,
                ProblemCode::StreamResumeUnsupported => 3,
                ProblemCode::IdempotencyKeyRequired => 4,
                ProblemCode::IdempotencyKeyInvalid => 5,
                ProblemCode::RequestTooLarge => 6,
                ProblemCode::UnsupportedMediaType => 7,
                ProblemCode::RequestBodyUnprocessable => 8,
                ProblemCode::InvalidRequestBody => 9,
                ProblemCode::ModelUnavailable => 10,
                ProblemCode::IdempotencyInProgress => 11,
                ProblemCode::IdempotencyConflict => 12,
                ProblemCode::IdempotencyTerminal => 13,
                ProblemCode::QuotaExceeded => 14,
                ProblemCode::RateLimitExceeded => 15,
                ProblemCode::ConcurrencyLimitExceeded => 16,
                ProblemCode::AccountingBoundExceeded => 17,
                ProblemCode::ProviderFailure => 18,
                ProblemCode::ProviderRateLimited => 19,
                ProblemCode::ProviderUnavailable => 20,
                ProblemCode::ProviderCircuitOpen => 21,
                ProblemCode::ProviderTimeout => 22,
                ProblemCode::ProviderInvalidRequest => 23,
                ProblemCode::InferenceUnavailable => 24,
                ProblemCode::AuthenticationUnavailable => 25,
                ProblemCode::ChatRequestOutOfBounds => 26,
                ProblemCode::ChatRoleInvalid => 27,
                ProblemCode::ChatInputBoundExceeded => 28,
                ProblemCode::TemperatureInvalid => 29,
                ProblemCode::MaxTokensInvalid => 30,
                ProblemCode::EmbeddingRequestOutOfBounds => 31,
                ProblemCode::EmbeddingInputBoundExceeded => 32,
                ProblemCode::FileNotFound => 33,
                ProblemCode::FilePurposeInvalid => 34,
                ProblemCode::BatchNotFound => 35,
                ProblemCode::BatchRequestInvalid => 36,
                ProblemCode::BatchValidationFailed => 37,
                ProblemCode::BatchStoreUnavailable => 38,
                ProblemCode::EndUserRequired => 39,
                ProblemCode::EndUserInvalid => 40,
                ProblemCode::EndUserNotRegistered => 41,
                ProblemCode::RoutingPolicyDenied => 42,
                ProblemCode::ChatToolsInvalid => 43,
                ProblemCode::ToolsUnsupported => 44,
            }
        }
        let all = [
            ProblemCode::AuthenticationRequired,
            ProblemCode::InvalidApiKey,
            ProblemCode::InsufficientScope,
            ProblemCode::StreamResumeUnsupported,
            ProblemCode::IdempotencyKeyRequired,
            ProblemCode::IdempotencyKeyInvalid,
            ProblemCode::RequestTooLarge,
            ProblemCode::UnsupportedMediaType,
            ProblemCode::RequestBodyUnprocessable,
            ProblemCode::InvalidRequestBody,
            ProblemCode::ModelUnavailable,
            ProblemCode::IdempotencyInProgress,
            ProblemCode::IdempotencyConflict,
            ProblemCode::IdempotencyTerminal,
            ProblemCode::QuotaExceeded,
            ProblemCode::RateLimitExceeded,
            ProblemCode::ConcurrencyLimitExceeded,
            ProblemCode::AccountingBoundExceeded,
            ProblemCode::ProviderFailure,
            ProblemCode::ProviderRateLimited,
            ProblemCode::ProviderUnavailable,
            ProblemCode::ProviderCircuitOpen,
            ProblemCode::ProviderTimeout,
            ProblemCode::ProviderInvalidRequest,
            ProblemCode::InferenceUnavailable,
            ProblemCode::AuthenticationUnavailable,
            ProblemCode::ChatRequestOutOfBounds,
            ProblemCode::ChatRoleInvalid,
            ProblemCode::ChatInputBoundExceeded,
            ProblemCode::TemperatureInvalid,
            ProblemCode::MaxTokensInvalid,
            ProblemCode::EmbeddingRequestOutOfBounds,
            ProblemCode::EmbeddingInputBoundExceeded,
            ProblemCode::FileNotFound,
            ProblemCode::FilePurposeInvalid,
            ProblemCode::BatchNotFound,
            ProblemCode::BatchRequestInvalid,
            ProblemCode::BatchValidationFailed,
            ProblemCode::BatchStoreUnavailable,
            ProblemCode::EndUserRequired,
            ProblemCode::EndUserInvalid,
            ProblemCode::EndUserNotRegistered,
            ProblemCode::RoutingPolicyDenied,
            ProblemCode::ChatToolsInvalid,
            ProblemCode::ToolsUnsupported,
        ];
        for (position, code) in all.iter().enumerate() {
            assert_eq!(index(*code), position);
        }
        let taxonomy: Value =
            serde_json::from_str(include_str!("../../clients/conformance/errors.json")).unwrap();
        let listed: std::collections::BTreeSet<&str> = taxonomy["codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["code"].as_str().unwrap())
            .collect();
        let served: std::collections::BTreeSet<&str> = all.iter().map(|code| code.code()).collect();
        assert_eq!(
            listed, served,
            "clients/conformance/errors.json is out of step"
        );
    }

    #[test]
    fn checked_sse_fixture_contains_only_named_events() {
        let fixture = include_str!("../../openapi/fixtures/sse/chat-completed.sse");
        assert!(!fixture.contains("id:"));
        for line in fixture.lines().filter(|line| line.starts_with("data: ")) {
            serde_json::from_str::<Value>(line.trim_start_matches("data: ")).unwrap();
        }
    }

    #[test]
    fn checked_openapi_paths_match_transport_routes() {
        let spec: Value =
            serde_json::from_str(include_str!("../../openapi/inference-v1.json")).unwrap();
        let paths = spec["paths"].as_object().unwrap();
        for request in ["ChatRequest", "EmbeddingRequest"] {
            let model = &spec["components"]["schemas"][request]["properties"]["model"];
            assert_eq!(model["type"], "string");
            assert_eq!(model["minLength"], 1);
            assert_eq!(model["maxLength"], MAX_MODEL_ALIAS_CHARS);
            assert!(model.get("enum").is_none());
        }
        let expected = [CHAT_PATH, STREAM_PATH, EMBEDDINGS_PATH];
        let batches = [
            crate::transport::batches::FILES_PATH,
            crate::transport::batches::FILE_PATH,
            crate::transport::batches::FILE_CONTENT_PATH,
            crate::transport::batches::BATCHES_PATH,
            crate::transport::batches::BATCH_PATH,
            crate::transport::batches::BATCH_CANCEL_PATH,
        ];
        assert_eq!(paths.len(), expected.len() + batches.len());
        for path in batches {
            assert!(paths.contains_key(path), "missing OpenAPI path {path}");
        }
        for path in expected {
            assert!(paths.contains_key(path), "missing OpenAPI path {path}");
            assert_eq!(
                paths[path]["post"]["security"][0]["gatewayApiKey"],
                Value::Array(vec![])
            );
            assert_eq!(
                paths[path]["post"]["responses"]["404"]["$ref"],
                "#/components/responses/Problem404"
            );
        }
        for event in paths[STREAM_PATH]["post"]["x-sse-events"]
            .as_array()
            .unwrap()
        {
            let reference = event["schema"]["$ref"].as_str().unwrap();
            let schema_name = reference.rsplit('/').next().unwrap();
            assert!(spec["components"]["schemas"].get(schema_name).is_some());
        }
    }

    #[tokio::test]
    async fn unknown_catalog_alias_is_a_not_found_problem() {
        let response =
            accounting_error(InferenceFacadeError::UnknownModel, CHAT_PATH, "request-id");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "model_unavailable");
        assert_eq!(body["status"], 404);
        assert_eq!(body["request_id"], "request-id");
    }

    #[tokio::test]
    async fn open_circuit_is_service_unavailable_with_retry_after() {
        let response = accounting_error(
            InferenceFacadeError::CircuitOpen {
                retry_after_seconds: 30,
            },
            CHAT_PATH,
            "request-id",
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "30");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "provider_circuit_open");
    }

    #[tokio::test]
    async fn limit_breaches_are_too_many_requests_with_retry_after() {
        for (kind, scope, seconds, code, detail) in [
            (
                RateLimitKind::RequestsPerMinute,
                "organization",
                17,
                "rate_limit_exceeded",
                "the organization's requests_per_minute limit is reached",
            ),
            (
                RateLimitKind::Concurrency,
                "team",
                1,
                "concurrency_limit_exceeded",
                "the team's max_concurrent limit is reached",
            ),
            (
                RateLimitKind::TokensPerMinute,
                "api-key",
                23,
                "rate_limit_exceeded",
                "the API key's tokens_per_minute limit is reached",
            ),
        ] {
            let breach = crate::persistence::rate_limits::RateLimitBreach {
                kind,
                scope,
                retry_after_seconds: seconds,
                never_fits: false,
            };
            let response = rate_limited(breach, CHAT_PATH, "request-id");
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(
                response.headers().get(RETRY_AFTER).unwrap(),
                &seconds.to_string()
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["code"], code);
            assert_eq!(body["status"], 429);
            assert_eq!(body["scope"], scope);
            assert_eq!(body["limit"], kind.as_str());
            assert_eq!(body["detail"], detail);
            assert!(body.get("end_user").is_none());
        }
        let never = crate::persistence::rate_limits::RateLimitBreach {
            kind: RateLimitKind::TokensPerMinute,
            scope: "service-account",
            retry_after_seconds: 60,
            never_fits: true,
        };
        let response = rate_limited(never, CHAT_PATH, "request-id");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["detail"],
            "the fewest tokens the request can be served with (its input, any reasoning allowance and, for chat, one output token) are more than the service account's tokens_per_minute limit"
        );
    }

    #[tokio::test]
    async fn an_end_users_rate_limit_names_them_and_the_limit_with_retry_after() {
        let refusal = crate::domain::end_user::EndUserRateRefusal {
            end_user: "user-42".into(),
            limit: crate::domain::end_user::EndUserRateLimitKind::TokensPerMinute,
            retry_after_seconds: 23,
            never_fits: false,
        };
        let response = end_user_rate_limited(&refusal, CHAT_PATH, "request-id");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "23");
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "rate_limit_exceeded");
        assert_eq!(body["end_user"], "user-42");
        assert_eq!(body["limit"], "tokens_per_minute");
        assert_eq!(body["status"], 429);
        // Their plan's concurrency is a concurrency limit, as a policy's.
        let refusal = crate::domain::end_user::EndUserRateRefusal {
            limit: crate::domain::end_user::EndUserRateLimitKind::Concurrency,
            retry_after_seconds: 1,
            ..refusal
        };
        let response = end_user_rate_limited(&refusal, CHAT_PATH, "request-id");
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "1");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "concurrency_limit_exceeded");
        assert_eq!(body["limit"], "max_concurrent");
    }

    #[tokio::test]
    async fn catalog_store_failure_is_a_retryable_service_unavailable_problem() {
        let response = accounting_error(
            InferenceFacadeError::Catalog(crate::persistence::catalog::CatalogStoreError::Graph),
            CHAT_PATH,
            "request-id",
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "inference_unavailable");
        assert_eq!(body["status"], 503);
        assert_eq!(body["request_id"], "request-id");
    }

    #[test]
    fn canonical_problem_codes_cover_scope_and_embedding_bounds() {
        assert_eq!(ProblemCode::InsufficientScope.code(), "insufficient_scope");
        assert_eq!(
            ProblemCode::InsufficientScope.detail(),
            "API key does not grant capability"
        );
        let oversized = EmbeddingBody {
            model: "catalog-alias".into(),
            input: vec!["x".repeat(MAX_TEXT_BYTES + 1)],
            user: None,
        };
        assert_eq!(
            embedding_request(oversized),
            Err(ProblemCode::EmbeddingInputBoundExceeded)
        );
        assert_eq!(
            ProblemCode::EmbeddingInputBoundExceeded.code(),
            "embedding_input_bound_exceeded"
        );
    }

    #[tokio::test]
    async fn accounting_input_bound_exceeded_is_an_unprocessable_problem() {
        let response = accounting_error(
            InferenceFacadeError::Accounting(
                crate::application::AccountingServiceError::InputBoundExceeded,
            ),
            CHAT_PATH,
            "request-id",
        );
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "accounting_bound_exceeded");
        assert_eq!(body["status"], 422);
        assert_eq!(body["request_id"], "request-id");
    }

    #[tokio::test]
    async fn empty_embedding_strings_are_rejected_before_accounting() {
        let rejected = EmbeddingBody {
            model: "catalog-alias".into(),
            input: vec!["valid".into(), String::new()],
            user: None,
        };
        assert_eq!(
            embedding_request(rejected),
            Err(ProblemCode::EmbeddingRequestOutOfBounds)
        );

        let response = problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::EmbeddingRequestOutOfBounds,
            EMBEDDINGS_PATH,
            "request-id",
        );
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "embedding_request_out_of_bounds");
        assert_eq!(body["status"], 422);

        let whitespace = EmbeddingBody {
            model: "catalog-alias".into(),
            input: vec!["   ".into()],
            user: None,
        };
        assert!(embedding_request(whitespace).is_ok());
    }
}

#[derive(Debug, Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    kind: String,
    code: &'static str,
    title: &'static str,
    status: u16,
    detail: &'static str,
    instance: String,
    request_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProblemCode {
    AuthenticationRequired,
    InvalidApiKey,
    InsufficientScope,
    StreamResumeUnsupported,
    IdempotencyKeyRequired,
    IdempotencyKeyInvalid,
    RequestTooLarge,
    UnsupportedMediaType,
    RequestBodyUnprocessable,
    InvalidRequestBody,
    ModelUnavailable,
    IdempotencyInProgress,
    IdempotencyConflict,
    IdempotencyTerminal,
    QuotaExceeded,
    RateLimitExceeded,
    ConcurrencyLimitExceeded,
    AccountingBoundExceeded,
    ProviderFailure,
    ProviderRateLimited,
    ProviderUnavailable,
    ProviderCircuitOpen,
    ProviderTimeout,
    ProviderInvalidRequest,
    InferenceUnavailable,
    AuthenticationUnavailable,
    ChatRequestOutOfBounds,
    ChatRoleInvalid,
    ChatInputBoundExceeded,
    TemperatureInvalid,
    MaxTokensInvalid,
    EmbeddingRequestOutOfBounds,
    EmbeddingInputBoundExceeded,
    FileNotFound,
    FilePurposeInvalid,
    BatchNotFound,
    BatchRequestInvalid,
    BatchValidationFailed,
    BatchStoreUnavailable,
    /// The service account requires `Gateway-End-User` (D-028).
    EndUserRequired,
    /// `Gateway-End-User` (or `user`) is not a valid end-user ID.
    EndUserInvalid,
    /// The end user has no plan and the service account has no default.
    EndUserNotRegistered,
    /// The organization's routing policy permits none of the alias's
    /// deployments (D-023).
    RoutingPolicyDenied,
    /// Tools, tool choice, tool calls or results that break D-032's rules.
    ChatToolsInvalid,
    /// The alias's route does not take tools, or its provider cannot honour
    /// a tool option the request asks for (D-032).
    ToolsUnsupported,
}

impl ProblemCode {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::AuthenticationRequired => "authentication_required",
            Self::InvalidApiKey => "invalid_api_key",
            Self::InsufficientScope => "insufficient_scope",
            Self::StreamResumeUnsupported => "stream_resume_unsupported",
            Self::IdempotencyKeyRequired => "idempotency_key_required",
            Self::IdempotencyKeyInvalid => "idempotency_key_invalid",
            Self::RequestTooLarge => "request_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::RequestBodyUnprocessable => "request_body_unprocessable",
            Self::InvalidRequestBody => "invalid_request_body",
            Self::ModelUnavailable => "model_unavailable",
            Self::IdempotencyInProgress => "idempotency_in_progress",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::IdempotencyTerminal => "idempotency_terminal",
            Self::QuotaExceeded => "quota_exceeded",
            Self::RateLimitExceeded => "rate_limit_exceeded",
            Self::ConcurrencyLimitExceeded => "concurrency_limit_exceeded",
            Self::AccountingBoundExceeded => "accounting_bound_exceeded",
            Self::ProviderFailure => "provider_failure",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderCircuitOpen => "provider_circuit_open",
            Self::ProviderTimeout => "provider_timeout",
            Self::ProviderInvalidRequest => "provider_invalid_request",
            Self::InferenceUnavailable => "inference_unavailable",
            Self::AuthenticationUnavailable => "authentication_unavailable",
            Self::ChatRequestOutOfBounds => "chat_request_out_of_bounds",
            Self::ChatRoleInvalid => "chat_role_invalid",
            Self::ChatInputBoundExceeded => "chat_input_bound_exceeded",
            Self::TemperatureInvalid => "temperature_invalid",
            Self::MaxTokensInvalid => "max_tokens_invalid",
            Self::EmbeddingRequestOutOfBounds => "embedding_request_out_of_bounds",
            Self::EmbeddingInputBoundExceeded => "embedding_input_bound_exceeded",
            Self::FileNotFound => "file_not_found",
            Self::FilePurposeInvalid => "file_purpose_invalid",
            Self::BatchNotFound => "batch_not_found",
            Self::BatchRequestInvalid => "batch_request_invalid",
            Self::BatchValidationFailed => "batch_validation_failed",
            Self::BatchStoreUnavailable => "batch_store_unavailable",
            Self::EndUserRequired => "end_user_required",
            Self::EndUserInvalid => "end_user_invalid",
            Self::EndUserNotRegistered => "end_user_not_registered",
            Self::RoutingPolicyDenied => "routing_policy_denied",
            Self::ChatToolsInvalid => "chat_tools_invalid",
            Self::ToolsUnsupported => "tools_unsupported",
        }
    }

    pub(crate) const fn detail(self) -> &'static str {
        match self {
            Self::AuthenticationRequired => "bearer authentication is required",
            Self::InvalidApiKey => "invalid API key",
            Self::InsufficientScope => "API key does not grant capability",
            Self::StreamResumeUnsupported => "stream resumption is not supported",
            Self::IdempotencyKeyRequired => "Idempotency-Key header is required",
            Self::IdempotencyKeyInvalid => "Idempotency-Key header is invalid",
            Self::RequestTooLarge
            | Self::UnsupportedMediaType
            | Self::RequestBodyUnprocessable
            | Self::InvalidRequestBody => "request body invalid",
            Self::ModelUnavailable => "model alias is not available for this operation",
            Self::IdempotencyInProgress => "idempotent request is already in progress",
            Self::IdempotencyConflict => "Idempotency-Key conflicts with a different request",
            Self::IdempotencyTerminal => "idempotent request is already terminal",
            Self::QuotaExceeded => {
                "the budgets covering this request cannot cover its largest possible cost"
            }
            Self::RateLimitExceeded => "request rate limit exceeded",
            Self::ConcurrencyLimitExceeded => "concurrent request limit exceeded",
            Self::AccountingBoundExceeded => "provider usage exceeded the configured route bound",
            Self::ProviderFailure => "provider request failed",
            Self::ProviderRateLimited => "provider rate limit",
            Self::ProviderUnavailable => "provider unavailable",
            Self::ProviderCircuitOpen => "provider deployment is failing; retry later",
            Self::ProviderTimeout => "provider timeout",
            Self::ProviderInvalidRequest => "provider rejected request",
            Self::InferenceUnavailable => "inference service unavailable",
            Self::AuthenticationUnavailable => "authentication service unavailable",
            Self::ChatRequestOutOfBounds => "chat request is outside the supported bounds",
            Self::ChatRoleInvalid => "chat message role is invalid",
            Self::ChatInputBoundExceeded => "chat request exceeds the input bound",
            Self::TemperatureInvalid => "temperature is invalid",
            Self::MaxTokensInvalid => "max_tokens is invalid",
            Self::EmbeddingRequestOutOfBounds => {
                "embedding request is outside the supported bounds"
            }
            Self::EmbeddingInputBoundExceeded => "embedding request exceeds input bound",
            Self::FileNotFound => "file not found",
            Self::FilePurposeInvalid => "file purpose must be batch",
            Self::BatchNotFound => "batch not found",
            Self::BatchRequestInvalid => "endpoint, completion_window or metadata is invalid",
            Self::BatchValidationFailed => "the input file failed validation; see errors",
            Self::BatchStoreUnavailable => "the batch file store is not configured",
            Self::EndUserRequired => {
                "this service account requires the Gateway-End-User header naming the app's user"
            }
            Self::EndUserInvalid => {
                "the end-user ID must be 1 to 256 bytes of visible UTF-8, with no surrounding whitespace"
            }
            Self::EndUserNotRegistered => {
                "the end user has no plan and this service account has no default plan"
            }
            Self::RoutingPolicyDenied => {
                "the organization's routing policy permits none of this model alias's deployments"
            }
            Self::ChatToolsInvalid => "the request's tools, tool calls or tool results are invalid",
            Self::ToolsUnsupported => {
                "this model alias does not take tools: not every deployment it routes to is set to take them"
            }
        }
    }
}

pub(crate) fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128 && !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

pub(crate) fn correlation(value: &str) -> CorrelationId {
    Uuid::parse_str(value)
        .map(CorrelationId)
        .unwrap_or_else(|_| CorrelationId::new())
}

/// The detail of `quota_exceeded` when the key's own budget is what refuses
/// the request: its service account's budgets still have room.
pub(crate) const API_KEY_BUDGET_DETAIL: &str = "this API key's budget cannot cover the request; an organization administrator raises it in the admin console (Policies, Budgets)";

/// The detail of `quota_exceeded` when no budget covers the request.
pub(crate) const NO_BUDGET_DETAIL: &str = "no budget covers this request; an organization administrator adds one in the admin console (Policies, Budgets)";

pub(crate) fn problem(status: StatusCode, problem: ProblemCode, path: &str, id: &str) -> Response {
    problem_with_detail(status, problem, problem.detail(), path, id)
}

/// A problem whose detail is more specific than its code's usual one.
pub(crate) fn problem_with_detail(
    status: StatusCode,
    problem: ProblemCode,
    detail: &'static str,
    path: &str,
    id: &str,
) -> Response {
    let code = problem.code();
    let body = ProblemBody {
        kind: format!("https://gateway.invalid/problems/{code}"),
        code,
        title: status.canonical_reason().unwrap_or("Request failed"),
        status: status.as_u16(),
        detail,
        instance: path.to_owned(),
        request_id: id.to_owned(),
    };
    let mut response = (status, Json(body)).into_response();
    // The outcome label of the inference request metrics.
    response
        .extensions_mut()
        .insert(crate::metrics::ProblemCode(code));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(id).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    response
}

/// 429 `quota_exceeded` on the end user's account (D-028): the detail names
/// the user, and `end_user`, `allowance_remaining`, `credit_balance` and
/// `currency` let the app offer an upgrade or a purchase.
pub(crate) fn end_user_quota_exceeded(
    refusal: &crate::domain::end_user::EndUserRefusal,
    path: &str,
    id: &str,
) -> Response {
    let status = StatusCode::TOO_MANY_REQUESTS;
    let code = ProblemCode::QuotaExceeded.code();
    let body = serde_json::json!({
        "type": format!("https://gateway.invalid/problems/{code}"),
        "code": code,
        "title": status.canonical_reason().unwrap_or("Request failed"),
        "status": status.as_u16(),
        "detail": format!(
            "end user {:?} has used this month's allowance and has no credits that cover this request",
            refusal.end_user
        ),
        "instance": path,
        "request_id": id,
        "end_user": refusal.end_user,
        "allowance_remaining": refusal.allowance_remaining,
        "credit_balance": refusal.credit_balance,
        "currency": refusal.currency,
    });
    let mut response = (status, Json(body)).into_response();
    response
        .extensions_mut()
        .insert(crate::metrics::ProblemCode(code));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(id).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    response
}

/// 429 `rate_limit_exceeded` with `Retry-After`, naming the end user and
/// which of their plan's limits refused the request (D-028), so the app can
/// tell it from the organization's limits, which name no end user.
pub(crate) fn end_user_rate_limited(
    refusal: &crate::domain::end_user::EndUserRateRefusal,
    path: &str,
    id: &str,
) -> Response {
    let status = StatusCode::TOO_MANY_REQUESTS;
    let code = match refusal.limit {
        crate::domain::end_user::EndUserRateLimitKind::Concurrency => {
            ProblemCode::ConcurrencyLimitExceeded.code()
        }
        _ => ProblemCode::RateLimitExceeded.code(),
    };
    let detail = if refusal.never_fits {
        format!(
            "the fewest tokens the request can be served with (its input, any reasoning allowance and, for chat, one output token) are more than end user {:?}'s {} limit",
            refusal.end_user,
            refusal.limit.as_str()
        )
    } else {
        format!(
            "end user {:?} has reached their plan's {} limit",
            refusal.end_user,
            refusal.limit.as_str()
        )
    };
    let body = serde_json::json!({
        "type": format!("https://gateway.invalid/problems/{code}"),
        "code": code,
        "title": status.canonical_reason().unwrap_or("Request failed"),
        "status": status.as_u16(),
        "detail": detail,
        "instance": path,
        "request_id": id,
        "end_user": refusal.end_user,
        "limit": refusal.limit.as_str(),
    });
    let mut response = (status, Json(body)).into_response();
    response
        .extensions_mut()
        .insert(crate::metrics::ProblemCode(code));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    headers.insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(id).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    headers.insert(RETRY_AFTER, HeaderValue::from(refusal.retry_after_seconds));
    response
}

/// The request's end user (D-028): `Gateway-End-User`, or else the body's
/// `user`; the header wins where both are sent.
pub(crate) fn end_user(
    headers: &HeaderMap,
    body_user: Option<&str>,
    path: &str,
    id: &str,
) -> Result<Option<String>, Response> {
    let invalid = || {
        problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::EndUserInvalid,
            path,
            id,
        )
    };
    let value = match headers.get(crate::domain::end_user::END_USER_HEADER) {
        Some(value) => Some(std::str::from_utf8(value.as_bytes()).map_err(|_| invalid())?),
        None => body_user,
    };
    value
        .map(|value| crate::domain::end_user::parse_end_user_id(value).map_err(|_| invalid()))
        .transpose()
}

/// 429 with `Retry-After`: the whole seconds until the gateway would admit
/// the request. `scope` names the level of the policy that refused it
/// (`organization`, `team`, `service-account` or `api-key`) and `limit`
/// which of its limits (`requests_per_minute`, `tokens_per_minute` or
/// `max_concurrent`), as an end user's refusal names their plan's.
pub(crate) fn rate_limited(
    breach: crate::persistence::rate_limits::RateLimitBreach,
    path: &str,
    id: &str,
) -> Response {
    let code = match breach.kind {
        RateLimitKind::RequestsPerMinute | RateLimitKind::TokensPerMinute => {
            ProblemCode::RateLimitExceeded
        }
        RateLimitKind::Concurrency => ProblemCode::ConcurrencyLimitExceeded,
    };
    let level = match breach.scope {
        "team" => "the team's",
        "service-account" => "the service account's",
        "api-key" => "the API key's",
        _ => "the organization's",
    };
    let detail = if breach.never_fits {
        format!(
            "the fewest tokens the request can be served with (its input, any reasoning allowance and, for chat, one output token) are more than {level} {} limit",
            breach.kind.as_str()
        )
    } else {
        format!("{level} {} limit is reached", breach.kind.as_str())
    };
    let status = StatusCode::TOO_MANY_REQUESTS;
    let body = serde_json::json!({
        "type": format!("https://gateway.invalid/problems/{}", code.code()),
        "code": code.code(),
        "title": status.canonical_reason().unwrap_or("Request failed"),
        "status": status.as_u16(),
        "detail": detail,
        "instance": path,
        "request_id": id,
        "scope": breach.scope,
        "limit": breach.kind.as_str(),
    });
    let mut response = (status, Json(body)).into_response();
    response
        .extensions_mut()
        .insert(crate::metrics::ProblemCode(code.code()));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    headers.insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(id).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    headers.insert(RETRY_AFTER, HeaderValue::from(breach.retry_after_seconds));
    response
}

pub(crate) fn body_rejection(
    rejection: axum::extract::rejection::JsonRejection,
    path: &str,
    id: &str,
) -> Response {
    let status = rejection.status();
    let code = match status {
        StatusCode::PAYLOAD_TOO_LARGE => ProblemCode::RequestTooLarge,
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ProblemCode::UnsupportedMediaType,
        StatusCode::UNPROCESSABLE_ENTITY => ProblemCode::RequestBodyUnprocessable,
        _ => ProblemCode::InvalidRequestBody,
    };
    problem(status, code, path, id)
}
fn provider_problem(error: &ProviderError, path: &str, id: &str) -> Response {
    let (status, code) = match error.category() {
        crate::domain::inference::ProviderErrorCategory::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::ProviderRateLimited,
        ),
        crate::domain::inference::ProviderErrorCategory::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            ProblemCode::ProviderUnavailable,
        ),
        crate::domain::inference::ProviderErrorCategory::Timeout => {
            (StatusCode::GATEWAY_TIMEOUT, ProblemCode::ProviderTimeout)
        }
        crate::domain::inference::ProviderErrorCategory::InvalidRequest => (
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::ProviderInvalidRequest,
        ),
        _ => (StatusCode::BAD_GATEWAY, ProblemCode::ProviderFailure),
    };
    let mut response = problem(status, code, path, id);
    // Pass on the provider's requested wait, in whole seconds rounded up.
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        if let Some(wait) = error.retry_after() {
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(seconds.max(1)));
        }
    }
    response
}
#[cfg(test)]
#[tokio::test]
async fn no_budget_keeps_quota_exceeded_and_says_why() {
    use crate::application::AccountingServiceError;
    use crate::persistence::AccountingStoreError;
    let body = |error| async move {
        let response = accounting_error(
            InferenceFacadeError::Accounting(AccountingServiceError::Store(error)),
            CHAT_PATH,
            "id",
        );
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    };
    let none = body(AccountingStoreError::NoBudget).await;
    assert_eq!(none["code"], "quota_exceeded");
    assert_eq!(none["detail"], NO_BUDGET_DETAIL);
    let spent = body(AccountingStoreError::QuotaExceeded).await;
    assert_eq!(spent["code"], "quota_exceeded");
    assert_ne!(spent["detail"], NO_BUDGET_DETAIL);
    let key = body(AccountingStoreError::ApiKeyBudgetExceeded).await;
    assert_eq!(key["code"], "quota_exceeded");
    assert_eq!(key["detail"], API_KEY_BUDGET_DETAIL);
}

#[cfg(test)]
#[test]
fn provider_problems_pass_on_retry_after() {
    use crate::domain::inference::ProviderErrorCategory;
    let error = |category, wait: Option<std::time::Duration>| {
        ProviderError::from_parts(category, None, None, Some(429), None, wait, None).unwrap()
    };
    let response = provider_problem(
        &error(
            ProviderErrorCategory::RateLimited,
            Some(std::time::Duration::from_millis(2_500)),
        ),
        CHAT_PATH,
        "id",
    );
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[RETRY_AFTER], "3");
    let response = provider_problem(
        &error(ProviderErrorCategory::RateLimited, None),
        CHAT_PATH,
        "id",
    );
    assert!(response.headers().get(RETRY_AFTER).is_none());
    let response = provider_problem(
        &error(
            ProviderErrorCategory::InvalidRequest,
            Some(std::time::Duration::from_secs(5)),
        ),
        CHAT_PATH,
        "id",
    );
    assert!(response.headers().get(RETRY_AFTER).is_none());
}

fn provider_event_code(
    category: crate::domain::inference::ProviderErrorCategory,
) -> (&'static str, u16) {
    match category {
        crate::domain::inference::ProviderErrorCategory::RateLimited => (
            "provider_rate_limited",
            StatusCode::TOO_MANY_REQUESTS.as_u16(),
        ),
        crate::domain::inference::ProviderErrorCategory::Unavailable => (
            "provider_unavailable",
            StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        ),
        crate::domain::inference::ProviderErrorCategory::Timeout => {
            ("provider_timeout", StatusCode::GATEWAY_TIMEOUT.as_u16())
        }
        crate::domain::inference::ProviderErrorCategory::InvalidRequest => (
            "provider_invalid_request",
            StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
        ),
        _ => ("provider_failure", StatusCode::BAD_GATEWAY.as_u16()),
    }
}

pub(crate) fn unauthorized(code: ProblemCode, path: &str, id: &str) -> Response {
    let mut response = problem(StatusCode::UNAUTHORIZED, code, path, id);
    response
        .headers_mut()
        .insert("www-authenticate", HeaderValue::from_static("Bearer"));
    response
}
pub(crate) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    scopes: &[ApiKeyScope],
    id: &str,
    path: &str,
) -> Result<VerifiedIdentity, Response> {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        return Err(unauthorized(ProblemCode::AuthenticationRequired, path, id));
    };
    let Some((scheme, token)) = value.split_once(char::is_whitespace) else {
        return Err(unauthorized(ProblemCode::AuthenticationRequired, path, id));
    };
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || token.chars().any(char::is_whitespace)
        || !token.starts_with("gw_")
    {
        return Err(unauthorized(ProblemCode::InvalidApiKey, path, id));
    }
    match state.identity().authenticate_bearer_key(token).await {
        Ok(Some(identity)) if scopes.iter().any(|scope| identity.scopes.contains(scope)) => {
            Ok(identity)
        }
        Ok(Some(_identity)) => Err(problem(
            StatusCode::FORBIDDEN,
            ProblemCode::InsufficientScope,
            path,
            id,
        )),
        Ok(None) => Err(unauthorized(ProblemCode::InvalidApiKey, path, id)),
        Err(_) => Err(problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ProblemCode::AuthenticationUnavailable,
            path,
            id,
        )),
    }
}
pub(crate) fn idempotency(headers: &HeaderMap, id: &str, path: &str) -> Result<String, Response> {
    let Some(value) = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::IdempotencyKeyRequired,
            path,
            id,
        ));
    };
    if value.trim().is_empty() || value.len() > 255 {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::IdempotencyKeyInvalid,
            path,
            id,
        ));
    }
    Ok(value.to_owned())
}
impl From<RequestBoundError> for ProblemCode {
    fn from(error: RequestBoundError) -> Self {
        match error {
            RequestBoundError::ChatRequestOutOfBounds => Self::ChatRequestOutOfBounds,
            RequestBoundError::ChatRoleInvalid => Self::ChatRoleInvalid,
            RequestBoundError::ChatInputBoundExceeded => Self::ChatInputBoundExceeded,
            RequestBoundError::TemperatureInvalid => Self::TemperatureInvalid,
            RequestBoundError::MaxTokensInvalid => Self::MaxTokensInvalid,
            RequestBoundError::EmbeddingRequestOutOfBounds => Self::EmbeddingRequestOutOfBounds,
            RequestBoundError::EmbeddingInputBoundExceeded => Self::EmbeddingInputBoundExceeded,
            RequestBoundError::ChatToolsInvalid(_) => Self::ChatToolsInvalid,
        }
    }
}

/// The request `body` asks for, or the 422 problem that says why it is out
/// of bounds.
fn chat_request(body: ChatBody, path: &str, id: &str) -> Result<(String, ChatRequest), Response> {
    crate::domain::public_request::chat_request(body).map_err(|error| match error {
        RequestBoundError::ChatToolsInvalid(detail) => problem_with_detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::ChatToolsInvalid,
            detail,
            path,
            id,
        ),
        other => problem(StatusCode::UNPROCESSABLE_ENTITY, other.into(), path, id),
    })
}

fn embedding_request(body: EmbeddingBody) -> Result<(String, EmbeddingRequest), ProblemCode> {
    crate::domain::public_request::embedding_request(body).map_err(Into::into)
}

pub(crate) fn accounting_error(error: InferenceFacadeError, path: &str, id: &str) -> Response {
    match error {
        InferenceFacadeError::CircuitOpen {
            retry_after_seconds,
        } => {
            let mut response = problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ProblemCode::ProviderCircuitOpen,
                path,
                id,
            );
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            response
        }
        InferenceFacadeError::UnknownModel => problem(
            StatusCode::NOT_FOUND,
            ProblemCode::ModelUnavailable,
            path,
            id,
        ),
        InferenceFacadeError::ToolsUnsupported(detail) => problem_with_detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::ToolsUnsupported,
            detail,
            path,
            id,
        ),
        InferenceFacadeError::RoutingPolicyDenied => problem(
            StatusCode::FORBIDDEN,
            ProblemCode::RoutingPolicyDenied,
            path,
            id,
        ),
        InferenceFacadeError::InsufficientScope => problem(
            StatusCode::FORBIDDEN,
            ProblemCode::InsufficientScope,
            path,
            id,
        ),
        InferenceFacadeError::Catalog(_) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ProblemCode::InferenceUnavailable,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(
            crate::application::AccountingServiceError::AlreadyClaimed,
        ) => problem(
            StatusCode::CONFLICT,
            ProblemCode::IdempotencyInProgress,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::IdempotencyConflict,
        )) => problem(
            StatusCode::CONFLICT,
            ProblemCode::IdempotencyConflict,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::NotClaimable,
        )) => problem(
            StatusCode::CONFLICT,
            ProblemCode::IdempotencyTerminal,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::RateLimited(breach),
        )) => rate_limited(breach, path, id),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::EndUserRequired,
        )) => problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::EndUserRequired,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::EndUserNotRegistered,
        )) => problem(
            StatusCode::FORBIDDEN,
            ProblemCode::EndUserNotRegistered,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::EndUserQuotaExceeded(refusal),
        )) => end_user_quota_exceeded(&refusal, path, id),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::EndUserRateLimited(refusal),
        )) => end_user_rate_limited(&refusal, path, id),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::QuotaExceeded,
        )) => problem(
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::QuotaExceeded,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::ApiKeyBudgetExceeded,
        )) => problem_with_detail(
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::QuotaExceeded,
            API_KEY_BUDGET_DETAIL,
            path,
            id,
        ),
        // The same code (a budget cannot cover the request), with the
        // reason: a new organization has none until an administrator adds
        // one.
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::NoBudget,
        )) => problem_with_detail(
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::QuotaExceeded,
            NO_BUDGET_DETAIL,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(
            crate::application::AccountingServiceError::InputBoundExceeded,
        ) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::AccountingBoundExceeded,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Store(
            crate::persistence::AccountingStoreError::BoundExceeded,
        )) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ProblemCode::AccountingBoundExceeded,
            path,
            id,
        ),
        InferenceFacadeError::Accounting(crate::application::AccountingServiceError::Provider(
            error,
        )) => provider_problem(&error, path, id),
        InferenceFacadeError::Accounting(_) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ProblemCode::InferenceUnavailable,
            path,
            id,
        ),
    }
}

pub async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<JsonExtractor<ChatBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let id = request_id(&headers);
    let path = CHAT_PATH;
    let identity = match authenticate(&state, &headers, &[ApiKeyScope::Inference], &id, path).await
    {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let key = match idempotency(&headers, &id, path) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    let identity = match end_user(&headers, body.user.as_deref(), path, &id) {
        Ok(end_user) => VerifiedIdentity {
            end_user,
            ..identity
        },
        Err(response) => return response,
    };
    let (alias, request) = match chat_request(body, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state
        .inference()
        .chat(&identity, &alias, request, &key, correlation(&id))
        .await
    {
        Ok(response) => {
            let payload = ChatBodyResponse {
                id: id.clone(),
                model: alias,
                choices: response.choices().iter().map(choice_response).collect(),
                usage: response.usage().map(|value| usage_response(value.usage())),
            };
            let mut result = Json(payload).into_response();
            result
                .headers_mut()
                .insert(REQUEST_ID_HEADER, HeaderValue::from_str(&id).unwrap());
            result
        }
        Err(error) => accounting_error(error, path, &id),
    }
}

pub async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<JsonExtractor<EmbeddingBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let id = request_id(&headers);
    let path = EMBEDDINGS_PATH;
    let identity = match authenticate(&state, &headers, &[ApiKeyScope::Embeddings], &id, path).await
    {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let key = match idempotency(&headers, &id, path) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    let identity = match end_user(&headers, body.user.as_deref(), path, &id) {
        Ok(end_user) => VerifiedIdentity {
            end_user,
            ..identity
        },
        Err(response) => return response,
    };
    let (alias, request) = match embedding_request(body) {
        Ok(value) => value,
        Err(code) => return problem(StatusCode::UNPROCESSABLE_ENTITY, code, path, &id),
    };
    match state
        .inference()
        .embeddings(&identity, &alias, request, &key, correlation(&id))
        .await
    {
        Ok(response) => {
            let payload = embedding_response(response, &alias);
            let mut result = Json(payload).into_response();
            result
                .headers_mut()
                .insert(REQUEST_ID_HEADER, HeaderValue::from_str(&id).unwrap());
            result
        }
        Err(error) => accounting_error(error, path, &id),
    }
}

pub async fn chat_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<JsonExtractor<ChatBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let id = request_id(&headers);
    let path = STREAM_PATH;
    let identity = match authenticate(&state, &headers, &[ApiKeyScope::Inference], &id, path).await
    {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if headers.contains_key("last-event-id") {
        return problem(
            StatusCode::CONFLICT,
            ProblemCode::StreamResumeUnsupported,
            path,
            &id,
        );
    }
    let key = match idempotency(&headers, &id, path) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    let identity = match end_user(&headers, body.user.as_deref(), path, &id) {
        Ok(end_user) => VerifiedIdentity {
            end_user,
            ..identity
        },
        Err(response) => return response,
    };
    let (alias, request) = match chat_request(body, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let stream = match state
        .inference()
        .chat_stream(&identity, &alias, request, &key, correlation(&id))
        .await
    {
        Ok(stream) => stream,
        Err(error) => return accounting_error(error, path, &id),
    };
    let stream = CountedStream {
        inner: stream,
        counted: false,
    }
    .map(move |event| Ok::<Bytes, Infallible>(Bytes::from(sse_event(event, &alias))));
    let mut response = Response::new(Body::from_stream(stream));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(REQUEST_ID_HEADER, HeaderValue::from_str(&id).unwrap());
    response
}

/// Counts a started stream's inference outcome once it is known: `ok` when
/// the response completes, the error event's code when it fails,
/// `incomplete` when it ends otherwise, and `abandoned` when the client
/// leaves first. Requests refused before streaming are counted by the HTTP
/// metrics layer.
struct CountedStream {
    inner: AccountedEventStream,
    counted: bool,
}

impl CountedStream {
    fn record(&mut self, outcome: &'static str) {
        if !self.counted {
            self.counted = true;
            crate::metrics::global().inc(
                &crate::metrics::INFERENCE_REQUESTS,
                &["chat_stream", outcome],
            );
        }
    }
}

impl Stream for CountedStream {
    type Item = Result<ChatStreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = self.inner.as_mut().poll_next(cx);
        match &item {
            Poll::Ready(Some(Ok(ChatStreamEvent::ResponseCompleted(_)))) => self.record("ok"),
            Poll::Ready(Some(Err(error))) => self.record(provider_event_code(error.category()).0),
            Poll::Ready(None) => self.record("incomplete"),
            _ => {}
        }
        item
    }
}

impl Drop for CountedStream {
    fn drop(&mut self) {
        self.record("abandoned");
    }
}

fn sse_event(event: Result<ChatStreamEvent, ProviderError>, public_model: &str) -> String {
    let (name, data): (&str, Value) = match event {
        Ok(ChatStreamEvent::ResponseStarted(_value)) => {
            ("response.started", json!({"model": public_model}))
        }
        Ok(ChatStreamEvent::ChoiceDelta(value)) => (
            "choice.delta",
            json!({"choice_index": value.choice_index, "content_delta": value.content_delta, "refusal_delta": value.refusal_delta, "annotations": value.annotations}),
        ),
        Ok(ChatStreamEvent::ToolCallDelta(value)) => (
            "tool_call.delta",
            json!({"choice_index": value.choice_index, "tool_call_index": value.tool_call_index, "id": value.id, "name": value.name, "arguments_delta": value.arguments_delta}),
        ),
        Ok(ChatStreamEvent::ChoiceFinished(value)) => (
            "choice.finished",
            json!({"choice_index": value.choice_index(), "finish_reason": value.finish_reason().as_str()}),
        ),
        Ok(ChatStreamEvent::Usage(value)) => ("usage", json!(usage_response(value.usage()))),
        Ok(ChatStreamEvent::ResponseCompleted(value)) => (
            "response.completed",
            json!({
                "model": public_model,
                "usage": value
                    .usage()
                    .map(|snapshot| usage_response(snapshot.usage()))
            }),
        ),
        Ok(ChatStreamEvent::SafetyAnnotation(value)) => ("safety.annotation", json!(value)),
        Err(error) => {
            let (code, status) = provider_event_code(error.category());
            ("error", json!({"code": code, "status": status}))
        }
    };
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
    )
}

#[cfg(test)]
mod stream_outcome_tests {
    use super::*;
    use crate::domain::inference::{ProviderErrorCategory, ResponseCompletion};
    use crate::metrics::{INFERENCE_REQUESTS, global as metrics};

    fn counted(events: Vec<Result<ChatStreamEvent, ProviderError>>) -> CountedStream {
        CountedStream {
            inner: Box::pin(futures::stream::iter(events)),
            counted: false,
        }
    }

    fn outcomes(outcome: &str) -> i64 {
        metrics()
            .value(&INFERENCE_REQUESTS, &["chat_stream", outcome])
            .unwrap_or(0)
    }

    // Other tests stream concurrently, so counts are checked as lower bounds.
    #[tokio::test]
    async fn a_started_stream_is_counted_once_when_it_ends() {
        let completed = || {
            Ok(ChatStreamEvent::ResponseCompleted(
                ResponseCompletion::new(None, "model".to_owned(), None).unwrap(),
            ))
        };
        let before = outcomes("ok");
        let events: Vec<_> = counted(vec![completed()]).collect().await;
        assert_eq!(events.len(), 1);
        assert!(outcomes("ok") > before);

        let before = outcomes("provider_timeout");
        let failed = counted(vec![Err(ProviderError::new(
            ProviderErrorCategory::Timeout,
        ))]);
        let _: Vec<_> = failed.collect().await;
        assert!(outcomes("provider_timeout") > before);

        let before = outcomes("incomplete");
        let _: Vec<_> = counted(vec![]).collect().await;
        assert!(outcomes("incomplete") > before);

        let before = outcomes("abandoned");
        drop(counted(vec![completed()]));
        assert!(outcomes("abandoned") > before);
    }
}
