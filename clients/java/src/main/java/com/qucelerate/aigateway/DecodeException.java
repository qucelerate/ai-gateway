package com.qucelerate.aigateway;

/** A response body or stream event did not match the contract. */
public final class DecodeException extends GatewayException {
    private static final long serialVersionUID = 1L;

    DecodeException(String message, Throwable cause) {
        super(message, cause);
    }
}
