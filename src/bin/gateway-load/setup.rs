//! Seeds the tenants a run sends as: organizations with a service account
//! and API keys, the load catalog (chat with failover, streamed chat and
//! embeddings), an organization budget, rate limits, optional per-key
//! budgets, end-user plans and trace content; and a few *tight*
//! organizations whose budget, and whose end users' small allowances, run
//! out during the run, to show that no budget is overspent under
//! concurrency. Writes the tenants file the
//! generator and the checks read (it holds API keys: mode 600).

use std::{io::Write, os::unix::fs::OpenOptionsExt};

use gateway::{
    ApiKeyPepper, CatalogManifest, CatalogStore, IdentityStore,
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::args::Flags;

/// Prices of the load catalog, per token in minor units; both deployments
/// of a route cost the same, so a request's charge does not depend on
/// which one served it.
pub const CHAT_INPUT_PRICE: i64 = 3;
pub const CHAT_OUTPUT_PRICE: i64 = 11;
pub const EMBED_INPUT_PRICE: i64 = 1;
/// Model aliases the generator sends.
pub const CHAT_ALIAS: &str = "load-chat";
pub const EMBED_ALIAS: &str = "load-embed";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Key {
    pub id: Uuid,
    pub bearer: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tenant {
    pub org: Uuid,
    pub name: String,
    pub service_account: Uuid,
    pub keys: Vec<Key>,
    /// A budget sized to run out during the run.
    pub tight: bool,
    /// Its organization budget's limit.
    pub limit_minor: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tenants {
    pub run: String,
    pub created_at: String,
    pub max_input_bytes: u64,
    pub max_output_tokens: u32,
    pub reservation_ttl_seconds: u64,
    pub end_users: bool,
    pub trace_content: bool,
    pub tenants: Vec<Tenant>,
}

impl Tenants {
    pub fn read(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
        serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))
    }

    pub fn org_ids(&self) -> Vec<Uuid> {
        self.tenants.iter().map(|tenant| tenant.org).collect()
    }
}

fn capability(operation: &str, max_input_bytes: u64, max_output: u32, ttl: u64) -> Value {
    json!({
        "operation": operation, "max_input_bytes": max_input_bytes,
        "max_output_tokens": max_output, "max_tokens_per_byte": 1,
        "tokenizer": "configured", "reservation_ttl_seconds": ttl,
        "chat_input_count_mode": "unsupported"
    })
}

fn price(deployment: &str, operation: &str, input: i64, output: i64) -> Value {
    json!({
        "deployment": deployment, "operation": operation, "input_minor_per_token": input,
        "output_minor_per_token": output, "cached_input_minor_per_token": 0,
        "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
        "source": "load-test", "source_version": "v1", "confidence": "known"
    })
}

/// The load catalog: `load-chat` (chat and streamed chat) on `chat-a`,
/// failing over to `chat-b`; `load-embed` on `embed-a`, then `embed-b`.
/// The fake rate-limits only models ending in `-a` for a `429` fault.
pub fn manifest(max_input_bytes: u64, max_output: u32, ttl: u64) -> CatalogManifest {
    let chat = |name: &str, model: &str| {
        json!({"name": name, "model": model, "location": null,
               "adapter_instance_key": "openai-main",
               "capabilities": [capability("chat", max_input_bytes, max_output, ttl),
                                capability("chat_stream", max_input_bytes, max_output, ttl)]})
    };
    let embed = |name: &str, model: &str| {
        json!({"name": name, "model": model, "location": null,
               "adapter_instance_key": "openai-main",
               "capabilities": [capability("embeddings", max_input_bytes, max_output, ttl)]})
    };
    let mut prices = Vec::new();
    for deployment in ["chat-a", "chat-b"] {
        for operation in ["chat", "chat_stream"] {
            prices.push(price(
                deployment,
                operation,
                CHAT_INPUT_PRICE,
                CHAT_OUTPUT_PRICE,
            ));
        }
    }
    for deployment in ["embed-a", "embed-b"] {
        prices.push(price(deployment, "embeddings", EMBED_INPUT_PRICE, 0));
    }
    let mut routes = Vec::new();
    for operation in ["chat", "chat_stream"] {
        routes.push(json!({"alias": CHAT_ALIAS, "operation": operation, "deployment": "chat-a"}));
        routes.push(
            json!({"alias": CHAT_ALIAS, "operation": operation, "deployment": "chat-b",
                           "priority": 2}),
        );
    }
    routes.push(json!({"alias": EMBED_ALIAS, "operation": "embeddings", "deployment": "embed-a"}));
    routes.push(
        json!({"alias": EMBED_ALIAS, "operation": "embeddings", "deployment": "embed-b",
                       "priority": 2}),
    );
    serde_json::from_value(json!({
        "release": {"version": "load-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [chat("chat-a", "gpt-load-a"), chat("chat-b", "gpt-load-b"),
                        embed("embed-a", "embed-load-a"), embed("embed-b", "embed-load-b")],
        "prices": prices,
        "routes": routes,
    }))
    .expect("the load catalog parses")
}

struct Options {
    keys: usize,
    key_budgets: bool,
    end_users: bool,
    end_user_allowance: i64,
    tight_end_user_allowance: i64,
    trace_content: bool,
    rate_limits: bool,
    rate_limit_rpm: i32,
    rate_limit_concurrent: i32,
    /// The organization policy's tokens per minute, if any (no lease is
    /// then taken for its scopes).
    rate_limit_tpm: Option<i32>,
    /// A policy on every key too: the same requests per minute and
    /// concurrency, and the tokens per minute if set (its requests are
    /// never leased).
    key_rate_limits: bool,
    manifest: CatalogManifest,
}

async fn budget(pool: &PgPool, org: Uuid, scope: &str, key: Option<Uuid>, limit: i64) {
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, scope_kind, api_key_id, period_start, period_end,
              currency, limit_minor)
         VALUES ($1, $2, $3, $4, now() - interval '1 day', now() + interval '30 days',
                 'USD', $5)",
    )
    .bind(Uuid::new_v4())
    .bind(org)
    .bind(scope)
    .bind(key)
    .bind(limit)
    .execute(pool)
    .await
    .expect("budget");
}

async fn tenant(
    pool: &PgPool,
    identity: &IdentityStore,
    name: String,
    limit: i64,
    tight: bool,
    options: &Options,
) -> Result<Tenant, String> {
    let fail = |what: &str, error: &dyn std::fmt::Debug| format!("{what}: {error:?}");
    let org = identity
        .create_organization(&name, "Load test")
        .await
        .map_err(|e| fail("organization", &e))?
        .id;
    let user = identity
        .create_user(org, None, None, None, Some("organization_owner"))
        .await
        .map_err(|e| fail("user", &e))?
        .id;
    identity
        .add_organization_membership(org, user, MembershipRole::Owner)
        .await
        .map_err(|e| fail("membership", &e))?;
    let account = identity
        .create_service_account(org, user, None, "load")
        .await
        .map_err(|e| fail("service account", &e))?
        .id;
    let mut keys = Vec::with_capacity(options.keys);
    for _ in 0..options.keys {
        let key = identity
            .create_api_key(
                org,
                account,
                &[ApiKeyScope::Inference, ApiKeyScope::Embeddings],
                None,
                user,
            )
            .await
            .map_err(|e| fail("API key", &e))?;
        if options.key_budgets {
            budget(pool, org.0, "api-key", Some(key.id.0), i64::MAX / 4).await;
        }
        if options.rate_limits && options.key_rate_limits {
            sqlx::query(
                "INSERT INTO gateway.rate_limit_policy
                     (id, organization_id, scope_kind, api_key_id, requests_per_minute,
                      max_concurrent, tokens_per_minute)
                 VALUES ($1, $2, 'api-key', $3, $4, $5, $6)",
            )
            .bind(Uuid::new_v4())
            .bind(org.0)
            .bind(key.id.0)
            .bind(options.rate_limit_rpm)
            .bind(options.rate_limit_concurrent)
            .bind(options.rate_limit_tpm)
            .execute(pool)
            .await
            .map_err(|e| fail("key rate limit", &e))?;
        }
        keys.push(Key {
            id: key.id.0,
            bearer: key.bearer,
        });
    }
    CatalogStore::new(pool.clone())
        .apply(org, &options.manifest, options.manifest.digest().unwrap())
        .await
        .map_err(|e| fail("catalog", &e))?;
    budget(pool, org.0, "organization", None, limit).await;
    if options.rate_limits {
        sqlx::query(
            "INSERT INTO gateway.rate_limit_policy
                 (id, organization_id, scope_kind, requests_per_minute, max_concurrent,
                  tokens_per_minute)
             VALUES ($1, $2, 'organization', $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(options.rate_limit_rpm)
        .bind(options.rate_limit_concurrent)
        .bind(options.rate_limit_tpm)
        .execute(pool)
        .await
        .map_err(|e| fail("rate limit", &e))?;
    }
    if options.end_users {
        let plan = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.end_user_setting
                 (service_account_id, organization_id, id, requirement, currency)
             VALUES ($1, $2, $3, 'optional', 'USD')",
        )
        .bind(account.0)
        .bind(org.0)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .map_err(|e| fail("end-user setting", &e))?;
        sqlx::query(
            "INSERT INTO gateway.end_user_plan
                 (id, organization_id, service_account_id, name, allowance_minor)
             VALUES ($1, $2, $3, 'load', $4)",
        )
        .bind(plan)
        .bind(org.0)
        .bind(account.0)
        .bind(if tight {
            options.tight_end_user_allowance
        } else {
            options.end_user_allowance
        })
        .execute(pool)
        .await
        .map_err(|e| fail("end-user plan", &e))?;
        sqlx::query(
            "UPDATE gateway.end_user_setting SET default_plan_id = $2 WHERE service_account_id = $1",
        )
        .bind(account.0)
        .bind(plan)
        .execute(pool)
        .await
        .map_err(|e| fail("default plan", &e))?;
    }
    if options.trace_content {
        sqlx::query(
            "INSERT INTO gateway.organization_trace_policy
                 (organization_id, id, mode, content, content_days)
             VALUES ($1, $2, 'organization', true, 1)",
        )
        .bind(org.0)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .map_err(|e| fail("trace policy", &e))?;
    }
    Ok(Tenant {
        org: org.0,
        name,
        service_account: account.0,
        keys,
        tight,
        limit_minor: limit,
    })
}

pub async fn run(flags: &Flags) -> Result<(), String> {
    let url = flags.required("database-url")?;
    let pepper = flags.required("pepper")?;
    let out = flags.string("tenants", "load-tenants.json");
    let orgs: usize = flags.parsed("orgs", 1)?;
    if orgs == 0 {
        return Err("--orgs must be at least 1".into());
    }
    let tight_orgs: usize = flags.parsed("tight-orgs", 1)?;
    let max_input_bytes: u64 = flags.parsed("max-input-bytes", 1_048_576)?;
    let max_output: u32 = flags.parsed("max-output-tokens", 4_096)?;
    let ttl: u64 = flags.parsed("reservation-ttl", 120)?;
    let options = Options {
        keys: match flags.parsed("keys", 10)? {
            0 => return Err("--keys must be at least 1".into()),
            keys => keys,
        },
        key_budgets: flags.parsed("key-budgets", false)?,
        end_users: flags.parsed("end-users", true)?,
        end_user_allowance: flags.parsed("end-user-allowance", 1_000_000_000_000)?,
        tight_end_user_allowance: flags.parsed("tight-end-user-allowance", 60_000)?,
        trace_content: flags.parsed("trace-content", false)?,
        rate_limits: flags.parsed("rate-limits", true)?,
        rate_limit_rpm: flags.parsed("rate-limit-rpm", 1_000_000)?,
        rate_limit_concurrent: flags.parsed("rate-limit-concurrent", 1_000_000)?,
        rate_limit_tpm: match flags.parsed("rate-limit-tpm", 0)? {
            0 => None,
            tpm => Some(tpm),
        },
        key_rate_limits: flags.parsed("key-rate-limits", false)?,
        manifest: manifest(max_input_bytes, max_output, ttl),
    };
    let limit: i64 = flags.parsed("budget", 1_000_000_000_000_000)?;
    let tight_limit: i64 = flags.parsed("tight-budget", 2_000_000)?;
    let run = flags.string(
        "run",
        &format!("load-{}", &Uuid::new_v4().simple().to_string()[..8]),
    );

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .map_err(|error| format!("database: {error}"))?;
    gateway::persistence::migrations::run(&pool)
        .await
        .map_err(|error| format!("migrations: {error}"))?;
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(pepper.as_bytes()).map_err(|_| "--pepper: too short")?,
    );
    let mut tenants = Vec::with_capacity(orgs + tight_orgs);
    for index in 0..orgs + tight_orgs {
        let tight = index >= orgs;
        let name = format!("{run}-{}{index:03}", if tight { "tight-" } else { "" });
        let budget = if tight { tight_limit } else { limit };
        tenants.push(tenant(&pool, &identity, name, budget, tight, &options).await?);
    }
    let file = Tenants {
        run: run.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        max_input_bytes,
        max_output_tokens: max_output,
        reservation_ttl_seconds: ttl,
        end_users: options.end_users,
        trace_content: options.trace_content,
        tenants,
    };
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&out)
        .map_err(|error| format!("{out}: {error}"))?;
    writer
        .write_all(serde_json::to_string_pretty(&file).unwrap().as_bytes())
        .map_err(|error| format!("{out}: {error}"))?;
    eprintln!(
        "run {run}: {orgs} organizations and {tight_orgs} tight, {} keys each; tenants in {out}",
        options.keys
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_load_catalog_parses_and_has_a_digest() {
        let manifest = super::manifest(262_144, 4_096, 120);
        assert!(manifest.digest().is_ok());
    }
}
