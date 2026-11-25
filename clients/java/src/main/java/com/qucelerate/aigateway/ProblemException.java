package com.qucelerate.aigateway;

import java.util.OptionalLong;

/**
 * An RFC 9457 problem the gateway returned. {@link #code()} is an open string:
 * match the codes you handle and fall back to {@link #status()} for the rest.
 * {@code clients/conformance/errors.json} lists the codes of contract 1.0.0 and
 * what each means for a retry.
 */
public final class ProblemException extends GatewayException {
    private static final long serialVersionUID = 1L;

    private final int status;
    private final String type;
    private final String code;
    private final String title;
    private final String detail;
    private final String instance;
    private final String requestId;
    private final Long retryAfter;

    ProblemException(int status, String type, String code, String title, String detail, String instance,
            String requestId, Long retryAfter) {
        super("gateway problem " + status + ": " + code + " (" + title + "; request " + requestId + ")");
        this.status = status;
        this.type = type;
        this.code = code;
        this.title = title;
        this.detail = detail;
        this.instance = instance;
        this.requestId = requestId;
        this.retryAfter = retryAfter;
    }

    public int status() {
        return status;
    }

    public String type() {
        return type;
    }

    public String code() {
        return code;
    }

    public String title() {
        return title;
    }

    public String detail() {
        return detail;
    }

    public String instance() {
        return instance;
    }

    public String requestId() {
        return requestId;
    }

    /** The {@code Retry-After} header in whole seconds, when the response had one. */
    public OptionalLong retryAfter() {
        return retryAfter == null ? OptionalLong.empty() : OptionalLong.of(retryAfter);
    }
}
