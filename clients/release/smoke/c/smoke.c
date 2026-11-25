/* Installed-library smoke test (C): the installed headers and libqucelerate-ai-gateway.so.0
 * link, and a call reaches libcurl (a closed port: a transport error). */
#include <gateway/gateway.h>

#include <stdio.h>

int main(void) {
    if (gw_global_init(NULL) != GW_OK) {
        return 1;
    }
    gw_client_options *options = gw_client_options_new();
    gw_client_options_set_base_url(options, "http://127.0.0.1:1");
    gw_client_options_set_api_key(options, "gw_smoke");
    gw_client *client = NULL;
    gw_error *error = NULL;
    if (gw_client_new(options, &client, &error) != GW_OK) {
        fprintf(stderr, "client: %s\n", gw_error_message(error));
        return 1;
    }
    gw_client_options_free(options);
    gw_chat_request *request = gw_chat_request_new("smoke");
    gw_chat_request_add_message(request, "user", "hi");
    gw_chat_response *response = NULL;
    gw_status status = gw_chat(client, request, NULL, &response, &error);
    gw_error_free(error);
    gw_chat_response_free(response);
    gw_chat_request_free(request);
    gw_client_free(client);
    gw_global_cleanup();
    if (status != GW_E_TRANSPORT) {
        fprintf(stderr, "expected GW_E_TRANSPORT, got %s\n", gw_status_name(status));
        return 1;
    }
    printf("ok: c library %s installs and runs\n", gw_version());
    return 0;
}
