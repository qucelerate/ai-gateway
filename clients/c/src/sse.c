/*
 * An incremental text/event-stream parser, as the WHATWG HTML specification
 * defines it: LF, CR and CRLF line endings, comment lines, data split across
 * lines and joined with LF, and chunk boundaries anywhere. Each byte is scanned
 * once, however the stream is chunked, and an unfinished event (its pending
 * line plus the data so far) is capped at GW_MAX_EVENT_BYTES.
 */
#include "internal.h"

#include <string.h>

static gw_sse_result append_line(gw_sse_parser *parser, const char *bytes, size_t count) {
    if (parser->data.length + parser->line.length + count > GW_MAX_EVENT_BYTES) {
        return GW_SSE_OVERSIZED;
    }
    return gw_buffer_append(&parser->line, bytes, count) ? GW_SSE_OK : GW_SSE_NOMEM;
}

static gw_sse_result dispatch(gw_sse_parser *parser, gw_sse_event_fn on_event, void *ctx) {
    const char *line = parser->line.data ? parser->line.data : "";
    size_t length = parser->line.length;
    gw_sse_result result = GW_SSE_OK;
    if (length == 0) {
        if (parser->has_data) {
            const char *name = parser->name.length ? parser->name.data : "message";
            if (!on_event(ctx, name, parser->data.data ? parser->data.data : "", parser->data.length)) {
                result = GW_SSE_NOMEM;
            }
        }
        gw_buffer_clear(&parser->name);
        gw_buffer_clear(&parser->data);
        parser->has_data = false;
    } else if (line[0] != ':') {
        const char *colon = memchr(line, ':', length);
        size_t field_length = colon ? (size_t)(colon - line) : length;
        const char *value = colon ? colon + 1 : line + length;
        size_t value_length = length - (size_t)(value - line);
        if (value_length > 0 && value[0] == ' ') {
            value++;
            value_length--;
        }
        if (field_length == 5 && memcmp(line, "event", 5) == 0) {
            gw_buffer_clear(&parser->name);
            if (!gw_buffer_append(&parser->name, value, value_length)) {
                result = GW_SSE_NOMEM;
            }
        } else if (field_length == 4 && memcmp(line, "data", 4) == 0) {
            /* One buffer, joined with LF as it grows, so the limit counts every
             * byte held, however many lines the event has. */
            if ((parser->has_data && !gw_buffer_append(&parser->data, "\n", 1)) ||
                !gw_buffer_append(&parser->data, value, value_length)) {
                result = GW_SSE_NOMEM;
            }
            parser->has_data = true;
        }
    }
    gw_buffer_clear(&parser->line);
    return result;
}

gw_sse_result gw_sse_feed(gw_sse_parser *parser, const char *chunk, size_t length, gw_sse_event_fn on_event,
                          void *ctx) {
    size_t start = 0;
    if (parser->skip_lf && length > 0) {
        if (chunk[0] == '\n') {
            start = 1;
        }
        parser->skip_lf = false;
    }
    for (size_t index = start; index < length; index++) {
        char byte = chunk[index];
        if (byte != '\n' && byte != '\r') {
            continue;
        }
        gw_sse_result result = append_line(parser, chunk + start, index - start);
        if (result == GW_SSE_OK) {
            result = dispatch(parser, on_event, ctx);
        }
        if (result != GW_SSE_OK) {
            return result;
        }
        if (byte == '\r') {
            if (index + 1 < length) {
                if (chunk[index + 1] == '\n') {
                    index++;
                }
            } else {
                parser->skip_lf = true;
            }
        }
        start = index + 1;
    }
    return append_line(parser, chunk + start, length - start);
}

void gw_sse_free(gw_sse_parser *parser) {
    gw_buffer_free(&parser->line);
    gw_buffer_free(&parser->data);
    gw_buffer_free(&parser->name);
}
