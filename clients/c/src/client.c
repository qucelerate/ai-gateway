/*
 * The transport: libcurl's multi interface drives every call, so the client
 * checks its cancellation flag and deadline between short polls, both for
 * blocking calls and for streams read one event at a time.
 */
#if !defined(_WIN32)
#  define _POSIX_C_SOURCE 200809L
#  define _DEFAULT_SOURCE 1
#endif

#include "decode.h"
#include "objects.h"

#include <ctype.h>
#include <curl/curl.h>
#include <stdio.h>
#include <string.h>

#if defined(_WIN32)
#  include <windows.h>
#  include <bcrypt.h>
#else
#  include <sys/random.h>
#  include <time.h>
#endif

#define CHAT_PATH "/api/inference/v1/chat/completions"
#define STREAM_PATH "/api/inference/v1/chat/completions/stream"
#define EMBEDDINGS_PATH "/api/inference/v1/embeddings"
#define MAX_WAIT_SECONDS 60
#define MAX_KEY_CHARS 255
#define MAX_REQUEST_ID_CHARS 128
#define MAX_ERROR_BODY_BYTES ((size_t)1 << 20)
#define MAX_RESPONSE_BYTES ((size_t)64 << 20)
#define POLL_MS 50

static const char *const same_key_retry_codes[] = {"rate_limit_exceeded", "concurrency_limit_exceeded",
                                                   "provider_circuit_open"};

struct gw_client {
    char *base_url;
    char *api_key;
    char *ca_file;
    char *ca_path;
    int max_retries;
    int64_t timeout_ms;
    gw_sleep_fn sleep;
    void *sleep_ctx;
};

/* ------------------------------------------------------------ utilities */

static int64_t now_ms(void) {
#if defined(_WIN32)
    return (int64_t)GetTickCount64();
#else
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000;
#endif
}

static void nap_ms(int64_t milliseconds) {
#if defined(_WIN32)
    Sleep((DWORD)milliseconds);
#else
    struct timespec pause = {(time_t)(milliseconds / 1000), (long)(milliseconds % 1000) * 1000000L};
    while (nanosleep(&pause, &pause) != 0) {
    }
#endif
}

/** The default wait before a retry: short naps that notice cancellation. */
static bool default_sleep(void *ctx, int64_t milliseconds, const gw_cancel *cancel) {
    (void)ctx;
    int64_t deadline = now_ms() + milliseconds;
    for (;;) {
        if (gw_cancel_triggered(cancel)) {
            return false;
        }
        int64_t left = deadline - now_ms();
        if (left <= 0) {
            return true;
        }
        nap_ms(left < POLL_MS ? left : POLL_MS);
    }
}

static bool random_bytes(unsigned char *out, size_t count) {
#if defined(_WIN32)
    return BCRYPT_SUCCESS(BCryptGenRandom(NULL, out, (ULONG)count, BCRYPT_USE_SYSTEM_PREFERRED_RNG));
#else
    return getentropy(out, count) == 0;
#endif
}

/** A random (version 4) UUID. */
static bool new_key(char out[37]) {
    unsigned char raw[16];
    if (!random_bytes(raw, sizeof raw)) {
        return false;
    }
    raw[6] = (unsigned char)((raw[6] & 0x0f) | 0x40);
    raw[8] = (unsigned char)((raw[8] & 0x3f) | 0x80);
    static const char hex[] = "0123456789abcdef";
    size_t at = 0;
    for (int i = 0; i < 16; i++) {
        if (i == 4 || i == 6 || i == 8 || i == 10) {
            out[at++] = '-';
        }
        out[at++] = hex[raw[i] >> 4];
        out[at++] = hex[raw[i] & 15];
    }
    out[at] = '\0';
    return true;
}

/** UTF-8 code points in a string. */
static size_t chars(const char *text) {
    size_t count = 0;
    for (; *text; text++) {
        count += ((unsigned char)*text & 0xc0) != 0x80;
    }
    return count;
}

/** A header value may not contain CR, LF or NUL (header injection). */
/* No control characters but tab, so a value cannot end its header line or smuggle another. */
static bool header_safe(const char *value) {
    for (const unsigned char *c = (const unsigned char *)value; *c; c++) {
        if ((*c < 0x20 && *c != '\t') || *c == 0x7f) {
            return false;
        }
    }
    return true;
}

static bool starts_with_ci(const char *text, size_t length, const char *prefix) {
    size_t n = strlen(prefix);
    if (length < n) {
        return false;
    }
    for (size_t i = 0; i < n; i++) {
        if (tolower((unsigned char)text[i]) != prefix[i]) {
            return false;
        }
    }
    return true;
}

/* --------------------------------------------------------------- client */

GW_API gw_status gw_client_new(const gw_client_options *options, gw_client **out, gw_error **error) {
    if (error) {
        *error = NULL;
    }
    if (!out) {
        return gw_fail(error, GW_E_CONFIG, "out is NULL");
    }
    *out = NULL;
    if (!gw_initialized()) {
        return gw_fail(error, GW_E_CONFIG, "call gw_global_init first");
    }
    if (!options) {
        return gw_fail(error, GW_E_CONFIG, "options are NULL");
    }
    const char *url = options->base_url;
    const char *host = NULL;
    if (url && strncmp(url, "https://", 8) == 0) {
        host = url + 8;
    } else if (url && strncmp(url, "http://", 7) == 0) {
        host = url + 7;
    }
    if (!host || !*host || *host == '/' || !header_safe(url) || strpbrk(url, " \t")) {
        return gw_fail(error, GW_E_CONFIG, "the base URL must be http or https");
    }
    if (!options->api_key || !*options->api_key || !header_safe(options->api_key) ||
        strspn(options->api_key, " \t") == strlen(options->api_key)) {
        return gw_fail(error, GW_E_CONFIG, "an API key is required");
    }
    gw_client *client = gw_calloc(1, sizeof *client);
    if (!client) {
        return gw_fail(error, GW_E_NOMEM, "out of memory");
    }
    client->base_url = gw_strdup(url);
    client->api_key = gw_strdup(options->api_key);
    client->ca_file = gw_strdup(options->ca_file);
    client->ca_path = gw_strdup(options->ca_path);
    client->max_retries = options->max_retries;
    client->timeout_ms = options->timeout_ms;
    client->sleep = options->sleep ? options->sleep : default_sleep;
    client->sleep_ctx = options->sleep_ctx;
    if (!client->base_url || !client->api_key || (options->ca_file && !client->ca_file) ||
        (options->ca_path && !client->ca_path)) {
        gw_client_free(client);
        return gw_fail(error, GW_E_NOMEM, "out of memory");
    }
    size_t length = strlen(client->base_url);
    while (length > 0 && client->base_url[length - 1] == '/') {
        client->base_url[--length] = '\0';
    }
    *out = client;
    return GW_OK;
}

GW_API void gw_client_free(gw_client *client) {
    if (client) {
        gw_free(client->base_url);
        if (client->api_key) {
            memset(client->api_key, 0, strlen(client->api_key));
        }
        gw_free(client->api_key);
        gw_free(client->ca_file);
        gw_free(client->ca_path);
        gw_free(client);
    }
}

/* ------------------------------------------------------------- transfer */

typedef struct transfer {
    CURL *easy;
    CURLM *multi;
    struct curl_slist *headers;
    char *url;
    const char *body;
    size_t body_length;
    bool get;
    size_t success_limit;
    bool stream;
    const gw_cancel *cancel;
    int64_t deadline;

    /* The final response's head. */
    long status;
    bool headers_done;
    char *request_id;
    char *content_type;
    int64_t retry_after;

    /* A non-streaming or error body. */
    gw_buffer content;
    size_t limit;
    bool exceeded;

    /* A successful stream's events. */
    gw_sse_parser parser;
    gw_event **queue;
    size_t queue_head;
    size_t queue_length;
    size_t queue_capacity;
    gw_status failure;
    char failure_message[256];
    char *failure_code;
    int failure_status;
    bool completed;

    bool finished;
    CURLcode result;
} transfer;

static void transfer_reset_head(transfer *t) {
    t->status = 0;
    t->headers_done = false;
    gw_free(t->request_id);
    gw_free(t->content_type);
    t->request_id = t->content_type = NULL;
    t->retry_after = -1;
}

static void queue_clear(transfer *t) {
    for (size_t i = t->queue_head; i < t->queue_length; i++) {
        gw_event_free(t->queue[i]);
    }
    t->queue_head = t->queue_length = 0;
}

static void transfer_close(transfer *t) {
    if (t->multi && t->easy) {
        curl_multi_remove_handle(t->multi, t->easy);
    }
    if (t->easy) {
        curl_easy_cleanup(t->easy);
    }
    if (t->multi) {
        curl_multi_cleanup(t->multi);
    }
    t->easy = NULL;
    t->multi = NULL;
}

static void transfer_free(transfer *t) {
    transfer_close(t);
    curl_slist_free_all(t->headers);
    gw_free(t->url);
    transfer_reset_head(t);
    gw_buffer_free(&t->content);
    gw_sse_free(&t->parser);
    queue_clear(t);
    gw_free(t->queue);
    gw_free(t->failure_code);
}

static size_t on_header(char *data, size_t size, size_t count, void *user) {
    transfer *t = user;
    size_t length = size * count;
    if (starts_with_ci(data, length, "http/")) {
        transfer_reset_head(t); /* a new response head: 1xx or the final one */
        return length;
    }
    if (length <= 2 && (data[0] == '\r' || data[0] == '\n')) {
        long status = 0;
        curl_easy_getinfo(t->easy, CURLINFO_RESPONSE_CODE, &status);
        if (status >= 200) {
            t->status = status;
            t->headers_done = true;
            bool success = status >= 200 && status < 300;
            t->limit = success ? t->success_limit : MAX_ERROR_BODY_BYTES;
        }
        return length;
    }
    const char *colon = memchr(data, ':', length);
    if (!colon) {
        return length;
    }
    const char *value = colon + 1;
    const char *end = data + length;
    while (value < end && (*value == ' ' || *value == '\t')) {
        value++;
    }
    while (end > value && (end[-1] == '\r' || end[-1] == '\n' || end[-1] == ' ' || end[-1] == '\t')) {
        end--;
    }
    size_t value_length = (size_t)(end - value);
    size_t name_length = (size_t)(colon - data);
    char **slot = NULL;
    if (name_length == 12 && starts_with_ci(data, name_length, "x-request-id")) {
        slot = &t->request_id;
    } else if (name_length == 12 && starts_with_ci(data, name_length, "content-type")) {
        slot = &t->content_type;
    } else if (name_length == 11 && starts_with_ci(data, name_length, "retry-after")) {
        int64_t seconds = 0;
        bool digits = value_length > 0;
        for (size_t i = 0; i < value_length && digits; i++) {
            if (!isdigit((unsigned char)value[i])) {
                digits = false;
            } else if (seconds < INT64_MAX / 10 - 10) {
                seconds = seconds * 10 + (value[i] - '0');
            } else {
                seconds = INT64_MAX / 10; /* clamped: capped to 60 seconds anyway */
            }
        }
        t->retry_after = digits ? seconds : -1;
        return length;
    }
    if (slot) {
        gw_free(*slot);
        *slot = gw_strndup(value, value_length);
        if (!*slot) {
            return 0;
        }
    }
    return length;
}

static void stream_fail(transfer *t, gw_status status, const char *message) {
    if (t->failure == GW_OK) {
        t->failure = status;
        snprintf(t->failure_message, sizeof t->failure_message, "%.255s", message); /* truncated on purpose */
    }
}

/* Queued events are consumed from queue_head; consumed slots are reused, so a
 * long stream read promptly keeps a small queue. */
static bool queue_push(transfer *t, gw_event *event) {
    if (t->queue_head > 0 && t->queue_length == t->queue_capacity) {
        memmove(t->queue, t->queue + t->queue_head, (t->queue_length - t->queue_head) * sizeof *t->queue);
        t->queue_length -= t->queue_head;
        t->queue_head = 0;
    }
    if (t->queue_length == t->queue_capacity) {
        size_t capacity = t->queue_capacity ? t->queue_capacity * 2 : 8;
        gw_event **queue = gw_realloc(t->queue, capacity * sizeof *queue);
        if (!queue) {
            return false;
        }
        t->queue = queue;
        t->queue_capacity = capacity;
    }
    t->queue[t->queue_length++] = event;
    return true;
}

/** An SSE event of a successful stream. */
static bool on_event(void *user, const char *name, const char *data, size_t length) {
    transfer *t = user;
    if (t->completed || t->failure != GW_OK) {
        return true;
    }
    if (strcmp(name, "error") == 0) {
        char *code = NULL;
        int status = 0;
        if (gw_stream_error_decode(data, length, &code, &status)) {
            t->failure_code = code;
            t->failure_status = status;
            char message[160];
            snprintf(message, sizeof message, "stream failed: %.100s (status %d)", code, status);
            stream_fail(t, GW_E_STREAM, message);
        } else {
            stream_fail(t, GW_E_DECODE, "error event: code must be a string and status an integer");
        }
        return true;
    }
    gw_event_type type;
    if (!gw_event_known(name, &type)) {
        return true; /* skipped by name, before its data is parsed */
    }
    gw_event *event = NULL;
    char message[256];
    gw_status status = gw_event_decode(type, name, data, length, &event, message, sizeof message);
    if (status == GW_E_NOMEM) {
        return false;
    }
    if (status != GW_OK) {
        char described[320];
        snprintf(described, sizeof described, "%s: %s", name, message);
        stream_fail(t, GW_E_DECODE, described);
        return true;
    }
    if (!queue_push(t, event)) {
        gw_event_free(event);
        return false;
    }
    if (type == GW_EVENT_RESPONSE_COMPLETED) {
        t->completed = true;
    }
    return true;
}

static size_t on_body(char *data, size_t size, size_t count, void *user) {
    transfer *t = user;
    size_t length = size * count;
    bool success = t->status >= 200 && t->status < 300;
    if (t->stream && success) {
        if (t->completed || t->failure != GW_OK) {
            return length; /* the stream is over; ignore what follows */
        }
        gw_sse_result result = gw_sse_feed(&t->parser, data, length, on_event, t);
        if (result == GW_SSE_OVERSIZED) {
            char message[96];
            snprintf(message, sizeof message, "a stream event exceeded %zu bytes", GW_MAX_EVENT_BYTES);
            stream_fail(t, GW_E_PROTOCOL, message);
        } else if (result == GW_SSE_NOMEM) {
            stream_fail(t, GW_E_NOMEM, "out of memory");
        }
        return length;
    }
    if (t->content.length + length > t->limit) {
        /* Keep what fits: an error body is only needed this far, and a
         * successful one this long fails the call. */
        gw_buffer_append(&t->content, data, t->limit - t->content.length);
        t->exceeded = true;
        return CURL_WRITEFUNC_ERROR;
    }
    return gw_buffer_append(&t->content, data, length) ? length : CURL_WRITEFUNC_ERROR;
}

static gw_status transfer_start(gw_client *client, transfer *t, gw_error **error) {
    t->easy = curl_easy_init();
    t->multi = curl_multi_init();
    if (!t->easy || !t->multi) {
        return gw_fail(error, GW_E_NOMEM, "could not create a transfer");
    }
    CURL *e = t->easy;
    curl_easy_setopt(e, CURLOPT_URL, t->url);
    if (t->get) {
        curl_easy_setopt(e, CURLOPT_HTTPGET, 1L);
    } else {
        curl_easy_setopt(e, CURLOPT_POST, 1L);
        curl_easy_setopt(e, CURLOPT_POSTFIELDS, t->body ? t->body : "");
        curl_easy_setopt(e, CURLOPT_POSTFIELDSIZE_LARGE, (curl_off_t)t->body_length);
    }
    curl_easy_setopt(e, CURLOPT_HTTPHEADER, t->headers);
    curl_easy_setopt(e, CURLOPT_HEADERFUNCTION, on_header);
    curl_easy_setopt(e, CURLOPT_HEADERDATA, t);
    curl_easy_setopt(e, CURLOPT_WRITEFUNCTION, on_body);
    curl_easy_setopt(e, CURLOPT_WRITEDATA, t);
    curl_easy_setopt(e, CURLOPT_NOSIGNAL, 1L);
    curl_easy_setopt(e, CURLOPT_FOLLOWLOCATION, 0L);
    curl_easy_setopt(e, CURLOPT_PROTOCOLS_STR, "http,https");
    curl_easy_setopt(e, CURLOPT_SSL_VERIFYPEER, 1L);
    curl_easy_setopt(e, CURLOPT_SSL_VERIFYHOST, 2L);
    long connect_ms = client->timeout_ms < 10000 ? (long)client->timeout_ms : 10000L;
    curl_easy_setopt(e, CURLOPT_CONNECTTIMEOUT_MS, connect_ms);
    if (client->ca_file) {
        curl_easy_setopt(e, CURLOPT_CAINFO, client->ca_file);
    }
    if (client->ca_path) {
        curl_easy_setopt(e, CURLOPT_CAPATH, client->ca_path);
    }
    if (curl_multi_add_handle(t->multi, e) != CURLM_OK) {
        return gw_fail(error, GW_E_NOMEM, "could not start the transfer");
    }
    t->deadline = now_ms() + client->timeout_ms;
    return GW_OK;
}

/** Whether the deadline still applies: always, except to a stream that started. */
static bool deadline_applies(const transfer *t) {
    return !(t->stream && t->headers_done && t->status >= 200 && t->status < 300);
}

typedef bool (*ready_fn)(const transfer *t);

/** Drive the transfer until `ready` holds or it finishes. */
static gw_status pump(transfer *t, ready_fn ready, gw_error **error) {
    while (!(ready && ready(t)) && !t->finished) {
        if (gw_cancel_triggered(t->cancel)) {
            return gw_fail(error, GW_E_CANCELLED, "the call was cancelled");
        }
        int64_t left = t->deadline - now_ms();
        if (deadline_applies(t) && left <= 0) {
            return gw_fail(error, GW_E_TIMEOUT, "the attempt did not finish within the timeout");
        }
        int running = 0;
        CURLMcode code = curl_multi_perform(t->multi, &running);
        if (code != CURLM_OK) {
            return gw_fail(error, GW_E_TRANSPORT, "the transfer failed: %s", curl_multi_strerror(code));
        }
        int pending;
        CURLMsg *message;
        while ((message = curl_multi_info_read(t->multi, &pending))) {
            if (message->msg == CURLMSG_DONE) {
                t->finished = true;
                t->result = message->data.result;
            }
        }
        if ((ready && ready(t)) || t->finished) {
            break;
        }
        int wait = POLL_MS;
        if (deadline_applies(t) && left < wait) {
            wait = left > 0 ? (int)left : 0;
        }
        curl_multi_poll(t->multi, NULL, 0, wait, NULL);
    }
    return GW_OK;
}

static gw_status transport_error(const transfer *t, gw_error **error) {
    if (t->result == CURLE_OPERATION_TIMEDOUT) {
        return gw_fail(error, GW_E_TIMEOUT, "the attempt did not finish within the timeout");
    }
    return gw_fail(error, GW_E_TRANSPORT, "the request failed: %s", curl_easy_strerror(t->result));
}

static bool is_retryable(const char *code) {
    for (size_t i = 0; i < sizeof same_key_retry_codes / sizeof same_key_retry_codes[0]; i++) {
        if (strcmp(code, same_key_retry_codes[i]) == 0) {
            return true;
        }
    }
    return false;
}

static const char *json_str(yyjson_val *object, const char *key) {
    yyjson_val *value = yyjson_obj_get(object, key);
    return yyjson_is_str(value) ? yyjson_get_str(value) : NULL;
}

/** The error of a non-2xx response; *retry_code is set for a same-key retry. */
static gw_status error_from(const transfer *t, gw_error **error, bool *retryable) {
    *retryable = false;
    bool json = false;
    if (t->content_type) {
        for (const char *c = t->content_type; *c; c++) {
            if (starts_with_ci(c, strlen(c), "json")) {
                json = true;
                break;
            }
        }
    }
    if (json && t->content.length) {
        yyjson_doc *doc = yyjson_read_opts(t->content.data, t->content.length, YYJSON_READ_NOFLAG,
                                           gw_json_allocator(), NULL);
        yyjson_val *root = doc ? yyjson_doc_get_root(doc) : NULL;
        const char *code = yyjson_is_obj(root) ? json_str(root, "code") : NULL;
        if (code) {
            const char *request_id = json_str(root, "request_id");
            *retryable = is_retryable(code);
            gw_status status = gw_fail(error, GW_E_PROBLEM, "gateway problem %ld: %.100s", t->status, code);
            if (error) {
                gw_error_set_http(*error, (int)t->status, code, json_str(root, "title"), json_str(root, "detail"),
                                  request_id ? request_id : t->request_id, t->retry_after);
            }
            yyjson_doc_free(doc);
            return status;
        }
        yyjson_doc_free(doc);
    }
    gw_status status = gw_fail(error, GW_E_HTTP, "HTTP %ld without a problem body", t->status);
    if (error) {
        gw_error_set_http(*error, (int)t->status, NULL, NULL, NULL, t->request_id, t->retry_after);
    }
    return status;
}

static bool stream_started(const transfer *t) { return t->headers_done && t->status >= 200 && t->status < 300; }

static bool append_header(struct curl_slist **headers, const char *line) {
    struct curl_slist *next = curl_slist_append(*headers, line);
    if (!next) {
        return false;
    }
    *headers = next;
    return true;
}

/**
 * Send `spec`, retrying with the same key only when nothing was reserved
 * (or, for a call without a key, when the gateway refused it before doing
 * anything). On success `t` holds the response: read for a non-streaming
 * call, started for a stream. `spec->body` must outlive the call.
 */
static gw_status execute_spec(gw_client *client, const gw_http_request *spec, const gw_call_options *options,
                              bool stream, transfer *t, gw_error **error) {
    /* Zeroed first: the caller frees `t` whatever this returns. */
    memset(t, 0, sizeof *t);
    t->retry_after = -1;
    if (error) {
        *error = NULL;
    }
    if (!client || !spec || !spec->path) {
        return gw_fail(error, GW_E_CONFIG, "client and request are required");
    }
    char generated[37];
    const char *key = NULL;
    if (spec->idempotent) {
        key = options ? options->idempotency_key : NULL;
        if (key) {
            size_t n = chars(key);
            if (n < 1 || n > MAX_KEY_CHARS || !header_safe(key)) {
                return gw_fail(error, GW_E_CONFIG, "an Idempotency-Key must be 1 to %d characters", MAX_KEY_CHARS);
            }
        } else {
            if (!new_key(generated)) {
                return gw_fail(error, GW_E_TRANSPORT, "no randomness for an Idempotency-Key");
            }
            key = generated;
        }
    }
    const char *request_id = options ? options->request_id : NULL;
    if (request_id && (chars(request_id) > MAX_REQUEST_ID_CHARS || !header_safe(request_id))) {
        return gw_fail(error, GW_E_CONFIG, "an X-Request-Id must be at most %d characters", MAX_REQUEST_ID_CHARS);
    }
    gw_status status = GW_OK;
    t->stream = stream;
    t->get = spec->get;
    t->success_limit = spec->limit ? spec->limit : MAX_RESPONSE_BYTES;
    t->cancel = options ? options->cancel : NULL;
    t->body = spec->body;
    t->body_length = spec->body_length;
    size_t url_length = strlen(client->base_url) + strlen(spec->path) + 1;
    t->url = gw_malloc(url_length);
    if (!t->url) {
        return gw_fail(error, GW_E_NOMEM, "out of memory");
    }
    snprintf(t->url, url_length, "%s%s", client->base_url, spec->path);

    char line[512];
    struct curl_slist *headers = NULL;
    static const char user_agent[] = "User-Agent: qucelerate-ai-gateway-c/" GW_VERSION_STRING;
    if (spec->content_type) {
        snprintf(line, sizeof line, "Content-Type: %s", spec->content_type);
        status = append_header(&headers, line) ? GW_OK : GW_E_NOMEM;
    } else if (!spec->get) {
        /* An empty POST: no form Content-Type of libcurl's own. */
        status = append_header(&headers, "Content-Type:") ? GW_OK : GW_E_NOMEM;
    }
    if (status == GW_OK) {
        snprintf(line, sizeof line, "Accept: %s", spec->accept ? spec->accept : "application/json");
        status = append_header(&headers, line) ? GW_OK : GW_E_NOMEM;
    }
    const char *fixed[] = {user_agent, "Expect:"};
    for (size_t i = 0; i < sizeof fixed / sizeof fixed[0] && status == GW_OK; i++) {
        status = append_header(&headers, fixed[i]) ? GW_OK : GW_E_NOMEM;
    }
    size_t auth_length = strlen(client->api_key) + 32;
    char *auth = gw_malloc(auth_length);
    if (auth && status == GW_OK) {
        snprintf(auth, auth_length, "Authorization: Bearer %s", client->api_key);
        status = append_header(&headers, auth) ? GW_OK : GW_E_NOMEM;
        memset(auth, 0, auth_length);
    } else {
        status = GW_E_NOMEM;
    }
    gw_free(auth);
    if (status == GW_OK && key) {
        snprintf(line, sizeof line, "Idempotency-Key: %s", key);
        status = append_header(&headers, line) ? GW_OK : GW_E_NOMEM;
    }
    if (status == GW_OK && request_id) {
        snprintf(line, sizeof line, "X-Request-Id: %s", request_id);
        status = append_header(&headers, line) ? GW_OK : GW_E_NOMEM;
    }
    t->headers = headers;
    if (status != GW_OK) {
        t->body = NULL;
        return gw_fail(error, status, "out of memory");
    }

    for (int retries = 0;; retries++) {
        status = transfer_start(client, t, error);
        if (status == GW_OK) {
            status = pump(t, stream ? stream_started : NULL, error);
        }
        if (status != GW_OK) {
            break;
        }
        if (stream && stream_started(t)) {
            break; /* the caller reads the events */
        }
        if (t->result != CURLE_OK && !(t->exceeded && t->status >= 300)) {
            status = t->exceeded ? gw_fail(error, GW_E_DECODE, "the response body exceeded %zu bytes", t->limit)
                                 : transport_error(t, error);
            break;
        }
        if (t->status >= 200 && t->status < 300) {
            break;
        }
        bool retryable;
        gw_error *failure = NULL;
        status = error_from(t, &failure, &retryable);
        if (!retryable || retries >= client->max_retries) {
            if (error) {
                *error = failure;
            } else {
                gw_error_free(failure);
            }
            break;
        }
        int64_t seconds = gw_error_retry_after(failure);
        gw_error_free(failure);
        seconds = seconds < 0 ? 1 : (seconds > MAX_WAIT_SECONDS ? MAX_WAIT_SECONDS : seconds);
        if (!client->sleep(client->sleep_ctx, seconds * 1000, t->cancel)) {
            status = gw_fail(error, GW_E_CANCELLED, "the call was cancelled");
            break;
        }
        transfer_close(t);
        transfer_reset_head(t);
        gw_buffer_clear(&t->content);
        t->exceeded = false;
        t->finished = false;
        t->result = CURLE_OK;
    }
    t->body = NULL;
    return status;
}

/** A JSON POST that sends an Idempotency-Key: chat, streams and embeddings. */
static gw_status execute(gw_client *client, const char *path, yyjson_mut_doc *body, const gw_call_options *options,
                         bool stream, transfer *t, gw_error **error) {
    if (!client || !body) {
        memset(t, 0, sizeof *t);
        t->retry_after = -1;
        if (error) {
            *error = NULL;
        }
        return gw_fail(error, GW_E_CONFIG, "client and request are required");
    }
    size_t json_length = 0;
    char *json = yyjson_mut_write_opts(body, YYJSON_WRITE_NOFLAG, gw_json_allocator(), &json_length, NULL);
    if (!json) {
        memset(t, 0, sizeof *t);
        t->retry_after = -1;
        if (error) {
            *error = NULL;
        }
        return gw_fail(error, GW_E_NOMEM, "could not serialize the request");
    }
    gw_http_request spec = {
        .get = false,
        .path = path,
        .body = json,
        .body_length = json_length,
        .content_type = "application/json",
        .accept = "application/json, text/event-stream",
        .idempotent = true,
        .limit = 0,
    };
    gw_status status = execute_spec(client, &spec, options, stream, t, error);
    gw_free(json);
    return status;
}

gw_status gw_http_send(gw_client *client, const gw_http_request *spec, const gw_call_options *options,
                       gw_http_response *out, gw_error **error) {
    memset(out, 0, sizeof *out);
    transfer t;
    gw_status status = execute_spec(client, spec, options, false, &t, error);
    if (status == GW_OK) {
        out->body = t.content; /* the buffer moves to the response */
        memset(&t.content, 0, sizeof t.content);
        out->request_id = t.request_id;
        t.request_id = NULL;
        out->content_type = t.content_type;
        t.content_type = NULL;
    }
    transfer_free(&t);
    return status;
}

void gw_http_response_free(gw_http_response *response) {
    if (response) {
        gw_buffer_free(&response->body);
        gw_free(response->request_id);
        gw_free(response->content_type);
        memset(response, 0, sizeof *response);
    }
}

bool gw_random_hex(char *out, size_t bytes) {
    unsigned char raw[32];
    if (bytes > sizeof raw || !random_bytes(raw, bytes)) {
        return false;
    }
    static const char digits[] = "0123456789abcdef";
    for (size_t i = 0; i < bytes; i++) {
        out[2 * i] = digits[raw[i] >> 4];
        out[2 * i + 1] = digits[raw[i] & 15];
    }
    out[2 * bytes] = '\0';
    return true;
}

/* ----------------------------------------------------------------- calls */

GW_API gw_status gw_chat(gw_client *client, const gw_chat_request *request, const gw_call_options *options,
                         gw_chat_response **out, gw_error **error) {
    if (out) {
        *out = NULL;
    }
    if (!out || !request) {
        if (error) {
            *error = NULL;
        }
        return gw_fail(error, GW_E_CONFIG, "request and out are required");
    }
    transfer t;
    gw_status status = execute(client, CHAT_PATH, request->doc, options, false, &t, error);
    if (status == GW_OK) {
        char message[256];
        *out = gw_chat_response_parse(t.content.data ? t.content.data : "", t.content.length, t.request_id, &status,
                                      message, sizeof message);
        if (status != GW_OK) {
            gw_fail(error, status, "%s", status == GW_E_NOMEM ? "out of memory" : message);
        }
    }
    transfer_free(&t);
    return status;
}

GW_API gw_status gw_embeddings(gw_client *client, const gw_embedding_request *request,
                               const gw_call_options *options, gw_embedding_response **out, gw_error **error) {
    if (out) {
        *out = NULL;
    }
    if (!out || !request) {
        if (error) {
            *error = NULL;
        }
        return gw_fail(error, GW_E_CONFIG, "request and out are required");
    }
    transfer t;
    gw_status status = execute(client, EMBEDDINGS_PATH, request->doc, options, false, &t, error);
    if (status == GW_OK) {
        char message[256];
        *out = gw_embedding_response_parse(t.content.data ? t.content.data : "", t.content.length, t.request_id,
                                           &status, message, sizeof message);
        if (status != GW_OK) {
            gw_fail(error, status, "%s", status == GW_E_NOMEM ? "out of memory" : message);
        }
    }
    transfer_free(&t);
    return status;
}

/* --------------------------------------------------------------- streams */

struct gw_stream {
    transfer t;
    gw_event *current;
    bool done;
};

GW_API gw_status gw_chat_stream_open(gw_client *client, const gw_chat_request *request,
                                     const gw_call_options *options, gw_stream **out, gw_error **error) {
    if (out) {
        *out = NULL;
    }
    if (!out || !request) {
        if (error) {
            *error = NULL;
        }
        return gw_fail(error, GW_E_CONFIG, "request and out are required");
    }
    gw_stream *stream = gw_calloc(1, sizeof *stream);
    if (!stream) {
        if (error) {
            *error = NULL;
        }
        return gw_fail(error, GW_E_NOMEM, "out of memory");
    }
    gw_status status = execute(client, STREAM_PATH, request->doc, options, true, &stream->t, error);
    if (status != GW_OK) {
        transfer_free(&stream->t);
        gw_free(stream);
        return status;
    }
    *out = stream;
    return GW_OK;
}

static bool event_ready(const transfer *t) {
    return t->queue_head < t->queue_length || t->failure != GW_OK || t->completed;
}

GW_API gw_status gw_stream_next(gw_stream *stream, const gw_event **event, gw_error **error) {
    if (error) {
        *error = NULL;
    }
    if (event) {
        *event = NULL;
    }
    if (!stream || !event) {
        return gw_fail(error, GW_E_CONFIG, "stream and event are required");
    }
    gw_event_free(stream->current);
    stream->current = NULL;
    transfer *t = &stream->t;
    if (stream->done) {
        return GW_DONE;
    }
    if (t->queue_head == t->queue_length && t->failure == GW_OK && !t->completed) {
        gw_status status = pump(t, event_ready, error);
        if (status != GW_OK) {
            stream->done = true;
            transfer_close(t);
            return status;
        }
    }
    if (t->queue_head < t->queue_length) {
        stream->current = t->queue[t->queue_head++];
        if (t->queue_head == t->queue_length) {
            t->queue_head = t->queue_length = 0;
        }
        *event = stream->current;
        return GW_OK;
    }
    stream->done = true;
    transfer_close(t);
    if (t->failure != GW_OK) {
        gw_status status = gw_fail(error, t->failure, "%s", t->failure_message);
        if (t->failure == GW_E_STREAM && error) {
            gw_error_set_http(*error, t->failure_status, t->failure_code, NULL, NULL, t->request_id, -1);
        }
        return status;
    }
    if (t->completed) {
        return GW_DONE;
    }
    if (t->result != CURLE_OK) {
        return transport_error(t, error);
    }
    return gw_fail(error, GW_E_PROTOCOL, "the stream closed before a terminal event");
}

GW_API const char *gw_stream_request_id(const gw_stream *stream) { return stream ? stream->t.request_id : NULL; }

GW_API void gw_stream_free(gw_stream *stream) {
    if (stream) {
        gw_event_free(stream->current);
        transfer_free(&stream->t);
        gw_free(stream);
    }
}
