//! The public request bodies of the single-request inference endpoints and
//! their bounds. A batch input line's `body` is the same request, so batch
//! validation applies exactly these rules.

use serde::{Deserialize, Serialize};

use std::collections::HashSet;

use serde_json::Value;

use super::inference::{ChatChoice, EmbeddingResponse, FinishReason};

use super::inference::{
    ChatMessage, ChatRequest, ChatRole, EmbeddingRequest, ToolCall, ToolChoice, ToolDefinition,
};

pub const MAX_MESSAGES: usize = 128;
pub const MAX_EMBEDDINGS: usize = 128;
pub const MAX_MODEL_ALIAS_CHARS: usize = 128;
pub const MAX_TEXT_BYTES: usize = 128 * 1024;
/// A chat request's messages together: the largest inference body the
/// gateway can be configured to read (`GATEWAY_INFERENCE_MAX_BODY_BYTES`,
/// at most 64 MiB). The configured body limit and each deployment's
/// `max_input_bytes` are what bound a prompt in practice; a 128 KiB bound
/// here would refuse prompts past about 30,000 tokens whatever they allow.
pub const MAX_CHAT_TEXT_BYTES: usize = 64 * 1024 * 1024;
/// Tools one request may define, and calls one message may carry (D-032).
pub const MAX_TOOLS: usize = 128;
pub const MAX_TOOL_CALLS: usize = 128;
/// A tool's name: what every provider accepts (`[A-Za-z_][A-Za-z0-9_-]*`).
pub const MAX_TOOL_NAME_CHARS: usize = 64;
/// A tool call's id. Ids are opaque: some providers' carry state the next
/// turn must send back (Gemini's thought signatures), so the bound is wide.
pub const MAX_TOOL_CALL_ID_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatBody {
    pub model: String,
    pub messages: Vec<PublicMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// OpenAI's end-user field: the app's user, where `Gateway-End-User`
    /// is not sent (D-028).
    #[serde(default)]
    pub user: Option<String>,
    /// Function tools the model may call (D-032).
    #[serde(default)]
    pub tools: Option<Vec<PublicTool>>,
    #[serde(default)]
    pub tool_choice: Option<PublicToolChoice>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMessage {
    pub role: String,
    /// Required, except on an assistant message that makes tool calls.
    #[serde(default)]
    pub content: Option<String>,
    /// An assistant message's tool calls.
    #[serde(default)]
    pub tool_calls: Option<Vec<PublicToolCall>>,
    /// The call a `tool` message answers.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// The only tool type: `function`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionType {
    Function,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicTool {
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: PublicFunction,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A JSON Schema object; no parameters when absent.
    #[serde(default)]
    pub parameters: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PublicToolChoice {
    /// `auto`, `none` or `required`.
    Mode(String),
    Function(PublicNamedToolChoice),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicNamedToolChoice {
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: PublicFunctionName,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicFunctionName {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: PublicFunctionCall,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicFunctionCall {
    pub name: String,
    /// A JSON object, as text.
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingBody {
    pub model: String,
    pub input: Vec<String>,
    /// As on chat (D-028).
    #[serde(default)]
    pub user: Option<String>,
}

/// Why a request body is outside the supported bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestBoundError {
    ChatRequestOutOfBounds,
    ChatRoleInvalid,
    ChatInputBoundExceeded,
    TemperatureInvalid,
    MaxTokensInvalid,
    EmbeddingRequestOutOfBounds,
    EmbeddingInputBoundExceeded,
    /// Tools, tool choice, tool calls or tool results that break the rules
    /// of D-032; the detail says which.
    ChatToolsInvalid(&'static str),
}

impl RequestBoundError {
    /// The problem code the single-request endpoints answer with.
    pub const fn code(self) -> &'static str {
        match self {
            Self::ChatRequestOutOfBounds => "chat_request_out_of_bounds",
            Self::ChatRoleInvalid => "chat_role_invalid",
            Self::ChatInputBoundExceeded => "chat_input_bound_exceeded",
            Self::TemperatureInvalid => "temperature_invalid",
            Self::MaxTokensInvalid => "max_tokens_invalid",
            Self::EmbeddingRequestOutOfBounds => "embedding_request_out_of_bounds",
            Self::EmbeddingInputBoundExceeded => "embedding_input_bound_exceeded",
            Self::ChatToolsInvalid(_) => "chat_tools_invalid",
        }
    }
}

pub fn chat_request(body: ChatBody) -> Result<(String, ChatRequest), RequestBoundError> {
    if body.model.trim().is_empty()
        || body.model.chars().count() > MAX_MODEL_ALIAS_CHARS
        || body.messages.is_empty()
        || body.messages.len() > MAX_MESSAGES
    {
        return Err(RequestBoundError::ChatRequestOutOfBounds);
    }
    let mut total = 0usize;
    let mut add = |bytes: usize| {
        total = total.saturating_add(bytes);
        if total > MAX_CHAT_TEXT_BYTES {
            Err(RequestBoundError::ChatInputBoundExceeded)
        } else {
            Ok(())
        }
    };
    let tools = tools(body.tools.unwrap_or_default())?;
    for tool in &tools {
        add(tool.bytes())?;
    }
    let names: HashSet<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let tool_choice = match body.tool_choice {
        None => None,
        Some(PublicToolChoice::Mode(mode)) => Some(match mode.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => {
                return Err(tools_invalid(
                    "tool_choice must be auto, none, required or a function",
                ));
            }
        }),
        Some(PublicToolChoice::Function(named)) => {
            if !names.contains(named.function.name.as_str()) {
                return Err(tools_invalid(
                    "tool_choice names a function that tools does not define",
                ));
            }
            Some(ToolChoice::Function(named.function.name))
        }
    };
    if tools.is_empty() && (tool_choice.is_some() || body.parallel_tool_calls.is_some()) {
        return Err(tools_invalid(
            "tool_choice and parallel_tool_calls need tools",
        ));
    }
    let mut messages = Vec::with_capacity(body.messages.len());
    // The calls of the latest assistant message not yet answered.
    let mut pending: Vec<String> = Vec::new();
    for message in body.messages {
        let role = match message.role.as_str() {
            "system" => ChatRole::System,
            "developer" => ChatRole::Developer,
            "user" => ChatRole::User,
            "assistant" => ChatRole::Assistant,
            "tool" => ChatRole::Tool,
            _ => return Err(RequestBoundError::ChatRoleInvalid),
        };
        let calls = message.tool_calls.unwrap_or_default();
        if !calls.is_empty() && role != ChatRole::Assistant {
            return Err(tools_invalid("only assistant messages carry tool_calls"));
        }
        if message.tool_call_id.is_some() != (role == ChatRole::Tool) {
            return Err(tools_invalid(
                "a tool message, and only a tool message, has tool_call_id",
            ));
        }
        let content = match message.content {
            Some(content) => content,
            None if !calls.is_empty() => String::new(),
            None => return Err(RequestBoundError::ChatRequestOutOfBounds),
        };
        add(content.len())?;
        if role == ChatRole::Tool {
            let id = message.tool_call_id.clone().unwrap_or_default();
            let Some(at) = pending.iter().position(|pending| *pending == id) else {
                return Err(tools_invalid(
                    "a tool message must answer a call of the assistant message just before it",
                ));
            };
            pending.remove(at);
            add(id.len())?;
        } else if !pending.is_empty() {
            return Err(tools_invalid(
                "every tool call must be answered by a tool message before the next message",
            ));
        }
        if calls.len() > MAX_TOOL_CALLS {
            return Err(tools_invalid("a message carries at most 128 tool calls"));
        }
        let mut tool_calls = Vec::with_capacity(calls.len());
        for call in calls {
            if !valid_call_id(&call.id) || pending.contains(&call.id) {
                return Err(tools_invalid(
                    "tool call ids must be distinct, visible text",
                ));
            }
            if !valid_tool_name(&call.function.name) {
                return Err(tools_invalid("a tool call names an invalid function"));
            }
            if !serde_json::from_str::<Value>(&call.function.arguments)
                .is_ok_and(|value| value.is_object())
            {
                return Err(tools_invalid("tool call arguments must be a JSON object"));
            }
            add(call.id.len() + call.function.name.len() + call.function.arguments.len())?;
            pending.push(call.id.clone());
            tool_calls.push(ToolCall {
                id: Some(call.id),
                name: Some(call.function.name),
                arguments: call.function.arguments,
            });
        }
        messages.push(ChatMessage {
            role,
            content,
            name: None,
            tool_call_id: message.tool_call_id,
            tool_calls,
        });
    }
    if !pending.is_empty() {
        return Err(tools_invalid(
            "every tool call must be answered by a tool message",
        ));
    }
    let carries_tools = messages
        .iter()
        .any(|message| message.role == ChatRole::Tool || !message.tool_calls.is_empty());
    if carries_tools && tools.is_empty() {
        return Err(tools_invalid(
            "a conversation with tool calls must define tools",
        ));
    }
    if body
        .temperature
        .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
    {
        return Err(RequestBoundError::TemperatureInvalid);
    }
    if body
        .max_tokens
        .is_some_and(|value| !(1..=4096).contains(&value))
    {
        return Err(RequestBoundError::MaxTokensInvalid);
    }
    Ok((
        body.model,
        ChatRequest {
            messages,
            temperature: body.temperature,
            max_tokens: body.max_tokens,
            tools,
            tool_choice,
            parallel_tool_calls: body.parallel_tool_calls,
        },
    ))
}

fn tools_invalid(detail: &'static str) -> RequestBoundError {
    RequestBoundError::ChatToolsInvalid(detail)
}

/// A tool name every provider accepts.
pub fn valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name.len() <= MAX_TOOL_NAME_CHARS
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TOOL_CALL_ID_BYTES
        && !id.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn tools(tools: Vec<PublicTool>) -> Result<Vec<ToolDefinition>, RequestBoundError> {
    if tools.len() > MAX_TOOLS {
        return Err(tools_invalid("a request defines at most 128 tools"));
    }
    let mut names = HashSet::new();
    tools
        .into_iter()
        .map(|tool| {
            let PublicFunction {
                name,
                description,
                parameters,
            } = tool.function;
            if !valid_tool_name(&name) {
                return Err(tools_invalid(
                    "a tool name is 1 to 64 letters, digits, underscores or hyphens, not starting with a digit or hyphen",
                ));
            }
            if !names.insert(name.clone()) {
                return Err(tools_invalid("tool names must be distinct"));
            }
            let parameters = parameters
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
            if parameters.get("type").and_then(Value::as_str) != Some("object") {
                return Err(tools_invalid(
                    "a tool's parameters must be a JSON Schema object with \"type\": \"object\"",
                ));
            }
            Ok(ToolDefinition {
                name,
                description: description.filter(|text| !text.is_empty()),
                parameters,
            })
        })
        .collect()
}

pub fn embedding_request(
    body: EmbeddingBody,
) -> Result<(String, EmbeddingRequest), RequestBoundError> {
    if body.model.trim().is_empty()
        || body.model.chars().count() > MAX_MODEL_ALIAS_CHARS
        || body.input.is_empty()
        || body.input.iter().any(|value| value.is_empty())
        || body.input.len() > MAX_EMBEDDINGS
    {
        return Err(RequestBoundError::EmbeddingRequestOutOfBounds);
    }
    body.input.iter().try_fold(0usize, |total, value| {
        total
            .checked_add(value.len())
            .filter(|&next| next <= MAX_TEXT_BYTES)
            .ok_or(RequestBoundError::EmbeddingInputBoundExceeded)
    })?;
    Ok((body.model, EmbeddingRequest { input: body.input }))
}

// The public response bodies, as the single endpoints answer and a batch
// output line carries them.

#[derive(Debug, Serialize)]
pub struct ChatBodyResponse {
    pub id: String,
    pub model: String,
    pub choices: Vec<ChoiceResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageResponse>,
}

#[derive(Debug, Serialize)]
pub struct ChoiceResponse {
    pub index: u32,
    pub message: MessageResponse,
    pub finish_reason: Option<FinishReason>,
    pub tool_calls: Vec<crate::domain::inference::ToolCall>,
    pub refusal: Option<String>,
    pub safety: Vec<crate::domain::inference::SafetyAnnotation>,
    pub annotations: Vec<crate::domain::inference::Annotation>,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub role: &'static str,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The choice's tool calls in the request's shape (D-032), so the
    /// message can be sent back as it is. Absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallResponse>,
}

/// A tool call as an assistant message carries it.
#[derive(Debug, Serialize)]
pub struct ToolCallResponse {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: FunctionCallResponse,
}

#[derive(Debug, Serialize)]
pub struct FunctionCallResponse {
    pub name: Option<String>,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingBodyResponse {
    pub object: &'static str,
    pub model: String,
    pub data: Vec<EmbeddingItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageResponse>,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingItem {
    pub object: &'static str,
    pub index: u32,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub other_tokens: Option<u64>,
    /// The wire values are lowercase, like every other value in the API; the
    /// domain enums' own serde form is internal.
    pub quality: &'static str,
    pub completeness: &'static str,
}

pub fn quality_value(quality: crate::domain::inference::UsageQuality) -> &'static str {
    use crate::domain::inference::UsageQuality;
    match quality {
        UsageQuality::Exact => "exact",
        UsageQuality::Estimated => "estimated",
        UsageQuality::Unknown => "unknown",
    }
}

pub fn completeness_value(
    completeness: crate::domain::inference::UsageCompleteness,
) -> &'static str {
    use crate::domain::inference::UsageCompleteness;
    match completeness {
        UsageCompleteness::Complete => "complete",
        UsageCompleteness::Partial => "partial",
        UsageCompleteness::Unavailable => "unavailable",
    }
}

pub fn choice_response(choice: &ChatChoice) -> ChoiceResponse {
    ChoiceResponse {
        index: choice.index,
        message: MessageResponse {
            role: match choice.message.role {
                ChatRole::System => "system",
                ChatRole::Developer => "developer",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => "tool",
            },
            content: choice.message.content.clone(),
            name: choice.message.name.clone(),
            tool_call_id: choice.message.tool_call_id.clone(),
            tool_calls: choice
                .tool_calls
                .iter()
                .map(|call| ToolCallResponse {
                    id: call.id.clone(),
                    kind: FunctionType::Function,
                    function: FunctionCallResponse {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                })
                .collect(),
        },
        finish_reason: choice.finish_reason.clone(),
        tool_calls: choice.tool_calls.clone(),
        refusal: choice.refusal.clone(),
        safety: choice.safety.clone(),
        annotations: choice.annotations.clone(),
    }
}

pub fn usage_response(usage: crate::domain::inference::Usage) -> UsageResponse {
    let value = |counter: Option<crate::domain::inference::TokenCounter>| {
        counter.and_then(|counter| counter.value())
    };
    UsageResponse {
        input_tokens: value(usage.input_tokens()),
        output_tokens: value(usage.output_tokens()),
        cached_input_tokens: value(usage.cached_input_tokens()),
        reasoning_tokens: value(usage.reasoning_tokens()),
        other_tokens: value(usage.other_tokens()),
        quality: quality_value(usage.quality()),
        completeness: completeness_value(usage.completeness()),
    }
}

pub fn embedding_response(
    response: EmbeddingResponse,
    public_model: &str,
) -> EmbeddingBodyResponse {
    EmbeddingBodyResponse {
        object: "list",
        model: public_model.to_owned(),
        data: response
            .data()
            .iter()
            .map(|item| EmbeddingItem {
                object: "embedding",
                index: item.index(),
                embedding: item.vector().to_vec(),
            })
            .collect(),
        usage: response.usage().map(|value| usage_response(value.usage())),
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;
    use serde_json::json;

    fn parse(body: Value) -> Result<ChatRequest, RequestBoundError> {
        chat_request(serde_json::from_value::<ChatBody>(body).expect("wire shape")).map(|(_, r)| r)
    }

    fn weather() -> Value {
        json!({"type": "function", "function": {
            "name": "get_weather", "description": "Current weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}})
    }

    fn turn() -> Value {
        json!([
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "18C"}
        ])
    }

    fn invalid(body: Value) -> &'static str {
        match parse(body) {
            Err(RequestBoundError::ChatToolsInvalid(detail)) => detail,
            other => panic!("expected chat_tools_invalid, got {other:?}"),
        }
    }

    #[test]
    fn tools_calls_and_results_parse_into_the_domain() {
        let request = parse(json!({
            "model": "m", "messages": turn(), "tools": [weather()],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
            "parallel_tool_calls": false
        }))
        .unwrap();
        assert_eq!(request.tools.len(), 1);
        assert_eq!(
            request.tools[0].description.as_deref(),
            Some("Current weather")
        );
        assert_eq!(
            request.tool_choice,
            Some(ToolChoice::Function("get_weather".into()))
        );
        assert_eq!(request.parallel_tool_calls, Some(false));
        assert_eq!(request.messages[1].content, "");
        assert_eq!(
            request.messages[1].tool_calls[0].id.as_deref(),
            Some("call_1")
        );
        assert_eq!(request.messages[2].role, ChatRole::Tool);
        assert!(request.uses_tools());
        // Parameters default to an empty object schema.
        let bare = parse(json!({"model": "m", "messages": [{"role": "user", "content": "x"}],
            "tools": [{"type": "function", "function": {"name": "now"}}], "tool_choice": "required"}))
        .unwrap();
        assert_eq!(
            bare.tools[0].parameters,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(bare.tool_choice, Some(ToolChoice::Required));
        let plain =
            parse(json!({"model": "m", "messages": [{"role": "user", "content": "x"}]})).unwrap();
        assert!(!plain.uses_tools());
    }

    #[test]
    fn malformed_tool_requests_are_refused_with_a_reason() {
        let user = json!([{"role": "user", "content": "x"}]);
        let tool = |name: &str| json!({"type": "function", "function": {"name": name}});
        invalid(json!({"model": "m", "messages": user, "tools": [tool("a b")]}));
        invalid(json!({"model": "m", "messages": user, "tools": [tool("a"), tool("a")]}));
        invalid(
            json!({"model": "m", "messages": user, "tools": [{"type": "function",
            "function": {"name": "a", "parameters": {"type": "string"}}}]}),
        );
        invalid(json!({"model": "m", "messages": user, "tool_choice": "auto"}));
        invalid(json!({"model": "m", "messages": user, "parallel_tool_calls": true}));
        invalid(
            json!({"model": "m", "messages": user, "tools": [tool("a")], "tool_choice": "sometimes"}),
        );
        invalid(json!({"model": "m", "messages": user, "tools": [tool("a")],
            "tool_choice": {"type": "function", "function": {"name": "b"}}}));
        let tools = json!([weather()]);
        // Calls and results with no tools defined.
        invalid(json!({"model": "m", "messages": turn()}));
        // An unanswered call.
        let mut unanswered = turn();
        unanswered.as_array_mut().unwrap().pop();
        invalid(json!({"model": "m", "messages": unanswered, "tools": tools}));
        // A result for a call that was not made.
        let mut stray = turn();
        stray[2]["tool_call_id"] = json!("call_9");
        invalid(json!({"model": "m", "messages": stray, "tools": tools}));
        // Arguments that are not a JSON object.
        let mut arguments = turn();
        arguments[1]["tool_calls"][0]["function"]["arguments"] = json!("[1]");
        invalid(json!({"model": "m", "messages": arguments, "tools": tools}));
        // Calls on a user message; a tool_call_id on an assistant message.
        let mut on_user = turn();
        on_user[0]["tool_calls"] = on_user[1]["tool_calls"].clone();
        invalid(json!({"model": "m", "messages": on_user, "tools": tools}));
        // Duplicate call ids in one message.
        let mut duplicate = turn();
        let call = duplicate[1]["tool_calls"][0].clone();
        duplicate[1]["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call);
        invalid(json!({"model": "m", "messages": duplicate, "tools": tools}));
        // A user message between a call and its result.
        let mut interrupted = turn();
        interrupted
            .as_array_mut()
            .unwrap()
            .insert(2, json!({"role": "user", "content": "hurry"}));
        invalid(json!({"model": "m", "messages": interrupted, "tools": tools}));
        // Content may be absent only on an assistant message with calls.
        assert_eq!(
            parse(json!({"model": "m", "messages": [{"role": "user"}]})).unwrap_err(),
            RequestBoundError::ChatRequestOutOfBounds
        );
        // Unknown fields stay refused.
        assert!(
            serde_json::from_value::<ChatBody>(json!({"model": "m",
            "messages": user, "tools": [{"type": "function",
            "function": {"name": "a", "strict": true}}]}))
            .is_err()
        );
    }

    #[test]
    fn tool_definitions_count_toward_the_text_bound() {
        let big = "x".repeat(MAX_CHAT_TEXT_BYTES);
        assert_eq!(
            parse(
                json!({"model": "m", "messages": [{"role": "user", "content": "x"}],
                "tools": [{"type": "function", "function": {"name": "a", "description": big}}]})
            )
            .unwrap_err(),
            RequestBoundError::ChatInputBoundExceeded
        );
    }
}
