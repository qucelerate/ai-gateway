# Rollback and incidents

Start every incident from what callers see: the status code, the problem `code`, and the `X-Request-Id`. Then check `/readyz` on the affected instances and the [reliability signals](/operations/signals.md). The repository's operations guide lists suggested alerts on the gateway's Prometheus metrics.

## A configuration change routes or prices badly

Every change to an organization's routes or price overrides publishes a new configuration version, and every instance applies it within one poll, five seconds by default. To roll back, an owner or admin pins the organization to the last good version under **Settings → Routing** in the admin UI. Operators can do the same from the command line, where `gateway config list` shows the versions, newest first:

```sh
gateway config list  --organization <org id>
gateway config pin   --organization <org id> --version <version id>
gateway config unpin --organization <org id>
```

Every instance serves the pinned version from its next poll. Changes made while pinned are published but not served, so fix forward with a new catalog release, then unpin.

A pin covers routing and prices only. It never brings back a revoked key or a deleted credential, and never undoes a budget or rate-limit change: those are read live on every request.

## A release misbehaves

Roll the workloads back to the previous image only if the release's change description says its migrations are compatible with that image. Migrations are forward-only, so otherwise [restore](/operations/recovery.md) the database from a backup taken before the upgrade, then deploy the previous image. Take a backup before every upgrade for this reason.

## A provider fails

When a provider deployment keeps failing, its circuit opens on each instance: callers get `503` with code `provider_circuit_open` and a `Retry-After` header, and the circuit probes for recovery by itself. There is no automatic failover between deployments. To move traffic, activate a catalog release that routes the alias to a healthy deployment, or pin to a version that did.

Provider calls that fail as `authentication` or `authorization` (the `outcome` of `gateway_provider_calls_total`) usually mean an expired or revoked provider credential, or a role the gateway's identity has lost; they do not open a circuit by themselves.

## A credential is exposed

- **An API key.** Revoke it under **Identities** in the admin UI. Revocation applies to the next request on every instance. To replace it without an outage, first **Rotate** the key: rotation issues a successor and leaves the old key working until you revoke it. Then review the usage report for the key's service account.
- **A provider credential.** Rotate it with the provider, then save the new value on the provider account in the admin UI. The change applies to the next request. Where the provider supports it, prefer a keyless workload identity, which leaves no stored secret to expose.
- **`API_KEY_PEPPER` or the credential encryption key, with a copy of the database.** Treat every API key or stored provider credential as exposed: issue new keys, revoke the old ones, and rotate the provider credentials.

## Spending looks wrong

Budgets bound what the gateway calculates, not what a provider invoices. Check open reconciliation items under **Usage & traces → Reconciliation**, and compare usage reports with the provider's invoice. See [budgets and their limits](/operations/budgets.md).

> **Scoped:** this page is a starting point, not an on-call runbook. Escalation, paging, and customer communication belong to whoever operates the deployment.
