# QuCelerate AI Gateway client (Go)

A client for the [QuCelerate AI Gateway](https://gateway.qucelerate.com)
inference API, contract 1.0.0: chat, streaming chat, and embeddings. It is a
transport wrapper; authorization, budgets, routing, and settlement stay on
the gateway. It uses only the standard library and needs Go 1.22 or later.

```sh
go get github.com/qucelerate/ai-gateway/clients/go@v0.202611.0
```

The client is released with the gateway, whose versions are calendar
versions (`YYYY.M.PATCH`). A Go module version of 2 or more needs a path
suffix, so the module's versions are `v0.YYYYMM.PATCH`: release
2026.11.0 is `v0.202611.0`, and `gateway.Version` reports `2026.11.0`.

The package is named `gateway`:

```go
import gateway "github.com/qucelerate/ai-gateway/clients/go"
```

```go
client, err := gateway.NewClient("https://gateway.example.com", os.Getenv("GATEWAY_API_KEY"))
if err != nil {
	return err
}
request := gateway.ChatRequest{
	Model:    "chat-default",
	Messages: []gateway.Message{{Role: gateway.RoleUser, Content: "Say hello."}},
}
response, err := client.Chat(ctx, request)
if err != nil {
	return err
}
fmt.Println(response.Content())

stream, err := client.ChatStream(ctx, request)
if err != nil {
	return err
}
defer stream.Close()
for stream.Next() {
	if delta, ok := stream.Event().(gateway.ChoiceDelta); ok && delta.ContentDelta != nil {
		fmt.Print(*delta.ContentDelta)
	}
}
if err := stream.Err(); err != nil {
	return err
}
```

`go run ./examples/chat` runs the same against `GATEWAY_URL` with
`GATEWAY_API_KEY`.

## Behavior

- **Idempotency.** Every call sends an `Idempotency-Key`: a generated UUID,
  or the key you pass with `WithIdempotencyKey`. A retry of the call reuses
  it, so the gateway never runs or charges the call twice.
- **Retries.** The client retries automatically only when the gateway
  refused the request before reserving anything (`rate_limit_exceeded`,
  `concurrency_limit_exceeded`, `provider_circuit_open`), at most
  `WithMaxRetries` times (default 2), waiting `Retry-After` (at most 60
  seconds, 1 second when absent). Anything that may have reached a provider
  is returned as an error, never retried. See
  `clients/conformance/errors.json` for every code.
- **Errors.** Match them with `errors.As` and `errors.Is`. `*ProblemError`
  carries the gateway's problem: `Status`, `Code` (an open string),
  `RequestID`, and `RetryAfter` (-1 when absent). `*HTTPError` is an error
  response without a problem body. A stream that ends in an `error` event
  fails with `*StreamError`; one that closes early or sends an event over
  16 MiB fails with `ErrProtocol`; a malformed body is `*DecodeError`.
  Network errors come from `net/http` unchanged.
- **Compatibility.** Unknown response fields and stream events are ignored,
  as contract 1.x requires. `FinishReason` and `Code` are open strings.
- **Cancellation and timeouts.** Cancel the context to cancel a call or
  close a stream. Each attempt of a non-streaming call times out after 120
  seconds by default (`WithTimeout`). A stream's response must start within
  the same limit (`ErrTimeout`); after that there is no limit between events.
  With `WithHTTPClient`, leave the `http.Client`'s `Timeout` at zero, or
  streams are cut off.
- **Request IDs.** `ChatResponse.RequestID`, `EmbeddingResponse.RequestID`
  and `Stream.RequestID` hold the `X-Request-Id` response header; quote it
  when reporting a problem. `WithRequestID` sends your own.
- **Secrets.** The API key is never logged; `Client` formats with it
  redacted.
- **Limits.** A successful non-streaming body over 64 MiB fails with a
  `*DecodeError` (`WithMaxResponseBytes`); an error body is read up to 1 MiB;
  a stream event over 16 MiB fails with `ErrProtocol`.

## Tests

`go test ./...` runs the unit tests and `conformance_test.go`, which serves
every case of `clients/conformance` from a local server and checks the
requests this client sends and the results it returns.

This module is not published; vendor it or use a `replace` directive.
