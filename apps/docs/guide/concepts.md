# Concepts

## Versioned resources

Use `/api/inference/v1/` as the stable prefix. A version is part of the URL, not a header or a model name.

## A request has an owner

`Authorization` identifies the caller. `Idempotency-Key` lets the gateway recognize a retry of the same operation and is required for documented inference calls. `X-Request-Id` is optional and useful for correlating a response with your logs.

## Responses are typed

Successful JSON responses have endpoint-specific shapes. Failed HTTP responses use [RFC 9457-style problem details](/quickstarts/errors.md). Streaming responses use named SSE events and end with one terminal event.

## Usage says how it was counted

A response's `usage` carries token counts and two labels. `quality` is the least certain of the counts: `exact` when the provider reported every one, `estimated` when any was estimated rather than reported, and `unknown` when any could not be counted (that count is `null`, never `0`); `completeness` is `complete`, `partial` (for example a stream that ended early) or `unavailable`. Do not treat an estimated or unknown count as what the provider will bill.

## Stability labels

Each page that describes something whose contract may still change says so near its top:

- **Stable:** changes only compatibly within its version. The inference API under `/api/inference/v1/` is stable; see [stability and compatibility](/reference/compatibility.md).
- **Experimental:** may change or be removed in any release, without a new version. The [client libraries](/clients/index.md) are experimental until their 1.0.

Pages without a label describe behavior, operations or policy rather than a versioned interface. Text marked **Scoped** says what a page deliberately does not promise.
