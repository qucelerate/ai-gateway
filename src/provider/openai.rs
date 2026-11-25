use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::{Client, Response, StatusCode, header};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, fmt, time::Duration};
use url::Url;

use crate::{
    config::OpenAiConfig,
    domain::discovery::{DiscoveredModel, ModelListBuilder},
    domain::inference::{
        Annotation, ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent,
        ChoiceFinished, Embedding, EmbeddingRequest, EmbeddingResponse, FinishReason,
        InferenceContext, ProviderError, ProviderErrorCategory, ResponseCompletion,
        ResponseStarted, SafeMetadata, StreamChoiceDelta, ToolCall, ToolCallDelta, Usage,
        UsageSnapshot,
    },
};

use super::{
    ChatEventStream, InferenceProviderAdapter, ValidatingChatEventStream,
    batch::{
        BatchProvider, BatchTag, LineOutcome, MAX_BATCH_FILE_BYTES, MAX_FIND_PAGES, PageSearch,
        ProviderBatch, openai_format,
        openai_format::{provider_batch, search_page},
    },
    model_list::{PageShape, push_page},
};
use crate::domain::batch::{BatchEndpoint, LineRequest};

const PROVIDER: &str = "openai";
const MAX_ERROR_BODY: usize = 16 * 1024;

pub(crate) struct OpenAiAdapter {
    client: Client,
    base_url: Url,
    api_key: String,
    max_response_bytes: usize,
}

impl fmt::Debug for OpenAiAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiAdapter")
            .field("base_url", &self.base_url)
            .field("api_key", &"REDACTED")
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl OpenAiAdapter {
    pub(crate) fn new(config: &OpenAiConfig) -> Result<Self, ProviderError> {
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
        Ok(Self {
            client,
            base_url,
            api_key: config.api_key.clone(),
            max_response_bytes: config.max_response_bytes,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url.join(path).map_err(|_| protocol_error())
    }

    fn get(&self, path: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self
            .client
            .get(self.endpoint(path)?)
            .header(header::AUTHORIZATION, self.authorization_header()?)
            .header(header::ACCEPT, "application/json"))
    }

    fn request(
        &self,
        path: &str,
        context: &InferenceContext,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self
            .client
            .post(self.endpoint(path)?)
            .header(header::AUTHORIZATION, self.authorization_header()?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                "idempotency-key",
                context.provider_attempt_idempotency_key.as_str(),
            )
            .header("x-correlation-id", context.correlation_id.0.to_string())
            .header("x-client-request-id", context.correlation_id.0.to_string()))
    }

    fn authorization_header(&self) -> Result<header::HeaderValue, ProviderError> {
        let mut value = header::HeaderValue::from_str(&format!("Bearer {}", self.api_key))
            .map_err(|_| ProviderError::new(ProviderErrorCategory::Internal))?;
        // Sensitivity redacts diagnostics only; the request still sends `Bearer <api key>`.
        value.set_sensitive(true);
        Ok(value)
    }

    async fn response_error(&self, response: Response) -> ProviderError {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
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

#[async_trait]
impl InferenceProviderAdapter for OpenAiAdapter {
    fn batches(&self) -> Option<&dyn BatchProvider> {
        Some(self)
    }

    async fn count_chat_input_tokens(
        &self,
        _context: &InferenceContext,
        _request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        // Chat Completions has no provider-side token counting operation. In
        // particular, do not approximate or make a generation request here.
        Err(ProviderError::new(
            ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let body = ChatWireRequest::from_domain(&context, &request, false);
        let response = self
            .request("chat/completions", &context)?
            .json(&body)
            .send()
            .await
            .map_err(|error| request_error(error))?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let request_id = response
            .headers()
            .get("x-request-id")
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
        let body = ChatWireRequest::from_domain(&context, &request, true);
        let response = self
            .request("chat/completions", &context)?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = response.bytes_stream();
        let stream = openai_sse_stream(bytes, request_id, self.max_response_bytes);
        Ok(ValidatingChatEventStream::new(stream))
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        let body = json!({
            "model": context.deployment.model(),
            "input": request.input,
        });
        let response = self
            .request("embeddings", &context)?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let value = read_json_bounded(response, self.max_response_bytes)
            .await
            .map_err(|error| preserve_request_id(error, request_id.as_deref()))?;
        parse_embedding_response(value, request_id.clone())
            .map_err(|error| preserve_request_id(error, request_id.as_deref()))
    }

    /// `GET models` returns every model in one page.
    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let response = self.get("models")?.send().await.map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let page = read_json_bounded(response, self.max_response_bytes).await?;
        let mut models = ModelListBuilder::default();
        push_page(&mut models, &page, &OPENAI_MODEL_PAGE)?;
        Ok(models.finish())
    }
}

const OPENAI_MODEL_PAGE: PageShape = PageShape {
    items: "data",
    id: "id",
    display_name: None,
    id_prefix: None,
    omits_empty: false,
};

#[derive(Serialize)]
struct ChatWireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
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
        Self::for_model(context.deployment.model(), request, stream)
    }

    fn for_model(model: &str, request: &ChatRequest, stream: bool) -> Self {
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
            max_completion_tokens: request.max_tokens,
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
        ("id", "openai_response_id"),
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

fn openai_sse_stream<S>(
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

impl OpenAiAdapter {
    async fn batch_json(&self, request: reqwest::RequestBuilder) -> Result<Value, ProviderError> {
        let response = request.send().await.map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        read_json_bounded(response, self.max_response_bytes).await
    }

    fn batch_post(&self, path: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self
            .client
            .post(self.endpoint(path)?)
            .header(header::AUTHORIZATION, self.authorization_header()?)
            .header(header::ACCEPT, "application/json"))
    }
}

#[async_trait]
impl BatchProvider for OpenAiAdapter {
    async fn find_batch(
        &self,
        tag: BatchTag,
        input_file_id: &str,
    ) -> Result<Option<ProviderBatch>, ProviderError> {
        // A job is created from its input, so it is no older than the
        // input file; the listing is newest first, so reaching a job older
        // than the file ends the search.
        let file = self
            .batch_json(self.get(&format!("files/{input_file_id}"))?)
            .await?;
        let input_created = file.get("created_at").and_then(Value::as_i64);
        let mut after: Option<String> = None;
        for _ in 0..MAX_FIND_PAGES {
            let mut request = self.get("batches")?.query(&[("limit", "100")]);
            if let Some(after) = &after {
                request = request.query(&[("after", after.as_str())]);
            }
            let page = self.batch_json(request).await?;
            match search_page(&page, tag, input_created)? {
                PageSearch::Found(found) => return provider_batch(found).map(Some),
                PageSearch::Absent => return Ok(None),
                PageSearch::Next(last) => after = Some(last),
            }
        }
        // Cut short: only a search that reached the end, or a job older
        // than the input, may say the job does not exist, as the worker
        // then settles it unsent.
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
            "endpoint": endpoint.as_str(),
            "completion_window": "24h",
            "metadata": openai_format::metadata(tag),
        });
        let request = self
            .batch_post("batches")?
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", format!("gw-batch-job-{}", tag.job_id))
            .json(&body);
        provider_batch(&self.batch_json(request).await?)
    }

    async fn batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let path = format!("batches/{}", urlencoding_segment(id));
        provider_batch(&self.batch_json(self.get(&path)?).await?)
    }

    async fn cancel_batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let path = format!("batches/{}/cancel", urlencoding_segment(id));
        provider_batch(&self.batch_json(self.batch_post(&path)?).await?)
    }

    async fn file_content(&self, id: &str) -> Result<Vec<u8>, ProviderError> {
        let path = format!("files/{}/content", urlencoding_segment(id));
        let response = self
            .client
            .get(self.endpoint(&path)?)
            .header(header::AUTHORIZATION, self.authorization_header()?)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        read_bounded(response, MAX_BATCH_FILE_BYTES).await
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
                serde_json::to_value(ChatWireRequest::for_model(model, request, false))
                    .map_err(|_| protocol_error())?
            }
            LineRequest::Embeddings(request) => json!({"model": model, "input": request.input}),
        };
        openai_format::input_line(line, endpoint.as_str(), body)
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

/// A provider id as one path segment.
fn urlencoding_segment(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, ProviderAttemptIdempotencyKey,
    };
    use crate::provider::batch::LineResponse;
    use futures::StreamExt;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::task::{Context, Poll};
    use std::thread;
    use std::{future::Future, pin::Pin, time::Instant};

    fn make_adapter(
        listener: TcpListener,
        response: String,
    ) -> (OpenAiAdapter, mpsc::Receiver<String>) {
        let address = listener.local_addr().expect("listener address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let request = read_request(&mut stream);
            sender.send(request).expect("request receiver");
            stream.write_all(response.as_bytes()).expect("response");
        });
        let config = OpenAiConfig {
            base_url: format!("http://{address}/v1"),
            api_key: "test-api-key".to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        (OpenAiAdapter::new(&config).expect("adapter"), receiver)
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
            DeploymentModelReference::new("deployment", "gpt-test").expect("deployment"),
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

    struct PendingBody {
        dropped: Arc<AtomicBool>,
    }

    impl Future for PendingBody {
        type Output = Result<bytes::Bytes, reqwest::Error>;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingBody {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_tag_search_ends_only_at_the_listing_end_or_older_jobs() {
        let tag = BatchTag {
            batch_id: uuid::Uuid::new_v4(),
            job_id: uuid::Uuid::new_v4(),
        };
        let other = |created: i64| json!({"id": "b", "status": "completed", "created_at": created});
        let page = |data: Vec<Value>, more: bool, last: Option<&str>| json!({"data": data, "has_more": more, "last_id": last});
        let ours = json!({"id": "mine", "status": "in_progress", "created_at": 90,
                          "metadata": {"gateway_batch": tag.batch_id.to_string(),
                                       "gateway_job": tag.job_id.to_string()}});
        let found = page(vec![other(100), ours.clone()], true, Some("mine"));
        assert_eq!(
            search_page(&found, tag, Some(80)).unwrap(),
            PageSearch::Found(&ours)
        );
        // Newer jobs only, more to come: search on.
        assert_eq!(
            search_page(&page(vec![other(100)], true, Some("b")), tag, Some(80)).unwrap(),
            PageSearch::Next("b".to_owned())
        );
        // A job older than the input: ours is not further on.
        assert_eq!(
            search_page(
                &page(vec![other(100), other(70)], true, Some("b")),
                tag,
                Some(80)
            )
            .unwrap(),
            PageSearch::Absent
        );
        assert_eq!(
            search_page(&page(vec![other(100)], false, None), tag, Some(80)).unwrap(),
            PageSearch::Absent
        );
        // More to come but no cursor, or no times to compare: never absent.
        assert!(search_page(&page(vec![other(100)], true, None), tag, Some(80)).is_err());
        assert_eq!(
            search_page(&page(vec![other(70)], true, Some("b")), tag, None).unwrap(),
            PageSearch::Next("b".to_owned())
        );
    }

    #[test]
    fn batch_results_map_each_line_by_its_number() {
        let adapter = make_batch_adapter();
        let ok = json!({"custom_id": "1", "response": {"status_code": 200, "request_id": "r1",
            "body": {"model": "m", "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "hi"}}],
                     "usage": {"prompt_tokens": 5, "completion_tokens": 7}}}, "error": null});
        let refused = json!({"custom_id": "2", "response": {"status_code": 400, "request_id": "r2",
            "body": {"error": {"code": "invalid_prompt"}}}, "error": null});
        let expired = json!({"custom_id": "3", "response": null,
            "error": {"code": "batch_expired", "message": "x"}});
        let garbled = json!({"custom_id": "4", "response": {"status_code": 200, "body": {}}});
        let output = format!("{ok}\n{garbled}\nnot json\n");
        let errors = format!("{refused}\n{expired}\n");
        let outcomes = adapter.parse_results(
            BatchEndpoint::ChatCompletions,
            output.as_bytes(),
            errors.as_bytes(),
        );
        assert!(
            matches!(&outcomes[&1], LineOutcome::Succeeded { response, .. }
            if matches!(response.as_ref(), LineResponse::Chat(r) if r.usage().is_some()))
        );
        assert!(
            matches!(&outcomes[&2], LineOutcome::Failed { status_code: Some(400), code: Some(c), .. }
            if c == "invalid_prompt")
        );
        assert!(matches!(outcomes[&3], LineOutcome::NotRun));
        assert!(matches!(outcomes[&4], LineOutcome::Unreadable));
        assert_eq!(outcomes.len(), 4);
        let line = adapter
            .input_line(
                BatchTag {
                    batch_id: uuid::Uuid::nil(),
                    job_id: uuid::Uuid::nil(),
                },
                9,
                BatchEndpoint::Embeddings,
                "text-embedding-3-small",
                &LineRequest::Embeddings(EmbeddingRequest {
                    input: vec!["a".into()],
                }),
            )
            .unwrap();
        let line: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(line["custom_id"], "9");
        assert_eq!(line["body"]["model"], "text-embedding-3-small");
        assert_eq!(urlencoding_segment("batch_abc/../x"), "batch_abc..x");
    }

    fn make_batch_adapter() -> OpenAiAdapter {
        OpenAiAdapter::new(&OpenAiConfig {
            api_key: "sk-test".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn chat_count_is_unsupported_without_network() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let config = OpenAiConfig {
            base_url: format!("http://{address}"),
            api_key: "test-api-key".to_owned(),
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(50),
            max_response_bytes: 1024,
        };
        let adapter = OpenAiAdapter::new(&config).expect("adapter");
        let error = adapter
            .count_chat_input_tokens(&context(), &request())
            .await
            .expect_err("Chat Completions must not count or generate");
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    }

    #[tokio::test]
    async fn a_refused_connection_is_marked_not_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let adapter = OpenAiAdapter::new(&OpenAiConfig {
            base_url: format!("http://{address}"),
            api_key: "test-api-key".to_owned(),
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(200),
            max_response_bytes: 1024,
        })
        .expect("adapter");
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("nothing is listening");
        assert!(error.was_not_sent(), "{error:?}");
        assert_eq!(error.category(), ProviderErrorCategory::Unavailable);
    }

    #[tokio::test]
    async fn chat_maps_request_headers_and_response_id() {
        let body = r#"{"id":"chat-body-id","model":"gpt-test","choices":[{"index":0,"message":{"role":"assistant","content":"hello","refusal":"no","annotations":[{"type":"url_citation","url_citation":{"url":"https://example.invalid"}}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Request-Id: chat-header-id\r\n\r\n{}",
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
        assert!(wire.contains("POST /v1/chat/completions HTTP/1.1"));
        assert!(wire.to_ascii_lowercase().contains("authorization: "));
        assert!(
            wire.to_ascii_lowercase()
                .contains(&format!("idempotency-key: {provider_attempt}"))
        );
        assert!(wire.contains("x-correlation-id: ".to_owned().as_str()));
        assert!(
            wire.to_ascii_lowercase()
                .contains(&format!("x-correlation-id: {correlation}"))
        );
        assert!(
            wire.to_ascii_lowercase()
                .contains(&format!("x-client-request-id: {correlation}"))
        );
        assert!(wire.contains("\"model\":\"gpt-test\""));
        assert!(wire.contains("\"max_completion_tokens\":12"));
        assert_eq!(result.provider_request_id(), Some("chat-header-id"));
        assert_eq!(result.choices()[0].message.content, "hello");
        assert_eq!(result.choices()[0].refusal.as_deref(), Some("no"));
        assert_eq!(result.choices()[0].annotations[0].kind, "url_citation");
        assert_eq!(
            serde_json::to_value(result.metadata()).expect("safe metadata")["openai_response_id"],
            "chat-body-id"
        );
    }

    #[tokio::test]
    async fn stream_emits_usage_and_completes_only_at_done() {
        let body = concat!(
            "data: {\"id\":\"chat-stream-id\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\",\"annotations\":[{\"type\":\"url_citation\",\"value\":\"cite\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat-stream-id\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chat-stream-id\",\"model\":\"gpt-test\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n",
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
        assert!(matches!(
            &events[1],
            Ok(ChatStreamEvent::ChoiceDelta(delta)) if delta.annotations.len() == 1
        ));
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
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 3\r\nX-Request-Id: rate-limit-id\r\n\r\n{}",
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
        let body = r#"{"id":"embedding-body-id","model":"embed-test","data":[{"index":0,"embedding":[0.25,-0.5]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Request-Id: embedding-header-id\r\n\r\n{}",
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
        assert!(wire.contains("POST /v1/embeddings HTTP/1.1"));
        assert!(wire.contains("\"model\":\"gpt-test\""));
        assert!(wire.contains("\"input\":[\"hello\"]"));
        assert_eq!(result.provider_request_id(), Some("embedding-header-id"));
        assert_eq!(result.model(), "embed-test");
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
            "model": "gpt-test",
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
            r#"{"id":"id","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#,
            &mut state,
        );
        assert!(
            matches!(events.last(), Some(Err(error)) if error.category() == ProviderErrorCategory::Protocol)
        );
    }

    #[tokio::test]
    async fn stream_eof_and_in_stream_errors_preserve_header_request_id() {
        let eof_body = "data: {\"id\":\"body-id\",\"model\":\"gpt-test\",\"choices\":[]}";
        let eof_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nX-Request-Id: eof-header-id\r\n\r\n{}",
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
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nX-Request-Id: stream-error-id\r\n\r\n{}",
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
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Request-Id: malformed-header-id\r\n\r\n{}",
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
    fn dropping_stream_drops_provider_body_without_detached_work() {
        let dropped = Arc::new(AtomicBool::new(false));
        let body = PendingBody {
            dropped: Arc::clone(&dropped),
        };
        let stream = openai_sse_stream(futures::stream::once(body), None, 1024);
        drop(stream);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_http_stream_disconnects_without_completion_or_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        let (status_sender, status_receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("initial request");
            let _request = read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\ndata: {\"id\":\"body-id\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n",
                )
                .expect("first event");
            stream.flush().expect("flush first event");
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_millis(100)))
                .expect("write timeout");
            let deadline = Instant::now() + Duration::from_secs(3);
            let disconnected = loop {
                let mut byte = [0_u8; 1];
                match stream.read(&mut byte) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                        ) =>
                    {
                        break true;
                    }
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            || error.kind() == ErrorKind::TimedOut =>
                    {
                        if Instant::now() >= deadline {
                            break false;
                        }
                        match stream.write(b"data: {\"probe\":true}\n\n") {
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                                ) =>
                            {
                                break true;
                            }
                            Err(error)
                                if error.kind() == ErrorKind::WouldBlock
                                    || error.kind() == ErrorKind::TimedOut => {}
                            Err(_) => break false,
                        }
                    }
                    Err(_) => break false,
                }
            };

            // A retry would create another accepted connection. Keep the
            // listener bounded and observe whether one appears after the
            // first provider body was dropped.
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let retry_deadline = Instant::now() + Duration::from_millis(300);
            let mut retry = false;
            while Instant::now() < retry_deadline {
                match listener.accept() {
                    Ok(_) => {
                        retry = true;
                        break;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            status_sender.send((disconnected, retry)).expect("status");
        });

        let config = OpenAiConfig {
            base_url: format!("http://{address}/v1"),
            api_key: "test-api-key".to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        let adapter = OpenAiAdapter::new(&config).expect("adapter");
        let mut stream = adapter
            .chat_stream(context(), request())
            .await
            .expect("stream");
        assert!(matches!(
            stream.next().await,
            Some(Ok(ChatStreamEvent::ResponseStarted(_)))
        ));
        drop(stream);
        drop(adapter);

        let (disconnected, retry) = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                match status_receiver.try_recv() {
                    Ok(status) => break status,
                    Err(mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("status server dropped channel")
                    }
                }
            }
        })
        .await
        .expect("bounded disconnect status");
        assert!(disconnected, "provider peer did not observe stream drop");
        assert!(!retry, "adapter retried after stream cancellation");
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
