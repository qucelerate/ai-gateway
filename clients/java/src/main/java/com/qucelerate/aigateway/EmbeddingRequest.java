package com.qucelerate.aigateway;

import java.util.List;
import java.util.Objects;

/** The body of an embeddings call: 1 to 128 non-empty strings. */
public record EmbeddingRequest(String model, List<String> input) {
    public EmbeddingRequest {
        Objects.requireNonNull(model, "model");
        input = List.copyOf(Objects.requireNonNull(input, "input"));
    }
}
