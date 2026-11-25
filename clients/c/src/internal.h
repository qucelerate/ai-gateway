/* Internal declarations shared by the library's sources. Not installed. */
#ifndef GATEWAY_INTERNAL_H
#define GATEWAY_INTERNAL_H

#include "gateway/gateway.h"
#include "json.h"

#include <stdarg.h>

/* ---- allocation (gw_global_init's allocator) */
void *gw_malloc(size_t size);
void *gw_calloc(size_t count, size_t size);
void *gw_realloc(void *ptr, size_t size);
void gw_free(void *ptr);
char *gw_strdup(const char *text);
char *gw_strndup(const char *text, size_t length);
const yyjson_alc *gw_json_allocator(void);
bool gw_initialized(void);

/* ---- a growable byte buffer, always NUL-terminated */
typedef struct gw_buffer {
    char *data;
    size_t length;
    size_t capacity;
} gw_buffer;

bool gw_buffer_append(gw_buffer *buffer, const void *bytes, size_t count);
void gw_buffer_clear(gw_buffer *buffer);
void gw_buffer_free(gw_buffer *buffer);

/* ---- errors */
gw_status gw_fail(gw_error **error, gw_status status, const char *format, ...);
gw_error *gw_error_make(gw_status status, const char *message);
void gw_error_set_http(gw_error *error, int http_status, const char *code, const char *title, const char *detail,
                       const char *request_id, int64_t retry_after);

/* ---- SSE parsing (sse.c) */
#define GW_MAX_EVENT_BYTES ((size_t)16 << 20)

typedef struct gw_sse_parser {
    gw_buffer line;
    gw_buffer data;
    gw_buffer name;
    bool has_data;
    bool skip_lf;
} gw_sse_parser;

/** Called for each completed event; return false to report an allocation failure. */
typedef bool (*gw_sse_event_fn)(void *ctx, const char *name, const char *data, size_t data_length);

typedef enum gw_sse_result { GW_SSE_OK = 0, GW_SSE_OVERSIZED = 1, GW_SSE_NOMEM = 2 } gw_sse_result;

/** Feed bytes; events before an oversized one are still delivered. */
gw_sse_result gw_sse_feed(gw_sse_parser *parser, const char *chunk, size_t length, gw_sse_event_fn on_event,
                          void *ctx);
void gw_sse_free(gw_sse_parser *parser);

/* ---- usage views (decode.c) */
struct gw_usage {
    bool present[5];
    int64_t counts[5];
    const char *quality;
    const char *completeness;
};

/** Validate and read a usage object; false if it does not match the contract. */
bool gw_usage_read(yyjson_val *value, gw_usage *usage);

/* ---- requests other than the JSON POSTs (client.c) */
typedef struct gw_http_request {
    bool get;                 /* GET, else POST */
    const char *path;         /* with its query */
    const char *body;         /* POST only; may be NULL for an empty body */
    size_t body_length;
    const char *content_type; /* NULL: none sent */
    const char *accept;       /* NULL: application/json */
    bool idempotent;          /* send an Idempotency-Key */
    size_t limit;             /* the largest successful body; 0 for 64 MiB */
} gw_http_request;

typedef struct gw_http_response {
    gw_buffer body;
    char *request_id;
    char *content_type;
} gw_http_response;

/** Send `request` with the client's retries; on success `out` holds the response, freed with gw_http_response_free. */
gw_status gw_http_send(gw_client *client, const gw_http_request *request, const gw_call_options *options,
                       gw_http_response *out, gw_error **error);
void gw_http_response_free(gw_http_response *response);
/** `bytes` random bytes (at most 32) as lowercase hex into `out` (2 * bytes + 1). */
bool gw_random_hex(char *out, size_t bytes);

#endif
