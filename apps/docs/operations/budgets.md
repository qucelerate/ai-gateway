# Budgets and their limits

Budgets cap what the gateway calculates for an organization, a team, a service account, one API key, or one end user of an app. Every inference request reserves its largest expected cost against each budget that covers it: its input bound, its output cap, and any reasoning allowance, at the catalog price. A request that does not fit is refused with `429` and code `quota_exceeded`, or has its output cap lowered to what still fits. A request that no budget covers is refused the same way, with a `detail` that says no budget covers it: an organization serves nothing until it has a budget. The request is then settled at its actual cost.

## Budgets per API key

Apps that share one service account can each have a limit of their own: give each app its own API key and set a budget on that key (*Policies → Budgets → Add budget → An API key*). A request with the key must fit the key's budget as well as its service account's, its team's and the organization's. When the key's budget is the only one that cannot fit a request, the `quota_exceeded` response's `detail` says so. A key's budget alone is enough to cover its requests. Revoking a key keeps its budget and what it spent visible.

## Budgets per end user of an app

An app that serves its own users through one service account can give each of them a limit, from a plan and from credits they bought. This is for apps that call the gateway from their own backend: the app names the user on each request, and its key is what makes that trustworthy. An app that calls from users' devices must give each user their own key instead.

- **Naming the user:** send `Gateway-End-User: <id>` on chat, embeddings and batch requests (or, without the header, the request body's `user` field). The ID is the app's own, 1 to 256 bytes, scoped to the service account. Send a pseudonymous ID, never an email address: it is stored as sent and shown to anyone who can read usage.
- **Settings:** *Identities → a service account → End users* (or `PUT …/service-accounts/{id}/end-user-settings`) picks the currency, whether every request must name a user (`required` refuses one without, `400 end_user_required`), and the default plan of users never assigned one. Without a default plan, unassigned users are refused (`403 end_user_not_registered`). Until settings exist, end users are recorded but not limited.
- **Plans** are a monthly allowance per user (UTC calendar month), which may be 0 for credits only. Moving a user to another plan applies from their next request and keeps what the month has spent.
- **Credits** are granted per purchase, with the app's order ID as the reference: the same reference again returns the grant already made, so a retried purchase webhook grants once. A refund or chargeback is a reversal, which takes back at most what is still unspent and reports how much that was.
- **Draw order:** a request spends the month's allowance first, then credits, the soonest-expiring grant first. Usage past what a request reserved is charged to the allowance, which can go over; credits never go below zero.
- **Refusals:** a request this user cannot afford is `429 quota_exceeded` with `end_user`, `allowance_remaining`, `credit_balance` and `currency` in the problem, so the app can offer an upgrade or a purchase. A `quota_exceeded` without `end_user` is an organization, team, service account or key budget, not the user's.
- **Who manages it:** settings and plans need `budget_write`; assigning plans and granting or reversing credits need `end_user_write` (owners, admins and operators), so an app's backend can do both with an agent token. `gwctl end-users …` and `gwctl credits …` and the matching MCP tools drive the same API.
- **Usage:** the usage dashboard's series and events split and filter by `end_user`.

The contract an app integrates against is `openapi/admin-end-users-v1.json` (admin endpoints) and `openapi/inference-v1.json` (the header and problems).

## What a budget does not cover

A budget is not a ceiling on your provider's invoice. The provider bills from its own meters, and the gateway cannot stop it billing for work it has already done. Spending can exceed a budget, or what the gateway records, when:

- **A model produces more than its cap allows for.** Some reasoning models reason past `max_tokens`. The provider bills the request, but the gateway records a reconciliation item and reports an error: `422` with code `accounting_bound_exceeded` for a non-streaming request, or, once a stream has started (the response is already `200`), a terminal `error` event with code `provider_failure` and status `502`.
- **The outcome is unknown.** A timeout or dropped connection after the provider accepted the request leaves its cost unknown. The gateway keeps the full reservation and records a reconciliation item, but the provider may bill the request.
- **Prices differ.** Charges use the catalog's prices. A placeholder, out-of-date, or overridden price makes the gateway's figure differ from the invoice.
- **Calls bypass the gateway.** Use of the same cloud account or keys outside the gateway is not counted, and neither are the gateway's own small model-availability checks.

## Recommended safeguards

- Set a spending limit or budget alert with each provider as well. It is the backstop for every case above.
- Watch for open reconciliation items, and resolve each with the provider's actual amount.
- Compare the gateway's usage reports with your provider invoices.

> **Scoped:** this page describes budget behavior; it makes no promise about provider billing, which each provider's own terms govern.
