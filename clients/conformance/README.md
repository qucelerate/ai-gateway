# Client conformance suite

Language-neutral test cases that every Gateway inference client must pass. The
cases are data, not code: each client's test harness reads them, serves the
canned responses from a local mock server, drives the client, and checks the
request it sent and the outcome it reported. `validate.py` checks every case
against the stable inference contract (`openapi/inference-v1.json`), so a case
cannot drift from what the gateway really sends.

The contract is stable at 1.0.0; see `apps/docs/reference/compatibility.md`.
Clients are transport wrappers: authorization, budgets, routing, and settlement
stay on the server.

## Files

- `cases/*.json`: one case per file, run in name order.
- `errors.json`: the problem `code` taxonomy: each code's HTTP status, whether
  the request was reserved, and what a client may do about it. A Rust test in
  `src/transport/inference.rs` keeps it in step with the server's codes.
- `validate.py`: validates the cases and the taxonomy. Run
  `python3 clients/conformance/validate.py`; `test_validate.py` checks that it
  rejects cases a correct client could not pass.

## Client configuration for every case

A harness constructs the client with:

- base URL: the mock server's URL (no trailing slash);
- API key: `gw_conformance_key`;
- default retry policy (see below), with the client's sleep replaced by a
  recorder, so a case runs instantly and can check the waits the client asked
  for;
- any `client` overrides the case lists (`max_retries`).

## Case format

```json
{
  "name": "chat_basic",
  "description": "What the case proves.",
  "operation": "chat | chat_stream | embeddings",
  "client": {"max_retries": 2},
  "call": {
    "body": {"model": "chat-default", "messages": [{"role": "user", "content": "hi"}]},
    "idempotency_key": "optional explicit key",
    "request_id": "optional X-Request-Id"
  },
  "expect_request": {
    "method": "POST",
    "path": "/api/inference/v1/chat/completions",
    "headers": {"authorization": "Bearer gw_conformance_key"},
    "generated_idempotency_key": true,
    "absent_headers": ["x-request-id"],
    "body": {"model": "chat-default", "messages": [{"role": "user", "content": "hi"}]}
  },
  "responses": [
    {"status": 200, "headers": {"x-request-id": "req-1"}, "json": {}},
    {"status": 200, "headers": {}, "sse": "fixtures/name.sse"},
    {"status": 502, "headers": {"content-type": "text/html"}, "text": "<html>"}
  ],
  "expect": {
    "attempts": 1,
    "same_idempotency_key": true,
    "sleeps": [1],
    "result": {},
    "events": [{"event": "choice.delta", "data": {}}],
    "error": {"kind": "problem", "status": 429, "code": "rate_limit_exceeded",
              "request_id": "req-1", "retry_after": 1}
  }
}
```

- **`call`**: the client call. `body` is the request as the caller would build
  it, in wire form; a typed client builds the equivalent typed request.
  `idempotency_key` and `request_id`, when present, are passed as the call's
  options.
- **`expect_request`**: what every attempt must look like. Header names are
  lowercase and compared case-insensitively; listed values must match exactly.
  Every attempt must send `content-type: application/json` and a JSON body
  equal to `body`. With `generated_idempotency_key: true` the caller passed no
  key, and the client must send a non-empty one of at most 255 characters.
  Headers in `absent_headers` must not be sent.
- **`responses`**: served in order, one per attempt. `json` is sent with
  `content-type: application/json` (or `application/problem+json` when the
  status is 400 or more), `sse` names a file under `fixtures/` sent as
  `text/event-stream`, and `text` is sent as is. Listed headers are added.
- **`expect.attempts`**: how many requests the client must send.
  `same_idempotency_key`: every attempt carries the same key. `sleeps`: the
  waits, in seconds, the client asked for between attempts (`[]` for none).
- **`expect.result`**: for `chat` and `embeddings`, a subset of the client's
  result, compared in wire form. Every listed key must be present with an
  equal value; objects not listed are not checked; arrays must have the listed
  length and match element by element. `request_id` is the `X-Request-Id`
  response header, which a client must expose on its result.
- **`expect.events`**: for `chat_stream`, the events the client yields, in
  order, each a subset match on `event` and `data`. Unknown events are not
  yielded. The last event is the terminal one. A client exposes the stream's
  `X-Request-Id` response header, and the harness checks it against the
  served header (absent when not served).
- **`expect.error`**: the call fails. `kind` is `problem` (a Problem body was
  parsed: `status`, `code`, and `request_id` are its fields, and `retry_after`
  is the `Retry-After` header as whole seconds), `http` (a non-Problem error
  response: only `status` is checked), `stream` (a stream's terminal `error`
  event: `code` and `status` from its data), or `protocol` (the stream ended
  without a terminal event). Events listed in `expect.events` must have been
  yielded before the error.

## Limits every client enforces

A client bounds what it buffers for one unfinished stream event (16 MiB in
the clients here) and fails the stream with a protocol error past it, so a
peer cannot grow its memory without limit. Each byte of a stream is scanned
once, however small the chunks.

## Retry policy every client implements

Clients never retry a request whose outcome may have reached the provider. They
retry automatically, with the same `Idempotency-Key`, only when the problem
code's `retry` in `errors.json` is `same_key`: the gateway refused the request
before reserving anything, so the key is still unused. The default is two
retries (three attempts). The wait is the `Retry-After` header when present,
else one second; a client never waits longer than 60 seconds. Every other
error surfaces at once, with `retry_after` when the response carried one.
