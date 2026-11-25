# Client libraries

**Status: experimental.** The clients are released with the gateway and carry its version (see [versions](#versions)). Their APIs may still change; a change that needs code changes is listed under **Breaking** in the release notes. The HTTP contract they call is [stable](/reference/compatibility.md).

The gateway is an HTTP API, so a client library is optional: anything that sends the documented requests works. The libraries save writing the idempotency, retry, error and SSE handling yourself. Each one is a transport wrapper; authorization, budgets, routing and settlement stay on the gateway.

| Language | Needs | Package |
| --- | --- | --- |
| Rust | Rust 1.85 or later, async (Tokio) | crate `qucelerate-ai-gateway` (crates.io) |
| Python | Python 3.10 or later; depends on `httpx` | `qucelerate-ai-gateway` (PyPI), imported as `qucelerate_ai_gateway` |
| Go | Go 1.22 or later; standard library only | module `github.com/qucelerate/ai-gateway/clients/go` |
| Java | Java 17 or later; `java.net.http` and Jackson | `com.qucelerate:qucelerate-ai-gateway` (Maven) |
| C and C++ | A C11 or C++17 compiler, CMake, libcurl | the library's source, as a `.tar.gz` release artifact |

The C library, `libgateway`, has a small ABI-stable API; the C++ wrapper is header-only and built on it.

## Getting a client

Install a client with your language's own tools:

```bash
pip install qucelerate-ai-gateway==2026.11.0                      # Python
cargo add qucelerate-ai-gateway@=2026.11.0                        # Rust
go get github.com/qucelerate/ai-gateway/clients/go@v0.202611.0    # Go
```

For Java, add the dependency `com.qucelerate:qucelerate-ai-gateway:2026.11.0` to your Maven or Gradle build:

```xml
<dependency>
  <groupId>com.qucelerate</groupId>
  <artifactId>qucelerate-ai-gateway</artifactId>
  <version>2026.11.0</version>
</dependency>
```

For C and C++, download the source archive from the release and build it with CMake.

### Versions

Releases are calendar versions, `YYYY.M.PATCH`: the year, the month without a leading zero (`2026.11.0`, `2027.1.0`), then the release within that month, from 0. A newer release always has a larger version, but the number says when it was made, not whether it breaks anything, so:

- **Rust:** pin the exact version (`qucelerate-ai-gateway = "=2026.11.0"`). Cargo treats every 2026 release as compatible with any other, so `"2026.11"` would accept a later 2026 release unseen.
- **Go:** a Go module version of 2 or more needs a path suffix, so the module's versions are `v0.YYYYMM.PATCH`: release 2026.11.0 is `v0.202611.0`. The client still reports 2026.11.0 (`gateway.Version`, User-Agent).
- **Python, Java, C:** the version as it is (PyPI spells a pre-release `2026.11.0rc1`). The C library's SONAME stays `libqucelerate-ai-gateway.so.0`; it changes only if the ABI breaks.

Read the release notes' **Breaking** headings before moving to a newer release.

Every release is a GitHub release of the `qucelerate/ai-gateway` repository, tagged `v<version>` (for example `v2026.11.0`). It holds every client's artifacts, a `SHA256SUMS` file, the compatibility result of the clients against that release's gateway, and the release notes.

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
