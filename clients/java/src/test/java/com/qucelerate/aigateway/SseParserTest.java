package com.qucelerate.aigateway;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTimeoutPreemptively;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

class SseParserTest {
    static List<SseParser.Event> parse(byte[] text, int size) throws SseParser.Oversized {
        SseParser parser = new SseParser();
        List<SseParser.Event> events = new ArrayList<>();
        for (int start = 0; start < text.length; start += size) {
            byte[] chunk = Arrays.copyOfRange(text, start, Math.min(start + size, text.length));
            parser.feed(chunk, chunk.length, events);
        }
        return events;
    }

    static byte[] repeat(String text, int times) {
        return text.repeat(times).getBytes(StandardCharsets.UTF_8);
    }

    @Test
    void everyChunkingGivesTheSameEvents() throws Exception {
        byte[] text = "event: a\r\ndata: {\"x\":\r\ndata: \"é\"}\r\n\r\n: comment\n\nevent: b\rdata: 2\r\r"
                .getBytes(StandardCharsets.UTF_8);
        List<SseParser.Event> want = List.of(new SseParser.Event("a", "{\"x\":\n\"é\"}"), new SseParser.Event("b", "2"));
        for (int size = 1; size <= text.length; size++) {
            assertEquals(want, parse(text, size), "chunk size " + size);
        }
    }

    @Test
    void anEventWithoutABlankLineIsNotDispatched() throws Exception {
        assertTrue(parse("event: a\ndata: 1\n".getBytes(StandardCharsets.UTF_8), 4).isEmpty());
    }

    @Test
    void emptyDataLinesJoinWithLF() throws Exception {
        List<String> data = parse("data:\ndata:\ndata: x\n\ndata:\n\n".getBytes(StandardCharsets.UTF_8), 3)
                .stream().map(SseParser.Event::data).toList();
        assertEquals(List.of("\n\nx", ""), data);
    }

    @Test
    void anEndlessLineFailsAtTheLimit() throws Exception {
        SseParser parser = new SseParser();
        List<SseParser.Event> events = new ArrayList<>();
        byte[] chunk = repeat("a", 1 << 20);
        for (int i = 0; i < 16; i++) {
            parser.feed(chunk, chunk.length, events);
        }
        assertThrows(SseParser.Oversized.class, () -> parser.feed(new byte[] {'a'}, 1, events));
        assertTrue(events.isEmpty());
    }

    @Test
    void manyEmptyDataLinesCountTowardTheLimit() {
        SseParser parser = new SseParser();
        byte[] chunk = repeat("data:\n", 1 << 20);
        assertThrows(SseParser.Oversized.class, () -> {
            for (int i = 0; i < 17; i++) {
                parser.feed(chunk, chunk.length, new ArrayList<>());
            }
        });
    }

    @Test
    void eventsBeforeAnOversizedOneAreKept() {
        byte[] head = "data: 1\n\n".getBytes(StandardCharsets.UTF_8);
        byte[] chunk = Arrays.copyOf(head, head.length + SseParser.MAX_EVENT_BYTES + 1);
        Arrays.fill(chunk, head.length, chunk.length, (byte) 'a');
        List<SseParser.Event> events = new ArrayList<>();
        assertThrows(SseParser.Oversized.class, () -> new SseParser().feed(chunk, chunk.length, events));
        assertEquals(1, events.size());
    }

    @Test
    void aLongEventInTinyChunksIsLinear() {
        byte[] text = ("event: a\ndata: " + "x".repeat(4 << 20) + "\n\n").getBytes(StandardCharsets.UTF_8);
        List<SseParser.Event> events = assertTimeoutPreemptively(Duration.ofSeconds(10), () -> parse(text, 7));
        assertEquals(1, events.size());
        assertEquals(4 << 20, events.get(0).data().length());
    }
}
