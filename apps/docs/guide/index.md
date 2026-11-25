# Overview

QuCelerate AI Gateway is an inference gateway with a small, versioned HTTP surface. These docs cover chat completions, non-resumable chat streaming, and embeddings, the client libraries that call them, and running a deployment.

## Scope

The documented API is available below `https://gateway.example.com/api/inference/v1/` in examples. Replace the host with your deployment. The examples use placeholders rather than working credentials.

## Start here

1. [Install your HTTP client](/quickstarts/installation.md).
2. [Send a Bearer credential](/quickstarts/authentication.md).
3. Try [chat](/quickstarts/chat.md) or [embeddings](/quickstarts/embeddings.md).
4. Add [error handling](/quickstarts/errors.md) before production traffic.

Deciding whether it fits? Start with [evaluating the gateway](/guide/evaluating.md). Running your own? Follow the [installation runbook](/install/agents.md), and read the [architecture overview](/architecture/index.md).

## Deliberately not covered yet

Provider-specific tuning, tool execution, and a hosted playground are not documented here and are not implied to be available. Tenant administration and usage reports live in the gateway's admin UI and its admin API, which these public docs do not cover.
