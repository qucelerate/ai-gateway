package com.qucelerate.aigateway;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;

class BatchesTest {
    static final String BATCH = "{\"id\":\"batch_1\",\"object\":\"batch\",\"endpoint\":\"/v1/chat/completions\","
            + "\"input_file_id\":\"file_1\",\"completion_window\":\"24h\",\"status\":\"completed\","
            + "\"output_file_id\":\"file_2\",\"error_file_id\":null,\"created_at\":1,\"expires_at\":2,"
            + "\"request_counts\":{\"total\":2,\"completed\":2,\"failed\":0},\"metadata\":{},"
            + "\"gateway\":{\"jobs\":[{\"mode\":\"native\"}]},\"new_field\":1}";

    record Seen(String method, String uri, String key, String contentType, String body) {}

    @Test
    void filesAndBatchesUseTheirPathsAndHeaders() throws Exception {
        List<Seen> seen = new ArrayList<>();
        GatewayClientTest.withServer(exchange -> {
            String body = new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            String uri = exchange.getRequestURI().toString();
            synchronized (seen) {
                seen.add(new Seen(exchange.getRequestMethod(), uri,
                        exchange.getRequestHeaders().getFirst("Idempotency-Key"),
                        exchange.getRequestHeaders().getFirst("Content-Type"), body));
            }
            String reply;
            int status = 200;
            if (uri.equals("/api/inference/v1/files")) {
                reply = "{\"id\":\"file_1\",\"object\":\"file\",\"bytes\":3,\"filename\":\"b.jsonl\","
                        + "\"purpose\":\"batch\",\"created_at\":1}";
            } else if (uri.endsWith("/content")) {
                reply = "{\"custom_id\":\"a\"}\n";
            } else if (uri.startsWith("/api/inference/v1/batches?")) {
                reply = "{\"object\":\"list\",\"data\":[" + BATCH + "],\"has_more\":true}";
            } else if (uri.endsWith("/missing")) {
                status = 404;
                reply = "{\"code\":\"batch_not_found\",\"status\":404}";
                exchange.getResponseHeaders().set("Content-Type", "application/problem+json");
            } else {
                reply = BATCH;
            }
            byte[] bytes = reply.getBytes(StandardCharsets.UTF_8);
            if (status == 200) {
                exchange.getResponseHeaders().set("Content-Type", "application/json");
            }
            exchange.sendResponseHeaders(status, bytes.length);
            try (OutputStream out = exchange.getResponseBody()) {
                out.write(bytes);
            }
        }, url -> {
            GatewayClient client = GatewayClientTest.client(url).build();
            FileObject file = client.uploadBatchFile("b.jsonl", "{}\n".getBytes(StandardCharsets.UTF_8));
            assertEquals("file_1", file.id());
            assertTrue(seen.get(0).contentType().startsWith("multipart/form-data; boundary="));
            assertTrue(seen.get(0).body().contains("name=\"purpose\"\r\n\r\nbatch\r\n"));
            assertNull(seen.get(0).key());

            Batch batch = client.createBatch(new BatchRequest("file_1", BatchRequest.CHAT_COMPLETIONS,
                    Map.of("run", "a")), CallOptions.idempotencyKey("k-1"));
            assertTrue(batch.done());
            assertEquals(2, batch.requestCounts().completed());
            assertEquals("native", batch.gateway().path("jobs").path(0).path("mode").asText());
            assertEquals("k-1", seen.get(1).key());
            JsonNode sent = Json.tree(seen.get(1).body());
            assertEquals("24h", sent.path("completion_window").asText());
            assertEquals("/v1/chat/completions", sent.path("endpoint").asText());
            assertEquals("a", sent.path("metadata").path("run").asText());

            BatchList list = client.listBatches("batch_0", 5);
            assertTrue(list.hasMore());
            assertEquals("/api/inference/v1/batches?after=batch_0&limit=5", seen.get(2).uri());
            client.cancelBatch("batch_1");
            assertEquals("POST", seen.get(3).method());
            assertEquals("/api/inference/v1/batches/batch_1/cancel", seen.get(3).uri());
            assertNull(seen.get(3).key());
            assertArrayEquals("{\"custom_id\":\"a\"}\n".getBytes(StandardCharsets.UTF_8), client.fileContent("file_2"));

            ProblemException missing = assertThrows(ProblemException.class, () -> client.batch("missing"));
            assertEquals("batch_not_found", missing.code());
            assertThrows(ConfigException.class, () -> client.batch("../files"));
            assertThrows(ConfigException.class, () -> new BatchRequest("file_1", "/v1/responses"));
        });
    }
}
