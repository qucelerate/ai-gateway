package com.qucelerate.aigateway;

/**
 * The request could not be sent or the response could not be read, including
 * an attempt that did not finish (or a stream that did not start) within the
 * client's timeout: then {@link #timedOut()} is true.
 */
public final class TransportException extends GatewayException {
    private static final long serialVersionUID = 1L;

    private final boolean timedOut;

    TransportException(String message, Throwable cause, boolean timedOut) {
        super(message, cause);
        this.timedOut = timedOut;
    }

    public boolean timedOut() {
        return timedOut;
    }
}
