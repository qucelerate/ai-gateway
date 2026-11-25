# Deployment & operations

Deploy the gateway behind your normal HTTPS boundary and point clients at the resulting host. Keep credentials in a secret manager and limit their exposure to the calling service.

## Operational basics

- Enforce TLS at the public edge.
- Set request and connection timeouts appropriate to your workload.
- Bound prompt, input, and output sizes before sending traffic.
- Capture status, latency, and opaque request IDs without recording bearer keys or sensitive prompts.
- Alert on sustained `429`, `502`, `503`, and `504` responses. A `503` with `provider_circuit_open` means a deployment's circuit is open; the gateway logs a warning with the organization, adapter instance, and deployment when a circuit opens and an info line when it closes.

## Profiles

One immutable image runs in one of four profiles, chosen with `GATEWAY_PROFILE`. PostgreSQL is the only other stateful service, apart from an optional trace-content bucket (see [backup and recovery](/operations/recovery.md)).

| Profile | Runs | Use it for |
| --- | --- | --- |
| `all-in-one` | Inference, the admin API and sign-in, and the background worker | Development and non-HA deployments, such as the repository's Docker Compose `app` profile |
| `api` | Inference, the admin API and sign-in | Production, scaled independently of the worker |
| `worker` | Background work only: accounting recovery, event and webhook delivery, usage export, model discovery, price sync, retention | Production; run at least one |
| `data-plane` | Inference, `/healthz` and `/readyz` only, connected as a restricted database role | Serving inference in further regions once configuration versions are in use |

The API profiles are stateless. Every reservation and settlement is a transaction on the PostgreSQL primary, so a process that cannot reach it refuses inference with `503` rather than serve unmetered requests.

## Health checks

- `GET /healthz` answers when the process is running. Use it for liveness.
- `GET /readyz` answers `503` until the process has loaded its configuration, and again whenever it has not read the configuration feed for three polls, which usually means the database primary is unreachable. Point load-balancer and readiness checks at it.

On `SIGTERM` the gateway stops accepting connections and lets open requests and streams finish. Give it a termination grace period at least as long as your longest stream.

## Migrations

Migrations are forward-only and embedded in the image. The gateway never migrates when it starts; run `gateway migrate` once per release, as the application role, before you roll the new image out:

1. Run `gateway migrate` with the new image. It takes an advisory lock, so a second run waits rather than races.
2. Roll the `api`, `worker`, and `data-plane` workloads to the new image.

There are no down migrations. Read the release's change description before you upgrade: rolling the image back over a migrated schema is safe only when the release says its migrations are compatible with the previous image. Otherwise, the way back from a schema change is [a restore](/operations/recovery.md).

> **Scoped:** this page does not prescribe a cloud, container platform, autoscaling policy, or managed-provider topology.

See [backup and recovery](/operations/recovery.md), [rollback and incidents](/operations/incidents.md), [reliability signals](/operations/signals.md), [budgets and their limits](/operations/budgets.md), and [security & privacy](/security/index.md).
