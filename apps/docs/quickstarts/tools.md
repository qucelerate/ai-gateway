# Tool calling

A chat request can define **tools**: functions the model may call. The model does not run them. It answers with the calls it wants made, your code runs them, and you send each result back so the model can continue. The shape follows OpenAI's: `tools`, `tool_choice` and `parallel_tool_calls` on the request, `tool_calls` on the assistant message, and `tool` messages that answer them.

## Define the tools

Each tool names a function, says what it does, and gives a JSON Schema for its arguments. The schema's `type` is `object`.

```sh
curl "$GATEWAY_BASE_URL/api/inference/v1/chat/completions" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 3c9d4e21-8b7a-4f60-9e15-0a2b6c8d1f47" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"What is the weather in Paris?"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"The current weather in a city.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}'
```

A model that calls a tool finishes with `finish_reason` `tool_calls`, and its message carries the calls. (The choice also lists them flat, as contract 1.0.0 did; send back the message's.) `arguments` is a JSON object, serialized, as the model wrote it: validate it against your schema before you act on it.

```json
{
  "id": "chatcmpl-01J8EXAMPLE",
  "model": "chat-default",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "",
        "tool_calls": [
          {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
        ]
      },
      "finish_reason": "tool_calls",
      "tool_calls": [{"id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}],
      "refusal": null,
      "safety": [],
      "annotations": []
    }
  ],
  "usage": {"input_tokens": 61, "output_tokens": 15}
}
```

## Send the results back

Send the conversation again: the assistant message with its `tool_calls` unchanged, then one `tool` message for each call, with the call's `id` as its `tool_call_id`. Keep the same `tools` in the request. Every call must be answered before the next user message.

```sh
curl "$GATEWAY_BASE_URL/api/inference/v1/chat/completions" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 9f1e7a3b-2c4d-4e85-b6a0-71d3c5e9f208" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"What is the weather in Paris?"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"{\"temperature_c\":21}"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"The current weather in a city.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}'
```

Treat a call id as opaque and send it back exactly. Some providers keep state in it that they need when the conversation is replayed.

## Choose how the model uses them

`tool_choice` is `auto` (the default: the model decides), `none`, `required` (at least one call), or `{"type":"function","function":{"name":"get_weather"}}` to make it call that tool. `parallel_tool_calls: false` asks for at most one call per reply. A provider that cannot honor an option refuses the request with `422` and code `tools_unsupported`, rather than ignoring it.

## Streaming

On the stream endpoint, calls arrive as `tool_call.delta` events. The first delta of a call carries its `id` and `name`; later ones carry only more of `arguments_delta`. Join the pieces by `tool_call_index`.

```text
event: response.started
data: {"model":"chat-default"}

event: tool_call.delta
data: {"choice_index":0,"tool_call_index":0,"id":"call_1","name":"get_weather","arguments_delta":"{\"city\":"}

event: tool_call.delta
data: {"choice_index":0,"tool_call_index":0,"id":null,"name":null,"arguments_delta":"\"Paris\"}"}

event: choice.finished
data: {"choice_index":0,"finish_reason":"tool_calls"}

event: response.completed
data: {"model":"chat-default","usage":null}
```

## Which models take tools

Not every deployment can take tools. Each chat deployment in the catalog says whether it does, and a model name takes tools only if every deployment it routes to does. A request with tools to one that cannot is refused with `422` and code `tools_unsupported`; the gateway never drops your tools and answers without them.

A model name whose deployments span providers with different tool call ids (Google's Gemini models beside another provider's) also refuses requests with tools, with `tools_unsupported`: a call one of them made cannot be sent back to the other. For tool calling, a model name's deployments must share a provider family.

Tool definitions are input: they count toward the request's input limit and the provider bills them as input tokens.

The admin console's **Playground** has a *Tools* section: define tools, see the calls a reply makes, enter their results to continue, and copy the same conversation as code.
