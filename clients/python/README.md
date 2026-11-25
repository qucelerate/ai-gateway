# qucelerate-ai-gateway (Python)

A client for the [QuCelerate AI Gateway](https://gateway.qucelerate.com)
inference API, contract 1.0.0: chat, streaming chat, and embeddings, with a
synchronous `Client` and an asyncio `AsyncClient`. Python 3.9 or later; its
one dependency is `httpx`. It is a transport wrapper: authorization, budgets,
routing, and settlement stay on the gateway.

```sh
pip install qucelerate-ai-gateway
```

```python
import os
from qucelerate_ai_gateway import Client, ChoiceDelta

with Client("https://gateway.example.com", os.environ["GATEWAY_API_KEY"]) as client:
    response = client.chat("chat-default", [{"role": "user", "content": "Say hello."}])
    print(response.content, response.usage)

    with client.chat_stream("chat-default", [{"role": "user", "content": "Say hello."}]) as stream:
        for event in stream:
            if isinstance(event, ChoiceDelta):
                print(event.content_delta or "", end="")
```

```python
from qucelerate_ai_gateway import AsyncClient

async with AsyncClient("https://gateway.example.com", api_key) as client:
    response = await client.embeddings("embedding-default", ["first", "second"])
    vectors = response.vectors
    async with await client.chat_stream("chat-default", messages) as stream:
        async for event in stream:
            ...
```

`python examples/chat.py MODEL PROMPT` runs a chat and a stream against
`GATEWAY_URL` with `GATEWAY_API_KEY`.

## Behavior

- **Idempotency.** Every call sends an `Idempotency-Key`: a generated UUID, or
  the `idempotency_key=` you pass. Its retries reuse it, so the gateway never
  runs or charges the call twice.
- **Retries.** Only when the gateway refused the request before reserving
  anything (`rate_limit_exceeded`, `concurrency_limit_exceeded`,
  `provider_circuit_open`), at most `max_retries` times (default 2), waiting
  `Retry-After` (at most 60 seconds). Anything that may have reached a
  provider raises, never retries. `clients/conformance/errors.json` lists every
  code.
- **Errors.** All derive from `GatewayError`. `ProblemError` carries `status`,
  `code` (an open string), `request_id` and `retry_after`; `HTTPError` is an
  error response without a problem body; `StreamError` is a stream's terminal
  `error` event; `ProtocolError` is a stream that closed early or sent an
  event over 16 MiB;
  `TransportError`, `DecodeError` and `ConfigError` cover the rest. A stream
  exposes its `X-Request-Id` header as `stream.request_id`.
- **Compatibility.** Unknown response fields and stream events are ignored, as
  contract 1.x requires.
- **Cancellation and timeouts.** Close the stream (or leave its `with` block)
  to stop it; cancelling an `AsyncClient` call closes its connection.
  Non-streaming calls time out after 120 seconds by default (`timeout=`).
- **Secrets.** The API key never appears in `repr()`.
- **Limits.** A successful non-streaming body over 64 MiB raises
  `DecodeError` (`max_response_bytes=`); an error body is read up to 1 MiB; a
  stream event over 16 MiB raises `ProtocolError`.

## Tests

`pytest` runs `tests/test_conformance.py`, which serves every case of
`clients/conformance` from a local HTTP server to both clients and checks the
requests they send and what they return or raise.
