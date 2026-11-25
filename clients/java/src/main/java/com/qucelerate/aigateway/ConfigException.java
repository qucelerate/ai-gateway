package com.qucelerate.aigateway;

/** The client or a call was configured with an invalid value. */
public final class ConfigException extends GatewayException {
    private static final long serialVersionUID = 1L;

    ConfigException(String message) {
        super(message);
    }
}
