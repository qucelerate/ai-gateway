---
description: "Request records, trace content capture and its encryption, retention policies, exports, and soft deletion."
---

# Data and retention

Every inference request leaves a request record of metadata: who made it, where it was routed, what it used and what it cost. Accounting, reports, reconciliation and exports depend on that record, so it is always kept and never holds content. Request and response content is kept only in traces, which an organization turns on, and is encrypted under a key per trace in an object store. Each organization sets how long traces, usage and audit events are kept; by default nothing expires. Deleting an organization hides it and keeps its data and secrets so that it can be restored, which is why the console tells you to revoke its credentials at each provider if it will not be.

## What the gateway keeps

| Data | Holds | Where | Kept |
| --- | --- | --- | --- |
| Request record | Organization, team, service account, API key, end user, alias, deployment, provider family, location, operation, status, outcome, error category, provider request id, token counts and their confidence, amount and currency, reservation, catalog provenance, timings. Never content. | PostgreSQL | Until the organization's usage or trace retention removes it |
| Usage and settlement | Immutable usage rows, allocations per budget, the price snapshot, the settlement record, reconciliation items | PostgreSQL | With its request record |
| Trace content | What was sent and what came back | Object store, encrypted | The trace policy's content retention (30 days by default) |
| Audit events | Admin access and every mutation, without secrets or content | PostgreSQL, append-only | The organization's audit retention |
| Outbox events | Fixed-shape events of identifiers and amounts | PostgreSQL | Until dispatched |
| Analytics copy | One row per request, never content | ClickHouse, if configured | Follows PostgreSQL's deletes |

Logs and metrics never carry prompts, completions, credentials or authorization headers. Metrics labels come from fixed sets only, never tenant identifiers. A failed provider request is logged with its category, HTTP status, error type and request id, never the body.

## Traces and content capture

Tracing is off for every organization until an owner or admin turns it on, and every change is audited. The trace policy has:

- **`mode`**: `off`, `organization` (every request), or `selected` (the requests of chosen teams and service accounts).
- **`content`**: whether a traced request keeps what was sent and what came back. On by default once tracing is on; off keeps timing only.
- **`content_retention_days`**: 1 to 3,650 days, 30 by default. Content never outlives its request record.

For a traced chat request (streamed or not) the gateway records when it took the request, called the provider, saw the first streamed event and finished, and, with content on, one document holding the request as the client sent it (messages, tools, parameters, `max_tokens` before the gateway's cap) and the response (content, refusal, tool calls, finish reason; a stream as assembled). For embeddings it keeps the input texts and, for each vector, its index, dimensions and first 8 values, never whole vectors. When a request fails over, only the last attempt keeps content. Credentials and headers are never part of it.

### How content is encrypted

```text
  request/response document
        │  sealed with AES-256-GCM under a fresh per-trace key
        ▼
  ciphertext ─────────────► object store: {prefix}{organization}/{trace}
  per-trace key
        │  sealed by the credential keyring (environment key or KMS data key),
        ▼  bound to the organization and the trace
  sealed key ─────────────► PostgreSQL, beside the request record
```

Neither the object store nor the database alone yields content. The store is an S3 bucket or any S3-compatible store with path-style addressing (`GATEWAY_TRACE_STORE_*`); its credentials need to put, get and delete objects under the prefix.

Content is written after the response, in the background, so tracing never delays a request. Each side is capped at `GATEWAY_TRACE_CONTENT_MAX_BYTES` (5 MiB by default): text past the cap is cut and marked truncated, keeping the structure. Each trace records what became of its content: `writing`, `stored`, `off`, `unavailable` (no store or keyring), `failed` or `expired`. A process runs at most 64 writes at once; a request that finishes while all are busy is not traced, so a slow store never queues content in memory.

### Reading traces

Reading request records (`trace_read`) and reading trace content (`trace_content_read`, owners and admins) are separate permissions. Every content read is audited, including refused reads and content that could not be opened. Traces are listed and searched in the console under *Traces*, through the admin API, `gwctl` and MCP.

## Retention

Owners and admins set retention per organization:

| Data | Bounds |
| --- | --- |
| Traces | 7 to 3,650 days, and no longer than usage when usage is set |
| Usage | 30 to 3,650 days |
| Audit events | 90 to 3,650 days |

A period that is not set keeps data indefinitely, which is the default. Every hour the worker deletes what is older than the organization's periods, in bounded batches per kind and organization, so a large backlog drains over later runs. Each purge batch is audited by the system in the same transaction (`retention.traces_purged`, `retention.usage_purged`, `retention.audit_events_purged`).

- Removing usage removes a request whole: its reservation with its usage rows, allocations, price snapshot, settlement record, resolved reconciliation item and trace. A reservation that is not final, or whose reconciliation item is still open, is kept until it is.
- Budget balances live in their budget buckets, so purging old usage never changes what a budget has consumed; it does remove those requests from usage reports.
- Before the purge, the worker deletes trace content older than its content retention, or whose request record is due to go, and marks it `expired`.
- Usage, trace and audit records are append-only to the application: only the purge deletes them.

Deletion by retention cannot be undone. Export first if the data must be kept elsewhere.

## Exports

- **On demand.** Usage, traces and audit events export as NDJSON or CSV, up to 366 days per request, filterable by batch or master model record. Each export is recorded and audited before its first row, and again if it fails or the client leaves early.
- **Continuously to object storage.** An organization can stream its settled usage to its own S3 bucket, Cloud Storage bucket or Azure Blob container, written with its verified cloud connection (no storage keys are stored). The worker queues a record for each `inference.finalized` event and writes gzip NDJSON objects per hour of `finalized_at`, under `organization=<id>/date=YYYY-MM-DD/hour=HH/`. A retry rewrites the same object with the same records, so each record lands in exactly one object. Records hold metadata only, never content; consumers sort by `finalized_at` and can deduplicate by `trace_id`. An export failing for an hour is reported in the admin API, metrics and a `usage_export.failing` webhook event.
- **Webhooks.** Outbox events go to an organization's webhook endpoints, signed with the endpoint's secret, at least once.

## Deletion

The gateway prefers hiding to erasing wherever history or a restore depends on what is deleted:

- **Organizations.** A deployment administrator can delete only a suspended organization, after typing its short name, and never the last one. The organization is hidden from every lookup, its short name and email domains are freed and pending invitations are revoked, but its rows and data stay. Its secrets stay stored, sealed and unused: provider credentials, cloud connections, webhook signing secrets and its identity provider's client secret. The delete dialog lists them (as counts, never values) and tells you to revoke them at each provider if the organization will not be restored. Restoring brings it back, suspended, with everything it had; nobody enters any secret again.
- **Service accounts.** Deleting a service account that has made requests disables it and keeps its usage.
- **API keys** are revoked, not removed: a revoked key keeps its budget, rate-limit policy and history.
- **Email domains, model-master links and similar records** are marked deleted and hidden, not erased.
- **Usage, traces and audit events** are never deleted by a person, only by the retention purge above.

Deleting or removing something in the gateway never revokes anything at a provider. A provider API key, cloud role, app registration or service account that the gateway was granted stays valid until you revoke it there.
