package com.qucelerate.aigateway;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.databind.JsonNode;
import java.util.Objects;

/**
 * A function the model may call. {@code parameters} is a JSON Schema whose
 * {@code type} is {@code object}; {@code null} sends none (the gateway takes an
 * empty object).
 */
public record Tool(String type, Function function) {
    public Tool {
        type = type == null ? "function" : type;
        Objects.requireNonNull(function, "function");
    }

    public static Tool function(String name, String description, JsonNode parameters) {
        return new Tool("function", new Function(name, description, parameters));
    }

    /** A tool's function: its name, description and parameter schema. */
    @JsonInclude(JsonInclude.Include.NON_NULL)
    public record Function(String name, String description, JsonNode parameters) {
        public Function {
            Objects.requireNonNull(name, "name");
        }
    }
}
