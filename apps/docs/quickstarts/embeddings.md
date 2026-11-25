# Embeddings

Pass 1–128 strings to the embeddings endpoint; each string is 1–131,072 characters, and the model name is at most 128 characters.

```sh
curl "$GATEWAY_BASE_URL/api/inference/v1/embeddings" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: c4e12a89-61d3-4b7e-8f20-0a4e7d9c2b16" \
  -d '{"model":"embedding-default","input":["A small, safe example.","Another short sentence."]}'
```

The response is a JSON list of vectors. Store vectors with the model name and any preprocessing choices needed to reproduce them.
