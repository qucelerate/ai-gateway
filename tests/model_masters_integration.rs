#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for master model records (Phase 13 milestones 1 and 2,
//! D-033): resolution from cards and the organization's links, its own
//! records, route proposals with their exclusions, and publishing one as a
//! release, driven through the real routers, RBAC and schema. The last test
//! routes an alias over unrelated models through a data plane.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set (and
//! `GATEWAY_TEST_DATA_PLANE_DATABASE_URL` for the routing test); tests are
//! skipped locally when it is absent (matching the rest of the integration
//! suite).

use std::{
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, StatusCode, header::COOKIE},
    response::Response,
    routing::post,
};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, OrganizationId,
    UserAccountId,
    domain::{
        auth::AuthCrypto,
        identity::{ApiKeyScope, MembershipRole},
        model_master::ProposedRoute,
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore, catalog_admin::CatalogAdminStore,
        model_cards::ModelCardStore, model_masters::ModelMasterStore,
        provider_credentials::ProviderCredentialStore, routing_policy::RoutingPolicyStore,
    },
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SESSION_COOKIE: &str = "__Host-gateway_session";
const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";
const TEST_PEPPER: &[u8] = b"model-masters-integration-pepper";
const HMAC_KEY: &[u8] = b"model-masters-integration-hmac-key-at-least-32";
const SONNET: &str = "anthropic/claude-sonnet-4-6";

fn test_database_url() -> Option<String> {
    match std::env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => Some(value),
        Err(_)
            if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() =>
        {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => None,
    }
}

async fn connect_test_pool(database_url: &str, max_connections: u32) -> PgPool {
    let mut last_error = None;
    for _ in 0..15 {
        match PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(StdDuration::from_secs(2))
            .connect(database_url)
            .await
        {
            Ok(pool) => return pool,
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(StdDuration::from_secs(1)).await;
            }
        }
    }
    panic!("GATEWAY_TEST_DATABASE_URL remained unreachable: {last_error:?}");
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
}

type Session = (String, String);

impl Harness {
    async fn new(database_url: &str) -> Self {
        Self::with_pool_size(database_url, 8).await
    }

    /// With a pool of `max_connections`: one, as
    /// `GATEWAY_DATABASE_MAX_CONNECTIONS=1` allows, proves a write never
    /// waits on a second connection while it holds its transaction.
    async fn with_pool_size(database_url: &str, max_connections: u32) -> Self {
        let pool = connect_test_pool(database_url, max_connections).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[19_u8; 32]).unwrap();
        let masters = gateway::transport::model_masters::router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            ModelCardStore::new(pool.clone()),
            ModelMasterStore::new(pool.clone()),
            CatalogAdminStore::new(pool.clone()),
            RoutingPolicyStore::new(pool.clone()),
            Arc::new(gateway::domain::model_card_files::CardLibrary::builtin().unwrap()),
        );
        let catalog = gateway::transport::catalog_admin::router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            CatalogAdminStore::new(pool.clone()),
            ProviderCredentialStore::new(pool.clone()),
            None,
        );
        Self {
            pool,
            identity,
            crypto,
            router: masters.merge(catalog),
        }
    }

    async fn org(&self) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("masters-{suffix}"), "Masters")
            .await
            .unwrap()
            .id
    }

    async fn user(&self, org: OrganizationId, role: &str) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some("Ada Admin"))
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, MembershipRole::Member)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, NULL, $4, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> Session {
        let browser = self.crypto.opaque_token().as_str().to_owned();
        let session = self
            .crypto
            .new_session(user, &browser, StdDuration::from_secs(3600));
        sqlx::query(
            "INSERT INTO gateway.session
                 (id, token_digest, user_id, browser_binding_digest, expires_at)
             VALUES ($1, $2, $3, $4, to_timestamp($5))",
        )
        .bind(session.id)
        .bind(&session.token_digest)
        .bind(user.0)
        .bind(&session.browser_binding_digest)
        .bind(session.expires_at as i64)
        .execute(&self.pool)
        .await
        .unwrap();
        (session.token.clone(), browser)
    }

    /// Sends a request; a write carries `if_match` and `key` (a fresh key
    /// when `None`).
    async fn send(
        &self,
        method: &str,
        org: OrganizationId,
        path: &str,
        session: &Session,
        write: Option<(&str, Option<&str>)>,
        body: Option<Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(format!("/api/admin/v1/organizations/{}/{path}", org.0))
            .header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            );
        if let Some((if_match, key)) = write {
            builder = builder.header("if-match", if_match).header(
                "idempotency-key",
                key.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned),
            );
        }
        let body = match body {
            Some(body) => {
                builder = builder.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    async fn list(&self, org: OrganizationId, session: &Session) -> Value {
        let (status, body, _) = self
            .send("GET", org, "catalog/model-masters", session, None, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn count(&self, sql: &str, org: OrganizationId) -> i64 {
        sqlx::query_scalar(sql)
            .bind(org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn evidence(
        &self,
        org: OrganizationId,
        account: &str,
        family: &str,
        model: &str,
        state: &str,
    ) {
        sqlx::query(
            "INSERT INTO gateway.catalog_model_evidence
                 (organization_id, adapter_instance_key, family, model, state, reason, source,
                  deployment, observed_at, verified_at, refused_since)
             VALUES ($1, $2, $3, $4, $5, $6, 'traffic', NULL, now(), NULL,
                     CASE WHEN $5 = 'refused' THEN now() END)",
        )
        .bind(org.0)
        .bind(account)
        .bind(family)
        .bind(model)
        .bind(state)
        .bind("access_denied")
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

fn capability(operation: &str, tools: bool) -> Value {
    json!({"operation": operation, "max_input_bytes": 4096, "max_output_tokens": 32,
           "max_tokens_per_byte": 1, "tokenizer": "configured",
           "reservation_ttl_seconds": 30, "chat_input_count_mode": "unsupported",
           "tools": tools})
}

fn price(deployment: &str, operation: &str, currency: &str) -> Value {
    json!({"deployment": deployment, "operation": operation,
           "input_minor_per_token": 3, "output_minor_per_token": 15,
           "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
           "other_minor_per_token": 0, "currency": currency, "source": "test",
           "source_version": "v1", "confidence": "known"})
}

fn deployment(name: &str, model: &str, account: &str, location: &str, tools: bool) -> Value {
    json!({"name": name, "model": model, "location": location, "adapter_instance_key": account,
           "capabilities": [capability("chat", tools)]})
}

/// Claude Sonnet 4.6 on Anthropic, two Bedrock inference profiles and
/// Foundry (priced in EUR); one more Bedrock profile unpriced and one in a
/// location the routing policy will refuse; Titan embeddings; a Gemini model
/// (its own card's, to be linked to the Sonnet card for its tool call ids);
/// and a
/// model of the organization's own servers no card covers. The alias
/// `claude-sonnet-4-6` already routes to Anthropic at weight 2.
fn catalog() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "masters-1"},
        "providers": [
            {"adapter_instance_key": "anthropic-main", "family": "anthropic"},
            {"adapter_instance_key": "bedrock-main", "family": "bedrock"},
            {"adapter_instance_key": "foundry-main", "family": "azure-ai-foundry"},
            {"adapter_instance_key": "gemini-main", "family": "gemini"},
            {"adapter_instance_key": "local", "family": "openai-compatible"}
        ],
        "deployments": [
            deployment("anthropic-sonnet", "claude-sonnet-4-6", "anthropic-main", "us", true),
            deployment("bedrock-sonnet-us", "us.anthropic.claude-sonnet-4-6", "bedrock-main", "us-east-1", true),
            deployment("bedrock-sonnet-global", "global.anthropic.claude-sonnet-4-6", "bedrock-main", "us-east-1", true),
            deployment("bedrock-sonnet-eu", "eu.anthropic.claude-sonnet-4-6", "bedrock-main", "eu-central-1", true),
            deployment("bedrock-sonnet-base", "anthropic.claude-sonnet-4-6", "bedrock-main", "us-east-1", true),
            deployment("foundry-sonnet", "claude-sonnet-4-6", "foundry-main", "us", true),
            deployment("gemini-argon", "gemini-4-argon", "gemini-main", "us", true),
            {"name": "bedrock-titan", "model": "amazon.titan-embed-text-v2:0", "location": "us-east-1",
             "adapter_instance_key": "bedrock-main", "capabilities": [capability("embeddings", false)]},
            deployment("local-qwen", "qwen-local", "local", "us", false)
        ],
        "prices": [
            price("anthropic-sonnet", "chat", "USD"),
            price("bedrock-sonnet-us", "chat", "USD"),
            price("bedrock-sonnet-global", "chat", "USD"),
            price("bedrock-sonnet-eu", "chat", "USD"),
            price("foundry-sonnet", "chat", "EUR"),
            price("gemini-argon", "chat", "USD"),
            price("bedrock-titan", "embeddings", "USD"),
            price("local-qwen", "chat", "USD")
        ],
        "routes": [
            {"alias": "claude-sonnet-4-6", "operation": "chat", "deployment": "anthropic-sonnet", "weight": 2},
            {"alias": "qwen", "operation": "chat", "deployment": "local-qwen"},
            {"alias": "titan", "operation": "embeddings", "deployment": "bedrock-titan"}
        ]
    }))
    .unwrap()
}

async fn seeded(harness: &Harness) -> (OrganizationId, Session, Session) {
    let org = harness.org().await;
    let owner = harness.user(org, "organization_owner").await;
    let auditor = harness.user(org, "auditor").await;
    let manifest = catalog();
    CatalogStore::new(harness.pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    (
        org,
        harness.session(owner).await,
        harness.session(auditor).await,
    )
}

fn item<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == id)
        .unwrap_or_else(|| panic!("no master {id}: {list}"))
}

fn names(deployments: &Value) -> Vec<String> {
    let mut names: Vec<String> = deployments
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["deployment"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn deployments_resolve_to_cards_and_the_organizations_links_and_records() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    // One connection: every link and record write runs its reads in its
    // own transaction.
    let harness = Harness::with_pool_size(&database_url, 1).await;
    let (org, owner, auditor) = seeded(&harness).await;
    let configs = "SELECT count(*) FROM gateway.config_version WHERE organization_id = $1";
    let config_versions = harness.count(configs, org).await;

    // Cards match by exact identifiers; nothing else is guessed.
    let list = harness.list(org, &auditor).await;
    let sonnet = item(&list, SONNET);
    assert_eq!(sonnet["kind"], "card");
    assert_eq!(
        names(&sonnet["deployments"]),
        [
            "anthropic-sonnet",
            "bedrock-sonnet-base",
            "bedrock-sonnet-eu",
            "bedrock-sonnet-global",
            "bedrock-sonnet-us",
            "foundry-sonnet"
        ]
    );
    let anthropic = sonnet["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deployment"] == "anthropic-sonnet")
        .unwrap();
    assert_eq!(anthropic["resolved_by"], "card");
    assert_eq!(anthropic["link"], Value::Null);
    assert_eq!(anthropic["adapter_instance_key"], "anthropic-main");
    assert_eq!(anthropic["operations"][0]["tools"], true);
    assert_eq!(anthropic["operations"][0]["price"]["currency"], "USD");
    assert_eq!(
        anthropic["routes"],
        json!([{"alias": "claude-sonnet-4-6", "operation": "chat"}])
    );
    assert_eq!(
        names(&item(&list, "amazon/titan-embed-text-v2")["deployments"]),
        ["bedrock-titan"]
    );
    assert_eq!(
        item(&list, "amazon/titan-embed-text-v2")["modality"],
        "embeddings"
    );
    assert_eq!(
        names(&item(&list, "google/gemini-4-argon")["deployments"]),
        ["gemini-argon"]
    );
    assert_eq!(names(&list["unlinked"]), ["local-qwen"]);
    assert_eq!(list["links"], json!([]));
    // The alias routes Anthropic only: the other priced Sonnet deployments
    // are suggested for it (not the unpriced base profile); nothing else
    // is routed by a master's alias, so nothing else is suggested.
    let suggested = |list: &Value, id: &str| -> Vec<String> {
        item(list, id)["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                format!(
                    "{} {} {}",
                    s["alias"].as_str().unwrap(),
                    s["operation"].as_str().unwrap(),
                    s["deployment"].as_str().unwrap()
                )
            })
            .collect()
    };
    assert_eq!(
        suggested(&list, SONNET),
        [
            "claude-sonnet-4-6 chat bedrock-sonnet-eu",
            "claude-sonnet-4-6 chat bedrock-sonnet-global",
            "claude-sonnet-4-6 chat bedrock-sonnet-us",
            "claude-sonnet-4-6 chat foundry-sonnet"
        ]
    );
    assert!(suggested(&list, "amazon/titan-embed-text-v2").is_empty());
    let (status, single, _) = harness
        .send(
            "GET",
            org,
            "catalog/model-masters/anthropic%2Fclaude-sonnet-4-6",
            &auditor,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{single}");
    assert_eq!(single["suggestions"], item(&list, SONNET)["suggestions"]);

    // Writes need catalog_write.
    let (status, _, _) = harness
        .send(
            "PUT",
            org,
            "catalog/model-masters/org%2Fqwen",
            &auditor,
            Some(("\"v0\"", None)),
            Some(json!({"name": "Qwen on our servers"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An organization record for the model no card covers.
    let key = Uuid::new_v4().to_string();
    let create = |key: String| {
        let harness = &harness;
        let owner = &owner;
        async move {
            harness
                .send(
                    "PUT",
                    org,
                    "catalog/model-masters/org%2Fqwen",
                    owner,
                    Some(("\"v0\"", Some(&key))),
                    Some(json!({"name": "  Qwen on our servers "})),
                )
                .await
        }
    };
    let (status, record, headers) = create(key.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{record}");
    assert_eq!(record["id"], "org/qwen");
    assert_eq!(record["name"], "Qwen on our servers");
    assert_eq!(record["modality"], "chat");
    assert_eq!(record["version"], 1);
    assert_eq!(headers["etag"], "\"v1\"");
    // A retry replays it.
    let (status, replayed, _) = create(key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed, record);
    // Creating it again where it exists conflicts.
    let (status, body, _) = create(Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    // Card ids are not written here, and slugs are checked.
    for path in ["anthropic%2Fclaude-x", "org%2FQwen", "qwen"] {
        let (status, body, _) = harness
            .send(
                "PUT",
                org,
                &format!("catalog/model-masters/{path}"),
                &owner,
                Some(("\"v0\"", None)),
                Some(json!({"name": "x"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["code"], "invalid_master");
    }

    // Link the local model to it.
    let link = "catalog/model-master-links/openai-compatible/qwen-local";
    let (status, body, headers) = harness
        .send(
            "PUT",
            org,
            link,
            &owner,
            Some(("\"v0\"", None)),
            Some(json!({"master": "org/qwen"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["master"], "org/qwen");
    assert_eq!(body["version"], 1);
    assert_eq!(headers["etag"], "\"v1\"");
    let (status, body, _) = harness.send("GET", org, link, &auditor, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["master"], "org/qwen");

    // A body without `master` is refused, not read as a link to none.
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            link,
            &owner,
            Some(("\"v1\"", None)),
            Some(json!({})),
        )
        .await;
    assert!(status.is_client_error(), "{status}: {body}");
    let (_, body, _) = harness.send("GET", org, link, &auditor, None, None).await;
    assert_eq!(
        (body["master"].clone(), body["version"].clone()),
        (json!("org/qwen"), json!(1))
    );

    // Detach Foundry's deployment from the card its identifier matches.
    let foundry = "catalog/model-master-links/azure-ai-foundry/claude-sonnet-4-6";
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            foundry,
            &owner,
            Some(("\"v0\"", None)),
            Some(json!({"master": null})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let list = harness.list(org, &owner).await;
    assert_eq!(
        names(&item(&list, "org/qwen")["deployments"]),
        ["local-qwen"]
    );
    assert_eq!(
        item(&list, "org/qwen")["deployments"][0]["resolved_by"],
        "link"
    );
    assert!(!names(&item(&list, SONNET)["deployments"]).contains(&"foundry-sonnet".to_owned()));
    let unlinked = list["unlinked"].as_array().unwrap();
    let detached = unlinked
        .iter()
        .find(|d| d["deployment"] == "foundry-sonnet")
        .unwrap();
    assert_eq!(detached["resolved_by"], "link");
    assert_eq!(detached["card"], SONNET);
    assert_eq!(list["links"].as_array().unwrap().len(), 2);

    // A stale version conflicts; the current one changes it.
    let (status, _, _) = harness
        .send(
            "PUT",
            org,
            foundry,
            &owner,
            Some(("\"v0\"", None)),
            Some(json!({"master": SONNET})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            foundry,
            &owner,
            Some(("\"v1\"", None)),
            Some(json!({"master": SONNET})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 2);

    // Cleared, the card decides again, and reading the one record keeps the
    // cleared link's version, as the list does, to set it again at.
    let (status, _, _) = harness
        .send("DELETE", org, foundry, &owner, Some(("\"v2\"", None)), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, one, _) = harness
        .send(
            "GET",
            org,
            "catalog/model-masters/anthropic%2Fclaude-sonnet-4-6",
            &auditor,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    let foundry_sonnet = one["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deployment"] == "foundry-sonnet")
        .expect("the card matches it again");
    assert_eq!(foundry_sonnet["resolved_by"], "card");
    assert_eq!(
        foundry_sonnet["link"],
        json!({"master": null, "version": 3, "cleared": true})
    );
    // Set again at that version, as before the clear.
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            foundry,
            &owner,
            Some(("\"v3\"", None)),
            Some(json!({"master": SONNET})),
        )
        .await;
    // Set again, a cleared link is created anew.
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["version"], 4);

    // A chat deployment never links to an embedding model's record, nor to
    // a master that does not exist or a model the release does not serve.
    for (path, master, status, code) in [
        (
            "catalog/model-master-links/bedrock/us.anthropic.claude-sonnet-4-6",
            json!("amazon/titan-embed-text-v2"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "modality_mismatch",
        ),
        (
            "catalog/model-master-links/bedrock/amazon.titan-embed-text-v2%3A0",
            json!("org/qwen"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "modality_mismatch",
        ),
        (
            "catalog/model-master-links/bedrock/us.anthropic.claude-sonnet-4-6",
            json!("anthropic/claude-nonexistent"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_master",
        ),
        (
            "catalog/model-master-links/bedrock/us.anthropic.claude-sonnet-4-6",
            json!("org/nobody"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_master",
        ),
        (
            "catalog/model-master-links/anthropic/us.anthropic.claude-sonnet-4-6",
            json!(SONNET),
            StatusCode::NOT_FOUND,
            "deployment_not_found",
        ),
        (
            "catalog/model-master-links/nobody/claude-sonnet-4-6",
            json!(SONNET),
            StatusCode::BAD_REQUEST,
            "invalid_family",
        ),
        (
            "catalog/model-master-links/bedrock/us.anthropic.claude-sonnet-4-6",
            json!("Not An Id"),
            StatusCode::BAD_REQUEST,
            "invalid_master",
        ),
    ] {
        let (got, body, _) = harness
            .send(
                "PUT",
                org,
                path,
                &owner,
                Some(("\"v0\"", None)),
                Some(json!({ "master": master })),
            )
            .await;
        assert_eq!(
            (got, body["code"].as_str()),
            (status, Some(code)),
            "{path} {master}: {body}"
        );
    }

    // A record links name can be neither deleted nor change modality.
    let (status, body, _) = harness
        .send(
            "DELETE",
            org,
            "catalog/model-masters/org%2Fqwen",
            &owner,
            Some(("\"v1\"", None)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "master_in_use");
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            "catalog/model-masters/org%2Fqwen",
            &owner,
            Some(("\"v1\"", None)),
            Some(json!({"name": "Qwen", "modality": "embeddings"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "master_in_use");

    // Clearing the link lets the card decide again (none here), softly.
    let (status, _, _) = harness
        .send("DELETE", org, link, &owner, Some(("\"v1\"", None)), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = harness
        .send("DELETE", org, link, &owner, Some(("*", None)), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A cleared link is kept and still read, with the version to set it
    // again at; the card decides meanwhile.
    let (status, cleared, headers) = harness.send("GET", org, link, &owner, None, None).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["cleared"], true);
    assert_eq!(cleared["master"], Value::Null);
    assert_eq!(cleared["version"], 2);
    assert_eq!(headers["etag"], "\"v2\"");
    let list = harness.list(org, &owner).await;
    let listed = list["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["model"] == "qwen-local")
        .unwrap();
    assert_eq!(
        (listed["cleared"].clone(), listed["version"].clone()),
        (json!(true), json!(2))
    );
    let qwen = list["unlinked"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deployment"] == "local-qwen")
        .unwrap();
    assert_eq!(qwen["resolved_by"], "none");
    assert_eq!(
        qwen["link"],
        json!({"master": null, "version": 2, "cleared": true})
    );
    // Set again: "v0" says none was ever set, so it conflicts; the cleared
    // link's version sets it.
    let set_again = |if_match: &'static str| {
        let harness = &harness;
        let owner = &owner;
        async move {
            harness
                .send(
                    "PUT",
                    org,
                    link,
                    owner,
                    Some((if_match, None)),
                    Some(json!({"master": "org/qwen"})),
                )
                .await
        }
    };
    let (status, body, _) = set_again("\"v0\"").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body, _) = set_again("\"v2\"").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        (body["version"].clone(), body["cleared"].clone()),
        (json!(3), json!(false))
    );
    assert_eq!(
        names(&item(&harness.list(org, &owner).await, "org/qwen")["deployments"]),
        ["local-qwen"]
    );
    let (status, _, _) = harness
        .send("DELETE", org, link, &owner, Some(("\"v3\"", None)), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = harness
        .send(
            "DELETE",
            org,
            "catalog/model-masters/org%2Fqwen",
            &owner,
            Some(("\"v1\"", None)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = harness
        .send(
            "GET",
            org,
            "catalog/model-masters/org%2Fqwen",
            &owner,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let list = harness.list(org, &owner).await;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["id"] != "org/qwen")
    );
    assert!(names(&list["unlinked"]).contains(&"local-qwen".to_owned()));
    // The list keeps the deleted record with the version to create it again
    // at; "v0" (none ever made) conflicts, that version creates it.
    let deleted = &list["deleted_records"][0];
    assert_eq!(
        (
            deleted["id"].clone(),
            deleted["deleted"].clone(),
            deleted["version"].clone()
        ),
        (json!("org/qwen"), json!(true), json!(2))
    );
    let recreate = |if_match: String| {
        let harness = &harness;
        let owner = &owner;
        async move {
            harness
                .send(
                    "PUT",
                    org,
                    "catalog/model-masters/org%2Fqwen",
                    owner,
                    Some((&if_match, None)),
                    Some(json!({"name": "Qwen", "modality": "chat"})),
                )
                .await
        }
    };
    let (status, body, _) = recreate("\"v0\"".into()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, record, _) = recreate(format!("\"v{}\"", deleted["version"])).await;
    assert_eq!(status, StatusCode::CREATED, "{record}");
    assert_eq!(
        (record["version"].clone(), record["deleted"].clone()),
        (json!(3), json!(false))
    );
    assert_eq!(
        harness.list(org, &owner).await["deleted_records"],
        json!([])
    );

    // Nothing was hard-deleted, every write was audited, and links changed
    // no configuration version: data planes never see them.
    assert_eq!(
        harness
            .count(
                "SELECT count(*) FROM gateway.model_master_link WHERE organization_id = $1",
                org
            )
            .await,
        2
    );
    assert_eq!(
        harness
            .count(
                "SELECT count(*) FROM gateway.model_master_link_revision WHERE organization_id = $1",
                org
            )
            .await,
        8
    );
    assert_eq!(
        harness
            .count(
                "SELECT count(*) FROM gateway.model_master_revision WHERE organization_id = $1",
                org
            )
            .await,
        3
    );
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type IN ('model_master', 'model_master_link')
         ORDER BY occurred_at, action",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    for action in [
        "model_master.set",
        "model_master.delete",
        "model_master_link.set",
        "model_master_link.clear",
    ] {
        assert!(actions.iter().any(|a| a == action), "{action}: {actions:?}");
    }
    assert_eq!(harness.count(configs, org).await, config_versions);
}

#[tokio::test]
async fn writes_replay_by_idempotency_key_and_need_catalog_write() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, owner, auditor) = seeded(&harness).await;
    let link = "catalog/model-master-links/bedrock/us.anthropic.claude-sonnet-4-6";
    let record = "catalog/model-masters/org%2Fspare";
    let count = |sql: &'static str| harness.count(sql, org);
    let link_revisions =
        "SELECT count(*) FROM gateway.model_master_link_revision WHERE organization_id = $1";
    let record_revisions =
        "SELECT count(*) FROM gateway.model_master_revision WHERE organization_id = $1";

    // Without catalog_write, nothing is written (deletes are tried below,
    // once there is something to delete).
    for (method, path, if_match, body) in [
        ("PUT", link, "\"v0\"", Some(json!({"master": null}))),
        ("PUT", record, "\"v0\"", Some(json!({"name": "Spare"}))),
    ] {
        let (status, body, _) = harness
            .send(method, org, path, &auditor, Some((if_match, None)), body)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }
    assert_eq!(count(link_revisions).await, 0);
    assert_eq!(count(record_revisions).await, 0);

    // A retried link write replays, even once the release no longer serves
    // the model (a fresh write would be refused).
    let key = Uuid::new_v4().to_string();
    let set = || async {
        harness
            .send(
                "PUT",
                org,
                link,
                &owner,
                Some(("\"v0\"", Some(&key))),
                Some(json!({"master": null})),
            )
            .await
    };
    let (status, first, _) = set().await;
    assert_eq!(status, StatusCode::CREATED, "{first}");

    let (status, body, _) = harness
        .send("DELETE", org, link, &auditor, Some(("*", None)), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let mut manifest = catalog();
    manifest.release.version = "masters-2 without the us profile".into();
    manifest
        .deployments
        .retain(|deployment| deployment.name != "bedrock-sonnet-us");
    manifest
        .prices
        .retain(|price| price.deployment != "bedrock-sonnet-us");
    CatalogStore::new(harness.pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    let (status, replayed, _) = set().await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed, first);
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            link,
            &owner,
            Some(("\"v1\"", None)),
            Some(json!({"master": SONNET})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "deployment_not_found");
    assert_eq!(count(link_revisions).await, 1);

    // A retried clear replays rather than finding nothing to clear.
    let key = Uuid::new_v4().to_string();
    for expected in [StatusCode::NO_CONTENT, StatusCode::NO_CONTENT] {
        let (status, body, _) = harness
            .send(
                "DELETE",
                org,
                link,
                &owner,
                Some(("\"v1\"", Some(&key))),
                None,
            )
            .await;
        assert_eq!(status, expected, "{body}");
    }
    assert_eq!(count(link_revisions).await, 2);

    // So does a retried record delete.
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            record,
            &owner,
            Some(("\"v0\"", None)),
            Some(json!({"name": "Spare"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body, _) = harness
        .send("DELETE", org, record, &auditor, Some(("*", None)), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let key = Uuid::new_v4().to_string();
    for _ in 0..2 {
        let (status, body, _) = harness
            .send(
                "DELETE",
                org,
                record,
                &owner,
                Some(("\"v1\"", Some(&key))),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert_eq!(count(record_revisions).await, 2);
    let deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND outcome = 'succeeded'
           AND action IN ('model_master.delete', 'model_master_link.clear')",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(deletes, 2);
    // Each refused write is audited as denied.
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND outcome = 'denied'
           AND entity_type IN ('model_master', 'model_master_link')",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(denied, 4);
}

fn routes(proposal: &Value) -> Vec<(String, String)> {
    proposal["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["operation"].as_str().unwrap().to_owned(),
                r["deployment"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn exclusions(proposal: &Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = proposal["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["deployment"].as_str().unwrap().to_owned(),
                e["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    found.sort();
    found
}

#[tokio::test]
async fn a_route_proposal_explains_its_exclusions_and_publishes_as_a_release() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, owner, auditor) = seeded(&harness).await;
    // The Gemini model is linked to the Sonnet card, as an organization may
    // link anything of the card's modality.
    let (status, body, _) = harness
        .send(
            "PUT",
            org,
            "catalog/model-master-links/gemini/gemini-4-argon",
            &owner,
            Some(("\"v0\"", None)),
            Some(json!({"master": SONNET})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    harness
        .evidence(
            org,
            "bedrock-main",
            "bedrock",
            "global.anthropic.claude-sonnet-4-6",
            "requires_action",
        )
        .await;
    sqlx::query(
        "INSERT INTO gateway.routing_policy (organization_id, id, providers, locations)
         VALUES ($1, $2, NULL, ARRAY['us', 'us-east-1'])",
    )
    .bind(org.0)
    .bind(Uuid::new_v4())
    .execute(&harness.pool)
    .await
    .unwrap();

    let proposal_path = "catalog/model-masters/anthropic%2Fclaude-sonnet-4-6/route-proposal";
    let (status, proposal, _) = harness
        .send("GET", org, proposal_path, &auditor, None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{proposal}");
    assert_eq!(proposal["alias"], "claude-sonnet-4-6");
    assert_eq!(proposal["operations"], json!(["chat"]));
    assert_eq!(
        routes(&proposal),
        [
            ("chat".to_owned(), "anthropic-sonnet".to_owned()),
            ("chat".to_owned(), "bedrock-sonnet-us".to_owned()),
            ("chat".to_owned(), "gemini-argon".to_owned()),
        ]
    );
    assert!(
        proposal["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["weight"] == 1 && r["priority"] == 1)
    );
    assert_eq!(
        exclusions(&proposal),
        [
            ("bedrock-sonnet-base".to_owned(), "no_price".to_owned()),
            (
                "bedrock-sonnet-eu".to_owned(),
                "location_not_permitted".to_owned()
            ),
            (
                "bedrock-sonnet-global".to_owned(),
                "availability_requires_action".to_owned()
            ),
            ("foundry-sonnet".to_owned(), "currency".to_owned()),
        ]
    );
    assert_eq!(proposal["problem"], Value::Null);
    // The existing alias is a diff, never overwritten silently.
    assert_eq!(proposal["alias_exists"], true);
    let changes: Vec<(String, String)> = proposal["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["deployment"].as_str().unwrap().to_owned(),
                c["change"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("anthropic-sonnet".to_owned(), "changed".to_owned()),
            ("bedrock-sonnet-us".to_owned(), "added".to_owned()),
            ("gemini-argon".to_owned(), "added".to_owned()),
        ]
    );

    // With tools, Gemini's call ids would mix with the others'.
    let (status, with_tools, _) = harness
        .send(
            "GET",
            org,
            &format!("{proposal_path}?tools=true"),
            &auditor,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{with_tools}");
    assert_eq!(
        routes(&with_tools),
        [
            ("chat".to_owned(), "anthropic-sonnet".to_owned()),
            ("chat".to_owned(), "bedrock-sonnet-us".to_owned()),
        ]
    );
    assert!(
        exclusions(&with_tools)
            .contains(&("gemini-argon".to_owned(), "mixed_tool_call_ids".to_owned()))
    );

    // Another alias is new; an operation the master's deployments do not
    // serve proposes nothing; streaming is not an operation of its own.
    let (_, fresh, _) = harness
        .send(
            "GET",
            org,
            &format!("{proposal_path}?alias=sonnet&operations=chat,embeddings"),
            &auditor,
            None,
            None,
        )
        .await;
    assert_eq!(fresh["alias_exists"], false);
    assert_eq!(fresh["operations"], json!(["chat", "embeddings"]));
    assert!(
        fresh["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["change"] == "added")
    );
    for query in [
        "operations=chat_stream",
        "operations=",
        "operations=nope",
        "colour=red",
    ] {
        let (status, body, _) = harness
            .send(
                "GET",
                org,
                &format!("{proposal_path}?{query}"),
                &auditor,
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    let (status, _, _) = harness
        .send(
            "GET",
            org,
            "catalog/model-masters/org%2Fnobody/route-proposal",
            &auditor,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Nothing routes differently until a release with it is published.
    let routed = |document: Value| -> Vec<(String, Value)> {
        let mut found: Vec<(String, Value)> = document["routes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["alias"] == "claude-sonnet-4-6")
            .map(|r| {
                (
                    r["deployment"].as_str().unwrap().to_owned(),
                    r["weight"].clone(),
                )
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    };
    let latest = || async {
        sqlx::query_scalar::<_, Value>(
            "SELECT document FROM gateway.config_version
             WHERE organization_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(org.0)
        .fetch_one(&harness.pool)
        .await
        .unwrap()
    };
    assert_eq!(
        routed(latest().await),
        [("anthropic-sonnet".to_owned(), json!(2))]
    );

    // Publish it as the console does: the active manifest with the alias's
    // routes of these operations replaced, created, then activated.
    let release_id = proposal["release_id"].as_str().unwrap();
    let (status, mut manifest, _) = harness
        .send(
            "GET",
            org,
            &format!("catalog/releases/{release_id}/manifest"),
            &owner,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{manifest}");
    manifest["release"]["version"] = json!("masters-2 route across providers");
    let mut manifest: CatalogManifest = serde_json::from_value(manifest).unwrap();
    let proposed: Vec<ProposedRoute> = serde_json::from_value(proposal["routes"].clone()).unwrap();
    gateway::application::model_masters::apply_routes(
        &mut manifest,
        "claude-sonnet-4-6",
        &["chat".to_owned()],
        &proposed,
    );
    let (status, release, _) = harness
        .send(
            "POST",
            org,
            "catalog/releases",
            &owner,
            Some(("*", None)),
            Some(serde_json::to_value(&manifest).unwrap()),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{release}");
    let (status, body, _) = harness
        .send(
            "POST",
            org,
            &format!(
                "catalog/releases/{}/activate",
                release["id"].as_str().unwrap()
            ),
            &owner,
            Some(("*", None)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        routed(latest().await),
        [
            ("anthropic-sonnet".to_owned(), Value::Null),
            ("bedrock-sonnet-us".to_owned(), Value::Null),
            ("gemini-argon".to_owned(), Value::Null),
        ]
    );
    // Proposed again, the alias is what the proposal said.
    let (_, again, _) = harness
        .send("GET", org, proposal_path, &auditor, None, None)
        .await;
    assert!(
        again["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["change"] == "unchanged")
    );
    // A price override in another currency than the route's takes its
    // deployment out (as configuration versions do while it is in effect),
    // and the route's currency is the release's, as validation checks it:
    // Foundry, released in EUR, stays out though overridden in USD.
    for (deployment, currency) in [("bedrock-sonnet-us", "EUR"), ("foundry-sonnet", "USD")] {
        sqlx::query(
            "INSERT INTO gateway.catalog_price_override
                 (id, organization_id, deployment, operation, currency, input_minor_per_token,
                  output_minor_per_token, cached_input_minor_per_token, reasoning_minor_per_token,
                  other_minor_per_token, version, confidence, reason, effective_from, created_by)
             VALUES ($1, $2, $3, 'chat', $4, 1, 1, 0, 0, 0, 'override:' || $1::text,
                     'known', 'contract', now() - interval '1 minute',
                     (SELECT user_id FROM gateway.admin_role_binding
                      WHERE organization_id = $2 LIMIT 1))",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(deployment)
        .bind(currency)
        .execute(&harness.pool)
        .await
        .unwrap();
    }
    let (_, overridden, _) = harness
        .send("GET", org, proposal_path, &auditor, None, None)
        .await;
    assert_eq!(
        routes(&overridden),
        [
            ("chat".to_owned(), "anthropic-sonnet".to_owned()),
            ("chat".to_owned(), "gemini-argon".to_owned()),
        ]
    );
    let excluded = overridden["excluded"].as_array().unwrap();
    let reason = |name: &str| {
        excluded
            .iter()
            .find(|e| e["deployment"] == name)
            .map(|e| {
                (
                    e["reason"].clone(),
                    e["detail"].as_str().unwrap().to_owned(),
                )
            })
            .unwrap()
    };
    let (code, detail) = reason("bedrock-sonnet-us");
    assert_eq!(code, "currency");
    assert!(detail.contains("override in EUR"), "{detail}");
    assert_eq!(reason("foundry-sonnet").0, "currency");
    assert_eq!(overridden["problem"], Value::Null);
}

// An alias over unrelated models (GPT and Claude, of different masters)
// publishes and routes: aliases stay free-form (D-033).

const PEPPER: &str = "masters-test-pepper-0000000000000000";

fn json_response(value: Value) -> Response {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

async fn openai(Json(body): Json<Value>) -> Response {
    json_response(json!({
        "id": "c", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "from gpt"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3},
    }))
}

async fn anthropic(Json(body): Json<Value>) -> Response {
    json_response(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": body["model"],
        "content": [{"type": "text", "text": "from claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3},
    }))
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

#[tokio::test]
async fn an_alias_over_unrelated_models_publishes_and_routes() {
    let (Ok(app_url), Ok(data_plane_url)) = (
        std::env::var("GATEWAY_TEST_DATABASE_URL"),
        std::env::var("GATEWAY_TEST_DATA_PLANE_DATABASE_URL"),
    ) else {
        if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() {
            panic!(
                "GATEWAY_TEST_DATABASE_URL and GATEWAY_TEST_DATA_PLANE_DATABASE_URL must be set in CI"
            );
        }
        return;
    };
    let pool = connect_test_pool(&app_url, 8).await;
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("mixed-{suffix}"), "Mixed models")
        .await
        .unwrap()
        .id;
    let user = identity
        .create_user(org, None, None, None, Some("Owner"))
        .await
        .unwrap()
        .id;
    identity
        .add_organization_membership(org, user, MembershipRole::Owner)
        .await
        .unwrap();
    let account = identity
        .create_service_account(org, user, None, "mixed")
        .await
        .unwrap()
        .id;
    let bearer = identity
        .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
        .await
        .unwrap()
        .bearer;
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, scope_kind, period_start, period_end, currency, limit_minor)
         VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '1 day',
                 'USD', 1000000000000)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, requests_per_minute, max_concurrent)
         VALUES ($1, $2, 'organization', 100000, 50)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    // GPT on OpenAI and Claude on Anthropic: two cards, two masters, one
    // alias. The manifest's own validation decides whether it publishes.
    let manifest = CatalogManifest::parse(
        json!({
            "release": {"version": "mixed-1"},
            "providers": [
                {"adapter_instance_key": "openai-main", "family": "openai"},
                {"adapter_instance_key": "anthropic-main", "family": "anthropic"}
            ],
            "deployments": [
                deployment("gpt-luna", "gpt-6-luna", "openai-main", "us", false),
                deployment("claude-sonnet", "claude-sonnet-4-6", "anthropic-main", "us", false)
            ],
            "prices": [price("gpt-luna", "chat", "USD"), price("claude-sonnet", "chat", "USD")],
            "routes": [
                {"alias": "mixed", "operation": "chat", "deployment": "gpt-luna"},
                {"alias": "mixed", "operation": "chat", "deployment": "claude-sonnet"}
            ]
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
    .0;
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    // The two deployments have different masters.
    let library = gateway::domain::model_card_files::CardLibrary::builtin().unwrap();
    assert_eq!(
        library.card("openai", "gpt-6-luna").unwrap().0.id,
        "openai/gpt-6-luna"
    );
    assert_eq!(
        library.card("anthropic", "claude-sonnet-4-6").unwrap().0.id,
        SONNET
    );
    // The API stores the cards' matches at start-up; this data plane
    // stores none.
    ModelMasterStore::new(pool.clone())
        .sync_card_matches(&library)
        .await
        .unwrap();

    let openai_base = serve(Router::new().route("/v1/chat/completions", post(openai))).await;
    let anthropic_base = serve(Router::new().route("/v1/messages", post(anthropic))).await;
    let vars = [
        ("DATABASE_URL", data_plane_url),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-mixed".to_owned()),
        ("OPENAI_BASE_URL", format!("{openai_base}/v1")),
        ("ANTHROPIC_API_KEY", "anthropic-mixed".to_owned()),
        ("ANTHROPIC_BASE_URL", anthropic_base),
        (
            "GATEWAY_INSTANCE_ID",
            format!("masters-test-{}", Uuid::new_v4().simple()),
        ),
        ("GATEWAY_CONFIG_POLL_SECONDS", "1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(Arc::new(
            gateway::application::trace_objects::MemoryTraceObjects::default(),
        )));
    let base = serve(gateway::transport::data_plane_router(state)).await;
    let client = reqwest::Client::new();
    let mut answers = std::collections::BTreeSet::new();
    for _ in 0..40 {
        let response = client
            .post(format!("{base}/api/inference/v1/chat/completions"))
            .bearer_auth(&bearer)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .json(
                &json!({"model": "mixed", "messages": [{"role": "user", "content": "hi"}],
                          "max_tokens": 8}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        answers.insert(
            body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    // Forty requests at equal weights reach both with odds of missing one
    // below one in a hundred billion.
    assert_eq!(
        answers.into_iter().collect::<Vec<_>>(),
        ["from claude", "from gpt"]
    );
    // The data plane's role admits each request; the database stamps it
    // with its deployment's master as it does (migration 0141).
    let stamped: Vec<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT deployment, model_master, count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 GROUP BY 1, 2 ORDER BY 1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stamped.len(), 2, "{stamped:?}");
    assert_eq!(stamped[0].0, "claude-sonnet");
    assert_eq!(stamped[0].1.as_deref(), Some(SONNET));
    assert_eq!(stamped[1].0, "gpt-luna");
    assert_eq!(stamped[1].1.as_deref(), Some("openai/gpt-6-luna"));
    assert_eq!(stamped[0].2 + stamped[1].2, 40);
}

#[tokio::test]
async fn card_matches_are_written_again_until_a_write_succeeds_and_then_kept() {
    use gateway::{
        application::model_masters::{
            CardMatchSchedule, spawn_card_match_sync, sync_card_matches_once,
        },
        metrics::{MODEL_CARD_MATCH_SYNCS, global as metrics},
        persistence::model_masters::CardMatches,
    };
    let library = gateway::domain::model_card_files::CardLibrary::builtin().unwrap();
    let matches = CardMatches::of(&library);
    assert!(matches.len() > 10);
    let schedule = CardMatchSchedule {
        first_retry: StdDuration::from_millis(10),
        max_retry: StdDuration::from_millis(20),
        resync: StdDuration::from_millis(20),
    };
    let count = |outcome: &str| {
        metrics()
            .value(&MODEL_CARD_MATCH_SYNCS, &[outcome])
            .unwrap_or(0)
    };
    let wait_for = |outcome: &'static str, at_least: i64| async move {
        for _ in 0..500 {
            if count(outcome) >= at_least {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("{outcome} stayed below {at_least}");
    };

    // A database that cannot be reached (or has no 0141 yet): each write
    // fails, is counted, and is tried again; nothing waits for a restart.
    let unreachable = PgPoolOptions::new()
        .acquire_timeout(StdDuration::from_millis(200))
        .connect_lazy("postgres://gateway@127.0.0.1:1/gateway")
        .unwrap();
    let store = ModelMasterStore::new(unreachable);
    let errors = count("error");
    assert_eq!(sync_card_matches_once(&store, &matches, 0).await, 1);
    assert_eq!(sync_card_matches_once(&store, &matches, 1).await, 2);
    let retrying = spawn_card_match_sync(store, matches.clone(), schedule, 2);
    wait_for("error", errors + 5).await;
    retrying.abort();

    let Some(database_url) = test_database_url() else {
        return;
    };
    let pool = connect_test_pool(&database_url, 2).await;
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let store = ModelMasterStore::new(pool.clone());
    // After a failure the next write succeeds and resets the count; then it
    // is written again on schedule, so a gap closes on its own.
    let oks = count("ok");
    assert_eq!(sync_card_matches_once(&store, &matches, 3).await, 0);
    let keeping = spawn_card_match_sync(store, matches.clone(), schedule, 0);
    wait_for("ok", oks + 3).await;
    keeping.abort();
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway.model_card_match")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, matches.len() as i64);
}
