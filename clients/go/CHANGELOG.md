# Changelog: QuCelerate AI Gateway client (Go)

All notable changes to this client. Each release has a `## <version>` section,
which becomes its GitHub release notes (`clients/release/changelog.py`).
Since 2026.11.0 the client is released with the gateway, under the same
calendar version (YYYY.M.PATCH, see the repository's CHANGELOG.md); a
`### Breaking` heading marks a change that needs code changes. The client
implements the gateway's inference contract 1.0.0.

## 2026.11.0-rc.4

- Released with the gateway under its calendar version: one tag, `v2026.11.0`,
  releases every component, and every client carries the gateway's version. The API is
  that of 0.2.0; nothing was removed or changed.
- A Go module cannot have a major version of 2 or more without a path suffix, so the module
  version is `v0.202611.0` (the tag `clients/go/v0.202611.0`):
  `go get github.com/qucelerate/ai-gateway/clients/go@v0.202611.0`. `gateway.Version` and
  the User-Agent say `2026.11.0`.

## 0.2.0

- Renamed for public release: the module is `github.com/qucelerate/ai-gateway/clients/go`
  (was `github.com/qucelerate/gateway/clients/go`); the package name stays `gateway`. The
  Go module proxy serves it from `clients/go/v<version>` tags. The User-Agent is
  `qucelerate-ai-gateway-go/<version>`.
- Tool calling (D-032): `ChatRequest.Tools`, `ToolChoice` and `ParallelToolCalls`; `Message`
  carries `ToolCalls` and `ToolCallID`, and `MessageResponse` the model's `ToolCalls`, with
  `ToMessage()`. `RoleTool` and `Bool` are new.
- Batches (OpenAI's Batch API shape): `UploadBatchFile`, `CreateBatch`, `Batch`,
  `ListBatches`, `CancelBatch` and `FileContent`. A batch is created
  with an Idempotency-Key; its single read carries the gateway's `gateway` account
  (native or emulated jobs, cost).

## 0.1.0

The first release.

- `gateway.NewClient` for chat, streaming chat and embeddings, using only the
  standard library. Go 1.22 or later.
- Streams are read event by event. Each unfinished event is capped at 16 MiB,
  and the client timeout bounds how long a stream takes to start.
- Response bodies are capped at 64 MiB (configurable), and error bodies at 1 MiB.
- Errors carry the gateway's problem code, status, request id and `Retry-After`,
  and retryable requests are retried with backoff.
- Passes the 25 conformance cases, on 64-bit and 32-bit platforms.
