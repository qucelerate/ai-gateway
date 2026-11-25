package com.qucelerate.aigateway;

/**
 * Options for one call. Leave {@code idempotencyKey} {@code null} to have the
 * client generate one per call, reused by that call's retries; set it (1 to 255
 * characters) to make your own retries of an operation safe across restarts.
 * {@code requestId} (at most 128 characters) is sent as {@code X-Request-Id} to
 * correlate the call with your logs.
 */
public record CallOptions(String idempotencyKey, String requestId) {
    public static final CallOptions DEFAULT = new CallOptions(null, null);

    public static CallOptions idempotencyKey(String key) {
        return new CallOptions(key, null);
    }

    public CallOptions withIdempotencyKey(String key) {
        return new CallOptions(key, requestId);
    }

    public CallOptions withRequestId(String id) {
        return new CallOptions(idempotencyKey, id);
    }
}
