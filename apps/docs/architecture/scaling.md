---
description: "Where requests serialize, connection pools, PgBouncer, leases, storage growth, the optional ClickHouse sink, and measured limits."
---

# Scaling

API processes are stateless and scale horizontally behind a load balancer; workers scale as replicas; PostgreSQL stays the single system of record for money, so its primary is what ultimately bounds throughput. Two things limit a busy deployment: the rows one budget or rate-limit policy shares between all of its requests, and the database's commit rate. This page explains each limit, the settings that move it (connection pools, budget and rate-limit leases, PgBouncer, the optional ClickHouse sink), and the orders of magnitude measured on a modest single PostgreSQL node.

## What scales how

| Part | How it scales | Notes |
| --- | --- | --- |
| `api` and `data-plane` processes | Horizontally: add instances behind a load balancer | No session affinity is needed. Each routes from its own copy of the configuration, and every reservation is a transaction on the primary. |
| `worker` | Replicas | Jobs are claimed transactionally and processed idempotently, so replicas never duplicate work. One worker is enough for most deployments. |
| PostgreSQL primary | Vertically: cores and, above all, fast WAL storage | Every reservation and settlement commits here. Keep `synchronous_commit = on`: a reservation must not be lost after a provider call starts. |
| Analytics reads | Optional ClickHouse sink | Moves dashboard, report and trace-search scans off PostgreSQL. |

## Where requests serialize

### One budget

Every reservation locks each covering budget's bucket row to hold its amount, and every settlement updates it. Requests charged to one budget therefore pass through that row one at a time, whatever the number of gateway processes: more processes only add waiters. A rate-limit policy's counter row behaves the same way for admission.

This is the limit no tuning fixes, and it is what [budget and rate-limit leases](accounting.md#leases) address. Spreading traffic over several budgets (per team, per service account, per key) also spreads it over several rows, though a request must still pass every budget above it, including the organization's.

### The commit path

Spread over many organizations and budgets, the database's commit rate becomes the limit. A request writes its own records in two commits: the claimed reservation with its pending request record, then the settlement with its usage, request record and outbox event. The worker's outbox dispatch and other jobs add a few more. Once the database spends its time flushing WAL, faster WAL storage raises throughput directly; a network disk lowers it.

## Connection pools

Each process opens one pool per role it runs, of `GATEWAY_DATABASE_MAX_CONNECTIONS` connections (1 to 1,000; 10 by default). An `api` or `worker` process has one pool; an `all-in-one` process has both, so it can open twice the setting.

- Raise the pool while `gateway_db_pool_connections{state="in_use"}` sits at the pool size and the database still has idle cores. Raising it past the point where the database's cores are busy adds only waiting.
- Size PostgreSQL's `max_connections` for the sum across every process and pool, plus headroom for migrations and administration.

## PgBouncer

Run PgBouncer in **transaction** mode in front of PostgreSQL once there are more gateway processes than the database's connection budget. The gateway uses prepared statements, so PgBouncer must be 1.21 or later with `max_prepared_statements` set. PgBouncer lowers the number of server connections, not the work: its server pool must be as large as the connections the gateways would otherwise use, and it adds no throughput. It helps only where PostgreSQL's connection limit is what runs out.

## Leases

Both kinds of lease are off by default and are turned on per deployment. Neither lets any limit be passed; both may refuse a request that another process could have admitted, for a short time, while that process's lease is unused. Details are on the [accounting page](accounting.md#leases).

| Setting | Batches | Helps when |
| --- | --- | --- |
| `GATEWAY_BUDGET_LEASES=true` | Budget bucket updates, per process | One budget takes more requests than its row can serialize |
| `GATEWAY_RATE_LIMIT_LEASES=true` | Rate-limit admissions, per process and scope | One organization with rate-limit policies is busy; turn it on together with budget leases |

Budgets near their limit, and limits near theirs, are never leased, so the end of every budget and limit is enforced row by row. An API key's own rate-limit policy and tokens-per-minute limits are never leased and stay serialized on their rows. Watch `gateway_budget_leases_total`, `gateway_budget_lease_reservations_total{path}` and `gateway_rate_limit_admissions_total{path}` to see how much traffic goes through leases.

## Request size and memory

An inference body may be up to `GATEWAY_INFERENCE_MAX_BODY_BYTES` (4 MiB by default; 64 KiB to 64 MiB), room for a prompt near a million tokens; a larger one is refused with `413 request_too_large`. Each deployment's `max_input_bytes` still bounds what it accepts. A request holds its body while it is read and parsed, so size gateway memory for the bodies in flight. An open stream costs a gateway process tens of kilobytes of memory and one file descriptor.

## Storage growth

Without trace content, the request tables grow by roughly 4 KB of PostgreSQL storage per request, before vacuum reclaims the outbox's dispatched rows. At tens of thousands of requests a minute that is terabytes a month, so a large deployment should set [usage and trace retention](data-and-retention.md#retention). Retention purges delete in bounded batches every hour. Trace content lives in the object store, not in PostgreSQL.

## Analytics off PostgreSQL

With `GATEWAY_CLICKHOUSE_URL` set, the worker copies each request's record (never content) to ClickHouse from the same outbox events as usage export, after a backfill of history. The usage dashboard, the usage report's grouping and trace search then read from it, taking requests ClickHouse may not have caught up with from one PostgreSQL snapshot, so their answers equal PostgreSQL's at any lag. Budgets, settlement, reconciliation, a trace's detail and exports always read PostgreSQL. Aggregations over hundreds of thousands of requests are one to two orders of magnitude faster; a single page of traces, which PostgreSQL answers from an index, is not. Without the URL nothing changes.

## Measured limits

The repository's load harness drives gateways against a fake provider and checks afterwards that accounting is exact: every request settled once, budgets equal to their allocations, nothing left reserved. These orders of magnitude come from one modest machine running PostgreSQL, the gateways and the load generator together, with `synchronous_commit = on`. They depend heavily on hardware (database cores and WAL storage above all), request mix and settings; measure your own.

| Situation | Order of magnitude | What limits it |
| --- | --- | --- |
| One budget, no leases | A few hundred requests a second, however many processes | The budget's row |
| One organization with rate-limit policies, no rate-limit leases | A few hundred requests a second | The policy's counter row |
| One organization with budget and rate-limit leases | Several hundred requests a second with tens of milliseconds added at p95, where the same load without leases queued for tens of seconds | The machine |
| Many organizations, one process with the default pool | Under a thousand requests a second | The pool (every connection busy) |
| Many organizations, several processes or a larger pool | Over a thousand requests a second | The database's commit path |
| Many organizations through PgBouncer with half as many server connections | About half the direct figure | PgBouncer's server pool |

At comfortable load the gateway adds tens of milliseconds at p95 over the provider's own latency. In failure tests, a gateway process killed mid-request and a PostgreSQL restart each recovered within a second of the process coming back, with in-flight requests failing or finishing cleanly, their reservations expired by the worker, and accounting exact afterwards.

## Checklist for a busy deployment

1. Split `api` and `worker`, and run enough `api` instances that no pool sits at its limit.
2. Put PostgreSQL's WAL on fast local storage; keep `synchronous_commit = on`.
3. Raise `GATEWAY_DATABASE_MAX_CONNECTIONS` while pools are the wait; add PgBouncer in transaction mode when connections run out.
4. Turn on budget leases, and rate-limit leases with them, when one organization is busy.
5. Set usage and trace retention.
6. Consider the ClickHouse sink when dashboards and reports slow down.
7. Watch the [reliability signals](../operations/signals.md) and keep provider-side spending limits in place ([budgets and their limits](../operations/budgets.md)).
