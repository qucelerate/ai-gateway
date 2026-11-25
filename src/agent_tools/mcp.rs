//! The gateway's operations as Model Context Protocol tools: over stdio
//! (`gwctl mcp`, newline-delimited JSON-RPC 2.0) and over streamable HTTP
//! (`/api/mcp`, which hands each message to [`handle`]). Each tool is a
//! `gwctl` command, planned by `commands.rs` and run by `client.rs`, so permissions,
//! the role ceiling, idempotency, `If-Match` and audit apply exactly as they
//! do from the command line. Secrets (API keys, agent tokens) appear only in
//! the results of the tools that create them.

use std::{
    io::{BufRead, Write},
    sync::Arc,
};

use serde_json::{Map, Value, json};

use super::{
    args::Args,
    client::{Client, Failure},
    commands::{self, Plan},
};

/// Protocol versions this server speaks, newest first.
/// Messages one batch may hold, and how many of them run at once: a small
/// request body must not start enough calls to crowd out other requests.
const MAX_BATCH: usize = 32;
const BATCH_CONCURRENCY: usize = 8;

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone, Copy)]
enum Kind {
    String,
    Integer,
    /// An object or array, passed to the command as JSON text.
    Json,
    /// A list of strings, passed comma-separated.
    List,
    /// An object of query parameters, each passed as its own option.
    Filters,
}

struct Param {
    name: &'static str,
    kind: Kind,
    required: bool,
    description: &'static str,
}

/// A tool: its `gwctl` words (`{name}` is replaced by that argument) and
/// its options (every argument not named in the words, as `--name`).
struct Tool {
    name: &'static str,
    description: &'static str,
    words: &'static [&'static str],
    params: &'static [Param],
}

const fn required(name: &'static str, kind: Kind, description: &'static str) -> Param {
    Param {
        name,
        kind,
        required: true,
        description,
    }
}

const fn optional(name: &'static str, kind: Kind, description: &'static str) -> Param {
    Param {
        name,
        kind,
        required: false,
        description,
    }
}

const FILTERS: Param = optional(
    "filters",
    Kind::Filters,
    "Query parameters, such as {\"page_size\": \"100\"} or {\"group_by\": \"model\", \"from\": \"2026-10-01T00:00:00Z\"}.",
);

const TOOLS: &[Tool] = &[
    Tool {
        name: "whoami",
        description: "The agent the token belongs to.",
        words: &["me"],
        params: &[],
    },
    Tool {
        name: "list_organizations",
        description: "The organization the token can act in.",
        words: &["orgs"],
        params: &[],
    },
    Tool {
        name: "get_cloud_instructions",
        description: "How to grant the gateway access to a cloud account: a CloudFormation link, CLI and Terraform for AWS; admin consent for Azure; gcloud and Terraform for Google. Without `cloud`, which clouds the gateway can connect to.",
        words: &["connections", "instructions"],
        params: &[
            optional("cloud", Kind::String, "aws, azure or google."),
            optional("account_id", Kind::String, "AWS account id (aws)."),
            optional("region", Kind::String, "AWS region (aws)."),
            optional(
                "project_id",
                Kind::String,
                "Google Cloud project id (google).",
            ),
            optional(
                "family",
                Kind::String,
                "azure-openai or azure-ai-foundry (azure).",
            ),
        ],
    },
    Tool {
        name: "verify_connection",
        description: "Proves the gateway can reach a cloud account with a credential, before anything is stored. Returns the status and the models it can see.",
        words: &["connections", "verify", "{adapter_instance_key}"],
        params: &[
            required(
                "adapter_instance_key",
                Kind::String,
                "The connection's key, such as bedrock-123456789012.",
            ),
            required(
                "family",
                Kind::String,
                "bedrock, azure-openai, azure-ai-foundry, gemini, openai, anthropic or openai-compatible.",
            ),
            required(
                "credential",
                Kind::Json,
                "The credential, such as {\"role_arn\": \"arn:aws:iam::123456789012:role/QucelerateGateway\"}.",
            ),
        ],
    },
    Tool {
        name: "set_connection",
        description: "Stores (or replaces) a connection's credential. The credential is write-only and never returned.",
        words: &["connections", "set", "{adapter_instance_key}"],
        params: &[
            required(
                "adapter_instance_key",
                Kind::String,
                "The connection's key.",
            ),
            required(
                "credential",
                Kind::Json,
                "The credential, as for verify_connection.",
            ),
        ],
    },
    Tool {
        name: "list_connections",
        description: "Stored connections (metadata only).",
        words: &["connections", "list"],
        params: &[FILTERS],
    },
    Tool {
        name: "enable_automatic_offers",
        description: "Opts a connection into automatic offers: each discovery run offers every listed model a price source prices, as an audited catalog release.",
        words: &["offers", "enable", "{adapter_instance_key}"],
        params: &[
            required(
                "adapter_instance_key",
                Kind::String,
                "The connection's key.",
            ),
            optional(
                "location",
                Kind::String,
                "The region or location to offer models in, such as us-east-2.",
            ),
        ],
    },
    Tool {
        name: "get_automatic_offers",
        description: "A connection's automatic offers: what the latest run offered, priced or withdrew.",
        words: &["offers", "get", "{adapter_instance_key}"],
        params: &[required(
            "adapter_instance_key",
            Kind::String,
            "The connection's key.",
        )],
    },
    Tool {
        name: "disable_automatic_offers",
        description: "Opts a connection out of automatic offers.",
        words: &["offers", "disable", "{adapter_instance_key}"],
        params: &[required(
            "adapter_instance_key",
            Kind::String,
            "The connection's key.",
        )],
    },
    Tool {
        name: "list_model_agreements",
        description: "A Bedrock connection's models that need an AWS Marketplace agreement before Bedrock serves them (evidence agreement_required), each with what it unblocks, its live offers (offer id, rate card, legal terms) and any agreement accepted through the gateway with its status (pending, available, verified, refused, failed). Read only.",
        words: &["agreements", "list", "{adapter_instance_key}"],
        params: &[required(
            "adapter_instance_key",
            Kind::String,
            "The Bedrock connection's key.",
        )],
    },
    Tool {
        name: "accept_model_agreements",
        description: "Accepts Bedrock models' AWS Marketplace agreements, creating subscriptions in the tenant's AWS account billed per use at the offer's rates. Name each model with the offer id list_model_agreements showed (there is no \"all\"; a model with several offers needs one chosen); nothing is accepted if any named offer changed. Without verify the gateway calls no model when accepting, but with automatic offers on, discovery then verifies each identifier it would offer with one 1-token request (at most hourly) before offering it. Only accept what the organization's administrator asked for. Accepting does not enable a model AWS has not made available to the account.",
        words: &["agreements", "accept", "{adapter_instance_key}"],
        params: &[
            required(
                "adapter_instance_key",
                Kind::String,
                "The Bedrock connection's key.",
            ),
            required(
                "models",
                Kind::List,
                "MODEL=OFFER_ID pairs, such as anthropic.claude-haiku-5-5=offer-u3aih6zr7uw5u.",
            ),
        ],
    },
    Tool {
        name: "run_discovery",
        description: "Starts a model discovery run for a connection now.",
        words: &["discovery", "sync", "{adapter_instance_key}"],
        params: &[required(
            "adapter_instance_key",
            Kind::String,
            "The connection's key.",
        )],
    },
    Tool {
        name: "list_discovered_models",
        description: "Models discovery found, with what the gateway has seen calling each (evidence). Filters: adapter_instance_key, state (listed, missing), deployed, evidence (comma-separated: verified, refused, entitled, requires_action, none).",
        words: &["discovery", "models"],
        params: &[FILTERS],
    },
    Tool {
        name: "list_deployable_models",
        description: "Models each provider account may deploy (Azure's account model catalog), with the listed deployments serving each; an empty list means none does yet. Filters: adapter_instance_key, deployed, lifecycle_status, operation (chat, chat_stream, embeddings).",
        words: &["discovery", "deployable"],
        params: &[FILTERS],
    },
    Tool {
        name: "list_deployments",
        description: "What the active catalog release offers: deployments with their operations and prices.",
        words: &["catalog", "deployments"],
        params: &[FILTERS],
    },
    Tool {
        name: "list_routes",
        description: "The model names (aliases) clients call, and the deployments they route to.",
        words: &["catalog", "routes"],
        params: &[FILTERS],
    },
    Tool {
        name: "list_releases",
        description: "Catalog releases, newest first.",
        words: &["catalog", "releases"],
        params: &[FILTERS],
    },
    Tool {
        name: "list_price_sources",
        description: "The gateway's price sources, when each last fetched, and what it publishes.",
        words: &["prices", "sources"],
        params: &[FILTERS],
    },
    Tool {
        name: "fetch_price_source",
        description: "Fetches a price source within about a minute instead of at its next daily fetch, such as after enabling automatic offers. Refused if the source was fetched in the last 10 minutes.",
        words: &["prices", "fetch", "{source}"],
        params: &[required(
            "source",
            Kind::String,
            "The price source, such as aws-price-list:bedrock.",
        )],
    },
    Tool {
        name: "list_price_mappings",
        description: "Which price source item prices each deployment.",
        words: &["prices", "mappings"],
        params: &[FILTERS],
    },
    Tool {
        name: "set_price_mapping",
        description: "Prices a deployment from a price source item.",
        words: &["prices", "map", "{deployment}", "{source}"],
        params: &[
            required("deployment", Kind::String, "The deployment name."),
            required(
                "source",
                Kind::String,
                "The price source, such as aws-price-list:bedrock.",
            ),
            required("item", Kind::String, "The price source item."),
            optional("region", Kind::String, "The item's region."),
        ],
    },
    Tool {
        name: "list_service_accounts",
        description: "Service accounts: the identities applications call inference as.",
        words: &["service-accounts", "list"],
        params: &[FILTERS],
    },
    Tool {
        name: "create_service_account",
        description: "Creates a service account.",
        words: &["service-accounts", "create"],
        params: &[required("name", Kind::String, "Its name.")],
    },
    Tool {
        name: "list_api_keys",
        description: "A service account's API keys (metadata only).",
        words: &["keys", "list", "{service_account_id}"],
        params: &[
            required("service_account_id", Kind::String, "The service account."),
            FILTERS,
        ],
    },
    Tool {
        name: "create_api_key",
        description: "Creates an API key for a service account. The secret (gw_...) is in this result only; store it now.",
        words: &["keys", "create", "{service_account_id}"],
        params: &[
            required("service_account_id", Kind::String, "The service account."),
            required(
                "scopes",
                Kind::List,
                "inference, embeddings, catalog-read, admin-read.",
            ),
            optional(
                "expires_at",
                Kind::String,
                "RFC 3339 time; never expires when absent.",
            ),
        ],
    },
    Tool {
        name: "revoke_api_key",
        description: "Revokes an API key at once.",
        words: &["keys", "revoke", "{service_account_id}", "{key_id}"],
        params: &[
            required("service_account_id", Kind::String, "The service account."),
            required("key_id", Kind::String, "The key."),
        ],
    },
    Tool {
        name: "get_end_user_settings",
        description: "A service account's budgets per end user of its app: whether requests must name the end user, the currency, and the default plan.",
        words: &["end-users", "settings", "{service_account_id}"],
        params: &[required(
            "service_account_id",
            Kind::String,
            "The app's service account.",
        )],
    },
    Tool {
        name: "set_end_user_settings",
        description: "Turns on budgets per end user for a service account, or changes them. The currency is fixed once a plan or credit grant exists.",
        words: &["end-users", "configure", "{service_account_id}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("currency", Kind::String, "ISO 4217 code, such as USD."),
            optional(
                "requirement",
                Kind::String,
                "optional (default) or required: required refuses requests that name no end user.",
            ),
            optional(
                "default_plan",
                Kind::String,
                "The plan of users never assigned one; absent refuses them.",
            ),
        ],
    },
    Tool {
        name: "list_end_user_plans",
        description: "A service account's end-user plans: allowance per period, per-user rate limits, and totals (users on each and their spend this period).",
        words: &["end-users", "plans", "{service_account_id}"],
        params: &[required(
            "service_account_id",
            Kind::String,
            "The app's service account.",
        )],
    },
    Tool {
        name: "set_end_user_plan",
        description: "Creates or replaces a plan: what each user on it may spend per period, in money units (10^-6 of the currency's smallest unit; $1.00 is 100000000; 0 is a credits-only plan), and optional per-user rate limits. Fields left out take their defaults: a monthly period, no limits.",
        words: &["end-users", "plan-set", "{service_account_id}", "{plan}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("plan", Kind::String, "Plan name: a-z, 0-9, _ and -."),
            required(
                "allowance",
                Kind::Integer,
                "Allowance per period in money units.",
            ),
            optional(
                "period",
                Kind::String,
                "monthly (UTC calendar month, the default), weekly (from Monday 00:00 UTC), daily (UTC day) or days (period_days days).",
            ),
            optional(
                "period_days",
                Kind::Integer,
                "With period days: 1 to 366 days per period.",
            ),
            optional(
                "requests_per_minute",
                Kind::Integer,
                "Requests each user may make per minute (1 to 1000000).",
            ),
            optional(
                "tokens_per_minute",
                Kind::Integer,
                "Tokens each user may use per minute (1 to 1000000000); a request's output cap is lowered to fit.",
            ),
            optional(
                "max_concurrent",
                Kind::Integer,
                "Requests each user may have in flight at once (1 to 1000000).",
            ),
        ],
    },
    Tool {
        name: "delete_end_user_plan",
        description: "Removes a plan no user is on and that is not the default.",
        words: &["end-users", "plan-delete", "{service_account_id}", "{plan}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("plan", Kind::String, "Plan name."),
        ],
    },
    Tool {
        name: "list_end_users",
        description: "A service account's end users with their plan, this period's allowance spent and left, and their credit balance.",
        words: &["end-users", "list", "{service_account_id}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            FILTERS,
        ],
    },
    Tool {
        name: "get_end_user",
        description: "One end user: their plan, this period's allowance spent and left, and their credit balance.",
        words: &["end-users", "get", "{service_account_id}", "{end_user}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("end_user", Kind::String, "The app's ID for the user."),
        ],
    },
    Tool {
        name: "assign_end_user_plan",
        description: "Puts an end user on a plan (`default` for the service account's default plan), from their next request; this month's spend is kept.",
        words: &["end-users", "assign", "{service_account_id}", "{end_user}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("end_user", Kind::String, "The app's ID for the user."),
            required("plan", Kind::String, "Plan name, or default."),
        ],
    },
    Tool {
        name: "list_credit_grants",
        description: "An end user's credit grants with what is spent, reserved and left of each.",
        words: &["credits", "list", "{service_account_id}", "{end_user}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("end_user", Kind::String, "The app's ID for the user."),
        ],
    },
    Tool {
        name: "grant_credits",
        description: "Grants an end user prepaid credits, once per reference: the same reference again returns the grant already made.",
        words: &["credits", "grant", "{service_account_id}", "{end_user}"],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("end_user", Kind::String, "The app's ID for the user."),
            required("reference", Kind::String, "The app's purchase or order ID."),
            required(
                "amount",
                Kind::Integer,
                "Money units (10^-6 of the currency's smallest unit).",
            ),
            optional(
                "expires_at",
                Kind::String,
                "RFC 3339 time; never expires when absent.",
            ),
        ],
    },
    Tool {
        name: "reverse_credits",
        description: "Takes back up to what is unspent and unreserved of a credit grant (a refund or chargeback), once per reference; reports what was taken.",
        words: &[
            "credits",
            "reverse",
            "{service_account_id}",
            "{end_user}",
            "{grant_id}",
        ],
        params: &[
            required(
                "service_account_id",
                Kind::String,
                "The app's service account.",
            ),
            required("end_user", Kind::String, "The app's ID for the user."),
            required("grant_id", Kind::String, "The grant."),
            required(
                "reference",
                Kind::String,
                "The app's refund or chargeback ID.",
            ),
            required("amount", Kind::Integer, "Money units to take back."),
            required("reason", Kind::String, "Why."),
        ],
    },
    Tool {
        name: "list_agents",
        description: "The organization's agents and their tokens' metadata.",
        words: &["agents", "list"],
        params: &[FILTERS],
    },
    Tool {
        name: "create_agent",
        description: "Creates an agent: a non-human administrator. Its role cannot be above the caller's.",
        words: &["agents", "create"],
        params: &[
            required("name", Kind::String, "Its name."),
            required(
                "role",
                Kind::String,
                "owner, admin, member, auditor or read-only.",
            ),
            optional("description", Kind::String, "What it is for."),
        ],
    },
    Tool {
        name: "create_agent_token",
        description: "Mints a token for an agent. The token (gwa_...) is in this result only. With service_account_id it reaches only that service account's end-user settings, plans and users (for an app's backend).",
        words: &["tokens", "create", "{agent_id}"],
        params: &[
            required("agent_id", Kind::String, "The agent."),
            required("name", Kind::String, "The token's name."),
            optional(
                "expires_in_days",
                Kind::Integer,
                "1 to 365; never expires when absent.",
            ),
            optional(
                "service_account_id",
                Kind::String,
                "Limits the token to this service account's end users.",
            ),
        ],
    },
    Tool {
        name: "usage_report",
        description: "Usage and spend, totalled per currency and confidence. Filters: from and to (required, RFC 3339), group_by (one or two of day, team, service_account, model, deployment, provider_family, location and model_master, such as \"team,model\"; default day), team_id, service_account_id, model, deployment, provider_family, location and model_master (the master record a request's deployment was linked to when served, such as anthropic/claude-sonnet-4-6; comma-separated lists match any), currency.",
        words: &["usage"],
        params: &[FILTERS],
    },
    Tool {
        name: "usage_series",
        description: "Requests, tokens, spend (per currency, never summed across currencies), failures and durations over time: from and to (required, RFC 3339), granularity (hour, day or week; at most 744 buckets), split_by (one of team, service_account, api_key, user, model, alias, deployment, provider_family, location, operation, status, outcome, error_category and model_master) into the top limit values (1 to 20, default 8) by rank_by (requests, tokens, or cost with a currency) plus other, with totals and the previous window's totals. Filters: team_id, service_account_id, api_key_id, user_id, model, alias, deployment, provider_family, location, operation, status (succeeded, failed, pending, expired, unknown, failed-over), outcome, error_category and model_master (comma-separated lists match any), currency.",
        words: &["usage", "series"],
        params: &[FILTERS],
    },
    Tool {
        name: "usage_events",
        description: "The requests behind a usage_series slice, newest first: tokens, amount, status, duration and the trace's content state per request; its id is the trace id for get_trace. Takes the usage_series filters (from and to required) and page_size and page_token. Needs trace_read.",
        words: &["usage", "events"],
        params: &[FILTERS],
    },
    Tool {
        name: "list_traces",
        description: "Recent requests (metadata only), newest first. Filters: team_id, service_account_id, api_key_id, model, deployment, provider_family, location, status (pending, finalized) and outcome (comma-separated lists match any), q (substring of the model or deployment, or a trace or request id), from and to.",
        words: &["traces"],
        params: &[FILTERS],
    },
    Tool {
        name: "trace_summary",
        description: "Requests counted by one or two of day (UTC), team, service_account, model, deployment, provider_family, location, status and outcome, with amounts per currency: filtered by any list_traces filter.",
        words: &["traces", "summary"],
        params: &[
            required(
                "group_by",
                Kind::String,
                "One or two dimensions, comma-separated, such as \"team,model\".",
            ),
            optional(
                "filters",
                Kind::Filters,
                "Any list_traces filter, such as {\"status\": \"finalized\", \"from\": \"2026-10-01T00:00:00Z\"}. No page_size or page_token: a summary is one response of at most 500 groups.",
            ),
        ],
    },
    Tool {
        name: "get_trace",
        description: "One request's metadata.",
        words: &["trace", "{trace_id}"],
        params: &[required("trace_id", Kind::String, "The trace.")],
    },
    Tool {
        name: "get_trace_content",
        description: "What a traced request sent and got back (request messages and parameters, response content, tool calls and finish reason), with its timing; or why it has none (pending, not_traced, off, unavailable, failed, expired). Needs trace_content_read (owner, admin); every read is audited.",
        words: &["trace", "{trace_id}", "content"],
        params: &[required("trace_id", Kind::String, "The trace.")],
    },
    Tool {
        name: "get_model_card",
        description: "One catalog model's card: overview, its deployments with region, on-demand and batch prices, availability and retirement dates, usage and request outcomes here (with usage_read), public facts from card files with their sources (evaluations, safety, data handling per provider), the content filter on each deployment where the cloud reports it, the organization's evaluations and note, and catalog history (with audit_read).",
        words: &["model", "card", "{model}"],
        params: &[required(
            "model",
            Kind::String,
            "The catalog model identifier, exactly as deployments name it (us.amazon.nova-2-lite-v1:0, gemini-4-argon).",
        )],
    },
    Tool {
        name: "list_model_cards",
        description: "Every model the active catalog release serves, with its card file (if any), context window, maximum output and retirement facts, and the earliest retirement date a provider listing gives its deployments.",
        words: &["model", "cards"],
        params: &[],
    },
    Tool {
        name: "set_model_note",
        description: "Sets the organization's note on a model's card, shown to everyone who reads the catalog; an empty note clears it. Needs catalog_write.",
        words: &["model", "note-set", "{model}"],
        params: &[
            required("model", Kind::String, "The catalog model identifier."),
            required("note", Kind::String, "At most 4000 characters."),
        ],
    },
    Tool {
        name: "import_model_card",
        description: "Imports a Hugging Face Hub model card (license, parameter count, reported evaluations, a link to the repository) as a model's card, shown only when no curated card file matches the model. Needs catalog_write and an operator-enabled Hub.",
        words: &["model", "import", "{model}"],
        params: &[
            required("model", Kind::String, "The catalog model identifier."),
            required(
                "repository",
                Kind::String,
                "The Hub repository, owner/name.",
            ),
            required(
                "reported-by",
                Kind::String,
                "provider when the repository is the model provider's own, else independent.",
            ),
        ],
    },
    Tool {
        name: "remove_model_card_import",
        description: "Removes a model's imported Hugging Face card (kept in history, no longer shown).",
        words: &["model", "import-remove", "{model}"],
        params: &[required(
            "model",
            Kind::String,
            "The catalog model identifier.",
        )],
    },
    Tool {
        name: "list_model_masters",
        description: "Master model records: one model whoever serves it (each model card's id, such as anthropic/claude-sonnet-4-6, and the organization's own org/SLUG records), each with its linked deployments in the active release side by side (provider, account, location, price per operation, limits, tool calling, availability, the aliases that route to each); deployments with no master; and every link the organization set. A deployment's master is the organization's link for its family and model, else the card that lists them exactly, else none.",
        words: &["model", "masters"],
        params: &[],
    },
    Tool {
        name: "get_model_master",
        description: "One master record (a card's id or org/SLUG) with its linked deployments in the active release.",
        words: &["model", "master", "{master}"],
        params: &[required(
            "master",
            Kind::String,
            "anthropic/claude-sonnet-4-6 or org/SLUG.",
        )],
    },
    Tool {
        name: "propose_route",
        description: "Read-only: a route across providers for a master record. For each operation, every linked deployment that serves it at weight 1 and priority 1, every deployment left out with a reason (no_price, provider_not_permitted, location_not_permitted, availability_refused, availability_requires_action, no_tools, mixed_tool_call_ids, currency, too_many_deployments), and a diff against an existing alias of that name. Nothing changes until a release with the routes is created and activated.",
        words: &["model", "route-proposal", "{master}"],
        params: &[
            required(
                "master",
                Kind::String,
                "anthropic/claude-sonnet-4-6 or org/SLUG.",
            ),
            optional(
                "operations",
                Kind::List,
                "chat (the default), embeddings, chat_batch, embeddings_batch.",
            ),
            optional(
                "tools",
                Kind::String,
                "true when the route must take tools.",
            ),
            optional(
                "alias",
                Kind::String,
                "The alias; the master's model part by default (claude-sonnet-4-6).",
            ),
        ],
    },
    Tool {
        name: "set_model_master",
        description: "Creates or renames one of the organization's own master records, org/SLUG, for a model no card covers (such as its own servers). Needs catalog_write.",
        words: &["model", "master-set", "{master}"],
        params: &[
            required(
                "master",
                Kind::String,
                "org/SLUG: 1-64 lowercase letters, digits, '-', '.' and '_'.",
            ),
            required("name", Kind::String, "At most 120 characters."),
            optional(
                "modality",
                Kind::String,
                "chat (the default) or embeddings.",
            ),
        ],
    },
    Tool {
        name: "delete_model_master",
        description: "Deletes an organization master record (kept in history). Refused while links name it. Needs catalog_write.",
        words: &["model", "master-delete", "{master}"],
        params: &[required("master", Kind::String, "org/SLUG.")],
    },
    Tool {
        name: "link_model_master",
        description: "Links a provider family's model identifier to a master record, or to none (detaching a deployment a card would match). Per organization, family and model, not per release; the active release must serve it, in the master's modality. Changes no route or price. Needs catalog_write.",
        words: &["model", "link", "{family}", "{model}"],
        params: &[
            required(
                "family",
                Kind::String,
                "The provider family (bedrock, anthropic, azure-ai-foundry, ...).",
            ),
            required(
                "model",
                Kind::String,
                "The catalog model identifier (us.anthropic.claude-sonnet-4-6).",
            ),
            required("master", Kind::String, "A card's id, org/SLUG, or none."),
        ],
    },
    Tool {
        name: "unlink_model_master",
        description: "Clears a model's link (kept in history), so the card that lists it decides again. Needs catalog_write.",
        words: &["model", "unlink", "{family}", "{model}"],
        params: &[
            required("family", Kind::String, "The provider family."),
            required("model", Kind::String, "The catalog model identifier."),
        ],
    },
    Tool {
        name: "list_evaluation_sets",
        description: "The organization's own model evaluation sets (items with expected answers).",
        words: &["evaluations", "sets"],
        params: &[],
    },
    Tool {
        name: "create_evaluation_set",
        description: "Creates an evaluation set: items with an id, the expected answer and how an answer matches (exact or contains). Run it by submitting a chat batch whose custom_ids are ITEM[:anything], then score_evaluation_batch. Needs catalog_write.",
        words: &["evaluations", "create"],
        params: &[
            required("name", Kind::String, "At most 120 characters."),
            required(
                "items",
                Kind::Json,
                "[{\"id\": \"q1\", \"expected\": \"Paris\", \"match\": \"exact\"}], at most 1000.",
            ),
            optional("description", Kind::String, "At most 1000 characters."),
        ],
    },
    Tool {
        name: "score_evaluation_batch",
        description: "Scores an evaluation set from a completed chat batch of the organization and records the run: each model's items, answers and correct answers (answers are not stored). The scores show on each model's card as the organization's own.",
        words: &["evaluations", "run", "{set}"],
        params: &[
            required("set", Kind::String, "The evaluation set id."),
            required("batch", Kind::String, "The completed batch's id."),
        ],
    },
    Tool {
        name: "get_evaluation_set",
        description: "One evaluation set of the organization, with its items and version.",
        words: &["evaluations", "set", "{set}"],
        params: &[required("set", Kind::String, "The evaluation set id.")],
    },
    Tool {
        name: "delete_evaluation_set",
        description: "Deletes an evaluation set: it leaves lists and model cards and takes no more runs, while its items, runs and scores are kept. Needs catalog_write.",
        words: &["evaluations", "delete", "{set}"],
        params: &[required("set", Kind::String, "The evaluation set id.")],
    },
    Tool {
        name: "list_evaluation_runs",
        description: "An evaluation set's runs, newest first, with each model's score.",
        words: &["evaluations", "runs", "{set}"],
        params: &[required("set", Kind::String, "The evaluation set id.")],
    },
    Tool {
        name: "list_batches",
        description: "The organization's batches (files of requests run within 24 hours), newest first: status, line counts and the service account that made each. Filters: status (validating, in_progress, finalizing, completed, failed, expired, cancelling, cancelled), service_account_id, page_size, page_token. Needs usage_read.",
        words: &["batches", "list"],
        params: &[FILTERS],
    },
    Tool {
        name: "get_batch",
        description: "One batch: each provider job's deployment, whether it runs as the provider's batch job (batch price) or emulated line by line (on-demand price), its line outcomes, and the batch's cost so far and still held per currency. Needs usage_read.",
        words: &["batches", "get", "{batch_id}"],
        params: &[required(
            "batch_id",
            Kind::String,
            "The batch, batch_<uuid>.",
        )],
    },
    Tool {
        name: "cancel_batch",
        description: "Asks a batch to stop: lines not yet run end at no charge; lines already with the provider settle as it reports them. Needs service_account_mutate; audited as batch.cancelled.",
        words: &["batches", "cancel", "{batch_id}"],
        params: &[required(
            "batch_id",
            Kind::String,
            "The batch, batch_<uuid>.",
        )],
    },
    Tool {
        name: "get_batch_output",
        description: "A finished batch's output file (JSONL, one answer per line, by custom_id). Results carry the model's answers: needs trace_content_read.",
        words: &["batches", "output", "{batch_id}"],
        params: &[required(
            "batch_id",
            Kind::String,
            "The batch, batch_<uuid>.",
        )],
    },
    Tool {
        name: "get_batch_errors",
        description: "A finished batch's error file (JSONL, one failure per line, by custom_id). Needs trace_content_read.",
        words: &["batches", "errors", "{batch_id}"],
        params: &[required(
            "batch_id",
            Kind::String,
            "The batch, batch_<uuid>.",
        )],
    },
    Tool {
        name: "get_batch_settings",
        description: "Whether lines no provider batch job can take run emulated (one by one at the on-demand price, within rate limits), and how many at once. Off unless set. Needs catalog_read.",
        words: &["batches", "settings"],
        params: &[],
    },
    Tool {
        name: "set_batch_settings",
        description: "Allows or forbids emulated batches. Needs catalog_write; audited as batch_settings.set.",
        words: &["batches", "configure"],
        params: &[
            required("emulated", Kind::String, "on or off."),
            optional(
                "concurrency",
                Kind::Integer,
                "Lines of one emulated job in flight at once, 1 to 32 (default 4).",
            ),
        ],
    },
    Tool {
        name: "list_batch_staging",
        description: "Where each Bedrock or Gemini (Agent Platform) connection stages its batch jobs: the tenant's S3 bucket and Bedrock service role, or Cloud Storage bucket. Needs catalog_read.",
        words: &["batches", "staging"],
        params: &[],
    },
    Tool {
        name: "set_batch_staging",
        description: "Sets where a connection's batch jobs stage. Bedrock: bucket, region and role_arn (the role Bedrock assumes; the connection's role needs iam:PassRole on it). Gemini on Agent Platform: bucket only. Needs credential_write; audited as batch_staging.set.",
        words: &["batches", "stage", "{connection}"],
        params: &[
            required(
                "connection",
                Kind::String,
                "The connection's adapter instance key.",
            ),
            required("bucket", Kind::String, "The bucket name."),
            optional("prefix", Kind::String, "Object name prefix, ending in /."),
            optional("region", Kind::String, "The S3 bucket's region (Bedrock)."),
            optional(
                "role_arn",
                Kind::String,
                "The Bedrock batch service role's ARN.",
            ),
        ],
    },
    Tool {
        name: "delete_batch_staging",
        description: "Deletes a connection's staging (kept, hidden): its new batch lines run emulated where allowed, else are refused; jobs already sent still collect their output. Needs credential_write.",
        words: &["batches", "unstage", "{connection}"],
        params: &[required(
            "connection",
            Kind::String,
            "The connection's adapter instance key.",
        )],
    },
    Tool {
        name: "list_notifications",
        description: "The agent's notifications in the organization, newest first: prices adopted or held for review, price sources failing to fetch, provider accounts whose model listing fails, budgets past their threshold, API keys and agent tokens near expiry, webhook endpoints failing, end users low on credits. Filters: kind (price.adopted, price.held, price_source.failing, provider_account.sync_failing, budget.threshold, api_key.expiring, agent_token.expiring, webhook.failing, end_user.credits_low), severity (info, warning, action), read (all, unread, read).",
        words: &["notifications"],
        params: &[FILTERS],
    },
    Tool {
        name: "count_unread_notifications",
        description: "How many of the agent's notifications are unread and not resolved.",
        words: &["notifications", "count"],
        params: &[],
    },
    Tool {
        name: "mark_notifications_read",
        description: "Marks chosen notifications read for this agent.",
        words: &["notifications", "read"],
        params: &[required(
            "ids",
            Kind::List,
            "Notification ids to mark read (at most 100).",
        )],
    },
    Tool {
        name: "mark_notifications_read_through",
        description: "Marks read for this agent a notification and every one created before it.",
        words: &["notifications", "read"],
        params: &[required(
            "through",
            Kind::String,
            "A notification id, such as the newest one listed.",
        )],
    },
    Tool {
        name: "list_audit_events",
        description: "Administrative audit events, newest first (metadata only). Filters: actor, actor_type, action, target_type and outcome (comma-separated lists match any), target_id, q (substring of the action or target type), occurred_after, occurred_before.",
        words: &["audit"],
        params: &[FILTERS],
    },
    Tool {
        name: "audit_summary",
        description: "Audit events counted by one or two of action, outcome, target_type, actor_type, actor and day (UTC), filtered by any list_audit_events filter.",
        words: &["audit", "summary"],
        params: &[
            required(
                "group_by",
                Kind::String,
                "One or two dimensions, comma-separated, such as \"action,day\".",
            ),
            optional(
                "filters",
                Kind::Filters,
                "Any list_audit_events filter, such as {\"q\": \"api_key\", \"occurred_after\": \"2026-10-01T00:00:00Z\"}. No page_size or page_token: a summary is one response of at most 500 groups.",
            ),
        ],
    },
    Tool {
        name: "chat",
        description: "Sends one chat message through the gateway with an API key, to check that a model answers.",
        words: &["chat", "{message}"],
        params: &[
            required("model", Kind::String, "The model name (alias) to call."),
            required("message", Kind::String, "The message."),
            optional(
                "api_key",
                Kind::String,
                "The API key (gw_...) to call with; default: GATEWAY_API_KEY under gwctl mcp. Required over HTTP.",
            ),
        ],
    },
    Tool {
        name: "gateway_api",
        description: "Any admin API call. `path` is absolute (/api/admin/v1/...) or relative to the organization (catalog/releases). Writes get an idempotency key; PUT, PATCH and DELETE send the path's current version as If-Match (when the path can be read) unless `if_match` is given.",
        words: &["api", "{method}", "{path}"],
        params: &[
            required("method", Kind::String, "GET, POST, PUT, PATCH or DELETE."),
            required("path", Kind::String, "The path, with any query string."),
            optional("data", Kind::Json, "The request body."),
        ],
    },
];

/// Every tool takes the organization, for a token that sees several, and
/// the retry arguments a write whose outcome was unknown reports.
const COMMON: &[Param] = &[
    optional(
        "organization",
        Kind::String,
        "The organization id; default: the token's only organization.",
    ),
    optional(
        "idempotency_key",
        Kind::String,
        "Only to retry a write whose outcome was unknown: the key that call reported, so the gateway replays it instead of applying it twice.",
    ),
    optional(
        "if_match",
        Kind::String,
        "The If-Match value (an ETag, or * for any state); to retry a write, the one that call reported. Default for PUT, PATCH and DELETE: the current version of `path`, or none when `path` cannot be read.",
    ),
];

/// Reads messages until standard input closes. Each runs as its own task,
/// so a slow tool call never holds up `ping`, `tools/list` or other calls;
/// every response carries its request's id.
pub async fn serve(client: Client) {
    let client = Arc::new(client);
    let mut running = tokio::task::JoinSet::new();
    loop {
        while running.try_join_next().is_some() {}
        let line = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map(|read| (read, line))
        })
        .await;
        let line = match line {
            Ok(Ok((0, _)) | Err(_)) | Err(_) => break,
            Ok(Ok((_, line))) => line,
        };
        if line.trim().is_empty() {
            continue;
        }
        let client = client.clone();
        running.spawn(async move {
            if let Some(response) = handle(&client, &line).await {
                // One locked write per response, so responses never interleave.
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "{response}");
                let _ = stdout.flush();
            }
        });
    }
    // Answer what is still running before exiting.
    while running.join_next().await.is_some() {}
}

/// The response to one message (a request, a notification or a batch), or
/// nothing when it holds only notifications.
pub async fn handle(client: &Client, line: &str) -> Option<Value> {
    match serde_json::from_str(line) {
        // A batch (JSON-RPC 2.0, MCP 2025-03-26): its requests run together
        // and their responses come back as one array; a batch of only
        // notifications gets no response.
        Ok(Value::Array(batch)) if batch.is_empty() => {
            Some(error(Value::Null, -32600, "empty batch"))
        }
        Ok(Value::Array(batch)) if batch.len() > MAX_BATCH => Some(error(
            Value::Null,
            -32600,
            &format!("a batch holds at most {MAX_BATCH} messages"),
        )),
        Ok(Value::Array(batch)) => {
            use futures::StreamExt;
            let responses: Vec<Value> = futures::stream::iter(batch)
                .map(|message| respond(client, message))
                .buffered(BATCH_CONCURRENCY)
                .filter_map(|response| async move { response })
                .collect()
                .await;
            (!responses.is_empty()).then_some(Value::Array(responses))
        }
        Ok(message) => respond(client, message).await,
        Err(_) => Some(error(Value::Null, -32700, "parse error")),
    }
}

/// The response to one request, or nothing for a notification.
async fn respond(client: &Client, message: Value) -> Option<Value> {
    if !message.is_object() {
        return Some(error(Value::Null, -32600, "invalid request"));
    }
    let id = message.get("id").cloned()?;
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|version| PROTOCOL_VERSIONS.contains(version))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "qucelerate-gateway", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Administer a QuCelerate AI gateway organization: connect a cloud account (get_cloud_instructions, verify_connection, set_connection), offer its models (enable_automatic_offers, list_deployments, list_routes), and issue API keys (create_service_account, create_api_key).",
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": TOOLS.iter().map(describe).collect::<Vec<_>>() }),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Some(tool) = TOOLS.iter().find(|tool| tool.name == name) else {
                return Some(error(id, -32602, &format!("unknown tool {name}")));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call(client, tool, &arguments).await
        }
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn describe(tool: &Tool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in tool.params.iter().chain(COMMON) {
        let schema = match param.kind {
            Kind::String => json!({ "type": "string" }),
            Kind::Integer => json!({ "type": "integer" }),
            Kind::Json => json!({ "type": ["object", "array"] }),
            Kind::List => json!({ "type": "array", "items": { "type": "string" } }),
            Kind::Filters => {
                json!({ "type": "object", "additionalProperties": { "type": "string" } })
            }
        };
        let mut schema = schema.as_object().cloned().unwrap_or_default();
        schema.insert("description".into(), json!(param.description));
        properties.insert(param.name.into(), Value::Object(schema));
        if param.required {
            required.push(param.name);
        }
    }
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": { "type": "object", "properties": properties, "required": required, "additionalProperties": false },
    })
}

/// The `gwctl` arguments a tool call stands for.
fn arguments(tool: &Tool, given: &Value) -> Result<Args, String> {
    let given = given.as_object().ok_or("arguments must be an object")?;
    let params: Vec<&Param> = tool.params.iter().chain(COMMON).collect();
    if let Some(unknown) = given
        .keys()
        .find(|name| !params.iter().any(|param| param.name == name.as_str()))
    {
        return Err(format!("unknown argument {unknown}"));
    }
    let mut args = Args::default();
    for word in tool.words {
        let word = match word
            .strip_prefix('{')
            .and_then(|word| word.strip_suffix('}'))
        {
            Some(name) => match given.get(name) {
                Some(Value::String(value)) => value.clone(),
                Some(_) => return Err(format!("{name} must be a string")),
                None => return Err(format!("{name} is required")),
            },
            None => (*word).to_owned(),
        };
        args.positional.push(word);
    }
    for param in params {
        if tool.words.contains(&format!("{{{}}}", param.name).as_str()) {
            continue;
        }
        // A credential, not a command argument: `call` hands it to the client.
        if param.name == "api_key" {
            match given.get(param.name) {
                None | Some(Value::Null | Value::String(_)) => continue,
                Some(_) => return Err(format!("{} has the wrong type", param.name)),
            }
        }
        let option = match param.name {
            "organization" => "org".to_owned(),
            name => name.replace('_', "-"),
        };
        let value = match (given.get(param.name), param.kind) {
            (None | Some(Value::Null), _) if param.required => {
                return Err(format!("{} is required", param.name));
            }
            (None | Some(Value::Null), _) => continue,
            (Some(Value::String(value)), Kind::String) => value.clone(),
            (Some(Value::Number(value)), Kind::Integer) => value.to_string(),
            (Some(value @ (Value::Object(_) | Value::Array(_))), Kind::Json) => value.to_string(),
            (Some(Value::Array(items)), Kind::List) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or(format!("{} must be a list of strings", param.name))
                })
                .collect::<Result<Vec<_>, _>>()?
                .join(","),
            (Some(Value::Object(filters)), Kind::Filters) => {
                for (name, value) in filters {
                    // A declared argument is passed as itself, never twice.
                    if tool.params.iter().any(|param| param.name == name) {
                        return Err(format!("pass {name} as its own argument, not a filter"));
                    }
                    let value = match value {
                        Value::String(value) => value.clone(),
                        Value::Number(_) | Value::Bool(_) => value.to_string(),
                        _ => return Err(format!("filter {name} must be a string")),
                    };
                    args.options.insert(name.clone(), value);
                }
                continue;
            }
            _ => return Err(format!("{} has the wrong type", param.name)),
        };
        args.options.insert(option, value);
    }
    // The optional cloud of get_cloud_instructions is a word, not an option.
    if tool.name == "get_cloud_instructions" {
        if let Some(cloud) = args.options.remove("cloud") {
            args.positional.push(cloud);
        }
    }
    Ok(args)
}

async fn call(client: &Client, tool: &Tool, given: &Value) -> Value {
    let keyed;
    let client = match given.get("api_key").and_then(Value::as_str) {
        Some(api_key) => {
            keyed = client.with_api_key(api_key.to_owned());
            &keyed
        }
        None => client,
    };
    let planned = arguments(tool, given).and_then(|args| {
        commands::plan(args, &|text: &str| {
            serde_json::from_str(text).map_err(|error| format!("not JSON: {error}"))
        })
    });
    let outcome = match planned {
        Ok(Plan::Call(call)) => client.run(call).await,
        Ok(Plan::Help) => Err(Failure::Usage("nothing to do".into())),
        // MCP names each offer: there is no unattended "accept all".
        Ok(Plan::AcceptAllAgreements { .. }) => Err(Failure::Usage(
            "name each model with its offer id (accept_model_agreements)".into(),
        )),
        Err(message) => Err(Failure::Usage(message)),
    };
    let (text, is_error) = match outcome {
        Ok(body) => (
            serde_json::to_string_pretty(&body).unwrap_or_default(),
            false,
        ),
        Err(Failure::Problem { status, body }) => (
            serde_json::to_string_pretty(&match body {
                Value::Null => json!({ "status": status.as_u16() }),
                body => body,
            })
            .unwrap_or_default(),
            true,
        ),
        Err(Failure::Usage(message)) => (message, true),
        Err(Failure::Transport(message)) => {
            (format!("the gateway could not be reached: {message}"), true)
        }
        Err(Failure::Unknown {
            message,
            idempotency_key,
            if_match,
        }) => {
            // Only arguments the tool's schema accepts: no null if_match.
            let mut retry = json!({ "idempotency_key": idempotency_key });
            if let Some(value) = if_match {
                retry["if_match"] = json!(value);
            }
            (
                format!(
                    "the gateway could not be reached: {message}. The write may have been applied; call this tool again with the same arguments plus {retry} so the gateway replays it instead of applying it twice."
                ),
                true,
            )
        }
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::super::commands::{Call, ORG, Precondition};
    use super::*;

    fn planned(name: &str, given: Value) -> Result<Call, String> {
        let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
        match commands::plan(arguments(tool, &given)?, &|text: &str| {
            serde_json::from_str(text).map_err(|error| error.to_string())
        })? {
            Plan::Call(call) => Ok(call),
            Plan::Help => Err("help".into()),
            Plan::AcceptAllAgreements { .. } => Err("accept all".into()),
        }
    }

    #[test]
    fn every_tool_plans_a_call_from_its_required_arguments() {
        for tool in TOOLS {
            let mut given = Map::new();
            for param in tool.params.iter().filter(|param| param.required) {
                let value = match (param.name, param.kind) {
                    ("method", _) => json!("GET"),
                    ("reported-by", _) => json!("independent"),
                    ("emulated", _) => json!("on"),
                    ("models", _) => json!(["anthropic.claude-haiku-5-5=offer-1"]),
                    (_, Kind::String) => json!("x"),
                    (_, Kind::Integer) => json!(1),
                    (_, Kind::Json) => json!({ "role_arn": "arn" }),
                    (_, Kind::List) => json!(["inference"]),
                    (_, Kind::Filters) => json!({}),
                };
                given.insert(param.name.into(), value);
            }
            planned(tool.name, Value::Object(given))
                .unwrap_or_else(|error| panic!("{}: {error}", tool.name));
            assert!(describe(tool)["inputSchema"]["properties"]["organization"].is_object());
        }
    }

    /// MCP accepts only named offers, and never asks for the billed probe.
    #[test]
    fn agreements_are_accepted_by_named_offer_only() {
        let call = planned(
            "accept_model_agreements",
            json!({
                "adapter_instance_key": "bedrock-main",
                "models": ["anthropic.claude-haiku-5-5=offer-1", "xai.grok-9=offer-2"],
            }),
        )
        .unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(
            call.body,
            Some(json!({
                "verify": false,
                "models": [
                    {"model": "anthropic.claude-haiku-5-5", "offer_id": "offer-1"},
                    {"model": "xai.grok-9", "offer_id": "offer-2"}
                ]
            }))
        );
        let tool = TOOLS
            .iter()
            .find(|tool| tool.name == "accept_model_agreements")
            .unwrap();
        assert!(
            tool.params
                .iter()
                .all(|param| param.name != "verify" && param.name != "all")
        );
    }

    #[test]
    fn summaries_take_group_by_as_a_required_argument() {
        for tool in ["trace_summary", "audit_summary"] {
            let schema = describe(
                TOOLS
                    .iter()
                    .find(|candidate| candidate.name == tool)
                    .unwrap(),
            );
            assert!(
                schema["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("group_by")),
                "{tool}"
            );
            assert!(planned(tool, json!({ "filters": { "q": "x" } })).is_err());
            assert!(
                planned(
                    tool,
                    json!({ "group_by": "day", "filters": { "group_by": "day" } })
                )
                .is_err()
            );
        }
        let call = planned(
            "trace_summary",
            json!({ "group_by": "team,model", "filters": { "status": "finalized" } }),
        )
        .unwrap();
        assert_eq!(call.path, format!("{ORG}/traces/summary"));
        assert_eq!(
            call.query,
            [
                ("group_by".to_owned(), "team,model".to_owned()),
                ("status".to_owned(), "finalized".to_owned())
            ]
        );
    }

    #[test]
    fn maps_arguments_onto_the_command() {
        let call = planned("create_api_key", json!({ "service_account_id": "sa", "scopes": ["inference", "catalog-read"], "organization": "o" })).unwrap();
        assert_eq!(call.path, format!("{ORG}/service-accounts/sa/keys"));
        assert_eq!(
            call.body.unwrap()["scopes"],
            json!(["Inference", "CatalogRead"])
        );
        assert_eq!(call.organization.as_deref(), Some("o"));

        let call = planned(
            "set_connection",
            json!({ "adapter_instance_key": "bedrock-1", "credential": { "role_arn": "arn" } }),
        )
        .unwrap();
        assert_eq!(call.body, Some(json!({ "role_arn": "arn" })));
        assert!(matches!(call.precondition, Precondition::Find { .. }));

        let call = planned(
            "usage_report",
            json!({ "filters": { "group_by": "model", "page_size": 50 } }),
        )
        .unwrap();
        assert_eq!(
            call.query,
            [
                ("group_by".to_owned(), "model".to_owned()),
                ("page_size".to_owned(), "50".to_owned())
            ]
        );

        let call = planned(
            "get_cloud_instructions",
            json!({ "cloud": "aws", "account_id": "1", "region": "us-east-2" }),
        )
        .unwrap();
        assert_eq!(call.path, format!("{ORG}/onboarding/aws"));
        assert_eq!(
            call.query,
            [
                ("account_id".to_owned(), "1".to_owned()),
                ("region".to_owned(), "us-east-2".to_owned())
            ]
        );
    }

    #[test]
    fn accepts_the_retry_arguments_a_lost_write_reports() {
        let call = planned(
            "create_api_key",
            json!({ "service_account_id": "sa", "scopes": ["inference"], "idempotency_key": "k-1" }),
        )
        .unwrap();
        assert_eq!(call.idempotency_key.as_deref(), Some("k-1"));
        let call = planned(
            "revoke_api_key",
            json!({ "service_account_id": "sa", "key_id": "k", "if_match": "\"v2\"" }),
        )
        .unwrap();
        assert_eq!(call.precondition, Precondition::Value("\"v2\"".into()));
    }

    #[test]
    fn refuses_unknown_and_missing_arguments() {
        assert_eq!(
            planned("create_service_account", json!({ "nme": "x" })).unwrap_err(),
            "unknown argument nme"
        );
        assert_eq!(
            planned("create_service_account", json!({})).unwrap_err(),
            "name is required"
        );
        assert_eq!(
            planned(
                "create_api_key",
                json!({ "service_account_id": "sa", "scopes": "inference" })
            )
            .unwrap_err(),
            "scopes has the wrong type"
        );
    }

    #[tokio::test]
    async fn speaks_json_rpc() {
        let client = Client::new(super::super::client::Settings {
            url: "http://127.0.0.1:9".into(),
            agent_token: None,
            api_key: None,
            organization: None,
        })
        .ok()
        .unwrap();
        let init = handle(&client, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).await.unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(
            handle(
                &client,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .await
            .is_none()
        );
        let tools = handle(&client, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        assert_eq!(
            tools["result"]["tools"].as_array().unwrap().len(),
            TOOLS.len()
        );
        let refused = handle(&client, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"whoami","arguments":{}}}"#).await.unwrap();
        assert_eq!(refused["result"]["isError"], true);
        assert!(
            refused["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("GATEWAY_AGENT_TOKEN")
        );
        assert_eq!(
            handle(&client, "not json").await.unwrap()["error"]["code"],
            -32700
        );
        assert_eq!(
            handle(
                &client,
                r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#
            )
            .await
            .unwrap()["error"]["code"],
            -32601
        );
        // A batch answers its requests, not its notifications, in one array.
        let batch = handle(
            &client,
            r#"[{"jsonrpc":"2.0","id":7,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":8,"method":"tools/list"}]"#,
        )
        .await
        .unwrap();
        let ids: Vec<&Value> = batch
            .as_array()
            .unwrap()
            .iter()
            .map(|response| &response["id"])
            .collect();
        assert_eq!(ids, [&json!(7), &json!(8)]);
        assert!(
            handle(
                &client,
                r#"[{"jsonrpc":"2.0","method":"notifications/initialized"}]"#
            )
            .await
            .is_none()
        );
        assert_eq!(
            handle(&client, "[]").await.unwrap()["error"]["code"],
            -32600
        );
        assert_eq!(
            handle(&client, "[1]").await.unwrap()[0]["error"]["code"],
            -32600
        );
        let too_many = format!(
            "[{}]",
            vec![r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#; MAX_BATCH + 1].join(",")
        );
        assert_eq!(
            handle(&client, &too_many).await.unwrap()["error"]["code"],
            -32600
        );
        let most = format!(
            "[{}]",
            (0..MAX_BATCH)
                .map(|id| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let answered = handle(&client, &most).await.unwrap();
        let ids: Vec<i64> = answered
            .as_array()
            .unwrap()
            .iter()
            .map(|response| response["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, (0..MAX_BATCH as i64).collect::<Vec<_>>(), "in order");
    }
}
