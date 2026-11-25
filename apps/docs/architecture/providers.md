---
description: "Provider families, the adapter contract, credentials, capabilities, discovery and automatic offers, availability evidence, circuits, failover and batches."
---

# Providers and adapters

The gateway calls models on provider accounts that each organization connects, through one adapter per provider family. Adapters translate the gateway's normalized chat, streaming, embeddings and batch contract into each provider's own API and back, preserving provider request ids, finish reasons and token counts. The catalog records what each deployment can do, discovery and availability evidence record what each account can actually call, and circuits and failover keep a failing deployment from taking requests down with it. For which models to call and how, see [providers and capabilities](../guide/providers.md).

## Provider families

| Family | Catalog name | Authenticates with | Notes |
| --- | --- | --- | --- |
| OpenAI | `openai` | API key | Reference adapter for chat, streaming, embeddings and native batches |
| Anthropic | `anthropic` | API key | Messages API; tools, prompt-cache reads and writes, Message Batches |
| Gemini | `gemini` | Gemini API key, or the gateway's Google identity on Agent Platform | Gemini API and Agent Platform (formerly Vertex AI); embeddings and batch prediction on Agent Platform |
| Amazon Bedrock | `bedrock` | Static keys, the gateway's runtime role, or a tenant role it assumes with an external id | Region and model id (or inference profile) are the deployment's identity; `Converse` for chat |
| Azure OpenAI | `azure-openai` | API key or Entra identity | Resource, API version, region and deployment name are deployment metadata |
| Azure AI Foundry | `azure-ai-foundry` | API key or Entra identity | A separate family from Azure OpenAI, with its own endpoint, auth, capability and price contract |
| OpenAI-compatible | `openai-compatible` | API key or configured headers | Any endpoint that speaks the OpenAI API, such as a self-hosted model server |

Azure OpenAI and Azure AI Foundry stay distinct in the catalog and in reports: the gateway does not assume that Azure OpenAI deployment names, API versions, usage fields or prices apply to Foundry.

## The adapter contract

Every adapter implements the same interface: count chat input tokens (where the provider offers it), chat, chat stream, embeddings, list models, list deployable models, probe a model, and check access. The rules every adapter follows:

- **Normalized results, provider facts kept.** Callers see one response shape; the provider's request id, finish reason, refusal and safety details and every token count it reports are carried through. Usage keeps cached-input, reasoning and other tokens apart from input and output.
- **Unsupported is an error, not a guess.** A request option an adapter cannot map is refused, never silently dropped. Function tools in the OpenAI shape are mapped to each provider's own tool format; an option a provider cannot honour (such as `parallel_tool_calls: false` where it is not supported) is refused with `tools_unsupported`.
- **One call, one attempt.** One adapter call is one logical provider attempt. Adapters mark an error whose request never left the gateway, which is what makes a gateway retry or failover safe.
- **Streams end once.** A stream emits at most one terminal completion or error. Dropping the stream cancels the provider request.
- **No accounting.** Adapters never reserve, settle or bill; the [accounting](accounting.md) layer wraps them.

Provider calls use HTTP/2 where the provider offers it, with connect and per-read timeouts. Credentials are referenced by id in configuration and are kept out of the catalog, traces, errors and audit payloads.

## Provider credentials

An organization connects its own provider accounts. Depending on the family, the gateway holds:

- **An API key** (OpenAI, Anthropic, Gemini API, Azure, OpenAI-compatible), stored [encrypted](security.md#provider-credentials).
- **A cloud identity without a long-lived key**, where the operator has configured the gateway's own identity for that cloud:
  - AWS: the gateway's role (static keys, IRSA, container credentials or instance metadata) assumes the tenant's role with an external id that the gateway assigns per organization.
  - Azure: the gateway's Entra identity (client secret, workload identity or managed identity) signs in to the tenant's directory as a multi-tenant app, or as a tenant app that trusts it through a federated credential.
  - Google: the gateway's Application Default Credentials (including workload identity federation) impersonate a tenant service account on Agent Platform.

Static keys are a compatibility fallback, not the preferred production mode. The operator binds which cloud principals each organization's credentials may act as, so one tenant cannot point the gateway's identity at another tenant's resources.

An OpenAI-compatible endpoint a tenant connects must be HTTPS on public addresses. The gateway resolves it, checks every address, and pins the connection to them, so it cannot be used to reach internal services. The operator can allow specific private or plain-HTTP origins, such as a model server inside the cluster, with `GATEWAY_OPENAI_COMPATIBLE_PRIVATE_ENDPOINTS`.

## Capabilities

The catalog records capabilities per deployment and operation, never per model alone:

| Operation | Endpoint |
| --- | --- |
| `chat` | Chat completions, unary and streamed |
| `embeddings` | Embeddings |
| `chat_batch`, `embeddings_batch` | Lines of a batch, at the deployment's batch price |

A `chat` capability carries the limits a request is checked against before anything is reserved: the maximum input bytes, maximum output tokens, the tokens-per-byte that bounds input, the input count mode (`provider_api_exact` where the provider counts tokens, otherwise `unsupported`), the reservation time-to-live, whether it takes tools, and any reasoning allowance. Streaming is a delivery mode of `chat`, with the same route, price and limits. Embedding metadata never implies chat capability. Routing selects only deployments whose recorded capability satisfies the request, and fails closed when it is unknown.

## Discovery

The worker lists each connected account's models on a schedule, with read-only calls:

- **Azure:** the resource's deployment list from Azure Resource Manager (exact deployments with model, version, SKU, capacity and state) and the region's deployable model catalog, with retirement dates.
- **AWS:** Bedrock's foundation models and inference profiles, the account's own provisioned throughput, application profiles and custom model deployments, and each model's availability (authorization, entitlement, agreement, region).
- **Google:** the publisher model catalog per location, Model Garden partner models with their per-model quota, and the project's own endpoints.
- **OpenAI, Anthropic, OpenAI-compatible:** the provider's model list.

Discovery records a model's operations only where the listing or the model family makes them certain; anything else is left `unknown` pending verification. A model listed only through an inference profile is recorded as `profile_only` and is never offered under its own id.

### Automatic offers

An organization can opt a provider account in to automatic offers. After each successful listing, the worker plans the next catalog release from the active one: every listed model the adapter can call, that a price source prices, and whose availability evidence is `verified` or `entitled`, becomes a deployment with a route whose alias is the model id. A model without a price is recorded as needing one, never guessed at. Offered deployments are withdrawn when the listing drops the model; deployments an admin added are left alone. Each change is a new release, created and activated in one audited transaction. The gateway never creates deployments in a tenant's cloud account.

## Model availability

A provider's listing is not proof that an account can call a model. Listings overstate (a model the account is denied, or that is retired for it) and understate (deployments an API does not list). The gateway therefore keeps availability **evidence** per account, model and location, from read-only permission and entitlement checks, small probes, and live traffic:

| Evidence | Means |
| --- | --- |
| `verified` | A call to the model succeeded. |
| `entitled` | Read-only checks show the account may call it. |
| `requires_action` | Something must be done first, such as accepting an agreement, enabling it in the provider's console, or getting quota. |
| `refused` | The provider refused a call: denied, not found, or retired. |

A live access error marks a deployment `refused` at once. Routing skips `refused` and `requires_action` deployments, automatic offers withhold them, and the console shows each account's *Deployed* and *Deployable* models with the evidence, the last check and the action that unblocks a model. Probes that the provider bills (one-token calls) are made only for deployed models; Gemini's are free token counts.

## Circuits and health

Circuits are kept per organization, adapter instance and deployment, so one tenant's bad credential or exhausted quota never trips another's. After 5 consecutive failures that indicate an unhealthy provider (unavailable, timeout, throttled, provider or protocol errors), the circuit opens: requests to that deployment fail fast with `503 provider_circuit_open` and `Retry-After`, before anything is reserved. When the open period (30 seconds at first) ends, one probe request is let through; success closes the circuit, failure reopens it for twice as long, up to 5 minutes. A rejected request, a bad credential or a cancelled call never trips a circuit.

Circuit state is per process: each instance learns a deployment's health from its own traffic. The gateway logs a warning when a circuit opens and an info line when it closes. The console shows each deployment's health over the last 15 minutes, computed from usage.

## Failover

A route can name up to 8 deployments, each with a weight and a priority, for example the same model in two regions, or a self-hosted server with a cloud deployment behind it at a lower priority. The gateway picks by weight among the healthy deployments of the lowest priority that has any.

A request fails over to the next healthy deployment only when the provider cannot have started the work: the connection was refused, DNS or TLS failed, the circuit was open, or the provider answered `429`. A `5xx` is not proof that no work was done, so it does not fail over. Nor does a timeout after the request was sent, or anything after a response or stream has started. One request makes at most 3 attempts within its first reservation's deadline. Each attempt has its own reservation and request record, linked to the one it replaces with the outcome `failed-over`, and `gateway_route_failovers_total` counts them. Because the first reservation is made at the route's highest price, a failover never exceeds what was reserved; settlement uses the serving deployment's price.

Every deployment of a route serves the same contract: one currency, the tightest of their limits, tools only if every deployment takes them, and the largest reasoning allowance. A manifest whose deployments cannot be reconciled is refused. See [catalog and releases](catalog.md#routes).

## Batches

Batches of chat or embeddings lines go through one OpenAI-shaped API (`/api/inference/v1/files` and `/api/inference/v1/batches`) for every family. The whole input is validated, routed per line by alias and reserved in full before the batch is accepted, and each line is settled on its own. Families with native batch interfaces (OpenAI, Azure OpenAI batch deployments, Anthropic Message Batches, Bedrock batch inference, Gemini batch prediction on Agent Platform) run at their batch price; Bedrock and Gemini stage files through a bucket in the tenant's own cloud. Other lines can run emulated, each sent once as an ordinary request at the on-demand price within the organization's rate limits, when the organization allows it. The worker submits, polls, collects and settles native jobs and is safe to restart at any point.
