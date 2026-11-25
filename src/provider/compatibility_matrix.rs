//! Compatibility matrix: every provider family's real adapter against every
//! operation. The catalog rules in `ProviderFamily` decide which routes can
//! exist; these tests prove each adapter agrees, refusing unsupported work
//! with `UnsupportedFeature` before contacting the provider.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    application::provider_resolver::ProviderTemplate,
    config::{
        AnthropicConfig, AzureAiFoundryConfig, AzureOpenAiConfig, BedrockConfig, GeminiConfig,
        OpenAiCompatibleConfig, OpenAiConfig,
    },
    domain::{
        catalog::{CatalogOperation, ProviderFamily},
        inference::{
            ChatMessage, ChatRequest, ChatRole, CorrelationId, DeploymentModelReference,
            EmbeddingRequest, InferenceContext, ProviderAttemptIdempotencyKey,
            ProviderErrorCategory,
        },
    },
    provider::InferenceProviderAdapter,
};

/// A provider stand-in that rejects every request and counts connections.
struct Provider {
    base_url: String,
    connections: Arc<AtomicUsize>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        Self {
            base_url,
            connections,
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn adapter(&self, family: ProviderFamily) -> Arc<dyn InferenceProviderAdapter> {
        let base_url = self.base_url.clone();
        let request_timeout = Duration::from_secs(5);
        let connect_timeout = Duration::from_secs(1);
        let max_response_bytes = 1024 * 1024;
        let template = match family {
            ProviderFamily::OpenAi => ProviderTemplate::OpenAi(OpenAiConfig {
                base_url,
                api_key: "test-key".into(),
                request_timeout,
                connect_timeout,
                max_response_bytes,
            }),
            ProviderFamily::Anthropic => ProviderTemplate::Anthropic(AnthropicConfig {
                base_url,
                api_key: "test-key".into(),
                request_timeout,
                connect_timeout,
                max_response_bytes,
            }),
            ProviderFamily::Gemini => ProviderTemplate::Gemini(GeminiConfig {
                base_url,
                auth: crate::config::GeminiAuth::ApiKey("test-key".into()),
                identity: None,
                agent_platform: None,
                agent_platform_location: None,
                storage_base_url: None,
                request_timeout,
                connect_timeout,
                max_response_bytes,
            }),
            ProviderFamily::Bedrock => ProviderTemplate::Bedrock(BedrockConfig {
                control_base_url: base_url.clone(),
                base_url,
                region: "us-east-1".into(),
                credentials: crate::aws_credentials::AwsCredentialSource::fixed(
                    crate::aws::AwsCredentials {
                        access_key_id: "AKIDEXAMPLE".into(),
                        secret_access_key: zeroize::Zeroizing::new("test-secret".into()),
                        session_token: None,
                    },
                ),
                sts_endpoint: None,
                s3_base_url: None,
                request_timeout,
                connect_timeout,
                max_response_bytes,
            }),
            ProviderFamily::AzureOpenAi => ProviderTemplate::AzureOpenAi(AzureOpenAiConfig {
                base_url,
                auth: crate::config::AzureAuth::ApiKey("test-key".into()),
                identity: Some(crate::azure_credentials::tests::unused_identity()),
                management: None,
                api_version: "2024-10-21".into(),
                request_timeout,
                connect_timeout,
                max_response_bytes,
            }),
            ProviderFamily::AzureAiFoundry => {
                ProviderTemplate::AzureAiFoundry(AzureAiFoundryConfig {
                    base_url,
                    auth: crate::config::AzureAuth::ApiKey("test-key".into()),
                    identity: Some(crate::azure_credentials::tests::unused_identity()),
                    management: None,
                    api_version: "2024-05-01-preview".into(),
                    request_timeout,
                    connect_timeout,
                    max_response_bytes,
                })
            }
            ProviderFamily::OpenAiCompatible => {
                ProviderTemplate::OpenAiCompatible(OpenAiCompatibleConfig {
                    base_url,
                    api_key: Some("test-key".into()),
                    extra_headers: Vec::new(),
                    request_stream_usage: true,
                    request_timeout,
                    connect_timeout,
                    max_response_bytes,
                    pinned_client: None,
                })
            }
        };
        assert_eq!(template.family(), family);
        template.adapter().unwrap()
    }
}

fn context(family: ProviderFamily, operation: CatalogOperation) -> InferenceContext {
    // Bedrock picks an embedding model's request body by its identifier.
    let model = if family == ProviderFamily::Bedrock && operation == CatalogOperation::Embeddings {
        "amazon.titan-embed-text-v2:0"
    } else {
        "model"
    };
    InferenceContext::new(
        CorrelationId::new(),
        ProviderAttemptIdempotencyKey::new("attempt-matrix").unwrap(),
        DeploymentModelReference::new("deployment", model).unwrap(),
    )
    .with_adapter_instance_key("matrix")
    .with_provider_family(family.as_str())
}

fn chat_request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_owned(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        temperature: None,
        max_tokens: Some(4),
        ..Default::default()
    }
}

/// Runs `operation` and returns the error category, if any.
async fn call(
    adapter: &dyn InferenceProviderAdapter,
    family: ProviderFamily,
    operation: CatalogOperation,
) -> Option<ProviderErrorCategory> {
    let context = context(family, operation);
    match operation {
        CatalogOperation::Chat => adapter.chat(context, chat_request()).await.err(),
        CatalogOperation::ChatStream => adapter.chat_stream(context, chat_request()).await.err(),
        CatalogOperation::Embeddings => {
            let request = EmbeddingRequest {
                input: vec!["hello".to_owned()],
            };
            adapter.embeddings(context, request).await.err()
        }
        // A batch is a provider job, not one adapter call (D-021).
        CatalogOperation::ChatBatch | CatalogOperation::EmbeddingsBatch => {
            unreachable!("batch operations are not single calls")
        }
    }
    .map(|error| error.category())
}

#[tokio::test]
async fn adapters_refuse_unsupported_operations_without_calling_the_provider() {
    for family in ProviderFamily::ALL {
        for operation in CatalogOperation::ALL
            .into_iter()
            .filter(|operation| !operation.is_batch())
        {
            let provider = Provider::start();
            let adapter = provider.adapter(family);
            let error = call(adapter.as_ref(), family, operation).await;
            let label = format!("{} {}", family.as_str(), operation.as_str());
            if family.supports(operation) {
                assert_ne!(
                    error,
                    Some(ProviderErrorCategory::UnsupportedFeature),
                    "{label}"
                );
                assert!(
                    provider.connections() > 0,
                    "{label} never reached the provider"
                );
            } else {
                assert_eq!(
                    error,
                    Some(ProviderErrorCategory::UnsupportedFeature),
                    "{label}"
                );
                assert_eq!(provider.connections(), 0, "{label} contacted the provider");
            }
        }
    }
}

#[tokio::test]
async fn only_exact_counting_families_ask_the_provider_for_a_token_count() {
    for family in ProviderFamily::ALL {
        let provider = Provider::start();
        let adapter = provider.adapter(family);
        let error = adapter
            .count_chat_input_tokens(&context(family, CatalogOperation::Chat), &chat_request())
            .await
            .err()
            .map(|error| error.category());
        let label = family.as_str();
        if family.counts_chat_input_exactly() {
            assert_ne!(
                error,
                Some(ProviderErrorCategory::UnsupportedFeature),
                "{label}"
            );
            assert!(
                provider.connections() > 0,
                "{label} never reached the provider"
            );
        } else {
            assert_eq!(
                error,
                Some(ProviderErrorCategory::UnsupportedFeature),
                "{label}"
            );
            assert_eq!(provider.connections(), 0, "{label} contacted the provider");
        }
    }
}

/// Gemini on Agent Platform serves the family's every operation, and lists
/// models, the way the Gemini API does; embeddings take an embedding model
/// and refuse any other unsent, since the gateway would not know how it
/// embeds.
#[tokio::test]
async fn gemini_on_agent_platform_serves_every_operation_and_lists_models() {
    let family = ProviderFamily::Gemini;
    let adapter_for = |provider: &Provider| {
        let platform = crate::config::AgentPlatform::new("tenant-project", "global").unwrap();
        ProviderTemplate::Gemini(GeminiConfig {
            base_url: format!(
                "{}/v1/projects/tenant-project/locations/global/publishers/google",
                provider.base_url
            ),
            auth: crate::config::GeminiAuth::ApiKey("test-key".into()),
            identity: None,
            agent_platform: Some(platform),
            agent_platform_location: None,
            storage_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        })
        .adapter()
        .unwrap()
    };
    let in_context = |model: &str| {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt-matrix").unwrap(),
            DeploymentModelReference::new("deployment", model).unwrap(),
        )
        .with_adapter_instance_key("matrix")
        .with_provider_family(family.as_str())
    };
    let embed = || EmbeddingRequest {
        input: vec!["hello".to_owned()],
    };
    for operation in CatalogOperation::ALL
        .into_iter()
        .filter(|operation| !operation.is_batch())
    {
        assert!(family.supports(operation));
        let provider = Provider::start();
        let adapter = adapter_for(&provider);
        let error = match operation {
            CatalogOperation::Chat => adapter
                .chat(in_context("gemini-2.5-flash"), chat_request())
                .await
                .err(),
            CatalogOperation::ChatStream => adapter
                .chat_stream(in_context("gemini-2.5-flash"), chat_request())
                .await
                .err(),
            CatalogOperation::Embeddings => adapter
                .embeddings(in_context("text-embedding-005"), embed())
                .await
                .err(),
            CatalogOperation::ChatBatch | CatalogOperation::EmbeddingsBatch => {
                unreachable!("batch operations are not single calls")
            }
        }
        .map(|error| error.category());
        let label = operation.as_str();
        assert_ne!(
            error,
            Some(ProviderErrorCategory::UnsupportedFeature),
            "{label}"
        );
        assert!(
            provider.connections() > 0,
            "{label} never reached the provider"
        );
    }

    let provider = Provider::start();
    let adapter = adapter_for(&provider);
    let error = adapter
        .embeddings(in_context("gemini-2.5-flash"), embed())
        .await
        .unwrap_err();
    assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    assert_eq!(provider.connections(), 0);
    let error = adapter.list_models().await.unwrap_err();
    assert_ne!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    assert!(provider.connections() > 0);
}
