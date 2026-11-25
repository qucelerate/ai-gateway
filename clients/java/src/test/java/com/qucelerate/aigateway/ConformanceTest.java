package com.qucelerate.aigateway;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.sun.net.httpserver.Headers;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.stream.Stream;
import org.junit.jupiter.api.DynamicTest;
import org.junit.jupiter.api.TestFactory;

/**
 * Runs every case of clients/conformance against a local server that serves the
 * case's responses in order and records what it received, once through the
 * blocking API and once through the asynchronous one.
 */
class ConformanceTest {
    static final Path SUITE = Path.of("..", "conformance");

    record Recorded(String method, String path, Headers headers, byte[] body) {}

    @TestFactory
    Stream<DynamicTest> everyCase() throws IOException {
        List<Path> paths;
        try (Stream<Path> files = Files.list(SUITE.resolve("cases"))) {
            paths = files.filter(path -> path.toString().endsWith(".json")).sorted().toList();
        }
        assertFalse(paths.isEmpty());
        return paths.stream().flatMap(path -> Stream.of(false, true).map(async -> DynamicTest.dynamicTest(
                path.getFileName() + (async ? " (async)" : ""), () -> run(Json.MAPPER.readTree(path.toFile()), async))));
    }

    static void run(JsonNode testCase, boolean async) throws Exception {
        String name = testCase.get("name").asText();
        List<Recorded> requests = Collections.synchronizedList(new ArrayList<>());
        JsonNode responses = testCase.get("responses");
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", exchange -> {
            byte[] body = exchange.getRequestBody().readAllBytes();
            int index;
            synchronized (requests) {
                index = requests.size();
                requests.add(new Recorded(exchange.getRequestMethod(), exchange.getRequestURI().getPath(),
                        exchange.getRequestHeaders(), body));
            }
            if (index >= responses.size()) {
                exchange.sendResponseHeaders(500, -1);
                exchange.close();
                return;
            }
            JsonNode response = responses.get(index);
            byte[] payload;
            String contentType;
            if (response.has("json")) {
                payload = Json.MAPPER.writeValueAsBytes(response.get("json"));
                contentType = "application/json";
            } else if (response.has("sse")) {
                payload = Files.readAllBytes(SUITE.resolve("fixtures").resolve(response.get("sse").asText()));
                contentType = "text/event-stream";
            } else {
                payload = response.get("text").asText().getBytes(StandardCharsets.UTF_8);
                contentType = "text/plain";
            }
            exchange.getResponseHeaders().set("Content-Type", contentType);
            response.path("headers").properties().forEach(header ->
                    exchange.getResponseHeaders().set(header.getKey(), header.getValue().asText()));
            exchange.sendResponseHeaders(response.get("status").asInt(), payload.length);
            exchange.getResponseBody().write(payload);
            exchange.close();
        });
        server.start();
        try {
            exercise(testCase, name, async, requests, "http://127.0.0.1:" + server.getAddress().getPort());
        } finally {
            server.stop(0);
        }
    }

    static void exercise(JsonNode testCase, String name, boolean async, List<Recorded> requests, String url)
            throws Exception {
        List<Double> sleeps = new ArrayList<>();
        GatewayClient.Builder builder = GatewayClient.builder(url).apiKey("gw_conformance_key")
                .sleeper(duration -> {
                    sleeps.add(duration.toMillis() / 1000.0);
                    return CompletableFuture.completedFuture(null);
                });
        if (testCase.path("client").has("max_retries")) {
            builder.maxRetries(testCase.get("client").get("max_retries").asInt());
        }
        GatewayClient client = builder.build();
        JsonNode call = testCase.get("call");
        CallOptions options = new CallOptions(
                call.hasNonNull("idempotency_key") ? call.get("idempotency_key").asText() : null,
                call.hasNonNull("request_id") ? call.get("request_id").asText() : null);
        JsonNode expect = testCase.get("expect");

        JsonNode result = null;
        ArrayNode events = Json.MAPPER.createArrayNode();
        Throwable failure = null;
        try {
            switch (testCase.get("operation").asText()) {
                case "chat" -> {
                    ChatRequest request = Json.convert(call.get("body"), ChatRequest.class);
                    ChatResponse response = async ? unwrapJoin(client.chatAsync(request, options))
                            : client.chat(request, options);
                    result = Json.MAPPER.valueToTree(response);
                }
                case "embeddings" -> {
                    EmbeddingRequest request = Json.convert(call.get("body"), EmbeddingRequest.class);
                    EmbeddingResponse response = async ? unwrapJoin(client.embeddingsAsync(request, options))
                            : client.embeddings(request, options);
                    result = Json.MAPPER.valueToTree(response);
                }
                case "chat_stream" -> {
                    ChatRequest request = Json.convert(call.get("body"), ChatRequest.class);
                    try (ChatStream stream = async ? unwrapJoin(client.chatStreamAsync(request, options))
                            : client.chatStream(request, options)) {
                        String served = responses(testCase).path("headers").path("x-request-id").asText(null);
                        assertEquals(served, stream.requestId(), name + ": stream request id");
                        for (StreamEvent event : stream) {
                            ObjectNode item = events.addObject();
                            item.put("event", event.eventName());
                            item.set("data", Json.MAPPER.valueToTree(event));
                        }
                    }
                }
                default -> fail(name + ": unknown operation");
            }
        } catch (GatewayException error) {
            failure = error;
        }

        if (testCase.get("operation").asText().equals("chat_stream")) {
            subset(expect.get("events"), events, name + ".events");
        }
        if (expect.has("error")) {
            assertNotNull(failure, name + ": expected an error");
            checkError(expect.get("error"), failure, name);
        } else if (failure != null) {
            throw new AssertionError(name + ": unexpected " + failure, failure);
        }
        if (expect.has("result")) {
            subset(expect.get("result"), result, name + ".result");
        }

        JsonNode expectRequest = testCase.get("expect_request");
        assertEquals(expect.get("attempts").asInt(), requests.size(), name + ": attempts");
        Set<String> keys = new HashSet<>();
        for (Recorded request : requests) {
            assertEquals(expectRequest.get("method").asText(), request.method(), name);
            assertEquals(expectRequest.get("path").asText(), request.path(), name);
            assertEquals("application/json", request.headers().getFirst("Content-Type"), name);
            for (Map.Entry<String, JsonNode> header : expectRequest.get("headers").properties()) {
                assertEquals(header.getValue().asText(), request.headers().getFirst(header.getKey()),
                        name + ": header " + header.getKey());
            }
            for (JsonNode header : expectRequest.path("absent_headers")) {
                assertFalse(request.headers().containsKey(header.asText()), name + ": " + header + " sent");
            }
            String key = request.headers().getFirst("Idempotency-Key");
            if (expectRequest.path("generated_idempotency_key").asBoolean()) {
                assertTrue(key != null && !key.isEmpty() && key.length() <= 255, name + ": key " + key);
            }
            keys.add(key);
            assertEquals(expectRequest.get("body"), Json.MAPPER.readTree(request.body()), name + ": body");
        }
        assertEquals(1, keys.size(), name + ": keys " + keys);
        List<Double> expectedSleeps = new ArrayList<>();
        expect.get("sleeps").forEach(sleep -> expectedSleeps.add(sleep.asDouble()));
        assertEquals(expectedSleeps, sleeps, name + ": sleeps");
    }

    static JsonNode responses(JsonNode testCase) {
        JsonNode responses = testCase.get("responses");
        return responses.get(responses.size() - 1);
    }

    static <T> T unwrapJoin(CompletableFuture<T> future) {
        try {
            return future.join();
        } catch (java.util.concurrent.CompletionException error) {
            if (error.getCause() instanceof GatewayException gateway) {
                throw gateway;
            }
            throw error;
        }
    }

    static void checkError(JsonNode expected, Throwable failure, String name) {
        switch (expected.get("kind").asText()) {
            case "problem" -> {
                ProblemException problem = assertInstanceOf(ProblemException.class, failure, name);
                assertEquals(expected.get("status").asInt(), problem.status(), name);
                assertEquals(expected.get("code").asText(), problem.code(), name);
                if (expected.has("request_id")) {
                    assertEquals(expected.get("request_id").asText(), problem.requestId(), name);
                }
                if (expected.has("retry_after")) {
                    assertEquals(expected.get("retry_after").asLong(), problem.retryAfter().orElse(-1), name);
                }
            }
            case "http" -> assertEquals(expected.get("status").asInt(),
                    assertInstanceOf(HttpStatusException.class, failure, name).status(), name);
            case "stream" -> {
                StreamException stream = assertInstanceOf(StreamException.class, failure, name);
                assertEquals(expected.get("code").asText(), stream.code(), name);
                assertEquals(expected.get("status").asInt(), stream.status(), name);
            }
            case "protocol" -> assertInstanceOf(ProtocolException.class, failure, name);
            default -> fail(name + ": unknown error kind " + expected.get("kind"));
        }
    }

    /** The suite's match: listed keys must be present and equal; arrays must have the listed length. */
    static void subset(JsonNode expected, JsonNode actual, String path) {
        assertNotNull(actual, path + ": missing");
        if (expected.isObject()) {
            assertTrue(actual.isObject(), path + ": expected an object, got " + actual);
            expected.properties().forEach(field -> {
                assertTrue(actual.has(field.getKey()), path + "." + field.getKey() + ": missing in " + actual);
                subset(field.getValue(), actual.get(field.getKey()), path + "." + field.getKey());
            });
        } else if (expected.isArray()) {
            assertTrue(actual.isArray() && actual.size() == expected.size(),
                    path + ": expected " + expected.size() + " items, got " + actual);
            for (int index = 0; index < expected.size(); index++) {
                subset(expected.get(index), actual.get(index), path + "[" + index + "]");
            }
        } else if (expected.isNumber()) {
            assertTrue(actual.isNumber(), path + ": expected a number, got " + actual);
            assertEquals(0, expected.decimalValue().compareTo(actual.decimalValue()), path + ": " + actual);
        } else if (expected.isNull()) {
            assertNull(actual.isNull() ? null : actual, path);
        } else {
            assertEquals(expected, actual, path);
        }
    }
}
