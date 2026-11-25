# Authentication

Send the Bearer credential on every inference request. The installation quickstart exports it as `$GATEWAY_API_KEY`.


```sh
curl "$GATEWAY_BASE_URL/api/inference/v1/embeddings" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 2f5c5d7e-7a11-4c1f-9e12-8d61d40a4b72" \
  -d '{"model":"embedding-default","input":["hello"]}'
```

`Idempotency-Key` is required. `X-Request-Id` is optional; if supplied, use an opaque identifier such as `req_01J8EXAMPLE8W5Y`. Keep keys out of source control and logs.
