# Streaming chat

Streaming uses `POST /api/inference/v1/chat/completions/stream` and `text/event-stream`. It is **non-resumable**: if the connection breaks, start a new request with a new idempotency key according to your retry policy.

```sh
curl -N "$GATEWAY_BASE_URL/api/inference/v1/chat/completions/stream" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: e8a31d5c-2f67-4b19-a0c4-6d7e9f1b2c83" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"Say hello in two words."}]}'
```

Simplified fixture sequence: `response.started`, `choice.delta`, `usage`, `choice.finished`, then terminal `response.completed`. Streams can interleave additional `choice.delta` and `tool_call.delta` events. `response.completed` and `error` are terminal events; there is no resume cursor.

```text
event: response.started
data: {"model":"chat-default"}

event: choice.delta
data: {"choice_index":0,"content_delta":"hello","refusal_delta":null,"annotations":[]}

event: usage
data: {"input_tokens":1,"output_tokens":1,"cached_input_tokens":null,"reasoning_tokens":null,"other_tokens":null,"quality":"exact","completeness":"complete"}

event: choice.finished
data: {"choice_index":0,"finish_reason":"stop"}

event: response.completed
data: {"model":"chat-default","usage":null}
```

Treat every event as untrusted input and stop reading after a terminal event.
