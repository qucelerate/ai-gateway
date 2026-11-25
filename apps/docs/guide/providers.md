# Providers & capabilities

The gateway presents a capability-oriented contract rather than exposing provider internals. Choose a model available to your deployment, such as `chat-default` or `embedding-default` in the examples.

## Capability map

| Capability | Endpoint | Output |
| --- | --- | --- |
| Chat | `POST /api/inference/v1/chat/completions` | One JSON response |
| Chat stream | `POST /api/inference/v1/chat/completions/stream` | Non-resumable SSE |
| Embeddings | `POST /api/inference/v1/embeddings` | Vector list |
| Tool calling | Chat and chat stream, with `tools` | Tool calls for your code to run ([guide](/quickstarts/tools.md)) |

Whether a model takes tools depends on its deployments: the catalog marks each chat deployment, and a request with tools to a model that cannot take them is refused with `tools_unsupported` rather than answered without them.

## Where model names come from

The `model` you send is an alias in your organization's catalog, which routes it to a deployment on one of the organization's provider accounts. Model availability and routing are deployment concerns, so do not assume a model name is available everywhere; handle a documented error response.

An organization's owners and admins connect its provider accounts in the admin console under **Connections → Connect a provider**: an Anthropic or OpenAI account with an API key (stored encrypted; a key used only by the gateway, with a spend limit, is recommended), an AWS, Azure or Google Cloud account without a long-lived key where the gateway's operator has set that cloud up, or any endpoint that speaks the OpenAI API. The gateway checks the connection before anything is saved, then adds the models chosen, with their aliases and prices, to the catalog. Anthropic and OpenAI publish no price list the gateway reads, so their prices are entered there. For Claude that includes the cache read (cached input) and cache write prices: requests that read or write Anthropic's prompt cache are charged at them, with the cache write price following 1.25 times input (Anthropic's five-minute write) until you change it ([Anthropic, prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)).

An organization can let the gateway discover the models a provider account can call and offer them by themselves. The gateway lists each account's models with read-only checks, and offers a model under its own name only once a price source prices it. It never creates deployments in your cloud account. A model that the checks show the account cannot call yet, for example because it needs an agreement accepted or has no quota, is shown to admins but not offered.

## What an account can call

For each provider account the gateway keeps evidence of what it can call, from read-only permission and entitlement checks, small probes, and live traffic:

| Evidence | Means |
| --- | --- |
| `verified` | A call to the model succeeded. |
| `entitled` | The provider's own checks say the account may call it. |
| `requires_action` | The account must act first: grant a permission, accept an agreement, get quota, enable the model in Google's Model Garden, or have an organization policy allow the model. |
| `refused` | The provider refused calls to it, for example because it is not served in the account's location. |

Only models that are not blocked are offered automatically. The admin UI's *Models → Availability* shows each account's *Deployed* and *Deployable* models. *Deployed* is not a promise that a model works: on Azure it lists every deployment, including ones whose evidence is `requires_action` or `refused`, and on other providers it lists models no check or call has blocked, some not checked yet. On Google it also lists the project's own endpoints (models deployed to dedicated machines), with what each serves: one serving a Gemini model, or an open model whose Model Garden container answers OpenAI chat completions, can be added as a deployment `endpoints/<id>` and is priced at your charge-back rate; the gateway does not call other endpoints, so they are never offered. Model Garden partner models (Anthropic's Claude, Mistral, xAI and other open-model publishers) are listed as `<publisher>/<model>` and can be added as deployments on an account that signs in with a Google identity; each row says which API the gateway calls them through. Read each row's evidence. *Deployable* lists what the account could use once the tenant acts. A blocked row says what to do, with a link to the provider's console; the gateway itself never makes those changes.

## Model access on your cloud account

The gateway can only offer models your cloud account may call. On every cloud some models need you to ask the provider first, and no permission you grant the gateway replaces that:

- **AWS:** third-party models on Amazon Bedrock need an AWS Marketplace agreement, and some need account-level access granted by AWS (your account team or a support case); quotas of new accounts may be low and are raised on request ([Bedrock model access](https://docs.aws.amazon.com/bedrock/latest/userguide/model-access.html), [Bedrock quotas](https://docs.aws.amazon.com/bedrock/latest/userguide/quotas.html)). If you let the gateway's role accept model agreements when connecting AWS, admins can accept them under *Models → Availability → Agreements*; each is a subscription billed per use. With that permission, a model's first ordinary call can also create the subscription, since Bedrock subscribes on first use when the caller may; the gateway never offers such a model by itself before its agreement is available. Agreements are only for an account connected with your own role, not one the gateway calls with its operator's credential.
- **Azure:** Limited Access models need a registration Microsoft approves, Claude needs an Azure Marketplace purchase on a paid subscription, and quota is per subscription and region ([Limited Access](https://learn.microsoft.com/en-us/azure/foundry/responsible-ai/openai/limited-access), [Claude in Foundry](https://learn.microsoft.com/en-us/azure/foundry/foundry-models/how-to/use-foundry-models-claude), [quotas](https://learn.microsoft.com/en-us/azure/foundry/openai/quotas-limits)).
- **Google Cloud:** Claude models are enabled with **Enable** on their Model Garden card ([use Claude](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/use-claude)), which takes the Consumer Procurement Entitlement Manager role and an organization policy allowing the Cloud Commerce Consumer Procurement API; partner models' default quotas can be viewed and raised on request in the Google Cloud console ([partner models](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/use-partner-models)); and some models are released behind an allowlist ([release notes](https://docs.cloud.google.com/gemini-enterprise-agent-platform/release-notes)).

A model refused for these reasons shows as `requires_action` or `refused` under *Availability*, with what to do. An agreement accepted through the gateway is checked with AWS's availability check, which is not a call: until a request succeeds, the first real request is the true check. Unless you ask for a test call, accepting calls no model; but with automatic offers on, the gateway's usual check sends one 1-token request per model identifier it would offer (at most hourly) before offering it.

## Batches on your cloud account

Batches run as the provider's own batch jobs at its batch price only when the connection may run them, which the default setup does not grant. When connecting AWS, Azure OpenAI or Google Cloud, **Batches (optional)** adds it to the instructions:

- **AWS:** name the S3 bucket batches are staged in and the service role Bedrock assumes to read and write it. The gateway's role may then create, read and stop batch inference jobs, pass that role to Bedrock, and use that bucket ([Bedrock batch inference permissions](https://docs.aws.amazon.com/bedrock/latest/userguide/batch-inference-permissions.html)).
- **Azure OpenAI:** a custom role that may upload batch files and create and cancel batch jobs, which the inference role cannot ([Azure built-in roles](https://learn.microsoft.com/en-us/azure/role-based-access-control/built-in-roles/ai-machine-learning)). Batches also need a deployment of a batch type. Azure AI Foundry runs no batch jobs.
- **Google Cloud:** name the Cloud Storage bucket: the service account and Agent Platform's service agent may then read and write it.

Then name the same bucket for the connection under *Usage → Batches → Staging*. Without these, batch lines run one by one at the on-demand price where your organization allows it, or are refused.

## When a provider fails

A provider deployment that keeps failing is taken out of service briefly: requests for it get `503` with code `provider_circuit_open` and a `Retry-After` header. Retry after that delay with the same `Idempotency-Key`. The gateway does not fail over to another deployment by itself.

> **Scoped:** provider failover guarantees and provider-specific request options are not part of this contract.
