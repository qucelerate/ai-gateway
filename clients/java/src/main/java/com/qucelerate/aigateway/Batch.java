package com.qucelerate.aigateway;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.Map;
import java.util.Set;

/**
 * A batch: OpenAI's batch object. {@code status} is an open string
 * (validating, in_progress, finalizing, completed, failed, expired,
 * cancelling, cancelled). {@code gateway} is the gateway's own account, on a
 * single-batch read: each job's {@code mode} (native or emulated) and
 * {@code price_source}, and the cost per currency.
 */
public record Batch(String id, String status, String endpoint, String inputFileId, String outputFileId,
        String errorFileId, long createdAt, long expiresAt, Counts requestCounts, Map<String, String> metadata,
        JsonNode gateway, String requestId) {
    private static final Set<String> ENDED = Set.of("completed", "failed", "expired", "cancelled");

    public Batch {
        metadata = metadata == null ? Map.of() : Map.copyOf(metadata);
    }

    /** Whether the batch has ended: its output and error files are final. */
    public boolean done() {
        return ENDED.contains(status);
    }

    Batch withRequestId(String requestId) {
        return new Batch(id, status, endpoint, inputFileId, outputFileId, errorFileId, createdAt, expiresAt,
                requestCounts, metadata, gateway, requestId);
    }

    /** A batch's line counts. */
    public record Counts(long total, long completed, long failed) {}
}
