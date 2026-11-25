# QuCelerate AI Gateway client (Java)

A client for the [QuCelerate AI Gateway](https://gateway.qucelerate.com)
inference API, contract 1.0.0: chat, streaming chat, and embeddings, each
with a blocking and a `CompletableFuture` form. It is a transport wrapper;
authorization, budgets, routing, and settlement stay on the gateway. It
needs Java 17 or later and depends only on `java.net.http` and Jackson.

```xml
<dependency>
  <groupId>com.qucelerate</groupId>
  <artifactId>qucelerate-ai-gateway</artifactId>
  <version>2026.11.0-rc.4</version>
</dependency>
```

The classes are in `com.qucelerate.aigateway` (automatic module name
`com.qucelerate.aigateway`).

```java
GatewayClient client = GatewayClient.builder("https://gateway.example.com")
        .apiKey(System.getenv("GATEWAY_API_KEY"))
        .build();
ChatRequest request = new ChatRequest("chat-default", List.of(Message.user("Say hello.")));

ChatResponse response = client.chat(request);
System.out.println(response.content());

try (ChatStream stream = client.chatStream(request)) {
    for (StreamEvent event : stream) {
        if (event instanceof StreamEvent.ChoiceDelta delta && delta.contentDelta() != null) {
            System.out.print(delta.contentDelta());
        }
    }
}

client.chatAsync(request).thenAccept(answer -> System.out.println(answer.content()));
```

For reactive code, `stream.publisher(executor)` adapts a stream to a
`java.util.concurrent.Flow.Publisher<StreamEvent>` that reads on the
executor as its subscriber requests events and sends exactly one terminal
signal; Reactor and RxJava wrap a `Flow.Publisher`
directly (`JdkFlowAdapter`, `FlowAdapters`).

`examples/Chat.java` sends one chat call and one streamed call:

```sh
mvn -q package
mvn -q dependency:build-classpath -Dmdep.includeScope=runtime -Dmdep.outputFile=classpath.txt
javac -d target/examples -cp "target/classes:$(cat classpath.txt)" examples/Chat.java
GATEWAY_URL=https://gateway.example.com GATEWAY_API_KEY=gw_... \
  java -cp "target/examples:target/classes:$(cat classpath.txt)" Chat chat-default "Say hello."
```

## Behavior

- **Idempotency.** Every call sends an `Idempotency-Key`: a generated UUID,
  or `CallOptions.idempotencyKey(...)` when you set one. A retry of the call
  reuses it, so the gateway never runs or charges the call twice.
- **Retries.** The client retries automatically only when the gateway
  refused the request before reserving anything (`rate_limit_exceeded`,
  `concurrency_limit_exceeded`, `provider_circuit_open`), at most
  `maxRetries` times (default 2), waiting `Retry-After` (at most 60 seconds,
  1 second when absent). Anything that may have reached a provider is
  thrown, never retried. See `clients/conformance/errors.json` for every
  code.
- **Errors.** Every exception extends the unchecked `GatewayException`.
  `ProblemException` carries the gateway's problem: `status()`, `code()` (an
  open string), `requestId()` and `retryAfter()`. `HttpStatusException` is an
  error response without a problem body. A stream that ends in an `error`
  event throws `StreamException`; one that closes early or sends an event
  over 16 MiB throws `ProtocolException`; a malformed body or event is
  `DecodeException`; network failures and timeouts are `TransportException`
  (`timedOut()` tells them apart); `ConfigException` is an invalid option.
  Async calls complete exceptionally with the same exceptions.
- **Compatibility.** Unknown response fields and stream events are ignored,
  as contract 1.x requires. `finishReason()` and `code()` are open strings.
- **Cancellation and timeouts.** Each attempt of a non-streaming call times
  out after 120 seconds by default (`Builder.timeout`). A stream's response
  must start within the same limit; after that there is no limit between
  events. Close a stream (or leave its try-with-resources block) to cancel
  it, from any thread. Cancelling an async call's future, or interrupting a
  blocking call, aborts its request or its wait before a retry; a stream
  that arrives afterwards is closed.
- **Limits.** A successful non-streaming body over 64 MiB fails with
  `DecodeException` (`Builder.maxResponseBytes`); an error body is read up
  to 1 MiB.
- **Request IDs.** `ChatResponse.requestId()`, `EmbeddingResponse.requestId()`
  and `ChatStream.requestId()` hold the `X-Request-Id` response header; quote
  it when reporting a problem. `CallOptions.withRequestId` sends your own.
- **Secrets.** The API key is never logged; `GatewayClient.toString()`
  redacts it.

## Tests

`mvn verify` compiles with every lint warning as an error and runs the unit
tests and `ConformanceTest`, which serves every case of `clients/conformance`
from a local server and checks the requests this client sends and the
results it returns, through both the blocking and the asynchronous API.

A plain `mvn deploy` publishes nothing (`maven.deploy.skip`): releases go
to Maven Central from the public repository's release workflow, with the
`release` profile (sources and javadoc jars, GPG signatures and the Central
Portal plugin). `mvn install` installs a local build.
