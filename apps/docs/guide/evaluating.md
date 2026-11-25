# Evaluating QuCelerate AI Gateway

This page is for deciding whether QuCelerate AI Gateway fits your organization. It says what the gateway does and does not do, how to try it, and what to check before relying on it.

## What it is

An inference gateway your organization runs itself. Applications call one versioned HTTP API, [`/api/inference/v1/`](/reference/index.md), with an API key; the gateway routes each call to a model deployment on your own provider accounts (OpenAI, Anthropic, Azure OpenAI, Azure AI Foundry, Amazon Bedrock, Google Gemini and Agent Platform, and OpenAI-compatible endpoints), and meters it against budgets and rate limits before it reaches the provider.

It is open-source software under the MIT license, with no commercial support offering, no hosted service and no warranty.

## What it does not do

- It does not hold or resell model access. Every call runs on provider accounts and credentials your organization connects.
- It never creates deployments, accepts model terms or requests quota in your cloud accounts. It shows which models an account can call, and what blocks the rest ([providers & capabilities](/guide/providers.md)).
- Budgets bound what the gateway calculates, not the provider's invoice; see [budgets and their limits](/operations/budgets.md).
- It does not fail over between deployments by itself, and it has no hosted playground or request console.

## How to try it

The repository's Docker Compose `app` profile runs the whole gateway on one host, non-HA, with PostgreSQL: the gateway in its `all-in-one` [profile](/operations/index.md#profiles), the admin UI and a proxy that serves both on one origin. Copy `.env.example` to `.env`, replace its placeholders, and run `docker compose --profile app up --build`; the [installation runbook](/install/agents.md) walks through it. Then connect a provider account in the admin UI, create a service account and an API key, and follow the [quickstarts](/quickstarts/index.md).

## What to check

| Question | Where |
| --- | --- |
| Will the contract stay put? | [Stability and compatibility](/reference/compatibility.md): version 1 changes only compatibly. |
| What does each label mean? | [Stability labels](/guide/concepts.md#stability-labels) |
| How is usage counted when a provider reports none? | [Usage labels](/guide/concepts.md#usage-says-how-it-was-counted) and [budgets and their limits](/operations/budgets.md) |
| How does it run in production? | [Deployment & operations](/operations/index.md), [backup and recovery](/operations/recovery.md), [rollback and incidents](/operations/incidents.md) |
| How are credentials and content handled? | [Security & privacy](/security/index.md) |
| Is there a library for my language? | [Client libraries](/clients/index.md) (experimental) |
