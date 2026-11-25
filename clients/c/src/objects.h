/* The layouts of the option and request objects. Not installed. */
#ifndef GATEWAY_OBJECTS_H
#define GATEWAY_OBJECTS_H

#include "internal.h"

struct gw_client_options {
    char *base_url;
    char *api_key;
    int max_retries;
    int64_t timeout_ms;
    char *ca_file;
    char *ca_path;
    gw_sleep_fn sleep;
    void *sleep_ctx;
};

void gw_client_options_clear(gw_client_options *options);

struct gw_call_options {
    char *idempotency_key;
    char *request_id;
    gw_cancel *cancel;
};

struct gw_chat_request {
    yyjson_mut_doc *doc;
    yyjson_mut_val *messages;
};

struct gw_embedding_request {
    yyjson_mut_doc *doc;
    yyjson_mut_val *inputs;
};

#endif
