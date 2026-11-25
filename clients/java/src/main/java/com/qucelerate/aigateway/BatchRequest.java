package com.qucelerate.aigateway;

import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.Map;
import java.util.Objects;

/**
 * Create a batch from an uploaded file. {@code endpoint} is
 * {@link #CHAT_COMPLETIONS} or {@link #EMBEDDINGS}; {@code metadata} may be
 * empty.
 */
public record BatchRequest(String inputFileId, String endpoint, Map<String, String> metadata) {
    public static final String CHAT_COMPLETIONS = "/v1/chat/completions";
    public static final String EMBEDDINGS = "/v1/embeddings";

    public BatchRequest {
        Objects.requireNonNull(inputFileId, "inputFileId");
        if (!CHAT_COMPLETIONS.equals(endpoint) && !EMBEDDINGS.equals(endpoint)) {
            throw new ConfigException("endpoint must be " + CHAT_COMPLETIONS + " or " + EMBEDDINGS);
        }
        metadata = metadata == null ? Map.of() : Map.copyOf(metadata);
    }

    public BatchRequest(String inputFileId, String endpoint) {
        this(inputFileId, endpoint, Map.of());
    }

    /** Always {@code 24h}. */
    @JsonProperty("completion_window")
    public String completionWindow() {
        return "24h";
    }
}
