package com.qucelerate.aigateway;

/**
 * The base of every error this client throws. Catch a subclass for the
 * failures you handle: {@link ProblemException}, {@link HttpStatusException},
 * {@link StreamException}, {@link ProtocolException}, {@link DecodeException},
 * {@link TransportException} and {@link ConfigException}.
 */
public class GatewayException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    GatewayException(String message) {
        super(message);
    }

    GatewayException(String message, Throwable cause) {
        super(message, cause);
    }
}
