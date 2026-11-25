# Client libraries

**Status: experimental.** The clients are at version 0.1.0. Their APIs may change before 1.0; the HTTP contract they call is [stable](/reference/compatibility.md).

The gateway is an HTTP API, so a client library is optional: anything that sends the documented requests works. The libraries save writing the idempotency, retry, error and SSE handling yourself. Each one is a transport wrapper; authorization, budgets, routing and settlement stay on the gateway.

| Language | Needs | Package |
| --- | --- | --- |
| Rust | Rust 1.85 or later, async (Tokio) | crate `qucelerate-ai-gateway` (crates.io) |
| Python | Python 3.9 or later; depends on `httpx` | `qucelerate-ai-gateway` (PyPI), imported as `qucelerate_ai_gateway` |
| Go | Go 1.22 or later; standard library only | module `github.com/qucelerate/ai-gateway/clients/go` |
| Java | Java 17 or later; `java.net.http` and Jackson | `com.qucelerate:qucelerate-ai-gateway` (Maven) |
| C and C++ | A C11 or C++17 compiler, CMake, libcurl | the library's source, as a `.tar.gz` release artifact |

The C library, `libgateway`, has a small ABI-stable API; the C++ wrapper is header-only and built on it.

## Getting a client

Install a client with your language's own tools:

```bash
pip install qucelerate-ai-gateway                       # Python
cargo add qucelerate-ai-gateway                         # Rust
go get github.com/qucelerate/ai-gateway/clients/go      # Go
```

For Java, add the dependency `com.qucelerate:qucelerate-ai-gateway` to your Maven or Gradle build. For C and C++, download the source archive from the release and build it with CMake.

Each client release is also a GitHub release of the `qucelerate/ai-gateway` repository, tagged `client-<language>-v<version>` (for example `client-python-v0.1.0`). It holds the artifacts, a `SHA256SUMS` file, the release's compatibility results and the changelog section.

Every artifact is signed: it comes with a cosign bundle holding a signed [SLSA v1 provenance](https://slsa.dev/provenance/v1) statement (its digest, the repository, the tagged commit and the workflow run that built it), and `SHA256SUMS` is signed too. The repository's `clients/release/verify.sh` checks a downloaded release against the project's public key, `clients/release/cosign.pub`. Nothing is recorded in a public transparency log, so trust rests on that key.

## What every client does

- **Idempotency.** Every call sends an `Idempotency-Key`, generated unless you pass one, and its retries reuse it, so the gateway does not run or charge the call twice while it keeps the call's record. A deployment's usage retention deletes old records; after that, the same request and key can run, and be charged, again.
- **Retries.** A client retries only a request the gateway refused before reserving anything (`rate_limit_exceeded`, `concurrency_limit_exceeded`, `provider_circuit_open`), at most twice by default, after `Retry-After`. A request that may have reached a provider is never retried; it returns an error.
- **Errors.** [Problem details](/quickstarts/errors.md) become typed errors carrying the status, the `code` (an open string), the request ID and `Retry-After`.
- **Streaming.** A stream ends with one terminal event; closing or cancelling it closes the connection. A stream that ends without one is a protocol error.
- **Compatibility.** Unknown response fields and stream events are ignored, as version 1 of the contract requires.
- **Limits and secrets.** Response bodies and stream events are size-capped, and the API key never appears in a client's debug output.

## Compatibility testing

Before a client is released, and daily, CI runs every client's example (one chat call and one streamed call) against the gateway versions it supports. A client release carries those results. See each client's `README.md` and `CHANGELOG.md` in the repository's `clients/` directory for its API and changes.
