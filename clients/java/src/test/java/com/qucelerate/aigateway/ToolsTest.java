package com.qucelerate.aigateway;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import org.junit.jupiter.api.Test;

class ToolsTest {
    @Test
    void toolsAndCallsGoOnTheWire() throws Exception {
        ChatResponse.MessageResponse response = Json.convert(
                Json.tree("{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\","
                        + "\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}"),
                ChatResponse.MessageResponse.class);
        assertEquals(List.of(ToolCall.of("call_1", "get_weather", "{\"city\":\"Paris\"}")), response.toolCalls());
        ChatRequest request = new ChatRequest("chat-default", List.of(
                        Message.user("Weather?"), response.toMessage(), Message.tool("call_1", "18C")))
                .withTools(List.of(Tool.function("get_weather", "Current weather", Json.tree("{\"type\":\"object\"}"))))
                .withToolChoice(ToolChoice.function("get_weather"))
                .withParallelToolCalls(false);
        assertEquals(
                Json.tree("{\"model\":\"chat-default\",\"messages\":[{\"role\":\"user\",\"content\":\"Weather?\"},"
                        + "{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\","
                        + "\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]},"
                        + "{\"role\":\"tool\",\"content\":\"18C\",\"tool_call_id\":\"call_1\"}],"
                        + "\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"get_weather\","
                        + "\"description\":\"Current weather\",\"parameters\":{\"type\":\"object\"}}}],"
                        + "\"tool_choice\":{\"type\":\"function\",\"function\":{\"name\":\"get_weather\"}},"
                        + "\"parallel_tool_calls\":false}"),
                Json.MAPPER.valueToTree(request));
        assertEquals(
                Json.tree("{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}"),
                Json.MAPPER.valueToTree(new ChatRequest("m", List.of(Message.user("hi")))));
    }

    @Test
    void theRecordsKeepTheirConstructorsFromBeforeTools() {
        ChatResponse.MessageResponse response = new ChatResponse.MessageResponse("assistant", "hi", null, null);
        assertEquals(List.of(), response.toolCalls());
        assertEquals(Message.assistant("hi"), response.toMessage());
        Message message = new Message("user", "hi");
        assertEquals(List.of(), message.toolCalls());
        assertEquals(null, message.toolCallId());
        ChatRequest request = new ChatRequest("m", List.of(message), 0.5, 16);
        assertEquals(List.of(), request.tools());
        assertEquals(null, request.toolChoice());
        assertEquals(null, request.parallelToolCalls());
    }
}
