/*
 * gateway.h: a C client for the Gateway inference API, contract 1.0.0.
 *
 * Chat, streaming chat, embeddings, and batch files and batches over HTTPS
 * (libcurl). The client is a transport wrapper: authorization, budgets,
 * routing and settlement stay on the gateway.
 *
 * ABI. Every type is opaque and used through pointers; structs are never
 * exposed except gw_allocator, whose layout is fixed. Status and enum values
 * never change meaning; new ones may be added, so treat an unknown negative
 * status as a failure and skip an unknown event type. Symbols are only ever
 * added within a major version (SONAME libqucelerate-ai-gateway.so.0).
 *
 * Ownership. A function named *_new or that returns an object through an out
 * parameter gives the caller ownership: free it with the matching *_free.
 * Every *_free accepts NULL. A `const char *` returned by an accessor is
 * borrowed: it stays valid until its owner is freed (an event: until the next
 * gw_stream_next or gw_stream_free). Strings passed in are copied.
 *
 * Errors. A fallible function returns a gw_status. When it fails and `error`
 * is not NULL, *error receives a gw_error the caller frees (or NULL when even
 * that allocation failed). On success *error is set to NULL.
 *
 * Threads. Call gw_global_init once, before any other thread uses the
 * library. A gw_client may be used from many threads at once. A request,
 * response, stream or options object is used by one thread at a time;
 * gw_cancel_trigger may be called from any thread.
 *
 * Retries. Chat, streams, embeddings and gw_batch_create send an
 * Idempotency-Key. Every call is retried automatically only when the gateway
 * refused it before reserving or doing anything
 * (rate_limit_exceeded, concurrency_limit_exceeded, provider_circuit_open),
 * waiting Retry-After (at most 60 seconds, 1 second when absent).
 *
 * TLS. Certificates are always verified, against the system roots or the CA
 * file or directory you set; there is no option to turn verification off.
 */
#ifndef GATEWAY_GATEWAY_H
#define GATEWAY_GATEWAY_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#  if defined(GW_BUILDING)
#    define GW_API __declspec(dllexport)
#  elif defined(GW_STATIC)
#    define GW_API
#  else
#    define GW_API __declspec(dllimport)
#  endif
#elif defined(__GNUC__)
#  define GW_API __attribute__((visibility("default")))
#else
#  define GW_API
#endif

#define GW_VERSION_MAJOR 2026
#define GW_VERSION_MINOR 11
#define GW_VERSION_PATCH 0
/* The same version as a string; CMakeLists.txt reads the version from here. */
#define GW_VERSION_STRING "2026.11.0"
/* Defined when this header declares the files and batches calls. */
#define GW_HAS_BATCHES 1

/* ---------------------------------------------------------------- status */

typedef enum gw_status {
    GW_OK = 0,
    /** gw_stream_next: the stream finished after response.completed. */
    GW_DONE = 1,
    /** An invalid argument or option. */
    GW_E_CONFIG = -1,
    /** An allocation failed. */
    GW_E_NOMEM = -2,
    /** The gateway returned an RFC 9457 problem; see gw_error_code. */
    GW_E_PROBLEM = -3,
    /** An error response without a problem body, for example from a proxy. */
    GW_E_HTTP = -4,
    /** A stream's terminal error event. Never retried automatically. */
    GW_E_STREAM = -5,
    /** A stream closed before its terminal event, or sent one over 16 MiB. */
    GW_E_PROTOCOL = -6,
    /** A response body or stream event did not match the contract. */
    GW_E_DECODE = -7,
    /** The request could not be sent or the response could not be read. */
    GW_E_TRANSPORT = -8,
    /** An attempt (or a stream's start) passed the client's timeout. */
    GW_E_TIMEOUT = -9,
    /** The call's gw_cancel was triggered. */
    GW_E_CANCELLED = -10
} gw_status;

/** A static, human-readable name for a status. */
GW_API const char *gw_status_name(gw_status status);

/** The library version, such as "0.1.0". */
GW_API const char *gw_version(void);

/* ------------------------------------------------------------ allocation */

/**
 * The functions this library allocates its own memory with. libcurl keeps
 * using its own allocator. The layout of this struct is fixed.
 */
typedef struct gw_allocator {
    void *(*malloc_fn)(void *ctx, size_t size);
    void *(*realloc_fn)(void *ctx, void *ptr, size_t size);
    void (*free_fn)(void *ctx, void *ptr);
    void *ctx;
} gw_allocator;

/**
 * Initialize the library (and libcurl) once per process, before other threads
 * use it. `allocator` may be NULL for malloc/realloc/free; it is copied.
 */
GW_API gw_status gw_global_init(const gw_allocator *allocator);

/** Release what gw_global_init acquired, after every object is freed. */
GW_API void gw_global_cleanup(void);

/* ----------------------------------------------------------------- error */

typedef struct gw_error gw_error;

GW_API gw_status gw_error_status(const gw_error *error);
GW_API const char *gw_error_message(const gw_error *error);
/** The HTTP status of a problem, error response or stream error; 0 if none. */
GW_API int gw_error_http_status(const gw_error *error);
/** A problem's or stream error's code (an open string), or NULL. */
GW_API const char *gw_error_code(const gw_error *error);
GW_API const char *gw_error_title(const gw_error *error);
GW_API const char *gw_error_detail(const gw_error *error);
/** The problem's request_id, else the X-Request-Id header, or NULL. */
GW_API const char *gw_error_request_id(const gw_error *error);
/** The Retry-After header in whole seconds, or -1 when the response had none. */
GW_API int64_t gw_error_retry_after(const gw_error *error);
GW_API void gw_error_free(gw_error *error);

/* ---------------------------------------------------------- cancellation */

/** A cancellation flag for one or more calls. */
typedef struct gw_cancel gw_cancel;

GW_API gw_cancel *gw_cancel_new(void);
/** Cancel every call using this flag. Safe from any thread. */
GW_API void gw_cancel_trigger(gw_cancel *cancel);
GW_API bool gw_cancel_triggered(const gw_cancel *cancel);
GW_API void gw_cancel_free(gw_cancel *cancel);

/* ---------------------------------------------------------------- client */

typedef struct gw_client_options gw_client_options;
typedef struct gw_client gw_client;

/**
 * Waits before a retry; returns false to stop waiting because the call was
 * cancelled. `cancel` is the call's flag or NULL.
 */
typedef bool (*gw_sleep_fn)(void *ctx, int64_t milliseconds, const gw_cancel *cancel);

GW_API gw_client_options *gw_client_options_new(void);
/** The gateway, such as https://gateway.example.com. Required. */
GW_API gw_status gw_client_options_set_base_url(gw_client_options *options, const char *base_url);
/** The gw_ API key sent as the bearer credential. Required. */
GW_API gw_status gw_client_options_set_api_key(gw_client_options *options, const char *api_key);
/** Automatic same-key retries (default 2). */
GW_API gw_status gw_client_options_set_max_retries(gw_client_options *options, int max_retries);
/**
 * The limit for each attempt (default 120000 ms): a non-streaming call's whole
 * response, or the start of a stream's. There is no limit between events.
 */
GW_API gw_status gw_client_options_set_timeout_ms(gw_client_options *options, int64_t timeout_ms);
/** A PEM file of CA certificates to trust instead of the system roots. */
GW_API gw_status gw_client_options_set_ca_file(gw_client_options *options, const char *path);
/** A directory of hashed CA certificates to trust instead of the system roots. */
GW_API gw_status gw_client_options_set_ca_path(gw_client_options *options, const char *path);
/** Replace how the client waits before a retry (default: a cancellable sleep). */
GW_API gw_status gw_client_options_set_sleep(gw_client_options *options, gw_sleep_fn sleep, void *ctx);
GW_API void gw_client_options_free(gw_client_options *options);

/** A client for the options' gateway; the options are copied. */
GW_API gw_status gw_client_new(const gw_client_options *options, gw_client **out, gw_error **error);
GW_API void gw_client_free(gw_client *client);

/* --------------------------------------------------------- call options */

typedef struct gw_call_options gw_call_options;

GW_API gw_call_options *gw_call_options_new(void);
/**
 * The call's Idempotency-Key (1 to 255 characters). Unset, the client
 * generates one per call and reuses it for that call's retries.
 */
GW_API gw_status gw_call_options_set_idempotency_key(gw_call_options *options, const char *key);
/** An X-Request-Id (at most 128 characters) to correlate the call with your logs. */
GW_API gw_status gw_call_options_set_request_id(gw_call_options *options, const char *request_id);
/** A borrowed cancellation flag; it must outlive every call that uses it. */
GW_API gw_status gw_call_options_set_cancel(gw_call_options *options, gw_cancel *cancel);
GW_API void gw_call_options_free(gw_call_options *options);

/* --------------------------------------------------------------- requests */

typedef struct gw_chat_request gw_chat_request;
typedef struct gw_embedding_request gw_embedding_request;

GW_API gw_chat_request *gw_chat_request_new(const char *model);
/** Append a message; `role` is system, developer, user, assistant or tool. */
GW_API gw_status gw_chat_request_add_message(gw_chat_request *request, const char *role, const char *content);
/** gw_chat_request_add_message with content of `length` bytes, which may include NUL bytes. */
GW_API gw_status gw_chat_request_add_message_n(gw_chat_request *request, const char *role, const char *content,
                                               size_t length);
/** 0 to 2; unset uses the model's default. */
GW_API gw_status gw_chat_request_set_temperature(gw_chat_request *request, double temperature);
/** 1 to 4096; unset uses the model's default. */
GW_API gw_status gw_chat_request_set_max_tokens(gw_chat_request *request, int max_tokens);
/**
 * Define a function the model may call. `description` may be NULL;
 * `parameters_json` is a JSON Schema object, or NULL for none (the gateway
 * takes an empty object). GW_E_CONFIG when it is not a JSON object. A model
 * alias that cannot take tools refuses the call (tools_unsupported).
 */
GW_API gw_status gw_chat_request_add_tool(gw_chat_request *request, const char *name, const char *description,
                                          const char *parameters_json);
/** "auto", "none" or "required"; needs tools. */
GW_API gw_status gw_chat_request_set_tool_choice(gw_chat_request *request, const char *mode);
/** Make the model call the function `name`, one of the request's tools. */
GW_API gw_status gw_chat_request_set_tool_choice_function(gw_chat_request *request, const char *name);
/** false asks for at most one call per response; needs tools. */
GW_API gw_status gw_chat_request_set_parallel_tool_calls(gw_chat_request *request, bool parallel);
/**
 * Append a call the model made to the last message, which must be an
 * assistant message (GW_E_CONFIG otherwise): send a response's calls back
 * unchanged before their results. An empty content is then sent as null.
 */
GW_API gw_status gw_chat_request_add_tool_call(gw_chat_request *request, const char *id, const char *name,
                                               const char *arguments);
/** Append a tool message: the result of the call `tool_call_id`. */
GW_API gw_status gw_chat_request_add_tool_result(gw_chat_request *request, const char *tool_call_id,
                                                 const char *content);
/** gw_chat_request_add_tool_result with content of `length` bytes, which may include NUL bytes. */
GW_API gw_status gw_chat_request_add_tool_result_n(gw_chat_request *request, const char *tool_call_id,
                                                   const char *content, size_t length);
GW_API void gw_chat_request_free(gw_chat_request *request);

GW_API gw_embedding_request *gw_embedding_request_new(const char *model);
/** Append one input string (1 to 128 inputs per call). */
GW_API gw_status gw_embedding_request_add_input(gw_embedding_request *request, const char *input);
/** gw_embedding_request_add_input with `length` bytes, which may include NUL bytes. */
GW_API gw_status gw_embedding_request_add_input_n(gw_embedding_request *request, const char *input, size_t length);
GW_API void gw_embedding_request_free(gw_embedding_request *request);

/* ----------------------------------------------------------------- usage */

/** A borrowed view of token usage, owned by its response or event. */
typedef struct gw_usage gw_usage;

typedef enum gw_usage_counter {
    GW_USAGE_INPUT_TOKENS = 0,
    GW_USAGE_OUTPUT_TOKENS = 1,
    GW_USAGE_CACHED_INPUT_TOKENS = 2,
    /** Counted apart from output tokens; unreported means it is inside them. */
    GW_USAGE_REASONING_TOKENS = 3,
    GW_USAGE_OTHER_TOKENS = 4
} gw_usage_counter;

/** True and *out set when the provider reported the counter; never 0 for unreported. */
GW_API bool gw_usage_get(const gw_usage *usage, gw_usage_counter counter, int64_t *out);
/** exact, estimated or unknown; treat other values as unknown. */
GW_API const char *gw_usage_quality(const gw_usage *usage);
/** complete, partial or unavailable; treat other values as unknown. */
GW_API const char *gw_usage_completeness(const gw_usage *usage);

/* ------------------------------------------------------------------ chat */

typedef struct gw_chat_response gw_chat_response;

GW_API gw_status gw_chat(gw_client *client, const gw_chat_request *request, const gw_call_options *options,
                         gw_chat_response **out, gw_error **error);

GW_API const char *gw_chat_response_id(const gw_chat_response *response);
GW_API const char *gw_chat_response_model(const gw_chat_response *response);
/** The X-Request-Id response header, or NULL: quote it when reporting a problem. */
GW_API const char *gw_chat_response_request_id(const gw_chat_response *response);
GW_API size_t gw_chat_response_choice_count(const gw_chat_response *response);
GW_API int64_t gw_chat_response_choice_index(const gw_chat_response *response, size_t choice);
GW_API const char *gw_chat_response_choice_role(const gw_chat_response *response, size_t choice);
/**
 * The message content, or NULL when the model returned none. Strings that
 * carry model text are NUL-terminated and may also contain NUL bytes: pass
 * `length` (or NULL) to get their full length.
 */
GW_API const char *gw_chat_response_choice_content(const gw_chat_response *response, size_t choice,
                                                   size_t *length);
/** An open string (stop, length, tool_calls, ...), or NULL. */
GW_API const char *gw_chat_response_choice_finish_reason(const gw_chat_response *response, size_t choice);
GW_API const char *gw_chat_response_choice_refusal(const gw_chat_response *response, size_t choice,
                                                   size_t *length);
/** The calls the model made in this choice; 0 when it made none. */
GW_API size_t gw_chat_response_choice_tool_call_count(const gw_chat_response *response, size_t choice);
/**
 * One call: its id, function name and arguments (a JSON object, serialized,
 * as the model wrote it: validate it before acting on it). Any out pointer
 * may be NULL. False when there is no such call.
 */
GW_API bool gw_chat_response_choice_tool_call(const gw_chat_response *response, size_t choice, size_t index,
                                              const char **id, const char **name, const char **arguments);
/** The usage, or NULL when the gateway reported none. */
GW_API const gw_usage *gw_chat_response_usage(const gw_chat_response *response);
/** The raw JSON body, for fields this version does not model (tool calls, safety). */
GW_API const char *gw_chat_response_json(const gw_chat_response *response, size_t *length);
GW_API void gw_chat_response_free(gw_chat_response *response);

/* ------------------------------------------------------------ embeddings */

typedef struct gw_embedding_response gw_embedding_response;

GW_API gw_status gw_embeddings(gw_client *client, const gw_embedding_request *request,
                               const gw_call_options *options, gw_embedding_response **out, gw_error **error);

GW_API const char *gw_embedding_response_model(const gw_embedding_response *response);
GW_API const char *gw_embedding_response_request_id(const gw_embedding_response *response);
/** One vector per input, in input order. */
GW_API size_t gw_embedding_response_count(const gw_embedding_response *response);
GW_API int64_t gw_embedding_response_index(const gw_embedding_response *response, size_t item);
/** The vector of `item`, with its length in *dimensions. */
GW_API const float *gw_embedding_response_vector(const gw_embedding_response *response, size_t item,
                                                 size_t *dimensions);
GW_API const gw_usage *gw_embedding_response_usage(const gw_embedding_response *response);
GW_API void gw_embedding_response_free(gw_embedding_response *response);

/* --------------------------------------------------------------- streams */

typedef struct gw_stream gw_stream;
typedef struct gw_event gw_event;

typedef enum gw_event_type {
    GW_EVENT_RESPONSE_STARTED = 1,
    GW_EVENT_CHOICE_DELTA = 2,
    GW_EVENT_TOOL_CALL_DELTA = 3,
    GW_EVENT_CHOICE_FINISHED = 4,
    GW_EVENT_SAFETY_ANNOTATION = 5,
    GW_EVENT_USAGE = 6,
    GW_EVENT_RESPONSE_COMPLETED = 7
} gw_event_type;

/**
 * Open a streamed chat completion. The response must start within the
 * client's timeout. Read it with gw_stream_next; free it to close it.
 */
GW_API gw_status gw_chat_stream_open(gw_client *client, const gw_chat_request *request,
                                     const gw_call_options *options, gw_stream **out, gw_error **error);

/**
 * The next event: GW_OK with *event set (borrowed until the next call),
 * GW_DONE after response.completed, or a failure (GW_E_STREAM for a terminal
 * error event, GW_E_PROTOCOL for an early close, ...) once the events before it
 * were returned. Events this version does not know are skipped.
 */
GW_API gw_status gw_stream_next(gw_stream *stream, const gw_event **event, gw_error **error);
/** The X-Request-Id response header, or NULL. */
GW_API const char *gw_stream_request_id(const gw_stream *stream);
/** Close the connection and free the stream. */
GW_API void gw_stream_free(gw_stream *stream);

GW_API gw_event_type gw_event_get_type(const gw_event *event);
/** The SSE event name, such as "choice.delta". */
GW_API const char *gw_event_name(const gw_event *event);
/** The event's raw JSON data. */
GW_API const char *gw_event_json(const gw_event *event, size_t *length);
/** response.started, response.completed: the model. */
GW_API const char *gw_event_model(const gw_event *event);
/** choice.delta, tool_call.delta, choice.finished, safety.annotation; else -1. */
GW_API int64_t gw_event_choice_index(const gw_event *event);
/** choice.delta: more content, or NULL. */
GW_API const char *gw_event_content_delta(const gw_event *event, size_t *length);
/** choice.delta: more refusal text, or NULL. */
GW_API const char *gw_event_refusal_delta(const gw_event *event, size_t *length);
/** tool_call.delta: the tool call's index; else -1. */
GW_API int64_t gw_event_tool_call_index(const gw_event *event);
/** tool_call.delta: the call id and tool name, in the first delta only, else NULL. */
GW_API const char *gw_event_tool_call_id(const gw_event *event);
GW_API const char *gw_event_tool_name(const gw_event *event);
/** tool_call.delta: more of the arguments JSON. */
GW_API const char *gw_event_arguments_delta(const gw_event *event, size_t *length);
/** choice.finished: an open string such as stop. */
GW_API const char *gw_event_finish_reason(const gw_event *event);
/** safety.annotation: the category and whether the provider blocked output. */
GW_API const char *gw_event_safety_category(const gw_event *event);
GW_API bool gw_event_safety_blocked(const gw_event *event);
/** usage, response.completed: the usage, or NULL. */
GW_API const gw_usage *gw_event_usage(const gw_event *event);

/* ---------------------------------------------------- files and batches */

/*
 * Batches (OpenAI's Batch API shape): upload a JSONL file of requests, create
 * a batch from it, follow it, and download its output and error files. Each
 * line is accounted as a request of its own, at the batch price.
 */

/** An uploaded file. */
typedef struct gw_file gw_file;

/**
 * Upload a batch input file: `length` bytes of JSONL (at most 64 MiB), one
 * request per line, sent as multipart form data with purpose "batch".
 * `filename` (1 to 255 bytes, no control characters, quotes or backslashes)
 * names it. No Idempotency-Key is sent.
 */
GW_API gw_status gw_file_upload(gw_client *client, const char *filename, const void *data, size_t length,
                                const gw_call_options *options, gw_file **out, gw_error **error);
/** The file id, such as "file-…": the input of gw_batch_request_new. */
GW_API const char *gw_file_id(const gw_file *file);
GW_API const char *gw_file_filename(const gw_file *file);
/** An open string such as "batch" or "batch_output". */
GW_API const char *gw_file_purpose(const gw_file *file);
GW_API int64_t gw_file_bytes(const gw_file *file);
/** Unix seconds. */
GW_API int64_t gw_file_created_at(const gw_file *file);
GW_API const char *gw_file_request_id(const gw_file *file);
GW_API void gw_file_free(gw_file *file);

/** A downloaded file's bytes. */
typedef struct gw_file_content gw_file_content;

/**
 * Download a file's content (at most 256 MiB), such as a batch's output or
 * error file: JSONL, one result per line, matched to its request by
 * custom_id.
 */
GW_API gw_status gw_file_download(gw_client *client, const char *file_id, const gw_call_options *options,
                                  gw_file_content **out, gw_error **error);
/** The bytes, NUL-terminated (they may also contain NUL bytes); `length` may be NULL. */
GW_API const char *gw_file_content_data(const gw_file_content *content, size_t *length);
GW_API const char *gw_file_content_request_id(const gw_file_content *content);
GW_API void gw_file_content_free(gw_file_content *content);

/** What gw_batch_create sends. */
typedef struct gw_batch_request gw_batch_request;

/**
 * A batch of the uploaded file's lines, for `endpoint`: "/v1/chat/completions"
 * or "/v1/embeddings". The completion window is "24h".
 */
GW_API gw_batch_request *gw_batch_request_new(const char *input_file_id, const char *endpoint);
/** Set a metadata entry (at most 16; keys up to 64 characters, values up to 512). */
GW_API gw_status gw_batch_request_set_metadata(gw_batch_request *request, const char *key, const char *value);
GW_API void gw_batch_request_free(gw_batch_request *request);

typedef struct gw_batch gw_batch;

/** Moments in a batch's life, in Unix seconds. */
typedef enum gw_batch_time {
    GW_BATCH_CREATED_AT = 0,
    GW_BATCH_IN_PROGRESS_AT = 1,
    GW_BATCH_EXPIRES_AT = 2,
    GW_BATCH_FINALIZING_AT = 3,
    GW_BATCH_COMPLETED_AT = 4,
    GW_BATCH_FAILED_AT = 5,
    GW_BATCH_EXPIRED_AT = 6,
    GW_BATCH_CANCELLING_AT = 7,
    GW_BATCH_CANCELLED_AT = 8
} gw_batch_time;

/** A batch's line counts. */
typedef enum gw_batch_count {
    GW_BATCH_LINES_TOTAL = 0,
    GW_BATCH_LINES_COMPLETED = 1,
    GW_BATCH_LINES_FAILED = 2
} gw_batch_count;

/**
 * Create a batch. The whole file is validated and every line reserved
 * against budgets before the call returns; a refused file is a problem
 * (batch_validation_failed, whose body lists the refused lines). Sends an
 * Idempotency-Key: a retry with the same key returns the same batch.
 */
GW_API gw_status gw_batch_create(gw_client *client, const gw_batch_request *request, const gw_call_options *options,
                                 gw_batch **out, gw_error **error);
/** Read a batch, with the gateway's account of its jobs and cost (see gw_batch_json). */
GW_API gw_status gw_batch_retrieve(gw_client *client, const char *batch_id, const gw_call_options *options,
                                   gw_batch **out, gw_error **error);
/** Cancel a batch: lines not yet run end without a charge. Returns the batch as it now is. */
GW_API gw_status gw_batch_cancel(gw_client *client, const char *batch_id, const gw_call_options *options,
                                 gw_batch **out, gw_error **error);

/** The batch id, such as "batch_…". */
GW_API const char *gw_batch_id(const gw_batch *batch);
/** An open string: validating, in_progress, finalizing, completed, failed, expired, cancelling, cancelled, ... */
GW_API const char *gw_batch_status(const gw_batch *batch);
GW_API const char *gw_batch_endpoint(const gw_batch *batch);
GW_API const char *gw_batch_completion_window(const gw_batch *batch);
GW_API const char *gw_batch_input_file_id(const gw_batch *batch);
/** The output and error files, or NULL until there are results. */
GW_API const char *gw_batch_output_file_id(const gw_batch *batch);
GW_API const char *gw_batch_error_file_id(const gw_batch *batch);
/** True and *out set when the batch has reached that moment. */
GW_API bool gw_batch_time_get(const gw_batch *batch, gw_batch_time moment, int64_t *out);
GW_API int64_t gw_batch_count_get(const gw_batch *batch, gw_batch_count count);
/** A metadata value, or NULL. */
GW_API const char *gw_batch_metadata(const gw_batch *batch, const char *key);
/** The batch's raw JSON, for fields this version does not model (errors, the gateway account). */
GW_API const char *gw_batch_json(const gw_batch *batch, size_t *length);
/** The X-Request-Id of the call that returned the batch, or NULL (NULL for a page's batches). */
GW_API const char *gw_batch_request_id(const gw_batch *batch);
GW_API void gw_batch_free(gw_batch *batch);

/** A page of batches, newest first. */
typedef struct gw_batch_page gw_batch_page;

/**
 * List the batches of the key's service account: `limit` (1 to 100, or 0 for
 * the gateway's 20) after the batch `after` (NULL for the first page).
 */
GW_API gw_status gw_batches_list(gw_client *client, const char *after, int limit, const gw_call_options *options,
                                 gw_batch_page **out, gw_error **error);
GW_API size_t gw_batch_page_count(const gw_batch_page *page);
/** A batch of the page, borrowed until the page is freed; never pass it to gw_batch_free. */
GW_API const gw_batch *gw_batch_page_item(const gw_batch_page *page, size_t index);
/** Whether another page follows: ask for it with `after` = gw_batch_page_last_id. */
GW_API bool gw_batch_page_has_more(const gw_batch_page *page);
GW_API const char *gw_batch_page_last_id(const gw_batch_page *page);
GW_API const char *gw_batch_page_request_id(const gw_batch_page *page);
GW_API void gw_batch_page_free(gw_batch_page *page);

#ifdef __cplusplus
}
#endif

#endif /* GATEWAY_GATEWAY_H */
