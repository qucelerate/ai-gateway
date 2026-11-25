---
description: "Budgets and rate limits at every level (organization, team, service account, API key, end user and plans), and the 429 problem bodies that name the scope and limit."
---

# Budgets and rate limits

Every inference request is checked at every level that covers it: the organization, the API key's team, its service account, the key itself, and, when an app names one, the app's end user. Budgets limit what requests may spend; rate limits limit how many requests, how many tokens and how many requests in flight. A request must fit all of them, and they are all checked and held in the same database transaction that reserves it. A refusal is a `429` problem that names the level and the limit. For what budgets can and cannot promise about provider bills, see [budgets and their limits](../operations/budgets.md).

## Levels

| Level | Covers | Budget | Requests per minute | Tokens per minute | Requests in flight |
| --- | --- | --- | --- | --- | --- |
| Organization | Every request | yes | yes | yes | yes |
| Team | Requests with keys of the team's service accounts | yes | yes | yes | yes |
| Service account | Requests with any of its keys | yes | yes | yes | yes |
| API key | Requests with that key only | yes | yes | yes | yes |
| End user | One user of an app, by the plan they are on | allowance and credits | yes | yes | yes |

Every policy that covers a request applies; a lower level never bypasses a higher one. A request made with an API key must fit that key's budget, its service account's, its team's and the organization's, and pass each of their rate limits.

The two kinds differ in one respect:

- **Budgets are required.** A request that no budget covers is refused with `429 quota_exceeded`, whose `detail` says that no budget covers it. An organization serves nothing until it has a budget, so spending is never unlimited by omission. One budget at any level is enough to cover its requests.
- **Rate limits are optional.** A request no rate-limit policy covers is not rate-limited.

Budgets and rate-limit policies are set in the admin console (*Policies*) or through the admin API, need `budget_write` or `rate_limit_write` (owners and admins), and every change is audited. A revoked API key keeps its budget and policy, still listed with what it spent.

## Budgets

A budget is an amount in [money units](accounting.md#money-units) of one currency (USD by default) over a period:

- `monthly`: each calendar month in UTC, from a fresh balance;
- `fixed`: one window, from `starts_at` to `ends_at`.

A request is charged in the currency of its price, and only budgets in that currency cover it. Raising or lowering a limit applies to the current period at once and keeps what it has spent; changing the currency, period or window starts a new balance. Removing a budget stops it covering new requests, while requests already admitted still settle.

Each request [reserves](accounting.md#reservations-at-the-upper-bound) its largest possible cost against every covering budget, and its output cap is lowered to what all of them can still afford. A request that cannot fit even one output token is refused:

```jsonc
// 429 Too Many Requests, Content-Type: application/problem+json
{
  "type": "https://gateway.invalid/problems/quota_exceeded",
  "code": "quota_exceeded",
  "title": "Too Many Requests",
  "status": 429,
  "detail": "the budgets covering this request cannot cover its largest possible cost",
  "instance": "/api/inference/v1/chat/completions",
  "request_id": "req_01J8EXAMPLE8W5Y"
}
```

The `detail` is more specific when it can be: when no budget covers the request, it says so, and when the API key's own budget is the only one that cannot fit it, it says that the key's budget cannot cover the request. Retrying does not help until a budget is raised, resets, or the request asks for a smaller `max_tokens`.

## Rate limits

A rate-limit policy at any level can set any of three limits:

| Limit | Counts | Window |
| --- | --- | --- |
| `requests_per_minute` | Requests admitted | Sliding 60 seconds, in whole seconds, on the database clock |
| `tokens_per_minute` | While a request runs: its input bound, output cap and reasoning allowance. Once settled: the tokens its provider reported. A request released without usage counts none. | The same sliding window |
| `max_concurrent` | Requests in flight: admitted and not yet settled, released, unknown or expired | Instantaneous |

Rules that apply to all of them:

- **Admission only.** Limits are checked when a new reservation is made, before any budget. A replay of an idempotency key that already has a reservation is never limited.
- **Lowered caps.** A request that fits a tokens-per-minute limit only with a smaller output cap is admitted with that cap, as under a budget. A request whose input bound and reasoning allowance, plus one output token, exceed the limit can never fit, and the `detail` says so.
- **Failover.** A failover attempt is the same request: it is not counted again towards requests per minute, is counted in the second of the request it continues, and never holds more tokens than the attempt it replaces.
- **Batches.** Creating a batch counts one request. An open batch does not count towards requests in flight, and its lines count no tokens (they are reserved against budgets in full). Emulated batch lines count as ordinary requests.
- **Exactness.** Each policy keeps a fixed-size counter row with a ring of per-second slots, updated in the reservation's transaction and again when the request settles or is released. Concurrent requests are counted one after another under the policy's lock, so they cannot pass a limit together.

A request over a limit is refused before anything is reserved, with `Retry-After` in whole seconds and problem fields naming the level (`scope`: `organization`, `team`, `service-account` or `api-key`) and the limit:

```jsonc
// 429 Too Many Requests, Retry-After: 17
{
  "type": "https://gateway.invalid/problems/rate_limit_exceeded",
  "code": "rate_limit_exceeded",
  "title": "Too Many Requests",
  "status": 429,
  "detail": "the team's requests_per_minute limit is reached",
  "instance": "/api/inference/v1/chat/completions",
  "request_id": "req_01J8EXAMPLE8W5Y",
  "scope": "team",
  "limit": "requests_per_minute"
}
```

A concurrency refusal has code `concurrency_limit_exceeded` and `limit: max_concurrent`. When several policies are over their limits, the one named is the one the request can never fit, or else the one with the longest wait. Nothing was reserved, so the request can be sent again with the same idempotency key after `Retry-After`.

## End users: plans and credits

An app that serves its own users through one service account can limit each of them. The gateway does not know the app's users; the app names one on each request with the `Gateway-End-User` header (or, without it, the body's `user` field), and the app's key is what makes that claim trustworthy. This is for apps that call the gateway from their own backend. An app whose users' devices call the gateway directly must give each user their own credential instead.

- **Identity.** The ID is the app's own, 1 to 256 bytes, scoped to the service account, so one app's `user-42` is never another's. Send a pseudonymous ID, never an email address: it is stored as sent and shown to anyone who can read usage.
- **Settings.** A service account's end-user settings fix one currency, whether a user must be named (`required` refuses a request without one with `400 end_user_required`), and a default plan. A user who was never assigned a plan is on the default plan, or refused with `403 end_user_not_registered` if there is none. Without settings, end users are recorded for attribution but not limited.
- **Plans.** A named plan gives each of its users an allowance per period, which may be 0 for credits only. The period is the UTC calendar month, the ISO week, the UTC day, or a fixed number of days. A plan can also set `requests_per_minute`, `tokens_per_minute` and `max_concurrent` for each of its users, with the same semantics as the policies above, applied after every policy has admitted the request. Moving a user to another plan applies from their next request and keeps what the period has spent.
- **Credits.** Prepaid grants per user, with an optional expiry and the app's purchase reference. The reference is unique per service account, so a retried purchase webhook grants once. A refund is a reversal, which takes back at most what is still unspent and unreserved.
- **Draw order.** A request spends the period's allowance first, then credits, soonest-expiring grant first. Usage past what a request reserved is charged to the allowance, which can go over; credits never go below zero.
- **Enforcement.** The user's allowance and credits are reserved in the same transaction as every other budget, with their rows locked in the same fixed order. End-user rows are created by the user's first request, so a million users need nothing in advance.
- **Management.** Settings and plans need `budget_write`; assigning plans and granting credits need `end_user_write`, so an app's backend can do both with an agent token without being able to change the organization's budgets. An agent token can be limited to one service account's end users.

An end user's refusals carry `end_user`, so the app can tell its user's limit from the organization's. A budget refusal also says what is left:

```jsonc
// 429 Too Many Requests
{
  "type": "https://gateway.invalid/problems/quota_exceeded",
  "code": "quota_exceeded",
  "title": "Too Many Requests",
  "status": 429,
  "detail": "end user \"user-42\" has used this month's allowance and has no credits that cover this request",
  "instance": "/api/inference/v1/chat/completions",
  "request_id": "req_01J8EXAMPLE8W5Y",
  "end_user": "user-42",
  "allowance_remaining": 0,
  "credit_balance": 1200000,
  "currency": "USD"
}
```

`credit_balance` can be above zero and still not cover a request, because the reservation is the request's largest cost; a smaller `max_tokens` may fit.

| Status and code | With `end_user` | Without `end_user` |
| --- | --- | --- |
| `429 quota_exceeded` | This user's allowance and credits cannot cover the request: offer an upgrade or a purchase | An organization, team, service account or key budget is used up: an operator problem |
| `429 rate_limit_exceeded`, `concurrency_limit_exceeded` | This user's plan limit (`limit`) is reached | A policy at `scope` is reached |

## Where they are enforced

Rate limits and budgets are read live on every request and are never part of the versioned routing configuration. Lowering a cap therefore takes effect at once in every region, and pinning an older configuration version never undoes it. All checks run on the PostgreSQL primary, which is why a process that cannot reach it refuses inference instead of serving it unmetered.

At very high request rates on a single budget or policy, the locked rows become the bottleneck. Optional [budget and rate-limit leases](accounting.md#leases) batch them per process without allowing any limit to be passed; see [scaling](scaling.md).
