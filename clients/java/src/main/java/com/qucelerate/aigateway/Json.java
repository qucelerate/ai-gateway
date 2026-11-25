package com.qucelerate.aigateway;

import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.fasterxml.jackson.databind.json.JsonMapper;
import java.io.IOException;

/** The contract's JSON: snake_case names, unknown fields ignored. */
final class Json {
    static final ObjectMapper MAPPER = JsonMapper.builder()
            .propertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE)
            .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
            .build();

    private Json() {}

    static <T> T decode(byte[] body, Class<T> type) {
        try {
            T value = MAPPER.readValue(body, type);
            if (value == null) {
                throw new DecodeException("the response body is null", null);
            }
            return value;
        } catch (IOException error) {
            throw new DecodeException("the response does not match the contract: " + error.getMessage(), error);
        }
    }

    static JsonNode tree(String text) throws IOException {
        return MAPPER.readTree(text);
    }

    static <T> T convert(JsonNode node, Class<T> type) throws IOException {
        return MAPPER.treeToValue(node, type);
    }
}
