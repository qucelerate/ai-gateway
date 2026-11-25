/*
 * Files and batches (OpenAI's Batch API shape): a multipart upload, JSON
 * calls for batches, and downloads of result files. The transport is
 * client.c's, with its retries, deadline and cancellation.
 */
#include "objects.h"

#include <stdio.h>
#include <string.h>

#define FILES_PATH "/api/inference/v1/files"
#define BATCHES_PATH "/api/inference/v1/batches"
#define MAX_UPLOAD_BYTES ((size_t)64 << 20)
#define MAX_DOWNLOAD_BYTES ((size_t)256 << 20)
#define MAX_ID_CHARS 128
#define MAX_FILENAME_BYTES 255
#define MAX_METADATA_ENTRIES 16
#define MAX_METADATA_KEY_CHARS 64
#define MAX_METADATA_VALUE_CHARS 512
#define BATCH_TIMES 9
#define BATCH_COUNTS 3

static const char *const time_keys[BATCH_TIMES] = {
    "created_at",   "in_progress_at", "expires_at",    "finalizing_at", "completed_at",
    "failed_at",    "expired_at",     "cancelling_at", "cancelled_at",
};
static const char *const count_keys[BATCH_COUNTS] = {"total", "completed", "failed"};

static void clear_error(gw_error **error) {
    if (error) {
        *error = NULL;
    }
}

/** An id the gateway gave out: 1 to 128 of [A-Za-z0-9_-], so it is safe in a path or query. */
static bool valid_id(const char *id) {
    if (!id || !*id) {
        return false;
    }
    size_t n = 0;
    for (const char *c = id; *c; c++, n++) {
        bool ok = (*c >= 'a' && *c <= 'z') || (*c >= 'A' && *c <= 'Z') || (*c >= '0' && *c <= '9') || *c == '_' ||
                  *c == '-';
        if (!ok || n >= MAX_ID_CHARS) {
            return false;
        }
    }
    return true;
}

/** UTF-8 code points in a string. */
static size_t code_points(const char *text) {
    size_t n = 0;
    for (const unsigned char *c = (const unsigned char *)text; *c; c++) {
        n += (*c & 0xC0) != 0x80;
    }
    return n;
}

/* --------------------------------------------------------------- decoding */

typedef struct check {
    char *message;
    size_t size;
    gw_status status;
} check;

static bool fail(check *c, const char *what) {
    if (c->status == GW_OK) {
        c->status = GW_E_DECODE;
        snprintf(c->message, c->size, "%s", what);
    }
    return false;
}

static yyjson_doc *parse(const char *body, size_t length, check *c) {
    yyjson_read_err error;
    yyjson_doc *doc = yyjson_read_opts((char *)body, length, YYJSON_READ_NOFLAG, gw_json_allocator(), &error);
    if (!doc) {
        if (error.code == YYJSON_READ_ERROR_MEMORY_ALLOCATION) {
            c->status = GW_E_NOMEM;
            snprintf(c->message, c->size, "out of memory");
        } else {
            char text[160];
            snprintf(text, sizeof text, "the response is not JSON: %s at %zu", error.msg, error.pos);
            fail(c, text);
        }
    }
    return doc;
}

/** A string member: required, or absent or null when not. */
static bool str_field(check *c, yyjson_val *object, const char *key, bool required, const char **out) {
    yyjson_val *value = yyjson_obj_get(object, key);
    *out = NULL;
    if (yyjson_is_str(value)) {
        *out = yyjson_get_str(value);
        return true;
    }
    if (!required && (!value || yyjson_is_null(value))) {
        return true;
    }
    char text[96];
    snprintf(text, sizeof text, "%s must be a string", key);
    return fail(c, text);
}

/** An integer member: required, or absent or null when not. */
static bool int_field(check *c, yyjson_val *object, const char *key, bool required, bool *present, int64_t *out) {
    yyjson_val *value = yyjson_obj_get(object, key);
    *present = false;
    *out = 0;
    if (yyjson_is_sint(value)) {
        *out = yyjson_get_sint(value);
        *present = true;
        return true;
    }
    if (yyjson_is_uint(value) && yyjson_get_uint(value) <= (uint64_t)INT64_MAX) {
        *out = (int64_t)yyjson_get_uint(value);
        *present = true;
        return true;
    }
    if (!required && (!value || yyjson_is_null(value))) {
        return true;
    }
    char text[96];
    snprintf(text, sizeof text, "%s must be an integer", key);
    return fail(c, text);
}

/* ------------------------------------------------------------------ files */

struct gw_file {
    yyjson_doc *doc;
    char *request_id;
    const char *id;
    const char *filename;
    const char *purpose;
    int64_t bytes;
    int64_t created_at;
};

GW_API void gw_file_free(gw_file *file) {
    if (file) {
        yyjson_doc_free(file->doc);
        gw_free(file->request_id);
        gw_free(file);
    }
}

static gw_file *file_parse(const gw_http_response *response, check *c) {
    gw_file *file = gw_calloc(1, sizeof *file);
    if (!file || (response->request_id && !(file->request_id = gw_strdup(response->request_id)))) {
        gw_file_free(file);
        c->status = GW_E_NOMEM;
        snprintf(c->message, c->size, "out of memory");
        return NULL;
    }
    file->doc = parse(response->body.data ? response->body.data : "", response->body.length, c);
    yyjson_val *root = file->doc ? yyjson_doc_get_root(file->doc) : NULL;
    bool present;
    if (file->doc && !yyjson_is_obj(root)) {
        fail(c, "the file is not an object");
    } else if (file->doc) {
        (void)(str_field(c, root, "id", true, &file->id) && str_field(c, root, "filename", false, &file->filename) &&
               str_field(c, root, "purpose", false, &file->purpose) &&
               int_field(c, root, "bytes", false, &present, &file->bytes) &&
               int_field(c, root, "created_at", false, &present, &file->created_at));
    }
    if (c->status != GW_OK) {
        gw_file_free(file);
        return NULL;
    }
    return file;
}

/** Whether `needle` occurs in the `length` bytes at `data`. */
static bool contains(const char *data, size_t length, const char *needle) {
    size_t n = strlen(needle);
    for (size_t i = 0; n <= length && i <= length - n; i++) {
        if (data[i] == needle[0] && memcmp(data + i, needle, n) == 0) {
            return true;
        }
    }
    return false;
}

GW_API gw_status gw_file_upload(gw_client *client, const char *filename, const void *data, size_t length,
                                const gw_call_options *options, gw_file **out, gw_error **error) {
    clear_error(error);
    if (out) {
        *out = NULL;
    }
    if (!client || !out || !filename || (!data && length)) {
        return gw_fail(error, GW_E_CONFIG, "client, filename, data and out are required");
    }
    size_t name_length = strlen(filename);
    bool name_ok = name_length >= 1 && name_length <= MAX_FILENAME_BYTES;
    for (const unsigned char *c = (const unsigned char *)filename; *c && name_ok; c++) {
        name_ok = *c >= 0x20 && *c != 0x7f && *c != '"' && *c != '\\';
    }
    if (!name_ok) {
        return gw_fail(error, GW_E_CONFIG,
                       "a filename must be 1 to %d bytes, without control characters, quotes or backslashes",
                       MAX_FILENAME_BYTES);
    }
    if (length > MAX_UPLOAD_BYTES) {
        return gw_fail(error, GW_E_CONFIG, "a batch file is at most %zu bytes", MAX_UPLOAD_BYTES);
    }
    /* A boundary the file does not contain. */
    char boundary[40] = "gw-";
    bool found = false;
    for (int attempt = 0; attempt < 4 && !found; attempt++) {
        if (!gw_random_hex(boundary + 3, 16)) {
            return gw_fail(error, GW_E_TRANSPORT, "no randomness for a multipart boundary");
        }
        found = !contains(data, length, boundary);
    }
    if (!found) {
        return gw_fail(error, GW_E_CONFIG, "no multipart boundary fits this file");
    }
    char head[512];
    int head_length = snprintf(head, sizeof head,
                               "--%s\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n"
                               "--%s\r\nContent-Disposition: form-data; name=\"file\"; filename=\"",
                               boundary, boundary);
    char middle[] = "\"\r\nContent-Type: application/jsonl\r\n\r\n";
    char tail[64];
    int tail_length = snprintf(tail, sizeof tail, "\r\n--%s--\r\n", boundary);
    char content_type[80];
    snprintf(content_type, sizeof content_type, "multipart/form-data; boundary=%s", boundary);
    gw_buffer body = {0};
    if (head_length < 0 || tail_length < 0 || !gw_buffer_append(&body, head, (size_t)head_length) ||
        !gw_buffer_append(&body, filename, name_length) || !gw_buffer_append(&body, middle, sizeof middle - 1) ||
        (length && !gw_buffer_append(&body, data, length)) || !gw_buffer_append(&body, tail, (size_t)tail_length)) {
        gw_buffer_free(&body);
        return gw_fail(error, GW_E_NOMEM, "out of memory");
    }
    gw_http_request spec = {
        .get = false,
        .path = FILES_PATH,
        .body = body.data,
        .body_length = body.length,
        .content_type = content_type,
        .idempotent = false,
    };
    gw_http_response response;
    gw_status status = gw_http_send(client, &spec, options, &response, error);
    gw_buffer_free(&body);
    if (status == GW_OK) {
        char message[256];
        check c = {message, sizeof message, GW_OK};
        *out = file_parse(&response, &c);
        status = c.status;
        if (status != GW_OK) {
            gw_fail(error, status, "%s", message);
        }
    }
    gw_http_response_free(&response);
    return status;
}

GW_API const char *gw_file_id(const gw_file *file) { return file ? file->id : NULL; }
GW_API const char *gw_file_filename(const gw_file *file) { return file ? file->filename : NULL; }
GW_API const char *gw_file_purpose(const gw_file *file) { return file ? file->purpose : NULL; }
GW_API int64_t gw_file_bytes(const gw_file *file) { return file ? file->bytes : 0; }
GW_API int64_t gw_file_created_at(const gw_file *file) { return file ? file->created_at : 0; }
GW_API const char *gw_file_request_id(const gw_file *file) { return file ? file->request_id : NULL; }

struct gw_file_content {
    gw_http_response response;
};

GW_API gw_status gw_file_download(gw_client *client, const char *file_id, const gw_call_options *options,
                                  gw_file_content **out, gw_error **error) {
    clear_error(error);
    if (out) {
        *out = NULL;
    }
    if (!client || !out) {
        return gw_fail(error, GW_E_CONFIG, "client and out are required");
    }
    if (!valid_id(file_id)) {
        return gw_fail(error, GW_E_CONFIG, "a file id is 1 to %d letters, digits, '_' or '-'", MAX_ID_CHARS);
    }
    char path[sizeof FILES_PATH + MAX_ID_CHARS + 16];
    snprintf(path, sizeof path, "%s/%s/content", FILES_PATH, file_id);
    gw_file_content *content = gw_calloc(1, sizeof *content);
    if (!content) {
        return gw_fail(error, GW_E_NOMEM, "out of memory");
    }
    gw_http_request spec = {
        .get = true,
        .path = path,
        .accept = "application/jsonl, application/json",
        .limit = MAX_DOWNLOAD_BYTES,
    };
    gw_status status = gw_http_send(client, &spec, options, &content->response, error);
    if (status != GW_OK) {
        gw_file_content_free(content);
        return status;
    }
    *out = content;
    return GW_OK;
}

GW_API const char *gw_file_content_data(const gw_file_content *content, size_t *length) {
    if (length) {
        *length = content ? content->response.body.length : 0;
    }
    if (!content) {
        return NULL;
    }
    return content->response.body.data ? content->response.body.data : "";
}

GW_API const char *gw_file_content_request_id(const gw_file_content *content) {
    return content ? content->response.request_id : NULL;
}

GW_API void gw_file_content_free(gw_file_content *content) {
    if (content) {
        gw_http_response_free(&content->response);
        gw_free(content);
    }
}

/* ---------------------------------------------------------------- batches */

struct gw_batch_request {
    yyjson_mut_doc *doc;
    yyjson_mut_val *metadata;
    size_t metadata_count;
};

GW_API gw_batch_request *gw_batch_request_new(const char *input_file_id, const char *endpoint) {
    if (!input_file_id || !endpoint) {
        return NULL;
    }
    gw_batch_request *request = gw_calloc(1, sizeof *request);
    if (!request) {
        return NULL;
    }
    request->doc = yyjson_mut_doc_new(gw_json_allocator());
    yyjson_mut_val *root = request->doc ? yyjson_mut_obj(request->doc) : NULL;
    if (!root || !yyjson_mut_obj_add_strcpy(request->doc, root, "input_file_id", input_file_id) ||
        !yyjson_mut_obj_add_strcpy(request->doc, root, "endpoint", endpoint) ||
        !yyjson_mut_obj_add_str(request->doc, root, "completion_window", "24h")) {
        gw_batch_request_free(request);
        return NULL;
    }
    yyjson_mut_doc_set_root(request->doc, root);
    return request;
}

GW_API gw_status gw_batch_request_set_metadata(gw_batch_request *request, const char *key, const char *value) {
    if (!request || !key || !value) {
        return GW_E_CONFIG;
    }
    size_t key_chars = code_points(key);
    if (key_chars < 1 || key_chars > MAX_METADATA_KEY_CHARS || code_points(value) > MAX_METADATA_VALUE_CHARS) {
        return GW_E_CONFIG;
    }
    yyjson_mut_doc *doc = request->doc;
    if (!request->metadata) {
        yyjson_mut_val *metadata = yyjson_mut_obj(doc);
        if (!metadata || !yyjson_mut_obj_add_val(doc, yyjson_mut_doc_get_root(doc), "metadata", metadata)) {
            return GW_E_NOMEM;
        }
        request->metadata = metadata;
    }
    bool replacing = yyjson_mut_obj_get(request->metadata, key) != NULL;
    if (!replacing && request->metadata_count >= MAX_METADATA_ENTRIES) {
        return GW_E_CONFIG;
    }
    yyjson_mut_val *name = yyjson_mut_strcpy(doc, key);
    yyjson_mut_val *text = yyjson_mut_strcpy(doc, value);
    if (!name || !text) {
        return GW_E_NOMEM;
    }
    if (replacing) {
        yyjson_mut_obj_remove_key(request->metadata, key);
    }
    if (!yyjson_mut_obj_add(request->metadata, name, text)) {
        return GW_E_NOMEM;
    }
    if (!replacing) {
        request->metadata_count++;
    }
    return GW_OK;
}

GW_API void gw_batch_request_free(gw_batch_request *request) {
    if (request) {
        yyjson_mut_doc_free(request->doc);
        gw_free(request);
    }
}

struct gw_batch {
    yyjson_doc *doc; /* NULL for a page's batch: the page owns the document */
    char *json;
    size_t json_length;
    char *request_id;
    const char *id;
    const char *status;
    const char *endpoint;
    const char *completion_window;
    const char *input_file_id;
    const char *output_file_id;
    const char *error_file_id;
    bool has_time[BATCH_TIMES];
    int64_t times[BATCH_TIMES];
    int64_t counts[BATCH_COUNTS];
    yyjson_val *metadata;
};

/** Read a batch object into `batch`, which borrows from its document. */
static bool batch_read(check *c, yyjson_val *value, gw_batch *batch) {
    if (!yyjson_is_obj(value)) {
        return fail(c, "a batch is not an object");
    }
    if (!(str_field(c, value, "id", true, &batch->id) && str_field(c, value, "status", true, &batch->status) &&
          str_field(c, value, "endpoint", true, &batch->endpoint) &&
          str_field(c, value, "input_file_id", true, &batch->input_file_id) &&
          str_field(c, value, "completion_window", false, &batch->completion_window) &&
          str_field(c, value, "output_file_id", false, &batch->output_file_id) &&
          str_field(c, value, "error_file_id", false, &batch->error_file_id))) {
        return false;
    }
    for (int i = 0; i < BATCH_TIMES; i++) {
        bool required = i == GW_BATCH_CREATED_AT || i == GW_BATCH_EXPIRES_AT;
        if (!int_field(c, value, time_keys[i], required, &batch->has_time[i], &batch->times[i])) {
            return false;
        }
    }
    yyjson_val *counts = yyjson_obj_get(value, "request_counts");
    if (!yyjson_is_obj(counts)) {
        return fail(c, "request_counts must be an object");
    }
    for (int i = 0; i < BATCH_COUNTS; i++) {
        bool present;
        if (!int_field(c, counts, count_keys[i], true, &present, &batch->counts[i])) {
            return false;
        }
    }
    yyjson_val *metadata = yyjson_obj_get(value, "metadata");
    if (metadata && !yyjson_is_null(metadata)) {
        if (!yyjson_is_obj(metadata)) {
            return fail(c, "metadata must be an object");
        }
        yyjson_obj_iter iter = yyjson_obj_iter_with(metadata);
        yyjson_val *key;
        while ((key = yyjson_obj_iter_next(&iter))) {
            if (!yyjson_is_str(yyjson_obj_iter_get_val(key))) {
                return fail(c, "metadata values must be strings");
            }
        }
        batch->metadata = metadata;
    }
    return true;
}

static void batch_release(gw_batch *batch) {
    yyjson_doc_free(batch->doc);
    gw_free(batch->json);
    gw_free(batch->request_id);
}

GW_API void gw_batch_free(gw_batch *batch) {
    if (batch) {
        batch_release(batch);
        gw_free(batch);
    }
}

static void nomem(check *c) {
    c->status = GW_E_NOMEM;
    snprintf(c->message, c->size, "out of memory");
}

static gw_batch *batch_parse(gw_http_response *response, check *c) {
    gw_batch *batch = gw_calloc(1, sizeof *batch);
    if (!batch) {
        nomem(c);
        return NULL;
    }
    /* The body becomes the batch's JSON. */
    batch->json = response->body.data ? response->body.data : gw_strdup("");
    batch->json_length = response->body.length;
    memset(&response->body, 0, sizeof response->body);
    batch->request_id = response->request_id;
    response->request_id = NULL;
    if (!batch->json) {
        gw_batch_free(batch);
        nomem(c);
        return NULL;
    }
    batch->doc = parse(batch->json, batch->json_length, c);
    if (batch->doc) {
        batch_read(c, yyjson_doc_get_root(batch->doc), batch);
    }
    if (c->status != GW_OK) {
        gw_batch_free(batch);
        return NULL;
    }
    return batch;
}

/** Send a batch call and decode the batch it returns. */
static gw_status batch_call(gw_client *client, const gw_http_request *spec, const gw_call_options *options,
                            gw_batch **out, gw_error **error) {
    gw_http_response response;
    gw_status status = gw_http_send(client, spec, options, &response, error);
    if (status == GW_OK) {
        char message[256];
        check c = {message, sizeof message, GW_OK};
        *out = batch_parse(&response, &c);
        status = c.status;
        if (status != GW_OK) {
            gw_fail(error, status, "%s", message);
        }
    }
    gw_http_response_free(&response);
    return status;
}

GW_API gw_status gw_batch_create(gw_client *client, const gw_batch_request *request, const gw_call_options *options,
                                 gw_batch **out, gw_error **error) {
    clear_error(error);
    if (out) {
        *out = NULL;
    }
    if (!client || !request || !out) {
        return gw_fail(error, GW_E_CONFIG, "client, request and out are required");
    }
    size_t length = 0;
    char *json = yyjson_mut_write_opts(request->doc, YYJSON_WRITE_NOFLAG, gw_json_allocator(), &length, NULL);
    if (!json) {
        return gw_fail(error, GW_E_CONFIG, "the request could not be serialized (is every string UTF-8?)");
    }
    gw_http_request spec = {
        .get = false,
        .path = BATCHES_PATH,
        .body = json,
        .body_length = length,
        .content_type = "application/json",
        .idempotent = true,
    };
    gw_status status = batch_call(client, &spec, options, out, error);
    gw_free(json);
    return status;
}

static gw_status batch_by_id(gw_client *client, const char *batch_id, bool cancel, const gw_call_options *options,
                             gw_batch **out, gw_error **error) {
    clear_error(error);
    if (out) {
        *out = NULL;
    }
    if (!client || !out) {
        return gw_fail(error, GW_E_CONFIG, "client and out are required");
    }
    if (!valid_id(batch_id)) {
        return gw_fail(error, GW_E_CONFIG, "a batch id is 1 to %d letters, digits, '_' or '-'", MAX_ID_CHARS);
    }
    char path[sizeof BATCHES_PATH + MAX_ID_CHARS + 16];
    snprintf(path, sizeof path, "%s/%s%s", BATCHES_PATH, batch_id, cancel ? "/cancel" : "");
    gw_http_request spec = {.get = !cancel, .path = path};
    return batch_call(client, &spec, options, out, error);
}

GW_API gw_status gw_batch_retrieve(gw_client *client, const char *batch_id, const gw_call_options *options,
                                   gw_batch **out, gw_error **error) {
    return batch_by_id(client, batch_id, false, options, out, error);
}

GW_API gw_status gw_batch_cancel(gw_client *client, const char *batch_id, const gw_call_options *options,
                                 gw_batch **out, gw_error **error) {
    return batch_by_id(client, batch_id, true, options, out, error);
}

GW_API const char *gw_batch_id(const gw_batch *batch) { return batch ? batch->id : NULL; }
GW_API const char *gw_batch_status(const gw_batch *batch) { return batch ? batch->status : NULL; }
GW_API const char *gw_batch_endpoint(const gw_batch *batch) { return batch ? batch->endpoint : NULL; }
GW_API const char *gw_batch_completion_window(const gw_batch *batch) {
    return batch ? batch->completion_window : NULL;
}
GW_API const char *gw_batch_input_file_id(const gw_batch *batch) { return batch ? batch->input_file_id : NULL; }
GW_API const char *gw_batch_output_file_id(const gw_batch *batch) { return batch ? batch->output_file_id : NULL; }
GW_API const char *gw_batch_error_file_id(const gw_batch *batch) { return batch ? batch->error_file_id : NULL; }

GW_API bool gw_batch_time_get(const gw_batch *batch, gw_batch_time moment, int64_t *out) {
    int index = (int)moment;
    if (!batch || index < 0 || index >= BATCH_TIMES || !batch->has_time[index]) {
        return false;
    }
    if (out) {
        *out = batch->times[index];
    }
    return true;
}

GW_API int64_t gw_batch_count_get(const gw_batch *batch, gw_batch_count count) {
    int index = (int)count;
    return batch && index >= 0 && index < BATCH_COUNTS ? batch->counts[index] : 0;
}

GW_API const char *gw_batch_metadata(const gw_batch *batch, const char *key) {
    if (!batch || !batch->metadata || !key) {
        return NULL;
    }
    return yyjson_get_str(yyjson_obj_get(batch->metadata, key));
}

GW_API const char *gw_batch_json(const gw_batch *batch, size_t *length) {
    if (length) {
        *length = batch ? batch->json_length : 0;
    }
    return batch ? batch->json : NULL;
}

GW_API const char *gw_batch_request_id(const gw_batch *batch) { return batch ? batch->request_id : NULL; }

struct gw_batch_page {
    yyjson_doc *doc;
    char *body;
    char *request_id;
    gw_batch *items;
    size_t count;
    bool has_more;
    const char *last_id;
};

GW_API void gw_batch_page_free(gw_batch_page *page) {
    if (page) {
        for (size_t i = 0; i < page->count; i++) {
            gw_free(page->items[i].json);
        }
        gw_free(page->items);
        yyjson_doc_free(page->doc);
        gw_free(page->body);
        gw_free(page->request_id);
        gw_free(page);
    }
}

static gw_batch_page *page_parse(gw_http_response *response, check *c) {
    gw_batch_page *page = gw_calloc(1, sizeof *page);
    if (!page) {
        nomem(c);
        return NULL;
    }
    page->body = response->body.data;
    size_t length = response->body.length;
    memset(&response->body, 0, sizeof response->body);
    page->request_id = response->request_id;
    response->request_id = NULL;
    page->doc = parse(page->body ? page->body : "", length, c);
    yyjson_val *root = page->doc ? yyjson_doc_get_root(page->doc) : NULL;
    yyjson_val *data = root ? yyjson_obj_get(root, "data") : NULL;
    yyjson_val *more = root ? yyjson_obj_get(root, "has_more") : NULL;
    if (page->doc && (!yyjson_is_obj(root) || !yyjson_is_arr(data) || !yyjson_is_bool(more))) {
        fail(c, "a page needs data and has_more");
    } else if (page->doc && str_field(c, root, "last_id", false, &page->last_id)) {
        page->has_more = yyjson_get_bool(more);
        size_t count = yyjson_arr_size(data);
        if (count && !(page->items = gw_calloc(count, sizeof *page->items))) {
            nomem(c);
        }
        for (size_t i = 0; i < count && c->status == GW_OK; i++) {
            yyjson_val *item = yyjson_arr_get(data, i);
            gw_batch *batch = &page->items[i];
            page->count = i + 1;
            if (batch_read(c, item, batch)) {
                batch->json =
                    yyjson_val_write_opts(item, YYJSON_WRITE_NOFLAG, gw_json_allocator(), &batch->json_length, NULL);
                if (!batch->json) {
                    nomem(c);
                }
            }
        }
    }
    if (c->status != GW_OK) {
        gw_batch_page_free(page);
        return NULL;
    }
    return page;
}

GW_API gw_status gw_batches_list(gw_client *client, const char *after, int limit, const gw_call_options *options,
                                 gw_batch_page **out, gw_error **error) {
    clear_error(error);
    if (out) {
        *out = NULL;
    }
    if (!client || !out) {
        return gw_fail(error, GW_E_CONFIG, "client and out are required");
    }
    if (limit < 0 || limit > 100) {
        return gw_fail(error, GW_E_CONFIG, "limit is 1 to 100, or 0 for the gateway's default");
    }
    if (after && !valid_id(after)) {
        return gw_fail(error, GW_E_CONFIG, "a batch id is 1 to %d letters, digits, '_' or '-'", MAX_ID_CHARS);
    }
    char path[sizeof BATCHES_PATH + MAX_ID_CHARS + 32];
    int at = snprintf(path, sizeof path, "%s", BATCHES_PATH);
    char separator = '?';
    if (limit > 0) {
        at += snprintf(path + at, sizeof path - (size_t)at, "%climit=%d", separator, limit);
        separator = '&';
    }
    if (after) {
        snprintf(path + at, sizeof path - (size_t)at, "%cafter=%s", separator, after);
    }
    gw_http_request spec = {.get = true, .path = path};
    gw_http_response response;
    gw_status status = gw_http_send(client, &spec, options, &response, error);
    if (status == GW_OK) {
        char message[256];
        check c = {message, sizeof message, GW_OK};
        *out = page_parse(&response, &c);
        status = c.status;
        if (status != GW_OK) {
            gw_fail(error, status, "%s", message);
        }
    }
    gw_http_response_free(&response);
    return status;
}

GW_API size_t gw_batch_page_count(const gw_batch_page *page) { return page ? page->count : 0; }

GW_API const gw_batch *gw_batch_page_item(const gw_batch_page *page, size_t index) {
    return page && index < page->count ? &page->items[index] : NULL;
}

GW_API bool gw_batch_page_has_more(const gw_batch_page *page) { return page && page->has_more; }
GW_API const char *gw_batch_page_last_id(const gw_batch_page *page) { return page ? page->last_id : NULL; }
GW_API const char *gw_batch_page_request_id(const gw_batch_page *page) { return page ? page->request_id : NULL; }
