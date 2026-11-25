package com.qucelerate.aigateway;

/**
 * A stream's terminal {@code error} event. The request reached the provider, so
 * it is never retried automatically.
 */
public final class StreamException extends GatewayException {
    private static final long serialVersionUID = 1L;

    private final String code;
    private final int status;

    StreamException(String code, int status) {
        super("stream failed: " + code + " (status " + status + ")");
        this.code = code;
        this.status = status;
    }

    public String code() {
        return code;
    }

    public int status() {
        return status;
    }
}
