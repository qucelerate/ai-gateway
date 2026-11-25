# Changelog: QuCelerate AI Gateway client (Java)

All notable changes to this client. Each release has a `## <version>` section,
which becomes its GitHub release notes (`clients/release/changelog.py`).
Versions follow semantic versioning; the client implements the gateway's
inference contract 1.0.0.

## 0.2.0

- Renamed for public release: the artifact is `com.qucelerate:qucelerate-ai-gateway` (was
  `dev.qucelerate:gateway-client`) and the package `com.qucelerate.aigateway` (was
  `dev.qucelerate.gateway.client`; the automatic module name follows). It is published to
  Maven Central with sources and javadoc jars. The User-Agent is
  `qucelerate-ai-gateway-java/<version>`.
- Tool calling (D-032): `ChatRequest.withTools`, `withToolChoice` and `withParallelToolCalls`;
  `Message` carries `toolCalls` and `toolCallId` (`Message.tool`, `Message.assistant(content, calls)`),
  and `MessageResponse` the model's `toolCalls`, with `toMessage()`. The records gain components;
  their old constructors remain.
- Batches (OpenAI's Batch API shape): `uploadBatchFile`, `createBatch`, `batch`,
  `listBatches`, `cancelBatch` and `fileContent`, blocking and async. A batch is created
  with an Idempotency-Key; its single read carries the gateway's `gateway` account
  (native or emulated jobs, cost).

## 0.1.0

The first release, published as `dev.qucelerate:gateway-client:0.1.0`.

- `GatewayClient` for chat, streaming chat and embeddings, each with a
  blocking form and a `CompletableFuture` form. Streams are also available
  as a `Flow.Publisher`.
- Java 17 or later. It depends only on `java.net.http` and Jackson.
- Errors carry the gateway's problem code, status, request id and `Retry-After`.
  Retryable requests are retried with backoff, and cancelling a call stops its retries.
- Passes the 25 conformance cases, both synchronous and asynchronous.
