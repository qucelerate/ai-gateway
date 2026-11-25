package com.qucelerate.aigateway;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.List;

/**
 * A chat completion. {@code requestId} is the {@code X-Request-Id} response
 * header: quote it when reporting a problem. {@code usage} is {@code null} when
 * the gateway reported none.
 */
public record ChatResponse(String id, String model, List<Choice> choices, Usage usage, String requestId) {
    public ChatResponse {
        choices = choices == null ? List.of() : List.copyOf(choices);
    }

    /** The first choice's message content, the usual answer, or {@code ""}. */
    public String content() {
        if (choices.isEmpty() || choices.get(0).message() == null || choices.get(0).message().content() == null) {
            return "";
        }
        return choices.get(0).message().content();
    }

    ChatResponse withRequestId(String requestId) {
        return new ChatResponse(id, model, choices, usage, requestId);
    }

    /**
     * One choice. {@code finishReason} is an open string ({@code stop},
     * {@code length}, {@code tool_calls}, ...): handle values you do not know.
     */
    public record Choice(
            int index,
            MessageResponse message,
            String finishReason,
            List<JsonNode> toolCalls,
            String refusal,
            List<JsonNode> safety,
            List<JsonNode> annotations) {}

    /**
     * The assistant message of a choice; {@code content} may be {@code null}.
     * {@code toolCalls} are the calls the model made, empty when it made none.
     */
    public record MessageResponse(String role, String content, String name, String toolCallId, List<ToolCall> toolCalls) {
        public MessageResponse {
            toolCalls = toolCalls == null ? List.of() : List.copyOf(toolCalls);
        }

        /** A message without tool calls, as before tool calling. */
        public MessageResponse(String role, String content, String name, String toolCallId) {
            this(role, content, name, toolCallId, List.of());
        }

        /** This response as the assistant message that continues the conversation, its calls included. */
        public Message toMessage() {
            boolean empty = content == null || content.isEmpty();
            return Message.assistant(empty && !toolCalls.isEmpty() ? null : empty ? "" : content, toolCalls);
        }
    }
}
