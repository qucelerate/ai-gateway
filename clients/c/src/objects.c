/* Options and request builders. */
#include "internal.h"
#include "objects.h"

#include <string.h>

static gw_status replace(char **slot, const char *value) {
    char *copy = NULL;
    if (value) {
        copy = gw_strdup(value);
        if (!copy) {
            return GW_E_NOMEM;
        }
    }
    gw_free(*slot);
    *slot = copy;
    return GW_OK;
}

GW_API gw_client_options *gw_client_options_new(void) {
    gw_client_options *options = gw_calloc(1, sizeof *options);
    if (options) {
        options->max_retries = 2;
        options->timeout_ms = 120000;
    }
    return options;
}

GW_API gw_status gw_client_options_set_base_url(gw_client_options *options, const char *base_url) {
    return options && base_url ? replace(&options->base_url, base_url) : GW_E_CONFIG;
}

GW_API gw_status gw_client_options_set_api_key(gw_client_options *options, const char *api_key) {
    return options && api_key ? replace(&options->api_key, api_key) : GW_E_CONFIG;
}

GW_API gw_status gw_client_options_set_max_retries(gw_client_options *options, int max_retries) {
    if (!options || max_retries < 0) {
        return GW_E_CONFIG;
    }
    options->max_retries = max_retries;
    return GW_OK;
}

GW_API gw_status gw_client_options_set_timeout_ms(gw_client_options *options, int64_t timeout_ms) {
    if (!options || timeout_ms <= 0) {
        return GW_E_CONFIG;
    }
    options->timeout_ms = timeout_ms;
    return GW_OK;
}

GW_API gw_status gw_client_options_set_ca_file(gw_client_options *options, const char *path) {
    return options ? replace(&options->ca_file, path) : GW_E_CONFIG;
}

GW_API gw_status gw_client_options_set_ca_path(gw_client_options *options, const char *path) {
    return options ? replace(&options->ca_path, path) : GW_E_CONFIG;
}

GW_API gw_status gw_client_options_set_sleep(gw_client_options *options, gw_sleep_fn sleep, void *ctx) {
    if (!options) {
        return GW_E_CONFIG;
    }
    options->sleep = sleep;
    options->sleep_ctx = ctx;
    return GW_OK;
}

void gw_client_options_clear(gw_client_options *options) {
    gw_free(options->base_url);
    gw_free(options->api_key);
    gw_free(options->ca_file);
    gw_free(options->ca_path);
}

GW_API void gw_client_options_free(gw_client_options *options) {
    if (options) {
        gw_client_options_clear(options);
        gw_free(options);
    }
}

GW_API gw_call_options *gw_call_options_new(void) { return gw_calloc(1, sizeof(gw_call_options)); }

GW_API gw_status gw_call_options_set_idempotency_key(gw_call_options *options, const char *key) {
    return options ? replace(&options->idempotency_key, key) : GW_E_CONFIG;
}

GW_API gw_status gw_call_options_set_request_id(gw_call_options *options, const char *request_id) {
    return options ? replace(&options->request_id, request_id) : GW_E_CONFIG;
}

GW_API gw_status gw_call_options_set_cancel(gw_call_options *options, gw_cancel *cancel) {
    if (!options) {
        return GW_E_CONFIG;
    }
    options->cancel = cancel;
    return GW_OK;
}

GW_API void gw_call_options_free(gw_call_options *options) {
    if (options) {
        gw_free(options->idempotency_key);
        gw_free(options->request_id);
        gw_free(options);
    }
}

/* Requests keep a yyjson mutable document: the body they serialize to. */

static yyjson_mut_doc *new_body(const char *model, const char *list, yyjson_mut_val **items) {
    yyjson_mut_doc *doc = yyjson_mut_doc_new(gw_json_allocator());
    if (!doc) {
        return NULL;
    }
    yyjson_mut_val *root = yyjson_mut_obj(doc);
    *items = yyjson_mut_arr(doc);
    if (!root || !*items || !yyjson_mut_obj_add_strcpy(doc, root, "model", model) ||
        !yyjson_mut_obj_add_val(doc, root, list, *items)) {
        yyjson_mut_doc_free(doc);
        return NULL;
    }
    yyjson_mut_doc_set_root(doc, root);
    return doc;
}

GW_API gw_chat_request *gw_chat_request_new(const char *model) {
    if (!model) {
        return NULL;
    }
    gw_chat_request *request = gw_calloc(1, sizeof *request);
    if (request && !(request->doc = new_body(model, "messages", &request->messages))) {
        gw_free(request);
        return NULL;
    }
    return request;
}

GW_API gw_status gw_chat_request_add_message(gw_chat_request *request, const char *role, const char *content) {
    return gw_chat_request_add_message_n(request, role, content, content ? strlen(content) : 0);
}

GW_API gw_status gw_chat_request_add_message_n(gw_chat_request *request, const char *role, const char *content,
                                               size_t length) {
    if (!request || !role || !content) {
        return GW_E_CONFIG;
    }
    yyjson_mut_val *message = yyjson_mut_obj(request->doc);
    if (!message || !yyjson_mut_obj_add_strcpy(request->doc, message, "role", role) ||
        !yyjson_mut_obj_add_strncpy(request->doc, message, "content", content, length) ||
        !yyjson_mut_arr_append(request->messages, message)) {
        return GW_E_NOMEM;
    }
    return GW_OK;
}

static gw_status set_number(gw_chat_request *request, const char *key, yyjson_mut_val *value) {
    yyjson_mut_val *root = yyjson_mut_doc_get_root(request->doc);
    yyjson_mut_obj_remove_key(root, key);
    return value && yyjson_mut_obj_add_val(request->doc, root, key, value) ? GW_OK : GW_E_NOMEM;
}

GW_API gw_status gw_chat_request_set_temperature(gw_chat_request *request, double temperature) {
    if (!request || !(temperature >= 0 && temperature <= 2)) {
        return GW_E_CONFIG;
    }
    return set_number(request, "temperature", yyjson_mut_real(request->doc, temperature));
}

GW_API gw_status gw_chat_request_set_max_tokens(gw_chat_request *request, int max_tokens) {
    if (!request || max_tokens < 1) {
        return GW_E_CONFIG;
    }
    return set_number(request, "max_tokens", yyjson_mut_sint(request->doc, max_tokens));
}

GW_API gw_status gw_chat_request_add_tool(gw_chat_request *request, const char *name, const char *description,
                                          const char *parameters_json) {
    if (!request || !name) {
        return GW_E_CONFIG;
    }
    yyjson_mut_val *parameters = NULL;
    if (parameters_json) {
        yyjson_doc *schema = yyjson_read_opts((char *)parameters_json, strlen(parameters_json), YYJSON_READ_NOFLAG,
                                              gw_json_allocator(), NULL);
        if (!schema || !yyjson_is_obj(yyjson_doc_get_root(schema))) {
            yyjson_doc_free(schema);
            return GW_E_CONFIG;
        }
        parameters = yyjson_val_mut_copy(request->doc, yyjson_doc_get_root(schema));
        yyjson_doc_free(schema);
        if (!parameters) {
            return GW_E_NOMEM;
        }
    }
    yyjson_mut_val *root = yyjson_mut_doc_get_root(request->doc);
    yyjson_mut_val *tools = yyjson_mut_obj_get(root, "tools");
    if (!tools) {
        tools = yyjson_mut_arr(request->doc);
        if (!tools || !yyjson_mut_obj_add_val(request->doc, root, "tools", tools)) {
            return GW_E_NOMEM;
        }
    }
    yyjson_mut_val *tool = yyjson_mut_obj(request->doc);
    yyjson_mut_val *function = yyjson_mut_obj(request->doc);
    if (!tool || !function || !yyjson_mut_obj_add_str(request->doc, tool, "type", "function") ||
        !yyjson_mut_obj_add_strcpy(request->doc, function, "name", name) ||
        (description && !yyjson_mut_obj_add_strcpy(request->doc, function, "description", description)) ||
        (parameters && !yyjson_mut_obj_add_val(request->doc, function, "parameters", parameters)) ||
        !yyjson_mut_obj_add_val(request->doc, tool, "function", function) || !yyjson_mut_arr_append(tools, tool)) {
        return GW_E_NOMEM;
    }
    return GW_OK;
}

GW_API gw_status gw_chat_request_set_tool_choice(gw_chat_request *request, const char *mode) {
    if (!request || !mode || (strcmp(mode, "auto") && strcmp(mode, "none") && strcmp(mode, "required"))) {
        return GW_E_CONFIG;
    }
    return set_number(request, "tool_choice", yyjson_mut_strcpy(request->doc, mode));
}

GW_API gw_status gw_chat_request_set_tool_choice_function(gw_chat_request *request, const char *name) {
    if (!request || !name) {
        return GW_E_CONFIG;
    }
    yyjson_mut_val *choice = yyjson_mut_obj(request->doc);
    yyjson_mut_val *function = yyjson_mut_obj(request->doc);
    if (!choice || !function || !yyjson_mut_obj_add_str(request->doc, choice, "type", "function") ||
        !yyjson_mut_obj_add_strcpy(request->doc, function, "name", name) ||
        !yyjson_mut_obj_add_val(request->doc, choice, "function", function)) {
        return GW_E_NOMEM;
    }
    return set_number(request, "tool_choice", choice);
}

GW_API gw_status gw_chat_request_set_parallel_tool_calls(gw_chat_request *request, bool parallel) {
    if (!request) {
        return GW_E_CONFIG;
    }
    return set_number(request, "parallel_tool_calls", yyjson_mut_bool(request->doc, parallel));
}

GW_API gw_status gw_chat_request_add_tool_call(gw_chat_request *request, const char *id, const char *name,
                                               const char *arguments) {
    if (!request || !id || !name || !arguments) {
        return GW_E_CONFIG;
    }
    yyjson_mut_val *message = yyjson_mut_arr_get_last(request->messages);
    const char *role = message ? yyjson_mut_get_str(yyjson_mut_obj_get(message, "role")) : NULL;
    if (!role || strcmp(role, "assistant")) {
        return GW_E_CONFIG;
    }
    yyjson_mut_val *calls = yyjson_mut_obj_get(message, "tool_calls");
    if (!calls) {
        calls = yyjson_mut_arr(request->doc);
        if (!calls || !yyjson_mut_obj_add_val(request->doc, message, "tool_calls", calls)) {
            return GW_E_NOMEM;
        }
        /* A message that only calls tools has null content. */
        yyjson_mut_val *content = yyjson_mut_obj_get(message, "content");
        if (yyjson_mut_is_str(content) && yyjson_mut_get_len(content) == 0) {
            yyjson_mut_set_null(content);
        }
    }
    yyjson_mut_val *call = yyjson_mut_obj(request->doc);
    yyjson_mut_val *function = yyjson_mut_obj(request->doc);
    if (!call || !function || !yyjson_mut_obj_add_strcpy(request->doc, call, "id", id) ||
        !yyjson_mut_obj_add_str(request->doc, call, "type", "function") ||
        !yyjson_mut_obj_add_strcpy(request->doc, function, "name", name) ||
        !yyjson_mut_obj_add_strcpy(request->doc, function, "arguments", arguments) ||
        !yyjson_mut_obj_add_val(request->doc, call, "function", function) || !yyjson_mut_arr_append(calls, call)) {
        return GW_E_NOMEM;
    }
    return GW_OK;
}

GW_API gw_status gw_chat_request_add_tool_result(gw_chat_request *request, const char *tool_call_id,
                                                 const char *content) {
    return gw_chat_request_add_tool_result_n(request, tool_call_id, content, content ? strlen(content) : 0);
}

GW_API gw_status gw_chat_request_add_tool_result_n(gw_chat_request *request, const char *tool_call_id,
                                                   const char *content, size_t length) {
    if (!request || !tool_call_id || !content) {
        return GW_E_CONFIG;
    }
    gw_status status = gw_chat_request_add_message_n(request, "tool", content, length);
    if (status != GW_OK) {
        return status;
    }
    yyjson_mut_val *message = yyjson_mut_arr_get_last(request->messages);
    return yyjson_mut_obj_add_strcpy(request->doc, message, "tool_call_id", tool_call_id) ? GW_OK : GW_E_NOMEM;
}

GW_API void gw_chat_request_free(gw_chat_request *request) {
    if (request) {
        yyjson_mut_doc_free(request->doc);
        gw_free(request);
    }
}

GW_API gw_embedding_request *gw_embedding_request_new(const char *model) {
    if (!model) {
        return NULL;
    }
    gw_embedding_request *request = gw_calloc(1, sizeof *request);
    if (request && !(request->doc = new_body(model, "input", &request->inputs))) {
        gw_free(request);
        return NULL;
    }
    return request;
}

GW_API gw_status gw_embedding_request_add_input(gw_embedding_request *request, const char *input) {
    return gw_embedding_request_add_input_n(request, input, input ? strlen(input) : 0);
}

GW_API gw_status gw_embedding_request_add_input_n(gw_embedding_request *request, const char *input, size_t length) {
    if (!request || !input) {
        return GW_E_CONFIG;
    }
    return yyjson_mut_arr_add_strncpy(request->doc, request->inputs, input, length) ? GW_OK : GW_E_NOMEM;
}

GW_API void gw_embedding_request_free(gw_embedding_request *request) {
    if (request) {
        yyjson_mut_doc_free(request->doc);
        gw_free(request);
    }
}
