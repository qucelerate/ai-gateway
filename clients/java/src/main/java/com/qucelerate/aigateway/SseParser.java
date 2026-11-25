package com.qucelerate.aigateway;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;

/**
 * An incremental text/event-stream parser, as the WHATWG HTML specification
 * defines it: LF, CR and CRLF line endings, comment lines, data split across
 * lines and joined with LF, and chunk boundaries anywhere. Each byte is scanned
 * once, however the stream is chunked, and an unfinished event (its pending line
 * plus the data so far) is capped at {@link #MAX_EVENT_BYTES}.
 */
final class SseParser {
    static final int MAX_EVENT_BYTES = 16 << 20;

    record Event(String name, String data) {}

    /** An event grew past {@link #MAX_EVENT_BYTES}. */
    static final class Oversized extends Exception {
        private static final long serialVersionUID = 1L;

        Oversized() {
            super(null, null, false, false);
        }
    }

    private final Bytes line = new Bytes();
    private final Bytes data = new Bytes();
    private boolean hasData;
    private boolean skipLF;
    private String name = "";

    /**
     * Parse {@code length} bytes of {@code chunk}, adding the events they
     * complete to {@code events}, even those before an {@link Oversized}.
     */
    void feed(byte[] chunk, int length, List<Event> events) throws Oversized {
        int start = 0;
        if (skipLF && length > 0) {
            if (chunk[0] == '\n') {
                start = 1;
            }
            skipLF = false;
        }
        for (int index = start; index < length; index++) {
            byte b = chunk[index];
            if (b != '\n' && b != '\r') {
                continue;
            }
            append(chunk, start, index);
            dispatch(events);
            if (b == '\r') {
                if (index + 1 < length) {
                    if (chunk[index + 1] == '\n') {
                        index++;
                    }
                } else {
                    skipLF = true;
                }
            }
            start = index + 1;
        }
        append(chunk, start, length);
    }

    private void append(byte[] chunk, int from, int to) throws Oversized {
        if ((long) data.length + line.length + (to - from) > MAX_EVENT_BYTES) {
            throw new Oversized();
        }
        line.append(chunk, from, to - from);
    }

    private void dispatch(List<Event> events) {
        String text = line.text();
        line.length = 0;
        if (text.isEmpty()) {
            if (hasData) {
                events.add(new Event(name.isEmpty() ? "message" : name, data.text()));
            }
            name = "";
            data.length = 0;
            hasData = false;
            return;
        }
        if (text.startsWith(":")) {
            return;
        }
        int colon = text.indexOf(':');
        String field = colon < 0 ? text : text.substring(0, colon);
        String value = colon < 0 ? "" : text.substring(colon + 1);
        if (value.startsWith(" ")) {
            value = value.substring(1);
        }
        switch (field) {
            case "event" -> name = value;
            case "data" -> {
                // One buffer, joined with LF as it grows, so the limit counts
                // every byte held, however many lines the event has.
                if (hasData) {
                    data.append(new byte[] {'\n'}, 0, 1);
                }
                byte[] bytes = value.getBytes(StandardCharsets.UTF_8);
                data.append(bytes, 0, bytes.length);
                hasData = true;
            }
            default -> {}
        }
    }

    /** A growable byte buffer. */
    private static final class Bytes {
        byte[] bytes = new byte[256];
        int length;

        void append(byte[] source, int offset, int count) {
            if (length + count > bytes.length) {
                bytes = Arrays.copyOf(bytes, Math.max(bytes.length * 2, length + count));
            }
            System.arraycopy(source, offset, bytes, length, count);
            length += count;
        }

        /** UTF-8, with invalid sequences replaced. */
        String text() {
            return new String(bytes, 0, length, StandardCharsets.UTF_8);
        }
    }
}
