/*
 * Decoding responses and stream events. Every shape the accessors rely on is
 * checked here, so a body that does not match the contract fails the call
 * with GW_E_DECODE instead of surfacing later. Unknown fields are ignored.
 */
#include "decode.h"

#include <stdio.h>
#include <string.h>

typedef struct check {
    char *message;
    size_t size;
    bool failed;
} check;

static bool fail(check *c, const char *what) {
    if (!c->failed) {
        snprintf(c->message, c->size, "the response does not match the contract: %s", what);
        c->failed = true;
    }
    return false;
}

/** An optional string: missing or null is NULL. */
static bool opt_str(check *c, yyjson_val *object, const char *key, const char **out) {
    yyjson_val *value = yyjson_obj_get(object, key);
    *out = NULL;
    if (!value || yyjson_is_null(value)) {
        return true;
    }
    if (!yyjson_is_str(value)) {
        return fail(c, key);
    }
    *out = yyjson_get_str(value);
    return true;
}

/** An optional string with its length, which may include NUL bytes. */
static bool opt_text(check *c, yyjson_val *object, const char *key, const char **out, size_t *length) {
    if (!opt_str(c, object, key, out)) {
        return false;
    }
    *length = *out ? yyjson_get_len(yyjson_obj_get(object, key)) : 0;
    return true;
}

/** A required, non-null string. */
static bool req_str(check *c, yyjson_val *object, const char *key, const char **out) {
    return opt_str(c, object, key, out) && (*out || fail(c, key));
}

/** A required, non-null string with its length. */
static bool req_text(check *c, yyjson_val *object, const char *key, const char **out, size_t *length) {
    return opt_text(c, object, key, out, length) && (*out || fail(c, key));
}

/** A key that must be present, though it may be null. */
static bool has_key(check *c, yyjson_val *object, const char *key) {
    return yyjson_obj_get(object, key) || fail(c, key);
}

/** An optional integer that fits int64. */
static bool opt_int(check *c, yyjson_val *object, const char *key, bool *present, int64_t *out) {
    yyjson_val *value = yyjson_obj_get(object, key);
    *present = false;
    if (!value || yyjson_is_null(value)) {
        return true;
    }
    if (yyjson_is_sint(value)) {
        *out = yyjson_get_sint(value);
    } else if (yyjson_is_uint(value) && yyjson_get_uint(value) <= (uint64_t)INT64_MAX) {
        *out = (int64_t)yyjson_get_uint(value);
    } else {
        return fail(c, key);
    }
    *present = true;
    return true;
}

static bool req_int(check *c, yyjson_val *object, const char *key, int64_t *out) {
    bool present;
    if (!opt_int(c, object, key, &present, out)) {
        return false;
    }
    return present || fail(c, key);
}

static const char *const counter_keys[5] = {"input_tokens", "output_tokens", "cached_input_tokens",
                                            "reasoning_tokens", "other_tokens"};

static bool read_usage(check *c, yyjson_val *value, gw_usage *usage) {
    memset(usage, 0, sizeof *usage);
    if (!yyjson_is_obj(value)) {
        return fail(c, "usage");
    }
    for (int i = 0; i < 5; i++) {
        if (!opt_int(c, value, counter_keys[i], &usage->present[i], &usage->counts[i])) {
            return false;
        }
    }
    return opt_str(c, value, "quality", &usage->quality) && opt_str(c, value, "completeness", &usage->completeness);
}

bool gw_usage_read(yyjson_val *value, gw_usage *usage) {
    char message[64];
    check c = {message, sizeof message, false};
    return read_usage(&c, value, usage);
}

/** An optional usage member: missing or null is none. */
static bool opt_usage(check *c, yyjson_val *object, bool *present, gw_usage *usage) {
    yyjson_val *value = yyjson_obj_get(object, "usage");
    *present = false;
    if (!value || yyjson_is_null(value)) {
        return true;
    }
    *present = true;
    return read_usage(c, value, usage);
}

/** An optional array member: missing or null is empty. */
static bool opt_arr(check *c, yyjson_val *object, const char *key, yyjson_val **out) {
    yyjson_val *value = yyjson_obj_get(object, key);
    *out = NULL;
    if (!value || yyjson_is_null(value)) {
        return true;
    }
    if (!yyjson_is_arr(value)) {
        return fail(c, key);
    }
    *out = value;
    return true;
}

static yyjson_doc *parse(const char *body, size_t length, check *c) {
    yyjson_read_err error;
    yyjson_doc *doc = yyjson_read_opts((char *)body, length, YYJSON_READ_NOFLAG, gw_json_allocator(), &error);
    if (!doc) {
        snprintf(c->message, c->size, "the response is not JSON: %s at %zu", error.msg, error.pos);
        c->failed = true;
    }
    return doc;
}

/* ------------------------------------------------------------------ chat */

/** Every call has a string id, and a function with a string name and arguments. */
static bool valid_tool_calls(check *c, yyjson_val *calls) {
    size_t i, n;
    yyjson_val *call;
    yyjson_arr_foreach(calls, i, n, call) {
        yyjson_val *function = yyjson_obj_get(call, "function");
        if (!yyjson_is_str(yyjson_obj_get(call, "id")) || !yyjson_is_str(yyjson_obj_get(function, "name")) ||
            !yyjson_is_str(yyjson_obj_get(function, "arguments"))) {
            return fail(c, "tool_calls");
        }
    }
    return true;
}

typedef struct gw_choice {
    int64_t index;
    const char *role;
    const char *content;
    size_t content_length;
    const char *finish_reason;
    const char *refusal;
    size_t refusal_length;
    yyjson_val *tool_calls;
} gw_choice;

struct gw_chat_response {
    yyjson_doc *doc;
    char *body;
    size_t body_length;
    char *request_id;
    const char *id;
    const char *model;
    size_t choice_count;
    gw_choice *choices;
    bool has_usage;
    gw_usage usage;
};

GW_API void gw_chat_response_free(gw_chat_response *response) {
    if (response) {
        yyjson_doc_free(response->doc);
        gw_free(response->body);
        gw_free(response->request_id);
        gw_free(response->choices);
        gw_free(response);
    }
}

static gw_status nomem(gw_status *status) {
    *status = GW_E_NOMEM;
    return GW_E_NOMEM;
}

gw_chat_response *gw_chat_response_parse(const char *body, size_t length, const char *request_id,
                                         gw_status *status, char *message, size_t message_size) {
    check c = {message, message_size, false};
    gw_chat_response *response = gw_calloc(1, sizeof *response);
    if (!response) {
        nomem(status);
        return NULL;
    }
    if (!(response->body = gw_strndup(body, length)) || (request_id && !(response->request_id = gw_strdup(request_id)))) {
        gw_chat_response_free(response);
        nomem(status);
        return NULL;
    }
    response->body_length = length;
    response->doc = parse(body, length, &c);
    yyjson_val *root = response->doc ? yyjson_doc_get_root(response->doc) : NULL;
    yyjson_val *choices = NULL;
    if (root && !yyjson_is_obj(root)) {
        fail(&c, "the body is not an object");
    } else if (root && opt_str(&c, root, "id", &response->id) && opt_str(&c, root, "model", &response->model) &&
               opt_arr(&c, root, "choices", &choices) && opt_usage(&c, root, &response->has_usage, &response->usage) &&
               (response->id && response->model && choices ? true : fail(&c, "id, model and choices are required"))) {
        response->choice_count = choices ? yyjson_arr_size(choices) : 0;
        if (response->choice_count &&
            !(response->choices = gw_calloc(response->choice_count, sizeof *response->choices))) {
            gw_chat_response_free(response);
            nomem(status);
            return NULL;
        }
        for (size_t i = 0; i < response->choice_count && !c.failed; i++) {
            yyjson_val *choice = yyjson_arr_get(choices, i);
            gw_choice *out = &response->choices[i];
            bool present;
            out->index = (int64_t)i;
            if (!yyjson_is_obj(choice)) {
                fail(&c, "choices");
                break;
            }
            yyjson_val *message_value = yyjson_obj_get(choice, "message");
            if (!yyjson_is_obj(message_value)) {
                fail(&c, "a choice's message is required");
                break;
            }
            if (!opt_str(&c, message_value, "role", &out->role) ||
                !opt_text(&c, message_value, "content", &out->content, &out->content_length)) {
                break;
            }
            if (!out->role) {
                fail(&c, "a message's role is required");
                break;
            }
            if (!opt_arr(&c, message_value, "tool_calls", &out->tool_calls) || !valid_tool_calls(&c, out->tool_calls)) {
                break;
            }
            if (!opt_int(&c, choice, "index", &present, &out->index) ||
                !opt_str(&c, choice, "finish_reason", &out->finish_reason) ||
                !opt_text(&c, choice, "refusal", &out->refusal, &out->refusal_length)) {
                break;
            }
            if (!present) {
                out->index = (int64_t)i;
            }
        }
    }
    if (c.failed) {
        gw_chat_response_free(response);
        *status = GW_E_DECODE;
        return NULL;
    }
    *status = GW_OK;
    return response;
}

static const gw_choice *choice_at(const gw_chat_response *response, size_t choice) {
    return response && choice < response->choice_count ? &response->choices[choice] : NULL;
}

GW_API const char *gw_chat_response_id(const gw_chat_response *r) { return r ? r->id : NULL; }
GW_API const char *gw_chat_response_model(const gw_chat_response *r) { return r ? r->model : NULL; }
GW_API const char *gw_chat_response_request_id(const gw_chat_response *r) { return r ? r->request_id : NULL; }
GW_API size_t gw_chat_response_choice_count(const gw_chat_response *r) { return r ? r->choice_count : 0; }

GW_API int64_t gw_chat_response_choice_index(const gw_chat_response *r, size_t choice) {
    const gw_choice *c = choice_at(r, choice);
    return c ? c->index : -1;
}

GW_API const char *gw_chat_response_choice_role(const gw_chat_response *r, size_t choice) {
    const gw_choice *c = choice_at(r, choice);
    return c ? c->role : NULL;
}

GW_API const char *gw_chat_response_choice_content(const gw_chat_response *r, size_t choice, size_t *length) {
    const gw_choice *c = choice_at(r, choice);
    if (length) {
        *length = c ? c->content_length : 0;
    }
    return c ? c->content : NULL;
}

GW_API const char *gw_chat_response_choice_finish_reason(const gw_chat_response *r, size_t choice) {
    const gw_choice *c = choice_at(r, choice);
    return c ? c->finish_reason : NULL;
}

GW_API const char *gw_chat_response_choice_refusal(const gw_chat_response *r, size_t choice, size_t *length) {
    const gw_choice *c = choice_at(r, choice);
    if (length) {
        *length = c ? c->refusal_length : 0;
    }
    return c ? c->refusal : NULL;
}

GW_API size_t gw_chat_response_choice_tool_call_count(const gw_chat_response *r, size_t choice) {
    const gw_choice *c = choice_at(r, choice);
    return c && c->tool_calls ? yyjson_arr_size(c->tool_calls) : 0;
}

GW_API bool gw_chat_response_choice_tool_call(const gw_chat_response *r, size_t choice, size_t index,
                                              const char **id, const char **name, const char **arguments) {
    const gw_choice *c = choice_at(r, choice);
    yyjson_val *call = c && c->tool_calls ? yyjson_arr_get(c->tool_calls, index) : NULL;
    if (!call) {
        return false;
    }
    yyjson_val *function = yyjson_obj_get(call, "function");
    if (id) {
        *id = yyjson_get_str(yyjson_obj_get(call, "id"));
    }
    if (name) {
        *name = yyjson_get_str(yyjson_obj_get(function, "name"));
    }
    if (arguments) {
        *arguments = yyjson_get_str(yyjson_obj_get(function, "arguments"));
    }
    return true;
}

GW_API const gw_usage *gw_chat_response_usage(const gw_chat_response *r) {
    return r && r->has_usage ? &r->usage : NULL;
}

GW_API const char *gw_chat_response_json(const gw_chat_response *r, size_t *length) {
    if (length) {
        *length = r ? r->body_length : 0;
    }
    return r ? r->body : NULL;
}

/* ------------------------------------------------------------ embeddings */

typedef struct gw_embedding {
    int64_t index;
    float *vector;
    size_t dimensions;
} gw_embedding;

struct gw_embedding_response {
    yyjson_doc *doc;
    char *request_id;
    const char *model;
    size_t count;
    gw_embedding *items;
    bool has_usage;
    gw_usage usage;
};

GW_API void gw_embedding_response_free(gw_embedding_response *response) {
    if (response) {
        for (size_t i = 0; i < response->count && response->items; i++) {
            gw_free(response->items[i].vector);
        }
        gw_free(response->items);
        gw_free(response->request_id);
        yyjson_doc_free(response->doc);
        gw_free(response);
    }
}

gw_embedding_response *gw_embedding_response_parse(const char *body, size_t length, const char *request_id,
                                                   gw_status *status, char *message, size_t message_size) {
    check c = {message, message_size, false};
    gw_embedding_response *response = gw_calloc(1, sizeof *response);
    if (!response || (request_id && !(response->request_id = gw_strdup(request_id)))) {
        gw_embedding_response_free(response);
        nomem(status);
        return NULL;
    }
    response->doc = parse(body, length, &c);
    yyjson_val *root = response->doc ? yyjson_doc_get_root(response->doc) : NULL;
    yyjson_val *data = NULL;
    if (root && !yyjson_is_obj(root)) {
        fail(&c, "the body is not an object");
    } else if (root && opt_str(&c, root, "model", &response->model) && opt_arr(&c, root, "data", &data) &&
               opt_usage(&c, root, &response->has_usage, &response->usage) &&
               (response->model && data ? true : fail(&c, "model and data are required"))) {
        size_t count = data ? yyjson_arr_size(data) : 0;
        if (count && !(response->items = gw_calloc(count, sizeof *response->items))) {
            gw_embedding_response_free(response);
            nomem(status);
            return NULL;
        }
        response->count = count;
        for (size_t i = 0; i < count && !c.failed; i++) {
            yyjson_val *item = yyjson_arr_get(data, i);
            gw_embedding *out = &response->items[i];
            bool present;
            if (!yyjson_is_obj(item)) {
                fail(&c, "data");
                break;
            }
            if (!opt_int(&c, item, "index", &present, &out->index)) {
                break;
            }
            if (!present) {
                out->index = (int64_t)i;
            }
            yyjson_val *vector = yyjson_obj_get(item, "embedding");
            if (!yyjson_is_arr(vector)) {
                fail(&c, "embedding");
                break;
            }
            out->dimensions = yyjson_arr_size(vector);
            if (out->dimensions && !(out->vector = gw_calloc(out->dimensions, sizeof(float)))) {
                gw_embedding_response_free(response);
                nomem(status);
                return NULL;
            }
            size_t at, max;
            yyjson_val *number;
            yyjson_arr_foreach(vector, at, max, number) {
                if (!yyjson_is_num(number)) {
                    fail(&c, "embedding");
                    break;
                }
                out->vector[at] = (float)yyjson_get_num(number);
            }
        }
    }
    if (c.failed) {
        gw_embedding_response_free(response);
        *status = GW_E_DECODE;
        return NULL;
    }
    *status = GW_OK;
    return response;
}

GW_API const char *gw_embedding_response_model(const gw_embedding_response *r) { return r ? r->model : NULL; }

GW_API const char *gw_embedding_response_request_id(const gw_embedding_response *r) {
    return r ? r->request_id : NULL;
}

GW_API size_t gw_embedding_response_count(const gw_embedding_response *r) { return r ? r->count : 0; }

GW_API int64_t gw_embedding_response_index(const gw_embedding_response *r, size_t item) {
    return r && item < r->count ? r->items[item].index : -1;
}

GW_API const float *gw_embedding_response_vector(const gw_embedding_response *r, size_t item, size_t *dimensions) {
    if (!r || item >= r->count) {
        if (dimensions) {
            *dimensions = 0;
        }
        return NULL;
    }
    if (dimensions) {
        *dimensions = r->items[item].dimensions;
    }
    return r->items[item].vector;
}

GW_API const gw_usage *gw_embedding_response_usage(const gw_embedding_response *r) {
    return r && r->has_usage ? &r->usage : NULL;
}

/* ----------------------------------------------------------------- usage */

GW_API bool gw_usage_get(const gw_usage *usage, gw_usage_counter counter, int64_t *out) {
    if (!usage || (int)counter < 0 || (int)counter >= 5 || !usage->present[counter]) {
        return false;
    }
    if (out) {
        *out = usage->counts[counter];
    }
    return true;
}

GW_API const char *gw_usage_quality(const gw_usage *usage) { return usage ? usage->quality : NULL; }
GW_API const char *gw_usage_completeness(const gw_usage *usage) { return usage ? usage->completeness : NULL; }

/* ---------------------------------------------------------------- events */

static const struct {
    const char *name;
    gw_event_type type;
} known_events[] = {
    {"response.started", GW_EVENT_RESPONSE_STARTED},   {"choice.delta", GW_EVENT_CHOICE_DELTA},
    {"tool_call.delta", GW_EVENT_TOOL_CALL_DELTA},     {"choice.finished", GW_EVENT_CHOICE_FINISHED},
    {"safety.annotation", GW_EVENT_SAFETY_ANNOTATION}, {"usage", GW_EVENT_USAGE},
    {"response.completed", GW_EVENT_RESPONSE_COMPLETED},
};

bool gw_event_known(const char *name, gw_event_type *type) {
    for (size_t i = 0; i < sizeof known_events / sizeof known_events[0]; i++) {
        if (strcmp(name, known_events[i].name) == 0) {
            *type = known_events[i].type;
            return true;
        }
    }
    return false;
}

void gw_event_free(gw_event *event) {
    if (event) {
        yyjson_doc_free(event->doc);
        gw_free(event->name);
        gw_free(event->data);
        gw_free(event);
    }
}

gw_status gw_event_decode(gw_event_type type, const char *name, const char *data, size_t length, gw_event **out,
                          char *message, size_t message_size) {
    check c = {message, message_size, false};
    gw_event *event = gw_calloc(1, sizeof *event);
    if (!event || !(event->name = gw_strdup(name)) || !(event->data = gw_strndup(data, length))) {
        gw_event_free(event);
        return GW_E_NOMEM;
    }
    event->type = type;
    event->data_length = length;
    event->choice_index = -1;
    event->tool_call_index = -1;
    event->doc = parse(data, length, &c);
    yyjson_val *root = event->doc ? yyjson_doc_get_root(event->doc) : NULL;
    if (root && !yyjson_is_obj(root)) {
        fail(&c, "the event is not an object");
    } else if (root) {
        switch (type) {
        case GW_EVENT_RESPONSE_STARTED:
            req_str(&c, root, "model", &event->model);
            break;
        case GW_EVENT_CHOICE_DELTA:
            if (req_int(&c, root, "choice_index", &event->choice_index) &&
                opt_text(&c, root, "content_delta", &event->content_delta, &event->content_delta_length)) {
                opt_text(&c, root, "refusal_delta", &event->refusal_delta, &event->refusal_delta_length);
            }
            break;
        case GW_EVENT_TOOL_CALL_DELTA:
            if (req_int(&c, root, "choice_index", &event->choice_index) &&
                req_int(&c, root, "tool_call_index", &event->tool_call_index) &&
                opt_str(&c, root, "id", &event->tool_call_id) && opt_str(&c, root, "name", &event->tool_name)) {
                req_text(&c, root, "arguments_delta", &event->arguments_delta, &event->arguments_delta_length);
            }
            break;
        case GW_EVENT_CHOICE_FINISHED:
            if (req_int(&c, root, "choice_index", &event->choice_index)) {
                req_str(&c, root, "finish_reason", &event->finish_reason);
            }
            break;
        case GW_EVENT_SAFETY_ANNOTATION: {
            if (req_int(&c, root, "choice_index", &event->choice_index) &&
                req_str(&c, root, "category", &event->safety_category)) {
                yyjson_val *blocked = yyjson_obj_get(root, "blocked");
                if (!yyjson_is_bool(blocked)) {
                    fail(&c, "blocked");
                } else {
                    event->safety_blocked = yyjson_get_bool(blocked);
                }
            }
            break;
        }
        case GW_EVENT_USAGE:
            event->has_usage = true;
            read_usage(&c, root, &event->usage);
            break;
        case GW_EVENT_RESPONSE_COMPLETED:
            if (req_str(&c, root, "model", &event->model) && has_key(&c, root, "usage")) {
                opt_usage(&c, root, &event->has_usage, &event->usage);
            }
            break;
        }
    }
    if (c.failed) {
        gw_event_free(event);
        return GW_E_DECODE;
    }
    *out = event;
    return GW_OK;
}

bool gw_stream_error_decode(const char *data, size_t length, char **code, int *status) {
    yyjson_doc *doc = yyjson_read_opts((char *)data, length, YYJSON_READ_NOFLAG, gw_json_allocator(), NULL);
    yyjson_val *root = doc ? yyjson_doc_get_root(doc) : NULL;
    yyjson_val *code_value = root ? yyjson_obj_get(root, "code") : NULL;
    yyjson_val *status_value = root ? yyjson_obj_get(root, "status") : NULL;
    bool ok = yyjson_is_str(code_value) && yyjson_is_int(status_value) && yyjson_get_sint(status_value) >= 0 &&
              yyjson_get_sint(status_value) <= 999;
    if (ok) {
        *code = gw_strdup(yyjson_get_str(code_value));
        *status = (int)yyjson_get_sint(status_value);
        ok = *code != NULL;
    }
    yyjson_doc_free(doc);
    return ok;
}

GW_API gw_event_type gw_event_get_type(const gw_event *e) { return e ? e->type : (gw_event_type)0; }
GW_API const char *gw_event_name(const gw_event *e) { return e ? e->name : NULL; }

GW_API const char *gw_event_json(const gw_event *e, size_t *length) {
    if (length) {
        *length = e ? e->data_length : 0;
    }
    return e ? e->data : NULL;
}

GW_API const char *gw_event_model(const gw_event *e) { return e ? e->model : NULL; }
GW_API int64_t gw_event_choice_index(const gw_event *e) { return e ? e->choice_index : -1; }
GW_API const char *gw_event_content_delta(const gw_event *e, size_t *length) {
    if (length) {
        *length = e ? e->content_delta_length : 0;
    }
    return e ? e->content_delta : NULL;
}

GW_API const char *gw_event_refusal_delta(const gw_event *e, size_t *length) {
    if (length) {
        *length = e ? e->refusal_delta_length : 0;
    }
    return e ? e->refusal_delta : NULL;
}
GW_API int64_t gw_event_tool_call_index(const gw_event *e) { return e ? e->tool_call_index : -1; }
GW_API const char *gw_event_tool_call_id(const gw_event *e) { return e ? e->tool_call_id : NULL; }
GW_API const char *gw_event_tool_name(const gw_event *e) { return e ? e->tool_name : NULL; }
GW_API const char *gw_event_arguments_delta(const gw_event *e, size_t *length) {
    if (length) {
        *length = e ? e->arguments_delta_length : 0;
    }
    return e ? e->arguments_delta : NULL;
}
GW_API const char *gw_event_finish_reason(const gw_event *e) { return e ? e->finish_reason : NULL; }
GW_API const char *gw_event_safety_category(const gw_event *e) { return e ? e->safety_category : NULL; }
GW_API bool gw_event_safety_blocked(const gw_event *e) { return e && e->safety_blocked; }
GW_API const gw_usage *gw_event_usage(const gw_event *e) { return e && e->has_usage ? &e->usage : NULL; }
