# Chat

Send a bounded conversation to the chat endpoint. The payload contains one user message and no sensitive data.

```sh
curl "$GATEWAY_BASE_URL/api/inference/v1/chat/completions" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 7b8f1c2e-4f8a-4d0c-9c1a-2e7f0b6d9a31" \
  -H "X-Request-Id: req_01J8EXAMPLE8W5Y" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"Give me one short greeting."}]}'
```

The documented request fields are `model` and `messages`; a request can also define [tools](/quickstarts/tools.md) for the model to call. Keep prompts bounded and treat returned text as untrusted content.
