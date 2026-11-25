package com.qucelerate.aigateway;

import com.fasterxml.jackson.annotation.JsonInclude;
import java.util.List;
import java.util.Objects;

/**
 * The body of a chat call. {@code temperature} (0 to 2) and {@code maxTokens}
 * (1 to 4096) are optional; leave them {@code null} to use the model's default.
 * {@code tools} are functions the model may call: a model alias that cannot
 * take tools refuses them ({@code tools_unsupported}), never drops them.
 * {@code parallelToolCalls} {@code false} asks for at most one call per response.
 */
@JsonInclude(JsonInclude.Include.NON_NULL)
public record ChatRequest(
        String model,
        List<Message> messages,
        Double temperature,
        Integer maxTokens,
        @JsonInclude(JsonInclude.Include.NON_EMPTY) List<Tool> tools,
        ToolChoice toolChoice,
        Boolean parallelToolCalls) {
    public ChatRequest {
        Objects.requireNonNull(model, "model");
        messages = List.copyOf(Objects.requireNonNull(messages, "messages"));
        tools = tools == null ? List.of() : List.copyOf(tools);
    }

    public ChatRequest(String model, List<Message> messages, Double temperature, Integer maxTokens) {
        this(model, messages, temperature, maxTokens, List.of(), null, null);
    }

    public ChatRequest(String model, List<Message> messages) {
        this(model, messages, null, null);
    }

    public ChatRequest withTemperature(double temperature) {
        return new ChatRequest(model, messages, temperature, maxTokens, tools, toolChoice, parallelToolCalls);
    }

    public ChatRequest withMaxTokens(int maxTokens) {
        return new ChatRequest(model, messages, temperature, maxTokens, tools, toolChoice, parallelToolCalls);
    }

    public ChatRequest withTools(List<Tool> tools) {
        return new ChatRequest(model, messages, temperature, maxTokens, tools, toolChoice, parallelToolCalls);
    }

    public ChatRequest withToolChoice(ToolChoice toolChoice) {
        return new ChatRequest(model, messages, temperature, maxTokens, tools, toolChoice, parallelToolCalls);
    }

    public ChatRequest withParallelToolCalls(boolean parallelToolCalls) {
        return new ChatRequest(model, messages, temperature, maxTokens, tools, toolChoice, parallelToolCalls);
    }
}
