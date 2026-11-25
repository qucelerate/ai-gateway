# Errors

Non-success responses use an RFC 9457 problem-details shape. Handle status first, then inspect the JSON fields; do not use color or message text as a control signal.

For example, an unknown model alias is documented as a `model_unavailable` problem:

```json
{
  "type": "https://gateway.invalid/problems/model_unavailable",
  "title": "Not Found",
  "status": 404,
  "detail": "model alias is not available for this operation",
  "instance": "/api/inference/v1/chat/completions",
  "request_id": "request-id",
  "code": "model_unavailable"
}
```

The API describes common statuses including `400`, `401`, `403`, `404`, `409`, `413`, `415`, `422`, `429`, `502`, `503`, and `504`. Retry only when policy allows it. A retry with the same idempotency key is safe while the record is claimable; after a terminal, non-claimable record, the same key returns HTTP `409` (`idempotency_terminal`). Reusing a key with a different request fingerprint returns HTTP `409` (`idempotency_conflict`). Repeated retries do not recover a terminal response.

A `429` with code `rate_limit_exceeded` or `concurrency_limit_exceeded` means the request would exceed a requests-per-minute, tokens-per-minute or concurrent-request limit. Rate limits apply at the levels budgets do, and every one covering a request applies:

| Level (`scope`) | Requests per minute | Tokens per minute | Requests in flight |
| --- | --- | --- | --- |
| `organization` | yes | yes | yes |
| `team` | yes | yes | yes |
| `service-account` | yes | yes | yes |
| `api-key` (your key alone) | yes | yes | yes |
| End user of an app (`end_user`, by plan) | yes | yes | yes |

The problem names the level in `scope` (or the end user in `end_user`) and the limit in `limit` (`requests_per_minute`, `tokens_per_minute` or `max_concurrent`). Nothing was reserved, so wait the `Retry-After` seconds and send the request again with the same idempotency key. A tokens-per-minute limit counts a request's input and output cap while it runs and the tokens it used once it finishes; a request that fits only with a smaller output cap is served with that cap, and one whose input (with any reasoning allowance and one output token) is more than the limit never fits (the `detail` says so): shorten it rather than retry. A `429` with `quota_exceeded` means no budget can cover the request: either the budget is exhausted, or no budget covers it at all (the `detail` then says so; a new organization has none until an administrator adds one). Retrying does not help until a budget is added, raised, or resets.

A `503` with code `provider_circuit_open` means the provider deployment behind the alias failed repeatedly, so the gateway refuses requests to it without calling the provider. Nothing was reserved; wait the `Retry-After` seconds and retry with the same idempotency key. After the wait, one request probes the deployment and closes the circuit if it succeeds.

For SSE, the terminal provider-error fixture is:

```text
event: error
data: {"code":"provider_error","status":502}
```
