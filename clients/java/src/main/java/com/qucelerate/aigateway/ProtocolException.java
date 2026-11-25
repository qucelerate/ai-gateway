package com.qucelerate.aigateway;

/**
 * A stream closed before its terminal event, or sent an event over 16 MiB.
 * Streams are not resumable.
 */
public final class ProtocolException extends GatewayException {
    private static final long serialVersionUID = 1L;

    ProtocolException(String message) {
        super(message);
    }
}
