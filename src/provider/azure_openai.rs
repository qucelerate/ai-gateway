use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::{Client, Response, StatusCode, header};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, fmt, time::Duration};
use url::Url;

use crate::{
    config::{AzureAuth, AzureOpenAiConfig},
    domain::inference::{
        Annotation, ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent,
        ChoiceFinished, Embedding, EmbeddingRequest, EmbeddingResponse, FinishReason,
        InferenceContext, ProviderError, ProviderErrorCategory, ResponseCompletion,
        ResponseStarted, SafeMetadata, StreamChoiceDelta, ToolCall, ToolCallDelta, Usage,
        UsageSnapshot,
    },
};

use crate::domain::discovery::DiscoveredModel;

use super::{
    ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream,
    batch::{
        BatchProvider, BatchTag, LineOutcome, MAX_BATCH_FILE_BYTES, MAX_FIND_PAGES, PageSearch,
        ProviderBatch, openai_format,
    },
};
use crate::domain::batch::{BatchEndpoint, LineRequest};

const PROVIDER: &str = "azure-openai";
const MAX_ERROR_BODY: usize = 16 * 1024;
/// The data-plane version that lists a resource's deployments.
const DEPLOYMENTS_API_VERSION: &str = "2022-12-01";
/// The data-plane version of the Files and Batch APIs: the first stable one
/// with Global and Data Zone Batch, whatever version requests use.
const BATCH_API_VERSION: &str = "2024-10-21";

pub(crate) struct AzureOpenAiAdapter {
    client: Client,
    base_url: Url,
    auth: AzureAuth,
    api_version: String,
    max_response_bytes: usize,
    /// Bounds the wait for an Entra token.
    request_timeout: Duration,
    /// The resource's deployments from Resource Manager, for discovery.
    deployments: Option<super::azure_management::DeploymentLister>,
}

impl fmt::Debug for AzureOpenAiAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureOpenAiAdapter")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("api_version", &self.api_version)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl AzureOpenAiAdapter {
    pub(crate) fn new(config: &AzureOpenAiConfig) -> Result<Self, ProviderError> {
        let base_url = Url::parse(&format!("{}/", config.base_url.trim_end_matches('/')))
            .map_err(|_| protocol_error())?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            // A total timeout would terminate an otherwise healthy long-lived
            // stream. Bound each idle read instead; connect timeout remains a
            // separate bound for establishing the request.
            .read_timeout(config.request_timeout)
            .build()
            .map_err(|_| ProviderError::new(ProviderErrorCategory::Internal))?;
        let deployments = config
            .management
            .as_ref()
            .map(|management| {
                super::azure_management::DeploymentLister::new(
                    management,
                    &base_url,
                    Some(super::azure_management::OPENAI_FORMATS),
                    PROVIDER,
                    config.connect_timeout,
                    config.request_timeout,
                    config.max_response_bytes,
                )
                .map(|lister| lister.checking_entitlement_for(&config.auth))
            })
            .transpose()?;
        Ok(Self {
            client,
            base_url,
            auth: config.auth.clone(),
            api_version: config.api_version.clone(),
            max_response_bytes: config.max_response_bytes,
            request_timeout: config.request_timeout,
            deployments,
        })
    }

    /// Azure's `deployments/{deployment-name}` path segment selects the
    /// underlying model; unlike a bare OpenAI model ID it is
    /// operator-chosen, so validate it the same way Bedrock validates its
    /// `modelId` path parameter before treating it as safe to encode.
    fn endpoint(&self, deployment: &str, suffix: &str) -> Result<Url, ProviderError> {
        if deployment.is_empty() || deployment.contains(['?', '#', ' ', '\n', '\r']) {
            return Err(internal_error());
        }
        let path = format!(
            "openai/deployments/{}/{suffix}",
            percent_encode_path_segment(deployment)
        );
        let mut url = self.base_url.join(&path).map_err(|_| protocol_error())?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);
        Ok(url)
    }

    async fn request(
        &self,
        url: Url,
        context: &InferenceContext,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let (name, value) = self.auth_header().await?;
        Ok(self
            .client
            .post(url)
            .header(name, value)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                "idempotency-key",
                context.provider_attempt_idempotency_key.as_str(),
            )
            .header("x-correlation-id", context.correlation_id.0.to_string())
            .header("x-client-request-id", context.correlation_id.0.to_string()))
    }

    /// `api-key` with a key; a bearer token with Entra.
    async fn auth_header(
        &self,
    ) -> Result<(header::HeaderName, header::HeaderValue), ProviderError> {
        let (name, value) = match &self.auth {
            AzureAuth::ApiKey(api_key) => (
                header::HeaderName::from_static("api-key"),
                header::HeaderValue::from_str(api_key),
            ),
            AzureAuth::Entra(source) => {
                let token = source
                    .get_within(self.request_timeout)
                    .await
                    .map_err(super::credentials_error)?;
                (
                    header::AUTHORIZATION,
                    header::HeaderValue::from_str(&format!("Bearer {}", token.as_str())),
                )
            }
        };
        let mut value = value.map_err(|_| ProviderError::new(ProviderErrorCategory::Internal))?;
        value.set_sensitive(true);
        Ok((name, value))
    }

    async fn response_error(&self, response: Response) -> ProviderError {
        let status = response.status();
        let request_id = response
            .headers()
            .get("apim-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let retry_after = retry_after(response.headers());
        let body = read_bounded(response, MAX_ERROR_BODY)
            .await
            .unwrap_or_default();
        let value = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        let provider_code = value
            .get("error")
            .and_then(|error| error.get("code").or_else(|| error.get("type")))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        ProviderError::from_parts(
            category_for_status(status),
            Some(PROVIDER.to_owned()),
            request_id,
            Some(status.as_u16()),
            provider_code,
            retry_after,
            None,
        )
        .unwrap_or_else(|_| ProviderError::new(ProviderErrorCategory::Protocol))
    }
}

impl AzureOpenAiAdapter {
    /// `openai/{path}` at the Batch API version.
    fn batch_url(&self, path: &str) -> Result<Url, ProviderError> {
        let mut url = self
            .base_url
            .join(&format!("openai/{path}"))
            .map_err(|_| protocol_error())?;
        url.query_pairs_mut()
            .append_pair("api-version", BATCH_API_VERSION);
        Ok(url)
    }

    async fn batch_send(
        &self,
        request: reqwest::RequestBuilder,
        limit: usize,
    ) -> Result<Vec<u8>, ProviderError> {
        let (name, value) = self.auth_header().await?;
        let response = request
            .header(name, value)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        read_bounded(response, limit).await
    }

    async fn batch_json(&self, request: reqwest::RequestBuilder) -> Result<Value, ProviderError> {
        let bytes = self
            .batch_send(
                request.header(header::ACCEPT, "application/json"),
                self.max_response_bytes,
            )
            .await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    fn batch_get(&self, path: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self.client.get(self.batch_url(path)?))
    }

    fn batch_post(&self, path: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self.client.post(self.batch_url(path)?))
    }
}

/// Azure OpenAI's Global and Data Zone Batch: OpenAI's Batch API under the
/// resource's `openai/` path, run by a batch-type deployment (`GlobalBatch`
/// or `DataZoneBatch`), which each line names as its model.
#[async_trait]
impl BatchProvider for AzureOpenAiAdapter {
    async fn find_batch(
        &self,
        tag: BatchTag,
        input_file_id: &str,
    ) -> Result<Option<ProviderBatch>, ProviderError> {
        // As for OpenAI: the listing is newest first, and a job is no older
        // than its input file.
        let file = self
            .batch_json(self.batch_get(&format!("files/{}", path_segment(input_file_id)))?)
            .await?;
        let input_created = file.get("created_at").and_then(Value::as_i64);
        let mut after: Option<String> = None;
        for _ in 0..MAX_FIND_PAGES {
            let mut request = self.batch_get("batches")?.query(&[("limit", "100")]);
            if let Some(after) = &after {
                request = request.query(&[("after", after.as_str())]);
            }
            let page = self.batch_json(request).await?;
            match openai_format::search_page(&page, tag, input_created)? {
                PageSearch::Found(found) => return openai_format::provider_batch(found).map(Some),
                PageSearch::Absent => return Ok(None),
                PageSearch::Next(last) => after = Some(last),
            }
        }
        Err(protocol_error())
    }

    async fn upload_batch_input(
        &self,
        tag: BatchTag,
        jsonl: Vec<u8>,
    ) -> Result<String, ProviderError> {
        let part = reqwest::multipart::Part::bytes(jsonl)
            .file_name(format!("gateway-{}-{}.jsonl", tag.batch_id, tag.job_id))
            .mime_str("application/jsonl")
            .map_err(|_| protocol_error())?;
        let form = reqwest::multipart::Form::new()
            .text("purpose", "batch")
            .part("file", part);
        let value = self
            .batch_json(self.batch_post("files")?.multipart(form))
            .await?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(protocol_error)
    }

    async fn create_batch(
        &self,
        input_file_id: &str,
        _input: Option<&[u8]>,
        endpoint: BatchEndpoint,
        tag: BatchTag,
        _model: &str,
    ) -> Result<ProviderBatch, ProviderError> {
        let body = json!({
            "input_file_id": input_file_id,
            "endpoint": azure_batch_url(endpoint),
            "completion_window": "24h",
            "metadata": openai_format::metadata(tag),
        });
        let request = self
            .batch_post("batches")?
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body);
        openai_format::provider_batch(&self.batch_json(request).await?)
    }

    async fn batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let path = format!("batches/{}", path_segment(id));
        openai_format::provider_batch(&self.batch_json(self.batch_get(&path)?).await?)
    }

    async fn cancel_batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let path = format!("batches/{}/cancel", path_segment(id));
        openai_format::provider_batch(&self.batch_json(self.batch_post(&path)?).await?)
    }

    async fn file_content(&self, id: &str) -> Result<Vec<u8>, ProviderError> {
        let path = format!("files/{}/content", path_segment(id));
        self.batch_send(self.batch_get(&path)?, MAX_BATCH_FILE_BYTES)
            .await
    }

    fn input_line(
        &self,
        _tag: BatchTag,
        line: u32,
        endpoint: BatchEndpoint,
        model: &str,
        request: &LineRequest,
    ) -> Result<String, ProviderError> {
        let body = match request {
            LineRequest::Chat(request) => {
                serde_json::to_value(ChatWireRequest::for_model(model, request))
                    .map_err(|_| protocol_error())?
            }
            LineRequest::Embeddings(request) => json!({"model": model, "input": request.input}),
        };
        openai_format::input_line(line, azure_batch_url(endpoint), body)
    }

    fn parse_results(
        &self,
        endpoint: BatchEndpoint,
        output: &[u8],
        errors: &[u8],
    ) -> std::collections::HashMap<u32, LineOutcome> {
        openai_format::parse_results(
            endpoint,
            output,
            errors,
            parse_chat_response,
            parse_embedding_response,
        )
    }
}

/// Azure's batch endpoints, without OpenAI's `/v1` prefix.
fn azure_batch_url(endpoint: BatchEndpoint) -> &'static str {
    match endpoint {
        BatchEndpoint::ChatCompletions => "/chat/completions",
        BatchEndpoint::Embeddings => "/embeddings",
    }
}

/// A provider id as one path segment.
fn path_segment(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

#[async_trait]
impl InferenceProviderAdapter for AzureOpenAiAdapter {
    fn batches(&self) -> Option<&dyn BatchProvider> {
        Some(self)
    }

    /// The resource's deployments from Resource Manager, when the resource
    /// is known (D-018): each one that can serve requests, with the model it
    /// serves and its operations. Otherwise unsupported: the data-plane
    /// listing ([`Self::check_access`]) uses a retired API version and does
    /// not say which model a deployment serves.
    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        match &self.deployments {
            Some(deployments) => deployments.list().await,
            None => Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            )),
        }
    }

    /// The resource's account model list, when the resource is known: what
    /// the account may deploy (D-018).
    async fn list_deployable_models(
        &self,
    ) -> Result<Option<Vec<crate::domain::discovery::DeployableModel>>, ProviderError> {
        match &self.deployments {
            Some(deployments) => deployments.list_deployable().await.map(Some),
            None => Ok(None),
        }
    }

    /// The resource's deployments, which requests are routed to: listing
    /// them proves the sign-in and the role on the resource, and nothing is
    /// generated. Discovery does not use it (`list_models`).
    async fn check_access(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let mut url = self
            .base_url
            .join("openai/deployments")
            .map_err(|_| protocol_error())?;
        url.query_pairs_mut()
            .append_pair("api-version", DEPLOYMENTS_API_VERSION);
        let (name, value) = self.auth_header().await?;
        let builder = self.client.get(url).header(name, value);
        let response = tokio::time::timeout(
            self.request_timeout,
            builder.header(header::ACCEPT, "application/json").send(),
        )
        .await
        .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
        .map_err(|error| {
            ProviderError::new(if error.is_timeout() {
                ProviderErrorCategory::Timeout
            } else {
                ProviderErrorCategory::Unavailable
            })
        })?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let body = read_bounded(response, self.max_response_bytes).await?;
        let page: Value = serde_json::from_slice(&body).map_err(|_| protocol_error())?;
        let mut models = crate::domain::discovery::ModelListBuilder::default();
        super::model_list::push_page(&mut models, &page, &super::model_list::DEPLOYMENT_PAGE)?;
        Ok(models.finish())
    }
    async fn count_chat_input_tokens(
        &self,
        _context: &InferenceContext,
        _request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        // Azure OpenAI Chat Completions has no provider-side token counting
        // operation, the same as public OpenAI. Do not approximate or make a
        // generation request here.
        Err(ProviderError::new(
            ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let deployment = context.deployment.model();
        let url = self.endpoint(deployment, "chat/completions")?;
        let body = ChatWireRequest::from_domain(&context, &request, false);
        let response = self
            .request(url, &context)
            .await?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let request_id = response
            .headers()
            .get("apim-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let value = read_json_bounded(response, self.max_response_bytes)
            .await
            .map_err(|error| preserve_request_id(error, request_id.as_deref()))?;
        parse_chat_response(value, request_id.clone())
            .map_err(|error| preserve_request_id(error, request_id.as_deref()))
    }

    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let deployment = context.deployment.model();
        let url = self.endpoint(deployment, "chat/completions")?;
        let body = ChatWireRequest::from_domain(&context, &request, true);
        let response = self
            .request(url, &context)
            .await?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let request_id = response
            .headers()
            .get("apim-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = response.bytes_stream();
        let stream = azure_openai_sse_stream(bytes, request_id, self.max_response_bytes);
        Ok(ValidatingChatEventStream::new(stream))
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        let deployment = context.deployment.model();
        let url = self.endpoint(deployment, "embeddings")?;
        let body = json!({
            "model": deployment,
            "input": request.input,
        });
        let response = self
            .request(url, &context)
            .await?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let request_id = response
            .headers()
            .get("apim-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let value = read_json_bounded(response, self.max_response_bytes)
            .await
            .map_err(|error| preserve_request_id(error, request_id.as_deref()))?;
        parse_embedding_response(value, request_id.clone())
            .map_err(|error| preserve_request_id(error, request_id.as_deref()))
    }
}

fn percent_encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

#[derive(Serialize)]
struct ChatWireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    // The default stable API version (2024-06-01) predates Azure's rollout
    // of `max_completion_tokens`; it only accepts `max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<Value>,
    /// `tools`, `tool_choice` and `parallel_tool_calls` (D-032).
    #[serde(flatten)]
    tools: super::tools::WireTools,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    /// Null on an assistant message that only calls tools.
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<super::tools::WireToolCall>>,
}

impl ChatWireRequest {
    fn from_domain(context: &InferenceContext, request: &ChatRequest, stream: bool) -> Self {
        Self::build(context.deployment.model(), request, stream)
    }

    /// A batch line's body: never streamed.
    fn for_model(model: &str, request: &ChatRequest) -> Self {
        Self::build(model, request, false)
    }

    fn build(model: &str, request: &ChatRequest, stream: bool) -> Self {
        Self {
            model: model.to_owned(),
            messages: request
                .messages
                .iter()
                .map(|message| WireMessage {
                    role: role_name(message.role),
                    content: super::tools::wire_content(message),
                    name: message.name.clone(),
                    tool_call_id: message.tool_call_id.clone(),
                    tool_calls: super::tools::wire_tool_calls(message),
                })
                .collect(),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stream,
            stream_options: stream.then(|| json!({"include_usage": true})),
            tools: super::tools::wire_tools(request),
        }
    }
}

fn role_name(role: ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

async fn read_bounded(response: Response, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(request_error)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(protocol_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_json_bounded(response: Response, limit: usize) -> Result<Value, ProviderError> {
    let bytes = read_bounded(response, limit).await?;
    serde_json::from_slice(&bytes).map_err(|_| protocol_error())
}

fn parse_chat_response(
    value: Value,
    request_id: Option<String>,
) -> Result<ChatResponse, ProviderError> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?
        .iter()
        .map(parse_choice)
        .collect::<Result<Vec<_>, _>>()?;
    let usage = parse_usage(value.get("usage"))?
        .map(|usage| UsageSnapshot::final_(usage).map_err(|_| protocol_error()))
        .transpose()?;
    let metadata = response_metadata(&value);
    ChatResponse::new(request_id, model.to_owned(), choices, usage, metadata)
        .map_err(|_| protocol_error())
}

fn parse_choice(value: &Value) -> Result<ChatChoice, ProviderError> {
    let message = value.get("message").ok_or_else(protocol_error)?;
    let role = parse_role(message.get("role").and_then(Value::as_str))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_calls = match message.get("tool_calls") {
        Some(value) => value
            .as_array()
            .ok_or_else(protocol_error)?
            .iter()
            .map(parse_tool_call)
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let finish_reason = value
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(|value| FinishReason::new(value.to_owned()));
    let annotations = parse_annotations(
        message
            .get("annotations")
            .or_else(|| value.get("annotations")),
    )?;
    let refusal = message
        .get("refusal")
        .and_then(Value::as_str)
        .or_else(|| value.get("refusal").and_then(Value::as_str))
        .map(ToOwned::to_owned);
    Ok(ChatChoice {
        index: u32::try_from(
            value
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(protocol_error)?,
        )
        .map_err(|_| protocol_error())?,
        message: ChatMessage {
            role,
            content,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        },
        finish_reason,
        tool_calls,
        refusal,
        safety: vec![],
        annotations,
    })
}

fn parse_tool_call(value: &Value) -> Result<ToolCall, ProviderError> {
    let function = value.get("function").ok_or_else(protocol_error)?;
    Ok(ToolCall {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        name: function
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        arguments: function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn parse_annotations(value: Option<&Value>) -> Result<Vec<Annotation>, ProviderError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(protocol_error)?;
    values
        .iter()
        .map(|value| {
            let kind = value
                .get("type")
                .or_else(|| value.get("kind"))
                .and_then(Value::as_str)
                .ok_or_else(protocol_error)?;
            let annotation_value = value
                .get("value")
                .or_else(|| value.get("url_citation"))
                .or_else(|| value.get("file_citation"))
                .unwrap_or(value);
            Ok(Annotation {
                kind: kind.to_owned(),
                value: serde_json::to_string(annotation_value).map_err(|_| protocol_error())?,
            })
        })
        .collect()
}

fn response_metadata(value: &Value) -> SafeMetadata {
    let mut metadata = Map::new();
    for (key, output_key) in [
        ("id", "azure_openai_response_id"),
        ("system_fingerprint", "system_fingerprint"),
        ("service_tier", "service_tier"),
    ] {
        if let Some(value) = value.get(key).filter(|value| value.is_string()) {
            metadata.insert(output_key.to_owned(), value.clone());
        }
    }
    SafeMetadata::from_trusted_mapping(metadata).unwrap_or_else(|_| SafeMetadata::new())
}

fn parse_embedding_response(
    value: Value,
    request_id: Option<String>,
) -> Result<EmbeddingResponse, ProviderError> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?
        .iter()
        .map(|item| {
            let index = u32::try_from(
                item.get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(protocol_error)?,
            )
            .map_err(|_| protocol_error())?;
            let vector = item
                .get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(protocol_error)?
                .iter()
                .map(|value| {
                    value
                        .as_f64()
                        .map(|value| value as f32)
                        .ok_or_else(protocol_error)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Embedding::new(index, vector).map_err(|_| protocol_error())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let usage = parse_embedding_usage(value.get("usage"))?
        .map(|usage| UsageSnapshot::final_(usage).map_err(|_| protocol_error()))
        .transpose()?;
    EmbeddingResponse::new(request_id, model.to_owned(), data, usage).map_err(|_| protocol_error())
}

fn parse_role(value: Option<&str>) -> Result<ChatRole, ProviderError> {
    match value.ok_or_else(protocol_error)? {
        "system" => Ok(ChatRole::System),
        "developer" => Ok(ChatRole::Developer),
        "user" => Ok(ChatRole::User),
        "assistant" => Ok(ChatRole::Assistant),
        "tool" => Ok(ChatRole::Tool),
        _ => Err(protocol_error()),
    }
}

fn parse_usage(value: Option<&Value>) -> Result<Option<Usage>, ProviderError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let input = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let output = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let cached = value
        .get("prompt_tokens_details")
        .and_then(|v| v.get("cached_tokens"))
        .and_then(Value::as_u64)
        .map(crate::domain::inference::TokenCounter::exact);
    let (output, reasoning) = super::split_reasoning(value, input, output);
    Usage::complete(
        crate::domain::inference::TokenCounter::exact(input),
        crate::domain::inference::TokenCounter::exact(output),
        cached,
        reasoning,
        None,
    )
    .map(Some)
    .map_err(|_| protocol_error())
}

fn parse_embedding_usage(value: Option<&Value>) -> Result<Option<Usage>, ProviderError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let input = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    Usage::embedding(crate::domain::inference::TokenCounter::exact(input))
        .map(Some)
        .map_err(|_| protocol_error())
}

struct SseState {
    started: bool,
    done: bool,
    model: Option<String>,
    request_id: Option<String>,
    seen_choices: HashSet<u32>,
    finished: HashSet<u32>,
    usage: Option<UsageSnapshot>,
}

fn azure_openai_sse_stream<S>(
    bytes: S,
    request_id: Option<String>,
    max_bytes: usize,
) -> impl Stream<Item = Result<ChatStreamEvent, ProviderError>> + Send + 'static
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let error_request_id = request_id.clone();
        let mut bytes = Box::pin(bytes);
        let mut pending = Vec::<u8>::new(); let mut data_lines = Vec::<String>::new(); let mut total = 0usize;
        let mut state = SseState {
            started: false,
            done: false,
            model: None,
            request_id,
            seen_choices: HashSet::new(),
            finished: HashSet::new(),
            usage: None,
        };
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk { Ok(chunk) => chunk, Err(error) => { yield Err(sse_error(request_error(error), error_request_id.as_deref())); return; } };
            total = total.saturating_add(chunk.len());
            if total > max_bytes { yield Err(sse_error(protocol_error(), error_request_id.as_deref())); return; }
            pending.extend_from_slice(&chunk);
            while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                let line = pending.drain(..=position).collect::<Vec<_>>();
                let line = match String::from_utf8(line[..line.len()-1].to_vec()) {
                    Ok(line) => line.trim_end_matches('\r').to_owned(),
                    Err(_) => { yield Err(sse_error(protocol_error(), error_request_id.as_deref())); return; }
                };
                if line.is_empty() { if !data_lines.is_empty() { let data = data_lines.join("\n"); data_lines.clear(); for event in process_sse_data(&data, &mut state) { match event { Ok(event) => { let done = event.is_terminal(); yield Ok(event); if done { return; } }, Err(error) => { yield Err(sse_error(error, error_request_id.as_deref())); return; } } } } }
                else if let Some(data) = line.strip_prefix("data:") { data_lines.push(data.strip_prefix(' ').unwrap_or(data).to_owned()); }
            }
        }
        if !pending.is_empty() { yield Err(sse_error(protocol_error(), error_request_id.as_deref())); return; }
        if !data_lines.is_empty() { let data = data_lines.join("\n"); for event in process_sse_data(&data, &mut state) { match event { Ok(event) => yield Ok(event), Err(error) => { yield Err(sse_error(error, error_request_id.as_deref())); return; } } } }
        if !state.done { yield Err(sse_error(protocol_error(), error_request_id.as_deref())); }
    }
}

fn process_sse_data(
    data: &str,
    state: &mut SseState,
) -> Vec<Result<ChatStreamEvent, ProviderError>> {
    if data == "[DONE]" {
        if !state.started || state.seen_choices.is_empty() || state.seen_choices != state.finished {
            return vec![Err(protocol_error())];
        }
        let completion = ResponseCompletion::new(
            state.request_id.clone(),
            state.model.clone().unwrap_or_default(),
            state.usage,
        )
        .map_err(|_| protocol_error());
        return match completion {
            Ok(completion) => {
                state.done = true;
                vec![Ok(ChatStreamEvent::ResponseCompleted(completion))]
            }
            Err(error) => vec![Err(error)],
        };
    }
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(_) => return vec![Err(protocol_error())],
    };
    if value.get("error").is_some() {
        return vec![Err(parse_stream_error(&value))];
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if model.is_empty() {
        return vec![Err(protocol_error())];
    }
    if state.model.as_deref().is_some_and(|old| old != model) {
        return vec![Err(protocol_error())];
    }
    state.model = Some(model.to_owned());
    let mut events = Vec::new();
    if !state.started {
        state.started = true;
        match ResponseStarted::new(
            state.request_id.clone(),
            model.to_owned(),
            response_metadata(&value),
        ) {
            Ok(started) => events.push(Ok(ChatStreamEvent::ResponseStarted(started))),
            Err(_) => return vec![Err(protocol_error())],
        }
    }
    match parse_usage(value.get("usage")) {
        Ok(Some(usage)) => match UsageSnapshot::final_(usage) {
            Ok(snapshot) => {
                state.usage = Some(snapshot);
                events.push(Ok(ChatStreamEvent::Usage(UsageSnapshot::interim(usage))));
            }
            Err(_) => return vec![Err(protocol_error())],
        },
        Ok(None) => {}
        Err(error) => return vec![Err(error)],
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let index = match choice
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
            {
                Some(index) => index,
                None => return vec![Err(protocol_error())],
            };
            state.seen_choices.insert(index);
            if let Some(delta) = choice.get("delta") {
                let content = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let refusal = delta
                    .get("refusal")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if let Some(role) = delta.get("role") {
                    if parse_role(role.as_str()).is_err() {
                        return vec![Err(protocol_error())];
                    }
                }
                let annotations = match parse_annotations(delta.get("annotations")) {
                    Ok(annotations) => annotations,
                    Err(error) => return vec![Err(error)],
                };
                if content.is_some() || refusal.is_some() {
                    events.push(Ok(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                        choice_index: index,
                        content_delta: content,
                        refusal_delta: refusal,
                        annotations,
                    })));
                } else if !annotations.is_empty() {
                    events.push(Ok(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                        choice_index: index,
                        content_delta: None,
                        refusal_delta: None,
                        annotations,
                    })));
                }
                if let Some(tools) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tool in tools {
                        let tool_call_index = match tool
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                        {
                            Some(index) => index,
                            None => return vec![Err(protocol_error())],
                        };
                        let function = match tool.get("function") {
                            Some(function) => function,
                            None => return vec![Err(protocol_error())],
                        };
                        events.push(Ok(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                            choice_index: index,
                            tool_call_index,
                            id: tool
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            name: function
                                .get("name")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            arguments_delta: function
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        })));
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                match ChoiceFinished::new(index, FinishReason::new(reason.to_owned())) {
                    Ok(finished) => {
                        state.finished.insert(index);
                        events.push(Ok(ChatStreamEvent::ChoiceFinished(finished)));
                    }
                    Err(_) => return vec![Err(protocol_error())],
                }
            }
        }
    }
    events
}

fn parse_stream_error(value: &Value) -> ProviderError {
    let error = value.get("error").unwrap_or(value);
    let provider_code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let status = error
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok());
    let category = status
        .and_then(|status| StatusCode::from_u16(status).ok())
        .map(category_for_status)
        .unwrap_or(ProviderErrorCategory::Provider);
    ProviderError::from_parts(
        category,
        Some(PROVIDER.to_owned()),
        None,
        status,
        provider_code,
        None,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

fn category_for_status(status: StatusCode) -> ProviderErrorCategory {
    match status {
        StatusCode::UNAUTHORIZED => ProviderErrorCategory::Authentication,
        StatusCode::FORBIDDEN => ProviderErrorCategory::Authorization,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            ProviderErrorCategory::InvalidRequest
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorCategory::RateLimited,
        StatusCode::REQUEST_TIMEOUT => ProviderErrorCategory::Timeout,
        status if status.is_server_error() => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    }
}
fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    super::retry_after(headers)
}
fn request_error(error: reqwest::Error) -> ProviderError {
    // A connection that was never opened carried nothing to the provider.
    if error.is_connect() {
        return ProviderError::not_sent(if error.is_timeout() {
            ProviderErrorCategory::Timeout
        } else {
            ProviderErrorCategory::Unavailable
        });
    }
    let category = if error.is_timeout() {
        ProviderErrorCategory::Timeout
    } else {
        ProviderErrorCategory::Unavailable
    };
    ProviderError::new(category)
}

fn preserve_request_id(error: ProviderError, request_id: Option<&str>) -> ProviderError {
    let provider_request_id = request_id
        .map(ToOwned::to_owned)
        .or_else(|| error.provider_request_id().map(ToOwned::to_owned));
    ProviderError::from_parts(
        error.category(),
        error.provider().map(ToOwned::to_owned),
        provider_request_id,
        error.http_status(),
        error.provider_code().map(ToOwned::to_owned),
        error.retry_after(),
        error.partial_usage(),
    )
    .unwrap_or(error)
}

fn sse_error(error: ProviderError, request_id: Option<&str>) -> ProviderError {
    preserve_request_id(error, request_id)
}
fn protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Protocol)
}
fn internal_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, ProviderAttemptIdempotencyKey,
    };
    use futures::StreamExt;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    fn make_adapter(
        listener: TcpListener,
        response: String,
    ) -> (AzureOpenAiAdapter, mpsc::Receiver<String>) {
        let address = listener.local_addr().expect("listener address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let request = read_request(&mut stream);
            sender.send(request).expect("request receiver");
            stream.write_all(response.as_bytes()).expect("response");
        });
        let config = AzureOpenAiConfig {
            base_url: format!("http://{address}"),
            auth: crate::config::AzureAuth::ApiKey("test-api-key".to_owned()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-06-01".to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        (AzureOpenAiAdapter::new(&config).expect("adapter"), receiver)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).expect("request bytes");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let header_text = String::from_utf8_lossy(&bytes[..header_end]);
            let length = header_text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .or_else(|| line.strip_prefix("content-length:"))
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + length {
                break;
            }
        }
        String::from_utf8(bytes).expect("UTF-8 request")
    }

    fn context() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt-123").expect("key"),
            DeploymentModelReference::new("deployment", "gpt-deployment").expect("deployment"),
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_owned(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: Some(0.2),
            max_tokens: Some(12),
            ..Default::default()
        }
    }

    #[test]
    fn batch_lines_name_the_batch_deployment_on_azures_paths() {
        let (adapter, _) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            String::new(),
        );
        let tag = BatchTag {
            batch_id: uuid::Uuid::nil(),
            job_id: uuid::Uuid::nil(),
        };
        let chat = adapter
            .input_line(
                tag,
                3,
                BatchEndpoint::ChatCompletions,
                "gpt-batch",
                &LineRequest::Chat(request()),
            )
            .unwrap();
        let chat: Value = serde_json::from_str(&chat).unwrap();
        assert_eq!(chat["custom_id"], "3");
        assert_eq!(chat["url"], "/chat/completions");
        assert_eq!(chat["body"]["model"], "gpt-batch");
        assert_eq!(chat["body"]["stream"], false);
        let embed = adapter
            .input_line(
                tag,
                4,
                BatchEndpoint::Embeddings,
                "embed-batch",
                &LineRequest::Embeddings(EmbeddingRequest {
                    input: vec!["a".into()],
                }),
            )
            .unwrap();
        let embed: Value = serde_json::from_str(&embed).unwrap();
        assert_eq!(embed["url"], "/embeddings");
        let url = adapter.batch_url("batches/abc").unwrap();
        assert!(
            url.as_str()
                .ends_with("/openai/batches/abc?api-version=2024-10-21")
        );
        assert_eq!(path_segment("batch_1/../x"), "batch_1..x");
        assert!(adapter.batches().is_some());
    }

    #[tokio::test]
    async fn chat_count_is_unsupported_without_network() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let config = AzureOpenAiConfig {
            base_url: format!("http://{address}"),
            auth: crate::config::AzureAuth::ApiKey("test-api-key".to_owned()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-06-01".to_owned(),
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(50),
            max_response_bytes: 1024,
        };
        let adapter = AzureOpenAiAdapter::new(&config).expect("adapter");
        let error = adapter
            .count_chat_input_tokens(&context(), &request())
            .await
            .expect_err("Chat Completions must not count or generate");
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    }

    #[tokio::test]
    async fn chat_uses_deployment_path_api_version_and_api_key_header() {
        let body = r#"{"id":"chat-body-id","model":"gpt-deployment","choices":[{"index":0,"message":{"role":"assistant","content":"hello","refusal":"no","annotations":[{"type":"url_citation","url_citation":{"url":"https://example.invalid"}}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nApim-Request-Id: chat-header-id\r\n\r\n{}",
            body.len(),
            body
        );
        let (adapter, receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            response,
        );
        let context = context();
        let correlation = context.correlation_id.0.to_string();
        let provider_attempt = context.provider_attempt_idempotency_key.as_str().to_owned();
        let result = adapter.chat(context, request()).await.expect("chat");
        let wire = receiver.recv().expect("wire request");
        assert!(wire.contains(
            "POST /openai/deployments/gpt-deployment/chat/completions?api-version=2024-06-01 HTTP/1.1"
        ));
        assert!(wire.to_ascii_lowercase().contains("api-key: test-api-key"));
        assert!(!wire.to_ascii_lowercase().contains("authorization: "));
        assert!(
            wire.to_ascii_lowercase()
                .contains(&format!("idempotency-key: {provider_attempt}"))
        );
        assert!(
            wire.to_ascii_lowercase()
                .contains(&format!("x-correlation-id: {correlation}"))
        );
        assert!(
            wire.to_ascii_lowercase()
                .contains(&format!("x-client-request-id: {correlation}"))
        );
        assert!(wire.contains("\"model\":\"gpt-deployment\""));
        assert!(wire.contains("\"max_tokens\":12"));
        assert_eq!(result.provider_request_id(), Some("chat-header-id"));
        assert_eq!(result.choices()[0].message.content, "hello");
        assert_eq!(result.choices()[0].refusal.as_deref(), Some("no"));
        assert_eq!(result.choices()[0].annotations[0].kind, "url_citation");
        assert_eq!(
            serde_json::to_value(result.metadata()).expect("safe metadata")["azure_openai_response_id"],
            "chat-body-id"
        );
    }

    #[tokio::test]
    async fn stream_emits_usage_and_completes_only_at_done() {
        let body = concat!(
            "data: {\"id\":\"chat-stream-id\",\"model\":\"gpt-deployment\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\",\"annotations\":[{\"type\":\"url_citation\",\"value\":\"cite\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat-stream-id\",\"model\":\"gpt-deployment\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chat-stream-id\",\"model\":\"gpt-deployment\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            response,
        );
        let events = adapter
            .chat_stream(context(), request())
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], Ok(ChatStreamEvent::ResponseStarted(_))));
        assert!(matches!(events[1], Ok(ChatStreamEvent::ChoiceDelta(_))));
        assert!(matches!(events[2], Ok(ChatStreamEvent::ChoiceFinished(_))));
        assert!(matches!(events[3], Ok(ChatStreamEvent::Usage(_))));
        assert!(matches!(
            events[4],
            Ok(ChatStreamEvent::ResponseCompleted(_))
        ));
    }

    #[tokio::test]
    async fn rate_limit_error_is_mapped_without_retry() {
        let body = r#"{"error":{"type":"rate_limit_exceeded","code":"rate_limit_exceeded"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 3\r\nApim-Request-Id: rate-limit-id\r\n\r\n{}",
            body.len(),
            body
        );
        let (adapter, receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            response,
        );
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("rate limit");
        let _wire = receiver.recv().expect("single request");
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        assert_eq!(error.provider_request_id(), Some("rate-limit-id"));
        assert_eq!(error.http_status(), Some(429));
        assert_eq!(error.provider_code(), Some("rate_limit_exceeded"));
    }

    #[tokio::test]
    async fn embeddings_map_embedding_usage_and_request_shape() {
        let body = r#"{"id":"embedding-body-id","model":"embed-deployment","data":[{"index":0,"embedding":[0.25,-0.5]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nApim-Request-Id: embedding-header-id\r\n\r\n{}",
            body.len(),
            body
        );
        let (adapter, receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            response,
        );
        let result = adapter
            .embeddings(
                context(),
                EmbeddingRequest {
                    input: vec!["hello".to_owned()],
                },
            )
            .await
            .expect("embedding response");
        let wire = receiver.recv().expect("wire request");
        assert!(wire.contains(
            "POST /openai/deployments/gpt-deployment/embeddings?api-version=2024-06-01 HTTP/1.1"
        ));
        assert!(wire.contains("\"model\":\"gpt-deployment\""));
        assert!(wire.contains("\"input\":[\"hello\"]"));
        assert_eq!(result.provider_request_id(), Some("embedding-header-id"));
        assert_eq!(result.model(), "embed-deployment");
        assert_eq!(result.data()[0].vector(), &[0.25, -0.5]);
        assert_eq!(
            result
                .usage()
                .expect("usage")
                .usage()
                .input_tokens()
                .unwrap()
                .value(),
            Some(4)
        );
        assert_eq!(
            result
                .usage()
                .expect("usage")
                .usage()
                .output_tokens()
                .unwrap()
                .value(),
            Some(0)
        );
    }

    #[test]
    fn malformed_roles_and_stream_tool_indices_are_protocol_errors() {
        let malformed = json!({
            "model": "gpt-deployment",
            "choices": [{
                "index": 0,
                "message": {"content": "missing role"},
                "finish_reason": "stop"
            }]
        });
        assert_eq!(
            parse_chat_response(malformed, None).unwrap_err().category(),
            ProviderErrorCategory::Protocol
        );

        let mut state = SseState {
            started: false,
            done: false,
            model: None,
            request_id: None,
            seen_choices: HashSet::new(),
            finished: HashSet::new(),
            usage: None,
        };
        let events = process_sse_data(
            r#"{"id":"id","model":"gpt-deployment","choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#,
            &mut state,
        );
        assert!(
            matches!(events.last(), Some(Err(error)) if error.category() == ProviderErrorCategory::Protocol)
        );
    }

    #[test]
    fn endpoint_percent_encodes_deployment_and_appends_api_version() {
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            String::new(),
        );
        let url = adapter.endpoint("dep:name/v1", "chat/completions").unwrap();
        assert_eq!(
            url.path(),
            "/openai/deployments/dep%3Aname%2Fv1/chat/completions"
        );
        assert_eq!(url.query(), Some("api-version=2024-06-01"));
    }

    #[test]
    fn endpoint_rejects_control_and_reserved_characters() {
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            String::new(),
        );
        for deployment in [
            "",
            "bad deployment",
            "bad\ndeployment",
            "bad?dep",
            "bad#dep",
        ] {
            assert_eq!(
                adapter
                    .endpoint(deployment, "chat/completions")
                    .unwrap_err()
                    .category(),
                ProviderErrorCategory::Internal
            );
        }
    }

    #[tokio::test]
    async fn stream_eof_and_in_stream_errors_preserve_header_request_id() {
        let eof_body = "data: {\"id\":\"body-id\",\"model\":\"gpt-deployment\",\"choices\":[]}";
        let eof_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nApim-Request-Id: eof-header-id\r\n\r\n{}",
            eof_body.len(),
            eof_body
        );
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            eof_response,
        );
        let events = adapter
            .chat_stream(context(), request())
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        let error = events
            .last()
            .expect("EOF error")
            .as_ref()
            .expect_err("protocol error");
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
        assert_eq!(error.provider_request_id(), Some("eof-header-id"));

        let error_body = "data: {\"error\":{\"type\":\"upstream_failed\"}}\n\n";
        let error_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nApim-Request-Id: stream-error-id\r\n\r\n{}",
            error_body.len(),
            error_body
        );
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            error_response,
        );
        let events = adapter
            .chat_stream(context(), request())
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        let error = events
            .last()
            .expect("in-stream error")
            .as_ref()
            .expect_err("provider error");
        assert_eq!(error.category(), ProviderErrorCategory::Provider);
        assert_eq!(error.provider_request_id(), Some("stream-error-id"));
        assert_eq!(error.provider_code(), Some("upstream_failed"));
    }

    #[tokio::test]
    async fn malformed_post_header_response_preserves_request_id() {
        let body = "not-json";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nApim-Request-Id: malformed-header-id\r\n\r\n{}",
            body.len(),
            body
        );
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            response,
        );
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("malformed response");
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
        assert_eq!(error.provider_request_id(), Some("malformed-header-id"));
    }

    #[test]
    fn percent_encode_path_segment_escapes_reserved_bytes() {
        assert_eq!(percent_encode_path_segment("a b/c"), "a%20b%2Fc");
        assert_eq!(percent_encode_path_segment("safe-._~1"), "safe-._~1");
    }

    #[tokio::test]
    async fn entra_auth_sends_a_bearer_token_and_a_refusal_is_not_sent() {
        use crate::azure_credentials::{COGNITIVE_SERVICES_SCOPE, tests};
        use crate::config::AzureAuth;
        let fake = tests::start().await;
        let identity = tests::secret_identity(&fake);
        let body = r#"{"id":"c","model":"gpt-deployment","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            sender.send(read_request(&mut stream)).expect("receiver");
            stream.write_all(response.as_bytes()).expect("response");
        });
        let config = |auth| AzureOpenAiConfig {
            base_url: format!("http://{address}"),
            auth,
            identity: Some(identity.clone()),
            management: None,
            api_version: "2024-06-01".to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        let adapter = AzureOpenAiAdapter::new(&config(AzureAuth::Entra(
            identity.token(COGNITIVE_SERVICES_SCOPE),
        )))
        .expect("adapter");
        adapter.chat(context(), request()).await.expect("chat");
        let wire = receiver.recv().expect("wire request").to_ascii_lowercase();
        assert!(wire.contains("authorization: bearer token-1"), "{wire}");
        assert!(!wire.contains("api-key:"));

        // A token Entra refuses: nothing is sent, and it is an authentication
        // failure.
        fake.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let refused = AzureOpenAiAdapter::new(&config(AzureAuth::Entra(
            identity
                .in_tenant(
                    "00000000-0000-0000-0000-000000000001",
                    None,
                    COGNITIVE_SERVICES_SCOPE,
                )
                .expect("a secret signs in elsewhere"),
        )))
        .expect("adapter");
        let error = refused.chat(context(), request()).await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Authentication);
        assert!(error.was_not_sent());
    }

    #[test]
    fn tools_and_tool_calls_go_on_the_wire_as_openai_documents() {
        use crate::domain::inference::{ChatMessage, ChatRole, ToolChoice};
        let mut request = crate::provider::tools::tool_conversation();
        request.tool_choice = Some(ToolChoice::Required);
        request.parallel_tool_calls = Some(false);
        let body = serde_json::to_value(ChatWireRequest::from_domain(&context(), &request, false))
            .unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "weather in Paris and Rome?"},
                {"role": "assistant", "content": "Checking.", "tool_calls": [
                    {"id": "toolu_01", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                    {"id": "toolu_02", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}
                ]},
                {"role": "tool", "content": "18C", "tool_call_id": "toolu_01"},
                {"role": "tool", "content": "24C", "tool_call_id": "toolu_02"}
            ])
        );
        assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["parallel_tool_calls"], false);
        // An assistant message that only calls tools has null content.
        request.messages[2].content.clear();
        let body = serde_json::to_value(ChatWireRequest::from_domain(&context(), &request, false))
            .unwrap();
        assert!(body["messages"][2]["content"].is_null());
        // A request without tools sends none of the fields.
        let plain = ChatRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "hi")],
            ..Default::default()
        };
        let body =
            serde_json::to_value(ChatWireRequest::from_domain(&context(), &plain, false)).unwrap();
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }
}
