# Changelog

All notable changes to QuCelerate AI Gateway are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The
inference API's own compatibility promise is in the documentation's
[stability and compatibility](https://gateway.qucelerate.com/reference/compatibility)
page; the client libraries have their own changelogs in `clients/*/CHANGELOG.md`.

## [Unreleased]

The first public release, planned as 0.1.0.

### Added

- **Inference API** (`/api/inference/v1/`, stable): chat completions,
  streaming chat (server-sent events with one terminal event), tool calling,
  embeddings, and batches. Idempotency keys on every call, RFC 9457 problem
  details, and usage that says how it was counted (`exact`, `estimated`,
  `unknown`).
- **Providers**: Anthropic, OpenAI, Azure OpenAI and Azure AI Foundry, Google
  Gemini (Gemini API and Vertex AI / Agent Platform), AWS Bedrock, and
  OpenAI-compatible endpoints (on-premises or hosted). Keyless cloud
  connections (AWS role assumption, Azure app consent, Google service-account
  impersonation), encrypted stored API keys, optional AWS KMS envelope
  encryption, and failover between deployments.
- **Budgets** for organizations, teams, service accounts, API keys and the
  end users of an app (monthly plans and purchased credits), with
  reservation-based accounting: each request reserves its largest expected
  cost before it is sent and settles at its actual cost, with reconciliation
  items for outcomes the gateway cannot know.
- **Rate limits** at the same levels, with optional leases for high request
  rates.
- **Catalog**: models, aliases and deployments in versioned releases with
  prices; discovery of what each provider account can call; daily price
  comparison with the Azure Retail Prices and AWS Price List APIs; model
  agreements; model cards and provider data-handling facts with sources.
- **Tracing**: per-request timing, and encrypted request and response content
  in an S3-compatible store when an organization turns it on.
- **Admin console**: organizations, teams, members and invitations, local
  accounts and OIDC sign-in, service accounts and API keys, connections,
  catalog, budgets and rate limits, usage dashboard, reports and exports to
  object storage, webhooks, notifications and audit.
- **Operations**: `all-in-one`, `api`, `worker` and `data-plane` process
  profiles; PostgreSQL migrations; Prometheus metrics; TLS, HTTP/2 and HTTP/3
  listeners; IPv6; retention; backup and restore drill; container images and
  a Docker Compose stack; `gwctl` command-line tool.
- **Client libraries** (experimental, 0.1.0): Rust, Python, Go, Java and
  C/C++.
- **Documentation** at <https://gateway.qucelerate.com>.

[Unreleased]: https://github.com/qucelerate/ai-gateway/commits/main
