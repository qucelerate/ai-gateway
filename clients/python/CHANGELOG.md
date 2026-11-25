# Changelog: qucelerate-ai-gateway (Python)

All notable changes to this client. Each release has a `## <version>` section,
which becomes its GitHub release notes (`clients/release/changelog.py`).
Since 2026.11.0 the client is released with the gateway, under the same
calendar version (YYYY.M.PATCH, see the repository's CHANGELOG.md); a
`### Breaking` heading marks a change that needs code changes. The client
implements the gateway's inference contract 1.0.0.

## 2026.11.0-rc.3

### Breaking

- Python 3.10 or later (`requires-python = ">=3.10"`): Python 3.9 reached its end of life
  in October 2025. Tested on 3.10 and 3.13.

### Changed

- Released with the gateway under its calendar version: one tag, `v2026.11.0`,
  releases every component, and every client carries the gateway's version. The API is
  that of 0.2.0; nothing was removed or changed.
- Supersedes 0.2.0 on PyPI: 2026.11.0 sorts after it, so `pip install --upgrade
  qucelerate-ai-gateway` moves to it, and `qucelerate-ai-gateway>=0.2` accepts it.

## 0.2.0

- Renamed for public release: the distribution is `qucelerate-ai-gateway` (was
  `gateway-client`) and the import package `qucelerate_ai_gateway` (was `gateway_client`);
  it is published to PyPI. The User-Agent is `qucelerate-ai-gateway-python/<version>`.
- Tool calling (D-032): `chat` and `chat_stream` take `tools` (`Tool` or dicts), `tool_choice` and
  `parallel_tool_calls`; `Message` carries `tool_calls` and `tool_call_id`, and `MessageResponse`
  the model's `tool_calls` (`ToolCall`), with `to_message()` to continue the conversation.
- Batches (OpenAI's Batch API shape): `upload_batch_file`, `create_batch`, `batch`,
  `list_batches`, `cancel_batch` and `file_content`, sync and async. A batch is created
  with an Idempotency-Key; its single read carries the gateway's `gateway` account
  (native or emulated jobs, cost).

## 0.1.0

The first release.

- A synchronous `Client` and an asyncio `AsyncClient` for chat, streaming chat
  and embeddings. Python 3.9 or later; the only dependency is `httpx`.
- Streams yield typed events. Each unfinished event is capped at 16 MiB, and
  the client timeout bounds how long a stream takes to start.
- Response bodies are capped at 64 MiB (configurable), and error bodies at 1 MiB.
- Errors carry the gateway's problem code, status, request id and `Retry-After`,
  and retryable requests are retried with backoff.
- Passes the 25 conformance cases.
