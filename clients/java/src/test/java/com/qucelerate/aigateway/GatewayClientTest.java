package com.qucelerate.aigateway;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.sun.net.httpserver.HttpHandler;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;

class GatewayClientTest {
    static final ChatRequest HELLO = new ChatRequest("m", List.of(Message.user("hi")));

    interface ServerBody {
        void run(String url) throws Exception;
    }

    static void withServer(HttpHandler handler, ServerBody body) throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.setExecutor(java.util.concurrent.Executors.newCachedThreadPool());
        server.createContext("/", handler);
        server.start();
        try {
            body.run("http://127.0.0.1:" + server.getAddress().getPort());
        } finally {
            server.stop(0);
        }
    }

    static GatewayClient.Builder client(String url) {
        return GatewayClient.builder(url).apiKey("gw_key");
    }

    @Test
    void aStreamThatNeverStartsTimesOut() throws Exception {
        CountDownLatch release = new CountDownLatch(1);
        withServer(exchange -> {
            try {
                release.await(30, TimeUnit.SECONDS);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            exchange.close();
        }, url -> {
            long started = System.nanoTime();
            TransportException error = assertThrows(TransportException.class,
                    () -> client(url).timeout(Duration.ofMillis(300)).build().chatStream(HELLO));
            release.countDown();
            assertTrue(error.timedOut());
            assertTrue(System.nanoTime() - started < TimeUnit.SECONDS.toNanos(5));
        });
    }

    @Test
    void aStreamOutlivesTheTimeoutOnceStarted() throws Exception {
        withServer(exchange -> {
            exchange.getResponseHeaders().set("Content-Type", "text/event-stream");
            exchange.sendResponseHeaders(200, 0);
            OutputStream out = exchange.getResponseBody();
            out.write(ChatStreamTest.STARTED.getBytes(StandardCharsets.UTF_8));
            out.flush();
            try {
                Thread.sleep(600);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            out.write(ChatStreamTest.COMPLETED.getBytes(StandardCharsets.UTF_8));
            exchange.close();
        }, url -> {
            try (ChatStream stream = client(url).timeout(Duration.ofMillis(300)).build().chatStream(HELLO)) {
                ChatStreamTest.Outcome outcome = ChatStreamTest.drain(stream);
                assertEquals(List.of("response.started", "response.completed"), outcome.names());
                assertEquals(null, outcome.error());
            }
        });
    }

    @Test
    void aSlowBodyTimesOutANonStreamingCall() throws Exception {
        withServer(exchange -> {
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.sendResponseHeaders(200, 0);
            exchange.getResponseBody().write("{\"id\":".getBytes(StandardCharsets.UTF_8));
            exchange.getResponseBody().flush();
            try {
                Thread.sleep(3000);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            exchange.close();
        }, url -> {
            TransportException error = assertThrows(TransportException.class,
                    () -> client(url).timeout(Duration.ofMillis(300)).build().chat(HELLO));
            assertTrue(error.timedOut());
        });
    }

    @Test
    void anErrorBodyCutShortIsATransportError() throws Exception {
        try (ServerSocket listener = new ServerSocket(0, 50, java.net.InetAddress.getLoopbackAddress())) {
            Thread server = new Thread(() -> {
                for (int i = 0; i < 2; i++) {
                    try (Socket socket = listener.accept()) {
                        InputStream in = socket.getInputStream();
                        byte[] buffer = new byte[65536];
                        in.read(buffer);
                        socket.getOutputStream().write(("HTTP/1.1 503 X\r\ncontent-type: application/json\r\n"
                                + "content-length: 1000\r\n\r\n{\"code\":").getBytes(StandardCharsets.UTF_8));
                    } catch (IOException closed) {
                        return;
                    }
                }
            });
            server.setDaemon(true);
            server.start();
            GatewayClient client = client("http://127.0.0.1:" + listener.getLocalPort()).build();
            assertInstanceOf(TransportException.class, assertThrows(GatewayException.class, () -> client.chat(HELLO)));
            assertInstanceOf(TransportException.class,
                    assertThrows(GatewayException.class, () -> client.chatStream(HELLO)));
        }
    }

    @Test
    void aHugeRetryAfterIsCapped() throws Exception {
        int[] calls = {0};
        withServer(exchange -> {
            exchange.getRequestBody().readAllBytes();
            byte[] body;
            if (calls[0]++ == 0) {
                exchange.getResponseHeaders().set("Content-Type", "application/problem+json");
                exchange.getResponseHeaders().set("Retry-After", "99999999999999999999999");
                body = "{\"code\":\"rate_limit_exceeded\"}".getBytes(StandardCharsets.UTF_8);
                exchange.sendResponseHeaders(429, body.length);
            } else {
                exchange.getResponseHeaders().set("Content-Type", "application/json");
                body = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[],\"usage\":null}".getBytes(StandardCharsets.UTF_8);
                exchange.sendResponseHeaders(200, body.length);
            }
            exchange.getResponseBody().write(body);
            exchange.close();
        }, url -> {
            List<Duration> sleeps = new ArrayList<>();
            GatewayClient client = client(url).sleeper(duration -> {
                sleeps.add(duration);
                return CompletableFuture.completedFuture(null);
            }).build();
            assertEquals("c", client.chat(HELLO).id());
            assertEquals(List.of(Duration.ofSeconds(60)), sleeps);
        });
    }

    @Test
    void aMalformedBodyIsADecodeError() throws Exception {
        for (String body : List.of("{\"id\":\"c\",\"usage\":[]}", "[]", "null", "{\"choices\":[\"x\"]}", "not json")) {
            withServer(exchange -> {
                byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
                exchange.getResponseHeaders().set("Content-Type", "application/json");
                exchange.sendResponseHeaders(200, bytes.length);
                exchange.getResponseBody().write(bytes);
                exchange.close();
            }, url -> assertThrows(DecodeException.class, () -> client(url).build().chat(HELLO), body));
        }
    }

    @Test
    void configurationIsChecked() {
        assertThrows(ConfigException.class, () -> GatewayClient.builder("ftp://x"));
        assertThrows(ConfigException.class, () -> GatewayClient.builder("not a url"));
        assertThrows(ConfigException.class, () -> GatewayClient.builder("https://x").build());
        assertThrows(ConfigException.class, () -> GatewayClient.builder("https://x").apiKey(" ").build());
        assertThrows(ConfigException.class, () -> client("https://x").maxRetries(-1).build());
        assertThrows(ConfigException.class, () -> client("https://x").timeout(Duration.ZERO).build());
        GatewayClient client = client("https://x").build();
        assertThrows(ConfigException.class, () -> client.chat(HELLO, CallOptions.idempotencyKey("")));
        assertThrows(ConfigException.class, () -> client.chat(HELLO, CallOptions.idempotencyKey("k".repeat(256))));
        assertThrows(ConfigException.class,
                () -> client.chat(HELLO, CallOptions.DEFAULT.withRequestId("r".repeat(129))));
    }

    @Test
    void theApiKeyIsRedacted() {
        assertFalse(client("https://x").apiKey("gw_secret").build().toString().contains("gw_secret"));
    }

    @Test
    void anInterruptCancelsABlockingCall() throws Exception {
        CountDownLatch release = new CountDownLatch(1);
        withServer(exchange -> {
            try {
                release.await(30, TimeUnit.SECONDS);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            exchange.close();
        }, url -> {
            GatewayClient client = client(url).build();
            CompletableFuture<Throwable> thrown = new CompletableFuture<>();
            Thread caller = new Thread(() -> {
                try {
                    client.chat(HELLO);
                    thrown.complete(null);
                } catch (Throwable error) {
                    thrown.complete(error);
                }
            });
            caller.start();
            Thread.sleep(200);
            caller.interrupt();
            assertInstanceOf(TransportException.class, thrown.get(5, TimeUnit.SECONDS));
            release.countDown();
        });
    }

    /** A server that accepts one request and reports when the client hangs up. */
    static final class HangingServer implements AutoCloseable {
        final ServerSocket listener;
        final CompletableFuture<Void> requested = new CompletableFuture<>();
        final CompletableFuture<Void> disconnected = new CompletableFuture<>();

        HangingServer() throws IOException {
            listener = new ServerSocket(0, 50, java.net.InetAddress.getLoopbackAddress());
            Thread thread = new Thread(() -> {
                try (Socket socket = listener.accept()) {
                    InputStream in = socket.getInputStream();
                    byte[] buffer = new byte[65536];
                    in.read(buffer);
                    requested.complete(null);
                    while (in.read(buffer) >= 0) {
                        // Wait for the client to close the connection.
                    }
                    disconnected.complete(null);
                } catch (IOException closed) {
                    disconnected.complete(null);
                }
            });
            thread.setDaemon(true);
            thread.start();
        }

        String url() {
            return "http://127.0.0.1:" + listener.getLocalPort();
        }

        @Override
        public void close() throws IOException {
            listener.close();
        }
    }

    @Test
    void cancellingAnAsyncCallAbortsTheRequest() throws Exception {
        try (HangingServer server = new HangingServer()) {
            CompletableFuture<ChatResponse> call = client(server.url()).build().chatAsync(HELLO);
            server.requested.get(5, TimeUnit.SECONDS);
            call.cancel(true);
            server.disconnected.get(5, TimeUnit.SECONDS);
        }
    }

    @Test
    void interruptingABlockingStreamCallAbortsTheRequest() throws Exception {
        try (HangingServer server = new HangingServer()) {
            GatewayClient client = client(server.url()).build();
            Thread caller = new Thread(() -> {
                try {
                    client.chatStream(HELLO).close();
                } catch (GatewayException expected) {
                    // Interrupted.
                }
            });
            caller.start();
            server.requested.get(5, TimeUnit.SECONDS);
            caller.interrupt();
            server.disconnected.get(5, TimeUnit.SECONDS);
        }
    }

    @Test
    void cancellingDuringARetryWaitStopsTheRetry() throws Exception {
        int[] calls = {0};
        withServer(exchange -> {
            exchange.getRequestBody().readAllBytes();
            synchronized (calls) {
                calls[0]++;
            }
            byte[] body = "{\"code\":\"rate_limit_exceeded\"}".getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "application/problem+json");
            exchange.sendResponseHeaders(429, body.length);
            exchange.getResponseBody().write(body);
            exchange.close();
        }, url -> {
            CompletableFuture<Void> wait = new CompletableFuture<>();
            CompletableFuture<Void> waiting = new CompletableFuture<>();
            GatewayClient client = client(url).sleeper(duration -> {
                waiting.complete(null);
                return wait;
            }).build();
            CompletableFuture<ChatResponse> call = client.chatAsync(HELLO);
            waiting.get(5, TimeUnit.SECONDS);
            call.cancel(true);
            // The sleeper signals before it returns the wait, so the client may
            // track (and so cancel) it just after this cancel: wait for that.
            assertThrows(CancellationException.class, () -> wait.get(5, TimeUnit.SECONDS));
            Thread.sleep(200);
            synchronized (calls) {
                assertEquals(1, calls[0]);
            }
        });
    }

    @Test
    void aBodyOverTheResponseLimitIsADecodeError() throws Exception {
        withServer(exchange -> {
            byte[] body = ("{\"id\":\"" + "x".repeat(4096) + "\"}").getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.sendResponseHeaders(200, body.length);
            exchange.getResponseBody().write(body);
            exchange.close();
        }, url -> {
            DecodeException error = assertThrows(DecodeException.class,
                    () -> client(url).maxResponseBytes(1024).build().chat(HELLO));
            assertTrue(error.getMessage().contains("1024"));
        });
        assertThrows(ConfigException.class, () -> client("https://x").maxResponseBytes(0));
    }

    @Test
    void anInvalidRequestDuringABlockedReadSignalsOneError() throws Exception {
        CountDownLatch release = new CountDownLatch(1);
        withServer(exchange -> {
            exchange.getResponseHeaders().set("Content-Type", "text/event-stream");
            exchange.sendResponseHeaders(200, 0);
            exchange.getResponseBody().write(ChatStreamTest.STARTED.getBytes(StandardCharsets.UTF_8));
            exchange.getResponseBody().flush();
            try {
                release.await(30, TimeUnit.SECONDS);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            exchange.close();
        }, url -> {
            java.util.concurrent.ExecutorService executor = java.util.concurrent.Executors.newSingleThreadExecutor();
            try (ChatStream stream = client(url).build().chatStream(HELLO)) {
                ChatStreamTest.Recording subscriber = new ChatStreamTest.Recording(2);
                stream.publisher(executor).subscribe(subscriber);
                long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
                while (subscriber.signals.isEmpty() && System.nanoTime() < deadline) {
                    Thread.sleep(10);
                }
                Thread.sleep(200); // the drain is now blocked reading the second event
                subscriber.subscription.request(0);
                subscriber.terminal.get(5, TimeUnit.SECONDS);
                Thread.sleep(200);
                assertEquals(List.of("response.started", "error:IllegalArgumentException"), subscriber.signals);
            } finally {
                release.countDown();
                executor.shutdownNow();
            }
        });
    }

    @Test
    void theResponseLimitIsExact() throws Exception {
        String body = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[],\"usage\":null}";
        for (String suffix : List.of("", " ")) {
            withServer(exchange -> {
                byte[] bytes = (body + suffix).getBytes(StandardCharsets.UTF_8);
                exchange.getResponseHeaders().set("Content-Type", "application/json");
                exchange.sendResponseHeaders(200, bytes.length);
                exchange.getResponseBody().write(bytes);
                exchange.close();
            }, url -> {
                GatewayClient client = client(url).maxResponseBytes(body.length()).build();
                if (suffix.isEmpty()) {
                    assertEquals("c", client.chat(HELLO).id());
                } else {
                    assertThrows(DecodeException.class, () -> client.chat(HELLO));
                }
            });
        }
    }
}
