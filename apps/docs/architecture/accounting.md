---
description: "Money units, reservation states, upper-bound reservations, settlement, release, expiry, reconciliation, why the ledger is exact, and budget and rate-limit leases."
---

# Accounting

The gateway's accounting is a reservation ledger in PostgreSQL. Before a provider call, a request reserves the most it could cost against every budget that covers it; afterwards it is settled at its actual cost, released if the provider did no work, or held as `unknown` for reconciliation when its cost cannot be known. Amounts are integers in a fixed sub-unit of one currency, every transition is a single transaction, and every operation is idempotent, so concurrent requests, retries and crashes never spend a budget twice. The figures are what the gateway calculates from provider-reported usage and catalog prices: a strong control, not a ceiling on your provider's invoice ([what budgets do not cover](../operations/budgets.md#what-a-budget-does-not-cover)).

## Money units

Every monetary integer (catalog per-token prices, budget limits, reservations, charges, credits) counts **money units**: 10⁻⁶ of the currency's smallest unit. For USD and EUR that is a micro-cent; for JPY a micro-yen.

| Amount | Money units |
| --- | --- |
| $1.00 | 100,000,000 |
| $100 budget | 10,000,000,000 (10¹⁰) |
| $2.50 per million tokens | 250 per token |
| $0.10 per million tokens | 10 per token |

At this scale per-token prices are exact integers, so a charge is an exact sum of token counts times per-token prices, with no floating point and no rounding between requests. A published price finer than one unit per token is stored rounded up to the next unit, so the gateway may charge very slightly more than the provider, never less.

Currencies are three-letter codes. Amounts in different currencies are never converted or summed: a request is checked only against budgets in its price's currency, and every deployment of one route must be priced in one currency.

Token counts are explicit integers per kind: input, output, cached input, reasoning, and other (such as prompt-cache writes). A count the provider did not report stays unknown; it never becomes zero.

## Reservation states

```text
                 ┌──────────► expired     (never sent; released by recovery)
                 │
 reserved ──► claimed ──┬──► settled      (exact, complete usage charged)
                        ├──► released     (provider did no work; nothing charged)
                        └──► unknown ──► settled by an operator's resolution
```

| State | Meaning | Budget effect |
| --- | --- | --- |
| `reserved` | Admitted and held, not yet sent | Full amount held |
| `claimed` | Sent, or about to be sent, to the provider | Full amount held |
| `settled` | Charged at the observed usage | Actual cost consumed; the rest released |
| `released` | Refused before any provider work | Everything released |
| `unknown` | The provider may have done work the gateway cannot measure | Full amount stays held; a reconciliation item is open |
| `expired` | Never claimed before its expiry | Everything released |

A new inference reservation is written already claimed, in the same commit, since the provider call follows at once.

## Reservations at the upper bound

A reservation holds the most the request could cost at the route's price:

```text
reserved = input_bound × input price
         + output_cap  × output price
         + any reasoning allowance at its price
```

- **Input bound:** a strict upper bound on input tokens. By default it is the request's size in bytes times the deployment's maximum tokens per byte, plus fixed allowances for tool framing. Where a deployment's count mode is `provider_api_exact`, the provider's own token-counting endpoint supplies the bound, and a settlement whose reported input differs from it is sent to reconciliation rather than settled silently.
- **Output cap:** derived from what remains. The gateway takes the smallest remaining allowance across every covering budget (and tokens-per-minute limit), subtracts the input cost, and divides by the output price. The caller's larger `max_tokens` is overwritten with that cap before the provider is called, so the provider cannot generate past the reservation. If not even one output token fits, the request is refused with `429 quota_exceeded`.
- **Reasoning allowance:** for models that reason past `max_tokens`, a deployment's chat capability can reserve extra reasoning tokens. Unpriced reasoning is charged at the output price.
- **Price:** for a route over several deployments that a request may fail over between, the reservation is made at the highest price among them; settlement uses the price of the deployment that served it.

Each reservation stores an immutable price snapshot and its catalog provenance: release, route, deployment, price and any override that applied. Settlement always uses that snapshot. A price change, an adopted price or a new release affects only requests reserved after it is active.

## Admission is one transaction

Reserving a request is one PostgreSQL transaction:

1. An advisory lock on the (organization, service account, idempotency key) triple serializes first use of a key. An existing reservation with the same key is returned as it stands (a different request fingerprint is a conflict), and is never admitted or limited again.
2. Rate-limit policies covering the request are locked in id order and count it.
3. Each covering budget's current bucket is locked in a fixed order and must have room: `consumed + reserved + this request ≤ limit`. The amount is added to each bucket's `reserved`.
4. The reservation, one allocation per bucket, the price snapshot and a pending request record are inserted.

Because every concurrent request reserves against the same rows, a request is refused as soon as no reservation can fit. Settled usage never frees capacity that another request already holds. Fixed lock order means two requests can wait on each other but never deadlock.

Budget buckets carry `consumed_minor` and `reserved_minor`. A monthly budget has one bucket per UTC calendar month; a fixed budget has one for its window. Changing a limit applies to the current bucket and keeps what it has spent.

## Settlement

Settlement is a second transaction, after the provider has answered:

1. The usage the provider reported is recorded as an immutable usage row with its confidence.
2. The charge is computed from the reservation's price snapshot: each token count times its per-token price.
3. Each allocation's held amount is removed from its bucket's `reserved`, and the charge is added to `consumed`.
4. The reservation becomes `settled`; the request record is finalized with its amount; an `inference.finalized` event is written to the outbox.

Only complete, exact usage settles automatically. If the provider reported estimated or partial usage, or none, the usage is recorded but the reservation becomes `unknown` with a reconciliation item, keeping its full hold: the gateway never silently charges an amount it cannot vouch for.

Each settlement carries a finalization id recorded in its own transaction. A retry after a lost commit reply replays the recorded outcome instead of charging again, and a second settlement with different usage is refused as a conflict.

## Release

A provider failure that proves the provider did no work is **released**: nothing is charged and every held amount returns to its bucket. The proof is the same one failover needs: the request was never sent (connection refused, DNS or TLS failure), the deployment's circuit was open, or the provider refused it with `429`. Every other failure is `unknown`.

## Expiry and recovery

Every reservation has an expiry, from the deployment's reservation time-to-live (or a batch's completion window). The worker's accounting recovery looks for overdue reservations:

- A reservation still `reserved` (never sent) is **expired**: its holds are released.
- A reservation `claimed` past its expiry, because the process handling it died or lost the database, becomes `unknown` with an `expired-after-claim` reconciliation item, since the provider may have done the work.

A background settlement retry that succeeds after recovery ran still settles the reservation and resolves the item recovery opened.

## Reconciliation

A reconciliation item records a request whose cost the gateway could not settle by itself. Reasons include:

| Reason | Cause |
| --- | --- |
| `provider-unknown` | A timeout or dropped connection after the request was sent, a stream that ended without usage, or usage that was estimated or partial |
| `expired-after-claim` | The request's process stopped before settling it |
| `bound-breach` | The provider reported more usage than the reservation allowed for (for example, a model reasoning past its cap) |
| Input-count drift | The provider's reported input differs from the exact count it gave before the call |

While an item is open, its reservation's full amount stays held, so the budget never under-counts the request. An operator resolves it with the amount the provider actually billed (under *Usage → Reconciliation* in the console, or the admin API). Resolution settles the reservation at that amount in every bucket it held, which may take a budget over its limit; the amount is recorded as a final estimated usage row that keeps the observed token counts. Items emit `reconciliation.opened`, `reconciliation.updated` and `reconciliation.resolved` outbox events, and `gateway_reconciliation_items_open` counts open items.

This is the main way spending can pass a budget: a provider timeout after the provider accepted the request is held at the maximum reservation until someone resolves it, and the provider may bill it regardless. Set a spending limit or budget alert with each provider too; see [what a budget does not cover](../operations/budgets.md#what-a-budget-does-not-cover).

## Why it is exact

- **Integer units.** Prices, limits and charges are integers in one scale per currency; nothing is converted or rounded between requests.
- **One transaction per transition.** Admission with every hold, and settlement with every charge, the request record and the outbox event, each commit or roll back whole. Durable commits (`synchronous_commit = on`) mean a reservation is not lost after a provider call starts.
- **Holds before work.** The provider is called only after the reservation commits, with an output cap the reservation covers.
- **Idempotent everywhere.** Reservations are unique per idempotency key; settlements per finalization id; operator resolutions per item.
- **Nothing unknown becomes zero.** Unknown usage keeps its hold and is surfaced for a person, never written off or guessed.
- **History is immutable.** Settled usage keeps the price it was reserved at. Repricing the catalog never rewrites what was charged.

## Leases

Under very high load on one budget, its bucket row becomes a serial section: every request locks it to reserve and again to settle. Two optional mechanisms batch that work per process without changing what is enforced. Both are off by default.

### Budget leases

With `GATEWAY_BUDGET_LEASES=true`, a process takes a *lease* on a busy budget: one transaction moves part of the bucket's remaining allowance into the lease, counted as reserved on the bucket. The process then reserves requests against its lease in memory, without locking the bucket row. Each request still writes its own reservation and allocations and is still settled exactly once; only the bucket update is batched.

- **No overshoot.** A lease's amount is reserved on the bucket when taken, so consumed plus reserved can never pass the limit. The cost of a lease is that it can refuse a request another process could have admitted, while that process's lease is unused.
- **Size.** The smaller of 2% of what remains (divided among the processes holding leases on the budget) and five seconds of this process's recent reservations on it. A budget with room for fewer than 100 of the request's reservations is never leased, so the end of every budget is enforced row by row, exactly as without leases.
- **Return.** A lease takes new requests for 20 seconds, and is returned when idle for 5 seconds, when drained, or on shutdown. The worker returns any lease past its 60-second expiry, so a crashed process's leases come back within a minute.
- **Memory never over-counts.** A hold is taken before the reservation's transaction and given back only if it did not commit. Anything uncertain stays used until the lease returns, when the database's allocations decide the amounts.
- End users' allowances are not leased: admitting one already locks only that user's row.

### Rate-limit leases

With `GATEWAY_RATE_LIMIT_LEASES=true`, a process takes a lease of N admissions for a scope (organization, team or service account) in one transaction, under the same locks and checks as one admission. Taking it counts all N, in the current second and in flight, on every policy covering the scope, and only if each limit has room for them. The process then admits up to N requests in memory.

- **No overshoot.** A lease admits only until the end of the second it was taken in, and each reservation under it is stamped with its admission time, so each leased request falls in the second already counted for it. A unique index allows one reservation per lease ticket, so the database never lets a lease admit more than N.
- **Size.** The smaller of the process's admissions for the scope in the last second and 10% of each limit's room (shared among the processes holding leases in the organization). A limit with room for fewer than 100 more is not leased.
- **Exclusions.** An API key's own policy and tokens-per-minute limits are never leased; requests they cover are admitted on the policies' rows, so both stay exact.

Turn rate-limit leases on together with budget leases. See [scaling](scaling.md) for when they help.
