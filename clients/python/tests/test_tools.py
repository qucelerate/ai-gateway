"""Typed tool calling (D-032): what the typed helpers put on the wire."""

from qucelerate_ai_gateway import Message, MessageResponse, Tool, ToolCall
from qucelerate_ai_gateway._base import chat_body


def test_typed_tools_and_calls_build_the_wire_body() -> None:
    response = MessageResponse.from_dict({
        "role": "assistant", "content": None,
        "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}],
    })
    assert response.content == ""
    assert response.tool_calls == [ToolCall("call_1", "get_weather", "{\"city\":\"Paris\"}")]
    body = chat_body(
        "chat-default",
        [Message("user", "Weather?"), response.to_message(),
         Message("tool", "18C", tool_call_id="call_1")],
        None, None,
        [Tool("get_weather", "Current weather", {"type": "object"})],
        {"type": "function", "function": {"name": "get_weather"}},
        False,
    )
    assert body["messages"][1] == {
        "role": "assistant", "content": None,
        "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}],
    }
    assert body["messages"][2] == {"role": "tool", "content": "18C", "tool_call_id": "call_1"}
    assert body["tools"] == [{"type": "function", "function": {
        "name": "get_weather", "description": "Current weather", "parameters": {"type": "object"}}}]
    assert body["tool_choice"] == {"type": "function", "function": {"name": "get_weather"}}
    assert body["parallel_tool_calls"] is False
    plain = chat_body("chat-default", [Message("user", "hi")], None, None)
    assert plain == {"model": "chat-default", "messages": [{"role": "user", "content": "hi"}]}
