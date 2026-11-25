# Reliability signals

Use status and timing as machine-readable signals. Keep dashboards and alerts separate from request content.

| Signal | Meaning to investigate |
| --- | --- |
| `2xx` | Request completed; validate the response shape |
| `4xx` | Caller, payload, quota, or routing issue |
| `429` | Rate or capacity pressure |
| `5xx` | The gateway or an upstream/provider issue |
| `X-Request-Id` | Correlation value when returned or supplied |

Streaming clients should record whether the terminal event was `response.completed` or `error` and close the connection in either case.
