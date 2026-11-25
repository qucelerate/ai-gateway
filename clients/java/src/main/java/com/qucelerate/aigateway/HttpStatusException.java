package com.qucelerate.aigateway;

/** An error response without a problem body, for example from a proxy. */
public final class HttpStatusException extends GatewayException {
    private static final long serialVersionUID = 1L;

    private final int status;
    private final String requestId;

    HttpStatusException(int status, String requestId) {
        super("HTTP " + status + " without a problem body");
        this.status = status;
        this.requestId = requestId;
    }

    public int status() {
        return status;
    }

    /** The {@code X-Request-Id} response header, or {@code null}. */
    public String requestId() {
        return requestId;
    }
}
