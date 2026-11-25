package com.qucelerate.aigateway;

import java.util.List;

/** One vector per input, in input order. */
public record EmbeddingResponse(String object, String model, List<Embedding> data, Usage usage, String requestId) {
    public EmbeddingResponse {
        data = data == null ? List.of() : List.copyOf(data);
    }

    EmbeddingResponse withRequestId(String requestId) {
        return new EmbeddingResponse(object, model, data, usage, requestId);
    }

    /** One embedding vector. */
    public record Embedding(String object, int index, float[] embedding) {}
}
