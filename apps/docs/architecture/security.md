---
description: "API keys and peppered hashes, agent tokens, OIDC and local accounts, roles, tenant isolation, provider-credential encryption, the egress guard and the data-plane database role."
---

# Security model

The gateway separates three kinds of caller and gives each its own credential: applications call inference with API keys, people administer through browser sessions from OIDC or local accounts, and automation administers with agent tokens. Bearer secrets are stored only as digests, provider credentials are sealed with AES-256-GCM under an environment key or an AWS KMS envelope, every tenant-owned row carries its organization, and every admin action is checked against a role and audited. This page describes the design; [security and privacy](../security/index.md) covers safe handling and how to report a vulnerability.

## Credentials at a glance

| Credential | Format | Used on | Stored as |
| --- | --- | --- | --- |
| API key | `gw_` and 64 hex characters | Inference (`/api/inference/v1/*`) only | HMAC-SHA256 digest under `API_KEY_PEPPER`, and a non-secret prefix |
| Agent token | `gwa_…` | Admin API and MCP (`/api/admin/v1/*`, `/api/mcp`) only | SHA-256 digest, and a 12-character prefix |
| Browser session | `__Host-` cookies | Admin API and console | Digests of the session token and of a browser-binding cookie |
| Provider credential | API key, role, directory or service account | Calls to the provider | Sealed with AES-256-GCM; write-only through the API |

API keys never reach admin routes, and agent tokens and sessions never authorize inference. Even the console's playground calls inference with an API key, never with the admin session.

## API keys

An API key is a bearer credential for a **service account**, the non-human identity an application runs as. It is 32 random bytes, shown once when it is created or rotated. The gateway stores only an HMAC-SHA256 digest of it, keyed with the server-held `API_KEY_PEPPER`, and compares digests in constant time; a database copy alone cannot be used to authenticate. Without the same pepper, every existing key fails, so the pepper is backed up with the database ([backup and recovery](../operations/recovery.md)).

- **Scopes.** `inference` (chat), `embeddings`, `catalog-read` and `admin-read`. Each inference operation checks the scope it needs; admin mutations are never granted to API keys.
- **Lifecycle.** Keys are listed by prefix, expire, record when they were last used, and are revoked or rotated; rotation issues the new key before the old one is revoked. Revocation is read live, so it takes effect on the next request in every region.
- **Limits.** A key can have its own budget and rate-limit policy, beside its service account's, team's and organization's; see [budgets and rate limits](budgets-and-rate-limits.md).

Keys belong in a secret store and are sent only over HTTPS. Never put one in a URL, a browser bundle or a log.

## Agents and agent tokens

Automation such as CI, coding agents, the `gwctl` CLI and MCP clients administers through **agents**, not through people's sessions or API keys.

- An agent is a user account of kind `agent`. It never signs in, and is a member of exactly one organization with an ordinary role, so permission checks, the role ceiling and the audit trail treat it exactly as they treat a person.
- It authenticates with an agent token (`Authorization: Bearer gwa_…`). Only a SHA-256 digest and a 12-character public prefix are stored. Tokens expire after 1 to 365 days, or never; an agent holds at most 20 live tokens, and an organization at most 200 agents. Disabling an agent revokes its tokens.
- A token is confined to its agent's organization: other organizations' routes answer `404`, and routes outside an organization answer `403`, apart from the session-shaped ones (`/me`, `/organizations`).
- A token can be limited further, to one service account's end users, so an app's backend manages only its own users' plans and credits.
- Owners and admins create agents and tokens in the console (*Identities → Agents*) or the admin API, never with a role above their own. The operator creates an organization's first agent on the server, which lets a deployment be automated end to end without anyone signing in:

```bash
gateway agent create --organization <slug> --name ci --role owner --expires-in-days 30
```

Agent tokens drive the same admin services as the console, through `gwctl` (a CLI over the admin and inference APIs) and the MCP server (`/api/mcp` over streamable HTTP, or `gwctl mcp` over stdio). Permissions, idempotency and audit apply unchanged, and secrets are returned only by the tools that create them. See [installing for agents](../install/agents.md).

## People: OIDC and local accounts

People sign in to the admin console in one of three ways:

- **The deployment's OIDC provider** (`OIDC_ISSUER`, `OIDC_CLIENT_ID`, …). The callback is single-use and checks state, nonce, PKCE, issuer, audience, signature and expiry; only RS256 and ES256 ID tokens are accepted. A person is identified by the stable `(issuer, subject)` pair, never by email, so accounts are never linked by email address.
- **An organization's own OIDC provider.** An owner configures it and proves it works with a test sign-in before it takes effect, so a wrong secret cannot lock people out. People reach it by entering an email address whose domain the organization has verified with a DNS TXT record. A session from an organization's provider can act only in that organization.
- **Local accounts**: an email address and an Argon2id password hash, so the gateway can be tried without an identity provider. They are on wherever the API has `GATEWAY_ORIGIN` and the two session secrets, unless `GATEWAY_LOCAL_ACCOUNTS=false` turns them off. Passwords must be 12 to 256 characters, not among the 10,000 most common and not containing the email address. Failed attempts back off per account and are limited per client address. An owner can require the organization's provider instead (`403 sso_required` for password sessions).

On first start, while no organization exists, the gateway logs a single-use setup link that creates the first organization, its owner and the owner's session.

Browser sessions are `__Host-`, `Secure`, `HttpOnly`, `SameSite=Lax` cookies, and a browser-binding cookie rotates at sign-in. Every admin write with a session cookie is refused if it is cross-site or from an origin other than `GATEWAY_ORIGIN`, and every admin write takes an `Idempotency-Key`. The console must therefore be served from the same origin as the admin API. Admin responses are `Cache-Control: no-store`, and every response carries `nosniff`, anti-framing and a restrictive content security policy.

## Roles and permissions

An organization member has one of these roles, organization-wide or for a team:

| Role | Typical use |
| --- | --- |
| Owner | Everything in the organization, including its identity provider |
| Admin | Day-to-day administration: members, keys, providers, catalog, policies, tracing |
| Team admin | A team's own service accounts, keys and usage |
| Operator | Creating, rotating and revoking API keys, managing end users, and reading usage, traces, reconciliation and policies |
| Auditor | Reading the organization, including members, usage, traces and audit events |
| Read-only member | Reading a narrower set |

Permissions are checked per endpoint and per resource on the server; the console only hides what a role cannot do. Read and write permissions are separate for the catalog, credentials, keys, budgets, rate limits, reports, traces, exports and audit. Reading a trace's **content** is a permission apart from reading request records (`trace_content_read`, owners and admins), and every content read is audited. Nobody can grant a role above their own, and team-scoped members cannot widen a request to the whole organization.

Above organizations, **deployment administrators** (the setup account, and anyone granted with `gateway admin grant`) create, suspend, delete and restore organizations and read the deployment's own audit log. They see nothing inside an organization they are not a member of.

Every admin access and every mutation is an audit event with actor, scope, action, target, outcome and request id. Keys, credentials, tokens, prompts and completions are never in it. Audit records are append-only: only a retention purge deletes them.

## Tenant isolation

The organization is the security boundary:

- Every tenant-owned row carries its organization's id, and every query that takes an id from a request is scoped by organization. Cross-tenant reads and writes are denied, and tests cover each admin lane.
- One check in the admin router keeps every session and agent token inside its organization, so a new endpoint cannot forget it. Another organization's resources answer `404`, which reveals nothing about whether they exist.
- Inference identity comes only from the API key: its service account, team and organization are read from the database, never from the request.
- Circuits, availability evidence and provider credentials are per organization, so one tenant's failures or credentials never affect another.
- A suspended organization's keys, agent tokens, sessions and identity provider are refused at once, and its background work waits.

## Provider credentials

Provider credentials, webhook signing secrets and organizations' identity-provider client secrets are sealed with AES-256-GCM, bound to the row they belong to. They are write-only through the API: no response, log, audit event, error or trace contains them.

- **Environment key.** By default the key is `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`: 32 bytes, base64url without padding. Without it, writes that need it answer `503 credential_encryption_unavailable`.
- **AWS KMS envelope.** With `GATEWAY_CREDENTIAL_KMS_KEY_ID`, secrets are sealed under a data key that AWS KMS generates, and the database holds only the KMS-wrapped data key, so a database copy alone never yields a secret. Each KMS call binds the data key with an encryption context. A process that cannot unwrap the active data key refuses to start. Moving to KMS, or rotating the KMS key, re-seals existing secrets automatically in the background, and each re-seal is audited.
- **Cloud identities instead of keys.** Where the operator configures the gateway's own AWS, Azure or Google identity, tenants connect a role, directory or service account rather than a long-lived key; see [provider credentials](providers.md#provider-credentials).

Removing a credential stops the gateway using it; it does not revoke anything at the provider. Revoke or rotate it at the provider as well.

## Outbound requests

Requests to tenant-supplied hosts (webhooks, organizations' OIDC providers, operator price sheets, OpenAI-compatible endpoints) go through an egress guard: HTTPS only, every resolved address must be public (loopback, private, link-local, metadata and translated IPv6 forms are refused), the connection is pinned to the checked addresses, and redirects and proxies are not followed. The operator can allow specific private origins for OpenAI-compatible model servers.

## Data plane database role

A `data-plane` process connects as `gateway_data_plane`, a least-privilege PostgreSQL role the operator creates; every `gateway migrate` grants it exactly what a data plane needs and revokes anything else.

| It can | It cannot |
| --- | --- |
| Read identities, keys, budgets, provider credentials and configuration versions | Read sessions, invitations, audit events, webhooks or the catalog |
| Write reservations, usage, traces, outbox events, a key's last-used time and its own instance row | Write audit events or catalog changes, or run migrations |

Rate-limit admission and reservation provenance checks run as functions owned by the application role, so a data plane cannot bypass them. A data plane still needs the credential key (or KMS access) to open provider credentials, and `API_KEY_PEPPER` to check keys.
