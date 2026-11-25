# Changelog

All notable changes to QuCelerate AI Gateway are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

One tag, `v<version>`, releases everything: the gateway and admin console
images, the Helm chart, the documentation site and the five client
libraries, all carrying the same version. Versions are calendar versions,
`YYYY.M.PATCH`: the year, the month without a leading zero (`2026.11.0`,
`2027.1.0`), then the number of the release within that month, from 0; a
pre-release appends `-rc.N`. The version says when a
release was made, not what it breaks: a change that needs operators or
client users to act is listed under a `### Breaking` heading. The inference
API's own compatibility promise is in the documentation's
[stability and compatibility](https://gateway.qucelerate.com/reference/compatibility)
page; each client's changes are also in `clients/*/CHANGELOG.md`.

## [Unreleased]

## [2026.11.0-rc.2] - 2026-11-25

The first public release.

### Added

- **Inference API** (`/api/inference/v1/`, stable): chat completions,
  streaming chat (server-sent events with one terminal event), tool calling,
  embeddings, and batches (native Bedrock and Gemini batch jobs, or emulated
  for any deployment). Idempotency keys on every call, RFC 9457 problem
  details, and usage that says how it was counted (`exact`, `estimated`,
  `unknown`).
- **Providers**: Anthropic, OpenAI, Azure OpenAI and Azure AI Foundry, Google
  Gemini (Gemini API and Vertex AI / Agent Platform), AWS Bedrock, and
  OpenAI-compatible endpoints (on-premises or hosted). Keyless cloud
  connections (AWS role assumption, Azure app consent, Google service-account
  impersonation), encrypted stored API keys, optional AWS KMS envelope
  encryption, Bedrock model agreements accepted from the console, and model
  access guidance for all three clouds.
- **Routing**: an alias routed across weighted, prioritized deployments, with
  failover to the next deployment when one refuses a request, deployment
  health in the route editor, and a routing policy for the providers and
  locations an organization's requests may go to.
- **Budgets** for organizations, teams, service accounts, API keys and the
  end users of an app (monthly plans and purchased credits), with
  reservation-based accounting: each request reserves its largest expected
  cost before it is sent and settles at its actual cost (prompt-cache and
  cached-input prices included), with reconciliation items for outcomes the
  gateway cannot know. Budgets bound spend as it is settled; they are not a
  hard ceiling on what a provider can charge.
- **Rate limits** at every budget level (requests and tokens per minute,
  concurrency per plan), with optional leases for high request rates.
- **Catalog**: models, aliases and deployments in versioned releases with
  prices; master model records across providers with route proposals;
  discovery of what each provider account can call; daily price comparison
  with the Azure Retail Prices and AWS Price List APIs; model cards and
  provider data-handling facts with sources.
- **Tracing**: per-request timing, and encrypted request and response content
  (chat and embeddings) in an S3-compatible store when an organization turns
  it on. An optional ClickHouse analytics sink, off unless configured.
- **Admin console**: organizations, teams, members and invitations, local
  accounts and OIDC sign-in, service accounts and API keys, connections,
  catalog and models across providers, budgets and rate limits, end users,
  usage dashboard and batches, reports and exports to object storage,
  webhooks, notifications and audit, and a playground for chat and
  embeddings that compares models and continues in code for every client.
- **Operations**: `all-in-one`, `api`, `worker` and `data-plane` process
  profiles; PostgreSQL migrations; Prometheus metrics; TLS, HTTP/2 and HTTP/3
  listeners; IPv6; retention; backup and restore drill; `gateway doctor`;
  container images, a Helm chart (`oci://ghcr.io/qucelerate/charts/ai-gateway`)
  and a Docker Compose stack; the `gwctl` command-line tool and its MCP
  server; a load and resilience test harness.
- **Client libraries** for Rust, Python, Go, Java and C/C++, at the
  gateway's version (see `clients/*/CHANGELOG.md`): chat, streaming chat,
  tool calling, embeddings and batches.
- **Documentation** at <https://gateway.qucelerate.com>.
- **Releases**: container images and the chart signed with cosign
  (keyless), client artifacts signed with the project's key and carrying
  SLSA provenance; one tag publishes everything.

[Unreleased]: https://github.com/qucelerate/ai-gateway/compare/v2026.11.0...main
[2026.11.0-rc.2]: https://github.com/qucelerate/ai-gateway/releases/tag/v2026.11.0-rc.2