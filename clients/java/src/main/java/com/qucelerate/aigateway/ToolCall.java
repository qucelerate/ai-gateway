package com.qucelerate.aigateway;

/**
 * A call the model made to one of the request's tools, OpenAI-shaped. Send it
 * back unchanged; {@code function().arguments()} is a JSON object, serialized,
 * as the model wrote it: validate it before acting on it.
 */
public record ToolCall(String id, String type, Function function) {
    public ToolCall {
        type = type == null ? "function" : type;
    }

    public static ToolCall of(String id, String name, String arguments) {
        return new ToolCall(id, "function", new Function(name, arguments));
    }

    /** The function a call calls and its arguments. */
    public record Function(String name, String arguments) {}
}
