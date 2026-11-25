package com.qucelerate.aigateway;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.ByteArrayInputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executors;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Flow;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import org.junit.jupiter.api.Test;

class ChatStreamTest {
    static final String STARTED = "event: response.started\ndata: {\"model\":\"m\"}\n\n";
    static final String DELTA = "event: choice.delta\ndata: {\"choice_index\":0,\"content_delta\":\"a\","
            + "\"refusal_delta\":null,\"annotations\":[]}\n\n";
    static final String COMPLETED = "event: response.completed\ndata: {\"model\":\"m\",\"usage\":null}\n\n";

    static ChatStream streamOf(String text) {
        return streamOf(new ByteArrayInputStream(text.getBytes(StandardCharsets.UTF_8)));
    }

    static ChatStream streamOf(InputStream input) {
        return new ChatStream(input, "req-1");
    }

    /** The event names read, and what ended the stream (null when it completed). */
    record Outcome(List<String> names, GatewayException error) {}

    static Outcome drain(ChatStream stream) {
        List<String> names = new ArrayList<>();
        try {
            for (StreamEvent event : stream) {
                names.add(event.eventName());
            }
            return new Outcome(names, null);
        } catch (GatewayException error) {
            return new Outcome(names, error);
        }
    }

    @Test
    void anUnknownEventWithNonJsonDataIsSkipped() {
        Outcome outcome = drain(streamOf(STARTED + "event: ping\ndata: keepalive\n\n" + COMPLETED));
        assertEquals(List.of("response.started", "response.completed"), outcome.names());
        assertEquals(null, outcome.error());
    }

    @ParameterizedTest
    @ValueSource(strings = {"not json", "[]", "{\"code\":\"x\",\"status\":\"bad\"}", "{\"status\":502}",
            "{\"code\":\"x\",\"status\":1.5}"})
    void aMalformedErrorEventKeepsEarlierEvents(String data) {
        Outcome outcome = drain(streamOf(STARTED + DELTA + "event: error\ndata: " + data + "\n\n"));
        assertEquals(List.of("response.started", "choice.delta"), outcome.names());
        assertInstanceOf(DecodeException.class, outcome.error());
    }

    @ParameterizedTest
    @ValueSource(strings = {"[]", "{\"choice_index\":\"x\"}", "{\"annotations\":3}"})
    void aMalformedKnownEventIsADecodeError(String data) {
        Outcome outcome = drain(streamOf(STARTED + "event: choice.delta\ndata: " + data + "\n\n"));
        assertEquals(List.of("response.started"), outcome.names());
        assertInstanceOf(DecodeException.class, outcome.error());
    }

    @Test
    void anOversizedEventIsAProtocolError() {
        Outcome outcome = drain(streamOf(STARTED + "a".repeat(SseParser.MAX_EVENT_BYTES + 1)));
        assertEquals(List.of("response.started"), outcome.names());
        assertInstanceOf(ProtocolException.class, outcome.error());
    }

    @Test
    void eventsAfterTheTerminalOneAreIgnored() {
        Outcome outcome = drain(streamOf(STARTED + COMPLETED + DELTA));
        assertEquals(List.of("response.started", "response.completed"), outcome.names());
    }

    @Test
    void closingTheStreamClosesTheBody() {
        boolean[] closed = {false};
        InputStream input = new ByteArrayInputStream(STARTED.getBytes(StandardCharsets.UTF_8)) {
            @Override
            public void close() {
                closed[0] = true;
            }
        };
        try (ChatStream stream = streamOf(input)) {
            assertTrue(stream.hasNext());
        }
        assertTrue(closed[0]);
    }

    @Test
    void thePublisherHonorsDemandAndCompletes() throws Exception {
        ExecutorService executor = Executors.newSingleThreadExecutor();
        try {
            List<String> names = new ArrayList<>();
            CompletableFuture<Void> done = new CompletableFuture<>();
            Flow.Subscription[] subscription = new Flow.Subscription[1];
            streamOf(STARTED + DELTA + COMPLETED).publisher(executor).subscribe(new Flow.Subscriber<>() {
                public void onSubscribe(Flow.Subscription s) {
                    subscription[0] = s;
                    s.request(1);
                }

                public void onNext(StreamEvent event) {
                    synchronized (names) {
                        names.add(event.eventName());
                    }
                    subscription[0].request(1);
                }

                public void onError(Throwable error) {
                    done.completeExceptionally(error);
                }

                public void onComplete() {
                    done.complete(null);
                }
            });
            done.get(5, TimeUnit.SECONDS);
            assertEquals(List.of("response.started", "choice.delta", "response.completed"), names);
        } finally {
            executor.shutdownNow();
        }
    }

    @Test
    void thePublisherSendsTheStreamError() throws Exception {
        ExecutorService executor = Executors.newSingleThreadExecutor();
        try {
            CompletableFuture<Throwable> failed = new CompletableFuture<>();
            streamOf(STARTED + "event: error\ndata: {\"code\":\"provider_failure\",\"status\":502}\n\n")
                    .publisher(executor).subscribe(new Flow.Subscriber<>() {
                        public void onSubscribe(Flow.Subscription s) {
                            s.request(Long.MAX_VALUE);
                        }

                        public void onNext(StreamEvent event) {}

                        public void onError(Throwable error) {
                            failed.complete(error);
                        }

                        public void onComplete() {
                            failed.complete(null);
                        }
                    });
            StreamException error = assertInstanceOf(StreamException.class, failed.get(5, TimeUnit.SECONDS));
            assertEquals("provider_failure", error.code());
        } finally {
            executor.shutdownNow();
        }
    }

    @Test
    void aSecondSubscriberIsRefused() throws Exception {
        Flow.Publisher<StreamEvent> publisher = streamOf(COMPLETED).publisher(Runnable::run);
        publisher.subscribe(new Collecting());
        Collecting second = new Collecting();
        publisher.subscribe(second);
        assertInstanceOf(IllegalStateException.class, second.error.get(5, TimeUnit.SECONDS));
    }

    @Test
    void aNonPositiveRequestIsAnError() throws Exception {
        Collecting subscriber = new Collecting() {
            @Override
            public void onSubscribe(Flow.Subscription s) {
                s.request(0);
            }
        };
        streamOf(COMPLETED).publisher(Runnable::run).subscribe(subscriber);
        assertInstanceOf(IllegalArgumentException.class, subscriber.error.get(5, TimeUnit.SECONDS));
    }

    @Test
    void theIteratorThrowsAfterTheEnd() {
        ChatStream stream = streamOf(COMPLETED);
        stream.next();
        assertThrows(java.util.NoSuchElementException.class, stream::next);
    }

    /** Records every signal, to check there is exactly one terminal signal. */
    static class Recording implements Flow.Subscriber<StreamEvent> {
        final long initial;
        final List<String> signals = java.util.Collections.synchronizedList(new ArrayList<>());
        final CompletableFuture<Void> terminal = new CompletableFuture<>();
        Flow.Subscription subscription;

        Recording(long initial) {
            this.initial = initial;
        }

        public void onSubscribe(Flow.Subscription s) {
            subscription = s;
            if (initial > 0) {
                s.request(initial);
            }
        }

        public void onNext(StreamEvent event) {
            signals.add(event.eventName());
        }

        public void onError(Throwable error) {
            signals.add("error:" + error.getClass().getSimpleName());
            terminal.complete(null);
        }

        public void onComplete() {
            signals.add("complete");
            terminal.complete(null);
        }
    }

    @Test
    void exactDemandStillCompletes() throws Exception {
        Recording subscriber = new Recording(3);
        streamOf(STARTED + DELTA + COMPLETED).publisher(Runnable::run).subscribe(subscriber);
        subscriber.terminal.get(5, TimeUnit.SECONDS);
        assertEquals(List.of("response.started", "choice.delta", "response.completed", "complete"),
                subscriber.signals);
    }

    @Test
    void aRejectingExecutorFailsTheSubscriberOnce() throws Exception {
        boolean[] closed = {false};
        InputStream input = new ByteArrayInputStream(COMPLETED.getBytes(StandardCharsets.UTF_8)) {
            @Override
            public void close() {
                closed[0] = true;
            }
        };
        Recording subscriber = new Recording(1);
        streamOf(input).publisher(task -> {
            throw new java.util.concurrent.RejectedExecutionException("full");
        }).subscribe(subscriber);
        subscriber.terminal.get(5, TimeUnit.SECONDS);
        subscriber.subscription.request(1);
        assertEquals(List.of("error:RejectedExecutionException"), subscriber.signals);
        assertTrue(closed[0]);
    }

    @Test
    void cancellingStopsSignalsAndClosesTheStream() throws Exception {
        boolean[] closed = {false};
        InputStream input = new ByteArrayInputStream((STARTED + DELTA + COMPLETED).getBytes(StandardCharsets.UTF_8)) {
            @Override
            public void close() {
                closed[0] = true;
            }
        };
        Recording subscriber = new Recording(0) {
            @Override
            public void onNext(StreamEvent event) {
                super.onNext(event);
                subscription.cancel();
            }
        };
        streamOf(input).publisher(Runnable::run).subscribe(subscriber);
        subscriber.subscription.request(10);
        assertEquals(List.of("response.started"), subscriber.signals);
        assertTrue(closed[0]);
    }

    static class Collecting implements Flow.Subscriber<StreamEvent> {
        final CompletableFuture<Throwable> error = new CompletableFuture<>();

        public void onSubscribe(Flow.Subscription s) {
            s.request(Long.MAX_VALUE);
        }

        public void onNext(StreamEvent event) {}

        public void onError(Throwable failure) {
            error.complete(failure);
        }

        public void onComplete() {}
    }
}
