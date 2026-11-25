package com.qucelerate.aigateway;

import com.fasterxml.jackson.databind.JsonNode;
import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.Executor;
import java.util.concurrent.Flow;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * The events of a streamed chat completion. Iterate it until {@code hasNext()}
 * is false; a terminal error, an early close or a malformed event is thrown
 * from {@code hasNext()} after the events before it. Always close it (use
 * try-with-resources); closing it from another thread cancels the stream. It is
 * not safe for concurrent iteration. {@link #publisher(Executor)} adapts it to
 * {@link Flow.Publisher} for reactive code.
 */
public final class ChatStream implements Iterator<StreamEvent>, Iterable<StreamEvent>, AutoCloseable {
    private static final Set<String> KNOWN = Set.of(
            "response.started", "choice.delta", "tool_call.delta", "choice.finished", "safety.annotation", "usage",
            "response.completed");

    private final InputStream body;
    private final String requestId;
    private final SseParser parser = new SseParser();
    private final ArrayDeque<StreamEvent> pending = new ArrayDeque<>();
    private final byte[] buffer = new byte[8192];
    private final AtomicBoolean closed = new AtomicBoolean();
    private GatewayException failure;
    private boolean done;

    ChatStream(InputStream body, String requestId) {
        this.body = body;
        this.requestId = requestId;
    }

    /** The {@code X-Request-Id} response header, or {@code null}. */
    public String requestId() {
        return requestId;
    }

    @Override
    public Iterator<StreamEvent> iterator() {
        return this;
    }

    @Override
    public boolean hasNext() {
        while (pending.isEmpty()) {
            if (failure != null) {
                GatewayException error = failure;
                failure = null;
                close();
                throw error;
            }
            if (done) {
                close();
                return false;
            }
            read();
        }
        return true;
    }

    @Override
    public StreamEvent next() {
        if (!hasNext()) {
            throw new NoSuchElementException();
        }
        return pending.poll();
    }

    private void read() {
        int count;
        try {
            count = body.read(buffer);
        } catch (IOException error) {
            done = true;
            failure = closed.get()
                    ? new TransportException("the stream was closed", error, false)
                    : new TransportException("reading the stream failed: " + error.getMessage(), error, false);
            return;
        }
        if (count < 0) {
            done = true;
            failure = new ProtocolException("the stream closed before a terminal event");
            return;
        }
        List<SseParser.Event> events = new ArrayList<>();
        boolean oversized = false;
        try {
            parser.feed(buffer, count, events);
        } catch (SseParser.Oversized error) {
            oversized = true;
        }
        for (SseParser.Event event : events) {
            if (done) {
                return;
            }
            accept(event);
        }
        if (oversized && !done) {
            done = true;
            failure = new ProtocolException("a stream event exceeded " + SseParser.MAX_EVENT_BYTES + " bytes");
        }
    }

    private void accept(SseParser.Event event) {
        if (event.name().equals("error")) {
            done = true;
            failure = streamError(event.data());
            return;
        }
        if (!KNOWN.contains(event.name())) {
            return;
        }
        try {
            StreamEvent decoded = decode(event.name(), Json.tree(event.data()));
            pending.add(decoded);
            done = decoded instanceof StreamEvent.ResponseCompleted;
        } catch (IOException | IllegalArgumentException error) {
            done = true;
            failure = new DecodeException(event.name() + ": " + error.getMessage(), error);
        }
    }

    private static StreamEvent decode(String name, JsonNode data) throws IOException {
        if (!data.isObject()) {
            throw new IllegalArgumentException("the payload is not an object");
        }
        return switch (name) {
            case "response.started" -> Json.convert(data, StreamEvent.ResponseStarted.class);
            case "choice.delta" -> Json.convert(data, StreamEvent.ChoiceDelta.class);
            case "tool_call.delta" -> Json.convert(data, StreamEvent.ToolCallDelta.class);
            case "choice.finished" -> Json.convert(data, StreamEvent.ChoiceFinished.class);
            case "safety.annotation" -> Json.convert(data, StreamEvent.SafetyAnnotation.class);
            case "usage" -> new StreamEvent.UsageReport(Json.convert(data, Usage.class));
            case "response.completed" -> Json.convert(data, StreamEvent.ResponseCompleted.class);
            default -> throw new IllegalStateException(name);
        };
    }

    /** The terminal error event's exception, or a DecodeException if malformed. */
    private static GatewayException streamError(String data) {
        try {
            JsonNode payload = Json.tree(data);
            JsonNode code = payload.get("code");
            JsonNode status = payload.get("status");
            if (code == null || !code.isTextual() || status == null || !status.canConvertToInt()
                    || !status.isIntegralNumber()) {
                return new DecodeException("error event: code must be a string and status an integer", null);
            }
            return new StreamException(code.asText(), status.asInt());
        } catch (IOException error) {
            return new DecodeException("error event: " + error.getMessage(), error);
        }
    }

    /** Close the connection. Safe to call more than once, from any thread. */
    @Override
    public void close() {
        if (closed.compareAndSet(false, true)) {
            try {
                body.close();
            } catch (IOException ignored) {
                // Closing is best effort; the connection is released either way.
            }
        }
    }

    /**
     * This stream as a single-use {@link Flow.Publisher} that reads on
     * {@code executor} as its subscriber requests events, then completes, fails
     * with the stream's exception, or closes the stream on cancel.
     */
    public Flow.Publisher<StreamEvent> publisher(Executor executor) {
        Objects.requireNonNull(executor, "executor");
        AtomicBoolean subscribed = new AtomicBoolean();
        return subscriber -> {
            Objects.requireNonNull(subscriber, "subscriber");
            if (!subscribed.compareAndSet(false, true)) {
                subscriber.onSubscribe(new Flow.Subscription() {
                    public void request(long n) {}

                    public void cancel() {}
                });
                subscriber.onError(new IllegalStateException("a ChatStream publisher accepts one subscriber"));
                return;
            }
            subscriber.onSubscribe(new StreamSubscription(subscriber, executor));
        };
    }

    /**
     * Reads and signals only on the drain task, one at a time, so signals are
     * serialized and exactly one terminal signal is sent (rules 1.3 and 1.7).
     * {@code request} and {@code cancel} only record state and schedule a drain.
     */
    private final class StreamSubscription implements Flow.Subscription {
        private final Flow.Subscriber<? super StreamEvent> subscriber;
        private final Executor executor;
        private final AtomicLong demand = new AtomicLong();
        private final AtomicBoolean draining = new AtomicBoolean();
        private final AtomicBoolean terminated = new AtomicBoolean();
        private volatile boolean cancelled;
        private volatile boolean invalidRequest;

        StreamSubscription(Flow.Subscriber<? super StreamEvent> subscriber, Executor executor) {
            this.subscriber = subscriber;
            this.executor = executor;
        }

        @Override
        public void request(long n) {
            if (n <= 0) {
                // Rule 3.9: signal onError. Closing the body wakes a blocked read.
                invalidRequest = true;
                close();
            } else {
                demand.getAndAccumulate(n, (current, add) -> current + add < 0 ? Long.MAX_VALUE : current + add);
            }
            schedule();
        }

        @Override
        public void cancel() {
            cancelled = true;
            terminated.set(true);
            close();
        }

        private void schedule() {
            if (terminated.get() || !draining.compareAndSet(false, true)) {
                return;
            }
            try {
                executor.execute(this::drain);
            } catch (RuntimeException rejected) {
                // No drain runs now, so this thread may send the terminal signal.
                draining.set(false);
                close();
                terminate(rejected);
            }
        }

        /** Send the one terminal signal: onComplete when error is null, else onError. */
        private void terminate(Throwable error) {
            if (!terminated.compareAndSet(false, true)) {
                return;
            }
            close();
            if (error == null) {
                subscriber.onComplete();
            } else {
                subscriber.onError(error);
            }
        }

        /** True when the next hasNext() answers without reading or waiting for demand. */
        private boolean terminalKnown() {
            return pending.isEmpty() && (done || failure != null);
        }

        private void drain() {
            try {
                while (!terminated.get()) {
                    if (invalidRequest) {
                        terminate(new IllegalArgumentException("request(n) needs n > 0 (rule 3.9)"));
                        return;
                    }
                    if (demand.get() == 0 && !terminalKnown()) {
                        break;
                    }
                    boolean more;
                    try {
                        more = hasNext();
                    } catch (GatewayException error) {
                        terminate(invalidRequest ? new IllegalArgumentException(
                                "request(n) needs n > 0 (rule 3.9)") : error);
                        return;
                    }
                    if (!more) {
                        terminate(null);
                        return;
                    }
                    if (demand.get() == 0) {
                        break;
                    }
                    StreamEvent event = pending.poll();
                    demand.decrementAndGet();
                    if (!cancelled) {
                        subscriber.onNext(event);
                    }
                }
            } catch (RuntimeException subscriberFailure) {
                // Rule 2.13: a subscriber that throws is treated as cancelled.
                cancel();
                return;
            } finally {
                draining.set(false);
            }
            if (!terminated.get() && (invalidRequest || demand.get() > 0 || terminalKnown())) {
                schedule();
            }
        }
    }
}
