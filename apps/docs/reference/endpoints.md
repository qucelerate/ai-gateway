# HTTP endpoints

## Shared headers

Every documented endpoint requires `Authorization: Bearer <key>` and `Idempotency-Key: <opaque-id>`. `X-Request-Id` is optional. Send `Content-Type: application/json` for JSON requests.

## Chat

`POST /api/inference/v1/chat/completions` accepts `model` and `messages` and returns JSON. Use the stream variant when you need incremental output. A request may also define `tools` the model can call, with `tool_choice` and `parallel_tool_calls`; see [tool calling](/quickstarts/tools.md).

## Streaming chat

`POST /api/inference/v1/chat/completions/stream` returns `text/event-stream`, `Cache-Control: no-store`, and named events. It is not resumable. `response.completed` and `error` are terminal.

## Embeddings

`POST /api/inference/v1/embeddings` accepts a model name up to 128 characters and an `input` array of 1–128 strings. Each input string may contain 1–131,072 characters. It returns a list response.

For exact schemas, response fields, and status references, use the [OpenAPI JSON artifact](/openapi/inference-v1.json).
