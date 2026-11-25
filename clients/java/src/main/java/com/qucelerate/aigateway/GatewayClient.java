package com.qucelerate.aigateway;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import java.io.IOException;
import java.io.InputStream;
import java.net.URI;
import java.net.URISyntaxException;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.Locale;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Flow;
import java.util.concurrent.Future;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Function;

/**
 * A client for the Gateway inference API, contract 1.0.0: chat, streaming chat
 * and embeddings, each with a blocking and a {@link CompletableFuture} form.
 *
 * <pre>{@code
 * GatewayClient client = GatewayClient.builder("https://gateway.example.com")
 *         .apiKey(System.getenv("GATEWAY_API_KEY"))
 *         .build();
 * ChatResponse response = client.chat(new ChatRequest("chat-default", List.of(Message.user("Say hello."))));
 * System.out.println(response.content());
 * }</pre>
 *
 * <p>The client is a transport wrapper: authorization, budgets, routing and
 * settlement stay on the gateway. Every call sends an {@code Idempotency-Key}
 * and is retried automatically only when the gateway refused it before
 * reserving anything ({@code rate_limit_exceeded},
 * {@code concurrency_limit_exceeded}, {@code provider_circuit_open}). It is safe
 * for concurrent use.
 */
public final class GatewayClient {
    static final String VERSION = "2026.11.0-rc.4";
    private static final String CHAT_PATH = "/api/inference/v1/chat/completions";
    private static final String STREAM_PATH = "/api/inference/v1/chat/completions/stream";
    private static final String EMBEDDINGS_PATH = "/api/inference/v1/embeddings";
    private static final String FILES_PATH = "/api/inference/v1/files";
    private static final String BATCHES_PATH = "/api/inference/v1/batches";
    private static final long MAX_WAIT_SECONDS = 60;
    private static final int MAX_KEY_CHARS = 255;
    private static final int MAX_REQUEST_ID_CHARS = 128;
    private static final int MAX_ERROR_BODY_BYTES = 1 << 20;
    private static final List<String> SAME_KEY_RETRY_CODES =
            List.of("rate_limit_exceeded", "concurrency_limit_exceeded", "provider_circuit_open");

    /** Cancels attempts that pass their timeout; one daemon thread for every client. */
    private static final ScheduledThreadPoolExecutor TIMERS = timers();

    /** How the client waits before a retry; replaced in tests. */
    interface Sleeper {
        CompletableFuture<Void> sleep(Duration duration);
    }

    private final String baseUrl;
    private final String apiKey;
    private final int maxRetries;
    private final Duration timeout;
    private final HttpClient http;
    private final Sleeper sleeper;
    private final int maxResponseBytes;

    private GatewayClient(Builder builder) {
        this.baseUrl = builder.baseUrl;
        this.apiKey = builder.apiKey;
        this.maxRetries = builder.maxRetries;
        this.timeout = builder.timeout;
        this.http = builder.http != null
                ? builder.http
                : HttpClient.newBuilder().connectTimeout(min(timeout, Duration.ofSeconds(10))).build();
        this.sleeper = builder.sleeper;
        this.maxResponseBytes = builder.maxResponseBytes;
    }

    /** A builder for the gateway at {@code baseUrl}, such as {@code https://gateway.example.com}. */
    public static Builder builder(String baseUrl) {
        return new Builder(baseUrl);
    }

    /** Create a chat completion. */
    public ChatResponse chat(ChatRequest request) {
        return chat(request, CallOptions.DEFAULT);
    }

    public ChatResponse chat(ChatRequest request, CallOptions options) {
        return await(chatAsync(request, options));
    }

    public CompletableFuture<ChatResponse> chatAsync(ChatRequest request) {
        return chatAsync(request, CallOptions.DEFAULT);
    }

    public CompletableFuture<ChatResponse> chatAsync(ChatRequest request, CallOptions options) {
        return call(CHAT_PATH, request, options, false, response ->
                Json.decode(response.body().bytes(), ChatResponse.class).withRequestId(requestId(response)));
    }

    /** Create one embedding per input. */
    public EmbeddingResponse embeddings(EmbeddingRequest request) {
        return embeddings(request, CallOptions.DEFAULT);
    }

    public EmbeddingResponse embeddings(EmbeddingRequest request, CallOptions options) {
        return await(embeddingsAsync(request, options));
    }

    public CompletableFuture<EmbeddingResponse> embeddingsAsync(EmbeddingRequest request) {
        return embeddingsAsync(request, CallOptions.DEFAULT);
    }

    public CompletableFuture<EmbeddingResponse> embeddingsAsync(EmbeddingRequest request, CallOptions options) {
        return call(EMBEDDINGS_PATH, request, options, false, response ->
                Json.decode(response.body().bytes(), EmbeddingResponse.class).withRequestId(requestId(response)));
    }

    /**
     * Stream a chat completion. The response must start within the client's
     * timeout; after that there is no limit between events, so close the stream
     * to stop it.
     */
    public ChatStream chatStream(ChatRequest request) {
        return chatStream(request, CallOptions.DEFAULT);
    }

    public ChatStream chatStream(ChatRequest request, CallOptions options) {
        return await(chatStreamAsync(request, options));
    }

    public CompletableFuture<ChatStream> chatStreamAsync(ChatRequest request) {
        return chatStreamAsync(request, CallOptions.DEFAULT);
    }

    public CompletableFuture<ChatStream> chatStreamAsync(ChatRequest request, CallOptions options) {
        return call(STREAM_PATH, request, options, true,
                response -> new ChatStream(response.body().stream(), requestId(response)));
    }

    /**
     * Upload a batch input file: JSONL, one request per line, each
     * {@code {"custom_id", "method": "POST", "url", "body"}}.
     */
    public FileObject uploadBatchFile(String filename, byte[] content) {
        return await(uploadBatchFileAsync(filename, content));
    }

    public CompletableFuture<FileObject> uploadBatchFileAsync(String filename, byte[] content) {
        Objects.requireNonNull(content, "content");
        if (filename == null || filename.isEmpty() || filename.matches(".*[\"\\r\\n].*")) {
            return CompletableFuture.failedFuture(new ConfigException("not a file name: " + filename));
        }
        String boundary = "gateway-" + UUID.randomUUID();
        byte[] head = ("--" + boundary + "\r\ncontent-disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n"
                + "--" + boundary + "\r\ncontent-disposition: form-data; name=\"file\"; filename=\"" + filename
                + "\"\r\ncontent-type: application/jsonl\r\n\r\n").getBytes(StandardCharsets.UTF_8);
        byte[] tail = ("\r\n--" + boundary + "--\r\n").getBytes(StandardCharsets.UTF_8);
        byte[] body = new byte[head.length + content.length + tail.length];
        System.arraycopy(head, 0, body, 0, head.length);
        System.arraycopy(content, 0, body, head.length, content.length);
        System.arraycopy(tail, 0, body, head.length + content.length, tail.length);
        HttpRequest request = plain(FILES_PATH)
                .POST(HttpRequest.BodyPublishers.ofByteArray(body))
                .header("Content-Type", "multipart/form-data; boundary=" + boundary)
                .build();
        return send(request, false, response -> Json.decode(response.body().bytes(), FileObject.class));
    }

    /**
     * Create a batch from an uploaded file. The whole batch is checked, routed
     * and reserved when it is created, or refused with nothing held. Retry with
     * the same idempotency key to get the same batch.
     */
    public Batch createBatch(BatchRequest request, CallOptions options) {
        return await(createBatchAsync(request, options));
    }

    public CompletableFuture<Batch> createBatchAsync(BatchRequest request, CallOptions options) {
        Objects.requireNonNull(request, "request");
        return call(BATCHES_PATH, request, options, false, GatewayClient::batch);
    }

    /** A batch, with the gateway's account of how it runs and its cost. */
    public Batch batch(String batchId) {
        return await(batchAsync(batchId));
    }

    public CompletableFuture<Batch> batchAsync(String batchId) {
        return get(BATCHES_PATH + "/", batchId, "", GatewayClient::batch);
    }

    /** Ask a batch to stop: lines not yet run end without a charge. */
    public Batch cancelBatch(String batchId) {
        return await(cancelBatchAsync(batchId));
    }

    public CompletableFuture<Batch> cancelBatchAsync(String batchId) {
        if (!isId(batchId)) {
            return CompletableFuture.failedFuture(new ConfigException("not an id: " + batchId));
        }
        HttpRequest request = plain(BATCHES_PATH + "/" + batchId + "/cancel")
                .POST(HttpRequest.BodyPublishers.noBody())
                .build();
        return send(request, false, GatewayClient::batch);
    }

    /**
     * The API key's service account's batches, newest first: at most
     * {@code limit} (0 for the gateway's default), after the batch
     * {@code after} ({@code null} for the first page).
     */
    public BatchList listBatches(String after, int limit) {
        return await(listBatchesAsync(after, limit));
    }

    public CompletableFuture<BatchList> listBatchesAsync(String after, int limit) {
        if (after != null && !isId(after)) {
            return CompletableFuture.failedFuture(new ConfigException("not an id: " + after));
        }
        List<String> query = new java.util.ArrayList<>();
        if (after != null) {
            query.add("after=" + after);
        }
        if (limit > 0) {
            query.add("limit=" + limit);
        }
        String path = BATCHES_PATH + (query.isEmpty() ? "" : "?" + String.join("&", query));
        HttpRequest request = plain(path).GET().build();
        return send(request, false, response -> Json.decode(response.body().bytes(), BatchList.class));
    }

    /**
     * A file's content: a batch's output or error file is JSONL, one line per
     * request line, matched by {@code custom_id}.
     */
    public byte[] fileContent(String fileId) {
        return await(fileContentAsync(fileId));
    }

    public CompletableFuture<byte[]> fileContentAsync(String fileId) {
        return get(FILES_PATH + "/", fileId, "/content", response -> response.body().bytes());
    }

    private <T> CompletableFuture<T> get(String prefix, String id, String suffix,
            Function<HttpResponse<Body>, T> finish) {
        if (!isId(id)) {
            return CompletableFuture.failedFuture(new ConfigException("not an id: " + id));
        }
        return send(plain(prefix + id + suffix).GET().build(), false, finish);
    }

    /** A files or batches request that creates nothing, so has no key. */
    private HttpRequest.Builder plain(String path) {
        return HttpRequest.newBuilder(URI.create(baseUrl + path))
                .header("Authorization", "Bearer " + apiKey)
                .header("User-Agent", "qucelerate-ai-gateway-java/" + VERSION);
    }

    private static Batch batch(HttpResponse<Body> response) {
        return Json.decode(response.body().bytes(), Batch.class).withRequestId(requestId(response));
    }

    /** An id that is one path segment: it can never name another path. */
    private static boolean isId(String id) {
        return id != null && id.matches("[A-Za-z0-9_-]+");
    }

    @Override
    public String toString() {
        return "GatewayClient{baseUrl=" + baseUrl + ", apiKey=<redacted>, maxRetries=" + maxRetries + "}";
    }

    /** A response body: the open stream of a successful stream call, else the bytes. */
    record Body(byte[] bytes, InputStream stream) {}

    /**
     * One call's in-flight step: the HTTP exchange or the wait before a retry.
     * Cancelling the call's public future cancels that step, so no request or
     * retry keeps running after the caller stops waiting.
     */
    private static final class Call {
        private volatile boolean cancelled;
        private volatile Future<?> current;

        /** Track the step now running; cancel it at once if the call already was. */
        <T extends Future<?>> T track(T step) {
            current = step;
            if (cancelled) {
                step.cancel(true);
            }
            return step;
        }

        void cancel() {
            cancelled = true;
            Future<?> step = current;
            if (step != null) {
                step.cancel(true);
            }
        }
    }

    /**
     * Send the request and turn the response into the call's result. A result
     * that arrives after the caller cancelled is closed when it is a stream.
     */
    private <T> CompletableFuture<T> call(String path, Object body, CallOptions options, boolean stream,
            Function<HttpResponse<Body>, T> finish) {
        HttpRequest request;
        try {
            request = request(path, body, options);
        } catch (GatewayException error) {
            return CompletableFuture.failedFuture(error);
        }
        return send(request, stream, finish);
    }

    private <T> CompletableFuture<T> send(HttpRequest request, boolean stream,
            Function<HttpResponse<Body>, T> finish) {
        CompletableFuture<T> result = new CompletableFuture<>();
        Call call = new Call();
        result.whenComplete((value, error) -> {
            if (result.isCancelled()) {
                call.cancel();
            }
        });
        attempt(call, request, stream, 0).whenComplete((response, error) -> {
            if (error != null) {
                result.completeExceptionally(gatewayError(error));
                return;
            }
            T value;
            try {
                value = finish.apply(response);
            } catch (RuntimeException failure) {
                closeQuietly(response.body().stream());
                result.completeExceptionally(failure);
                return;
            }
            if (!result.complete(value) && value instanceof ChatStream late) {
                late.close();
            }
        });
        return result;
    }

    private HttpRequest request(String path, Object body, CallOptions options) {
        Objects.requireNonNull(body, "request");
        Objects.requireNonNull(options, "options");
        String key = options.idempotencyKey();
        if (key == null) {
            key = UUID.randomUUID().toString();
        } else if (key.isEmpty() || key.codePointCount(0, key.length()) > MAX_KEY_CHARS) {
            throw new ConfigException("an Idempotency-Key must be 1 to " + MAX_KEY_CHARS + " characters");
        }
        String requestId = options.requestId();
        if (requestId != null && requestId.codePointCount(0, requestId.length()) > MAX_REQUEST_ID_CHARS) {
            throw new ConfigException("an X-Request-Id must be at most " + MAX_REQUEST_ID_CHARS + " characters");
        }
        byte[] json;
        try {
            json = Json.MAPPER.writeValueAsBytes(body);
        } catch (JsonProcessingException error) {
            throw new ConfigException("the request is not serializable: " + error);
        }
        HttpRequest.Builder builder = HttpRequest.newBuilder(URI.create(baseUrl + path))
                .POST(HttpRequest.BodyPublishers.ofByteArray(json))
                .header("Authorization", "Bearer " + apiKey)
                .header("Content-Type", "application/json")
                .header("Idempotency-Key", key)
                .header("User-Agent", "qucelerate-ai-gateway-java/" + VERSION);
        if (requestId != null) {
            builder.header("X-Request-Id", requestId);
        }
        return builder.build();
    }

    private CompletableFuture<HttpResponse<Body>> attempt(Call call, HttpRequest request, boolean stream,
            int retries) {
        return timed(call.track(http.sendAsync(request, handler(stream, maxResponseBytes)))).thenCompose(response -> {
            int status = response.statusCode();
            if (status >= 200 && status < 300) {
                return CompletableFuture.completedFuture(response);
            }
            GatewayException error = errorFrom(response);
            if (error instanceof ProblemException problem && retries < maxRetries
                    && SAME_KEY_RETRY_CODES.contains(problem.code())) {
                long seconds = Math.min(problem.retryAfter().orElse(1), MAX_WAIT_SECONDS);
                return call.track(sleeper.sleep(Duration.ofSeconds(seconds)))
                        .thenCompose(ignored -> attempt(call, request, stream, retries + 1));
            }
            return CompletableFuture.failedFuture(error);
        });
    }

    /**
     * The attempt, cancelled (which aborts the exchange) when it does not
     * complete within the timeout. A stream's attempt completes when its
     * response starts; any other attempt when its body is read.
     */
    private <T> CompletableFuture<T> timed(CompletableFuture<T> attempt) {
        AtomicBoolean timedOut = new AtomicBoolean();
        ScheduledFuture<?> timer = TIMERS.schedule(() -> {
            timedOut.set(true);
            attempt.cancel(true);
        }, timeout.toNanos(), TimeUnit.NANOSECONDS);
        return attempt.handle((value, error) -> {
            timer.cancel(false);
            if (error == null) {
                return value;
            }
            Throwable cause = unwrap(error);
            if (timedOut.get() || cause instanceof java.net.http.HttpTimeoutException) {
                throw new TransportException("the attempt did not finish within " + timeout, cause, true);
            }
            if (cause instanceof GatewayException gateway) {
                throw gateway;
            }
            throw new TransportException("the request failed: " + cause, cause, false);
        });
    }

    private static HttpResponse.BodyHandler<Body> handler(boolean stream, int maxResponseBytes) {
        return info -> {
            boolean success = info.statusCode() >= 200 && info.statusCode() < 300;
            if (stream && success) {
                return HttpResponse.BodySubscribers.mapping(
                        HttpResponse.BodySubscribers.ofInputStream(), input -> new Body(null, input));
            }
            // A successful body over the limit fails the call; an error body is
            // only read far enough to find its problem.
            LimitedBytes bytes = success ? new LimitedBytes(maxResponseBytes, true)
                    : new LimitedBytes(MAX_ERROR_BODY_BYTES, false);
            return HttpResponse.BodySubscribers.mapping(bytes, body -> new Body(body, null));
        };
    }

    private static GatewayException gatewayError(Throwable error) {
        Throwable cause = unwrap(error);
        if (cause instanceof GatewayException gateway) {
            return gateway;
        }
        if (cause instanceof CancellationException) {
            return new TransportException("the call was cancelled", cause, false);
        }
        return new TransportException("the request failed: " + cause, cause, false);
    }

    private static void closeQuietly(InputStream stream) {
        if (stream != null) {
            try {
                stream.close();
            } catch (IOException ignored) {
                // Releasing the connection is best effort.
            }
        }
    }

    private static GatewayException errorFrom(HttpResponse<Body> response) {
        int status = response.statusCode();
        Long retryAfter = response.headers().firstValue("Retry-After").map(String::trim)
                .filter(value -> !value.isEmpty() && value.chars().allMatch(Character::isDigit))
                .map(value -> {
                    try {
                        return Long.parseLong(value);
                    } catch (NumberFormatException tooLarge) {
                        return Long.MAX_VALUE;
                    }
                })
                .orElse(null);
        String contentType = response.headers().firstValue("Content-Type").orElse("").toLowerCase(Locale.ROOT);
        byte[] bytes = response.body().bytes();
        if (contentType.contains("json") && bytes != null) {
            try {
                JsonNode problem = Json.MAPPER.readTree(bytes);
                if (problem != null && problem.isObject() && problem.path("code").isTextual()) {
                    return new ProblemException(status, text(problem, "type"), problem.get("code").asText(),
                            text(problem, "title"), text(problem, "detail"), text(problem, "instance"),
                            text(problem, "request_id"), retryAfter);
                }
            } catch (IOException notJson) {
                // Not a problem body: report the status.
            }
        }
        return new HttpStatusException(status, requestId(response));
    }

    private static String text(JsonNode node, String field) {
        JsonNode value = node.get(field);
        return value == null || value.isNull() ? null : value.asText();
    }

    private static String requestId(HttpResponse<?> response) {
        return response.headers().firstValue("X-Request-Id").orElse(null);
    }

    /** Wait for a call, throwing its GatewayException; an interrupt cancels it. */
    private static <T> T await(CompletableFuture<T> future) {
        try {
            return future.get();
        } catch (InterruptedException interrupted) {
            future.cancel(true);
            Thread.currentThread().interrupt();
            throw new TransportException("interrupted", interrupted, false);
        } catch (ExecutionException | CancellationException error) {
            throw gatewayError(error);
        }
    }

    private static Throwable unwrap(Throwable error) {
        while ((error instanceof CompletionException || error instanceof ExecutionException)
                && error.getCause() != null) {
            error = error.getCause();
        }
        return error;
    }

    private static Duration min(Duration a, Duration b) {
        return a.compareTo(b) <= 0 ? a : b;
    }

    private static ScheduledThreadPoolExecutor timers() {
        ScheduledThreadPoolExecutor executor = new ScheduledThreadPoolExecutor(1, runnable -> {
            Thread thread = new Thread(runnable, "ai-gateway-timeouts");
            thread.setDaemon(true);
            return thread;
        });
        executor.setRemoveOnCancelPolicy(true);
        return executor;
    }

    private static CompletableFuture<Void> sleep(Duration duration) {
        CompletableFuture<Void> done = new CompletableFuture<>();
        ScheduledFuture<?> wake = TIMERS.schedule(() -> done.complete(null), duration.toNanos(), TimeUnit.NANOSECONDS);
        done.whenComplete((ignored, error) -> wake.cancel(false));
        return done;
    }

    /** Collects a body up to {@code limit} bytes, then stops reading. */
    private static final class LimitedBytes implements HttpResponse.BodySubscriber<byte[]> {
        private final int limit;
        private final boolean failWhenExceeded;
        private final CompletableFuture<byte[]> result = new CompletableFuture<>();
        private byte[] bytes = new byte[0];
        private int length;
        private Flow.Subscription subscription;

        LimitedBytes(int limit, boolean failWhenExceeded) {
            this.limit = limit;
            this.failWhenExceeded = failWhenExceeded;
        }

        @Override
        public CompletableFuture<byte[]> getBody() {
            return result;
        }

        @Override
        public void onSubscribe(Flow.Subscription subscription) {
            this.subscription = subscription;
            subscription.request(Long.MAX_VALUE);
        }

        @Override
        public void onNext(List<ByteBuffer> buffers) {
            if (result.isDone()) {
                return;
            }
            if (failWhenExceeded) {
                long total = length;
                for (ByteBuffer buffer : buffers) {
                    total += buffer.remaining();
                }
                if (total > limit) {
                    subscription.cancel();
                    result.completeExceptionally(
                            new DecodeException("the response body exceeded " + limit + " bytes", null));
                    return;
                }
            }
            for (ByteBuffer buffer : buffers) {
                int count = Math.min(buffer.remaining(), limit - length);
                if (length + count > bytes.length) {
                    bytes = java.util.Arrays.copyOf(bytes, Math.max(bytes.length * 2, length + count));
                }
                buffer.get(bytes, length, count);
                length += count;
            }
            // A successful body ends at onComplete, so one byte past the limit
            // is still seen; an error body is only needed up to the limit.
            if (!failWhenExceeded && length >= limit && !result.isDone()) {
                subscription.cancel();
                result.complete(java.util.Arrays.copyOf(bytes, length));
            }
        }

        @Override
        public void onError(Throwable error) {
            result.completeExceptionally(error);
        }

        @Override
        public void onComplete() {
            result.complete(java.util.Arrays.copyOf(bytes, length));
        }
    }

    /** Configures a {@link GatewayClient}. */
    public static final class Builder {
        private final String baseUrl;
        private String apiKey;
        private int maxRetries = 2;
        private Duration timeout = Duration.ofSeconds(120);
        private HttpClient http;
        private Sleeper sleeper = GatewayClient::sleep;
        private int maxResponseBytes = 64 << 20;

        private Builder(String baseUrl) {
            Objects.requireNonNull(baseUrl, "baseUrl");
            URI uri;
            try {
                uri = new URI(baseUrl);
            } catch (URISyntaxException error) {
                throw new ConfigException("base URL must be http or https: " + baseUrl);
            }
            if (!("http".equals(uri.getScheme()) || "https".equals(uri.getScheme())) || uri.getHost() == null) {
                throw new ConfigException("base URL must be http or https: " + baseUrl);
            }
            this.baseUrl = baseUrl.replaceAll("/+$", "");
        }

        /** The {@code gw_} API key sent as the bearer credential. Required. */
        public Builder apiKey(String apiKey) {
            this.apiKey = apiKey;
            return this;
        }

        /**
         * How many times a request the gateway refused before reserving anything
         * is retried (default 2). Other failures are never retried automatically.
         */
        public Builder maxRetries(int maxRetries) {
            this.maxRetries = maxRetries;
            return this;
        }

        /**
         * The limit for each attempt (default 120 seconds): a non-streaming call's
         * whole response, or the start of a stream's.
         */
        public Builder timeout(Duration timeout) {
            this.timeout = Objects.requireNonNull(timeout, "timeout");
            return this;
        }

        /**
         * The largest successful non-streaming response body read (default 64
         * MiB); a larger one fails the call with a {@link DecodeException}.
         */
        public Builder maxResponseBytes(int maxResponseBytes) {
            if (maxResponseBytes < 1) {
                throw new ConfigException("maxResponseBytes must be positive");
            }
            this.maxResponseBytes = maxResponseBytes;
            return this;
        }

        /** Your own {@link HttpClient}, for example for a proxy or TLS roots. */
        public Builder httpClient(HttpClient http) {
            this.http = Objects.requireNonNull(http, "http");
            return this;
        }

        Builder sleeper(Sleeper sleeper) {
            this.sleeper = sleeper;
            return this;
        }

        public GatewayClient build() {
            if (apiKey == null || apiKey.isBlank()) {
                throw new ConfigException("an API key is required");
            }
            if (maxRetries < 0) {
                throw new ConfigException("maxRetries must be 0 or more");
            }
            if (timeout.isNegative() || timeout.isZero()) {
                throw new ConfigException("timeout must be positive");
            }
            return new GatewayClient(this);
        }
    }
}
