use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::{Client, StatusCode, Url, header};
use serde_json::{Value, json};
use std::{fmt, time::Duration};

use super::{
    ChatEventStream, InferenceProviderAdapter, MAX_MODEL_LIST_PAGES, ValidatingChatEventStream,
    batch::{
        BatchProvider, BatchTag, LineOutcome, LineResponse, MAX_BATCH_FILE_BYTES, MAX_FIND_PAGES,
        ProviderBatch, ProviderBatchStatus,
    },
    model_list::{PageShape, next_token, push_page, too_many_pages},
};
use crate::domain::batch::{BatchEndpoint, LineRequest};
use crate::{
    config::{AnthropicConfig, AzureAuth, GeminiAuth},
    domain::discovery::{DiscoveredModel, ModelListBuilder},
    domain::inference::{
        ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent,
        ChoiceFinished, FinishReason, InferenceContext, ProviderError, ProviderErrorCategory,
        ResponseCompletion, ResponseStarted, SafeMetadata, StreamChoiceDelta, TokenCounter,
        ToolCall, ToolCallDelta, ToolChoice, Usage, UsageSnapshot,
    },
};

pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// The Messages API version Agent Platform takes, in the body.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// How requests are signed: Anthropic's API key, or, for Claude on Azure AI
/// Foundry, a bearer token (the resource's key or an Entra token), or, for
/// Claude on Agent Platform, a Google token.
enum AnthropicAuth {
    ApiKey(String),
    AzureBearer(AzureAuth),
    Google(GeminiAuth),
}

pub(crate) struct AnthropicAdapter {
    client: Client,
    base_url: Url,
    auth: AnthropicAuth,
    /// Claude on Agent Platform (Model Garden): the Messages API through
    /// `rawPredict` and `streamRawPredict` on the model's own URL under
    /// `base_url` (`.../publishers/anthropic/`), with the version in the
    /// body and no `model` field.
    vertex: bool,
    max_response_bytes: usize,
    request_timeout: Duration,
    idle_read_timeout: Duration,
}

impl fmt::Debug for AnthropicAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicAdapter")
            .field("base_url", &self.base_url)
            .field(
                "auth",
                &match self.auth {
                    AnthropicAuth::ApiKey(_) => "api key (REDACTED)",
                    AnthropicAuth::AzureBearer(_) => "Azure bearer (REDACTED)",
                    AnthropicAuth::Google(_) => "Google (REDACTED)",
                },
            )
            .field("api_version", &ANTHROPIC_API_VERSION)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl AnthropicAdapter {
    pub(crate) fn new(config: &AnthropicConfig) -> Result<Self, ProviderError> {
        Self::with_auth(config, AnthropicAuth::ApiKey(config.api_key.clone()))
    }

    /// Claude on an Azure AI Foundry resource: the Messages API under
    /// `{resource}/anthropic/`, signed with the resource's bearer token.
    pub(crate) fn for_azure_ai_foundry(
        config: &AnthropicConfig,
        auth: AzureAuth,
    ) -> Result<Self, ProviderError> {
        Self::with_auth(config, AnthropicAuth::AzureBearer(auth))
    }

    /// Claude on Agent Platform: `config.base_url` is the project and
    /// location's `.../publishers/anthropic` URL, signed with the Gemini
    /// account's Google token (an API key cannot call partner models).
    pub(crate) fn for_agent_platform(
        config: &AnthropicConfig,
        auth: GeminiAuth,
    ) -> Result<Self, ProviderError> {
        Self::with_auth(config, AnthropicAuth::Google(auth))
    }

    fn with_auth(config: &AnthropicConfig, auth: AnthropicAuth) -> Result<Self, ProviderError> {
        let base_url = Url::parse(&format!("{}/", config.base_url.trim_end_matches('/')))
            .map_err(|_| protocol_error())?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.request_timeout)
            .build()
            .map_err(|_| protocol_error())?;
        Ok(Self {
            client,
            base_url,
            vertex: matches!(auth, AnthropicAuth::Google(_)),
            auth,
            max_response_bytes: config.max_response_bytes,
            request_timeout: config.request_timeout,
            idle_read_timeout: config.request_timeout,
        })
    }

    fn endpoint(&self, model: &str, stream: bool) -> Result<Url, ProviderError> {
        if self.vertex {
            if model.is_empty() || model.contains(['/', '?', '#', ' ', ':', '\n', '\r']) {
                return Err(protocol_error());
            }
            let method = if stream {
                "streamRawPredict"
            } else {
                "rawPredict"
            };
            return self
                .base_url
                .join(&format!("models/{model}:{method}"))
                .map_err(|_| protocol_error());
        }
        self.base_url
            .join("v1/messages")
            .map_err(|_| protocol_error())
    }

    fn count_endpoint(&self) -> Result<Url, ProviderError> {
        // Agent Platform counts every Claude model at one free model,
        // `count-tokens`, which the body names the model to count for.
        let path = if self.vertex {
            "models/count-tokens:rawPredict"
        } else {
            "v1/messages/count_tokens"
        };
        self.base_url.join(path).map_err(|_| protocol_error())
    }

    /// A refused call's error. Agent Platform answers Google's error
    /// envelope (quota, permission, Model Garden terms), which says more
    /// than the status.
    async fn error(&self, response: reqwest::Response) -> ProviderError {
        if self.vertex {
            return super::gemini::response_error(response, self.idle_read_timeout).await;
        }
        status_error(response.status(), response.headers())
    }

    /// The signing header and the API version, on a request.
    async fn signed(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let builder = match &self.auth {
            AnthropicAuth::ApiKey(api_key) => {
                let mut value =
                    header::HeaderValue::from_str(api_key).map_err(|_| protocol_error())?;
                value.set_sensitive(true);
                builder.header("x-api-key", value)
            }
            AnthropicAuth::AzureBearer(auth) => builder.header(
                header::AUTHORIZATION,
                super::azure_bearer(auth, self.request_timeout).await?,
            ),
            // The version is in the body (`anthropic_version`).
            AnthropicAuth::Google(auth) => {
                return super::gemini::google_authorize(auth, builder, self.request_timeout).await;
            }
        };
        Ok(builder.header("anthropic-version", ANTHROPIC_API_VERSION))
    }

    /// Pages through `GET v1/models`, following `last_id` while `has_more`.
    async fn models_page(&self, after_id: Option<&str>) -> Result<Value, ProviderError> {
        let mut url = self
            .base_url
            .join("v1/models")
            .map_err(|_| protocol_error())?;
        url.query_pairs_mut().append_pair("limit", "1000");
        if let Some(after_id) = after_id {
            url.query_pairs_mut().append_pair("after_id", after_id);
        }
        let response = send_with_timeout(
            self.signed(
                self.client
                    .get(url)
                    .header(header::ACCEPT, "application/json"),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(status_error(response.status(), response.headers()));
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    fn request_body(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<Value, ProviderError> {
        let mut body = message_params(context.deployment.model(), request)?;
        body["stream"] = Value::Bool(stream);
        if self.vertex {
            let object = body.as_object_mut().ok_or_else(protocol_error)?;
            object.remove("model");
            object.insert(
                "anthropic_version".to_owned(),
                Value::String(VERTEX_ANTHROPIC_VERSION.to_owned()),
            );
        }
        Ok(body)
    }

    async fn request(
        &self,
        stream: bool,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let body = self.request_body(context, request, stream)?;
        Ok(self
            .signed(
                self.client
                    .post(self.endpoint(context.deployment.model(), stream)?)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(
                        header::ACCEPT,
                        if stream {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    ),
            )
            .await?
            .json(&body))
    }
}

impl AnthropicAdapter {
    fn batches_url(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url
            .join(&format!("v1/messages/batches{path}"))
            .map_err(|_| protocol_error())
    }

    async fn batch_send(
        &self,
        builder: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = send_with_timeout(self.signed(builder).await?, timeout).await?;
        if !response.status().is_success() {
            return Err(status_error(response.status(), response.headers()));
        }
        Ok(response)
    }

    async fn batch_json(&self, builder: reqwest::RequestBuilder) -> Result<Value, ProviderError> {
        let response = self
            .batch_send(
                builder.header(header::ACCEPT, "application/json"),
                self.request_timeout,
            )
            .await?;
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    /// The first result of an ended batch, enough to tell whose it is.
    async fn first_result(&self, id: &str) -> Result<Option<Value>, ProviderError> {
        let url = self.batches_url(&format!("/{}/results", path_segment(id)))?;
        let response = self
            .batch_send(self.client.get(url), self.request_timeout)
            .await?;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::time::timeout(self.idle_read_timeout, stream.next())
            .await
            .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
        {
            bytes.extend_from_slice(&chunk.map_err(network_error)?);
            if let Some(end) = bytes.iter().position(|byte| *byte == b'\n') {
                bytes.truncate(end);
                break;
            }
            if bytes.len() > MAX_FIRST_RESULT_BYTES {
                return Err(protocol_error());
            }
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| protocol_error())
    }
}

/// Anthropic's Message Batches: the requests go in the create call, each
/// with a `custom_id` naming the job and line, and an ended batch's
/// results stream back keyed by it.
#[async_trait]
impl BatchProvider for AnthropicAdapter {
    /// Batches carry no metadata, so a job created before a crash is found
    /// among the batches created since the worker recorded it: one holding
    /// as many requests, whose results name the job. A batch that could be
    /// the job but has not ended cannot be read yet, so the search is an
    /// error until it ends, rather than proving there is none.
    async fn find_batch(
        &self,
        tag: BatchTag,
        input_file_id: &str,
    ) -> Result<Option<ProviderBatch>, ProviderError> {
        let input = InlineInput::parse(input_file_id).ok_or_else(protocol_error)?;
        let since = input.recorded_at - BATCH_CLOCK_SKEW_SECONDS;
        let prefix = custom_id_prefix(tag);
        let mut unconfirmed = false;
        let mut after: Option<String> = None;
        for _ in 0..MAX_FIND_PAGES {
            let mut url = self.batches_url("")?;
            url.query_pairs_mut().append_pair("limit", "100");
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let page = self.batch_json(self.client.get(url)).await?;
            let data = page
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(protocol_error)?;
            let mut older = false;
            for batch in data {
                let created = batch
                    .get("created_at")
                    .and_then(Value::as_str)
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .ok_or_else(protocol_error)?
                    .timestamp();
                if created < since {
                    older = true;
                    break;
                }
                if request_total(batch) != Some(input.requests) {
                    continue;
                }
                let found = message_batch(batch)?;
                if found.output_file_id.is_none() {
                    unconfirmed = true;
                    continue;
                }
                let ours = self.first_result(&found.id).await?.is_some_and(|result| {
                    result
                        .get("custom_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with(&prefix))
                });
                if ours {
                    return Ok(Some(found));
                }
            }
            if older || page.get("has_more").and_then(Value::as_bool) != Some(true) {
                return if unconfirmed {
                    Err(ProviderError::new(ProviderErrorCategory::Unavailable))
                } else {
                    Ok(None)
                };
            }
            after = Some(
                page.get("last_id")
                    .and_then(Value::as_str)
                    .ok_or_else(protocol_error)?
                    .to_owned(),
            );
        }
        Err(protocol_error())
    }

    /// Nothing is uploaded: the reference records when, and how many
    /// requests, for [`Self::find_batch`].
    async fn upload_batch_input(
        &self,
        _tag: BatchTag,
        jsonl: Vec<u8>,
    ) -> Result<String, ProviderError> {
        let requests = jsonl
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .count();
        Ok(InlineInput {
            recorded_at: chrono::Utc::now().timestamp(),
            requests: u64::try_from(requests).map_err(|_| protocol_error())?,
        }
        .reference())
    }

    fn inline_input(&self) -> bool {
        true
    }

    async fn create_batch(
        &self,
        _input_file_id: &str,
        input: Option<&[u8]>,
        endpoint: BatchEndpoint,
        _tag: BatchTag,
        _model: &str,
    ) -> Result<ProviderBatch, ProviderError> {
        if endpoint != BatchEndpoint::ChatCompletions {
            return Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ));
        }
        let input = input.ok_or_else(protocol_error)?;
        let requests = input
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(serde_json::from_slice::<Value>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| protocol_error())?;
        let builder = self
            .client
            .post(self.batches_url("")?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .json(&json!({ "requests": requests }));
        let response = self
            .batch_send(builder, self.request_timeout.max(BATCH_CREATE_TIMEOUT))
            .await?;
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        message_batch(&serde_json::from_slice(&bytes).map_err(|_| protocol_error())?)
    }

    async fn batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let url = self.batches_url(&format!("/{}", path_segment(id)))?;
        message_batch(&self.batch_json(self.client.get(url)).await?)
    }

    async fn cancel_batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let url = self.batches_url(&format!("/{}/cancel", path_segment(id)))?;
        message_batch(&self.batch_json(self.client.post(url)).await?)
    }

    /// An ended batch's results (`id` is the batch's).
    async fn file_content(&self, id: &str) -> Result<Vec<u8>, ProviderError> {
        let url = self.batches_url(&format!("/{}/results", path_segment(id)))?;
        let response = self
            .batch_send(self.client.get(url), self.request_timeout)
            .await?;
        bounded_bytes(response, MAX_BATCH_FILE_BYTES, self.idle_read_timeout).await
    }

    fn input_line(
        &self,
        tag: BatchTag,
        line: u32,
        _endpoint: BatchEndpoint,
        model: &str,
        request: &LineRequest,
    ) -> Result<String, ProviderError> {
        let LineRequest::Chat(request) = request else {
            return Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ));
        };
        serde_json::to_string(&json!({
            "custom_id": custom_id(tag, line),
            "params": message_params(model, request)?,
        }))
        .map_err(|_| protocol_error())
    }

    fn parse_results(
        &self,
        _endpoint: BatchEndpoint,
        output: &[u8],
        _errors: &[u8],
    ) -> std::collections::HashMap<u32, LineOutcome> {
        let mut outcomes = std::collections::HashMap::new();
        for raw in String::from_utf8_lossy(output).lines() {
            let Ok(value) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let Some(line) = value
                .get("custom_id")
                .and_then(Value::as_str)
                .and_then(line_of)
            else {
                continue;
            };
            outcomes.insert(line, result_outcome(&value));
        }
        outcomes
    }
}

/// A provider id as one path segment.
fn path_segment(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

#[async_trait]
impl InferenceProviderAdapter for AnthropicAdapter {
    /// Message Batches, with Anthropic's own API key; Claude on Azure AI
    /// Foundry has no batch API.
    fn batches(&self) -> Option<&dyn BatchProvider> {
        matches!(self.auth, AnthropicAuth::ApiKey(_)).then_some(self as &dyn BatchProvider)
    }

    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        let mut body = self.request_body(context, request, false)?;
        let object = body.as_object_mut().ok_or_else(protocol_error)?;
        object.remove("max_tokens");
        object.remove("temperature");
        object.remove("stream");
        if self.vertex {
            // The count names the model it counts for, and no version.
            object.remove("anthropic_version");
            object.insert(
                "model".to_owned(),
                Value::String(context.deployment.model().to_owned()),
            );
        }

        let response = send_with_timeout(
            self.signed(
                self.client
                    .post(self.count_endpoint()?)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json"),
            )
            .await?
            .json(&body),
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_count_tokens(value)
    }

    fn validate_chat_request(
        &self,
        _context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        validate_text_request(request)
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let response = send_with_timeout(
            self.request(false, &context, &request).await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.error(response).await);
        }
        let request_id = response
            .headers()
            .get("request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_response(value, request_id)
    }

    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let response = send_with_timeout(
            self.request(true, &context, &request).await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.error(response).await);
        }
        let model = context.deployment.model().to_owned();
        let request_id = response
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = response.bytes_stream();
        let max = self.max_response_bytes;
        let stream = anthropic_sse_stream(bytes, model, request_id, max, self.idle_read_timeout);
        Ok(ValidatingChatEventStream::new(stream))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: crate::domain::inference::EmbeddingRequest,
    ) -> Result<crate::domain::inference::EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let mut models = ModelListBuilder::default();
        let mut after_id: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self.models_page(after_id.as_deref()).await?;
            push_page(&mut models, &page, &ANTHROPIC_MODEL_PAGE)?;
            // A page that does not say whether more follow may be partial.
            let has_more = page
                .get("has_more")
                .and_then(Value::as_bool)
                .ok_or_else(protocol_error)?;
            if !has_more {
                return Ok(models.finish());
            }
            // A page claiming more without a cursor cannot be continued.
            after_id = Some(
                next_token(&page, "last_id")
                    .ok_or_else(protocol_error)?
                    .to_owned(),
            );
        }
        Err(too_many_pages())
    }
}

/// The Messages API parameters of a request to `model`, not streamed.
/// Tools map to Anthropic's own (D-032): definitions to `tools` with their
/// schema as `input_schema`, an assistant message's calls to `tool_use`
/// blocks after its text, and the `tool` messages that answer them to one
/// user message of `tool_result` blocks, as Anthropic requires.
fn message_params(model: &str, request: &ChatRequest) -> Result<Value, ProviderError> {
    validate_text_request(request)?;
    let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len());
    let mut system = Vec::new();
    let mut results_open = false;
    for message in &request.messages {
        let is_result = message.role == ChatRole::Tool;
        match message.role {
            ChatRole::System => system.push(message.content.clone()),
            ChatRole::User => messages.push(json!({"role":"user","content":message.content})),
            ChatRole::Assistant if message.tool_calls.is_empty() => {
                messages.push(json!({"role":"assistant","content":message.content}))
            }
            ChatRole::Assistant => {
                let mut content = Vec::with_capacity(message.tool_calls.len() + 1);
                if !message.content.is_empty() {
                    content.push(json!({"type": "text", "text": message.content}));
                }
                for call in &message.tool_calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id.as_deref().ok_or_else(invalid_request)?,
                        "name": call.name.as_deref().ok_or_else(invalid_request)?,
                        "input": super::tool_arguments(&call.arguments)?,
                    }));
                }
                messages.push(json!({"role": "assistant", "content": content}));
            }
            ChatRole::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().ok_or_else(invalid_request)?,
                    "content": message.content,
                });
                match messages.last_mut() {
                    Some(last) if results_open => last["content"]
                        .as_array_mut()
                        .ok_or_else(invalid_request)?
                        .push(block),
                    _ => messages.push(json!({"role": "user", "content": [block]})),
                }
            }
            ChatRole::Developer => return Err(invalid_request()),
        }
        results_open = is_result;
    }
    if messages.is_empty() {
        return Err(invalid_request());
    }
    let mut body = json!({
        "model": model,
        "max_tokens": request.max_tokens.unwrap_or(4096),
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = Value::String(system.join("\n"));
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    let mut definition = json!({
                        "name": tool.name,
                        "input_schema": tool.parameters,
                    });
                    if let Some(description) = &tool.description {
                        definition["description"] = json!(description);
                    }
                    definition
                })
                .collect(),
        );
        let mut choice = match &request.tool_choice {
            None | Some(ToolChoice::Auto) => json!({"type": "auto"}),
            Some(ToolChoice::None) => json!({"type": "none"}),
            Some(ToolChoice::Required) => json!({"type": "any"}),
            Some(ToolChoice::Function(name)) => json!({"type": "tool", "name": name}),
        };
        if request.parallel_tool_calls == Some(false)
            && request.tool_choice != Some(ToolChoice::None)
        {
            choice["disable_parallel_tool_use"] = Value::Bool(true);
        }
        if request.tool_choice.is_some() || request.parallel_tool_calls == Some(false) {
            body["tool_choice"] = choice;
        }
    }
    Ok(body)
}

/// Anthropic's stop reason in the contract's terms: a stop to call tools is
/// `tool_calls` (D-032); every other reason is passed on as it is.
fn finish_reason_of(reason: &str) -> FinishReason {
    FinishReason::new(if reason == "tool_use" {
        "tool_calls"
    } else {
        reason
    })
}

/// How far a batch's `created_at`, on Anthropic's clock, may precede the
/// time the worker recorded before creating it, on the gateway's.
const BATCH_CLOCK_SKEW_SECONDS: i64 = 600;
/// The longest the create call, which carries every request, may take.
const BATCH_CREATE_TIMEOUT: Duration = Duration::from_secs(300);
/// The most of a results stream read to recognize a batch as the job's.
const MAX_FIRST_RESULT_BYTES: usize = 4 * 1024 * 1024;

/// What the worker records for a job before creating it: Message Batches
/// take the requests in the create call, so nothing is uploaded, but the
/// search for a job created before a crash needs when it was created and
/// how many requests it holds.
#[derive(Debug, PartialEq, Eq)]
struct InlineInput {
    recorded_at: i64,
    requests: u64,
}

impl InlineInput {
    const PREFIX: &'static str = "inline-";

    fn reference(&self) -> String {
        format!("{}{}-{}", Self::PREFIX, self.recorded_at, self.requests)
    }

    fn parse(reference: &str) -> Option<Self> {
        let (at, requests) = reference.strip_prefix(Self::PREFIX)?.split_once('-')?;
        Some(Self {
            recorded_at: at.parse().ok()?,
            requests: requests.parse().ok()?,
        })
    }
}

/// A request's `custom_id`: the job, so a batch is recognized as the job's
/// from any of its results, and the line.
fn custom_id(tag: BatchTag, line: u32) -> String {
    format!("{}{line}", custom_id_prefix(tag))
}

fn custom_id_prefix(tag: BatchTag) -> String {
    format!("gw-{}-", tag.job_id.simple())
}

fn line_of(custom_id: &str) -> Option<u32> {
    let rest = custom_id.strip_prefix("gw-")?;
    let (_, line) = rest.rsplit_once('-')?;
    line.parse().ok()
}

/// A Message Batch as a provider job. An ended batch's results are its
/// output; it has no error file.
fn message_batch(value: &Value) -> Result<ProviderBatch, ProviderError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(protocol_error)?
        .to_owned();
    let processing = value
        .get("processing_status")
        .and_then(Value::as_str)
        .ok_or_else(protocol_error)?;
    let (status, raw_status) = match processing {
        "in_progress" => (ProviderBatchStatus::Running, "in_progress"),
        "canceling" => (ProviderBatchStatus::Cancelling, "canceling"),
        "ended" => {
            let cancelled = value
                .get("cancel_initiated_at")
                .is_some_and(|at| !at.is_null());
            let expired = value
                .pointer("/request_counts/expired")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0;
            if cancelled {
                (ProviderBatchStatus::Cancelled, "cancelled")
            } else if expired {
                (ProviderBatchStatus::Expired, "expired")
            } else {
                (ProviderBatchStatus::Completed, "ended")
            }
        }
        _ => return Err(protocol_error()),
    };
    let ended = processing == "ended";
    Ok(ProviderBatch {
        output_file_id: ended.then(|| id.clone()),
        id,
        status,
        raw_status: raw_status.to_owned(),
        error_file_id: None,
    })
}

/// All requests of a batch, as its request counts say.
fn request_total(value: &Value) -> Option<u64> {
    let counts = value.get("request_counts")?.as_object()?;
    counts
        .values()
        .try_fold(0_u64, |total, count| total.checked_add(count.as_u64()?))
}

fn result_outcome(value: &Value) -> LineOutcome {
    let result = value.get("result");
    match result.and_then(|r| r.get("type")).and_then(Value::as_str) {
        Some("succeeded") => {
            let Some(message) = result.and_then(|r| r.get("message")).cloned() else {
                return LineOutcome::Unreadable;
            };
            match parse_response(message, None) {
                Ok(response) => LineOutcome::Succeeded {
                    request_id: response.provider_request_id().map(str::to_owned),
                    response: Box::new(LineResponse::Chat(response)),
                },
                Err(_) => LineOutcome::Unreadable,
            }
        }
        // Anthropic does not bill a request that errored.
        Some("errored") => LineOutcome::Failed {
            status_code: None,
            code: result
                .and_then(|r| r.pointer("/error/error/type"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            request_id: None,
        },
        Some("canceled" | "expired") => LineOutcome::NotRun,
        _ => LineOutcome::Unreadable,
    }
}

const ANTHROPIC_MODEL_PAGE: PageShape = PageShape {
    items: "data",
    id: "id",
    display_name: Some("display_name"),
    id_prefix: None,
    omits_empty: false,
};

fn parse_count_tokens(value: Value) -> Result<u64, ProviderError> {
    let object = value.as_object().ok_or_else(protocol_error)?;
    if object.len() != 1 {
        return Err(protocol_error());
    }
    object
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)
}

pub(super) fn validate_text_request(request: &ChatRequest) -> Result<(), ProviderError> {
    let mut system_seen = false;
    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            ChatRole::System if index == 0 && !system_seen => system_seen = true,
            ChatRole::System => return Err(invalid_request()),
            ChatRole::Developer => return Err(invalid_request()),
            ChatRole::User | ChatRole::Assistant | ChatRole::Tool => {}
        }
    }
    if request.messages.is_empty() || request.messages.iter().all(|m| m.role == ChatRole::System) {
        return Err(invalid_request());
    }
    Ok(())
}

/// Extended thinking blocks, which are not relayed: the answer is the text
/// that follows them, and their tokens are counted in `output_tokens`.
fn is_thinking(kind: &str) -> bool {
    matches!(kind, "thinking" | "redacted_thinking")
}

fn parse_response(value: Value, request_id: Option<String>) -> Result<ChatResponse, ProviderError> {
    if value.get("type").and_then(Value::as_str) != Some("message")
        || value.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return Err(protocol_error());
    }
    let id = request_id.or_else(|| value.get("id").and_then(Value::as_str).map(str::to_owned));
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(protocol_error)?
        .to_owned();
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    // Extended thinking comes first as `thinking` (or `redacted_thinking`)
    // blocks; the answer is the text blocks and the `tool_use` blocks, its
    // tool calls (D-032). Any other block (a server tool's) is not one the
    // contract carries.
    if blocks
        .iter()
        .any(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block.get("text").and_then(Value::as_str).is_none(),
            Some("tool_use") => {
                block.get("id").and_then(Value::as_str).is_none()
                    || block.get("name").and_then(Value::as_str).is_none()
                    || !block.get("input").is_some_and(Value::is_object)
            }
            Some(kind) => !is_thinking(kind),
            None => true,
        })
    {
        return Err(protocol_error());
    }
    let tool_calls = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|block| {
            Ok(ToolCall {
                id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                arguments: serde_json::to_string(&block["input"]).map_err(|_| protocol_error())?,
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    let reason = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(finish_reason_of);
    let usage = parse_usage(value.get("usage").ok_or_else(protocol_error)?)?;
    let choice = ChatChoice {
        index: 0,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content: text,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        },
        finish_reason: reason,
        tool_calls,
        refusal: None,
        safety: vec![],
        annotations: vec![],
    };
    ChatResponse::new(
        id,
        model,
        vec![choice],
        Some(UsageSnapshot::final_(usage).map_err(|_| protocol_error())?),
        SafeMetadata::default(),
    )
    .map_err(|_| protocol_error())
}

/// Anthropic's `input_tokens` counts only the prompt not read from or
/// written to the prompt cache; the whole prompt is `input_tokens +
/// cache_read_input_tokens + cache_creation_input_tokens`
/// (<https://platform.claude.com/docs/en/build-with-claude/prompt-caching>).
/// The gateway's input counts cache reads (charged at the cached-input
/// price, the rest at the input price); cache writes are the `other`
/// counter, charged on top at the catalog's cache write price. The
/// `cache_creation` breakdown by TTL is not read: every write is charged at
/// the one price.
fn parse_usage(value: &Value) -> Result<Usage, ProviderError> {
    let input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let (read, write) = cache_counts(value)?;
    usage_of(input, output, read, write)
}

fn usage_of(input: u64, output: u64, read: u64, write: u64) -> Result<Usage, ProviderError> {
    Usage::complete(
        TokenCounter::exact(input.checked_add(read).ok_or_else(protocol_error)?),
        TokenCounter::exact(output),
        Some(TokenCounter::exact(read)),
        None,
        Some(TokenCounter::exact(write)),
    )
    .map_err(|_| protocol_error())
}

/// The cache reads and writes `value` reports: an absent (or null) count is
/// zero, a malformed one fails closed.
fn cache_counts(value: &Value) -> Result<(u64, u64), ProviderError> {
    validate_unsupported_usage(value)?;
    Ok((
        optional_count(value, "cache_read_input_tokens")?.unwrap_or(0),
        optional_count(value, "cache_creation_input_tokens")?.unwrap_or(0),
    ))
}

fn optional_count(value: &Value, field: &str) -> Result<Option<u64>, ProviderError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(count) => count.as_u64().map(Some).ok_or_else(protocol_error),
    }
}

/// Usage dimensions Anthropic does not report (thinking is counted in
/// `output_tokens`): any non-zero one fails closed.
fn validate_unsupported_usage(value: &Value) -> Result<(), ProviderError> {
    for field in ["cached_input_tokens", "reasoning_tokens", "other_tokens"] {
        if let Some(value) = value.get(field) {
            if value.as_u64() != Some(0) {
                return Err(protocol_error());
            }
        }
    }
    Ok(())
}

fn anthropic_sse_stream<S>(
    mut bytes: S,
    model: String,
    request_id: Option<String>,
    max: usize,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<ChatStreamEvent, ProviderError>> + Send + 'static
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    stream! {
        let mut pending = Vec::<u8>::new();
        let mut event_name = String::new();
        let mut data = String::new();
        let mut data_seen = false;
        let mut total = 0usize;
        let mut message_started = false;
        let mut response_started = false;
        let mut content_open = false;
        // Whether the open block is thinking, whose deltas are dropped.
        let mut thinking_open = false;
        // The open block's tool call, by its index among the response's
        // calls, and whether any of its arguments arrived (D-032).
        let mut tool_open: Option<(u32, bool)> = None;
        let mut tool_calls = 0u32;
        let mut message_delta_seen = false;
        let mut message_stop_seen = false;
        let mut input_tokens = None;
        let mut output_tokens = None;
        // Cache reads and writes: `message_start` reports them, and a
        // `message_delta` that repeats them has the final counts.
        let mut cache = (0u64, 0u64);
        let mut finish_reason = None;
        let mut provider_id = request_id;
        let mut terminal = false;
        loop {
            let Some(chunk) = (match tokio::time::timeout(idle_timeout, bytes.next()).await {
                Ok(chunk) => chunk,
                Err(_) => {
                    yield Err(ProviderError::new(ProviderErrorCategory::Timeout));
                    break;
                }
            }) else { break; };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => { yield Err(network_error_simple()); break; }
            };
            total = total.saturating_add(chunk.len());
            if total > max { yield Err(protocol_error()); break; }
            pending.extend_from_slice(&chunk);
            while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                let line = pending.drain(..=position).collect::<Vec<_>>();
                let line = match decode_sse_line(line) {
                    Ok(line) => line,
                    Err(error) => { yield Err(error); terminal = true; break; }
                };
                if line.is_empty() {
                    if event_name.is_empty() && data.is_empty() { continue; }
                    if event_name.is_empty() || !data_seen {
                        yield Err(protocol_error()); terminal = true; break;
                    }
                    let value: Value = match serde_json::from_str(&data) {
                        Ok(value) => value,
                        Err(_) => { yield Err(protocol_error()); terminal = true; break; }
                    };
                    let event_result: Result<Option<ChatStreamEvent>, ProviderError> = match event_name.as_str() {
                        "message_start" => {
                            if message_started { Err(protocol_error()) } else {
                                let message = value.get("message").ok_or_else(protocol_error)?;
                                let usage = message.get("usage").ok_or_else(protocol_error)?;
                                cache = cache_counts(usage)?;
                                let input = usage.get("input_tokens").and_then(Value::as_u64).ok_or_else(protocol_error)?;
                                input_tokens = Some(input);
                                provider_id = message.get("id").and_then(Value::as_str).map(str::to_owned).or(provider_id);
                                message_started = true;
                                response_started = true;
                                Ok(Some(ChatStreamEvent::ResponseStarted(ResponseStarted::new(provider_id.clone(), model.clone(), SafeMetadata::default()).map_err(|_| protocol_error())?)))
                            }
                        }
                        "content_block_start" => {
                            let block = value.get("content_block");
                            let kind = block.and_then(|block| block.get("type")).and_then(Value::as_str);
                            if !message_started || content_open {
                                Err(protocol_error())
                            } else if kind == Some("tool_use") {
                                let id = block.and_then(|block| block.get("id")).and_then(Value::as_str);
                                let name = block.and_then(|block| block.get("name")).and_then(Value::as_str);
                                match (id, name) {
                                    (Some(id), Some(name)) => {
                                        content_open = true;
                                        tool_open = Some((tool_calls, false));
                                        tool_calls += 1;
                                        Ok(Some(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                                            choice_index: 0,
                                            tool_call_index: tool_calls - 1,
                                            id: Some(id.to_owned()),
                                            name: Some(name.to_owned()),
                                            arguments_delta: String::new(),
                                        })))
                                    }
                                    _ => Err(protocol_error()),
                                }
                            } else if !(kind == Some("text") || kind.is_some_and(is_thinking)) { Err(protocol_error()) } else { content_open = true; thinking_open = kind != Some("text"); Ok(None) }
                        }
                        "content_block_delta" if tool_open.is_some() => {
                            let delta = value.get("delta");
                            let partial = delta
                                .filter(|delta| delta.get("type").and_then(Value::as_str) == Some("input_json_delta"))
                                .and_then(|delta| delta.get("partial_json"))
                                .and_then(Value::as_str);
                            match (partial, tool_open.as_mut()) {
                                (Some(partial), Some((index, seen))) => {
                                    *seen |= !partial.is_empty();
                                    Ok(Some(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                                        choice_index: 0,
                                        tool_call_index: *index,
                                        id: None,
                                        name: None,
                                        arguments_delta: partial.to_owned(),
                                    })))
                                }
                                _ => Err(protocol_error()),
                            }
                        }
                        "content_block_delta" if thinking_open => {
                            let kind = value.get("delta").and_then(|delta| delta.get("type")).and_then(Value::as_str);
                            if !message_started || !content_open || !matches!(kind, Some("thinking_delta" | "signature_delta")) { Err(protocol_error()) } else { Ok(None) }
                        }
                        "content_block_delta" => {
                            if !message_started || !content_open || value.get("delta").and_then(|delta| delta.get("type")).and_then(Value::as_str) != Some("text_delta") { Err(protocol_error()) } else {
                                let text = value.get("delta").and_then(|delta| delta.get("text")).and_then(Value::as_str).ok_or_else(protocol_error)?;
                                Ok(Some(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta { choice_index: 0, content_delta: Some(text.to_owned()), refusal_delta: None, annotations: vec![] })))
                            }
                        }
                        "content_block_stop" => {
                            if !content_open { Err(protocol_error()) } else {
                                content_open = false;
                                thinking_open = false;
                                // A call without arguments has an empty object.
                                match tool_open.take() {
                                    Some((index, false)) => Ok(Some(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                                        choice_index: 0,
                                        tool_call_index: index,
                                        id: None,
                                        name: None,
                                        arguments_delta: "{}".to_owned(),
                                    }))),
                                    _ => Ok(None),
                                }
                            }
                        }
                        "message_delta" => {
                            if !message_started || message_delta_seen { Err(protocol_error()) } else {
                                let reason = value.get("delta").and_then(|delta| delta.get("stop_reason")).and_then(Value::as_str).ok_or_else(protocol_error)?;
                                let usage = value.get("usage").ok_or_else(protocol_error)?;
                                validate_unsupported_usage(usage)?;
                                if let Some(read) = optional_count(usage, "cache_read_input_tokens")? { cache.0 = read; }
                                if let Some(write) = optional_count(usage, "cache_creation_input_tokens")? { cache.1 = write; }
                                if let Some(input) = optional_count(usage, "input_tokens")? { input_tokens = Some(input); }
                                let output = usage.get("output_tokens").and_then(Value::as_u64).ok_or_else(protocol_error)?;
                                finish_reason = Some(finish_reason_of(reason));
                                output_tokens = Some(output);
                                message_delta_seen = true;
                                Ok(None)
                            }
                        }
                        "message_stop" => {
                            if !message_started || !message_delta_seen || content_open || message_stop_seen { Err(protocol_error()) } else {
                                message_stop_seen = true;
                                let reason = finish_reason.clone().ok_or_else(protocol_error)?;
                                let usage = usage_of(input_tokens.ok_or_else(protocol_error)?, output_tokens.ok_or_else(protocol_error)?, cache.0, cache.1)?;
                                let _finished = ChoiceFinished::new(0, reason).map_err(|_| protocol_error())?;
                                let completed = ResponseCompletion::new(provider_id.clone(), model.clone(), Some(UsageSnapshot::final_(usage).map_err(|_| protocol_error())?)).map_err(|_| protocol_error())?;
                                terminal = true;
                                Ok(Some(ChatStreamEvent::ResponseCompleted(completed)))
                            }
                        }
                        "error" => {
                            terminal = true;
                            Err(stream_error(&value, provider_id.clone()))
                        }
                        "ping" => Ok(None),
                        _ => Err(protocol_error()),
                    };
                    match event_result {
                        Ok(Some(event)) => {
                            if let ChatStreamEvent::ResponseCompleted(_) = event {
                                if !response_started { yield Err(protocol_error()); break; }
                                yield Ok(ChatStreamEvent::ChoiceFinished(ChoiceFinished::new(0, finish_reason.clone().unwrap_or_else(|| FinishReason::new("stop"))).map_err(|_| protocol_error())?));
                            }
                            yield Ok(event);
                        }
                        Ok(None) => {}
                        Err(error) => { yield Err(error); terminal = true; break; }
                    }
                    event_name.clear();
                    data.clear();
                    data_seen = false;
                } else if let Some(value) = line.strip_prefix("event: ") {
                    if !event_name.is_empty() { yield Err(protocol_error()); terminal = true; break; }
                    event_name = value.to_owned();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    if data_seen { data.push('\n'); }
                    data.push_str(value);
                    data_seen = true;
                } else if !line.starts_with(':') {
                    yield Err(protocol_error()); terminal = true; break;
                }
                if terminal { break; }
            }
            if terminal { break; }
        }
        if !terminal || !message_stop_seen { yield Err(protocol_error()); }
    }
}

fn decode_sse_line(mut line: Vec<u8>) -> Result<String, ProviderError> {
    if line.pop() != Some(b'\n') {
        return Err(protocol_error());
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.iter().any(|byte| *byte == b'\r') {
        return Err(protocol_error());
    }
    String::from_utf8(line).map_err(|_| protocol_error())
}

async fn send_with_timeout(
    request: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
        .map_err(network_error)
}

async fn bounded_bytes(
    response: reqwest::Response,
    max: usize,
    idle_timeout: Duration,
) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(idle_timeout, stream.next())
        .await
        .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
    {
        let chunk = chunk.map_err(network_error)?;
        if bytes.len().saturating_add(chunk.len()) > max {
            return Err(protocol_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn network_error(error: reqwest::Error) -> ProviderError {
    // A connection that was never opened carried nothing to the provider.
    if error.is_connect() {
        return ProviderError::not_sent(if error.is_timeout() {
            ProviderErrorCategory::Timeout
        } else {
            ProviderErrorCategory::Unavailable
        });
    }
    ProviderError::new(if error.is_timeout() {
        ProviderErrorCategory::Timeout
    } else {
        ProviderErrorCategory::Provider
    })
}
fn network_error_simple() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Provider)
}
fn protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Protocol)
}
fn invalid_request() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::InvalidRequest)
}
fn stream_error(value: &Value, request_id: Option<String>) -> ProviderError {
    let error_type = value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str);
    let category = match error_type {
        Some("rate_limit_error") => ProviderErrorCategory::RateLimited,
        Some("overloaded_error") => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    };
    ProviderError::from_parts(
        category,
        Some("anthropic".into()),
        request_id,
        None,
        None,
        None,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}
fn status_error(status: StatusCode, headers: &header::HeaderMap) -> ProviderError {
    let category = match status {
        StatusCode::UNAUTHORIZED => ProviderErrorCategory::Authentication,
        StatusCode::FORBIDDEN => ProviderErrorCategory::Authorization,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            ProviderErrorCategory::InvalidRequest
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorCategory::RateLimited,
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ProviderErrorCategory::Timeout,
        s if s.is_server_error() => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    };
    let retry = super::retry_after(headers);
    ProviderError::from_parts(
        category,
        Some("anthropic".into()),
        headers
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        Some(status.as_u16()),
        None,
        retry,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, EmbeddingRequest, ProviderAttemptIdempotencyKey,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn adapter() -> AnthropicAdapter {
        AnthropicAdapter::new(&AnthropicConfig {
            base_url: "http://127.0.0.1:9".into(),
            api_key: "secret-that-must-not-be-logged".into(),
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(50),
            max_response_bytes: 4096,
        })
        .unwrap()
    }

    fn context() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            DeploymentModelReference::new("deployment", "claude-model").unwrap(),
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: Some(0.2),
            max_tokens: Some(12),
            ..Default::default()
        }
    }

    fn tag() -> BatchTag {
        BatchTag {
            batch_id: uuid::Uuid::from_u128(1),
            job_id: uuid::Uuid::from_u128(0xabc),
        }
    }

    #[test]
    fn batch_lines_name_their_job_and_carry_message_params() {
        let adapter = adapter();
        let line = adapter
            .input_line(
                tag(),
                7,
                BatchEndpoint::ChatCompletions,
                "claude-test",
                &LineRequest::Chat(request()),
            )
            .unwrap();
        let line: Value = serde_json::from_str(&line).unwrap();
        let custom_id = line["custom_id"].as_str().unwrap();
        assert_eq!(custom_id, "gw-00000000000000000000000000000abc-7");
        assert!(custom_id.len() <= 64);
        assert_eq!(line_of(custom_id), Some(7));
        assert_eq!(line["params"]["model"], "claude-test");
        assert!(line["params"].get("stream").is_none());
        assert!(
            adapter
                .input_line(
                    tag(),
                    1,
                    BatchEndpoint::Embeddings,
                    "m",
                    &LineRequest::Embeddings(EmbeddingRequest {
                        input: vec!["a".into()]
                    }),
                )
                .is_err()
        );
        assert_eq!(line_of("7"), None);
        assert_eq!(line_of("gw-x-y"), None);
    }

    #[test]
    fn inline_input_references_round_trip() {
        let input = InlineInput {
            recorded_at: 1_700_000_000,
            requests: 42,
        };
        assert_eq!(InlineInput::parse(&input.reference()), Some(input));
        for bad in [
            "file-abc",
            "inline-",
            "inline-1",
            "inline-x-2",
            "inline-1-x",
        ] {
            assert_eq!(InlineInput::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn message_batches_map_to_provider_jobs() {
        let batch = |status: &str, cancel: Value, expired: u64| {
            json!({"id": "msgbatch_1", "processing_status": status,
                   "cancel_initiated_at": cancel,
                   "request_counts": {"processing": 0, "succeeded": 2, "errored": 1,
                                      "canceled": 0, "expired": expired}})
        };
        let running = message_batch(&batch("in_progress", Value::Null, 0)).unwrap();
        assert_eq!(running.status, ProviderBatchStatus::Running);
        assert_eq!(running.output_file_id, None);
        assert_eq!(
            message_batch(&batch("canceling", json!("t"), 0))
                .unwrap()
                .status,
            ProviderBatchStatus::Cancelling
        );
        let ended = message_batch(&batch("ended", Value::Null, 0)).unwrap();
        assert_eq!(ended.status, ProviderBatchStatus::Completed);
        assert_eq!(ended.output_file_id.as_deref(), Some("msgbatch_1"));
        assert_eq!(ended.error_file_id, None);
        assert_eq!(
            message_batch(&batch("ended", json!("t"), 0))
                .unwrap()
                .status,
            ProviderBatchStatus::Cancelled
        );
        assert_eq!(
            message_batch(&batch("ended", Value::Null, 1))
                .unwrap()
                .status,
            ProviderBatchStatus::Expired
        );
        assert!(message_batch(&batch("other", Value::Null, 0)).is_err());
        assert_eq!(request_total(&batch("ended", Value::Null, 1)), Some(4));
    }

    #[test]
    fn batch_results_map_each_line_by_its_custom_id() {
        let adapter = adapter();
        let id = |line: u32| custom_id(tag(), line);
        let lines = [
            json!({"custom_id": id(1), "result": {"type": "succeeded", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
                "stop_reason": "end_turn", "content": [{"type": "text", "text": "hi"}],
                "usage": {"input_tokens": 5, "output_tokens": 7}}}}),
            json!({"custom_id": id(2), "result": {"type": "errored", "error": {"type": "error",
                "error": {"type": "invalid_request_error", "message": "no"}}}}),
            json!({"custom_id": id(3), "result": {"type": "expired"}}),
            json!({"custom_id": id(4), "result": {"type": "canceled"}}),
            json!({"custom_id": id(5), "result": {"type": "succeeded", "message": {}}}),
            json!({"custom_id": "not-ours", "result": {"type": "expired"}}),
        ];
        let output = lines
            .iter()
            .map(Value::to_string)
            .chain(["not json".to_owned()])
            .collect::<Vec<_>>()
            .join("\n");
        let outcomes =
            adapter.parse_results(BatchEndpoint::ChatCompletions, output.as_bytes(), b"");
        assert_eq!(outcomes.len(), 5);
        assert!(
            matches!(&outcomes[&1], LineOutcome::Succeeded { response, request_id }
            if request_id.as_deref() == Some("msg_1")
                && matches!(response.as_ref(), LineResponse::Chat(r) if r.usage().is_some()))
        );
        assert!(
            matches!(&outcomes[&2], LineOutcome::Failed { code: Some(code), .. }
            if code == "invalid_request_error")
        );
        assert!(matches!(outcomes[&3], LineOutcome::NotRun));
        assert!(matches!(outcomes[&4], LineOutcome::NotRun));
        assert!(matches!(outcomes[&5], LineOutcome::Unreadable));
    }

    #[test]
    fn claude_on_foundry_has_no_batches() {
        assert!(adapter().batches().is_some());
        let foundry = AnthropicAdapter::for_azure_ai_foundry(
            &AnthropicConfig {
                base_url: "http://127.0.0.1:9".into(),
                api_key: String::new(),
                request_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_millis(50),
                max_response_bytes: 4096,
            },
            AzureAuth::ApiKey("k".into()),
        )
        .unwrap();
        assert!(foundry.batches().is_none());
    }

    #[tokio::test]
    async fn embeddings_are_rejected_before_any_network_request() {
        let error = adapter()
            .embeddings(
                context(),
                EmbeddingRequest {
                    input: vec!["x".into()],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    }

    #[test]
    fn request_mapping_keeps_system_separate_and_rejects_tools() {
        let adapter = adapter();
        let request = ChatRequest {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "system".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "hello".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
            temperature: Some(0.2),
            max_tokens: Some(12),
            ..Default::default()
        };
        let body = adapter.request_body(&context(), &request, false).unwrap();
        assert_eq!(body["system"], "system");
        assert_eq!(body["messages"][0]["role"], "user");
        let tool_request = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::Tool,
                content: "x".into(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: None,
            max_tokens: Some(1),
            ..Default::default()
        };
        assert_eq!(
            adapter
                .request_body(&context(), &tool_request, false)
                .unwrap_err()
                .category(),
            ProviderErrorCategory::InvalidRequest
        );
    }

    #[test]
    fn text_validation_allows_only_one_leading_system_message() {
        let adapter = adapter();
        let base = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: None,
            max_tokens: Some(1),
            ..Default::default()
        };
        assert!(adapter.validate_chat_request(&context(), &base).is_ok());

        let mut trailing_system = base.clone();
        trailing_system.messages.push(ChatMessage {
            role: ChatRole::System,
            content: "late".into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
        assert!(
            adapter
                .validate_chat_request(&context(), &trailing_system)
                .is_err()
        );

        let developer = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::Developer,
                content: "unsupported".into(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: None,
            max_tokens: Some(1),
            ..Default::default()
        };
        assert!(
            adapter
                .validate_chat_request(&context(), &developer)
                .is_err()
        );
    }

    /// Anthropic's `input_tokens` leaves out cache reads and writes: the
    /// gateway's input adds the reads back (charged as cached input) and
    /// the writes are the `other` counter, so no token is lost or charged
    /// twice.
    #[test]
    fn cache_reads_and_writes_are_counted_apart_from_uncached_input() {
        let counters = |usage: Usage| {
            (
                usage.input_tokens().and_then(|c| c.value()),
                usage.cached_input_tokens().and_then(|c| c.value()),
                usage.other_tokens().and_then(|c| c.value()),
            )
        };
        // The documented example: 2048 uncached, 1800 read, 248 written.
        let usage = parse_usage(&serde_json::json!({
            "input_tokens": 2048,
            "cache_read_input_tokens": 1800,
            "cache_creation_input_tokens": 248,
            "output_tokens": 503,
            "cache_creation": {"ephemeral_5m_input_tokens": 148, "ephemeral_1h_input_tokens": 100}
        }))
        .unwrap();
        assert_eq!(counters(usage), (Some(3848), Some(1800), Some(248)));
        // 2048 at the input price, 1800 at cached input, 248 at the cache
        // write price.
        let price = crate::domain::accounting::PriceSnapshot::configured_with_breakdown(
            100,
            500,
            10,
            0,
            125,
            "test",
            crate::domain::accounting::PricingConfidence::Known,
        )
        .unwrap();
        assert_eq!(
            price.charge(&usage).unwrap().amount.value(),
            2048 * 100 + 1800 * 10 + 248 * 125 + 503 * 500
        );
        // Absent or null counts are zero.
        let plain = parse_usage(&serde_json::json!({
            "input_tokens": 5, "output_tokens": 2,
            "cache_creation_input_tokens": null
        }))
        .unwrap();
        assert_eq!(counters(plain), (Some(5), Some(0), Some(0)));
        for rejected in [
            serde_json::json!({"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": "1"}),
            serde_json::json!({"input_tokens": 1, "output_tokens": 2, "cache_creation_input_tokens": -1}),
            serde_json::json!({"input_tokens": 1, "output_tokens": 2, "reasoning_tokens": 1}),
        ] {
            assert_eq!(
                parse_usage(&rejected).unwrap_err().category(),
                ProviderErrorCategory::Protocol,
                "{rejected}"
            );
        }
    }

    #[tokio::test]
    async fn a_stream_reports_cache_reads_and_writes() {
        // `message_start` reports the cache counts; a `message_delta` that
        // repeats them has the final ones.
        let stream = |delta_usage: &str| {
            format!(
                concat!(
                    "event: message_start\n",
                    "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"usage\":{{\"input_tokens\":30,\"cache_read_input_tokens\":1000,\"cache_creation_input_tokens\":200,\"output_tokens\":1}}}}}}\n\n",
                    "event: content_block_start\n",
                    "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                    "event: content_block_delta\n",
                    "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"pong\"}}}}\n\n",
                    "event: content_block_stop\n",
                    "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                    "event: message_delta\n",
                    "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{}}}\n\n",
                    "event: message_stop\n",
                    "data: {{\"type\":\"message_stop\"}}\n\n"
                ),
                delta_usage
            )
        };
        for (delta, expected) in [
            (r#"{"output_tokens":12}"#, (1030, 1000, 200)),
            (
                r#"{"input_tokens":30,"cache_read_input_tokens":1000,"cache_creation_input_tokens":250,"output_tokens":12}"#,
                (1030, 1000, 250),
            ),
        ] {
            let (adapter, _closed) =
                mock_stream_adapter(vec![stream(delta).into_bytes()], false).await;
            let events = adapter
                .chat_stream(context(), request())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let Some(Ok(ChatStreamEvent::ResponseCompleted(completed))) = events.last() else {
                panic!("{events:?}");
            };
            let usage = completed.usage().unwrap().usage();
            let value = |c: Option<TokenCounter>| c.and_then(|c| c.value()).unwrap();
            assert_eq!(
                (
                    value(usage.input_tokens()),
                    value(usage.cached_input_tokens()),
                    value(usage.other_tokens())
                ),
                expected,
                "{delta}"
            );
            assert_eq!(value(usage.output_tokens()), 12);
        }
    }

    #[test]
    fn count_response_is_strictly_bounded_to_input_tokens() {
        assert_eq!(
            parse_count_tokens(serde_json::json!({"input_tokens": 42})).unwrap(),
            42
        );
        for value in [
            serde_json::json!({"input_tokens": -1}),
            serde_json::json!({"input_tokens": "42"}),
            serde_json::json!({"input_tokens": 42, "output_tokens": 1}),
            serde_json::json!([{"input_tokens": 42}]),
        ] {
            assert_eq!(
                parse_count_tokens(value).unwrap_err().category(),
                ProviderErrorCategory::Protocol
            );
        }
    }

    #[tokio::test]
    async fn api_key_header_is_marked_sensitive() {
        let request = adapter()
            .request(
                false,
                &context(),
                &ChatRequest {
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "hello".into(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    }],
                    temperature: None,
                    max_tokens: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .build()
            .unwrap();
        assert!(request.headers().get("x-api-key").unwrap().is_sensitive());
    }

    #[tokio::test]
    async fn count_tokens_uses_anthropic_wire_path_headers_and_projected_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end;
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    header_end = position + 4;
                    break;
                }
            }
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
            }
            request_sender
                .send(String::from_utf8(request).unwrap())
                .unwrap();
            let body = r#"{"input_tokens":9}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = AnthropicAdapter::new(&AnthropicConfig {
            base_url: format!("http://{address}"),
            api_key: "count-key".into(),
            request_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 4096,
        })
        .unwrap();
        let request = ChatRequest {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "system-token-bearing".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "user-token-bearing".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
            temperature: Some(0.2),
            max_tokens: Some(12),
            ..Default::default()
        };
        assert_eq!(
            adapter
                .count_chat_input_tokens(&context(), &request)
                .await
                .unwrap(),
            9
        );
        let wire = request_receiver.await.unwrap();
        let body = wire.split("\r\n\r\n").nth(1).unwrap();
        assert!(wire.starts_with("POST /v1/messages/count_tokens HTTP/1.1"));
        assert!(wire.to_ascii_lowercase().contains("x-api-key: count-key"));
        assert!(
            wire.to_ascii_lowercase()
                .contains("anthropic-version: 2023-06-01")
        );
        assert!(body.contains("system-token-bearing"));
        assert!(body.contains("user-token-bearing"));
        assert!(body.contains("\"model\":\"claude-model\""));
        assert!(!body.contains("max_tokens"));
        assert!(!body.contains("temperature"));
        assert!(!body.contains("\"stream\""));
    }

    #[tokio::test]
    async fn no_response_headers_timeout_count_chat_and_stream_setup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut sockets = Vec::new();
            for _ in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                sockets.push(socket);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            sockets
        });
        let adapter = AnthropicAdapter::new(&AnthropicConfig {
            base_url: format!("http://{address}"),
            api_key: "timeout-key".into(),
            request_timeout: Duration::from_millis(40),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 4096,
        })
        .unwrap();
        let count_context = context();
        let count_request = request();
        let (count, chat, stream) = tokio::join!(
            adapter.count_chat_input_tokens(&count_context, &count_request),
            adapter.chat(context(), request()),
            adapter.chat_stream(context(), request()),
        );
        assert_eq!(
            count.unwrap_err().category(),
            ProviderErrorCategory::Timeout
        );
        assert_eq!(chat.unwrap_err().category(), ProviderErrorCategory::Timeout);
        let stream_error = match stream {
            Err(error) => error,
            Ok(_) => panic!("stream setup must time out"),
        };
        assert_eq!(stream_error.category(), ProviderErrorCategory::Timeout);
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn mock_http_chat_captures_request_id_and_validates_wire_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        let response_body = r#"{"type":"message","id":"message-id","role":"assistant","model":"claude","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nrequest-id: req-header\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end;
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    header_end = position + 4;
                    break;
                }
            }
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
            }
            request_sender
                .send(String::from_utf8(request).unwrap())
                .unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let config = AnthropicConfig {
            base_url: format!("http://{address}"),
            api_key: "test-anthropic-key".into(),
            request_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 4096,
        };
        let adapter = AnthropicAdapter::new(&config).unwrap();
        let result = adapter
            .chat(context(), {
                ChatRequest {
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "hello".into(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    }],
                    temperature: None,
                    max_tokens: Some(3),
                    ..Default::default()
                }
            })
            .await
            .unwrap();
        let wire = request_receiver.await.unwrap().to_ascii_lowercase();
        assert!(wire.contains("x-api-key: test-anthropic-key"));
        assert!(wire.contains("anthropic-version: 2023-06-01"));
        assert!(wire.contains("\"stream\":false"));
        assert_eq!(result.provider_request_id(), Some("req-header"));
        assert_eq!(result.choices()[0].message.content, "ok");
    }

    async fn mock_stream_adapter(
        chunks: Vec<Vec<u8>>,
        hold_open: bool,
    ) -> (AnthropicAdapter, tokio::sync::oneshot::Receiver<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (closed_sender, closed_receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end;
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    header_end = position + 4;
                    break;
                }
            }
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nrequest-id: stream-request\r\nConnection: close\r\n\r\n").await.unwrap();
            for chunk in chunks {
                socket.write_all(&chunk).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if hold_open {
                let mut probe = [0_u8; 1];
                let closed = matches!(
                    tokio::time::timeout(Duration::from_secs(3), socket.read(&mut probe)).await,
                    Ok(Ok(0))
                );
                let _ = closed_sender.send(closed);
            } else {
                let _ = closed_sender.send(true);
            }
        });
        let adapter = AnthropicAdapter::new(&AnthropicConfig {
            base_url: format!("http://{address}"),
            api_key: "test-anthropic-key".into(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 4096,
        })
        .unwrap();
        (adapter, closed_receiver)
    }

    fn valid_stream_body() -> &'static str {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
    }

    #[tokio::test]
    async fn mock_http_stream_handles_fragmented_multiline_sse() {
        let body = valid_stream_body().as_bytes();
        let split = body.len() / 3;
        let (adapter, closed) =
            mock_stream_adapter(vec![body[..split].to_vec(), body[split..].to_vec()], false).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
        assert!(closed.await.unwrap());
    }

    #[test]
    fn thinking_blocks_are_skipped_and_the_text_is_the_answer() {
        // Claude Sonnet 5.5 on Azure AI Foundry, 2026-11-02 (signature shortened).
        let value = serde_json::json!({
            "model": "claude-sonnet-5-5", "id": "msg_1", "type": "message", "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "private thought", "signature": "CAQS5gQK"},
                {"type": "redacted_thinking", "data": "EmwKAhgB"},
                {"type": "text", "text": "pong"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 30, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 12}
        });
        let response = parse_response(value, Some("req-1".into())).expect("thinking is skipped");
        assert_eq!(response.choices()[0].message.content, "pong");
    }

    #[test]
    fn other_blocks_are_still_refused() {
        let value = serde_json::json!({
            "model": "claude", "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "server_tool_use", "id": "t", "name": "web_search", "input": {}}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        assert_eq!(
            parse_response(value, None).unwrap_err().category(),
            ProviderErrorCategory::Protocol
        );
    }

    #[tokio::test]
    async fn a_streamed_thinking_block_is_dropped() {
        // As streamed by Claude Sonnet 5.5 on Azure AI Foundry, 2026-11-02.
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":30,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private thought\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"CAQS5gQK\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"pong\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":12}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (adapter, closed) = mock_stream_adapter(vec![body.as_bytes().to_vec()], false).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let text = events
            .iter()
            .filter_map(|event| match event {
                Ok(ChatStreamEvent::ChoiceDelta(delta)) => delta.content_delta.clone(),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "pong");
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
        assert!(closed.await.unwrap());
    }

    #[tokio::test]
    async fn text_inside_a_thinking_block_is_refused() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"leak\"}}\n\n"
        );
        let (adapter, _closed) = mock_stream_adapter(vec![body.as_bytes().to_vec()], false).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(event, Err(error) if error.category() == ProviderErrorCategory::Protocol)));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(ChatStreamEvent::ChoiceDelta(_))))
        );
    }

    #[tokio::test]
    async fn mock_http_stream_provider_error_and_premature_eof_fail_closed() {
        let error_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n"
        );
        let (adapter, closed) =
            mock_stream_adapter(vec![error_body.as_bytes().to_vec()], false).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(event, Err(error) if error.category() == ProviderErrorCategory::Unavailable)));
        assert!(closed.await.unwrap());

        let start_only = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
        let (adapter, closed) =
            mock_stream_adapter(vec![start_only.as_bytes().to_vec()], false).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(event, Err(error) if error.category() == ProviderErrorCategory::Protocol)));
        assert!(closed.await.unwrap());
    }

    #[tokio::test]
    async fn dropping_mock_http_stream_cancels_upstream_without_completion() {
        let start_only = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
        let (adapter, closed) =
            mock_stream_adapter(vec![start_only.as_bytes().to_vec()], true).await;
        let mut stream = adapter.chat_stream(context(), request()).await.unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(ChatStreamEvent::ResponseStarted(_)))
        ));
        drop(stream);
        assert!(closed.await.unwrap());
    }

    #[tokio::test]
    async fn sse_requires_provider_terminal_sequence() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\n",
            "data: \"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let events = anthropic_sse_stream(
            futures::stream::iter(vec![Ok(bytes::Bytes::from(body))]),
            "claude".into(),
            None,
            4096,
            Duration::from_secs(1),
        )
        .collect::<Vec<_>>()
        .await;
        assert!(events.iter().all(Result::is_ok));
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
    }

    use crate::provider::tools::tool_conversation;

    #[test]
    fn tools_map_to_tool_use_and_one_user_turn_of_results() {
        let body = message_params("claude-x", &tool_conversation()).unwrap();
        assert_eq!(
            body["tools"],
            json!([{"name": "get_weather", "description": "Current weather",
                    "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}])
        );
        // No choice asked for: Anthropic's default (auto) is left to it.
        assert!(body.get("tool_choice").is_none());
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "weather in Paris and Rome?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "Paris"}},
                    {"type": "tool_use", "id": "toolu_02", "name": "get_weather", "input": {"city": "Rome"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": "18C"},
                    {"type": "tool_result", "tool_use_id": "toolu_02", "content": "24C"}
                ]}
            ])
        );
        let choice = |tool_choice, parallel| {
            message_params(
                "claude-x",
                &ChatRequest {
                    tool_choice,
                    parallel_tool_calls: parallel,
                    ..tool_conversation()
                },
            )
            .unwrap()["tool_choice"]
                .clone()
        };
        assert_eq!(
            choice(Some(ToolChoice::Auto), None),
            json!({"type": "auto"})
        );
        assert_eq!(
            choice(Some(ToolChoice::None), Some(false)),
            json!({"type": "none"})
        );
        assert_eq!(
            choice(Some(ToolChoice::Required), None),
            json!({"type": "any"})
        );
        assert_eq!(
            choice(
                Some(ToolChoice::Function("get_weather".into())),
                Some(false)
            ),
            json!({"type": "tool", "name": "get_weather", "disable_parallel_tool_use": true})
        );
        assert_eq!(
            choice(None, Some(false)),
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        assert_eq!(choice(None, Some(true)), Value::Null);
    }

    #[test]
    fn tool_use_blocks_are_tool_calls() {
        // Anthropic's documented response to a tool request.
        let value = json!({
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-x",
            "content": [
                {"type": "text", "text": "I'll check."},
                {"type": "tool_use", "id": "toolu_01A", "name": "get_weather",
                 "input": {"city": "Paris", "unit": "c"}}
            ],
            "stop_reason": "tool_use", "stop_sequence": null,
            "usage": {"input_tokens": 400, "output_tokens": 50}
        });
        let response = parse_response(value, None).unwrap();
        let choice = &response.choices()[0];
        assert_eq!(choice.message.content, "I'll check.");
        assert_eq!(choice.finish_reason.as_ref().unwrap().0, "tool_calls");
        assert_eq!(
            choice.tool_calls,
            vec![ToolCall {
                id: Some("toolu_01A".into()),
                name: Some("get_weather".into()),
                arguments: r#"{"city":"Paris","unit":"c"}"#.into(),
            }]
        );
        let bad = json!({
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-x",
            "content": [{"type": "tool_use", "id": "toolu_01A", "name": "f", "input": "x"}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        assert!(parse_response(bad, None).is_err());
    }

    #[tokio::test]
    async fn streamed_tool_use_becomes_tool_call_deltas() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\": \\\"Pa\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ris\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_02\",\"name\":\"now\",\"input\":{}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":40}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let events = anthropic_sse_stream(
            futures::stream::iter(vec![Ok(bytes::Bytes::from(body))]),
            "claude-x".into(),
            None,
            1 << 16,
            Duration::from_secs(1),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        let deltas = events
            .iter()
            .filter_map(|event| match event {
                ChatStreamEvent::ToolCallDelta(delta) => Some(delta.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas[0].id.as_deref(), Some("toolu_01"));
        assert_eq!(deltas[0].name.as_deref(), Some("get_weather"));
        let arguments = |index| {
            deltas
                .iter()
                .filter(|delta| delta.tool_call_index == index)
                .map(|delta| delta.arguments_delta.as_str())
                .collect::<String>()
        };
        assert_eq!(arguments(0), r#"{"city": "Paris"}"#);
        // A call streamed without input has an empty object.
        assert_eq!(arguments(1), "{}");
        assert!(events.iter().any(|event| matches!(
            event,
            ChatStreamEvent::ChoiceFinished(finished) if finished.finish_reason().0 == "tool_calls"
        )));
    }
}
