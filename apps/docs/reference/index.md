# API reference

The inference API is versioned under `/api/inference/v1/` and is **stable** at contract version 1.0.0; see [stability and compatibility](/reference/compatibility.md) for what may change within v1.

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/chat/completions` | Return a chat response |
| `POST` | `/chat/completions/stream` | Return a non-resumable SSE stream |
| `POST` | `/embeddings` | Return embedding vectors |

See [HTTP endpoints](/reference/endpoints.md) for headers and payloads, or open the [versioned OpenAPI artifact](/openapi/inference-v1.json) in a new tab.

> The artifact is a static copy of the repository's versioned `openapi/inference-v1.json`; it is not a live API explorer.
