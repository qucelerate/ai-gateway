---
description: "The path of one inference request: authentication, idempotency, routing, admission and reservation, the provider call and failover, settlement, streaming, and the outbox."
---

# Request lifecycle

Every inference request takes the same path: the gateway authenticates the API key, resolves the model alias from its loaded configuration, picks a healthy deployment, admits the request against every rate limit and reserves its largest possible cost against every budget in one database transaction, calls the provider with an output cap it can afford, and settles the actual cost in a second transaction that also writes the request record and an outbox event. Nothing is held in a database transaction while the provider works, and no request is charged twice.

<figure class="diagram">
<svg role="img" aria-labelledby="lifecycle-title lifecycle-desc" viewBox="0 0 760 500" width="100%" xmlns="http://www.w3.org/2000/svg" font-family="system-ui, sans-serif" font-size="14">
<title id="lifecycle-title">The path of one inference request</title>
<desc id="lifecycle-desc">Seven steps from top to bottom: authenticate the key, resolve the alias, pick a deployment, admit and reserve in one PostgreSQL transaction, call the provider, settle in a second transaction, and deliver events from the outbox. Beside each step is what it reads or writes.</desc>
<defs>
<marker id="lifecycle-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="currentColor"/></marker>
</defs>
<g fill="none" stroke="currentColor" stroke-width="1.5">
<rect x="20" y="10" width="290" height="50" rx="6"/>
<rect x="20" y="80" width="290" height="50" rx="6"/>
<rect x="20" y="150" width="290" height="50" rx="6"/>
<rect x="20" y="220" width="290" height="50" rx="6" stroke-width="3"/>
<rect x="20" y="290" width="290" height="50" rx="6"/>
<rect x="20" y="360" width="290" height="50" rx="6" stroke-width="3"/>
<rect x="20" y="430" width="290" height="50" rx="6"/>
</g>
<g fill="none" stroke="currentColor" stroke-width="1.5" marker-end="url(#lifecycle-arrow)">
<line x1="165" y1="60" x2="165" y2="78"/>
<line x1="165" y1="130" x2="165" y2="148"/>
<line x1="165" y1="200" x2="165" y2="218"/>
<line x1="165" y1="270" x2="165" y2="288"/>
<line x1="165" y1="340" x2="165" y2="358"/>
<line x1="165" y1="410" x2="165" y2="428"/>
</g>
<g fill="currentColor">
<text x="40" y="40">1. Authenticate the key</text>
<text x="40" y="110">2. Resolve the alias</text>
<text x="40" y="180">3. Pick a deployment</text>
<text x="40" y="250" font-weight="600">4. Admit and reserve</text>
<text x="40" y="320">5. Call the provider</text>
<text x="40" y="390" font-weight="600">6. Settle</text>
<text x="40" y="460">7. Deliver events</text>
<g font-size="13">
<text x="340" y="31">API key digest, scopes and organization state,</text>
<text x="340" y="49">read from PostgreSQL on every request</text>
<text x="340" y="101">configuration version held in memory:</text>
<text x="340" y="119">routes, limits, prices, routing policy</text>
<text x="340" y="171">weight and priority; skip open circuits,</text>
<text x="340" y="189">provider back-off and refused evidence</text>
<text x="340" y="241">one PostgreSQL transaction: rate limits,</text>
<text x="340" y="259">every budget, the reservation and its trace</text>
<text x="340" y="311">no transaction open; output cap enforced;</text>
<text x="340" y="329">failover only when no work can have started</text>
<text x="340" y="381">one PostgreSQL transaction: usage, charge,</text>
<text x="340" y="399">release of the rest, request record, outbox event</text>
<text x="340" y="451">worker: webhooks, usage export,</text>
<text x="340" y="469">analytics sink</text>
</g>
</g>
</svg>
<figcaption>One inference request. The two bold steps are the database transactions that hold and charge money.</figcaption>
</figure>

The same steps in words:

1. **Authenticate.** The gateway checks the bearer API key against its stored digest and reads the key's service account, team, organization, scopes and state.
2. **Resolve the alias.** The `model` in the request is an alias. The process looks it up in the configuration version it has loaded for the organization, which names the route's deployments, their limits and prices.
3. **Pick a deployment.** Among the route's healthy deployments, it picks by priority, then weight.
4. **Admit and reserve.** In one transaction, rate-limit policies admit the request and every budget that covers it holds its largest possible cost.
5. **Call the provider.** The request is sent with the output cap the reservation granted. A refusal that proves no work started may fail over to another deployment.
6. **Settle.** In a second transaction, the observed usage is priced, charged, and the rest of the reservation is released. The request record and an `inference.finalized` outbox event are written in the same transaction.
7. **Deliver.** The worker dispatches outbox events to webhooks, usage export and the optional analytics sink.

## Authentication

Inference accepts only API keys (`Authorization: Bearer gw_…`). Each key belongs to a service account and carries scopes: chat needs `inference`, embeddings need `embeddings`. Admin sessions and agent tokens are never accepted on inference routes, and API keys are never accepted on admin routes. A suspended organization's keys are refused. Keys, their revocation and the organization's state are read live, never from cached configuration, so revoking a key takes effect on the next request in every region. See the [security model](security.md).

An app that serves its own users can name the user with the `Gateway-End-User` header (or the body's `user` field) so that per-user plans apply; see [budgets and rate limits](budgets-and-rate-limits.md#end-users-plans-and-credits).

## Idempotency

Every inference request carries an `Idempotency-Key`. The gateway records it with the reservation, unique per service account, together with a fingerprint of the request:

- The same key with a different request is refused with `409 idempotency_conflict`.
- The same key while the first attempt is still running is refused with `409 idempotency_in_progress`.
- The same key after the first attempt finished is refused with `409 idempotency_terminal`.

No response is stored or replayed, so a key never runs or charges a request twice. A request refused before anything was reserved (a rate limit, an open circuit) can be sent again with the same key. A caller that lost an answer sends a new request with a new key. Provider calls carry a provider idempotency key derived from the request where the provider supports one.

## Routing

Every API-serving process routes from a *configuration version* it holds in memory: an immutable snapshot of the organization's active catalog release (aliases, routes, deployments, capability limits and prices) and its price overrides. A request is never routed from a database read, so routing survives a brief loss of the primary, though the request then fails at reservation. See [configuration versions](catalog.md#configuration-versions).

A route names one or more deployments, each with a `weight` and a `priority`. The gateway considers the lowest priority that has a healthy deployment and picks among those by weight. A deployment is skipped while:

- its circuit is open (see [circuits](providers.md#circuits-and-health));
- its provider's `429` back-off (`Retry-After`) is running; or
- the account's availability evidence for the model is `refused` or `requires_action`.

The organization's routing policy can restrict provider families and deployment locations. It narrows the candidates before weights apply; a request that no permitted deployment can serve is refused with `routing_policy_denied` rather than sent elsewhere.

Before anything is reserved, the request is checked against the route's contract: its input size, its `max_tokens`, its tools and their options. Every deployment of a route takes the tightest limits of all of them, so a request never fits one deployment and not another.

## Admission and reservation

The reservation is computed from bounds the gateway trusts, not from what the caller claims:

- **Input bound.** A strict upper bound on input tokens: the request's size in bytes times the route's tokens-per-byte, or, where the route uses it, the provider's own token-counting endpoint. Tool definitions and tool calls count towards it.
- **Output cap.** The smaller of the caller's `max_tokens` and the deployment's limit, lowered further to what every budget and every tokens-per-minute limit can still afford. If not even one output token is affordable, the request is refused.
- **Reasoning allowance.** For models that reason past `max_tokens`, the route's reasoning allowance is reserved too.
- **Price.** The route's highest price among the deployments the request may reach, so a failover never costs more than was reserved.

One PostgreSQL transaction then admits and reserves:

1. It checks the idempotency key (above).
2. Every rate-limit policy that covers the request (organization, team, service account, API key, and the end user's plan) counts it, or refuses it with `429`.
3. Every budget that covers it (organization, team, service account, API key, end user) holds the reserved amount, or the request is refused with `429 quota_exceeded`.
4. It inserts the reservation, with an expiry, its price snapshot, the catalog provenance (release, route, deployment, price), and a pending request record.

Locks are always taken in a fixed order, so concurrent requests never deadlock and never pass a limit together. The reservation is committed before the provider is called; the transaction never stays open over network I/O. The [accounting page](accounting.md) explains the amounts, and [budgets and rate limits](budgets-and-rate-limits.md) explains every level and refusal.

## The provider call

The gateway overwrites `max_tokens` with the output cap the reservation granted, so the provider cannot generate past what was reserved. It then calls the deployment through its adapter within the reservation's deadline.

**Retries.** The gateway retries a provider call only when the attempt cannot have been processed: the connection never opened, or the provider refused it with a transient `429`. It tries at most three times in all, with jittered backoff or the provider's `Retry-After`, within a bounded wait. Timeouts, dropped connections and `5xx` responses are never retried, because the provider may already have done (and billed) the work.

**Failover.** When a route has more than one deployment, a request whose attempt was refused before any work (never sent, an open circuit, a `429`) moves to the route's next healthy deployment: at most three attempts within the first reservation's deadline. Each attempt has its own reservation and request record, linked to the one it replaces. A request never fails over after a response has started, or after a timeout once it was sent, since that could charge twice.

## Settlement

When the provider answers, a second transaction settles the reservation:

- the observed usage is recorded, immutably;
- each token count is charged at the price snapshot the reservation took, never today's price;
- every budget's held amount becomes the actual charge, and the difference is released;
- the request record is finalized, and an `inference.finalized` event is written to the outbox.

A provider failure that proves no work was done is released, charging nothing. Anything else whose cost the gateway cannot know exactly (a timeout after the request was sent, a connection dropped mid-response, missing or estimated usage) is marked `unknown`: the full reservation stays held and a reconciliation item is opened for an operator to resolve with the provider's actual amount. See [reconciliation](accounting.md#reconciliation).

A settlement that hits a transient database failure is retried with the same finalization id, a few times inside the request and then in the background for up to 15 minutes. A retry after a lost commit reply replays the recorded outcome instead of charging twice.

## Streaming

Streaming (`POST /api/inference/v1/chat/completions/stream`) is a delivery mode of chat: a streamed request takes the alias's `chat` route, with the same price and limits. The differences are in how it ends:

- Events are sent as the provider produces them. A stream ends with one terminal event, `response.completed` or `error`, unless the connection itself is lost (for example, the serving instance fails).
- The provider-side output cap is the hard stop. Cancelling on a client disconnect is an extra safeguard, not the enforcement.
- Settlement runs when the provider's terminal usage arrives, before the terminal event is sent to the client.
- A client that disconnects drops the response, which cancels the provider call. Without the provider's final usage the cost is unknown, so the reservation is held as `unknown` with a reconciliation item.
- Failover is possible only before the stream starts. Once events flow, a failure ends the stream with an `error` event.
- Streams cannot be resumed. A broken stream is a new request with a new idempotency key.

See [streaming](../quickstarts/streaming.md) for the event format.

## After the request

The outbox holds fixed-shape events built from identifiers, amounts and closed vocabularies, never content or credentials. The worker claims them transactionally and delivers each at least once; consumers deduplicate by event id. Events include `inference.finalized`, the `reconciliation.*` events, `usage_export.failing` and `notification.created`.

A reservation that is never settled, because its process died, expires. The worker's accounting recovery then releases it if it was never sent, or marks it `unknown` with a reconciliation item if the provider may have done the work. See [accounting](accounting.md#expiry-and-recovery).

## Errors along the way

| Step | Typical refusal | Anything reserved? |
| --- | --- | --- |
| Authentication | `401 invalid_api_key`, `403 insufficient_scope` | No |
| Idempotency | `409 idempotency_conflict`, `idempotency_in_progress`, `idempotency_terminal` | No new reservation |
| Routing | `model_unavailable`, `routing_policy_denied`, `tools_unsupported`, `503 provider_circuit_open` | No |
| Admission | `429 rate_limit_exceeded`, `concurrency_limit_exceeded`, `quota_exceeded` | No |
| Provider call | `provider_rate_limited`, `provider_unavailable`, `provider_timeout`, `provider_failure` | Released, or held as `unknown` |

The [error reference](../support/errors.md) lists every code, and [handling errors](../quickstarts/errors.md) says which to retry.
