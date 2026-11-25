# QuCelerate AI Gateway

One OpenAI-style API in front of many model providers, with exact budgets,
rate limits, a priced model catalog and tracing, run on your own
infrastructure.

**Documentation:** <https://gateway.qucelerate.com>

QuCelerate AI Gateway is a self-hosted LLM gateway written in Rust. Your apps
call one versioned HTTP API (`/api/inference/v1/`: chat, streaming chat with
tools, embeddings and batches); the gateway authenticates the caller, checks
its budgets and rate limits, routes the request to a deployment on one of the
organization's provider accounts, and records what it cost.

## What it does

- **Providers.** Anthropic, OpenAI, Azure OpenAI and Azure AI Foundry, Google
  Gemini (Gemini API and Vertex AI / Agent Platform), AWS Bedrock, and any
  on-premises or hosted endpoint that speaks the OpenAI API. Cloud accounts
  connect without long-lived keys (AWS role, Azure app consent, Google
  service-account impersonation) where the operator has set that cloud up;
  API keys are stored encrypted.
- **Budgets and rate limits** at the organization, team, service account,
  API key and end-user level. Every request reserves its largest expected
  cost against each budget that covers it before it is sent, and is settled
  at its actual cost afterwards, so concurrent requests cannot overspend a
  budget the gateway can see. End users of an app can have monthly plans and
  purchased credits.
- **Catalog** of models, aliases and deployments, with versioned releases
  and prices; the gateway can discover what each provider account can call
  and offer it once it is priced. Model cards carry sourced facts about each
  model and each provider's data handling.
- **Tracing**: per-request timing always, and request and response content,
  encrypted, when an organization turns it on.
- **Batches** across providers, with the same budgets and accounting.
- **Admin console** (`apps/admin`): organizations, teams, identities and
  sign-in (local accounts or OIDC), connections, catalog, budgets, usage
  reports and exports, webhooks, audit.
- **Client libraries** for Rust, Python, Go, Java and C/C++ (`clients/`),
  experimental until 1.0. Any HTTP client works too.

### Budgets are a strong control, not a hard ceiling

A budget caps what the gateway calculates, not what a provider bills.
Providers bill from their own meters: a model that produces more than its cap
allows for, a request whose outcome is unknown after a timeout, an
out-of-date catalog price, or use of the same account outside the gateway can
all make the invoice differ. **Set a spending limit or budget alert with each
provider as well**, watch the gateway's reconciliation items, and compare its
usage reports with your invoices. See
[Budgets and their limits](https://gateway.qucelerate.com/operations/budgets).

## Quickstart (Docker Compose)

Needs Docker with Compose 2.24 or later (`compose.yaml` uses `env_file` with
`required`), `openssl`, and `uuidgen` (or another way to make a UUID, for the
`Idempotency-Key` below). This runs the whole gateway on
one host, non-HA, with PostgreSQL: the gateway (`all-in-one` profile), the
admin console and a proxy that serves both on `http://localhost:8080`.

```sh
git clone https://github.com/qucelerate/ai-gateway.git
cd ai-gateway
cp .env.example .env

# Replace the placeholders in .env with fresh local values. The database
# passwords go into a URL, so keep them URL-safe (hex is).
secret() { openssl rand -base64 32 | tr '+/' '-_' | tr -d '='; }
sed -i.bak \
  -e "s/REPLACE_WITH_RANDOM_LOCAL_ADMIN_PASSWORD/$(openssl rand -hex 24)/" \
  -e "s/REPLACE_WITH_RANDOM_LOCAL_APP_PASSWORD/$(openssl rand -hex 24)/g" \
  -e "s/REPLACE_WITH_RANDOM_LOCAL_API_KEY_PEPPER/$(openssl rand -hex 32)/" \
  -e "s/REPLACE_WITH_32_BYTE_BASE64_SECRET/$(secret)/" \
  -e "s/REPLACE_WITH_32_BYTE_BASE64_KEY/$(secret)/" \
  -e "s/^# GATEWAY_CREDENTIAL_ENCRYPTION_KEY=.*/GATEWAY_CREDENTIAL_ENCRYPTION_KEY=$(secret)/" \
  .env && rm .env.bak

docker compose --profile app up --build -d
```

The first build compiles the gateway and takes several minutes. When the
stack is up, the gateway logs a one-time setup link (valid for 24 hours):

```sh
docker compose logs gateway | grep 'first-run setup'
```

Open the link to create the first organization and its owner, with an email
address and a password; no identity provider is needed. (If the link has
expired, `docker compose exec gateway /usr/local/bin/gateway setup-link`
makes another.) Then, in the console at <http://localhost:8080>:

1. **Connections → Connect a provider**: add a provider account (an Anthropic
   or OpenAI API key, preferably one used only by the gateway and with a spend
   limit, or an OpenAI-compatible endpoint) and choose models, their aliases
   and prices. API keys are stored sealed under
   `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`; keep a copy of `.env`, since losing
   that key makes stored credentials unreadable.
2. **Policies → Budgets**: add an organization budget. An organization serves
   nothing until a budget covers it.
3. **Identities**: create a service account and an API key.
4. Call the API:

```sh
curl http://localhost:8080/api/inference/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Idempotency-Key: $(uuidgen)" \
  -H "Content-Type: application/json" \
  -d '{"model": "<your alias>", "messages": [{"role": "user", "content": "Hello"}]}'
```

`docker compose --profile app down` stops the stack; add `-v` to delete its
database. The [quickstarts](https://gateway.qucelerate.com/quickstarts/) cover
authentication, streaming, tools and embeddings, and
[Deployment](https://gateway.qucelerate.com/operations/) covers production
topologies (separate API and worker processes, Kubernetes, TLS and HTTP/3,
OIDC sign-in).

## Repository layout

| Path | What |
| --- | --- |
| `src/` | The gateway (Rust): API, worker, provider adapters, accounting |
| `migrations/` | PostgreSQL schema migrations |
| `openapi/` | The published OpenAPI contracts (inference and admin APIs) |
| `apps/admin/` | Admin console (React, Carbon) |
| `apps/docs/` | Documentation site (VitePress), published at gateway.qucelerate.com |
| `clients/` | Client libraries: Rust, Python, Go, Java, C/C++ |
| `model-cards/` | Model cards and provider data-handling facts |
| `docker/`, `Dockerfile`, `compose.yaml` | Container images and the local stack |
| `tests/` | Integration, functional (browser) and load tests |

## Contributing, security and support

- [CONTRIBUTING.md](CONTRIBUTING.md): building, testing, sign-off, and how
  pull requests are merged and released.
- [SECURITY.md](SECURITY.md): report vulnerabilities privately, never in a
  public issue.
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) and [CHANGELOG.md](CHANGELOG.md).

## License

MIT; see [LICENSE](LICENSE) and, for third-party code and data,
[NOTICE](NOTICE). QuCelerate AI Gateway is open-source software provided
without warranty. There is no commercial offering or paid support: issues
and pull requests are handled by the maintainers on a best-effort basis.
