---
layout: home
title: QuCelerate AI Gateway
titleTemplate: Self-hosted LLM gateway with budgets, catalog releases and tracing
hero:
  name: QuCelerate AI Gateway
  text: One API in front of every model provider you use.
  tagline: A self-hosted, open-source (MIT) gateway written in Rust. It authenticates every request, holds its cost against budgets and rate limits before calling the provider, routes it through a versioned catalog, and records what happened.
  actions:
    - theme: brand
      text: Install it
      link: /install/agents
    - theme: alt
      text: How it works
      link: /architecture/
    - theme: alt
      text: Get help
      link: /support/
features:
  - title: Seven provider families
    details: OpenAI, Anthropic, Gemini (API or Agent Platform), AWS Bedrock, Azure OpenAI, Azure AI Foundry and any OpenAI-compatible server, behind one versioned chat, streaming and embeddings API.
    link: /architecture/providers
    linkText: Providers and adapters
  - title: Budgets and rate limits at every level
    details: Organization, team, service account, API key and each end user of an app. Requests, tokens and concurrency per minute. A request's largest possible cost is reserved before the call and settled at the actual cost in integer money units.
    link: /architecture/budgets-and-rate-limits
    linkText: Budgets and rate limits
  - title: Catalog releases and prices
    details: Deployments, prices and route aliases are versioned, immutable releases. Activating one is a single switch, and so is rolling it back. Prices come from you or from the providers' public price lists.
    link: /architecture/catalog
    linkText: Catalog and releases
  - title: Tracing and usage
    details: Every request is recorded with its route, usage, cost and timing. Request and response content is kept only when an organization turns it on, encrypted before it reaches the object store.
    link: /architecture/data-and-retention
    linkText: Data and retention
  - title: Batches
    details: Upload a JSONL file and run it as a batch, on the provider's batch API where there is one. Each line is accounted as a request of its own.
    link: /architecture/providers
    linkText: Batches and providers
  - title: Admin console and agent tooling
    details: A web console for people, and for automation, agent tokens, the gwctl CLI and MCP tools. Each goes through the same permission checks and audit log.
    link: /architecture/security
    linkText: Security model
  - title: Clients in five languages
    details: Python, Rust, Go, Java and C/C++ clients, checked against one conformance suite. Any HTTP client works too.
    link: /clients/
    linkText: Client libraries
  - title: Self-hosted, MIT licensed
    details: Docker Compose for one machine, a Helm chart for Kubernetes, and PostgreSQL as the system of record. There is no hosted service and no commercial edition.
    link: /architecture/
    linkText: Architecture overview
---

## Quickstart

On a machine with Docker, git and openssl, the [installation runbook](/install/agents.md) goes from clone to first request. In short:

```bash
git clone https://github.com/qucelerate/ai-gateway.git && cd ai-gateway
cp .env.example .env          # then replace each REPLACE_WITH_… placeholder
docker compose --profile app up --build -d
curl -fsS http://localhost:8080/healthz             # {"status":"ok"}
docker compose --profile app exec gateway gateway doctor
```

Then create an organization, a catalog release, a key and a budget. The runbook gives every command and the output to expect.

## What it is, and what it is not

- **It is a control point.** Every request is authenticated, admitted against budgets and rate limits, routed through the active catalog release, and settled. Accounting is exact within the gateway: integer money units, recorded in PostgreSQL transactions.
- **A budget is not a ceiling on a provider's invoice.** If a provider times out after it has accepted a request, the gateway keeps the full reservation and records an item to reconcile, but the provider may still bill the request. Calls made outside the gateway are not counted at all. Set spending limits with each provider as well. [Budgets and their limits](/operations/budgets.md) lists every case.
- **It runs where you run it.** There is no hosted service and no commercial offering. The source, releases and issue tracker are in the `qucelerate/ai-gateway` repository on GitHub (the icon at the top of every page). See [getting help](/support/index.md).

## Choose a path

- **Installing:** the [installation runbook](/install/agents.md), written for people and AI coding agents alike.
- **Understanding the design:** the [architecture section](/architecture/index.md), from the [request lifecycle](/architecture/request-lifecycle.md) to [accounting](/architecture/accounting.md) and [scaling](/architecture/scaling.md).
- **Calling the API:** the [quickstarts](/quickstarts/index.md), the [HTTP reference](/reference/index.md) and the [client libraries](/clients/index.md).
- **Operating it:** [deployment](/operations/index.md), [reliability signals](/operations/signals.md) and [security](/security/index.md).
- **Something is wrong:** the [troubleshooting guide](/support/troubleshooting.md) and the [error-code reference](/support/errors.md).

::: tip For AI agents
This site publishes `/llms.txt`, an index of every page, and `/llms-full.txt`, all pages in one Markdown file. Every page is also available as Markdown at its own path with `.md` added, such as `/install/agents.md`. Start with the [installation runbook](/install/agents.md).
:::
