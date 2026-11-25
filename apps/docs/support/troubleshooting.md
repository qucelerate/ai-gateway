---
description: "Symptom, diagnosis, fix and verification for common problems, one per section, starting with gateway doctor."
---

# Troubleshooting guide

Each section below covers one problem, under a heading that describes what you see. Every section has the same four parts: **Symptom**, **Diagnose** (commands to run), **Fix**, and **Verify**. For a specific problem `code`, look it up in the [error-code reference](./errors.md). For a quick table of symptoms by HTTP status, see the [troubleshooting overview](../troubleshooting/index.md).

## Start with gateway doctor

`gateway doctor` checks configuration, database, credentials, stores and providers, and changes nothing. Run it in the same environment as the gateway, with the same variables, so it sees what the gateway sees:

```bash
# A binary on the host, with the gateway's environment loaded
gateway doctor

# Docker Compose (the service is named gateway; the binary is on its PATH)
docker compose --profile app exec gateway gateway doctor

# Kubernetes
kubectl exec deploy/<gateway-deployment> -- gateway doctor

# Also resolve and TLS-connect to each configured provider host (no model calls)
gateway doctor --network

# Machine-readable output, for scripts and bug reports
gateway doctor --json
```

It exits with `1` if any check fails, and with `0` otherwise, even when some checks warn. Each check reports `ok`, `warn`, `fail` or `skip`, a summary, and a hint for anything not `ok`. The JSON output looks like this (abridged):

```jsonc
{
  "schema_version": 1,
  "gateway_version": "0.0.0-example",
  "status": "fail",
  "checks": [
    { "id": "config.load", "status": "ok", "summary": "configuration loads (profile all-in-one)", "hint": null },
    { "id": "database.migrations", "status": "fail", "summary": "3 migration(s) pending: …", "hint": "run gateway migrate" },
    { "id": "database.clock_skew", "status": "warn", "summary": "clock is 3 s ahead", "hint": "sync NTP" }
    // …
  ]
}
```

| Check id | What it checks |
| --- | --- |
| `config.load` | The environment parses: every variable well formed. |
| `config.required` | Variables the profile needs are set (`DATABASE_URL`, `GATEWAY_PROFILE`, `API_KEY_PEPPER` for profiles that serve requests). |
| `credentials.encryption_key` | `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` or a KMS key is set, so secrets can be stored. |
| `database.connect` | `DATABASE_URL` connects. |
| `database.migrations` | Every migration in this image is applied. |
| `database.grants` | The connecting role has the privileges the gateway needs. |
| `database.clock_skew` | This host's clock agrees with the database server's. |
| `providers.configured` | Which provider accounts are configured on this process. |
| `providers.network` | With `--network` only: DNS and a TLS connection to each configured provider host. |
| `trace_store.reachable` | The trace-content bucket, when one is configured. |
| `clickhouse.reachable` | The ClickHouse analytics sink, when one is configured. |

The output contains no secret values. Still, review it before you share it.

## The gateway exits at startup with "missing required environment variable"

**Symptom**

The process exits as soon as it starts and prints something like:

```text
Error: Config(Missing { name: "DATABASE_URL" })
```

The message names the variable: `missing required environment variable DATABASE_URL` (or `GATEWAY_PROFILE`, `API_KEY_PEPPER`, …).

**Diagnose**

```bash
gateway doctor
# Look at config.load and config.required. In Compose:
docker compose --profile app exec gateway gateway doctor
docker compose --profile app config --format json | jq -r '.services.gateway.environment | keys[]'   # names only, never values
```

`gateway doctor` reports a configuration that does not load as a failed `config.load` check rather than exiting. If the container exits too fast to `exec` into it, run the check in a one-off container: `docker compose --profile app run --rm --no-deps gateway doctor`.

**Fix**

Set the variable where the process reads it: `.env` for Compose (copied from `.env.example`), the Kubernetes Secret or ConfigMap, or the service unit. `API_KEY_PEPPER` is needed by every profile that serves requests (`all-in-one`, `api`, `data-plane`); `gateway migrate` and the `worker` profile do not read it. Generate one with:

```bash
openssl rand -base64 32 | tr '+/' '-_' | tr -d '='
```

Keep the pepper stable: changing it makes every existing API key invalid.

**Verify**

`gateway doctor` shows `config.load` and `config.required` as `ok`, and the process stays up.

## The gateway exits with "API_KEY_PEPPER is invalid" or "API_KEY_PEPPER is required"

**Symptom**

```text
Error: Config(InvalidPepper(MissingPepper))
```

or `API_KEY_PEPPER is invalid: API_KEY_PEPPER is required and must not be empty`, or `Error: Application(PepperRequired)` (`API_KEY_PEPPER is required for API application state`).

**Diagnose**

```bash
gateway doctor   # config.load and config.required name the variable
# The image has no shell; check the source of the variable instead, without printing it:
grep -c '^API_KEY_PEPPER=.' .env
```

**Fix**

Set `API_KEY_PEPPER` to a random value of 32 bytes or more (see the command in the previous section). Use the same value on every `api`, `all-in-one` and `data-plane` process of one deployment. Restoring an earlier value brings back the keys issued under it.

**Verify**

`gateway doctor` shows `config.required` as `ok`, and an inference request with a known key no longer returns `401` [`invalid_api_key`](./errors.md#invalid_api_key).

## The gateway exits with "unsupported GATEWAY_PROFILE"

**Symptom**

```text
Error: Config(InvalidProfile { value: "allinone" })
```

The message reads `unsupported GATEWAY_PROFILE "allinone"; expected all-in-one, api, worker, or data-plane`. A similar `unsupported command or argument` error means the arguments after `gateway` are wrong.

**Diagnose**

```bash
gateway doctor   # config.load repeats the message
```

**Fix**

Set `GATEWAY_PROFILE` to exactly one of `all-in-one`, `api`, `worker` or `data-plane`; see [profiles](../operations/index.md#profiles). For an argument error, compare the command with the usage the message prints.

**Verify**

The process starts and `gateway doctor` shows `config.load` as `ok`.

## The gateway cannot connect to its database

**Symptom**

At startup: `Error: Config(InvalidDatabaseUrl)` (`DATABASE_URL must be a PostgreSQL URL`) or `Error: Application(DatabaseConnection)` (`database connection failed: unable to connect using DATABASE_URL`). A gateway that is already running answers `503` with [`database_unavailable`](./errors.md#database_unavailable), [`inference_unavailable`](./errors.md#inference_unavailable) or [`authentication_unavailable`](./errors.md#authentication_unavailable), and `/readyz` fails.

**Diagnose**

```bash
gateway doctor   # database.connect, then database.grants
# From the same host or container, with the same URL:
psql "$DATABASE_URL" -c 'select 1'
```

Check the URL's scheme (`postgres://` or `postgresql://`), host (inside Compose it is `postgres`, not `localhost`), port, database name, and user. Special characters in the password must be percent-encoded.

**Fix**

Correct `DATABASE_URL`. In Compose, `DATABASE_URL` is built from `GATEWAY_APP_USER`, `GATEWAY_APP_PASSWORD` and `POSTGRES_DB`; if the password is not URL-safe, set `GATEWAY_COMPOSE_DATABASE_URL` to the full URL with the password percent-encoded. The gateway connects as the least-privilege application role, not the database superuser; if `database.grants` fails, run the role setup your deployment uses again.

**Verify**

`gateway doctor` shows `database.connect` and `database.grants` as `ok`, and `curl -fsS "$GATEWAY_BASE_URL/readyz"` succeeds.

## Requests fail with 503 after an upgrade: migrations are pending

**Symptom**

After you deploy a new image, admin and inference requests answer `503` (`database_unavailable` or `inference_unavailable`), or the logs show queries failing on missing tables or columns. The gateway never migrates on its own when it starts.

**Diagnose**

```bash
gateway doctor   # database.migrations names the migrations this image has that the database lacks
```

**Fix**

Run the migrations with the new image, as the application role, once per release:

```bash
gateway migrate
# Docker Compose runs it as the one-off migrate service:
docker compose --profile app run --rm migrate
```

`gateway migrate` takes an advisory lock, so a second run waits rather than races. Then restart or roll the gateway workloads. See [migrations](../operations/index.md#migrations). Migrations are forward-only. If you rolled the image back over a newer schema, check the release notes before you run the older image.

**Verify**

`gateway doctor` shows `database.migrations` as `ok`, and requests succeed.

## The setup link has expired or was lost

**Symptom**

Opening the first-run setup link shows that it is not valid or has expired ([`setup_link_invalid`](./errors.md#setup_link_invalid)), or the link has scrolled out of the logs.

**Diagnose**

Setup links last 24 hours and exist only while the gateway has no organization. If setup has already been done, you get [`setup_already_done`](./errors.md#setup_already_done) instead; in that case, see the next section.

```bash
docker compose --profile app logs gateway | grep -i 'setup'
```

**Fix**

Print a new link with the gateway's own environment:

```bash
gateway setup-link
docker compose --profile app exec gateway gateway setup-link
```

It needs `GATEWAY_ORIGIN` to be the origin your browser uses (for Compose, `http://localhost:8080` by default), because the link is built from it.

**Verify**

The new link opens the setup form and creates the first organization and its owner.

## Locked out of the admin console

**Symptom**

You cannot sign in. The password is rejected ([`invalid_credentials`](./errors.md#invalid_credentials)), the organization requires its identity provider and that provider is broken ([`sso_required`](./errors.md#sso_required)), or nobody left can administer the deployment ([`deployment_admin_required`](./errors.md#deployment_admin_required)).

**Diagnose**

```bash
gateway doctor   # confirms database access; these commands need it
```

Decide which case applies. A forgotten password for a local account needs a reset link. A broken identity provider needs password sign-in turned back on. If you can sign in but lack the right, you need a deployment-administrator grant.

**Fix**

Run the command for your case on the server, with the gateway's environment. In Compose, put `docker compose --profile app exec gateway` before it.

```bash
# A local account's password: prints a single-use link, valid for 24 hours
gateway user reset-password --email owner@example.com

# The organization's identity provider is broken: allow passwords again
gateway sign-in-policy allow-passwords --organization <organization-slug>

# Make an existing account a deployment administrator
gateway admin grant --email owner@example.com
gateway admin grant --issuer https://issuer.example.com --subject <oidc-subject>
```

`gateway admin grant` needs an account that exists already. `reset-password` works only for local (password) accounts; an identity-provider account changes its password at the provider.

**Verify**

You can sign in, and the admin console shows the organization and the pages you expect.

## The admin console refuses changes with 403 cross-origin or CSRF errors

**Symptom**

Pages load, but saving answers `403` with [`cross_origin_request_refused`](./errors.md#cross_origin_request_refused) or [`csrf_origin_or_token_required`](./errors.md#csrf_origin_or_token_required), or sign-in loops back to the start.

**Diagnose**

Compare the address in the browser bar, including scheme, host and port, with `GATEWAY_ORIGIN` and `OIDC_REDIRECT_URI`:

```bash
gateway doctor   # config.load shows the configuration it parsed
```

**Fix**

Serve the admin UI and `/api/` on one origin, set `GATEWAY_ORIGIN` to exactly that origin, and make the reverse proxy pass `Host` and `Origin` through unchanged. For scripts, use an agent token (`gateway agent create --organization <slug> --name <name>`) with `Authorization: Bearer`, not a copied session cookie.

**Verify**

Saving a setting in the admin console succeeds.

## Every inference request returns 401

**Symptom**

`401` with [`authentication_required`](./errors.md#authentication_required) (no key arrived) or [`invalid_api_key`](./errors.md#invalid_api_key) (the key is unknown, revoked, disabled or expired).

**Diagnose**

```sh
curl -i "$GATEWAY_BASE_URL/api/inference/v1/chat/completions" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 5b0e7c1a-9d2f-4e83-b6a4-1c8f0d3e2a57" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"ping"}]}'
```

Check that `$GATEWAY_API_KEY` is set in this shell (`test -n "$GATEWAY_API_KEY" && echo set`), and that the key is shown as active in the admin console. If the request goes through a proxy or API manager, check that it forwards `Authorization`.

**Fix**

Send `Authorization: Bearer` with the full key. Issue a new key if the old one is revoked or expired. If every key stopped working at once, `API_KEY_PEPPER` changed: restore its previous value.

**Verify**

The same request answers `200`.

## Requests return 403

**Symptom**

`403` with [`insufficient_scope`](./errors.md#insufficient_scope), [`routing_policy_denied`](./errors.md#routing_policy_denied) or [`end_user_not_registered`](./errors.md#end_user_not_registered).

**Diagnose**

Read `code` and `detail` in the problem body. `insufficient_scope` means the key was issued without the scope for this endpoint (for example, embeddings or batches). `routing_policy_denied` means the organization's routing policy excludes every deployment behind the alias. `end_user_not_registered` means the app's end user has no plan.

```bash
curl -s "$GATEWAY_BASE_URL/api/admin/v1/organizations/$ORG_ID/catalog/routing-policy" \
  -H "Authorization: Bearer $GATEWAY_AGENT_TOKEN"
```

**Fix**

Issue a key with the needed scope. Widen the routing policy, or route the alias to a permitted provider and location. Assign the end user a plan, or set a default plan for the service account.

**Verify**

The request answers `200`.

## Requests return 404 model_unavailable

**Symptom**

`404` with [`model_unavailable`](./errors.md#model_unavailable) for an alias you expect to work.

**Diagnose**

List the routes of the organization's active catalog release for that alias:

```bash
curl -s "$GATEWAY_BASE_URL/api/admin/v1/organizations/$ORG_ID/catalog/routes?alias=chat-default" \
  -H "Authorization: Bearer $GATEWAY_AGENT_TOKEN"
```

No routes means the alias does not exist in the active release. Aliases are case-sensitive. A route for `chat` but not `embeddings` (or the reverse) means the alias does not serve the endpoint you called. No active release at all gives [`no_active_release`](./errors.md#no_active_release) on catalog reads.

**Fix**

Use an alias that the active release routes for this operation. Otherwise, add the route to the catalog and activate the new release (`gateway catalog apply --organization <slug> --file <manifest>`, or the admin API), and offer the alias to the key's service account.

**Verify**

The routes call lists the alias for the operation, and the inference request answers `200`.

## Requests return 429: budget or rate limit?

**Symptom**

`429` from an inference endpoint.

**Diagnose**

The `code` in the problem body tells you which limit refused the request:

```sh
curl -s -i "$GATEWAY_BASE_URL/api/inference/v1/chat/completions" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 0d7c4f2b-8a61-4e9d-a3b5-6f1e2c9d8b70" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"ping"}]}'
```

| `code` | What refused it | Extra fields |
| --- | --- | --- |
| [`rate_limit_exceeded`](./errors.md#rate_limit_exceeded) | A requests-per-minute or tokens-per-minute limit | `scope` (`organization`, `team`, `service-account`, `api-key`) or `end_user`; `limit` |
| [`concurrency_limit_exceeded`](./errors.md#concurrency_limit_exceeded) | A requests-in-flight limit | `scope` or `end_user`; `limit` is `max_concurrent` |
| [`quota_exceeded`](./errors.md#quota_exceeded) | A budget, or an end user's allowance and credits | `end_user`, `allowance_remaining`, `credit_balance`, `currency` for an end user |
| [`provider_rate_limited`](./errors.md#provider_rate_limited) | The provider's own quota, not the gateway | none |

To see the policies and budgets at each level:

```bash
curl -s "$GATEWAY_BASE_URL/api/admin/v1/organizations/$ORG_ID/rate-limits" -H "Authorization: Bearer $GATEWAY_AGENT_TOKEN"
curl -s "$GATEWAY_BASE_URL/api/admin/v1/organizations/$ORG_ID/budgets" -H "Authorization: Bearer $GATEWAY_AGENT_TOKEN"
```

**Fix**

- Rate or concurrency limit: wait `Retry-After` seconds and retry with the same `Idempotency-Key`. If it keeps happening, spread load or raise the limit at the `scope` named in the body. If `detail` says the request can never fit a tokens-per-minute limit, shorten the request or lower `max_tokens`.
- Budget: retrying does not help. An administrator adds or raises a budget at the level that ran out. A new organization has no budget until one is added; `detail` then says that no budget covers the request. See [budgets](../operations/budgets.md).
- Provider rate limit: raise the quota at the provider, or route the alias to more deployments.

**Verify**

The request answers `200`, and the budget or rate-limit listing shows the new values.

## Requests fail with provider errors or provider_circuit_open

**Symptom**

`502` [`provider_failure`](./errors.md#provider_failure), `503` [`provider_unavailable`](./errors.md#provider_unavailable), `504` [`provider_timeout`](./errors.md#provider_timeout), or `503` [`provider_circuit_open`](./errors.md#provider_circuit_open) with `Retry-After`.

**Diagnose**

```bash
gateway doctor --network   # providers.configured, then providers.network: DNS and TLS to each provider host
```

Search the gateway logs for the response's `request_id`, and for the circuit warning, which names the organization, adapter instance and deployment. Check the provider's own status page and quota.

**Fix**

- `providers.network` fails: fix DNS, egress firewall rules, or the outbound proxy for the provider hosts.
- The provider rejects the credential: rotate it in the catalog.
- The circuit is open: it closes once a probe request succeeds after `Retry-After`. To keep serving in the meantime, route the alias to another deployment. See [rollback and incidents](../operations/incidents.md).
- `provider_timeout` and `provider_failure` may already have been billed, so do not resend them automatically.

**Verify**

`gateway doctor --network` shows `providers.network` as `ok`, and requests through the alias succeed.

## Streams stop early or arrive all at once behind a proxy

**Symptom**

`/chat/completions/stream` returns all events at the end instead of one by one, or the stream ends without a terminal `response.completed` or `error` event, often after a fixed time such as 60 seconds.

**Diagnose**

Compare a request sent straight to the gateway with one sent through the proxy:

```sh
curl -N "$GATEWAY_BASE_URL/api/inference/v1/chat/completions/stream" \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 3a9e5d71-2c4b-4f08-8e6d-b1f7a0c5d294" \
  -d '{"model":"chat-default","messages":[{"role":"user","content":"Count to twenty."}]}'
```

If events come one by one from the gateway but not through the proxy, the proxy buffers the response or times out idle reads.

**Fix**

For `/api/` on the proxy: turn response buffering off (nginx: `proxy_buffering off;`), use HTTP/1.1 to the upstream, set read and send timeouts longer than your longest stream (the bundled Compose proxy uses 15 minutes), and do not compress `text/event-stream`. Give the gateway a termination grace period at least as long as your longest stream, so that rollouts do not cut streams. Streams cannot be resumed: a client whose stream breaks starts a new request with a new `Idempotency-Key`, if running it twice is safe.

**Verify**

Through the proxy, `curl -N` prints events one by one and ends with `response.completed`.

## Traces show timing but no request content

**Symptom**

The organization has turned tracing on, but traces in the admin console show timing and metadata only. Reading content answers `503` [`trace_content_unavailable`](./errors.md#trace_content_unavailable), or there is no content at all.

**Diagnose**

```bash
gateway doctor   # trace_store.reachable and credentials.encryption_key
curl -s "$GATEWAY_BASE_URL/api/admin/v1/organizations/$ORG_ID/trace-policy" -H "Authorization: Bearer $GATEWAY_AGENT_TOKEN"
```

`trace_store.reachable` shows `skip` when no trace store is configured. Without one, the gateway keeps timing only.

**Fix**

Configure a trace store on both the API and the worker processes: `GATEWAY_TRACE_STORE_BUCKET` and region, plus `GATEWAY_TRACE_STORE_ENDPOINT` for an S3-compatible store, and credentials. Content is encrypted under the credential keyring, so `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` (or a KMS key) must be set too. Make sure the trace policy records content. Content is stored only for requests made after the change.

**Verify**

`gateway doctor` shows `trace_store.reachable` as `ok`, and a new traced request shows its content.

## Saving a provider credential returns 503 credential_encryption_unavailable

**Symptom**

Saving a provider credential, an identity-provider client secret, or a webhook secret answers `503` [`credential_encryption_unavailable`](./errors.md#credential_encryption_unavailable).

**Diagnose**

```bash
gateway doctor   # credentials.encryption_key
```

**Fix**

Generate a key and set it as `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` on every process, then restart them:

```bash
openssl rand -base64 32 | tr '+/' '-_' | tr -d '='
```

It must be 32 bytes in base64url without padding. Otherwise the gateway refuses to start with `GATEWAY_CREDENTIAL_ENCRYPTION_KEY must be 32 bytes (base64url without padding)`. Keep it apart from the session secrets, and back it up: if you lose it, stored credentials can no longer be read. Instead of a local key, you can set `GATEWAY_CREDENTIAL_KMS_KEY_ID` to use AWS KMS.

**Verify**

`gateway doctor` shows `credentials.encryption_key` as `ok`, and saving the credential succeeds.

## Sign-in or provider calls fail because the clock is wrong

**Symptom**

OIDC sign-in fails with [`claim_validation_failed`](./errors.md#claim_validation_failed) for everyone. Cloud provider calls fail with signature or token-expiry errors. Reservations or links expire earlier or later than expected.

**Diagnose**

```bash
gateway doctor   # database.clock_skew compares this host's clock with the database server's
timedatectl status   # on a systemd host: "System clock synchronized: yes"
```

**Fix**

Run NTP (chrony, systemd-timesyncd, or your platform's time service) on the gateway hosts and the database host. In containers, fix the clock on the host; containers use the host's clock.

**Verify**

`gateway doctor` shows `database.clock_skew` as `ok`, and sign-in works.

## Still stuck

Collect `gateway doctor --json`, the `request_id` of a failing request, and the steps to reproduce, then see [getting help](./index.md).
