# Changelog: QuCelerate AI Gateway client (C and C++)

All notable changes to this client. Each release has a `## <version>` section,
which becomes its GitHub release notes (`clients/release/changelog.py`).
Versions follow semantic versioning; the client implements the gateway's
inference contract 1.0.0.

## 0.2.0

- Renamed for public release: the library is `libqucelerate-ai-gateway.so.0` (was
  `libgateway.so.0`), the pkg-config module `qucelerate-ai-gateway` (was `gateway`), and the
  CMake package `find_package(qucelerate-ai-gateway)` with the target
  `qucelerate::ai-gateway` (was `gateway::gateway`). Rebuild and relink against the new
  name; the headers stay `<gateway/gateway.h>` and `<gateway/gateway.hpp>`, and the C API
  is unchanged. The User-Agent is `qucelerate-ai-gateway-c/<version>`.

Apart from the rename, it adds symbols only: no existing signature or
behavior changes, so the SONAME's major version stays 0.

- Tool calling (D-032): `gw_chat_request_add_tool`, `gw_chat_request_set_tool_choice`,
  `gw_chat_request_set_tool_choice_function`, `gw_chat_request_set_parallel_tool_calls`,
  `gw_chat_request_add_tool_call`, `gw_chat_request_add_tool_result` and `gw_chat_request_add_tool_result_n`
  (a result of `length` bytes, NUL bytes kept, which the C++ wrapper uses); a response's calls through
  `gw_chat_response_choice_tool_call_count` and `gw_chat_response_choice_tool_call`. The C++ wrapper's
  `ChatRequest`, `Message` and `Choice` carry them too. New symbols only: the ABI stays compatible.
- Batches (OpenAI's Batch API shape): `gw_file_upload` (multipart, purpose
  `batch`, at most 64 MiB), `gw_batch_create` (with an Idempotency-Key),
  `gw_batch_retrieve`, `gw_batch_cancel`, `gw_batches_list` and
  `gw_file_download` (output and error files, at most 256 MiB), with the
  opaque `gw_file`, `gw_file_content`, `gw_batch_request`, `gw_batch` and
  `gw_batch_page` types and their accessors. `GW_HAS_BATCHES` marks
  headers that declare them.
- `gateway.hpp`: `Client::upload_file`, `file_content`, `create_batch`,
  `batch`, `cancel_batch` and `list_batches`, with `File`, `BatchRequest`,
  `Batch` and `BatchPage`.

## 0.1.0

The first release.

- `libgateway` (`libgateway.so.0`) is a C11 library with an ABI-stable,
  opaque `gw_*` API for chat, streaming chat and embeddings, on libcurl.
  Its JSON parser, yyjson 0.13.0, is vendored and hidden.
- Text accessors return the length, so content containing NUL bytes is not
  truncated. `_n` builders take explicit lengths.
- Errors carry the gateway's problem code, status, request id and `Retry-After`.
- `gateway.hpp` is a header-only C++17 wrapper with RAII types and exceptions.
- The library installs a CMake package (`find_package(gateway)`, target
  `gateway::gateway`) and a relocatable pkg-config file.
- Passes the 25 conformance cases. Tested with gcc, clang, ASan and UBSan.
