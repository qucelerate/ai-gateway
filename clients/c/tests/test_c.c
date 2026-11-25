/*
 * Unit tests of the C API: the SSE parser, argument and configuration
 * checks, NULL handling, limits, timeouts and cancellation. The library runs
 * on a counting allocator, so every test also proves nothing leaked.
 * Network tests use a forked one-shot HTTP server on a loopback socket.
 */
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE 1

#include "decode.h"
#include "internal.h"
#include "objects.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

#define CHECK(condition)                                                                                       \
    do {                                                                                                       \
        if (!(condition)) {                                                                                    \
            failures++;                                                                                        \
            fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #condition);                      \
        }                                                                                                      \
    } while (0)

/* ------------------------------------------------------------ allocator */

static long live_allocations;

static void *count_malloc(void *ctx, size_t size) {
    (void)ctx;
    void *p = malloc(size);
    if (p) {
        live_allocations++;
    }
    return p;
}

static void *count_realloc(void *ctx, void *ptr, size_t size) {
    (void)ctx;
    void *p = realloc(ptr, size);
    if (p && !ptr) {
        live_allocations++;
    }
    return p;
}

static void count_free(void *ctx, void *ptr) {
    (void)ctx;
    if (ptr) {
        live_allocations--;
    }
    free(ptr);
}

/* ----------------------------------------------------------- SSE parser */

typedef struct recorded {
    char text[4096];
    size_t length;
    int events;
} recorded;

static bool record(void *ctx, const char *name, const char *data, size_t data_length) {
    recorded *r = ctx;
    size_t available = sizeof r->text - r->length;
    int n = snprintf(r->text + r->length, available, "[%s|%.*s]", name, (int)data_length, data);
    if (n > 0) {
        r->length += (size_t)n < available ? (size_t)n : available - 1; /* stays inside the buffer */
    }
    r->events++;
    return true;
}

static const char sample[] = ": comment\r\n"
                             "event: response.started\r\n"
                             "data: {\"model\":\"m\"}\r\n"
                             "\r\n"
                             "event: choice.delta\n"
                             "data: line one\n"
                             "data:line two\n"
                             "id: ignored\n"
                             "\n"
                             "data: unnamed\r"
                             "\r"
                             "event: response.completed\n"
                             "data: {}\n"
                             "\n";

static const char sample_events[] = "[response.started|{\"model\":\"m\"}]"
                                    "[choice.delta|line one\nline two]"
                                    "[message|unnamed]"
                                    "[response.completed|{}]";

static void test_sse_chunking(void) {
    for (size_t size = 1; size <= sizeof sample; size++) {
        gw_sse_parser parser = {0};
        recorded r = {0};
        for (size_t at = 0; at < sizeof sample - 1; at += size) {
            size_t n = sizeof sample - 1 - at < size ? sizeof sample - 1 - at : size;
            CHECK(gw_sse_feed(&parser, sample + at, n, record, &r) == GW_SSE_OK);
        }
        if (strcmp(r.text, sample_events) != 0) {
            failures++;
            fprintf(stderr, "chunks of %zu: got %s\n", size, r.text);
        }
        gw_sse_free(&parser);
    }
}

static void test_sse_oversized(void) {
    size_t chunk = (size_t)1 << 20;
    char *filler = malloc(chunk);
    memset(filler, 'x', chunk);

    /* One unterminated line past the limit. */
    gw_sse_parser parser = {0};
    recorded r = {0};
    const char before[] = "event: choice.delta\ndata: ok\n\ndata: ";
    gw_sse_result result = gw_sse_feed(&parser, before, sizeof before - 1, record, &r);
    for (size_t sent = 0; result == GW_SSE_OK && sent <= GW_MAX_EVENT_BYTES; sent += chunk) {
        result = gw_sse_feed(&parser, filler, chunk, record, &r);
    }
    CHECK(result == GW_SSE_OVERSIZED);
    CHECK(r.events == 1); /* the event before it was delivered */
    gw_sse_free(&parser);

    /* Many complete data lines whose joined data passes the limit. */
    gw_sse_parser lines = {0};
    recorded none = {0};
    memcpy(filler, "data: ", 6);
    filler[chunk - 1] = '\n';
    result = GW_SSE_OK;
    for (size_t sent = 0; result == GW_SSE_OK && sent <= GW_MAX_EVENT_BYTES + chunk; sent += chunk) {
        result = gw_sse_feed(&lines, filler, chunk, record, &none);
    }
    CHECK(result == GW_SSE_OVERSIZED);
    CHECK(none.events == 0);
    gw_sse_free(&lines);

    /* Just under the limit is fine. */
    gw_sse_parser under = {0};
    recorded one = {0};
    size_t data = GW_MAX_EVENT_BYTES - 64;
    char *big = malloc(data + 16);
    memcpy(big, "data: ", 6);
    memset(big + 6, 'y', data - 6);
    memcpy(big + data, "\n\n", 2);
    CHECK(gw_sse_feed(&under, big, data + 2, record, &one) == GW_SSE_OK);
    CHECK(one.events == 1);
    gw_sse_free(&under);
    free(big);
    free(filler);
}

/* ------------------------------------------------- arguments and config */

static gw_client *client_for(const char *url, int64_t timeout_ms) {
    gw_client_options *options = gw_client_options_new();
    CHECK(gw_client_options_set_base_url(options, url) == GW_OK);
    CHECK(gw_client_options_set_api_key(options, "gw_test") == GW_OK);
    CHECK(gw_client_options_set_timeout_ms(options, timeout_ms) == GW_OK);
    gw_client *client = NULL;
    gw_error *error = NULL;
    CHECK(gw_client_new(options, &client, &error) == GW_OK);
    gw_error_free(error);
    gw_client_options_free(options);
    return client;
}

static gw_status new_client_status(const char *url, const char *key) {
    gw_client_options *options = gw_client_options_new();
    if (url) {
        gw_client_options_set_base_url(options, url);
    }
    if (key) {
        gw_client_options_set_api_key(options, key);
    }
    gw_client *client = NULL;
    gw_error *error = NULL;
    gw_status status = gw_client_new(options, &client, &error);
    CHECK((status == GW_OK) == (client != NULL));
    CHECK((status == GW_OK) == (error == NULL));
    CHECK(status == GW_OK || gw_error_status(error) == status);
    gw_error_free(error);
    gw_client_free(client);
    gw_client_options_free(options);
    return status;
}

static void test_config(void) {
    CHECK(new_client_status("https://gateway.example.com", "gw_x") == GW_OK);
    CHECK(new_client_status("http://127.0.0.1:1/", "gw_x") == GW_OK);
    CHECK(new_client_status(NULL, "gw_x") == GW_E_CONFIG);
    CHECK(new_client_status("https://gateway.example.com", NULL) == GW_E_CONFIG);
    CHECK(new_client_status("https://gateway.example.com", "") == GW_E_CONFIG);
    CHECK(new_client_status("https://gateway.example.com", "  ") == GW_E_CONFIG);
    CHECK(new_client_status("ftp://gateway.example.com", "gw_x") == GW_E_CONFIG);
    CHECK(new_client_status("https://", "gw_x") == GW_E_CONFIG);
    CHECK(new_client_status("gateway.example.com", "gw_x") == GW_E_CONFIG);
    CHECK(new_client_status("https://a.example.com\r\nX-Evil: 1", "gw_x") == GW_E_CONFIG);
    CHECK(new_client_status("https://gateway.example.com", "gw_x\r\nX-Evil: 1") == GW_E_CONFIG);
    CHECK(new_client_status("https://gateway.example.com", "gw_x\x01") == GW_E_CONFIG);

    gw_client_options *options = gw_client_options_new();
    CHECK(gw_client_options_set_max_retries(options, -1) == GW_E_CONFIG);
    CHECK(gw_client_options_set_max_retries(options, 0) == GW_OK);
    CHECK(gw_client_options_set_timeout_ms(options, 0) == GW_E_CONFIG);
    CHECK(gw_client_options_set_timeout_ms(options, -5) == GW_E_CONFIG);
    gw_client_options_free(options);

    gw_chat_request *request = gw_chat_request_new("m");
    CHECK(gw_chat_request_set_temperature(request, -0.1) == GW_E_CONFIG);
    CHECK(gw_chat_request_set_temperature(request, 2.5) == GW_E_CONFIG);
    CHECK(gw_chat_request_set_temperature(request, 0.0 / 0.0) == GW_E_CONFIG);
    CHECK(gw_chat_request_set_temperature(request, 0.5) == GW_OK);
    CHECK(gw_chat_request_set_temperature(request, 0.7) == GW_OK); /* replaces, does not duplicate */
    CHECK(gw_chat_request_set_max_tokens(request, 0) == GW_E_CONFIG);
    CHECK(gw_chat_request_add_message(request, "user", NULL) == GW_E_CONFIG);
    CHECK(gw_chat_request_add_message(request, "user", "hi") == GW_OK);

    /* Request text keeps NUL bytes too. */
    gw_chat_request *nul = gw_chat_request_new("m");
    CHECK(gw_chat_request_add_message_n(nul, "user", "a\0b", 3) == GW_OK);
    gw_embedding_request *inputs = gw_embedding_request_new("e");
    CHECK(gw_embedding_request_add_input_n(inputs, "c\0d", 3) == GW_OK);
    char *json = yyjson_mut_write(nul->doc, 0, NULL);
    CHECK(json && strstr(json, "\"content\":\"a\\u0000b\""));
    free(json);
    json = yyjson_mut_write(inputs->doc, 0, NULL);
    CHECK(json && strstr(json, "[\"c\\u0000d\"]"));
    free(json);
    gw_chat_request_free(nul);
    gw_embedding_request_free(inputs);
    gw_chat_request *result = gw_chat_request_new("m");
    CHECK(gw_chat_request_add_tool_result_n(result, "call_1", NULL, 0) == GW_E_CONFIG);
    CHECK(gw_chat_request_add_tool_result_n(result, "call_1", "e\0f", 3) == GW_OK);
    json = yyjson_mut_write(result->doc, 0, NULL);
    CHECK(json && strstr(json, "{\"role\":\"tool\",\"content\":\"e\\u0000f\",\"tool_call_id\":\"call_1\"}"));
    free(json);
    gw_chat_request_free(result);

    /* Bad call options fail before any request is sent (nothing listens on port 1). */
    gw_client *client = client_for("http://127.0.0.1:1", 1000);
    char long_key[300];
    memset(long_key, 'k', sizeof long_key - 1);
    long_key[sizeof long_key - 1] = '\0';
    const char *bad_keys[] = {long_key, "", "key\nX-Evil: 1"};
    for (size_t i = 0; i < sizeof bad_keys / sizeof *bad_keys; i++) {
        gw_call_options *call = gw_call_options_new();
        gw_call_options_set_idempotency_key(call, bad_keys[i]);
        gw_chat_response *response = NULL;
        gw_error *error = NULL;
        CHECK(gw_chat(client, request, call, &response, &error) == GW_E_CONFIG);
        CHECK(response == NULL && error != NULL);
        gw_error_free(error);
        gw_call_options_free(call);
    }
    long_key[129] = '\0';
    const char *bad_ids[] = {long_key, "id\r\n"};
    for (size_t i = 0; i < sizeof bad_ids / sizeof *bad_ids; i++) {
        gw_call_options *call = gw_call_options_new();
        gw_call_options_set_request_id(call, bad_ids[i]);
        gw_error *error = NULL;
        gw_stream *stream = NULL;
        CHECK(gw_chat_stream_open(client, request, call, &stream, &error) == GW_E_CONFIG);
        CHECK(stream == NULL);
        gw_error_free(error);
        gw_call_options_free(call);
    }

    /* A refused connection is a transport error; the error is optional. */
    gw_chat_response *response = NULL;
    CHECK(gw_chat(client, request, NULL, &response, NULL) == GW_E_TRANSPORT);
    CHECK(response == NULL);
    gw_client_free(client);
    gw_chat_request_free(request);
}

static void test_null(void) {
    gw_error_free(NULL);
    gw_cancel_free(NULL);
    gw_client_options_free(NULL);
    gw_client_free(NULL);
    gw_call_options_free(NULL);
    gw_chat_request_free(NULL);
    gw_embedding_request_free(NULL);
    gw_chat_response_free(NULL);
    gw_embedding_response_free(NULL);
    gw_stream_free(NULL);
    CHECK(gw_error_message(NULL) == NULL || gw_error_message(NULL)[0] == '\0');
    CHECK(gw_error_code(NULL) == NULL);
    CHECK(gw_error_retry_after(NULL) == -1);
    CHECK(gw_chat_request_new(NULL) == NULL);
    CHECK(gw_chat_request_add_message(NULL, "user", "hi") == GW_E_CONFIG);
    CHECK(gw_client_options_set_base_url(NULL, "https://x") == GW_E_CONFIG);
    CHECK(gw_chat_response_choice_count(NULL) == 0);
    CHECK(gw_chat_response_choice_content(NULL, 0, NULL) == NULL);
    CHECK(gw_usage_quality(NULL) == NULL);
    int64_t value = 7;
    CHECK(!gw_usage_get(NULL, GW_USAGE_INPUT_TOKENS, &value) && value == 7);
    CHECK(gw_event_choice_index(NULL) == -1);
    CHECK(gw_stream_next(NULL, NULL, NULL) == GW_E_CONFIG);
    CHECK(gw_chat(NULL, NULL, NULL, NULL, NULL) == GW_E_CONFIG);
    CHECK(strcmp(gw_status_name(GW_E_TIMEOUT), "timeout") == 0);
    CHECK(strcmp(gw_version(), "0.2.0") == 0);
}

/* --------------------------------------------------------------- decode */

static gw_status chat_status(const char *body) {
    gw_status status = GW_OK;
    char message[256];
    gw_chat_response *response = gw_chat_response_parse(body, strlen(body), "req", &status, message, sizeof message);
    CHECK((response != NULL) == (status == GW_OK));
    gw_chat_response_free(response);
    return status;
}

static void test_decode(void) {
    const char *good = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\","
                       "\"content\":null},\"finish_reason\":null,\"future\":1}],\"usage\":null,\"later\":[]}";
    CHECK(chat_status(good) == GW_OK);
    CHECK(chat_status("") == GW_E_DECODE);
    CHECK(chat_status("[]") == GW_E_DECODE);
    CHECK(chat_status("{\"id\":\"c\",\"model\":\"m\"}") == GW_E_DECODE);
    CHECK(chat_status("{\"id\":1,\"model\":\"m\",\"choices\":[]}") == GW_E_DECODE);
    CHECK(chat_status("{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0}]}") == GW_E_DECODE);
    CHECK(chat_status("{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"message\":{\"content\":\"x\"}}]}") == GW_E_DECODE);
    CHECK(chat_status("{\"id\":\"c\",\"model\":\"m\",\"choices\":[],\"usage\":{\"input_tokens\":\"9\"}}") ==
          GW_E_DECODE);

    gw_status status = GW_OK;
    char message[256];
    const char *usage = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[],\"usage\":{\"input_tokens\":9,"
                        "\"output_tokens\":null,\"quality\":\"exact\",\"completeness\":\"complete\"}}";
    gw_chat_response *response = gw_chat_response_parse(usage, strlen(usage), NULL, &status, message, sizeof message);
    CHECK(response != NULL);
    const gw_usage *u = gw_chat_response_usage(response);
    int64_t value = -1;
    CHECK(gw_usage_get(u, GW_USAGE_INPUT_TOKENS, &value) && value == 9);
    CHECK(!gw_usage_get(u, GW_USAGE_OUTPUT_TOKENS, &value));
    CHECK(!gw_usage_get(u, (gw_usage_counter)99, &value));
    CHECK(gw_chat_response_request_id(response) == NULL);
    CHECK(gw_chat_response_choice_role(response, 5) == NULL);
    gw_chat_response_free(response);

    gw_event *event = NULL;
    const char *delta = "{\"choice_index\":1,\"content_delta\":\"hi\",\"refusal_delta\":null}";
    CHECK(gw_event_decode(GW_EVENT_CHOICE_DELTA, "choice.delta", delta, strlen(delta), &event, message,
                          sizeof message) == GW_OK);
    CHECK(gw_event_choice_index(event) == 1 && strcmp(gw_event_content_delta(event, NULL), "hi") == 0);
    CHECK(gw_event_refusal_delta(event, NULL) == NULL && gw_event_model(event) == NULL);
    gw_event_free(event);
    event = NULL;
    CHECK(gw_event_decode(GW_EVENT_CHOICE_DELTA, "choice.delta", "{\"choice_index\":\"1\"}", 20, &event, message,
                          sizeof message) == GW_E_DECODE);
    CHECK(event == NULL);
    CHECK(gw_event_decode(GW_EVENT_CHOICE_DELTA, "choice.delta", "not json", 8, &event, message, sizeof message) ==
          GW_E_DECODE);

    /* Model text keeps NUL bytes, with its full length. */
    const char *nul = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"message\":{\"role\":"
                      "\"assistant\",\"content\":\"a\\u0000b\"}}]}";
    response = gw_chat_response_parse(nul, strlen(nul), NULL, &status, message, sizeof message);
    size_t text_length = 0;
    const char *text = gw_chat_response_choice_content(response, 0, &text_length);
    CHECK(text && text_length == 3 && memcmp(text, "a\0b", 3) == 0);
    gw_chat_response_free(response);
    const char *nul_delta = "{\"choice_index\":0,\"content_delta\":\"x\\u0000y\",\"refusal_delta\":null}";
    CHECK(gw_event_decode(GW_EVENT_CHOICE_DELTA, "choice.delta", nul_delta, strlen(nul_delta), &event, message,
                          sizeof message) == GW_OK);
    text = gw_event_content_delta(event, &text_length);
    CHECK(text && text_length == 3 && memcmp(text, "x\0y", 3) == 0);
    gw_event_free(event);

    /* Known events must carry the fields the contract requires. */
    struct {
        gw_event_type type;
        const char *data;
        gw_status want;
    } events[] = {
        {GW_EVENT_RESPONSE_STARTED, "{}", GW_E_DECODE},
        {GW_EVENT_RESPONSE_STARTED, "{\"model\":\"m\",\"future\":1}", GW_OK},
        {GW_EVENT_CHOICE_FINISHED, "{\"choice_index\":0}", GW_E_DECODE},
        {GW_EVENT_CHOICE_FINISHED, "{\"choice_index\":0,\"finish_reason\":null}", GW_E_DECODE},
        {GW_EVENT_SAFETY_ANNOTATION, "{\"choice_index\":0}", GW_E_DECODE},
        {GW_EVENT_SAFETY_ANNOTATION, "{\"choice_index\":0,\"blocked\":false}", GW_E_DECODE},
        {GW_EVENT_SAFETY_ANNOTATION, "{\"choice_index\":0,\"category\":\"c\",\"blocked\":null}", GW_E_DECODE},
        {GW_EVENT_SAFETY_ANNOTATION, "{\"choice_index\":0,\"category\":\"c\",\"blocked\":true}", GW_OK},
        {GW_EVENT_TOOL_CALL_DELTA, "{\"choice_index\":0,\"tool_call_index\":0}", GW_E_DECODE},
        {GW_EVENT_TOOL_CALL_DELTA, "{\"choice_index\":0,\"tool_call_index\":0,\"arguments_delta\":\"\"}", GW_OK},
        {GW_EVENT_RESPONSE_COMPLETED, "{}", GW_E_DECODE},
        {GW_EVENT_RESPONSE_COMPLETED, "{\"model\":\"m\"}", GW_E_DECODE},
        {GW_EVENT_RESPONSE_COMPLETED, "{\"model\":\"m\",\"usage\":null}", GW_OK},
    };
    for (size_t i = 0; i < sizeof events / sizeof *events; i++) {
        event = NULL;
        gw_status got = gw_event_decode(events[i].type, "e", events[i].data, strlen(events[i].data), &event,
                                        message, sizeof message);
        if (got != events[i].want) {
            failures++;
            fprintf(stderr, "event %zu (%s): got %d\n", i, events[i].data, (int)got);
        }
        gw_event_free(event);
    }

    char *code = NULL;
    int http = 0;
    const char *error = "{\"code\":\"provider_failure\",\"status\":502,\"message\":\"m\"}";
    CHECK(gw_stream_error_decode(error, strlen(error), &code, &http));
    CHECK(code && strcmp(code, "provider_failure") == 0 && http == 502);
    gw_free(code);
    CHECK(!gw_stream_error_decode("{\"status\":502}", 14, &code, &http));
}

/* --------------------------------------------------------- network tests */

/* When set, the next served child keeps its connection open after replying. */
static bool hold_open;

typedef struct server {
    int fd;
    int port;
    pid_t child;
} server;

/** Listen on a loopback port; with `reply`, a child answers one request with it. */
static server serve(const char *reply, size_t reply_length, size_t body_repeat, char body_byte) {
    server s = {socket(AF_INET, SOCK_STREAM, 0), 0, -1};
    struct sockaddr_in address = {0};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t length = sizeof address;
    CHECK(bind(s.fd, (struct sockaddr *)&address, sizeof address) == 0);
    CHECK(listen(s.fd, 4) == 0);
    CHECK(getsockname(s.fd, (struct sockaddr *)&address, &length) == 0);
    s.port = ntohs(address.sin_port);
    if (!reply) {
        return s; /* accepts nothing: connections wait in the backlog */
    }
    s.child = fork();
    if (s.child == 0) {
        int fd = accept(s.fd, NULL, NULL);
        char buffer[65536];
        /* Read the request head; the bodies here are small and sent with it. */
        ssize_t n = recv(fd, buffer, sizeof buffer, 0);
        (void)n;
        if (write(fd, reply, reply_length) < 0) {
            _exit(1);
        }
        memset(buffer, body_byte, sizeof buffer);
        for (size_t sent = 0; sent < body_repeat;) {
            size_t part = body_repeat - sent < sizeof buffer ? body_repeat - sent : sizeof buffer;
            ssize_t w = write(fd, buffer, part);
            if (w <= 0) {
                break;
            }
            sent += (size_t)w;
        }
        if (hold_open) {
            pause();
        }
        close(fd);
        _exit(0);
    }
    return s;
}

static void server_close(server *s) {
    close(s->fd);
    if (s->child > 0) {
        kill(s->child, SIGKILL);
        waitpid(s->child, NULL, 0);
    }
}

static char *url_of(const server *s, char *buffer, size_t size) {
    snprintf(buffer, size, "http://127.0.0.1:%d", s->port);
    return buffer;
}

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec * 1000.0 + (double)t.tv_nsec / 1e6;
}

static gw_chat_request *hello(void) {
    gw_chat_request *request = gw_chat_request_new("m");
    gw_chat_request_add_message(request, "user", "hi");
    return request;
}

static void test_timeout_and_cancel(void) {
    server silent = serve(NULL, 0, 0, 0);
    char url[64];
    gw_client *client = client_for(url_of(&silent, url, sizeof url), 300);
    gw_chat_request *request = hello();

    double start = now_ms();
    gw_chat_response *response = NULL;
    gw_error *error = NULL;
    CHECK(gw_chat(client, request, NULL, &response, &error) == GW_E_TIMEOUT);
    CHECK(now_ms() - start < 3000);
    gw_error_free(error);

    /* A stream's start is bounded by the timeout too. */
    gw_stream *stream = NULL;
    start = now_ms();
    CHECK(gw_chat_stream_open(client, request, NULL, &stream, NULL) == GW_E_TIMEOUT);
    CHECK(stream == NULL && now_ms() - start < 3000);

    /* A cancelled flag stops the call at once. */
    gw_cancel *cancel = gw_cancel_new();
    gw_cancel_trigger(cancel);
    CHECK(gw_cancel_triggered(cancel));
    gw_call_options *call = gw_call_options_new();
    gw_call_options_set_cancel(call, cancel);
    start = now_ms();
    CHECK(gw_chat(client, request, call, &response, NULL) == GW_E_CANCELLED);
    CHECK(now_ms() - start < 250);
    gw_call_options_free(call);
    gw_cancel_free(cancel);
    gw_client_free(client);

    /* Cancelling a started stream ends it, though the server stays silent. */
    const char reply[] = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n"
                         "event: response.started\ndata: {\"model\":\"m\"}\n\n";
    hold_open = true;
    server held = serve(reply, sizeof reply - 1, 0, 0);
    hold_open = false;
    char held_url[64];
    client = client_for(url_of(&held, held_url, sizeof held_url), 300);
    cancel = gw_cancel_new();
    call = gw_call_options_new();
    gw_call_options_set_cancel(call, cancel);
    CHECK(gw_chat_stream_open(client, request, call, &stream, NULL) == GW_OK);
    const gw_event *event = NULL;
    CHECK(gw_stream_next(stream, &event, NULL) == GW_OK);
    gw_cancel_trigger(cancel);
    start = now_ms();
    CHECK(gw_stream_next(stream, &event, NULL) == GW_E_CANCELLED);
    CHECK(now_ms() - start < 250);
    gw_stream_free(stream);
    gw_call_options_free(call);
    gw_cancel_free(cancel);
    gw_client_free(client);
    server_close(&held);

    gw_chat_request_free(request);
    server_close(&silent);
}

static void test_body_limits(void) {
    char url[64];
    char head[256];
    gw_chat_request *request = hello();

    /* A successful body over 64 MiB fails the call instead of growing without bound. */
    size_t big = ((size_t)64 << 20) + 1;
    int n = snprintf(head, sizeof head,
                     "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %zu\r\n\r\n", big);
    server large = serve(head, (size_t)n, big, ' ');
    gw_client *client = client_for(url_of(&large, url, sizeof url), 20000);
    gw_chat_response *response = NULL;
    gw_error *error = NULL;
    CHECK(gw_chat(client, request, NULL, &response, &error) == GW_E_DECODE);
    CHECK(error && strstr(gw_error_message(error), "exceeded"));
    gw_error_free(error);
    gw_client_free(client);
    server_close(&large);

    /* An error body is read only so far, and the status still surfaces. */
    size_t error_body = (size_t)8 << 20;
    n = snprintf(head, sizeof head,
                 "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nX-Request-Id: req-big\r\n"
                 "Content-Length: %zu\r\n\r\n",
                 error_body);
    server proxy = serve(head, (size_t)n, error_body, '<');
    client = client_for(url_of(&proxy, url, sizeof url), 20000);
    error = NULL;
    CHECK(gw_chat(client, request, NULL, &response, &error) == GW_E_HTTP);
    CHECK(gw_error_http_status(error) == 502);
    CHECK(gw_error_request_id(error) && strcmp(gw_error_request_id(error), "req-big") == 0);
    CHECK(gw_error_retry_after(error) == -1);
    gw_error_free(error);
    gw_client_free(client);
    server_close(&proxy);

    /* A non-problem error keeps its Retry-After. */
    const char busy[] = "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nRetry-After: 30\r\n"
                        "Content-Length: 4\r\n\r\nbusy";
    server proxy_busy = serve(busy, sizeof busy - 1, 0, 0);
    client = client_for(url_of(&proxy_busy, url, sizeof url), 20000);
    error = NULL;
    CHECK(gw_chat(client, request, NULL, &response, &error) == GW_E_HTTP);
    CHECK(gw_error_http_status(error) == 503 && gw_error_retry_after(error) == 30);
    gw_error_free(error);
    gw_client_free(client);
    server_close(&proxy_busy);

    /* A stream event over 16 MiB is a protocol error. */
    size_t event = ((size_t)16 << 20) + 16;
    n = snprintf(head, sizeof head,
                 "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n"
                 "event: response.started\ndata: {\"model\":\"m\"}\n\ndata: ");
    server flood = serve(head, (size_t)n, event, 'x');
    client = client_for(url_of(&flood, url, sizeof url), 20000);
    gw_stream *stream = NULL;
    CHECK(gw_chat_stream_open(client, request, NULL, &stream, NULL) == GW_OK);
    const gw_event *first = NULL;
    CHECK(gw_stream_next(stream, &first, NULL) == GW_OK);
    CHECK(first && gw_event_get_type(first) == GW_EVENT_RESPONSE_STARTED);
    error = NULL;
    CHECK(gw_stream_next(stream, &first, &error) == GW_E_PROTOCOL);
    CHECK(error && strstr(gw_error_message(error), "exceeded"));
    gw_error_free(error);
    gw_stream_free(stream);
    gw_client_free(client);
    server_close(&flood);

    gw_chat_request_free(request);
}

/* ----------------------------------------------------------------- tools */

static void test_tools(void) {
    gw_chat_request *request = gw_chat_request_new("m");
    CHECK(gw_chat_request_add_tool(request, "get_weather", "Current weather", "[1]") == GW_E_CONFIG);
    CHECK(gw_chat_request_add_tool(request, "get_weather", "Current weather", "{\"type\":") == GW_E_CONFIG);
    CHECK(gw_chat_request_add_tool(request, "get_weather", "Current weather", "{\"type\":\"object\"}") == GW_OK);
    CHECK(gw_chat_request_add_tool(request, "now", NULL, NULL) == GW_OK);
    CHECK(gw_chat_request_set_tool_choice(request, "sometimes") == GW_E_CONFIG);
    CHECK(gw_chat_request_set_tool_choice(request, "required") == GW_OK);
    CHECK(gw_chat_request_set_tool_choice_function(request, "get_weather") == GW_OK); /* replaces */
    CHECK(gw_chat_request_set_parallel_tool_calls(request, false) == GW_OK);
    CHECK(gw_chat_request_add_message(request, "user", "Weather?") == GW_OK);
    CHECK(gw_chat_request_add_tool_call(request, "call_1", "get_weather", "{}") == GW_E_CONFIG); /* not assistant */
    CHECK(gw_chat_request_add_message(request, "assistant", "") == GW_OK);
    CHECK(gw_chat_request_add_tool_call(request, "call_1", "get_weather", "{\"city\":\"Paris\"}") == GW_OK);
    CHECK(gw_chat_request_add_tool_result(request, "call_1", "18C") == GW_OK);
    char *json = yyjson_mut_write(request->doc, 0, NULL);
    CHECK(json && strstr(json, "\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"get_weather\","
                               "\"description\":\"Current weather\",\"parameters\":{\"type\":\"object\"}}},"
                               "{\"type\":\"function\",\"function\":{\"name\":\"now\"}}]"));
    CHECK(json && strstr(json, "\"tool_choice\":{\"type\":\"function\",\"function\":{\"name\":\"get_weather\"}}"));
    CHECK(json && strstr(json, "\"parallel_tool_calls\":false"));
    CHECK(json && strstr(json, "{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"call_1\","
                               "\"type\":\"function\",\"function\":{\"name\":\"get_weather\","
                               "\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}"));
    CHECK(json && strstr(json, "{\"role\":\"tool\",\"content\":\"18C\",\"tool_call_id\":\"call_1\"}"));
    free(json);
    gw_chat_request_free(request);

    const char *body = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\","
                       "\"content\":null,\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":"
                       "{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}";
    gw_status status = GW_OK;
    char message[256];
    gw_chat_response *response = gw_chat_response_parse(body, strlen(body), "req", &status, message, sizeof message);
    CHECK(response && gw_chat_response_choice_tool_call_count(response, 0) == 1);
    const char *id = NULL, *name = NULL, *arguments = NULL;
    CHECK(gw_chat_response_choice_tool_call(response, 0, 0, &id, &name, &arguments));
    CHECK(id && strcmp(id, "call_1") == 0 && name && strcmp(name, "get_weather") == 0 && arguments &&
          strcmp(arguments, "{}") == 0);
    CHECK(!gw_chat_response_choice_tool_call(response, 0, 1, &id, NULL, NULL));
    CHECK(gw_chat_response_choice_tool_call_count(response, 1) == 0);
    gw_chat_response_free(response);
    CHECK(chat_status("{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"message\":{\"role\":\"assistant\","
                      "\"content\":null,\"tool_calls\":[{\"id\":1}]}}]}") == GW_E_DECODE);
}

/* ---------------------------------------------------- files and batches */

typedef struct captured {
    server s;
    int pipe_fd; /* the child writes the request it read here */
} captured;

/** A one-shot server that reads one whole request (head and Content-Length body), hands it back, and replies. */
static captured serve_capture(const char *reply) {
    captured c = {serve(NULL, 0, 0, 0), -1};
    int fds[2];
    CHECK(pipe(fds) == 0);
    c.s.child = fork();
    if (c.s.child == 0) {
        close(fds[0]);
        int fd = accept(c.s.fd, NULL, NULL);
        size_t capacity = 1 << 16, length = 0;
        char *request = malloc(capacity);
        size_t want = 0;
        for (;;) {
            if (length == capacity) {
                capacity *= 2;
                request = realloc(request, capacity);
            }
            ssize_t n = recv(fd, request + length, capacity - length, 0);
            if (n <= 0) {
                break;
            }
            length += (size_t)n;
            char *end = NULL;
            for (size_t i = 0; i + 3 < length && !end; i++) {
                if (memcmp(request + i, "\r\n\r\n", 4) == 0) {
                    end = request + i + 4;
                }
            }
            if (end && !want) {
                size_t body = 0;
                for (char *line = request; line < end; line = strstr(line, "\r\n") + 2) {
                    if (strncasecmp(line, "Content-Length:", 15) == 0) {
                        body = (size_t)strtoull(line + 15, NULL, 10);
                    }
                }
                want = (size_t)(end - request) + body;
            }
            if (want && length >= want) {
                break;
            }
        }
        for (size_t sent = 0; sent < length;) {
            ssize_t w = write(fds[1], request + sent, length - sent);
            if (w <= 0) {
                break;
            }
            sent += (size_t)w;
        }
        close(fds[1]);
        if (write(fd, reply, strlen(reply)) < 0) {
            _exit(1);
        }
        close(fd);
        free(request);
        _exit(0);
    }
    close(fds[1]);
    c.pipe_fd = fds[0];
    return c;
}

/** The request the server read, NUL-terminated (its body may hold NUL bytes too); free it. */
static char *captured_request(captured *c, size_t *length) {
    size_t capacity = 1 << 16;
    char *text = malloc(capacity + 1);
    *length = 0;
    for (;;) {
        if (*length == capacity) {
            capacity *= 2;
            text = realloc(text, capacity + 1);
        }
        ssize_t n = read(c->pipe_fd, text + *length, capacity - *length);
        if (n <= 0) {
            break;
        }
        *length += (size_t)n;
    }
    text[*length] = '\0';
    close(c->pipe_fd);
    server_close(&c->s);
    return text;
}

static bool has_header(const char *request, const char *name) {
    const char *end = strstr(request, "\r\n\r\n");
    size_t n = strlen(name);
    for (const char *line = strstr(request, "\r\n"); line && line < end; line = strstr(line + 2, "\r\n")) {
        if (strncasecmp(line + 2, name, n) == 0 && line[2 + n] == ':') {
            return true;
        }
    }
    return false;
}

static const char batch_body[] =
    "{\"id\":\"batch_1\",\"object\":\"batch\",\"endpoint\":\"/v1/chat/completions\",\"errors\":null,"
    "\"input_file_id\":\"file-1\",\"completion_window\":\"24h\",\"status\":\"completed\","
    "\"output_file_id\":\"file-2\",\"error_file_id\":null,\"created_at\":100,\"in_progress_at\":110,"
    "\"expires_at\":86500,\"finalizing_at\":null,\"completed_at\":200,\"failed_at\":null,\"expired_at\":null,"
    "\"cancelling_at\":null,\"cancelled_at\":null,"
    "\"request_counts\":{\"total\":3,\"completed\":2,\"failed\":1},\"metadata\":{\"name\":\"nightly\"},"
    "\"gateway\":{\"jobs\":[],\"cost\":[]}}";

static char *reply_with(const char *body) {
    size_t size = strlen(body) + 256;
    char *reply = malloc(size);
    snprintf(reply, size,
             "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Request-Id: req-1\r\nContent-Length: %zu\r\n"
             "Connection: close\r\n\r\n%s",
             strlen(body), body);
    return reply;
}

static void test_files(void) {
    char url[64];
    size_t length = 0;

    /* An upload is multipart, with purpose batch and the file's bytes as given. */
    char *reply = reply_with("{\"id\":\"file-1\",\"object\":\"file\",\"bytes\":13,\"created_at\":100,"
                             "\"filename\":\"in.jsonl\",\"purpose\":\"batch\"}");
    captured upload = serve_capture(reply);
    gw_client *client = client_for(url_of(&upload.s, url, sizeof url), 20000);
    const char data[] = "{\"a\":1}\n\0{}\n";
    gw_file *file = NULL;
    gw_error *error = NULL;
    CHECK(gw_file_upload(client, "in.jsonl", data, sizeof data - 1, NULL, &file, &error) == GW_OK);
    CHECK(error == NULL);
    CHECK(file && strcmp(gw_file_id(file), "file-1") == 0 && gw_file_bytes(file) == 13);
    CHECK(strcmp(gw_file_filename(file), "in.jsonl") == 0 && strcmp(gw_file_purpose(file), "batch") == 0);
    CHECK(gw_file_created_at(file) == 100 && strcmp(gw_file_request_id(file), "req-1") == 0);
    gw_file_free(file);
    char *request = captured_request(&upload, &length);
    CHECK(strncmp(request, "POST /api/inference/v1/files HTTP/1.1\r\n", 39) == 0);
    CHECK(strstr(request, "Content-Type: multipart/form-data; boundary=gw-") != NULL);
    CHECK(!has_header(request, "Idempotency-Key"));
    CHECK(strstr(request, "name=\"purpose\"\r\n\r\nbatch\r\n") != NULL);
    CHECK(strstr(request, "name=\"file\"; filename=\"in.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n") !=
          NULL);
    const char *body = strstr(request, "application/jsonl\r\n\r\n");
    CHECK(body && (size_t)(request + length - body) > sizeof data &&
          memcmp(body + 21, data, sizeof data - 1) == 0); /* NUL bytes and all */
    free(request);
    free(reply);
    gw_client_free(client);

    /* A download keeps every byte. */
    const char content[] = "HTTP/1.1 200 OK\r\nContent-Type: application/jsonl\r\nContent-Length: 7\r\n"
                           "Connection: close\r\n\r\n{}\n\0{}\n";
    char reply_bytes[sizeof content];
    memcpy(reply_bytes, content, sizeof content);
    /* serve_capture writes with strlen: send the NUL-holding reply through serve instead. */
    server download = serve(reply_bytes, sizeof content - 1, 0, 0);
    client = client_for(url_of(&download, url, sizeof url), 20000);
    gw_file_content *downloaded = NULL;
    CHECK(gw_file_download(client, "file-2", NULL, &downloaded, NULL) == GW_OK);
    const char *bytes = gw_file_content_data(downloaded, &length);
    CHECK(length == 7 && memcmp(bytes, "{}\n\0{}\n", 7) == 0);
    gw_file_content_free(downloaded);
    gw_client_free(client);
    server_close(&download);

    captured path = serve_capture("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    client = client_for(url_of(&path.s, url, sizeof url), 20000);
    CHECK(gw_file_download(client, "file-2", NULL, &downloaded, NULL) == GW_OK);
    CHECK(gw_file_content_data(downloaded, &length) && length == 0);
    gw_file_content_free(downloaded);
    request = captured_request(&path, &length);
    CHECK(strncmp(request, "GET /api/inference/v1/files/file-2/content HTTP/1.1\r\n", 53) == 0);
    free(request);

    /* Arguments are checked before anything is sent. */
    gw_file_content *none = NULL;
    CHECK(gw_file_download(client, "../admin", NULL, &none, NULL) == GW_E_CONFIG && none == NULL);
    CHECK(gw_file_download(client, "", NULL, &none, NULL) == GW_E_CONFIG);
    CHECK(gw_file_upload(client, "a\"b", "x", 1, NULL, &file, NULL) == GW_E_CONFIG && file == NULL);
    CHECK(gw_file_upload(client, "a\nb", "x", 1, NULL, &file, NULL) == GW_E_CONFIG);
    CHECK(gw_file_upload(client, "", "x", 1, NULL, &file, NULL) == GW_E_CONFIG);
    CHECK(gw_file_upload(client, "in.jsonl", NULL, 1, NULL, &file, NULL) == GW_E_CONFIG);
    error = NULL;
    CHECK(gw_file_upload(client, "in.jsonl", data, ((size_t)64 << 20) + 1, NULL, &file, &error) == GW_E_CONFIG);
    CHECK(error && strstr(gw_error_message(error), "at most"));
    gw_error_free(error);
    gw_client_free(client);
}

static void test_batches(void) {
    char url[64];
    size_t length = 0;

    /* Create: JSON with a completion window and metadata, and an Idempotency-Key. */
    char *reply = reply_with(batch_body);
    captured create = serve_capture(reply);
    gw_client *client = client_for(url_of(&create.s, url, sizeof url), 20000);
    gw_batch_request *spec = gw_batch_request_new("file-1", "/v1/chat/completions");
    CHECK(gw_batch_request_set_metadata(spec, "name", "first") == GW_OK);
    CHECK(gw_batch_request_set_metadata(spec, "name", "nightly") == GW_OK); /* replaced, not added */
    gw_call_options *options = gw_call_options_new();
    CHECK(gw_call_options_set_idempotency_key(options, "key-1") == GW_OK);
    gw_batch *batch = NULL;
    gw_error *error = NULL;
    CHECK(gw_batch_create(client, spec, options, &batch, &error) == GW_OK && error == NULL);
    CHECK(batch && strcmp(gw_batch_id(batch), "batch_1") == 0 && strcmp(gw_batch_status(batch), "completed") == 0);
    CHECK(strcmp(gw_batch_endpoint(batch), "/v1/chat/completions") == 0);
    CHECK(strcmp(gw_batch_completion_window(batch), "24h") == 0);
    CHECK(strcmp(gw_batch_input_file_id(batch), "file-1") == 0);
    CHECK(strcmp(gw_batch_output_file_id(batch), "file-2") == 0 && gw_batch_error_file_id(batch) == NULL);
    int64_t at = 0;
    CHECK(gw_batch_time_get(batch, GW_BATCH_CREATED_AT, &at) && at == 100);
    CHECK(gw_batch_time_get(batch, GW_BATCH_COMPLETED_AT, &at) && at == 200);
    CHECK(!gw_batch_time_get(batch, GW_BATCH_CANCELLED_AT, &at));
    CHECK(!gw_batch_time_get(batch, (gw_batch_time)99, &at));
    CHECK(gw_batch_count_get(batch, GW_BATCH_LINES_TOTAL) == 3);
    CHECK(gw_batch_count_get(batch, GW_BATCH_LINES_COMPLETED) == 2);
    CHECK(gw_batch_count_get(batch, GW_BATCH_LINES_FAILED) == 1);
    CHECK(strcmp(gw_batch_metadata(batch, "name"), "nightly") == 0 && gw_batch_metadata(batch, "x") == NULL);
    CHECK(strstr(gw_batch_json(batch, &length), "\"gateway\"") && length == sizeof batch_body - 1);
    CHECK(strcmp(gw_batch_request_id(batch), "req-1") == 0);
    gw_batch_free(batch);
    char *request = captured_request(&create, &length);
    CHECK(strncmp(request, "POST /api/inference/v1/batches HTTP/1.1\r\n", 41) == 0);
    CHECK(strstr(request, "Idempotency-Key: key-1\r\n") && strstr(request, "Content-Type: application/json\r\n"));
    CHECK(strstr(request, "{\"input_file_id\":\"file-1\",\"endpoint\":\"/v1/chat/completions\","
                          "\"completion_window\":\"24h\",\"metadata\":{\"name\":\"nightly\"}}"));
    free(request);
    gw_client_free(client);

    /* Retrieve is a GET; cancel a POST without a body or key. */
    captured get = serve_capture(reply);
    client = client_for(url_of(&get.s, url, sizeof url), 20000);
    CHECK(gw_batch_retrieve(client, "batch_1", NULL, &batch, NULL) == GW_OK && batch);
    gw_batch_free(batch);
    request = captured_request(&get, &length);
    CHECK(strncmp(request, "GET /api/inference/v1/batches/batch_1 HTTP/1.1\r\n", 48) == 0);
    free(request);
    gw_client_free(client);

    captured cancel = serve_capture(reply);
    client = client_for(url_of(&cancel.s, url, sizeof url), 20000);
    CHECK(gw_batch_cancel(client, "batch_1", NULL, &batch, NULL) == GW_OK && batch);
    gw_batch_free(batch);
    request = captured_request(&cancel, &length);
    CHECK(strncmp(request, "POST /api/inference/v1/batches/batch_1/cancel HTTP/1.1\r\n", 56) == 0);
    CHECK(!has_header(request, "Idempotency-Key") && !has_header(request, "Content-Type"));
    CHECK(strstr(request, "Content-Length: 0\r\n"));
    free(request);
    gw_client_free(client);
    free(reply);

    /* A page: borrowed batches, has_more and last_id. */
    size_t size = 2 * sizeof batch_body + 128;
    char *page_body = malloc(size);
    snprintf(page_body, size,
             "{\"object\":\"list\",\"data\":[%s,%s],\"first_id\":\"batch_1\",\"last_id\":\"batch_1\",\"has_more\":true}",
             batch_body, batch_body);
    reply = reply_with(page_body);
    captured list = serve_capture(reply);
    client = client_for(url_of(&list.s, url, sizeof url), 20000);
    gw_batch_page *page = NULL;
    CHECK(gw_batches_list(client, "batch_0", 2, NULL, &page, NULL) == GW_OK);
    CHECK(gw_batch_page_count(page) == 2 && gw_batch_page_has_more(page));
    CHECK(strcmp(gw_batch_page_last_id(page), "batch_1") == 0);
    CHECK(strcmp(gw_batch_page_request_id(page), "req-1") == 0);
    const gw_batch *item = gw_batch_page_item(page, 1);
    CHECK(item && strcmp(gw_batch_id(item), "batch_1") == 0 && gw_batch_request_id(item) == NULL);
    CHECK(gw_batch_json(item, &length) && length > 0 && strstr(gw_batch_json(item, NULL), "\"batch_1\""));
    CHECK(gw_batch_page_item(page, 2) == NULL);
    gw_batch_page_free(page);
    request = captured_request(&list, &length);
    CHECK(strncmp(request, "GET /api/inference/v1/batches?limit=2&after=batch_0 HTTP/1.1\r\n", 62) == 0);
    free(request);
    free(reply);
    free(page_body);
    gw_client_free(client);

    /* A problem surfaces with its code; a body off the contract is a decode error. */
    const char missing[] = "HTTP/1.1 404 Not Found\r\nContent-Type: application/problem+json\r\nContent-Length: 63\r\n"
                           "Connection: close\r\n\r\n"
                           "{\"type\":\"about:blank\",\"code\":\"batch_not_found\",\"status\":404}   ";
    server gone = serve(missing, sizeof missing - 1, 0, 0);
    client = client_for(url_of(&gone, url, sizeof url), 20000);
    error = NULL;
    CHECK(gw_batch_retrieve(client, "batch_2", NULL, &batch, &error) == GW_E_PROBLEM && batch == NULL);
    CHECK(error && strcmp(gw_error_code(error), "batch_not_found") == 0 && gw_error_http_status(error) == 404);
    gw_error_free(error);
    gw_client_free(client);
    server_close(&gone);

    reply = reply_with("{\"object\":\"batch\",\"status\":\"completed\"}");
    server odd = serve(reply, strlen(reply), 0, 0);
    client = client_for(url_of(&odd, url, sizeof url), 20000);
    error = NULL;
    CHECK(gw_batch_retrieve(client, "batch_1", NULL, &batch, &error) == GW_E_DECODE && batch == NULL);
    CHECK(error && strstr(gw_error_message(error), "id"));
    gw_error_free(error);
    gw_client_free(client);
    server_close(&odd);
    free(reply);

    /* Arguments. */
    client = client_for("http://127.0.0.1:9", 1000);
    CHECK(gw_batch_retrieve(client, "batch_1/../x", NULL, &batch, NULL) == GW_E_CONFIG && batch == NULL);
    CHECK(gw_batch_cancel(client, NULL, NULL, &batch, NULL) == GW_E_CONFIG);
    CHECK(gw_batches_list(client, NULL, 101, NULL, &page, NULL) == GW_E_CONFIG && page == NULL);
    CHECK(gw_batches_list(client, "a b", 0, NULL, &page, NULL) == GW_E_CONFIG);
    CHECK(gw_batch_create(client, NULL, NULL, &batch, NULL) == GW_E_CONFIG);
    CHECK(gw_batch_request_new(NULL, "/v1/embeddings") == NULL);
    CHECK(gw_batch_request_set_metadata(spec, "", "x") == GW_E_CONFIG);
    char key[80];
    memset(key, 'k', 65);
    key[65] = '\0';
    CHECK(gw_batch_request_set_metadata(spec, key, "x") == GW_E_CONFIG);
    for (int i = 1; i < 16; i++) {
        snprintf(key, sizeof key, "k%d", i);
        CHECK(gw_batch_request_set_metadata(spec, key, "x") == GW_OK);
    }
    CHECK(gw_batch_request_set_metadata(spec, "one-too-many", "x") == GW_E_CONFIG);
    CHECK(gw_batch_request_set_metadata(spec, "k1", "replaced") == GW_OK);
    gw_batch_request_free(spec);
    gw_call_options_free(options);
    gw_client_free(client);

    gw_file_free(NULL);
    gw_file_content_free(NULL);
    gw_batch_request_free(NULL);
    gw_batch_free(NULL);
    gw_batch_page_free(NULL);
    CHECK(gw_batch_id(NULL) == NULL && gw_batch_page_count(NULL) == 0 && gw_file_id(NULL) == NULL);
    CHECK(gw_file_content_data(NULL, &length) == NULL && length == 0);
}

int main(void) {
    gw_allocator counting = {count_malloc, count_realloc, count_free, NULL};
    if (gw_global_init(&counting) != GW_OK) {
        fprintf(stderr, "gw_global_init failed\n");
        return 1;
    }
    struct {
        const char *name;
        void (*run)(void);
    } tests[] = {
        {"sse_chunking", test_sse_chunking},
        {"sse_oversized", test_sse_oversized},
        {"config", test_config},
        {"null", test_null},
        {"decode", test_decode},
        {"timeout_and_cancel", test_timeout_and_cancel},
        {"body_limits", test_body_limits},
        {"tools", test_tools},
        {"files", test_files},
        {"batches", test_batches},
    };
    for (size_t i = 0; i < sizeof tests / sizeof *tests; i++) {
        int before = failures;
        tests[i].run();
        if (live_allocations != 0) {
            failures++;
            fprintf(stderr, "%s leaked %ld allocations\n", tests[i].name, live_allocations);
            live_allocations = 0;
        }
        printf("%s %s\n", failures == before ? "ok  " : "FAIL", tests[i].name);
    }
    gw_global_cleanup();
    printf("%s\n", failures ? "FAILED" : "all tests passed");
    return failures ? 1 : 0;
}
