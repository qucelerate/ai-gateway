# QuCelerate AI Gateway client (C and C++)

A client for the [QuCelerate AI Gateway](https://gateway.qucelerate.com) inference API, contract 1.0.0: chat, streaming
chat, embeddings, and batch files and batches. It is a transport wrapper; authorization, budgets,
routing, and settlement stay on the gateway.

- `libqucelerate-ai-gateway` is a C11 library with a small, ABI-stable C API
  (`include/gateway/gateway.h`). Every object is opaque and created and
  freed through the API, so later versions can change their layout without
  breaking callers. The shared library exports only `gw_*` functions and is
  versioned `libqucelerate-ai-gateway.so.0`.
- `include/gateway/gateway.hpp` is a header-only C++17 wrapper. It copies
  results into standard types (`std::string`, `std::optional`,
  `std::variant`) and throws on failure, so no C++ type crosses the library
  boundary and the wrapper works with any compiler that can call the C API.

It needs libcurl 7.85 or later. JSON is parsed by a vendored copy of
[yyjson](third_party/yyjson/README.md), compiled in with hidden symbols.

```cpp
#include <gateway/gateway.hpp>

gateway::Client client({"https://gateway.example.com", std::getenv("GATEWAY_API_KEY")});
gateway::ChatRequest request{"chat-default", {gateway::Message::user("Say hello.")}};

auto response = client.chat(request);
std::cout << response.content() << "\n";

auto stream = client.chat_stream(request);
while (auto event = stream.next()) {
    if (auto *delta = std::get_if<gateway::ChoiceDelta>(&*event); delta && delta->content_delta) {
        std::cout << *delta->content_delta;
    }
}
```

The same in C:

```c
#include <gateway/gateway.h>

gw_global_init(NULL);
gw_client_options *options = gw_client_options_new();
gw_client_options_set_base_url(options, "https://gateway.example.com");
gw_client_options_set_api_key(options, getenv("GATEWAY_API_KEY"));
gw_client *client = NULL;
gw_error *error = NULL;
if (gw_client_new(options, &client, &error) != GW_OK) { /* gw_error_message(error) */ }
gw_client_options_free(options);

gw_chat_request *request = gw_chat_request_new("chat-default");
gw_chat_request_add_message(request, "user", "Say hello.");

gw_chat_response *response = NULL;
if (gw_chat(client, request, NULL, &response, &error) == GW_OK) {
    const char *content = gw_chat_response_choice_content(response, 0, NULL);
    puts(content ? content : "");
    gw_chat_response_free(response);
} else {
    fprintf(stderr, "%s (request %s)\n", gw_error_message(error), gw_error_request_id(error));
    gw_error_free(error);
}

gw_stream *stream = NULL;
if (gw_chat_stream_open(client, request, NULL, &stream, &error) == GW_OK) {
    const gw_event *event;
    gw_status status;
    while ((status = gw_stream_next(stream, &event, &error)) == GW_OK) {
        size_t length;
        const char *delta = gw_event_content_delta(event, &length);
        if (gw_event_get_type(event) == GW_EVENT_CHOICE_DELTA && delta) {
            fwrite(delta, 1, length, stdout);
        }
    }
    if (status != GW_DONE) { /* the stream failed: error is set */ }
    gw_stream_free(stream);
}
gw_error_free(error);
gw_chat_request_free(request);
gw_client_free(client);
gw_global_cleanup();
```

`examples/chat.cpp` (built as `gateway_chat`) runs a call and a stream
against `GATEWAY_URL` with `GATEWAY_API_KEY`.

### Batches

A batch (OpenAI's Batch API shape) is a JSONL file of requests, one per
line, each `{"custom_id", "method": "POST", "url", "body"}`, run within 24
hours at the batch price. Every line is validated and reserved against
budgets when the batch is created; each is then accounted as a request of
its own.

```cpp
auto file = client.upload_file("questions.jsonl", jsonl);
auto batch = client.create_batch({file.id, "/v1/chat/completions", {{"name", "nightly"}}});
// later
batch = client.batch(batch.id);
if (batch.output_file_id) {
    std::string results = client.file_content(*batch.output_file_id); // JSONL, by custom_id
}
```

In C: `gw_file_upload` (multipart, at most 64 MiB), `gw_batch_request_new`
with `gw_batch_request_set_metadata`, then `gw_batch_create`,
`gw_batch_retrieve`, `gw_batch_cancel` and `gw_batches_list` (pages of
borrowed `gw_batch`es with `gw_batch_page_has_more` and
`gw_batch_page_last_id`), and `gw_file_download` (at most 256 MiB) for the
output and error files. A `gw_batch` gives its status, files, moments
(`gw_batch_time_get`), line counts (`gw_batch_count_get`), metadata, and its
raw JSON (`gw_batch_json`), which includes the gateway's account of its jobs
and cost on a single read. `GW_HAS_BATCHES` is defined by headers that
declare these calls.

## Building

```sh
cmake -S clients/c -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
ctest --test-dir build --output-on-failure
cmake --install build --prefix /usr/local
```

The install holds `libqucelerate-ai-gateway.so.0`, the headers
(`<gateway/gateway.h>`, `<gateway/gateway.hpp>`), a CMake package and
`qucelerate-ai-gateway.pc`, so a project finds the library either way:

```cmake
find_package(qucelerate-ai-gateway REQUIRED)   # CMAKE_PREFIX_PATH names the prefix
target_link_libraries(app PRIVATE qucelerate::ai-gateway)
```

```sh
cc app.c $(pkg-config --cflags --libs qucelerate-ai-gateway)
```

`-DGATEWAY_BUILD_TESTS=OFF` skips the tests (a release source archive
builds without them). `-DGATEWAY_SANITIZE=ON` builds with AddressSanitizer
and UndefinedBehaviorSanitizer, and `-DGATEWAY_WERROR=ON` turns this
library's warnings into errors, as CI does.

## Behavior

- **Idempotency.** Chat, streams, embeddings and `gw_batch_create` send an
  `Idempotency-Key`: a generated UUID, or the key set with
  `gw_call_options_set_idempotency_key`. A retry of the call reuses it, so
  the gateway never runs or charges the call twice. File uploads, batch
  reads, lists, cancels and downloads send none.
- **Retries.** The client retries automatically only when the gateway
  refused the request before reserving anything (`rate_limit_exceeded`,
  `concurrency_limit_exceeded`, `provider_circuit_open`), at most
  `gw_client_options_set_max_retries` times (default 2), waiting
  `Retry-After` (at most 60 seconds, 1 second when absent). Anything that
  may have reached a provider is returned as an error, never retried. See
  `clients/conformance/errors.json` for every code.
- **Errors.** Every call returns a `gw_status` and, on failure, an optional
  `gw_error` to free. `GW_E_PROBLEM` carries the gateway's problem:
  `gw_error_http_status`, `gw_error_code` (an open string),
  `gw_error_request_id`, and `gw_error_retry_after` (-1 when absent).
  `GW_E_HTTP` is an error response without a problem body. A stream that
  ends in an `error` event fails with `GW_E_STREAM`, after the events
  before it; one that closes early or sends an event over 16 MiB fails with
  `GW_E_PROTOCOL`; a body that does not match the contract is
  `GW_E_DECODE`. The C++ wrapper throws `gateway::ProblemError`,
  `HttpError`, `StreamError` and so on, all derived from `gateway::Error`.
- **Compatibility.** Unknown response fields and stream events are ignored,
  as contract 1.x requires. Finish reasons and codes are open strings.
  `gw_chat_response_json` and `gw_event_json` give the raw JSON for fields
  this version does not model. Known events missing a field the contract
  requires fail the stream with `GW_E_DECODE`.
- **Text.** Strings are NUL-terminated UTF-8. Model text (content, refusals,
  deltas, tool arguments) may also contain NUL bytes, so its accessors take
  an optional `size_t *length`; `gw_chat_request_add_message_n` and
  `gw_embedding_request_add_input_n` send such text. The C++ wrapper always
  keeps the full length.
- **Cancellation and timeouts.** A `gw_cancel` flag, set with
  `gw_call_options_set_cancel`, cancels a call, its retry wait, or a stream
  from any thread (`GW_E_CANCELLED`). Each attempt of a non-streaming call
  times out after 120 seconds by default
  (`gw_client_options_set_timeout_ms`, `GW_E_TIMEOUT`). A stream's response
  must start within the same limit; after that there is no limit between
  events.
- **Limits.** A successful body over 64 MiB (256 MiB for a file download) fails the call with
  `GW_E_DECODE`, and an error body is read only to its first 1 MiB, so a
  misbehaving peer cannot grow the client's memory without bound.
- **Request IDs.** `gw_chat_response_request_id`,
  `gw_embedding_response_request_id` and `gw_stream_request_id` give the
  `X-Request-Id` response header; quote it when reporting a problem.
  `gw_call_options_set_request_id` sends your own.
- **Transport.** Only `http` and `https` are allowed, redirects are not
  followed, and TLS verification is always on; trust a private CA with
  `gw_client_options_set_ca_file` or `gw_client_options_set_ca_path`. Header
  values with control characters are rejected (`GW_E_CONFIG`).
- **Threads.** Call `gw_global_init` once before other threads use the
  library. A `gw_client` may be used by several threads at once; every
  other object is used by one thread at a time. `gw_global_init` can take a
  `gw_allocator` for the library's own memory (libcurl keeps its own).
- **Secrets.** The API key is sent only in the `Authorization` header and
  is never logged.

## Tests

`ctest` runs `test_c`, unit tests of the C API (the stream parser, argument
checks, limits, timeouts, cancellation, and the files and batches requests
as a local server receives them) on a counting allocator that fails
any test that leaks, and `conformance`, which serves every case of
`clients/conformance` from a local server through the C++ wrapper and
checks the requests this client sends and the results it returns. The
conformance cases cover chat, streams and embeddings; files and batches
have none yet in any client.

This library is not published; build it from this directory.
