package com.qucelerate.aigateway;

import com.fasterxml.jackson.annotation.JsonInclude;
import java.util.List;
import java.util.Objects;

/**
 * One message of a chat request.
 *
 * @param role       {@code system}, {@code developer}, {@code user}, {@code assistant} or {@code tool}
 * @param content    the message text; {@code null} only on an assistant message that calls tools
 * @param toolCalls  an assistant message's calls, sent back unchanged before their results
 * @param toolCallId a tool message's: the id of the call it answers
 */
public record Message(
        String role,
        @JsonInclude(JsonInclude.Include.ALWAYS) String content,
        @JsonInclude(JsonInclude.Include.NON_EMPTY) List<ToolCall> toolCalls,
        @JsonInclude(JsonInclude.Include.NON_NULL) String toolCallId) {
    public Message {
        Objects.requireNonNull(role, "role");
        toolCalls = toolCalls == null ? List.of() : List.copyOf(toolCalls);
        if (content == null && toolCalls.isEmpty()) {
            throw new NullPointerException("content");
        }
    }

    public Message(String role, String content) {
        this(role, content, List.of(), null);
    }

    public static Message system(String content) {
        return new Message("system", content);
    }

    public static Message developer(String content) {
        return new Message("developer", content);
    }

    public static Message user(String content) {
        return new Message("user", content);
    }

    public static Message assistant(String content) {
        return new Message("assistant", content);
    }

    /** The assistant's turn that called {@code toolCalls}; {@code content} may be {@code null}. */
    public static Message assistant(String content, List<ToolCall> toolCalls) {
        return new Message("assistant", content, toolCalls, null);
    }

    /** A tool's result, answering the call whose id is {@code toolCallId}. */
    public static Message tool(String toolCallId, String content) {
        return new Message("tool", content, List.of(), Objects.requireNonNull(toolCallId, "toolCallId"));
    }
}
