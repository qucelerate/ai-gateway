/* Response and event decoding. Not installed. */
#ifndef GATEWAY_DECODE_H
#define GATEWAY_DECODE_H

#include "internal.h"

/** Parse a chat body; takes ownership of nothing. On failure returns NULL and sets *status. */
gw_chat_response *gw_chat_response_parse(const char *body, size_t length, const char *request_id,
                                         gw_status *status, char *message, size_t message_size);
gw_embedding_response *gw_embedding_response_parse(const char *body, size_t length, const char *request_id,
                                                   gw_status *status, char *message, size_t message_size);

struct gw_event {
    gw_event_type type;
    char *name;
    char *data;
    size_t data_length;
    yyjson_doc *doc;
    const char *model;
    int64_t choice_index;
    const char *content_delta;
    size_t content_delta_length;
    const char *refusal_delta;
    size_t refusal_delta_length;
    int64_t tool_call_index;
    const char *tool_call_id;
    const char *tool_name;
    const char *arguments_delta;
    size_t arguments_delta_length;
    const char *finish_reason;
    const char *safety_category;
    bool safety_blocked;
    bool has_usage;
    gw_usage usage;
};

/** The SSE event names this version knows. */
bool gw_event_known(const char *name, gw_event_type *type);

/**
 * Decode a known event into a new gw_event. Returns GW_OK, GW_E_DECODE (with
 * `message` filled) or GW_E_NOMEM.
 */
gw_status gw_event_decode(gw_event_type type, const char *name, const char *data, size_t length, gw_event **out,
                          char *message, size_t message_size);
void gw_event_free(gw_event *event);

/** Decode a terminal error event's code and status; false if malformed. */
bool gw_stream_error_decode(const char *data, size_t length, char **code, int *status);

#endif
