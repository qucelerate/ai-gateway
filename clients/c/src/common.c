/* Allocation, buffers, errors, status names and the cancellation flag. */
#include "internal.h"

#include <curl/curl.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void *default_malloc(void *ctx, size_t size) {
    (void)ctx;
    return malloc(size);
}

static void *default_realloc(void *ctx, void *ptr, size_t size) {
    (void)ctx;
    return realloc(ptr, size);
}

static void default_free(void *ctx, void *ptr) {
    (void)ctx;
    free(ptr);
}

static gw_allocator allocator = {default_malloc, default_realloc, default_free, NULL};
static bool initialized;

static void *json_malloc(void *ctx, size_t size) {
    (void)ctx;
    return gw_malloc(size);
}

static void *json_realloc(void *ctx, void *ptr, size_t old_size, size_t size) {
    (void)ctx;
    (void)old_size;
    return gw_realloc(ptr, size);
}

static void json_free(void *ctx, void *ptr) {
    (void)ctx;
    gw_free(ptr);
}

static const yyjson_alc json_allocator = {json_malloc, json_realloc, json_free, NULL};

GW_API gw_status gw_global_init(const gw_allocator *custom) {
    if (initialized) {
        return GW_OK;
    }
    if (custom) {
        if (!custom->malloc_fn || !custom->realloc_fn || !custom->free_fn) {
            return GW_E_CONFIG;
        }
        allocator = *custom;
    }
    if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) {
        return GW_E_TRANSPORT;
    }
    initialized = true;
    return GW_OK;
}

GW_API void gw_global_cleanup(void) {
    if (initialized) {
        curl_global_cleanup();
        initialized = false;
    }
}

bool gw_initialized(void) { return initialized; }

const yyjson_alc *gw_json_allocator(void) { return &json_allocator; }

void *gw_malloc(size_t size) { return allocator.malloc_fn(allocator.ctx, size ? size : 1); }

void *gw_calloc(size_t count, size_t size) {
    if (size && count > SIZE_MAX / size) {
        return NULL;
    }
    void *memory = gw_malloc(count * size);
    if (memory) {
        memset(memory, 0, count * size);
    }
    return memory;
}

void *gw_realloc(void *ptr, size_t size) { return allocator.realloc_fn(allocator.ctx, ptr, size ? size : 1); }

void gw_free(void *ptr) {
    if (ptr) {
        allocator.free_fn(allocator.ctx, ptr);
    }
}

char *gw_strndup(const char *text, size_t length) {
    char *copy = gw_malloc(length + 1);
    if (copy) {
        memcpy(copy, text, length);
        copy[length] = '\0';
    }
    return copy;
}

char *gw_strdup(const char *text) { return text ? gw_strndup(text, strlen(text)) : NULL; }

bool gw_buffer_append(gw_buffer *buffer, const void *bytes, size_t count) {
    if (buffer->length + count + 1 > buffer->capacity) {
        size_t capacity = buffer->capacity ? buffer->capacity : 64;
        while (capacity < buffer->length + count + 1) {
            if (capacity > SIZE_MAX / 2) {
                return false;
            }
            capacity *= 2;
        }
        char *data = gw_realloc(buffer->data, capacity);
        if (!data) {
            return false;
        }
        buffer->data = data;
        buffer->capacity = capacity;
    }
    if (count) {
        memcpy(buffer->data + buffer->length, bytes, count);
    }
    buffer->length += count;
    buffer->data[buffer->length] = '\0';
    return true;
}

void gw_buffer_clear(gw_buffer *buffer) {
    buffer->length = 0;
    if (buffer->data) {
        buffer->data[0] = '\0';
    }
}

void gw_buffer_free(gw_buffer *buffer) {
    gw_free(buffer->data);
    buffer->data = NULL;
    buffer->length = buffer->capacity = 0;
}

struct gw_error {
    gw_status status;
    char *message;
    int http_status;
    char *code;
    char *title;
    char *detail;
    char *request_id;
    int64_t retry_after;
};

gw_error *gw_error_make(gw_status status, const char *message) {
    gw_error *error = gw_calloc(1, sizeof *error);
    if (!error) {
        return NULL;
    }
    error->status = status;
    error->message = gw_strdup(message);
    error->retry_after = -1;
    if (!error->message) {
        gw_free(error);
        return NULL;
    }
    return error;
}

void gw_error_set_http(gw_error *error, int http_status, const char *code, const char *title, const char *detail,
                       const char *request_id, int64_t retry_after) {
    if (!error) {
        return;
    }
    error->http_status = http_status;
    error->code = gw_strdup(code);
    error->title = gw_strdup(title);
    error->detail = gw_strdup(detail);
    error->request_id = gw_strdup(request_id);
    error->retry_after = retry_after;
}

gw_status gw_fail(gw_error **error, gw_status status, const char *format, ...) {
    if (error) {
        char message[512];
        va_list arguments;
        va_start(arguments, format);
        vsnprintf(message, sizeof message, format, arguments);
        va_end(arguments);
        *error = gw_error_make(status, message);
    }
    return status;
}

GW_API gw_status gw_error_status(const gw_error *error) { return error ? error->status : GW_OK; }
GW_API const char *gw_error_message(const gw_error *error) { return error ? error->message : NULL; }
GW_API int gw_error_http_status(const gw_error *error) { return error ? error->http_status : 0; }
GW_API const char *gw_error_code(const gw_error *error) { return error ? error->code : NULL; }
GW_API const char *gw_error_title(const gw_error *error) { return error ? error->title : NULL; }
GW_API const char *gw_error_detail(const gw_error *error) { return error ? error->detail : NULL; }
GW_API const char *gw_error_request_id(const gw_error *error) { return error ? error->request_id : NULL; }
GW_API int64_t gw_error_retry_after(const gw_error *error) { return error ? error->retry_after : -1; }

GW_API void gw_error_free(gw_error *error) {
    if (!error) {
        return;
    }
    gw_free(error->message);
    gw_free(error->code);
    gw_free(error->title);
    gw_free(error->detail);
    gw_free(error->request_id);
    gw_free(error);
}

GW_API const char *gw_status_name(gw_status status) {
    switch (status) {
    case GW_OK: return "ok";
    case GW_DONE: return "done";
    case GW_E_CONFIG: return "config";
    case GW_E_NOMEM: return "nomem";
    case GW_E_PROBLEM: return "problem";
    case GW_E_HTTP: return "http";
    case GW_E_STREAM: return "stream";
    case GW_E_PROTOCOL: return "protocol";
    case GW_E_DECODE: return "decode";
    case GW_E_TRANSPORT: return "transport";
    case GW_E_TIMEOUT: return "timeout";
    case GW_E_CANCELLED: return "cancelled";
    }
    return "unknown";
}

GW_API const char *gw_version(void) { return GW_VERSION_STRING; }

struct gw_cancel {
    atomic_bool triggered;
};

GW_API gw_cancel *gw_cancel_new(void) {
    gw_cancel *cancel = gw_malloc(sizeof *cancel);
    if (cancel) {
        atomic_init(&cancel->triggered, false);
    }
    return cancel;
}

GW_API void gw_cancel_trigger(gw_cancel *cancel) {
    if (cancel) {
        atomic_store(&cancel->triggered, true);
    }
}

GW_API bool gw_cancel_triggered(const gw_cancel *cancel) {
    return cancel && atomic_load(&((gw_cancel *)cancel)->triggered);
}

GW_API void gw_cancel_free(gw_cancel *cancel) { gw_free(cancel); }
