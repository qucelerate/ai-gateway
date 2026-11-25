---
description: "The components (API, worker, admin console, PostgreSQL, optional ClickHouse, trace object store), the four process profiles, and the deployment shapes."
---

# Architecture overview

The QuCelerate AI Gateway is one Rust binary that serves an authenticated, tenant-aware inference API in front of your own model provider accounts. It routes each request to a deployment from a versioned catalog, reserves its largest possible cost against every budget and rate limit that covers it before the provider sees it, and settles the actual cost afterwards. PostgreSQL is the system of record for all of it. This section describes how the parts fit together; the [guide](../guide/concepts.md) and [reference](../reference/endpoints.md) describe how to call it.

## Components

<figure class="diagram">
<svg role="img" aria-labelledby="arch-overview-title arch-overview-desc" viewBox="0 0 800 440" width="100%" xmlns="http://www.w3.org/2000/svg" font-family="system-ui, sans-serif" font-size="14">
<title id="arch-overview-title">Gateway components</title>
<desc id="arch-overview-desc">Applications and people reach an HTTPS edge. The edge forwards API traffic to gateway API processes and serves the static admin console. API processes and the worker read and write PostgreSQL. API processes call model providers and write trace content to an object store. The worker also calls providers for discovery, prices and batches, and can feed an optional ClickHouse analytics store.</desc>
<defs>
<marker id="arch-overview-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="currentColor"/></marker>
</defs>
<g fill="none" stroke="currentColor" stroke-width="1.5">
<rect x="40" y="20" width="220" height="50" rx="6"/>
<rect x="300" y="20" width="300" height="50" rx="6"/>
<rect x="40" y="110" width="560" height="44" rx="6"/>
<rect x="40" y="200" width="260" height="70" rx="6"/>
<rect x="330" y="200" width="130" height="70" rx="6"/>
<rect x="490" y="200" width="110" height="70" rx="6"/>
<rect x="640" y="200" width="140" height="70" rx="6"/>
<rect x="40" y="340" width="200" height="70" rx="6"/>
<rect x="270" y="340" width="190" height="70" rx="6"/>
<rect x="490" y="340" width="150" height="70" rx="6" stroke-dasharray="6 4"/>
</g>
<g fill="none" stroke="currentColor" stroke-width="1.5" marker-end="url(#arch-overview-arrow)">
<line x1="150" y1="70" x2="150" y2="108"/>
<line x1="450" y1="70" x2="450" y2="108"/>
<line x1="150" y1="154" x2="150" y2="198"/>
<line x1="395" y1="154" x2="395" y2="198"/>
<polyline points="280,200 280,180 710,180 710,198"/>
<polyline points="545,200 545,180"/>
<line x1="120" y1="270" x2="120" y2="338"/>
<line x1="260" y1="270" x2="330" y2="338"/>
<line x1="520" y1="270" x2="200" y2="338"/>
<line x1="570" y1="270" x2="570" y2="338"/>
</g>
<g fill="currentColor" text-anchor="middle">
<text x="150" y="41">Applications</text>
<text x="150" y="60" font-size="13">API keys (gw_)</text>
<text x="450" y="41">People and agents</text>
<text x="450" y="60" font-size="13">browser sessions, agent tokens (gwa_)</text>
<text x="320" y="137">HTTPS edge: ingress, load balancer or reverse proxy</text>
<text x="170" y="226">Gateway API process</text>
<text x="170" y="246" font-size="13">all-in-one · api · data-plane</text>
<text x="395" y="226">Admin console</text>
<text x="395" y="246" font-size="13">static files</text>
<text x="545" y="226">Worker</text>
<text x="545" y="246" font-size="13">background jobs</text>
<text x="710" y="226">Model providers</text>
<text x="710" y="246" font-size="13">your accounts</text>
<text x="140" y="366">PostgreSQL</text>
<text x="140" y="386" font-size="13">system of record</text>
<text x="365" y="366">Trace object store</text>
<text x="365" y="386" font-size="13">S3-compatible, encrypted</text>
<text x="565" y="366">ClickHouse</text>
<text x="565" y="386" font-size="13">optional analytics</text>
</g>
</svg>
<figcaption>The gateway's components. Dashed: optional.</figcaption>
</figure>

In words:

1. Applications call the inference API with API keys; people use the admin console with a browser session, and automation uses agent tokens. Both arrive through your HTTPS edge.
2. The edge sends `/api/*` and `/healthz` to gateway API processes and everything else to the admin console's static files, so the console and the admin API share one origin.
3. API processes authenticate, route, reserve, call the provider and settle. Every reservation and settlement is a transaction on the PostgreSQL primary.
4. Traced request content goes to an S3-compatible object store, encrypted before it leaves the process.
5. The worker runs the background jobs. It also calls providers (model discovery, price sources, native batches) and, when configured, copies request records to ClickHouse.

| Component | What it is | State it keeps |
| --- | --- | --- |
| Gateway API process | The `gateway` binary in the `all-in-one`, `api` or `data-plane` profile. Serves `/api/inference/v1/*`; the `all-in-one` and `api` profiles also serve the admin API (`/api/admin/v1/*`), sign-in, and the MCP endpoint for agents (`/api/mcp`). Metrics are served on a separate listener (`GATEWAY_METRICS_ADDR`). | None of its own. Routing configuration is cached in memory; everything else is read from PostgreSQL. |
| Worker | The same binary in the `worker` profile (also inside `all-in-one`). Runs accounting recovery, outbox dispatch and webhook delivery, usage export, model discovery, price sync and price adoption, native batches, trace-content expiry, retention purges and the optional analytics sink. Jobs are claimed transactionally and are safe to run on several replicas. | None of its own. |
| Admin console | A static single-page application. It holds no credentials and talks only to the admin API. | None. |
| PostgreSQL | The only mandatory stateful service. Holds tenants, identities, key digests, sealed provider credentials, the catalog, configuration versions, budgets, rate-limit counters, reservations, usage, request records, audit events and the outbox. | Everything authoritative. |
| Trace object store | An S3 bucket or S3-compatible store, used only when tracing keeps content. Each trace's content is sealed with its own key; neither the store nor the database alone yields it. | Encrypted request and response content. |
| ClickHouse | Optional. A copy of each request's record (never content) that the usage dashboard, the usage report's grouping and trace search read from once it has caught up. Budgets and settlement never read it. | A derived copy, rebuilt from PostgreSQL. |

## Profiles

One immutable image runs in one of four profiles, chosen with `GATEWAY_PROFILE`:

| Profile | Serves | Typical use |
| --- | --- | --- |
| `all-in-one` | Inference, admin API and sign-in, and the worker loop in one process | Development and non-HA deployments |
| `api` | Inference, admin API and sign-in | Production, scaled apart from the worker |
| `worker` | Background jobs only | Production; run at least one |
| `data-plane` | Inference, `/healthz` and `/readyz` only, connected as a restricted database role | Serving inference in further regions |

API-serving processes are stateless: load balancers may send any request to any instance. A process that cannot reach the PostgreSQL primary refuses inference with `503` rather than serve unmetered requests. See [deployment and operations](../operations/index.md) for health checks, shutdown and migrations.

## Deployment shapes

### Single process

One `all-in-one` process and one PostgreSQL primary. The repository's Docker Compose `app` profile runs this shape, with a migration job, the admin console image and a reverse proxy in front. It is suitable for development, evaluation and small non-HA installations.

### Split API and worker

Several stateless `api` instances behind a load balancer, one or more `worker` replicas, and one PostgreSQL primary. The API and worker scale and fail independently: if the worker stops, inference continues, while recovery, event delivery, exports, discovery and retention wait until it is back.

### Regional data planes

`data-plane` instances placed near providers or tenants, while `api` and `worker` stay with the primary database. A data plane has no admin API, no sign-in and no worker loops, never runs migrations, and connects as the `gateway_data_plane` role, which the operator creates. That role can read identities, keys, budgets, provider credentials and configuration versions, and write reservations, usage, traces, outbox events and its own instance row. It cannot read sessions, audit events, invitations, webhooks or the catalog. Reservations and settlements still go to the central primary, so a data plane pays a round trip to it on every request and no budget is spent twice. See [the catalog page](catalog.md#configuration-versions) for how data planes route, and [security](security.md#data-plane-database-role).

### Kubernetes

The Helm chart `oci://ghcr.io/qucelerate/charts/ai-gateway` deploys the API, worker and data-plane workloads, the admin console and, optionally, this documentation site on a host of its own. The images are:

| Image | Contents |
| --- | --- |
| `ghcr.io/qucelerate/ai-gateway` | The `gateway` binary (every profile, and the operator commands such as `gateway migrate`) and the `gwctl` CLI |
| `ghcr.io/qucelerate/ai-gateway-admin` | The admin console's static build, served by unprivileged nginx with the client-side routing fallback |
| `ghcr.io/qucelerate/ai-gateway-docs` | This documentation site |

Images are tagged with their commit. Run `gateway migrate` once per release before rolling the workloads; the gateway never migrates on start.

## In this section

- [Request lifecycle](request-lifecycle.md): what happens to one inference request, from authentication to the outbox event.
- [Accounting](accounting.md): money units, reservations, settlement, expiry, reconciliation and leases.
- [Budgets and rate limits](budgets-and-rate-limits.md): every level a request is checked at, and the `429` problems that say which one refused it.
- [Providers and adapters](providers.md): provider families, capabilities, discovery, model availability, circuits and failover.
- [Catalog and releases](catalog.md): manifests, prices and their sources, activation, configuration versions and pins.
- [Security model](security.md): credentials, roles, tenant isolation and encryption.
- [Data and retention](data-and-retention.md): request records, traces, content capture, retention, exports and deletion.
- [Scaling](scaling.md): connection pools, leases, PgBouncer, horizontal scaling and measured limits.

The gateway is open-source software under the MIT license. There is no commercial offering or support contract.
