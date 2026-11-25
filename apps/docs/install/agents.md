---
description: "Step-by-step installation for AI agents and people: prerequisites, Docker Compose or Helm, gateway doctor, first organization, provider, key, budget and first request, each with expected output."
---

# Installation runbook

This runbook installs QuCelerate AI Gateway and takes it as far as a first successful chat request. It is written so that an AI coding agent can follow it step by step, and so that a person can read it. Each step gives the exact commands, the output that shows the step worked, and what to do when the output is different.

There are two paths:

- **[Docker Compose](#path-a-docker-compose)**: one machine, all-in-one, for evaluation and small installs. It builds the images from source.
- **[Helm](#path-b-helm-on-kubernetes)**: a Kubernetes cluster, with the published images `ghcr.io/qucelerate/ai-gateway` and `ghcr.io/qucelerate/ai-gateway-admin`.

Both paths end with the same steps: [first admin](#step-5-create-the-first-organization-and-an-agent-token), [provider](#step-6-add-a-provider-and-a-catalog-release), [key and budget](#step-7-create-a-service-account-a-key-and-a-budget), [first request](#step-8-send-the-first-request), and the [final checklist](#final-verification-checklist).

::: tip For agents
Run the commands one step at a time and compare each result with its **Expected** block before you go on. When a result differs, follow **If it differs**. If that does not resolve it, run `gateway doctor` (step 4) and look up the failing check in the [troubleshooting guide](../support/troubleshooting.md). Never print or log the values of `.env`, API keys or agent tokens. Show their names only.
:::

## Step 1: Check prerequisites

```bash
docker --version
docker compose version
git --version
openssl version
curl --version | head -n 1
jq --version
```

**Expected:** each command prints a version. You need Docker Compose 2.24 or later, because `compose.yaml` uses the `env_file` `required` key. For the Helm path, you need `kubectl`, `helm` (3.8 or later, for OCI charts) and `psql` instead of Docker Compose, and Docker only to copy `gwctl` out of the image (step 5):

```bash
kubectl version --client
helm version --short
```

**If it differs:** install the missing tool. `jq` is used only to read IDs out of JSON responses, and you can read them by eye instead. Check that ports 8080 and 5432 on `127.0.0.1` are free (`ss -ltn 'sport = :8080 or sport = :5432'` on Linux, `lsof -iTCP:8080 -iTCP:5432 -sTCP:LISTEN` on macOS). If one is taken, set `GATEWAY_HTTP_PORT` or `POSTGRES_PORT` in `.env` in step 2, and use that port in every URL below.

## Path A: Docker Compose

### Step 2: Get the source and write `.env`

```bash
git clone https://github.com/qucelerate/ai-gateway.git
cd ai-gateway
cp .env.example .env
```

Replace the placeholders with generated values. The application password appears twice (in `GATEWAY_APP_PASSWORD` and in `DATABASE_URL`), so it is generated once:

```bash
hex() { openssl rand -hex 24; }
b64url() { openssl rand -base64 32 | tr '+/' '-_' | tr -d '='; }
APP_PASSWORD=$(hex)
sed -i.bak \
  -e "s|REPLACE_WITH_RANDOM_LOCAL_ADMIN_PASSWORD|$(hex)|" \
  -e "s|REPLACE_WITH_RANDOM_LOCAL_APP_PASSWORD|${APP_PASSWORD}|g" \
  -e "s|REPLACE_WITH_RANDOM_LOCAL_API_KEY_PEPPER|$(hex)|" \
  -e "s|REPLACE_WITH_32_BYTE_BASE64_SECRET|$(b64url)|" \
  -e "s|REPLACE_WITH_32_BYTE_BASE64_KEY|$(b64url)|" \
  -e "s|^# GATEWAY_CREDENTIAL_ENCRYPTION_KEY=REPLACE_WITH_32_BYTE_BASE64URL_KEY|GATEWAY_CREDENTIAL_ENCRYPTION_KEY=$(b64url)|" \
  .env
rm .env.bak
unset APP_PASSWORD
```

These settings are what they do:

| Setting | Purpose |
| --- | --- |
| `POSTGRES_ADMIN_PASSWORD` | The local PostgreSQL superuser, used only to create the roles below |
| `GATEWAY_APP_PASSWORD` | The application role `gateway_app` that the gateway connects as |
| `API_KEY_PEPPER` | The secret mixed into every stored API key hash. If you change it, every key stops working |
| `OIDC_SESSION_HMAC_SECRET`, `OIDC_SESSION_ENCRYPTION_KEY` | Sign and encrypt admin console sessions, and turn on local (email and password) accounts |
| `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` | Seals provider credentials stored through the admin API. If you lose it, the stored credentials cannot be read |

To call a provider with a key from the environment, add it now. This example uses OpenAI. The other families and their settings are in `.env.example`.

```bash
printf 'OPENAI_API_KEY=%s\n' "$OPENAI_API_KEY" >> .env
```

**Verify:**

```bash
grep -cE '^[A-Z0-9_]+=.*REPLACE_WITH' .env
grep -E '^(API_KEY_PEPPER|GATEWAY_CREDENTIAL_ENCRYPTION_KEY|OPENAI_API_KEY)=' .env | cut -d= -f1
```

**Expected:** the first command prints `0`, because no active line still holds a placeholder. The second prints the names `API_KEY_PEPPER`, `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` and `OPENAI_API_KEY`, never their values.

**If it differs:** a count above `0` means a placeholder was not replaced. Run `grep -nE '^[A-Z0-9_]+=.*REPLACE_WITH' .env | cut -d= -f1` to see which line, and set it by hand. If `OPENAI_API_KEY` is missing, `$OPENAI_API_KEY` was empty in your shell. You can skip the provider key and add a credential in the admin console later (step 6).

### Step 3: Start the stack

```bash
docker compose --profile app up --build -d
docker compose --profile app ps --all --format '{{.Service}} {{.State}} {{.ExitCode}}'
curl -fsS http://localhost:8080/healthz
```

The first build compiles the gateway from source, which takes several minutes.

**Expected:** `postgres`, `gateway`, `admin` and `proxy` are `running`, and `migrate` is `exited 0`. The health check prints:

```text
{"status":"ok"}
```

**If it differs:**

- `migrate` exited with a non-zero code: run `docker compose --profile app logs migrate`. A password or connection error usually means `.env` changed after PostgreSQL first created its volume. The roles are created only on first start. For a throwaway install, run `docker compose --profile app down -v` (this deletes the database) and start again.
- `gateway` keeps restarting: run `docker compose --profile app logs gateway | tail -n 20`. The error names the setting at fault. See [the gateway exits at startup](../support/troubleshooting.md#the-gateway-exits-at-startup-with-missing-required-environment-variable).
- `curl` cannot connect: the proxy is not up yet, or port 8080 is taken (see step 1).

### Step 4: Run `gateway doctor`

`gateway doctor` checks the deployment and changes nothing. Run it inside the gateway container, so it sees the same settings as the gateway:

```bash
docker compose --profile app exec gateway gateway doctor
```

**Expected:** every check line starts with `[ok]`, `[warn]` or `[skip]`, and the last line is `result: ok` or `result: warn`. A `hint:` line under a check says what to do; skipped checks can have one too. This is a real run, against a test database that already holds stored provider credentials (a fresh install shows `no stored credentials`, and the role and PostgreSQL version are yours):

```text
gateway doctor (gateway 0.1.0)

[ok]   config.required             set: DATABASE_URL, GATEWAY_PROFILE, API_KEY_PEPPER
[ok]   config.load                 configuration loads (profile all-in-one)
[ok]   credentials.encryption_key  GATEWAY_CREDENTIAL_ENCRYPTION_KEY is set: provider credentials can be stored
[ok]   database.connect            connected as role gateway (PostgreSQL 17.11 (Debian 17.11-1.pgdg13+2))
[ok]   database.migrations         up to date: 124 migrations applied, latest 159
[ok]   database.grants             the role reads, writes and deletes in every gateway table and can run migrations; role `gateway_data_plane` exists for data planes
[ok]   database.clock_skew         within 2000 ms (0 ms)
[ok]   providers.configured        environment: openai; stored credentials: anthropic (8), azure-openai (16), bedrock (69), gemini (28), openai (40), openai-compatible (10)
[skip] providers.network           not run
                                   hint: pass --network to resolve and TLS-connect to each provider host (no model calls)
[skip] trace_store.reachable       no trace store configured (GATEWAY_TRACE_STORE_BUCKET unset): traces keep timing and usage, not content
[skip] clickhouse.reachable        ClickHouse not configured (GATEWAY_CLICKHOUSE_URL unset): analytics read PostgreSQL

result: ok (8 ok, 0 warn, 0 fail, 3 skip)
```

Add `--network` to also resolve each provider host configured in the environment and complete a TLS handshake with it. This sends no request and calls no model. Behind an HTTP proxy (`HTTPS_PROXY` or `ALL_PROXY`, minus `NO_PROXY`) the handshake goes through the proxy's `CONNECT` tunnel; a proxy it cannot tunnel through makes the check `warn` with "not probed: traffic goes through a proxy". Credentials stored in the database (added in the console) are not probed. Add `--json` for output a program can read, in this shape:

```jsonc
{
  "schema_version": 1,
  "gateway_version": "…",
  "status": "ok",          // ok | warn | fail: the worst check, skips ignored
  "checks": [
    { "id": "database.connect", "status": "ok", "summary": "…", "hint": null }
  ]
}
```

The exit code is `1` when any check is `fail`, and `0` otherwise.

**If it differs:** each `[fail]` or `[warn]` line has a `hint:` line under it. Act on the hint, then run the doctor again. The check IDs are stable and are explained in the [troubleshooting guide](../support/troubleshooting.md#start-with-gateway-doctor).

Now go to [step 5](#step-5-create-the-first-organization-and-an-agent-token).

## Path B: Helm on Kubernetes

The chart is published at `oci://ghcr.io/qucelerate/charts/ai-gateway`. It runs the gateway (API and worker), the admin console and a migration job, and it uses a PostgreSQL database that you provide. The chart's own README is the reference for its value names. This path shows the steps and what to check, not every value.

### Step 2: Prepare the database and the secrets

The gateway needs PostgreSQL 16 or later, with a database, a role that owns a `gateway` schema in it, and a URL for that role. Set these first. The two admin URLs are for a role that may create roles and databases: `POSTGRES_ADMIN_URL` names a database that exists, such as `/postgres`, and `GATEWAY_DB_ADMIN_URL` is the same URL with the path `/gateway`, the database created below:

```bash
export GATEWAY_HOST=gateway.example.com           # the public host name of the console and API
export GATEWAY_VERSION=X.Y.Z                      # the release to install
export DB_HOST=db.example.internal                # PostgreSQL, as the cluster reaches it
export POSTGRES_ADMIN_URL='postgres://postgres:…@db.example.internal:5432/postgres?sslmode=require'
export GATEWAY_DB_ADMIN_URL='postgres://postgres:…@db.example.internal:5432/gateway?sslmode=require'
APP_PASSWORD=$(openssl rand -hex 24)              # hex, so it needs no escaping in a URL
```

Then create the role, the database and the schema. The schema is created in the new database, so the second command connects to it:

```bash
psql "$POSTGRES_ADMIN_URL" -v app_password="$APP_PASSWORD" <<'SQL'
CREATE ROLE gateway_app LOGIN PASSWORD :'app_password';
CREATE DATABASE gateway OWNER gateway_app;
SQL
psql "$GATEWAY_DB_ADMIN_URL" -c 'CREATE SCHEMA gateway AUTHORIZATION gateway_app;'
```

`:'app_password'` is filled in by psql from `-v app_password=…`, quoted as a literal. A managed database service may instead have you create the role in its console. Then create the secret the chart reads by default, `ai-gateway-secrets`, with the gateway's secret settings. The names are the same as in `.env.example`:

```bash
kubectl create namespace ai-gateway
kubectl -n ai-gateway create secret generic ai-gateway-secrets \
  --from-literal=DATABASE_URL="postgres://gateway_app:${APP_PASSWORD}@${DB_HOST}:5432/gateway?sslmode=require" \
  --from-literal=API_KEY_PEPPER="$(openssl rand -hex 24)" \
  --from-literal=OIDC_SESSION_HMAC_SECRET="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')" \
  --from-literal=OIDC_SESSION_ENCRYPTION_KEY="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')" \
  --from-literal=GATEWAY_CREDENTIAL_ENCRYPTION_KEY="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')" \
  --from-literal=OPENAI_API_KEY="$OPENAI_API_KEY"
unset APP_PASSWORD
```

**Expected:** `CREATE ROLE`, `CREATE DATABASE` and `CREATE SCHEMA`, then `namespace/ai-gateway created` and `secret/ai-gateway-secrets created`.

**If it differs:** a percent sign or `@` in a password must be percent-encoded in a URL; the hex password above has neither. `sslmode=require` assumes the database accepts TLS: drop it for one that does not, inside a trusted network only.

### Step 3: Install the chart

The chart refuses to render without `host`. It also needs an ingress controller, and a TLS secret for the host named `ai-gateway-tls` (or `ingress.tlsSecretName`), as the chart's README describes:

```bash
helm show readme oci://ghcr.io/qucelerate/charts/ai-gateway --version "$GATEWAY_VERSION" | less
helm install ai-gateway oci://ghcr.io/qucelerate/charts/ai-gateway --version "$GATEWAY_VERSION" \
  --namespace ai-gateway --set host="$GATEWAY_HOST" --wait
kubectl -n ai-gateway get deploy,job
curl -fsS "https://${GATEWAY_HOST}/healthz"
```

Put further settings in a values file (`helm show values oci://ghcr.io/qucelerate/charts/ai-gateway > values.yaml`) and add `-f values.yaml`.

**Expected:** `helm` reports `STATUS: deployed`. The deployments `ai-gateway-api`, `ai-gateway-worker` and `ai-gateway-admin` are ready, and the job `ai-gateway-migrate` is complete. These names come from the chart's name, not the release name; with `nameOverride` set they start with that instead. The health check prints `{"status":"ok"}`.

**If it differs:** `kubectl -n ai-gateway logs job/ai-gateway-migrate` shows why a migration failed, and `kubectl -n ai-gateway describe pod <pod>` shows why a pod cannot start, for example an image pull error or a missing secret key. Browsers accept the console's `__Host-` cookies only over HTTPS, except on `localhost`, so the ingress needs TLS.

### Step 4: Run `gateway doctor`

```bash
kubectl -n ai-gateway exec deploy/ai-gateway-worker -- gateway doctor
```

**Expected** and **If it differs** are as in [Path A, step 4](#step-4-run-gateway-doctor), with the profile shown as `worker`. On this path, the later steps change like this:

- `docker compose --profile app exec gateway gateway …` (the server's CLI: `agent create`, `setup-link`, `invitation create`, `doctor`) runs as `kubectl -n ai-gateway exec deploy/ai-gateway-worker -- gateway …`.
- `docker compose --profile app logs gateway` is `kubectl -n ai-gateway logs deploy/ai-gateway-api`.
- `gwctl` runs on your machine against `https://${GATEWAY_HOST}`, as step 5 shows. The worker serves no HTTP, so `gwctl` cannot run there against `localhost`.
- Every `http://localhost:8080` is `https://${GATEWAY_HOST}`.

## Step 5: Create the first organization and an agent token

There are two ways to create the first organization. People use the setup link in a browser. Agents and scripts create the organization together with an agent token, which works the admin API without a browser.

**A person, in the browser.** While no organization exists, the gateway logs a single-use setup link that is valid for 24 hours:

```bash
docker compose --profile app logs gateway | grep 'first-run setup'
docker compose --profile app exec gateway gateway setup-link
```

The second command prints a new link if the first has expired. Open the link, then enter the organization's name and short name (slug), and the owner's email, name and password (12 or more characters).

**An agent, headless.** This creates the organization by its slug, if it does not exist yet, along with an owner agent and a token:

```bash
docker compose --profile app exec gateway gateway agent create \
  --organization acme --organization-name "Acme" \
  --name installer --role owner --expires-in-days 7
```

**Expected:** three lines. The third is the token, which is shown only this once:

```text
agent 3f6c…-… (created) in organization 9a1d…-…
token cli expires in 7 days
gwa_…
```

Keep the token and the organization ID in your shell, not in a file you commit:

```bash
export GATEWAY_AGENT_TOKEN='gwa_…'   # the third line
export GATEWAY_ORGANIZATION='9a1d…'  # the organization id from the first line
gwctl() {
  docker compose --profile app exec -T \
    -e GATEWAY_URL=http://localhost:3000 \
    -e GATEWAY_AGENT_TOKEN="$GATEWAY_AGENT_TOKEN" \
    -e GATEWAY_ORGANIZATION="$GATEWAY_ORGANIZATION" \
    gateway gwctl "$@"
}
gwctl orgs
```

`gwctl` is the admin and inference client that ships in the gateway image. The function runs it inside the gateway container, which serves the API on port 3000, and passes it the token and organization from your shell. **Expected:** JSON that lists one organization, whose slug is `acme`.

On the Helm path, run `gwctl` on your machine instead. The image is distroless and the worker serves no HTTP, so copy the binary out of the image (a Linux build, for the image's architecture; elsewhere build it from the source with `cargo build --release --bin gwctl`) and point it at the public host:

```bash
id=$(docker create "ghcr.io/qucelerate/ai-gateway:${GATEWAY_VERSION}")
docker cp "$id:/usr/local/bin/gwctl" ./gwctl
docker rm "$id"
gwctl() { GATEWAY_URL="https://${GATEWAY_HOST}" ./gwctl "$@"; }
gwctl orgs
```

`GATEWAY_VERSION` is the release from step 2, whose image the chart runs. `GATEWAY_AGENT_TOKEN` and `GATEWAY_ORGANIZATION` are read from the environment you exported them in.

**If it differs:** `the organization must be a slug` means the slug may hold only lowercase letters, digits and hyphens. A `401` from `gwctl` means the token was copied incompletely. Run `gateway agent create` again, which mints a new token for the same agent. If the organization was created through the setup link, give its slug to `gateway agent create` and the agent joins that organization. To bring a person in after a headless setup, run `gateway invitation create --organization acme --role owner` in the container and open the link it prints.

## Step 6: Add a provider and a catalog release

A provider adapter is configured either by the environment (`OPENAI_API_KEY` in step 2, adapter key `openai-main`) or by a credential that the organization stores. To store one instead, use *Connections* in the admin console (`http://localhost:8080`). Either way, requests reach a model only through the organization's active **catalog release**: a versioned manifest of deployments, prices and route aliases.

Write a release that routes the alias `chat-default` to an OpenAI model. Replace the model and its prices with ones your key can use. Prices are in money units, 10⁻⁶ of a cent per token, so $0.15 per million tokens is `15`:

```bash
cat > release.json <<'JSON'
{
  "release": {"version": "first-install-1"},
  "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
  "deployments": [{
    "name": "openai-chat", "model": "gpt-4o-mini", "location": null,
    "adapter_instance_key": "openai-main",
    "capabilities": [
      {"operation": "chat", "max_input_bytes": 131072, "max_output_tokens": 4096,
       "max_tokens_per_byte": 1, "tokenizer": "configured",
       "reservation_ttl_seconds": 300, "chat_input_count_mode": "unsupported"},
      {"operation": "chat_stream", "max_input_bytes": 131072, "max_output_tokens": 4096,
       "max_tokens_per_byte": 1, "tokenizer": "configured",
       "reservation_ttl_seconds": 300, "chat_input_count_mode": "unsupported"}
    ]
  }],
  "prices": [
    {"deployment": "openai-chat", "operation": "chat", "input_minor_per_token": 15,
     "output_minor_per_token": 60, "cached_input_minor_per_token": 8,
     "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
     "source": "operator", "source_version": "first-install-1", "confidence": "conservative"},
    {"deployment": "openai-chat", "operation": "chat_stream", "input_minor_per_token": 15,
     "output_minor_per_token": 60, "cached_input_minor_per_token": 8,
     "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
     "source": "operator", "source_version": "first-install-1", "confidence": "conservative"}
  ],
  "routes": [
    {"alias": "chat-default", "operation": "chat", "deployment": "openai-chat"},
    {"alias": "chat-default", "operation": "chat_stream", "deployment": "openai-chat"}
  ]
}
JSON
RELEASE_ID=$(gwctl api POST catalog/releases --data - < release.json | jq -r .id)
gwctl catalog activate "$RELEASE_ID" | jq '.release | {version, active}'
```

**Expected:** `{"version": "first-install-1", "active": true}`. The response also carries the organization's new `activation_version`.

**If it differs:** a `400` or `422` problem names the field it rejects. Check the manifest against the [catalog reference](../architecture/catalog.md). If the release already exists with that version, change `release.version`. The same release can also be loaded on the server with `gateway catalog apply --organization <organization id> --file <path>`, which creates and activates it in one step. That command takes the organization's UUID, not its slug, and a path inside the container.

## Step 7: Create a service account, a key and a budget

An organization serves no requests until a budget covers them. Budgets are a strong control, not a hard ceiling on the provider's bill (see [budgets and their limits](../operations/budgets.md)). Set a provider-side spending limit too.

```bash
SA_ID=$(gwctl service-accounts create --name first-app | jq -r .id)
export GATEWAY_API_KEY=$(gwctl keys create "$SA_ID" --scopes inference,embeddings | jq -r .secret)
gwctl api PUT budgets/organization --if-match '*' \
  --data '{"currency": "USD", "limit_minor": 1000000000, "period": "monthly"}' | jq '{limit_minor, period}'
echo "${GATEWAY_API_KEY:0:3}"
```

`limit_minor` uses the same units: `1000000000` is $10 a month.

**Expected:** `{"limit_minor": 1000000000, "period": "monthly"}`, then `gw_`. The key is shown only by this response.

**If it differs:** `403` means the agent's role cannot write. Use an `owner` or `admin` agent. If `jq -r .secret` prints `null`, the request was a replay of an earlier one. Create another key. Rate limits (requests and tokens per minute, concurrency) are optional and are set the same way. See [budgets and rate limits](../architecture/budgets-and-rate-limits.md).

## Step 8: Send the first request

```sh
export GATEWAY_BASE_URL="http://localhost:8080"
curl "$GATEWAY_BASE_URL/api/inference/v1/chat/completions" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 0d4f6b1a-6a2e-4c3b-9f7d-2b8e5c1a9e40" \
  -H "X-Request-Id: req_01J8INSTALLCHECK1" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"Reply with the single word: pong"}]}'
```

**Expected:** `200` with a chat completion whose `choices[0].message.content` holds the model's reply. Its cost then shows in the admin console under *Usage*.

**If it differs:** the problem document's `code` says why. The common ones:

| Status and code | Meaning | Fix |
| --- | --- | --- |
| `401 authentication_required` | The request has no API key | Export `GATEWAY_API_KEY` from step 7 |
| `401 invalid_api_key` | The key is mistyped, revoked or expired | Copy it again, or create a new key (step 7) |
| `404 model_unavailable` | No active route has that alias | Check the release is active (step 6) and the alias is `chat-default` |
| `429 quota_exceeded` | No budget covers the request, or it is spent | Set the budget (step 7). The `detail` names the budget |
| `502` or `503` with a provider code | The provider refused the call or cannot be reached | Check the provider key. Run `gateway doctor --network` |

Every code is listed in the [error-code reference](../support/errors.md). Send each new request with a new `Idempotency-Key`. The gateway does not store answers: reusing a key whose request was accepted returns `409 idempotency_in_progress` or `409 idempotency_terminal`, never the first answer. A request refused before it was accepted (its error says nothing was reserved) can be retried with the same key.

## Final verification checklist

Each item must hold. The command after it shows whether it does:

- [ ] **Health:** `curl -fsS http://localhost:8080/healthz` prints `{"status":"ok"}`.
- [ ] **Doctor:** `docker compose --profile app exec gateway gateway doctor --json | jq -r .status` prints `ok` or `warn`, and the exit code is `0`.
- [ ] **Migrations:** the doctor's `database.migrations` check is `ok` (`… | jq -r '.checks[] | select(.id=="database.migrations") | .status'`).
- [ ] **Provider:** `providers.configured` is `ok`. With a provider set in the environment (`OPENAI_API_KEY` here), `gateway doctor --network` reports `providers.network` as `ok`; credentials added in the console are not probed, and the check is `skip` when there are no others.
- [ ] **Catalog:** `gwctl catalog routes | jq -r '.items[].alias'` lists `chat-default`.
- [ ] **Budget:** `gwctl api GET budgets | jq '.items | length'` is at least `1`.
- [ ] **Request:** step 8 returns `200`.
- [ ] **Secrets:** `.env` is not tracked (`git status --short .env` prints nothing), and the agent token is set to expire (step 5).

When every item holds, the installation is complete. Next steps:

- Sign in to the admin console at `http://localhost:8080`.
- Read the [architecture overview](../architecture/index.md).
- Plan backups with [backup and recovery](../operations/recovery.md).
- Point applications at the gateway with the [client libraries](../clients/index.md).

If an item fails and the [troubleshooting guide](../support/troubleshooting.md) does not resolve it, see [getting help](../support/index.md).
