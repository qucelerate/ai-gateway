//! Request and response types of the inference contract 1.0.0.
//!
//! Requests serialize to exactly the documented fields. Responses ignore
//! fields they do not know, so a later 1.x gateway that adds fields keeps
//! working; the structs are `#[non_exhaustive]` so this crate can add them
//! too without a breaking release.

use serde::{Deserialize, Serialize};

/// Who a chat message is from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    /// A tool's result, answering one of the assistant's tool calls.
    Tool,
}

/// One message of a chat request. An assistant message that called tools
/// carries its [`ToolCall`]s (an empty content is then sent as null); a
/// [`Role::Tool`] message answers one of them by `tool_call_id`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(deserialize_with = "null_as_empty")]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        if self.content.is_empty() && !self.tool_calls.is_empty() {
            map.serialize_entry("content", &())?;
        } else {
            map.serialize_entry("content", &self.content)?;
        }
        if !self.tool_calls.is_empty() {
            map.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        map.end()
    }
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn developer(content: impl Into<String>) -> Self {
        Self::new(Role::Developer, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
    /// The assistant's turn that called `tool_calls`, to send back before
    /// their results.
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::new(Role::Assistant, content)
        }
    }
    /// A tool's result, answering the call whose id is `tool_call_id`.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            ..Self::new(Role::Tool, content)
        }
    }
}

/// A call the model made to one of the request's tools, OpenAI-shaped.
/// Send it back unchanged; `function.arguments` is a JSON object,
/// serialized, as the model wrote it: validate it before acting on it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

fn function_type() -> String {
    "function".to_owned()
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: function_type(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

/// A function the model may call. `parameters` is a JSON Schema whose
/// `type` is `object`; `None` sends none (the gateway takes an empty
/// object).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Tool {
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

impl Tool {
    pub fn function(name: impl Into<String>) -> Self {
        Self {
            kind: function_type(),
            function: FunctionDefinition {
                name: name.into(),
                description: None,
                parameters: None,
            },
        }
    }
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.function.description = Some(description.into());
        self
    }
    pub fn parameters(mut self, schema: serde_json::Value) -> Self {
        self.function.parameters = Some(schema);
        self
    }
}

/// Whether the model must call tools.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolChoice {
    /// The model decides (the default).
    Auto,
    /// The model calls none.
    None,
    /// The model calls at least one.
    Required,
    /// The model calls this function.
    Function(String),
}

impl Serialize for ToolChoice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Required => serializer.serialize_str("required"),
            Self::Function(name) => {
                serde_json::json!({"type": "function", "function": {"name": name}})
                    .serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(mode) => match mode.as_str() {
                "auto" => Ok(Self::Auto),
                "none" => Ok(Self::None),
                "required" => Ok(Self::Required),
                _ => Err(serde::de::Error::custom("unknown tool_choice")),
            },
            _ => value["function"]["name"]
                .as_str()
                .map(|name| Self::Function(name.to_owned()))
                .ok_or_else(|| serde::de::Error::custom("tool_choice names no function")),
        }
    }
}

/// `POST /api/inference/v1/chat/completions` and its `/stream` variant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ChatRequest {
    /// A model alias offered to the caller, such as `chat-default`.
    pub model: String,
    pub messages: Vec<Message>,
    /// 0 to 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// 1 to 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Functions the model may call. A model alias that cannot take tools
    /// refuses them (`tools_unsupported`), never drops them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// `false` asks for at most one call per response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
        }
    }
    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
    pub fn tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }
    pub fn tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }
    pub fn parallel_tool_calls(mut self, parallel: bool) -> Self {
        self.parallel_tool_calls = Some(parallel);
        self
    }
}

/// `POST /api/inference/v1/embeddings`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct EmbeddingRequest {
    pub model: String,
    /// 1 to 128 non-empty strings.
    pub input: Vec<String>,
}

impl EmbeddingRequest {
    pub fn new<I, S>(model: impl Into<String>, input: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            model: model.into(),
            input: input.into_iter().map(Into::into).collect(),
        }
    }
}

/// How the gateway counted the usage it reports.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageQuality {
    Exact,
    Estimated,
    Unknown,
}

/// Whether every counter of a usage report is known.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageCompleteness {
    Complete,
    Partial,
    Unavailable,
}

/// Token usage. A counter the provider did not report is `None`, never 0.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Generated tokens other than reasoning.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    /// Reasoning tokens counted apart from `output_tokens`; `None` when the
    /// provider does not report them, in which case any reasoning is inside
    /// `output_tokens`.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub other_tokens: Option<u64>,
    pub quality: UsageQuality,
    pub completeness: UsageCompleteness,
}

/// The assistant message of a choice.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct MessageResponse {
    pub role: String,
    /// Empty when the model returned only tool calls or a refusal.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub content: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// The calls the model made; empty when it made none.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

impl MessageResponse {
    /// This response as the assistant message that continues the
    /// conversation, its tool calls included.
    pub fn to_message(&self) -> Message {
        Message::assistant_tool_calls(self.content.clone(), self.tool_calls.clone())
    }
}

/// One choice of a chat response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Choice {
    pub index: u32,
    pub message: MessageResponse,
    /// An open string (`stop`, `length`, `tool_calls`, ...): handle values
    /// you do not know.
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub safety: Vec<serde_json::Value>,
    #[serde(default)]
    pub annotations: Vec<serde_json::Value>,
}

/// A chat completion.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The `X-Request-Id` response header: quote it when reporting a problem.
    #[serde(default, skip_deserializing)]
    pub request_id: Option<String>,
}

impl ChatResponse {
    /// The first choice's message content, the usual answer.
    pub fn content(&self) -> Option<&str> {
        self.choices
            .first()
            .map(|choice| choice.message.content.as_str())
    }
}

/// One embedding vector.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Embedding {
    pub object: String,
    pub index: u32,
    pub embedding: Vec<f32>,
}

/// An embeddings response: one vector per input, in input order.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct EmbeddingResponse {
    pub object: String,
    pub model: String,
    pub data: Vec<Embedding>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The `X-Request-Id` response header.
    #[serde(default, skip_deserializing)]
    pub request_id: Option<String>,
}

/// `response.started`: the first event of a stream.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ResponseStarted {
    pub model: String,
}

/// `choice.delta`: more content or refusal text for a choice.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ChoiceDelta {
    pub choice_index: u32,
    pub content_delta: Option<String>,
    pub refusal_delta: Option<String>,
    #[serde(default)]
    pub annotations: Vec<serde_json::Value>,
}

/// `tool_call.delta`: part of a tool call; `id` and `name` arrive once.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ToolCallDelta {
    pub choice_index: u32,
    pub tool_call_index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_delta: String,
}

/// `choice.finished`: a choice is complete.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ChoiceFinished {
    pub choice_index: u32,
    /// An open string; see [`Choice::finish_reason`].
    pub finish_reason: String,
}

/// `safety.annotation`: a provider safety signal for a choice.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct SafetyAnnotation {
    pub choice_index: u32,
    pub category: String,
    pub blocked: bool,
}

/// `response.completed`: the stream finished successfully.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ResponseCompleted {
    pub model: String,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// An event of a chat stream. Events this version does not know are
/// skipped; a terminal `error` event ends the stream as
/// [`Error::Stream`](crate::Error::Stream).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum StreamEvent {
    ResponseStarted(ResponseStarted),
    ChoiceDelta(ChoiceDelta),
    ToolCallDelta(ToolCallDelta),
    ChoiceFinished(ChoiceFinished),
    SafetyAnnotation(SafetyAnnotation),
    Usage(Usage),
    /// The last event of a successful stream.
    ResponseCompleted(ResponseCompleted),
}

impl StreamEvent {
    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ResponseStarted(_) => "response.started",
            Self::ChoiceDelta(_) => "choice.delta",
            Self::ToolCallDelta(_) => "tool_call.delta",
            Self::ChoiceFinished(_) => "choice.finished",
            Self::SafetyAnnotation(_) => "safety.annotation",
            Self::Usage(_) => "usage",
            Self::ResponseCompleted(_) => "response.completed",
        }
    }

    /// The event's data in wire form.
    pub fn data(&self) -> serde_json::Value {
        let value = match self {
            Self::ResponseStarted(data) => serde_json::to_value(data),
            Self::ChoiceDelta(data) => serde_json::to_value(data),
            Self::ToolCallDelta(data) => serde_json::to_value(data),
            Self::ChoiceFinished(data) => serde_json::to_value(data),
            Self::SafetyAnnotation(data) => serde_json::to_value(data),
            Self::Usage(data) => serde_json::to_value(data),
            Self::ResponseCompleted(data) => serde_json::to_value(data),
        };
        value.expect("stream events always serialize")
    }

    pub(crate) fn decode(name: &str, data: &str) -> Option<Result<Self, serde_json::Error>> {
        fn parse<T: serde::de::DeserializeOwned>(
            data: &str,
            wrap: fn(T) -> StreamEvent,
        ) -> Option<Result<StreamEvent, serde_json::Error>> {
            Some(serde_json::from_str(data).map(wrap))
        }
        match name {
            "response.started" => parse(data, Self::ResponseStarted),
            "choice.delta" => parse(data, Self::ChoiceDelta),
            "tool_call.delta" => parse(data, Self::ToolCallDelta),
            "choice.finished" => parse(data, Self::ChoiceFinished),
            "safety.annotation" => parse(data, Self::SafetyAnnotation),
            "usage" => parse(data, Self::Usage),
            "response.completed" => parse(data, Self::ResponseCompleted),
            _ => None,
        }
    }
}

/// An uploaded batch input file, or a batch's output or error file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct FileObject {
    pub id: String,
    pub bytes: u64,
    pub filename: String,
    pub purpose: String,
    pub created_at: i64,
}

/// The endpoint every line of a batch calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum BatchEndpoint {
    #[serde(rename = "/v1/chat/completions")]
    ChatCompletions,
    #[serde(rename = "/v1/embeddings")]
    Embeddings,
}

/// A request to create a batch from an uploaded file.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct BatchRequest {
    pub input_file_id: String,
    pub endpoint: BatchEndpoint,
    /// Always `24h`.
    pub completion_window: &'static str,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, String>,
}

impl BatchRequest {
    pub fn new(input_file_id: impl Into<String>, endpoint: BatchEndpoint) -> Self {
        Self {
            input_file_id: input_file_id.into(),
            endpoint,
            completion_window: "24h",
            metadata: Default::default(),
        }
    }

    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// A batch's line counts.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct BatchCounts {
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
}

/// A batch: OpenAI's batch object. `status` is an open string (validating,
/// in_progress, finalizing, completed, failed, expired, cancelling,
/// cancelled).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Batch {
    pub id: String,
    pub status: String,
    pub endpoint: String,
    pub input_file_id: String,
    #[serde(default)]
    pub output_file_id: Option<String>,
    #[serde(default)]
    pub error_file_id: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub request_counts: BatchCounts,
    #[serde(default)]
    pub metadata: Option<std::collections::BTreeMap<String, String>>,
    /// The gateway's own account, on a single-batch read: each job's `mode`
    /// (native or emulated) and `price_source`, and `cost` per currency.
    #[serde(default)]
    pub gateway: Option<serde_json::Value>,
    #[serde(default, skip_deserializing)]
    pub request_id: Option<String>,
}

impl Batch {
    /// Whether the batch has ended: its output and error files are final.
    pub fn done(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "failed" | "expired" | "cancelled"
        )
    }
}

/// A page of batches, newest first.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub struct BatchList {
    pub data: Vec<Batch>,
    #[serde(default)]
    pub has_more: bool,
}
