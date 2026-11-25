use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::{Client, Response, StatusCode, header};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, fmt, sync::Mutex, time::Duration};
use url::Url;

use crate::{
    config::{AzureAiFoundryConfig, AzureAuth},
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
    anthropic::AnthropicAdapter,
};

const PROVIDER: &str = "azure-ai-foundry";
const MAX_ERROR_BODY: usize = 16 * 1024;
/// The data-plane version that lists a resource's deployments.
const DEPLOYMENTS_API_VERSION: &str = "2022-12-01";
/// At most this many deployments are remembered as wanting
/// `max_completion_tokens`, or as Claude; past it, each new one costs one
/// retry per request.
const MAX_REMEMBERED_DEPLOYMENTS: usize = 1024;

pub(crate) struct AzureAiFoundryAdapter {
    client: Client,
    base_url: Url,
    auth: AzureAuth,
    api_version: String,
    max_response_bytes: usize,
    /// Bounds the wait for an Entra token.
    request_timeout: Duration,
    /// Deployments that refused `max_tokens` for `max_completion_tokens`
    /// (OpenAI's newer models), so later requests send the accepted field
    /// first. A deployment's name need not reveal its model, so this is
    /// learned from the service rather than guessed from the name.
    completion_token_deployments: Mutex<HashSet<String>>,
    /// Claude deployments, which Foundry serves only through Anthropic's
    /// Messages API (`{resource}/anthropic/v1/messages`); the Model
    /// Inference API refuses them with `api_not_supported`.
    claude_deployments: Mutex<HashSet<String>>,
    /// The Messages API on this resource, signed the same way.
    claude: AnthropicAdapter,
    /// The resource's deployments from Resource Manager, for discovery.
    deployments: Option<super::azure_management::DeploymentLister>,
}

impl fmt::Debug for AzureAiFoundryAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureAiFoundryAdapter")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("api_version", &self.api_version)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl AzureAiFoundryAdapter {
    pub(crate) fn new(config: &AzureAiFoundryConfig) -> Result<Self, ProviderError> {
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
        let claude = AnthropicAdapter::for_azure_ai_foundry(
            &crate::config::AnthropicConfig {
                base_url: base_url
                    .join("anthropic")
                    .map_err(|_| protocol_error())?
                    .to_string(),
                api_key: String::new(),
                request_timeout: config.request_timeout,
                connect_timeout: config.connect_timeout,
                max_response_bytes: config.max_response_bytes,
            },
            config.auth.clone(),
        )?;
        let deployments = config
            .management
            .as_ref()
            .map(|management| {
                super::azure_management::DeploymentLister::new(
                    management,
                    &base_url,
                    None,
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
            completion_token_deployments: Mutex::new(HashSet::new()),
            claude_deployments: Mutex::new(HashSet::new()),
            claude,
            deployments,
        })
    }

    /// Unlike Azure OpenAI, the Foundry Model Inference API exposes a fixed,
    /// deployment-agnostic path; the target deployment is selected by the
    /// request body's `model` field instead of a URL path segment.
    fn endpoint(&self, suffix: &str) -> Result<Url, ProviderError> {
        let mut url = self
            .base_url
            .join(&format!("models/{suffix}"))
            .map_err(|_| protocol_error())?;
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);
        Ok(url)
    }

    async fn request(
        &self,
        url: Url,
        context: &InferenceContext,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        self.request_keyed(
            url,
            context,
            context.provider_attempt_idempotency_key.as_str(),
        )
        .await
    }

    async fn request_keyed(
        &self,
        url: Url,
        context: &InferenceContext,
        idempotency_key: &str,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self
            .client
            .post(url)
            .header(header::AUTHORIZATION, self.authorization_header().await?)
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", idempotency_key)
            .header("x-correlation-id", context.correlation_id.0.to_string())
            .header("x-client-request-id", context.correlation_id.0.to_string()))
    }

    /// The key, or an Entra token, as a bearer token.
    async fn authorization_header(&self) -> Result<header::HeaderValue, ProviderError> {
        super::azure_bearer(&self.auth, self.request_timeout).await
    }

    async fn response_error(&self, response: Response) -> ProviderError {
        ErrorReply::read(response).await.into_error()
    }

    /// Sends a chat request, through the Messages API (`claude`) when the
    /// deployment is Claude. A remembered Claude deployment the Messages API
    /// no longer knows (404: its name now holds another model, or nothing)
    /// is forgotten and routed afresh, once; the 404 generated nothing.
    async fn route<T, F, Fut>(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
        stream: bool,
        claude: F,
    ) -> Result<Routed<T>, ProviderError>
    where
        F: Fn(InferenceContext, ChatRequest) -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let deployment = context.deployment.model();
        let known = remembered(&self.claude_deployments, deployment);
        if let Sent::Inference(response) = self.send_chat(context, request, stream).await? {
            return Ok(Routed::Inference(response));
        }
        match claude(context.clone(), request.clone()).await {
            Err(error) if known && error.http_status() == Some(404) => {
                forget(&self.claude_deployments, deployment);
                match self.send_chat(context, request, stream).await? {
                    Sent::Inference(response) => Ok(Routed::Inference(response)),
                    Sent::Claude => claude(context.clone(), request.clone())
                        .await
                        .map(Routed::Claude)
                        .map_err(as_foundry),
                }
            }
            answer => answer.map(Routed::Claude).map_err(as_foundry),
        }
    }

    /// Sends a chat request through the Model Inference API, or says it is
    /// Claude's to send. A deployment that refuses the API with
    /// `api_not_supported` is Claude, and one that refuses `max_tokens` for
    /// `max_completion_tokens` (OpenAI's newer models) is asked once more
    /// with that field; either is remembered. A refusal generates nothing,
    /// so only the request that follows it can.
    async fn send_chat(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<Sent, ProviderError> {
        let deployment = context.deployment.model();
        if remembered(&self.claude_deployments, deployment) {
            return Ok(Sent::Claude);
        }
        let url = self.endpoint("chat/completions")?;
        let limited = request.max_tokens.is_some();
        let completion_tokens =
            limited && remembered(&self.completion_token_deployments, deployment);
        let body = ChatWireRequest::from_domain(context, request, stream, completion_tokens);
        let response = self
            .request_keyed(url.clone(), context, &chat_key(context, completion_tokens))
            .await?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if response.status().is_success() {
            return Ok(Sent::Inference(response));
        }
        let reply = ErrorReply::read(response).await;
        if reply.is_api_not_supported() {
            remember(&self.claude_deployments, deployment);
            return Ok(Sent::Claude);
        }
        if completion_tokens || !limited || !reply.asks_for_completion_tokens() {
            return Err(reply.into_error());
        }
        remember(&self.completion_token_deployments, deployment);
        let body = ChatWireRequest::from_domain(context, request, stream, true);
        let response = self
            .request_keyed(url, context, &chat_key(context, true))
            .await?
            .json(&body)
            .send()
            .await
            .map_err(request_error)?;
        if response.status().is_success() {
            Ok(Sent::Inference(response))
        } else {
            Err(self.response_error(response).await)
        }
    }
}

/// A chat request's answer: the Model Inference API's response, or
/// Claude's from the Messages API.
enum Routed<T> {
    Inference(Response),
    Claude(T),
}

/// An error met on the Messages API, reported as this resource's: the
/// request was the Foundry deployment's, not Anthropic's.
fn as_foundry(error: ProviderError) -> ProviderError {
    error.with_provider(PROVIDER)
}

/// Where a chat request went.
enum Sent {
    /// The Model Inference API answered.
    Inference(Response),
    /// The deployment is Claude: the Messages API is to be asked.
    Claude,
}

fn remembered(deployments: &Mutex<HashSet<String>>, deployment: &str) -> bool {
    deployments
        .lock()
        .is_ok_and(|deployments| deployments.contains(deployment))
}

fn forget(deployments: &Mutex<HashSet<String>>, deployment: &str) {
    if let Ok(mut deployments) = deployments.lock() {
        deployments.remove(deployment);
    }
}

fn remember(deployments: &Mutex<HashSet<String>>, deployment: &str) {
    if let Ok(mut deployments) = deployments.lock()
        && deployments.len() < MAX_REMEMBERED_DEPLOYMENTS
    {
        deployments.insert(deployment.to_owned());
    }
}

/// The idempotency key of a chat request. A body with
/// `max_completion_tokens` is a different request from the attempt's
/// `max_tokens` one, so it has its own key, which a retry of the attempt
/// (sent with that field once the deployment is remembered) reuses: the
/// provider never matches either body to the other's key.
fn chat_key(context: &InferenceContext, completion_tokens: bool) -> String {
    let key = context.provider_attempt_idempotency_key.as_str();
    if completion_tokens {
        format!("{key}-mct")
    } else {
        key.to_owned()
    }
}

/// A failed response, read once so it can both be inspected and become the
/// error.
struct ErrorReply {
    status: StatusCode,
    request_id: Option<String>,
    retry_after: Option<Duration>,
    body: Value,
}

impl ErrorReply {
    async fn read(response: Response) -> Self {
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
        Self {
            status,
            request_id,
            retry_after,
            body: serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
        }
    }

    /// The Foundry Model Inference API's own errors are flat (`{"status",
    /// "code", "message", "param"}`); the OpenAI models it serves answer in
    /// OpenAI's nested `{"error": {...}}` shape. Either is read.
    fn detail(&self) -> &Value {
        self.body
            .get("error")
            .filter(|error| error.is_object())
            .unwrap_or(&self.body)
    }

    /// The Model Inference API's refusal of a deployment it cannot serve,
    /// such as Claude.
    fn is_api_not_supported(&self) -> bool {
        self.status == StatusCode::NOT_FOUND
            && self.detail().get("code").and_then(Value::as_str) == Some("api_not_supported")
    }

    fn asks_for_completion_tokens(&self) -> bool {
        let detail = self.detail();
        self.status == StatusCode::BAD_REQUEST
            && detail.get("code").and_then(Value::as_str) == Some("unsupported_parameter")
            && detail.get("param").and_then(Value::as_str) == Some("max_tokens")
    }

    fn into_error(self) -> ProviderError {
        let provider_code = self
            .detail()
            .get("code")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        ProviderError::from_parts(
            category_for_status(self.status),
            Some(PROVIDER.to_owned()),
            self.request_id,
            Some(self.status.as_u16()),
            provider_code,
            self.retry_after,
            None,
        )
        .unwrap_or_else(|_| ProviderError::new(ProviderErrorCategory::Protocol))
    }
}

#[async_trait]
impl InferenceProviderAdapter for AzureAiFoundryAdapter {
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
        let builder = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.authorization_header().await?);
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
        // The Foundry Model Inference API has no provider-side token
        // counting operation. Do not approximate or make a generation
        // request here.
        Err(ProviderError::new(
            ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    /// A deployment known to be Claude takes the Messages API's text-only
    /// conversations; one not yet known is checked when Foundry routes it.
    fn validate_chat_request(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        if remembered(&self.claude_deployments, context.deployment.model()) {
            self.claude.validate_chat_request(context, request)
        } else {
            Ok(())
        }
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let response = match self
            .route(&context, &request, false, |context, request| {
                self.claude.chat(context, request)
            })
            .await?
        {
            Routed::Inference(response) => response,
            Routed::Claude(response) => return Ok(response),
        };
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
        let response = match self
            .route(&context, &request, true, |context, request| {
                self.claude.chat_stream(context, request)
            })
            .await?
        {
            Routed::Inference(response) => response,
            // Errors met mid-stream are this resource's too.
            Routed::Claude(stream) => {
                return Ok(ValidatingChatEventStream::new(
                    stream.map(|item| item.map_err(as_foundry)),
                ));
            }
        };
        let request_id = response
            .headers()
            .get("apim-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = response.bytes_stream();
        let stream = azure_ai_foundry_sse_stream(bytes, request_id, self.max_response_bytes);
        Ok(ValidatingChatEventStream::new(stream))
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        let deployment = context.deployment.model();
        let url = self.endpoint("embeddings")?;
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

#[derive(Serialize)]
struct ChatWireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
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
    /// The output limit goes in `max_tokens`, the Model Inference API's
    /// field, or in `max_completion_tokens` for a deployment that asked for
    /// it.
    fn from_domain(
        context: &InferenceContext,
        request: &ChatRequest,
        stream: bool,
        completion_tokens: bool,
    ) -> Self {
        Self {
            // The Foundry deployment is selected by this body field, not a
            // URL path segment; the service matches it against the
            // deployment's configured name.
            model: context.deployment.model().to_owned(),
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
            max_tokens: request.max_tokens.filter(|_| !completion_tokens),
            max_completion_tokens: request.max_tokens.filter(|_| completion_tokens),
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
        ("id", "azure_ai_foundry_response_id"),
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

fn azure_ai_foundry_sse_stream<S>(
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
    if value.get("error").is_some() || value.get("code").is_some() {
        return vec![Err(parse_stream_error(&value))];
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Azure first sends the prompt's content-filter results
    // (`prompt_filter_results`) in a chunk with no model, choices or usage:
    // nothing to relay.
    let no_choices = value
        .get("choices")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if model.is_empty() && no_choices && value.get("usage").is_none_or(Value::is_null) {
        return Vec::new();
    }
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
    // The flat error envelope may also appear inline in an SSE data frame;
    // fall back to a nested `error` object only if present, matching
    // `response_error`'s top-level-first read order.
    let error = value.get("error").unwrap_or(value);
    let provider_code = error
        .get("code")
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
    ) -> (AzureAiFoundryAdapter, mpsc::Receiver<String>) {
        let address = listener.local_addr().expect("listener address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let request = read_request(&mut stream);
            sender.send(request).expect("request receiver");
            stream.write_all(response.as_bytes()).expect("response");
        });
        let config = AzureAiFoundryConfig {
            base_url: format!("http://{address}"),
            auth: crate::config::AzureAuth::ApiKey("test-api-key".to_owned()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-05-01-preview".to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        (
            AzureAiFoundryAdapter::new(&config).expect("adapter"),
            receiver,
        )
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
            DeploymentModelReference::new("deployment", "foundry-deployment").expect("deployment"),
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

    #[tokio::test]
    async fn chat_count_is_unsupported_without_network() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let config = AzureAiFoundryConfig {
            base_url: format!("http://{address}"),
            auth: crate::config::AzureAuth::ApiKey("test-api-key".to_owned()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-05-01-preview".to_owned(),
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(50),
            max_response_bytes: 1024,
        };
        let adapter = AzureAiFoundryAdapter::new(&config).expect("adapter");
        let error = adapter
            .count_chat_input_tokens(&context(), &request())
            .await
            .expect_err("Foundry Model Inference must not count or generate");
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    }

    #[tokio::test]
    async fn chat_uses_fixed_path_body_model_and_bearer_auth() {
        let body = r#"{"id":"chat-body-id","model":"foundry-deployment","choices":[{"index":0,"message":{"role":"assistant","content":"hello","refusal":"no","annotations":[{"type":"url_citation","url_citation":{"url":"https://example.invalid"}}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#;
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
        assert!(
            wire.contains("POST /models/chat/completions?api-version=2024-05-01-preview HTTP/1.1")
        );
        assert!(
            wire.to_ascii_lowercase()
                .contains("authorization: bearer test-api-key")
        );
        assert!(!wire.to_ascii_lowercase().contains("api-key:"));
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
        assert!(wire.contains("\"model\":\"foundry-deployment\""));
        assert!(wire.contains("\"max_tokens\":12"));
        assert_eq!(result.provider_request_id(), Some("chat-header-id"));
        assert_eq!(result.choices()[0].message.content, "hello");
        assert_eq!(result.choices()[0].refusal.as_deref(), Some("no"));
        assert_eq!(result.choices()[0].annotations[0].kind, "url_citation");
        assert_eq!(
            serde_json::to_value(result.metadata()).expect("safe metadata")["azure_ai_foundry_response_id"],
            "chat-body-id"
        );
    }

    #[tokio::test]
    async fn stream_emits_usage_and_completes_only_at_done() {
        let body = concat!(
            "data: {\"id\":\"chat-stream-id\",\"model\":\"foundry-deployment\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\",\"annotations\":[{\"type\":\"url_citation\",\"value\":\"cite\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat-stream-id\",\"model\":\"foundry-deployment\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chat-stream-id\",\"model\":\"foundry-deployment\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n",
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

    /// Answers one request per response, in order, on fresh connections.
    fn make_adapter_serving(
        responses: Vec<String>,
    ) -> (AzureAiFoundryAdapter, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("listener address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("request");
                let request = read_request(&mut stream);
                sender.send(request).expect("request receiver");
                stream.write_all(response.as_bytes()).expect("response");
            }
        });
        let config = AzureAiFoundryConfig {
            base_url: format!("http://{address}"),
            auth: crate::config::AzureAuth::ApiKey("test-api-key".to_owned()),
            identity: Some(crate::azure_credentials::tests::unused_identity()),
            management: None,
            api_version: "2024-05-01-preview".to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        (
            AzureAiFoundryAdapter::new(&config).expect("adapter"),
            receiver,
        )
    }

    fn closing(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    const MAX_TOKENS_REFUSED: &str = r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","param":"max_tokens","code":"unsupported_parameter"}}"#;
    const CHAT_OK: &str = r#"{"id":"chat-id","model":"foundry-deployment","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;

    #[tokio::test]
    async fn a_deployment_refusing_max_tokens_is_asked_with_max_completion_tokens_and_remembered() {
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("400 Bad Request", "application/json", MAX_TOKENS_REFUSED),
            closing("200 OK", "application/json", CHAT_OK),
            closing("200 OK", "application/json", CHAT_OK),
        ]);
        let context = context();
        let key = context.provider_attempt_idempotency_key.as_str().to_owned();
        let result = adapter.chat(context, request()).await.expect("chat");
        assert_eq!(result.choices()[0].message.content, "ok");
        let refused = receiver.recv().expect("first request");
        assert!(refused.contains("\"max_tokens\":12"));
        assert!(!refused.contains("max_completion_tokens"));
        let retried = receiver.recv().expect("second request");
        assert!(retried.contains("\"max_completion_tokens\":12"));
        assert!(!retried.contains("\"max_tokens\""));
        assert!(
            retried
                .to_ascii_lowercase()
                .contains(&format!("idempotency-key: {key}-mct"))
        );

        // The next request to the deployment sends the accepted field first.
        adapter
            .chat(context_for_next(), request())
            .await
            .expect("chat");
        let next = receiver.recv().expect("third request");
        assert!(next.contains("\"max_completion_tokens\":12"));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_retried_attempt_sends_max_completion_tokens_under_its_own_key() {
        let busy = r#"{"error":{"code":"rate_limited","message":"busy"}}"#;
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("400 Bad Request", "application/json", MAX_TOKENS_REFUSED),
            closing("429 Too Many Requests", "application/json", busy),
            closing("200 OK", "application/json", CHAT_OK),
        ]);
        let key = context()
            .provider_attempt_idempotency_key
            .as_str()
            .to_owned();
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("fallback rate limited");
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        // The retry layer asks again with the same attempt.
        adapter.chat(context(), request()).await.expect("retry");
        let refused = receiver.recv().expect("refused");
        assert!(
            refused
                .to_ascii_lowercase()
                .contains(&format!("idempotency-key: {key}\r\n"))
        );
        for _ in 0..2 {
            let sent = receiver.recv().expect("max_completion_tokens request");
            assert!(sent.contains("\"max_completion_tokens\":12"));
            assert!(
                sent.to_ascii_lowercase()
                    .contains(&format!("idempotency-key: {key}-mct"))
            );
        }
    }

    #[tokio::test]
    async fn a_stream_is_retried_with_max_completion_tokens() {
        let stream = concat!(
            "data: {\"id\":\"s\",\"model\":\"foundry-deployment\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"s\",\"model\":\"foundry-deployment\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        );
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("400 Bad Request", "application/json", MAX_TOKENS_REFUSED),
            closing("200 OK", "text/event-stream", stream),
        ]);
        let events = adapter
            .chat_stream(context(), request())
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
        let _refused = receiver.recv().expect("first request");
        let retried = receiver.recv().expect("second request");
        assert!(retried.contains("\"max_completion_tokens\":12"));
        assert!(retried.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn other_bad_requests_and_unlimited_requests_are_not_retried() {
        let other = r#"{"error":{"message":"bad temperature","type":"invalid_request_error","param":"temperature","code":"unsupported_parameter"}}"#;
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("400 Bad Request", "application/json", other),
            closing("400 Bad Request", "application/json", MAX_TOKENS_REFUSED),
        ]);
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("other refusal");
        assert_eq!(error.http_status(), Some(400));
        // The nested OpenAI envelope's code is read too.
        assert_eq!(error.provider_code(), Some("unsupported_parameter"));
        let _first = receiver.recv().expect("first request");

        // Without an output limit there is nothing to rename.
        let unlimited = ChatRequest {
            max_tokens: None,
            ..request()
        };
        adapter
            .chat(context_for_next(), unlimited)
            .await
            .expect_err("refusal without a limit");
        let second = receiver.recv().expect("second request");
        assert!(!second.contains("max_tokens"));
        assert!(receiver.try_recv().is_err());
    }

    const API_NOT_SUPPORTED: &str = r#"{"error":{"code":"api_not_supported","message":"Requested API is currently not supported","details":"Requested API is currently not supported"}}"#;
    const CLAUDE_OK: &str = r#"{"type":"message","id":"msg_1","role":"assistant","model":"foundry-deployment","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}"#;

    #[tokio::test]
    async fn a_claude_deployment_is_sent_to_the_messages_api_and_remembered() {
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("404 Not Found", "application/json", API_NOT_SUPPORTED),
            closing("200 OK", "application/json", CLAUDE_OK),
            closing("200 OK", "application/json", CLAUDE_OK),
        ]);
        let result = adapter.chat(context(), request()).await.expect("chat");
        assert_eq!(result.choices()[0].message.content, "ok");
        let refused = receiver.recv().expect("inference request");
        assert!(refused.starts_with("POST /models/chat/completions?"));
        let claude = receiver.recv().expect("messages request");
        assert!(claude.starts_with("POST /anthropic/v1/messages HTTP/1.1"));
        let lower = claude.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer test-api-key"));
        assert!(!lower.contains("x-api-key"));
        assert!(lower.contains("anthropic-version: 2023-06-01"));
        assert!(claude.contains("\"model\":\"foundry-deployment\""));
        assert!(claude.contains("\"max_tokens\":12"));

        // Known now: straight to the Messages API.
        adapter
            .chat(context_for_next(), request())
            .await
            .expect("chat");
        let next = receiver.recv().expect("third request");
        assert!(next.starts_with("POST /anthropic/v1/messages HTTP/1.1"));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_claude_stream_is_sent_to_the_messages_api() {
        let stream = concat!(
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
        );
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("404 Not Found", "application/json", API_NOT_SUPPORTED),
            closing("200 OK", "text/event-stream", stream),
        ]);
        let events = adapter
            .chat_stream(context(), request())
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
        let _refused = receiver.recv().expect("inference request");
        let claude = receiver.recv().expect("messages request");
        assert!(claude.starts_with("POST /anthropic/v1/messages HTTP/1.1"));
        assert!(claude.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn a_claude_name_reused_for_another_model_is_routed_afresh() {
        let gone = r#"{"error":{"code":"DeploymentNotFound","message":"not a Claude deployment"}}"#;
        let (adapter, receiver) = make_adapter_serving(vec![
            closing("404 Not Found", "application/json", API_NOT_SUPPORTED),
            closing("200 OK", "application/json", CLAUDE_OK),
            // The name now holds a model the Messages API does not serve.
            closing("404 Not Found", "application/json", gone),
            closing("200 OK", "application/json", CHAT_OK),
            closing("200 OK", "application/json", CHAT_OK),
        ]);
        adapter.chat(context(), request()).await.expect("claude");
        let result = adapter
            .chat(context_for_next(), request())
            .await
            .expect("rerouted");
        assert_eq!(result.choices()[0].message.content, "ok");
        let paths = (0..4)
            .map(|_| {
                receiver
                    .recv()
                    .expect("request")
                    .split(' ')
                    .nth(1)
                    .expect("path")
                    .split('?')
                    .next()
                    .expect("path")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "/models/chat/completions",
                "/anthropic/v1/messages",
                "/anthropic/v1/messages",
                "/models/chat/completions",
            ]
        );
        // Forgotten: the next request goes to the Model Inference API.
        adapter.chat(context(), request()).await.expect("inference");
        assert!(
            receiver
                .recv()
                .expect("request")
                .starts_with("POST /models/chat/completions?")
        );
    }

    #[tokio::test]
    async fn messages_api_errors_are_reported_as_the_foundry_resource() {
        let busy = r#"{"type":"error","error":{"type":"rate_limit_error","message":"busy"}}"#;
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n"
        );
        let (adapter, _receiver) = make_adapter_serving(vec![
            closing("404 Not Found", "application/json", API_NOT_SUPPORTED),
            closing("429 Too Many Requests", "application/json", busy),
            closing("200 OK", "text/event-stream", stream),
        ]);
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("rate limited");
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        assert_eq!(error.provider(), Some(PROVIDER));

        let events = adapter
            .chat_stream(context_for_next(), request())
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;
        let stream_error = events
            .iter()
            .find_map(|event| event.as_ref().err())
            .expect("stream error");
        assert_eq!(stream_error.provider(), Some(PROVIDER));
    }

    #[tokio::test]
    async fn a_missing_deployment_is_not_sent_to_the_messages_api() {
        let missing = r#"{"error":{"code":"DeploymentNotFound","message":"The API deployment foundry-deployment does not exist."}}"#;
        let (adapter, receiver) =
            make_adapter_serving(vec![closing("404 Not Found", "application/json", missing)]);
        let error = adapter
            .chat(context(), request())
            .await
            .expect_err("missing deployment");
        assert_eq!(error.http_status(), Some(404));
        assert_eq!(error.provider_code(), Some("DeploymentNotFound"));
        let _only = receiver.recv().expect("inference request");
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_known_claude_deployment_is_validated_as_claude() {
        let (adapter, _receiver) = make_adapter_serving(vec![
            closing("404 Not Found", "application/json", API_NOT_SUPPORTED),
            closing("200 OK", "application/json", CLAUDE_OK),
        ]);
        let tool_turn = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::Developer,
                content: "result".to_owned(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            ..request()
        };
        assert!(
            adapter
                .validate_chat_request(&context(), &tool_turn)
                .is_ok()
        );
        adapter.chat(context(), request()).await.expect("chat");
        assert!(
            adapter
                .validate_chat_request(&context(), &tool_turn)
                .is_err()
        );
    }

    fn context_for_next() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt-456").expect("key"),
            DeploymentModelReference::new("deployment", "foundry-deployment").expect("deployment"),
        )
    }

    /// GPT-6 on Foundry, as captured on 2026-11-02 (trimmed of routing and
    /// latency fields): a prompt-filter preamble without a model first.
    #[tokio::test]
    async fn a_prompt_filter_preamble_is_skipped() {
        let body = concat!(
            "data: {\"choices\":[],\"created\":0,\"id\":\"\",\"model\":\"\",\"object\":\"\",\"prompt_filter_results\":[{\"prompt_index\":0,\"content_filter_results\":{\"hate\":{\"filtered\":false,\"severity\":\"safe\"},\"jailbreak\":{\"detected\":false,\"filtered\":false}}}]}\n\n",
            "data: {\"choices\":[{\"content_filter_results\":{},\"delta\":{\"content\":\"\",\"refusal\":null,\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0,\"logprobs\":null}],\"created\":1793653306,\"id\":\"chatcmpl-1\",\"model\":\"gpt-6-luna-2026-09-22\",\"obfuscation\":\"3DHAZ\",\"object\":\"chat.completion.chunk\",\"service_tier\":\"default\",\"system_fingerprint\":null,\"usage\":null}\n\n",
            "data: {\"choices\":[{\"content_filter_results\":{\"protected_material_text\":{\"detected\":false,\"filtered\":false}},\"delta\":{\"content\":\"ok\"},\"finish_reason\":null,\"index\":0,\"logprobs\":null}],\"created\":1793653306,\"id\":\"chatcmpl-1\",\"model\":\"gpt-6-luna-2026-09-22\",\"obfuscation\":\"74JWH\",\"object\":\"chat.completion.chunk\",\"usage\":null}\n\n",
            "data: {\"choices\":[{\"content_filter_results\":{},\"delta\":{},\"finish_reason\":\"stop\",\"index\":0,\"logprobs\":null}],\"created\":1793653306,\"id\":\"chatcmpl-1\",\"model\":\"gpt-6-luna-2026-09-22\",\"obfuscation\":\"E\",\"object\":\"chat.completion.chunk\",\"usage\":null}\n\n",
            "data: {\"choices\":[],\"created\":1793653306,\"id\":\"chatcmpl-1\",\"latency_checkpoint\":{\"engine_ttft_ms\":72},\"model\":\"gpt-6-luna-2026-09-22\",\"obfuscation\":\"hP\",\"object\":\"chat.completion.chunk\",\"usage\":{\"completion_tokens\":17,\"completion_tokens_details\":{\"reasoning_tokens\":7},\"prompt_tokens\":13,\"prompt_tokens_details\":{\"cached_tokens\":0},\"total_tokens\":30}}\n\n",
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
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let text = events
            .iter()
            .filter_map(|event| match event {
                Ok(ChatStreamEvent::ChoiceDelta(delta)) => delta.content_delta.clone(),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "ok");
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
    }

    #[tokio::test]
    async fn a_chunk_without_a_model_that_carries_choices_is_refused() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null,\"index\":0}],\"model\":\"\"}\n\n",
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
        assert!(
            matches!(events.first(), Some(Err(error)) if error.category() == ProviderErrorCategory::Protocol)
        );
    }

    #[tokio::test]
    async fn flat_error_envelope_is_mapped_without_retry() {
        let body = r#"{"status":429,"code":"rate_limited","message":"too many requests"}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nApim-Request-Id: rate-limit-id\r\n\r\n{}",
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
        assert_eq!(error.provider_code(), Some("rate_limited"));
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
        assert!(wire.contains("POST /models/embeddings?api-version=2024-05-01-preview HTTP/1.1"));
        assert!(wire.contains("\"model\":\"foundry-deployment\""));
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
            "model": "foundry-deployment",
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
            r#"{"id":"id","model":"foundry-deployment","choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#,
            &mut state,
        );
        assert!(
            matches!(events.last(), Some(Err(error)) if error.category() == ProviderErrorCategory::Protocol)
        );
    }

    #[test]
    fn endpoint_is_fixed_and_appends_api_version() {
        let (adapter, _receiver) = make_adapter(
            TcpListener::bind("127.0.0.1:0").expect("listener"),
            String::new(),
        );
        let url = adapter.endpoint("chat/completions").unwrap();
        assert_eq!(url.path(), "/models/chat/completions");
        assert_eq!(url.query(), Some("api-version=2024-05-01-preview"));
    }

    #[tokio::test]
    async fn stream_eof_and_in_stream_errors_preserve_header_request_id() {
        let eof_body = "data: {\"id\":\"body-id\",\"model\":\"foundry-deployment\",\"choices\":[]}";
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

        let error_body =
            "data: {\"status\":502,\"code\":\"upstream_failed\",\"message\":\"bad gateway\"}\n\n";
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
        assert_eq!(error.category(), ProviderErrorCategory::Unavailable);
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
    fn tools_and_tool_calls_go_on_the_wire_as_openai_documents() {
        use crate::domain::inference::{ChatMessage, ChatRole, ToolChoice};
        let mut request = crate::provider::tools::tool_conversation();
        request.tool_choice = Some(ToolChoice::Required);
        request.parallel_tool_calls = Some(false);
        let body = serde_json::to_value(ChatWireRequest::from_domain(
            &context(),
            &request,
            false,
            false,
        ))
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
        let body = serde_json::to_value(ChatWireRequest::from_domain(
            &context(),
            &request,
            false,
            false,
        ))
        .unwrap();
        assert!(body["messages"][2]["content"].is_null());
        // A request without tools sends none of the fields.
        let plain = ChatRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "hi")],
            ..Default::default()
        };
        let body = serde_json::to_value(ChatWireRequest::from_domain(
            &context(),
            &plain,
            false,
            false,
        ))
        .unwrap();
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }
}
