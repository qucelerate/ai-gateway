//! Shared decoding of provider model-list pages.

use serde_json::Value;

use crate::domain::{
    discovery::ModelListBuilder,
    inference::{ProviderError, ProviderErrorCategory},
};

/// Where a family's model-list page keeps its entries.
pub(super) struct PageShape {
    /// The array of entries.
    pub(super) items: &'static str,
    /// The entry field holding the identifier a deployment calls.
    pub(super) id: &'static str,
    pub(super) display_name: Option<&'static str>,
    /// Removed from the front of each identifier (Gemini's `models/`).
    pub(super) id_prefix: Option<&'static str>,
    /// Proto3 JSON omits an empty repeated field, so an absent array is an
    /// empty page rather than a malformed one.
    pub(super) omits_empty: bool,
}

/// Adds one page to `builder`. A page without its entry array is a protocol
/// error unless the family omits empty arrays; an entry without a usable
/// identifier is skipped.
/// An Azure resource's deployments: `{"data": [{"id": <deployment>, "model": <model>}]}`.
pub(super) const DEPLOYMENT_PAGE: PageShape = PageShape {
    items: "data",
    id: "id",
    display_name: Some("model"),
    id_prefix: None,
    omits_empty: false,
};

pub(super) fn push_page(
    builder: &mut ModelListBuilder,
    page: &Value,
    shape: &PageShape,
) -> Result<(), ProviderError> {
    let items = match page.get(shape.items) {
        None if shape.omits_empty && page.is_object() => return Ok(()),
        items => items.and_then(Value::as_array).ok_or_else(protocol_error)?,
    };
    for item in items {
        let Some(id) = item.get(shape.id).and_then(Value::as_str) else {
            continue;
        };
        let id = match shape.id_prefix {
            Some(prefix) => id.strip_prefix(prefix).unwrap_or(id),
            None => id,
        };
        let display_name = shape
            .display_name
            .and_then(|field| item.get(field))
            .and_then(Value::as_str);
        builder
            .push(id, display_name)
            .map_err(|_| protocol_error())?;
    }
    Ok(())
}

/// The next page token, or `None` on the last page. An empty token ends the
/// listing too, so a provider cannot loop on it.
pub(super) fn next_token<'a>(page: &'a Value, field: &str) -> Option<&'a str> {
    page.get(field)
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
}

/// A listing that is still paging after `MAX_MODEL_LIST_PAGES` pages.
pub(super) fn too_many_pages() -> ProviderError {
    protocol_error()
}

fn protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Protocol)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::{
        config::{
            AnthropicConfig, AzureAiFoundryConfig, AzureOpenAiConfig, BedrockConfig, GeminiConfig,
            OpenAiCompatibleConfig, OpenAiConfig,
        },
        domain::discovery::DiscoveredModel,
        provider::{
            InferenceProviderAdapter, MAX_MODEL_LIST_PAGES, anthropic::AnthropicAdapter,
            azure_ai_foundry::AzureAiFoundryAdapter, azure_openai::AzureOpenAiAdapter,
            bedrock::BedrockAdapter, gemini::GeminiAdapter, openai::OpenAiAdapter,
            openai_compatible::OpenAiCompatibleAdapter,
        },
    };

    const SHAPE: PageShape = PageShape {
        items: "models",
        id: "name",
        display_name: Some("displayName"),
        id_prefix: Some("models/"),
        omits_empty: false,
    };

    #[test]
    fn entries_are_decoded_and_unusable_ones_skipped() {
        let mut builder = ModelListBuilder::default();
        push_page(
            &mut builder,
            &json!({"models": [
                {"name": "models/gemini-pro", "displayName": "Gemini Pro"},
                {"name": 7},
                {"displayName": "nameless"},
                {"name": "tunedModels/x"}
            ]}),
            &SHAPE,
        )
        .unwrap();
        let models = builder.finish();
        let ids: Vec<_> = models.iter().map(|model| model.model.as_str()).collect();
        assert_eq!(ids, ["gemini-pro", "tunedModels/x"]);
        assert_eq!(models[0].display_name.as_deref(), Some("Gemini Pro"));
    }

    #[test]
    fn a_page_without_its_entries_is_a_protocol_error() {
        let error = push_page(
            &mut ModelListBuilder::default(),
            &json!({"data": []}),
            &SHAPE,
        )
        .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
    }

    #[test]
    fn an_omitted_array_is_an_empty_page_only_where_the_family_omits_empties() {
        let omitting = PageShape {
            omits_empty: true,
            ..SHAPE
        };
        push_page(&mut ModelListBuilder::default(), &json!({}), &omitting).unwrap();
        assert!(push_page(&mut ModelListBuilder::default(), &json!(null), &omitting).is_err());
        assert!(push_page(&mut ModelListBuilder::default(), &json!({}), &SHAPE).is_err());
    }

    #[test]
    fn an_empty_token_ends_the_listing() {
        assert_eq!(next_token(&json!({"t": "abc"}), "t"), Some("abc"));
        assert_eq!(next_token(&json!({"t": ""}), "t"), None);
        assert_eq!(next_token(&json!({"t": null}), "t"), None);
        assert_eq!(next_token(&json!({}), "t"), None);
    }

    /// Serves `responses` in order, one per connection, and reports each
    /// request's head.
    async fn serve(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap();
                    head.extend_from_slice(&buffer[..count]);
                }
                sender.send(String::from_utf8(head).unwrap()).unwrap();
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), receiver)
    }

    fn heads(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    fn ids(models: &[DiscoveredModel]) -> Vec<&str> {
        models.iter().map(|model| model.model.as_str()).collect()
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn openai_lists_one_page_with_the_bearer_key() {
        let (base, mut requests) = serve(vec![(
            200,
            r#"{"object":"list","data":[{"id":"gpt-4o","owned_by":"system"},{"id":"babbage-002"}]}"#.into(),
        )])
        .await;
        let adapter = OpenAiAdapter::new(&OpenAiConfig {
            base_url: format!("{base}/v1"),
            api_key: "list-key".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let models = adapter.list_models().await.unwrap();
        assert_eq!(ids(&models), ["babbage-002", "gpt-4o"]);
        let heads = heads(&mut requests);
        assert!(heads[0].starts_with("GET /v1/models HTTP/1.1"), "{heads:?}");
        assert!(heads[0].contains("authorization: Bearer list-key"));
    }

    #[tokio::test]
    async fn openai_compatible_sends_its_extra_headers_and_maps_errors() {
        let (base, mut requests) = serve(vec![(
            401,
            r#"{"error":{"type":"invalid_api_key"}}"#.into(),
        )])
        .await;
        let adapter = OpenAiCompatibleAdapter::new(&OpenAiCompatibleConfig {
            base_url: format!("{base}/v1"),
            api_key: None,
            extra_headers: vec![("x-tenant-id".into(), "tenant-1".into())],
            request_stream_usage: false,
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
            pinned_client: None,
        })
        .unwrap();
        let error = adapter.list_models().await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Authentication);
        let heads = heads(&mut requests);
        assert!(heads[0].starts_with("GET /v1/models HTTP/1.1"));
        assert!(heads[0].contains("x-tenant-id: tenant-1"));
        assert!(!heads[0].contains("authorization"));
    }

    #[tokio::test]
    async fn anthropic_follows_last_id_while_more_pages_remain() {
        let (base, mut requests) = serve(vec![
            (
                200,
                r#"{"data":[{"id":"claude-b","display_name":"Claude B"}],"has_more":true,"last_id":"claude-b"}"#.into(),
            ),
            (
                200,
                r#"{"data":[{"id":"claude-a"}],"has_more":false,"last_id":"claude-a"}"#.into(),
            ),
        ])
        .await;
        let adapter = AnthropicAdapter::new(&AnthropicConfig {
            base_url: base,
            api_key: "anthropic-key".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let models = adapter.list_models().await.unwrap();
        assert_eq!(ids(&models), ["claude-a", "claude-b"]);
        assert_eq!(models[1].display_name.as_deref(), Some("Claude B"));
        let heads = heads(&mut requests);
        assert!(
            heads[0].starts_with("GET /v1/models?limit=1000 HTTP/1.1"),
            "{heads:?}"
        );
        assert!(heads[1].starts_with("GET /v1/models?limit=1000&after_id=claude-b HTTP/1.1"));
        assert!(heads[0].contains("x-api-key: anthropic-key"));
        assert!(heads[0].contains("anthropic-version: "));
    }

    #[tokio::test]
    async fn anthropic_malformed_paging_is_a_protocol_error() {
        for body in [
            r#"{"data":[{"id":"claude-a"}],"has_more":true}"#,
            r#"{"data":[{"id":"claude-a"}]}"#,
            r#"{"data":[{"id":"claude-a"}],"has_more":"false"}"#,
        ] {
            let (base, _requests) = serve(vec![(200, body.into())]).await;
            let adapter = AnthropicAdapter::new(&AnthropicConfig {
                base_url: base,
                api_key: "anthropic-key".into(),
                request_timeout: TIMEOUT,
                connect_timeout: TIMEOUT,
                max_response_bytes: 1 << 20,
            })
            .unwrap();
            let error = adapter.list_models().await.unwrap_err();
            assert_eq!(error.category(), ProviderErrorCategory::Protocol, "{body}");
        }
    }

    #[tokio::test]
    async fn gemini_strips_the_models_prefix_and_follows_page_tokens() {
        let (base, mut requests) = serve(vec![
            (
                200,
                r#"{"models":[{"name":"models/gemini-2.0-flash","displayName":"Gemini 2.0 Flash"}],"nextPageToken":"p2=/+"}"#.into(),
            ),
            (200, "{}".into()),
        ])
        .await;
        let adapter = GeminiAdapter::new(&GeminiConfig {
            base_url: base,
            auth: crate::config::GeminiAuth::ApiKey("gemini-key".into()),
            identity: None,
            agent_platform: None,
            agent_platform_location: None,
            storage_base_url: None,
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let models = adapter.list_models().await.unwrap();
        assert_eq!(ids(&models), ["gemini-2.0-flash"]);
        let heads = heads(&mut requests);
        assert!(
            heads[0].starts_with("GET /models?pageSize=1000 HTTP/1.1"),
            "{heads:?}"
        );
        assert!(heads[1].starts_with("GET /models?pageSize=1000&pageToken=p2%3D%2F%2B HTTP/1.1"));
        assert!(heads[0].contains("x-goog-api-key: gemini-key"));
    }

    #[tokio::test]
    async fn gemini_stops_at_the_page_limit() {
        let page = (200, r#"{"models":[],"nextPageToken":"again"}"#.to_owned());
        let (base, _requests) = serve(vec![page; MAX_MODEL_LIST_PAGES]).await;
        let adapter = GeminiAdapter::new(&GeminiConfig {
            base_url: base,
            auth: crate::config::GeminiAuth::ApiKey("gemini-key".into()),
            identity: None,
            agent_platform: None,
            agent_platform_location: None,
            storage_base_url: None,
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let error = adapter.list_models().await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
    }

    #[tokio::test]
    async fn bedrock_lists_foundation_models_and_inference_profiles_on_the_control_plane() {
        let (base, mut requests) = serve(vec![
            (
                200,
                r#"{"modelSummaries":[{"modelId":"anthropic.claude-3-haiku-20240307-v1:0","modelName":"Claude 3 Haiku","inferenceTypesSupported":["ON_DEMAND"]},{"modelId":"anthropic.claude-sonnet-4-6","modelName":"Claude Sonnet 4.6","inferenceTypesSupported":["INFERENCE_PROFILE"]},{"modelId":"amazon.nova-micro-v1:0"}]}"#.into(),
            ),
            (
                200,
                r#"{"inferenceProfileSummaries":[{"inferenceProfileId":"us.anthropic.claude-3-haiku-20240307-v1:0"}],"nextToken":"t/1="}"#.into(),
            ),
            (200, r#"{"inferenceProfileSummaries":[]}"#.into()),
            // The account's own resources: none.
            (200, r#"{"inferenceProfileSummaries":[]}"#.into()),
            (200, r#"{"provisionedModelSummaries":[]}"#.into()),
            (200, r#"{"modelSummaries":[]}"#.into()),
            (200, r#"{"marketplaceModelEndpoints":[]}"#.into()),
            (200, r#"{"modelDeploymentSummaries":[]}"#.into()),
        ])
        .await;
        let adapter = BedrockAdapter::new(&BedrockConfig {
            base_url: "http://127.0.0.1:9".into(),
            control_base_url: base,
            region: "us-east-1".into(),
            credentials: crate::aws_credentials::AwsCredentialSource::fixed(
                crate::aws::AwsCredentials {
                    access_key_id: "AKIDEXAMPLE".into(),
                    secret_access_key: zeroize::Zeroizing::new("secret".into()),
                    session_token: None,
                },
            ),
            sts_endpoint: None,
            s3_base_url: None,
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let models = adapter.list_models().await.unwrap();
        assert_eq!(
            ids(&models),
            [
                "amazon.nova-micro-v1:0",
                "anthropic.claude-3-haiku-20240307-v1:0",
                "anthropic.claude-sonnet-4-6",
                "us.anthropic.claude-3-haiku-20240307-v1:0"
            ]
        );
        // Callable only through an inference profile: listed, never offered.
        let availability = |id: &str| {
            models
                .iter()
                .find(|model| model.model == id)
                .unwrap()
                .availability
        };
        assert_eq!(
            availability("anthropic.claude-sonnet-4-6"),
            crate::domain::discovery::ModelAvailability::ProfileOnly
        );
        assert_eq!(
            availability("anthropic.claude-3-haiku-20240307-v1:0"),
            crate::domain::discovery::ModelAvailability::Listed
        );
        // A page that states no inference types leaves the model listed.
        assert_eq!(
            availability("amazon.nova-micro-v1:0"),
            crate::domain::discovery::ModelAvailability::Listed
        );
        let heads = heads(&mut requests);
        assert!(
            heads[0].starts_with("GET /foundation-models HTTP/1.1"),
            "{heads:?}"
        );
        assert!(
            heads[1].starts_with(
                "GET /inference-profiles?maxResults=1000&type=SYSTEM_DEFINED HTTP/1.1"
            )
        );
        assert!(heads[2].starts_with(
            "GET /inference-profiles?maxResults=1000&nextToken=t%2F1%3D&type=SYSTEM_DEFINED HTTP/1.1"
        ));
        for (head, request) in heads[3..7].iter().zip([
            "GET /inference-profiles?maxResults=1000&type=APPLICATION HTTP/1.1",
            "GET /provisioned-model-throughputs?maxResults=1000&statusEquals=InService HTTP/1.1",
            "GET /imported-models?maxResults=1000 HTTP/1.1",
            "GET /marketplace-model/endpoints?maxResults=1000 HTTP/1.1",
        ]) {
            assert!(head.starts_with(request), "{heads:?}");
        }
        assert!(heads[0].contains("authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
        assert!(heads[0].contains("/us-east-1/bedrock/aws4_request"));
    }

    #[tokio::test]
    async fn azure_families_do_not_list_models() {
        let azure = AzureOpenAiAdapter::new(&AzureOpenAiConfig {
            base_url: "http://127.0.0.1:9".into(),
            auth: crate::config::AzureAuth::ApiKey("key".into()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-06-01".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let foundry = AzureAiFoundryAdapter::new(&AzureAiFoundryConfig {
            base_url: "http://127.0.0.1:9".into(),
            auth: crate::config::AzureAuth::ApiKey("key".into()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-05-01-preview".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        for error in [
            azure.list_models().await.unwrap_err(),
            foundry.list_models().await.unwrap_err(),
        ] {
            assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
        }
    }

    #[tokio::test]
    async fn azure_access_lists_the_resources_deployments() {
        let page = r#"{"data":[{"id":"chat-prod","model":"gpt-4.1-mini"},{"id":"embed","model":"text-embedding-3-small"}],"object":"list"}"#;
        let (base, mut requests) = serve(vec![(200, page.into()), (200, page.into())]).await;
        let azure = AzureOpenAiAdapter::new(&AzureOpenAiConfig {
            base_url: base.clone(),
            auth: crate::config::AzureAuth::ApiKey("azure-key".into()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-06-01".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        let foundry = AzureAiFoundryAdapter::new(&AzureAiFoundryConfig {
            base_url: base,
            auth: crate::config::AzureAuth::ApiKey("foundry-key".into()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-05-01-preview".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        for adapter in [&azure as &dyn InferenceProviderAdapter, &foundry] {
            let models = adapter.check_access().await.unwrap();
            assert_eq!(ids(&models), ["chat-prod", "embed"]);
            assert_eq!(models[0].display_name.as_deref(), Some("gpt-4.1-mini"));
        }
        let heads = heads(&mut requests);
        for head in &heads {
            assert!(
                head.starts_with("GET /openai/deployments?api-version=2022-12-01 HTTP/1.1"),
                "{heads:?}"
            );
        }
        assert!(heads[0].contains("api-key: azure-key"));
        assert!(heads[1].contains("authorization: Bearer foundry-key"));
    }

    #[tokio::test]
    async fn azure_access_reports_a_missing_role() {
        let (base, _requests) = serve(vec![(
            403,
            r#"{"error":{"code":"PermissionDenied","message":"no role"}}"#.into(),
        )])
        .await;
        let azure = AzureOpenAiAdapter::new(&AzureOpenAiConfig {
            base_url: base,
            auth: crate::config::AzureAuth::ApiKey("azure-key".into()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-06-01".into(),
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap();
        assert_eq!(
            azure.check_access().await.unwrap_err().category(),
            ProviderErrorCategory::Authorization
        );
    }

    fn agent_platform(base: &str) -> GeminiAdapter {
        GeminiAdapter::new(&GeminiConfig {
            base_url: format!(
                "{base}/v1/projects/tenant-project/locations/global/publishers/google"
            ),
            auth: crate::config::GeminiAuth::ApiKey("gemini-key".into()),
            identity: None,
            agent_platform: crate::config::AgentPlatform::new("tenant-project", "global"),
            agent_platform_location: None,
            storage_base_url: None,
            request_timeout: TIMEOUT,
            connect_timeout: TIMEOUT,
            max_response_bytes: 1 << 20,
        })
        .unwrap()
    }

    const PUBLISHER_MODELS: &str = r#"{"publisherModels":[
        {"name":"publishers/google/models/gemini-1.0-pro-002"},
        {"name":"publishers/google/models/gemini-embedding-001"},
        {"name":"publishers/google/models/imagen-3"},
        {"name":"publishers/google/models/gemini-2.5-flash"}]}"#;

    #[tokio::test]
    async fn agent_platform_access_counts_tokens_in_the_tenants_project() {
        let (base, mut requests) = serve(vec![
            (200, PUBLISHER_MODELS.into()),
            // No partner models.
            (200, "{}".into()),
            (200, "{}".into()),
            (200, "{}".into()),
            // No endpoints, and a model policy it may not read.
            (200, "{}".into()),
            (
                403,
                r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#.into(),
            ),
            // Newest first; a listed model the location does not have is
            // skipped.
            (404, r#"{"error":{"code":404,"status":"NOT_FOUND"}}"#.into()),
            (200, r#"{"totalTokens":1}"#.into()),
        ])
        .await;
        let models = agent_platform(&base).check_access().await.unwrap();
        // Gemini and embedding models, without the one the location lacks.
        assert_eq!(ids(&models), ["gemini-1.0-pro-002", "gemini-embedding-001"]);
        let heads = heads(&mut requests);
        assert!(
            heads[0].starts_with("GET /v1beta1/publishers/google/models?pageSize=300 HTTP/1.1"),
            "{heads:?}"
        );
        assert!(heads[4].starts_with(
            "GET /v1/projects/tenant-project/locations/global/endpoints?pageSize=100 HTTP/1.1"
        ));
        assert!(heads[6].starts_with(
            "POST /v1/projects/tenant-project/locations/global/publishers/google/models/gemini-2.5-flash:countTokens"
        ));
        assert!(heads[7].contains("/models/gemini-1.0-pro-002:countTokens"));
    }

    #[tokio::test]
    async fn agent_platform_access_reports_a_missing_role() {
        let (base, _requests) = serve(vec![
            (200, PUBLISHER_MODELS.into()),
            (200, "{}".into()),
            (200, "{}".into()),
            (200, "{}".into()),
            (200, "{}".into()),
            (
                403,
                r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#.into(),
            ),
            (
                403,
                r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#.into(),
            ),
        ])
        .await;
        assert_eq!(
            agent_platform(&base)
                .check_access()
                .await
                .unwrap_err()
                .category(),
            ProviderErrorCategory::Authorization
        );
        // Signed in, but nothing to count with: unproven, not connected.
        let (base, _requests) = serve(vec![
            (200, r#"{"publisherModels":[]}"#.into()),
            (200, "{}".into()),
            (200, "{}".into()),
            (200, "{}".into()),
            (200, "{}".into()),
        ])
        .await;
        assert_eq!(
            agent_platform(&base)
                .check_access()
                .await
                .unwrap_err()
                .category(),
            ProviderErrorCategory::UnsupportedFeature
        );
    }
}
