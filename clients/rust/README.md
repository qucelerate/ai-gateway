# qucelerate-ai-gateway (Rust)

An async client for the [QuCelerate AI Gateway](https://gateway.qucelerate.com)
inference API, contract 1.0.0: chat, streaming chat, and embeddings. It is a
transport wrapper; authorization, budgets, routing, and settlement stay on
the gateway.

```sh
cargo add qucelerate-ai-gateway
```

```rust
use futures_util::StreamExt;
use qucelerate_ai_gateway::{ChatRequest, Client, Message, StreamEvent};

let client = Client::builder("https://gateway.example.com")
    .api_key(std::env::var("GATEWAY_API_KEY")?)
    .build()?;

let request = ChatRequest::new("chat-default", vec![Message::user("Say hello.")]);
let response = client.chat(&request).await?;
println!("{}", response.content().unwrap_or_default());

let mut stream = client.chat_stream(&request).await?;
while let Some(event) = stream.next().await {
    if let StreamEvent::ChoiceDelta(delta) = event? {
        print!("{}", delta.content_delta.unwrap_or_default());
    }
}
```

`cargo run --example chat` runs the same against `GATEWAY_URL` with
`GATEWAY_API_KEY`.

## Behavior

- **Idempotency.** Every call sends an `Idempotency-Key`: a generated UUID,
  or `RequestOptions::idempotency_key` when you set one. A retry of the call
  reuses it, so the gateway never runs or charges the call twice.
- **Retries.** The client retries automatically only when the gateway
  refused the request before reserving anything (`rate_limit_exceeded`,
  `concurrency_limit_exceeded`, `provider_circuit_open`), at most
  `max_retries` times (default 2), waiting `Retry-After` (at most 60
  seconds). Anything that may have reached a provider is returned as an
  error, never retried. See `clients/conformance/errors.json` for every code.
- **Errors.** `Error::Problem` carries the gateway's problem: `status`,
  `code` (an open string), `request_id`, and `retry_after`. A stream that
  ends in an `error` event is `Error::Stream`; one that closes early is
  `Error::Protocol`, as is a stream event over 16 MiB. A stream exposes its
  `X-Request-Id` header as `ChatStream::request_id()`.
- **Compatibility.** Unknown response fields and stream events are ignored,
  as contract 1.x requires. Response types are `#[non_exhaustive]`.
- **Cancellation.** Drop the future or the stream to cancel and close the
  connection. Non-streaming calls time out after 120 seconds by default
  (`ClientBuilder::timeout`).
- **Secrets.** The API key is never logged; `Client`'s `Debug` redacts it.
- **Limits.** A successful non-streaming body over 64 MiB fails with
  `Error::Decode` (`ClientBuilder::max_response_bytes`); an error body is
  read up to 1 MiB; a stream event over 16 MiB ends the stream.

## Tests

`cargo test` runs the unit tests and `tests/conformance.rs`, which serves
every case of `clients/conformance` from a local server and checks the
requests this client sends and the results it returns.
