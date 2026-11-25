# Changelog: qucelerate-ai-gateway (Rust)

All notable changes to this client. Each release has a `## <version>` section,
which becomes its GitHub release notes (`clients/release/changelog.py`).
Since 2026.11.0 the client is released with the gateway, under the same
calendar version (YYYY.M.PATCH, see the repository's CHANGELOG.md); a
`### Breaking` heading marks a change that needs code changes. The client
implements the gateway's inference contract 1.0.0.

## 2026.11.0

- Released with the gateway under its calendar version: one tag, `v2026.11.0`,
  releases every component, and every client carries the gateway's version. The API is
  that of 0.2.0; nothing was removed or changed.
- Cargo treats every 2026.x version as compatible with 2026.11.0, so a caret
  requirement (`"2026.11"`) would accept any later 2026 release: pin the exact version,
  `qucelerate-ai-gateway = "=2026.11.0"`, and move it on purpose.
- bytes 1.12.2.

## 0.2.0

- Renamed for public release: the crate is `qucelerate-ai-gateway` (was `gateway-client`),
  imported as `qucelerate_ai_gateway`, and is published to crates.io. The User-Agent is
  `qucelerate-ai-gateway-rust/<version>`.
- Tool calling (D-032): `ChatRequest::tools`, `tool_choice` and `parallel_tool_calls`; `Message`
  carries `tool_calls` and `tool_call_id` (`Message::tool`, `Message::assistant_tool_calls`), and
  `MessageResponse` the model's `tool_calls`, with `to_message()`. `Role` gains `Tool` and
  `Message` two fields: code that matches `Role` exhaustively or builds `Message` literally
  needs a change.
- Batches (OpenAI's Batch API shape): `upload_batch_file`, `create_batch`, `batch`,
  `list_batches`, `cancel_batch` and `file_content`. A batch is created
  with an Idempotency-Key; its single read carries the gateway's `gateway` account
  (native or emulated jobs, cost).

## 0.1.0

The first release.

- An async client (`Client`, built with `Client::builder`) for chat, streaming
  chat and embeddings, on reqwest with rustls.
- Streams are a `Stream` of typed events. Each unfinished event is capped at
  16 MiB, and the client timeout bounds how long a stream takes to start.
- Response bodies are capped at 64 MiB (configurable), and error bodies at 1 MiB.
- Errors carry the gateway's problem code, status, request id and `Retry-After`,
  and retryable requests are retried with backoff.
- Passes the 25 conformance cases. Minimum supported Rust version: 1.85.
