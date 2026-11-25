//! Tool calling shared by the adapters (D-032): the OpenAI wire shape the
//! OpenAI-compatible families send, and helpers every mapping uses.

use serde::Serialize;
use serde_json::{Value, json};

use crate::domain::inference::{
    ChatMessage, ChatRequest, ProviderError, ProviderErrorCategory, ToolChoice,
};

/// A tool call's arguments as the JSON object providers other than OpenAI's
/// take. The public request checked they are one; anything else is refused
/// rather than sent changed.
pub(crate) fn tool_arguments(arguments: &str) -> Result<Value, ProviderError> {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(ProviderError::new(ProviderErrorCategory::InvalidRequest)),
    }
}

/// An assistant message's tool calls on the OpenAI wire.
#[derive(Serialize)]
pub(crate) struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall,
}

#[derive(Serialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

/// `message`'s tool calls in OpenAI's shape, or `None` when it makes none.
/// The public request gives every call an id and a name.
pub(crate) fn wire_tool_calls(message: &ChatMessage) -> Option<Vec<WireToolCall>> {
    (!message.tool_calls.is_empty()).then(|| {
        message
            .tool_calls
            .iter()
            .map(|call| WireToolCall {
                id: call.id.clone().unwrap_or_default(),
                kind: "function",
                function: WireFunctionCall {
                    name: call.name.clone().unwrap_or_default(),
                    arguments: call.arguments.clone(),
                },
            })
            .collect()
    })
}

/// An assistant message's content on the OpenAI wire: null when it only
/// calls tools, as OpenAI documents.
pub(crate) fn wire_content(message: &ChatMessage) -> Option<String> {
    (!(message.content.is_empty() && !message.tool_calls.is_empty()))
        .then(|| message.content.clone())
}

/// The request's tools, tool choice and parallel calls as OpenAI's Chat
/// Completions fields, or nothing when it defines no tools.
#[derive(Serialize)]
pub(crate) struct WireTools {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parallel_tool_calls: Option<bool>,
}

pub(crate) fn wire_tools(request: &ChatRequest) -> WireTools {
    if request.tools.is_empty() {
        return WireTools {
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
        };
    }
    WireTools {
        tools: Some(
            request
                .tools
                .iter()
                .map(|tool| {
                    let mut function = json!({
                        "name": tool.name,
                        "parameters": tool.parameters,
                    });
                    if let Some(description) = &tool.description {
                        function["description"] = json!(description);
                    }
                    json!({"type": "function", "function": function})
                })
                .collect(),
        ),
        tool_choice: request.tool_choice.as_ref().map(|choice| match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function(name) => {
                json!({"type": "function", "function": {"name": name}})
            }
        }),
        parallel_tool_calls: request.parallel_tool_calls,
    }
}

/// A conversation with two parallel calls answered, in the contract's
/// terms, for the adapters' mapping tests.
#[cfg(test)]
pub(crate) fn tool_conversation() -> ChatRequest {
    use crate::domain::inference::{ChatRole, ToolCall, ToolDefinition};
    ChatRequest {
        messages: vec![
            ChatMessage::text(ChatRole::System, "be brief"),
            ChatMessage::text(ChatRole::User, "weather in Paris and Rome?"),
            ChatMessage {
                tool_calls: vec![
                    ToolCall {
                        id: Some("toolu_01".into()),
                        name: Some("get_weather".into()),
                        arguments: r#"{"city":"Paris"}"#.into(),
                    },
                    ToolCall {
                        id: Some("toolu_02".into()),
                        name: Some("get_weather".into()),
                        arguments: r#"{"city":"Rome"}"#.into(),
                    },
                ],
                ..ChatMessage::text(ChatRole::Assistant, "Checking.")
            },
            ChatMessage {
                tool_call_id: Some("toolu_01".into()),
                ..ChatMessage::text(ChatRole::Tool, "18C")
            },
            ChatMessage {
                tool_call_id: Some("toolu_02".into()),
                ..ChatMessage::text(ChatRole::Tool, "24C")
            },
        ],
        tools: vec![ToolDefinition {
            name: "get_weather".into(),
            description: Some("Current weather".into()),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }],
        max_tokens: Some(64),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{ChatRole, ToolCall, ToolDefinition};

    #[test]
    fn openai_wire_carries_tools_choice_and_calls() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                tool_calls: vec![ToolCall {
                    id: Some("call_1".into()),
                    name: Some("lookup".into()),
                    arguments: r#"{"q":"x"}"#.into(),
                }],
                ..ChatMessage::text(ChatRole::Assistant, "")
            }],
            tools: vec![ToolDefinition {
                name: "lookup".into(),
                description: Some("Looks it up".into()),
                parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            }],
            tool_choice: Some(ToolChoice::Function("lookup".into())),
            parallel_tool_calls: Some(false),
            ..Default::default()
        };
        let wire = wire_tools(&request);
        assert_eq!(
            Value::Array(wire.tools.unwrap()),
            json!([{"type": "function", "function": {
                "name": "lookup", "description": "Looks it up",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}}])
        );
        assert_eq!(
            wire.tool_choice.unwrap(),
            json!({"type": "function", "function": {"name": "lookup"}})
        );
        assert_eq!(wire.parallel_tool_calls, Some(false));
        assert_eq!(
            serde_json::to_value(wire_tool_calls(&request.messages[0])).unwrap(),
            json!([{"id": "call_1", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}}])
        );
        let without = wire_tools(&ChatRequest::default());
        assert!(without.tools.is_none() && without.tool_choice.is_none());
        assert_eq!(wire_content(&request.messages[0]), None);
        assert!(wire_tool_calls(&ChatMessage::text(ChatRole::User, "hi")).is_none());
        assert!(tool_arguments("[1]").is_err());
        assert_eq!(tool_arguments(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
    }
}
