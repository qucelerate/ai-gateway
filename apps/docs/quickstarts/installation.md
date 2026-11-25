# Installation

The gateway is an HTTP API, so calling it needs no runtime package. Use curl or an HTTP client in your language of choice, or one of the [client libraries](/clients/index.md). This page sets up a client. To install the gateway itself, follow the [installation runbook](/install/agents.md).

```sh
curl --version
```

Set the host and credential in your local shell. Do not commit credentials.

```sh
export GATEWAY_BASE_URL="https://gateway.example.com"
export GATEWAY_API_KEY="gw_example_replace_me"
```

Continue to [authentication](/quickstarts/authentication.md).
