# Troubleshooting

Start with the HTTP status, the `X-Request-Id` value, and whether the response is JSON or SSE. Do not paste bearer credentials into an issue or log.

| Symptom | Check |
| --- | --- |
| `401` | Bearer header exists and the credential is valid for this deployment |
| `422` | `model`, messages, and bounded input match the documented shape |
| `409` | Reused idempotency key has different request content |
| `429` | Apply backoff and review capacity/rate limits |
| Stream ends early | Treat it as non-resumable; inspect the terminal event and retry policy |
| `502`–`504` | Record request ID and investigate upstream or gateway health |

For exact fields, compare against the [OpenAPI artifact](/openapi/inference-v1.json).
