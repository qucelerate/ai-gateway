package com.qucelerate.aigateway;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;
import java.util.Map;
import java.util.Objects;

/**
 * Whether the model must call tools: {@link #AUTO} (the default), {@link #NONE},
 * {@link #REQUIRED}, or {@link #function(String)} to make it call one.
 */
public final class ToolChoice {
    public static final ToolChoice AUTO = new ToolChoice("auto");
    public static final ToolChoice NONE = new ToolChoice("none");
    public static final ToolChoice REQUIRED = new ToolChoice("required");

    private final Object value;

    private ToolChoice(Object value) {
        this.value = value;
    }

    public static ToolChoice function(String name) {
        return new ToolChoice(Map.of("type", "function", "function", Map.of("name", Objects.requireNonNull(name))));
    }

    @JsonCreator
    static ToolChoice fromJson(Object value) {
        return new ToolChoice(value);
    }

    @JsonValue
    Object value() {
        return value;
    }

    @Override
    public boolean equals(Object other) {
        return other instanceof ToolChoice choice && value.equals(choice.value);
    }

    @Override
    public int hashCode() {
        return value.hashCode();
    }
}
