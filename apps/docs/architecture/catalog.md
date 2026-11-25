---
description: "Catalog manifests, routes, releases and activation, price sources, price adoption and overrides, configuration versions, the feed and pins."
---

# Catalog and releases

Each organization's catalog says which provider accounts, deployments, prices and routes it serves. The catalog is a sequence of immutable, versioned **releases**, each a manifest addressed by its SHA-256 digest, and exactly one release is active. Activating a release publishes a **configuration version**: the snapshot every API-serving process routes from. Prices carry their source and confidence, follow their sources automatically unless an organization turns that off, and can be overridden for a window; an organization can pin every process to an earlier configuration version to roll back a bad change.

## Manifests

A manifest is a JSON document with five parts:

| Part | Holds |
| --- | --- |
| `release` | The release's own `version` label |
| `providers` | Adapter instances: an `adapter_instance_key` (the provider account) and its `family` |
| `deployments` | A deployment `name`, the provider `model` id, its `location`, the adapter instance, and one capability per operation with its limits |
| `prices` | Per deployment and operation: per-token prices in [money units](accounting.md#money-units) for input, output, cached input, reasoning and other tokens, the `currency`, the `source` and `source_version`, and the `confidence` |
| `routes` | Aliases: an `alias` and `operation` routed to a deployment, with an optional `weight` and `priority` |

An abbreviated example:

```jsonc
{
  "release": { "version": "2026-01" },
  "providers": [
    { "adapter_instance_key": "openai-main", "family": "openai" }
  ],
  "deployments": [
    {
      "name": "openai-chat",
      "model": "gpt-4o-mini",
      "location": null,
      "adapter_instance_key": "openai-main",
      "capabilities": [
        {
          "operation": "chat",
          "max_input_bytes": 131072,
          "max_output_tokens": 4096,
          "max_tokens_per_byte": 1,
          "tokenizer": "configured",
          "reservation_ttl_seconds": 300,
          "chat_input_count_mode": "unsupported"
        }
      ]
    }
  ],
  "prices": [
    {
      "deployment": "openai-chat",
      "operation": "chat",
      "input_minor_per_token": 15,
      "output_minor_per_token": 60,
      "cached_input_minor_per_token": 8,
      "reasoning_minor_per_token": 0,
      "other_minor_per_token": 0,
      "currency": "USD",
      "source": "operator",
      "source_version": "2026-01",
      "confidence": "known"
    }
  ],
  "routes": [
    { "alias": "chat-default", "operation": "chat", "deployment": "openai-chat" }
  ]
}
```

The manifest schema is closed (an unknown field is an error), limited to 1 MiB and 256 entries, and validated as a whole: every route must name a deployment whose family supports the operation, and every deployment of one route must be reconcilable into one contract (below). A manifest that fails is refused and changes nothing.

### Routes

An alias routes one operation to between one and 8 deployments. Each has a `weight` (1 to 1,000, default 1) and a `priority` (1 to 10, default 1; lower is preferred). A request goes to the lowest priority that has a healthy deployment, split by weight within it. Every deployment of a route must serve one contract:

- every price in one currency, since amounts are never converted;
- the tightest of the deployments' limits (input bytes, output tokens), so a request never fits one deployment and not another;
- tools only if every deployment takes them;
- the largest reasoning allowance among them, since no cap controls reasoning past `max_tokens`.

Streaming has no route of its own: a streamed request takes the alias's `chat` route.

## Releases and activation

Every change to the catalog, whether made in the console, through the admin API, by automatic offers or price adoption, or with the operator CLI, is a new release. Nothing is edited in place: a release, once stored, never changes, and its digest is the SHA-256 of its manifest. Applying a manifest whose digest matches a stored release reuses that release.

Activation makes a release the organization's active one. The activation pointer has its own version, so an admin's activation is conditional on the version they read (`If-Match`): a release that automatic price adoption or offers activated meanwhile is never silently undone. Any stored release can be activated again, which is the ordinary way to roll the catalog back, and any release can be read back as its manifest.

Operators apply a manifest from the server, which stores and activates it:

```bash
gateway catalog apply --organization <organization id> --file catalog.json
```

The repository's `examples/catalog-manifest.json` is a complete example.

## Prices

Pricing is separate from model discovery. A model can be known before its price is, and a price varies by deployment, region, operation, token type and currency, so it is never a property of a model alone. Each price records where it came from (`source`, `source_version`) and its `confidence`: `known` for a published or contracted price, `conservative` for a deliberately high stand-in.

### Price sources

The worker fetches each price source daily and keeps what it publishes as a versioned snapshot:

| Source | Covers |
| --- | --- |
| `azure-retail:azure-openai` | The Azure Retail Prices API, for Azure OpenAI, in the currencies the operator configures (USD by default) |
| `aws-price-list:bedrock` | The AWS Price List for Bedrock, including partner foundation models |
| `price-sheet:{family}` | An operator-hosted price sheet per provider family, at the HTTPS URL in `GATEWAY_PRICE_SHEET_URL_{FAMILY}` (the URL may carry a token, so it is never stored, logged or returned) |

On-demand and batch token prices are kept; batch prices price the batch operations. Public price lists price *products* (a meter, a usage type), not models, so the gateway never guesses which deployment a product prices: an admin maps a deployment to a published item, and drift and adoption compare against the mapped item, or against a price published for that exact model. Providers that publish no price list the gateway reads, such as OpenAI and Anthropic, are priced by hand in the catalog.

**Price drift** compares the billed price of every active deployment, override included, with each source, in the same currency only.

### Automatic price adoption

On by default per organization. After a source is fetched, the worker plans a release from the active one in which every price that differs from its source is updated, and creates and activates it in one audited transaction (`catalog_release.prices_adopted`, listing each old price, new price, source and snapshot). Only the dimensions that differ change; a published price finer than one money unit is rounded up. Prices billed through an override, and prices no source publishes, are left alone.

A change that looks wrong is held for an admin to adopt or dismiss instead, raised as a notification:

- a dimension more than doubles, or falls below half;
- a billed dimension becomes free;
- two sources publish different prices for one deployment.

With adoption off, every differing price waits for an admin under *Price changes*.

### Price overrides

An organization can override the catalog price of one deployment and operation for a validity window, with a reason, recorded with the actor who made it. Overrides are matched by deployment name, so they survive catalog upgrades; they are immutable except for a one-time revocation; and the routing snapshot carries their windows, so one that starts or ends needs no new release.

### Prices and accounting

A new price bills from the activation of its release, or from the start of its override's window. Every reservation snapshots the price it was made at and settles at that price, so a reservation already made is never repriced, and settled usage is never recalculated from today's price. See [accounting](accounting.md#reservations-at-the-upper-bound).

## Configuration versions

Routing does not read the catalog on each request. Each organization has a sequence of **configuration versions**: immutable documents, addressed by a SHA-256 digest, that hold its active release's routes (alias, operation, deployments with weights and priorities, adapter instance, capability limits, prices), its routing policy, and its price overrides with their windows. The control plane publishes a new version in the same transaction as each change to those inputs: a release activation, an override created or revoked, and a routing policy change.

What is deliberately **not** versioned is anything that grants or limits access: API keys, service-account and membership state, provider credentials, budgets, rate and concurrency limits, and circuit state. Those are read live on every request, so revoking a key, deleting a credential or lowering a cap takes effect at once in every region, and rolling routing back can never reverse it.

### The feed

Every published version and every pin change takes the next position in one deployment-wide feed. Publishers serialize on a transaction-level advisory lock, so feed order is commit order.

```text
  control plane                  PostgreSQL                    every API-serving process
  ─────────────                  ──────────                    ─────────────────────────
  activate release ──► same transaction: new version ──┐
  create override  ──►          + feed entry n         │
                                                       ├──► poll every 5 s (GATEWAY_CONFIG_POLL_SECONDS)
  pin / unpin      ──►          feed entry n+1   ──────┘    check digest, swap atomically
                                                            record applied position in instance row
```

- At start, a process (`all-in-one`, `api` or `data-plane`) loads the version each organization serves, then polls the feed every `GATEWAY_CONFIG_POLL_SECONDS` (5 by default). It checks each document's digest before swapping it in atomically. A request for an alias the process does not know makes it read the feed at once, at most once a second, so a just-activated alias is served immediately.
- Each feed entry has a random id. On every poll the process checks that the entry at its applied position still has the id it applied; after a restore or a failover that lost entries, positions are reused, so a mismatch makes the process reload everything.
- When a poll fails, the process keeps serving what it has loaded. `/readyz` answers `503` until the first load completes, and again whenever the feed has not been read for three polls, so load balancers move traffic away from a process cut off from the primary.
- Each process records an instance row on every poll: its id (`GATEWAY_INSTANCE_ID`), profile, region (`GATEWAY_REGION`), build, applied position and when it was last seen. *Settings → Routing* shows, per organization, the version each instance serves, which instances have not caught up, and which have not been seen for five minutes. The metrics `gateway_config_applied_sequence` and `gateway_config_age_seconds` carry no tenant labels.

### Pins

An owner or admin can **pin** the organization to an earlier configuration version (an audited write with `If-Match`, in *Settings → Routing* or `PUT …/config-versions/pin`). Every instance serves the pinned version from its next poll until the organization is unpinned. Changes made while pinned are still published and marked as not served; fix forward with a new release, then unpin. A pin never undoes a key revocation, a credential deletion or a budget change. Operators have the same controls:

```bash
gateway config list  --organization <organization id>
gateway config pin   --organization <organization id> --version <version id>
gateway config unpin --organization <organization id>
```

A pin made with the CLI has no signed-in actor, so it is not in the organization's audit log, but it changes the pin's version, so an admin client holding the earlier `ETag` gets a conflict rather than undoing it.

Versions are kept for 90 days, except that each organization's newest and pinned versions are always kept. Instance rows unseen for seven days are deleted.
