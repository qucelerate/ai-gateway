# Backup and recovery

The gateway keeps its state in the `gateway` schema of one PostgreSQL database. The one exception is captured trace content: an organization that traces with content has each request and response written, encrypted, to an S3 or S3-compatible bucket (the trace store, set with `GATEWAY_TRACE_STORE_*`). Trace rows stay in the database; only their content is in the bucket.

## What to back up

Back up the database with your platform's tooling, for example managed snapshots with point-in-time recovery. A database backup is only useful with the secrets that read it, so keep these apart from the backups and restore them together:

- `API_KEY_PEPPER`. API keys are stored as digests keyed with it; with another pepper, no existing key authenticates.
- `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`, or access to the AWS KMS key. Stored provider credentials and webhook secrets are sealed with it.

If you use a trace store, back up or version its bucket as well, on a schedule that matches the database's. Trace content can be read only with both the bucket's object and the credential key or KMS key that sealed it, and a database restored without the bucket keeps its trace rows without their content. Restore the bucket's configuration and access too: the API and worker need `s3:PutObject`, `s3:GetObject`, and `s3:DeleteObject` on its prefix.

The session secrets only sign browser sessions. Losing them signs everyone out of the admin UI and loses nothing else.

## Restore

1. Create an empty database with a `gateway` schema owned by the application role.
2. Restore the backup into it.
3. Run `gateway migrate` as the application role. A restore carries no privileges, and migrating grants the data-plane role its access again; without data planes it changes nothing.
4. Start the gateway with the original `API_KEY_PEPPER`, credential key or KMS settings, and trace store settings.
5. Check `GET /readyz`, sign in to the admin UI, compare a recent day's usage report with what you expect, and send one inference request with an existing key.

Running processes notice a restore by themselves: each one finds that the configuration it applied is missing or different and reloads everything, so none keeps serving configuration the database no longer has.

Rehearse a restore at least once a quarter and after every major upgrade, and record how long it takes. The repository's `scripts/restore-drill.sh` dumps a database, restores it into an empty one, and checks row counts, sequences, and migration history.

## When part of the system fails

| Failure | What callers see | Recovery |
| --- | --- | --- |
| An instance or zone fails | Requests on it fail; open streams end without a terminal event | Automatic once health checks stop routing to it. Its unsettled reservations expire, and the worker settles them. |
| A region cannot reach the database primary | `503` from that region | `/readyz` fails, so load balancers drain it. Restore connectivity, then resolve any open reconciliation items. |
| The database primary fails | `503` everywhere | Promote a standby, then restart or redeploy the `api` and `worker` workloads against it. Data planes reconnect by themselves. |
| The `api` workload fails | The admin UI is unavailable; data planes keep serving inference | Restart it. |
| The `worker` fails | Inference continues; accounting recovery, event delivery, usage export, discovery, and price sync pause | Restart it. It catches up from durable state. |
| A provider deployment fails | `503` `provider_circuit_open` with `Retry-After` | See [rollback and incidents](/operations/incidents.md). |

An `Idempotency-Key` makes sure a request never runs or is charged twice; it does not store answers. A retry with the same key gets `409` with code `idempotency_in_progress` while the first attempt is still running, and `409` with code `idempotency_terminal` once it has finished, whether it succeeded or failed. Only a request refused before anything was reserved, such as one refused by a rate limit or an open circuit, can be sent again with its key. Retrying cannot recover a lost answer or resume a broken stream; if the request is safe to run again, send it with a new key. See [errors](/quickstarts/errors.md).

> **Scoped:** the gateway does not replicate or fail over its database. Multi-region writes are a non-goal; your PostgreSQL platform provides standbys and promotion.
