#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the catalog admin lane: release, provider account,
//! deployment, and route reads; release creation and activation; and
//! encrypted provider credentials, driven through the real router, RBAC,
//! and schema.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, TeamId, UserAccountId,
    domain::{
        auth::AuthCrypto, credentials::CredentialCipher, identity::MembershipRole,
        keys::ApiKeyPepper,
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore, catalog_admin::CatalogAdminStore,
        provider_credentials::ProviderCredentialStore,
    },
    transport::catalog_admin::router,
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
const TEST_PEPPER: &[u8] = b"catalog-admin-integration-pepper";
const HMAC_KEY: &[u8] = b"catalog-admin-integration-hmac-key-32";
const CREDENTIAL_KEY: [u8; 32] = [42; 32];

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

async fn connect_test_pool(database_url: &str) -> PgPool {
    let mut last_error = None;
    for _ in 0..15 {
        match PgPoolOptions::new()
            .max_connections(8)
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

fn capability(operation: &str) -> Value {
    json!({
        "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 32,
        "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 17,
        "chat_input_count_mode": "unsupported"
    })
}

fn price(deployment: &str, operation: &str, input: i64, version: &str) -> Value {
    json!({
        "deployment": deployment, "operation": operation,
        "input_minor_per_token": input, "output_minor_per_token": 7,
        "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
        "other_minor_per_token": 0, "currency": "USD", "source": "test",
        "source_version": version, "confidence": "known"
    })
}

/// Two providers and three deployments. `gpt-a` is catalogued for chat and
/// embeddings but priced only for chat; `input` varies the chat price so two
/// releases are distinguishable. Stored directly, as a release made before
/// streaming became a mode of chat (D-020): `fast` streams through `gpt-b`
/// while its unary chat goes to `gpt-a`, which a manifest can no longer say.
fn manifest(version: &str, input: i64) -> CatalogManifest {
    let mut value = manifest_json(version, input);
    value["deployments"][1]["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(capability("chat_stream"));
    value["prices"]
        .as_array_mut()
        .unwrap()
        .insert(2, price("gpt-b", "chat_stream", 4, version));
    value["routes"].as_array_mut().unwrap().insert(
        1,
        json!({"alias": "fast", "operation": "chat_stream", "deployment": "gpt-b"}),
    );
    serde_json::from_value(value).unwrap()
}

/// [`manifest`] as a manifest is written now, with no `chat_stream`.
fn manifest_json(version: &str, input: i64) -> Value {
    json!({
        "release": {"version": version},
        "providers": [
            {"adapter_instance_key": "openai-main", "family": "openai"},
            {"adapter_instance_key": "anthropic-main", "family": "anthropic"}
        ],
        "deployments": [
            {"name": "gpt-a", "model": "gpt", "location": null, "adapter_instance_key": "openai-main",
             "capabilities": [capability("chat"), capability("embeddings")]},
            {"name": "gpt-b", "model": "gpt", "location": "eu", "adapter_instance_key": "openai-main",
             "capabilities": [capability("chat")]},
            {"name": "claude-a", "model": "claude", "location": null,
             "adapter_instance_key": "anthropic-main", "capabilities": [capability("chat")]}
        ],
        "prices": [
            price("gpt-a", "chat", input, version),
            price("gpt-b", "chat", 4, version),
            price("claude-a", "chat", 5, version)
        ],
        "routes": [
            {"alias": "fast", "operation": "chat", "deployment": "gpt-a"},
            {"alias": "smart", "operation": "chat", "deployment": "claude-a"},
            {"alias": "tiered", "operation": "chat", "deployment": "gpt-b"}
        ]
    })
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    catalog: CatalogStore,
    router: Router,
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[17_u8; 32]).unwrap();
        let router = catalog_router(
            &pool,
            &crypto,
            Some(CredentialCipher::new(&CREDENTIAL_KEY).unwrap()),
        );
        Self {
            catalog: CatalogStore::new(pool.clone()),
            pool,
            identity,
            crypto,
            router,
        }
    }

    async fn org(&self, slug: &str) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id
    }

    /// Applies and activates a release, returning its id.
    async fn release(&self, org: OrganizationId, manifest: &CatalogManifest) -> Uuid {
        self.catalog
            .apply(org, manifest, manifest.digest().unwrap())
            .await
            .unwrap()
            .id
    }

    /// A user holding exactly one admin role binding: org-wide when `team` is
    /// `None`, otherwise `team_admin` of that team.
    async fn user(&self, org: OrganizationId, team: Option<TeamId>, role: &str) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some(role))
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, MembershipRole::Member)
            .await
            .unwrap();
        if let Some(team) = team {
            self.identity
                .add_team_membership(org, team, user, MembershipRole::Admin)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(team.map(|team| team.0))
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> (String, String) {
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

    async fn get(
        &self,
        uri: &str,
        session: Option<&(String, String)>,
    ) -> (StatusCode, Value, HeaderMap) {
        self.send(Method::GET, uri, session, &[], None).await
    }

    /// Sends a JSON (or empty) request with extra headers.
    async fn send(
        &self,
        method: Method,
        uri: &str,
        session: Option<&(String, String)>,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        send(&self.router, method, uri, session, headers, body).await
    }
}

fn catalog_router(pool: &PgPool, crypto: &AuthCrypto, cipher: Option<CredentialCipher>) -> Router {
    router(
        AuthStore::new(pool.clone()),
        AdminStore::new(pool.clone()),
        Some(crypto.clone()),
        CatalogAdminStore::new(pool.clone()),
        ProviderCredentialStore::new(pool.clone()),
        cipher,
    )
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    session: Option<&(String, String)>,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (StatusCode, Value, HeaderMap) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(session) = session {
        builder = builder.header(
            COOKIE,
            format!(
                "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                session.0, session.1
            ),
        );
    }
    let body = match body {
        Some(body) => Body::from(serde_json::to_vec(body).unwrap()),
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body, headers)
}

impl Harness {
    /// Follows `next_page_token` to the end and returns every item.
    async fn collect(&self, uri: &str, session: &(String, String)) -> Vec<Value> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let separator = if uri.contains('?') { '&' } else { '?' };
            let page_uri = match &token {
                Some(token) => format!("{uri}{separator}page_token={token}"),
                None => uri.to_owned(),
            };
            let (status, body, _) = self.get(&page_uri, Some(session)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            items.extend(body["items"].as_array().unwrap().iter().cloned());
            match body["next_page_token"].as_str() {
                Some(next) => token = Some(next.to_owned()),
                None => return items,
            }
        }
    }
}

fn catalog_uri(org: OrganizationId, collection: &str, query: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/catalog/{collection}{query}",
        org.0
    )
}

fn field<'a>(items: &'a [Value], name: &str) -> Vec<&'a str> {
    items
        .iter()
        .map(|item| item[name].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn catalog_reads_the_active_release_unless_one_is_named() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog").await;
    let first = harness.release(org, &manifest("v1", 3)).await;
    let second = harness.release(org, &manifest("v2", 9)).await;
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;

    let (status, body, headers) = harness
        .get(&catalog_uri(org, "releases", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    let releases = body["items"].as_array().unwrap();
    assert_eq!(field(releases, "version"), ["v2", "v1"]);
    assert_eq!(releases[0]["id"], json!(second));
    assert_eq!(releases[0]["active"], json!(true));
    assert!(releases[0]["activated_at"].is_string());
    assert_eq!(releases[1]["active"], json!(false));
    assert!(releases[1]["activated_at"].is_null());
    for release in releases {
        assert!(release["sealed_at"].is_string());
        let digest = release["digest"].as_str().unwrap();
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
    }

    let (status, body, _) = harness
        .get(&catalog_uri(org, "provider-accounts", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["release_id"], json!(second));
    let accounts = body["items"].as_array().unwrap();
    assert_eq!(
        field(accounts, "adapter_instance_key"),
        ["anthropic-main", "openai-main"]
    );
    assert_eq!(field(accounts, "family"), ["anthropic", "openai"]);
    assert_eq!(accounts[0]["deployment_count"], json!(1));
    assert_eq!(accounts[1]["deployment_count"], json!(2));
    // Nothing credential-shaped is exposed.
    assert_eq!(
        accounts[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "adapter_instance_key",
            "deployment_count",
            "family",
            "id",
            "release_id"
        ]
    );
    let openai = accounts[1]["id"].as_str().unwrap().to_owned();

    let (status, body, _) = harness
        .get(&catalog_uri(org, "deployments", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let deployments = body["items"].as_array().unwrap();
    assert_eq!(field(deployments, "name"), ["claude-a", "gpt-a", "gpt-b"]);
    let gpt_a = &deployments[1];
    assert_eq!(gpt_a["family"], "openai");
    assert_eq!(gpt_a["provider_account_id"], json!(openai));
    let operations = gpt_a["operations"].as_array().unwrap();
    assert_eq!(field(operations, "operation"), ["chat", "embeddings"]);
    assert_eq!(operations[0]["price"]["input_minor_per_token"], json!(9));
    assert_eq!(operations[0]["price"]["currency"], "USD");
    assert_eq!(operations[0]["capability"]["max_input_bytes"], json!(4096));
    assert_eq!(operations[0]["capability"]["tokenizer"], "configured");
    // Catalogued but unpriced: no route can target it.
    assert!(operations[1]["capability"].is_object());
    assert!(operations[1]["price"].is_null());
    assert_eq!(deployments[2]["location"], "eu");
    assert_eq!(
        field(
            deployments[2]["operations"].as_array().unwrap(),
            "operation"
        ),
        ["chat", "chat_stream"]
    );

    let (status, body, _) = harness
        .get(
            &catalog_uri(
                org,
                "deployments",
                &format!("?provider_account_id={openai}&model=gpt"),
            ),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        field(body["items"].as_array().unwrap(), "name"),
        ["gpt-a", "gpt-b"]
    );

    let (status, body, _) = harness
        .get(&catalog_uri(org, "routes", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let routes = body["items"].as_array().unwrap();
    assert_eq!(field(routes, "alias"), ["fast", "fast", "smart", "tiered"]);
    assert_eq!(
        field(routes, "operation"),
        ["chat", "chat_stream", "chat", "chat"]
    );
    assert_eq!(routes[0]["deployment_name"], "gpt-a");
    assert_eq!(routes[2]["family"], "anthropic");
    assert_eq!(routes[2]["model"], "claude");
    assert_eq!(
        routes[0]["deployment_id"], deployments[1]["id"],
        "route and deployment views agree on ids"
    );
    assert_eq!(routes[0]["price_id"], operations[0]["price"]["id"]);
    assert_eq!(
        routes[0]["capability_id"],
        operations[0]["capability"]["id"]
    );

    let (_, body, _) = harness
        .get(
            &catalog_uri(org, "routes", "?alias=fast&operation=chat_stream"),
            Some(&owner),
        )
        .await;
    assert_eq!(
        field(body["items"].as_array().unwrap(), "deployment_name"),
        ["gpt-b"]
    );

    // A historical release stays readable by id.
    let (status, body, _) = harness
        .get(
            &catalog_uri(org, "deployments", &format!("?release_id={first}")),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["release_id"], json!(first));
    assert_eq!(
        body["items"][1]["operations"][0]["price"]["input_minor_per_token"],
        json!(3)
    );
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["release_id"] == json!(first))
    );

    harness.pool.close().await;
}

#[tokio::test]
async fn pagination_is_stable_and_bound_to_release_and_filters() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-paging").await;
    let first = harness.release(org, &manifest("v1", 3)).await;
    harness.release(org, &manifest("v2", 9)).await;
    harness.release(org, &manifest("v3", 11)).await;
    let owner = harness.user(org, None, "operator").await;
    let owner = harness.session(owner).await;

    let releases = harness
        .collect(&catalog_uri(org, "releases", "?page_size=1"), &owner)
        .await;
    assert_eq!(field(&releases, "version"), ["v3", "v2", "v1"]);
    let accounts = harness
        .collect(
            &catalog_uri(org, "provider-accounts", "?page_size=1"),
            &owner,
        )
        .await;
    assert_eq!(
        field(&accounts, "adapter_instance_key"),
        ["anthropic-main", "openai-main"]
    );
    let deployments = harness
        .collect(&catalog_uri(org, "deployments", "?page_size=2"), &owner)
        .await;
    assert_eq!(field(&deployments, "name"), ["claude-a", "gpt-a", "gpt-b"]);
    let routes = harness
        .collect(&catalog_uri(org, "routes", "?page_size=1"), &owner)
        .await;
    assert_eq!(
        routes
            .iter()
            .map(|route| format!(
                "{}/{}",
                route["alias"].as_str().unwrap(),
                route["operation"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        ["fast/chat", "fast/chat_stream", "smart/chat", "tiered/chat"]
    );

    let (_, body, _) = harness
        .get(&catalog_uri(org, "routes", "?page_size=1"), Some(&owner))
        .await;
    let token = body["next_page_token"].as_str().unwrap().to_owned();
    for query in [
        format!("?page_size=1&alias=fast&page_token={token}"),
        format!("?page_size=1&release_id={first}&page_token={token}"),
    ] {
        let (status, body, _) = harness
            .get(&catalog_uri(org, "routes", &query), Some(&owner))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body["code"], "page_token_query_mismatch", "{query}");
    }
    // A token from one collection does not page another.
    let (status, body, _) = harness
        .get(
            &catalog_uri(
                org,
                "deployments",
                &format!("?page_size=1&page_token={token}"),
            ),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["code"].as_str().unwrap().starts_with("page_token_"),
        "{body}"
    );
    // Activating a new release invalidates tokens minted against the old one.
    harness.release(org, &manifest("v4", 13)).await;
    let (status, body, _) = harness
        .get(
            &catalog_uri(org, "routes", &format!("?page_size=1&page_token={token}")),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "page_token_query_mismatch");

    let (status, body, _) = harness
        .get(&catalog_uri(org, "routes", "?page_size=101"), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "pagination_limit_invalid");

    harness.pool.close().await;
}

#[tokio::test]
async fn tenancy_roles_and_query_errors_are_enforced() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-tenant").await;
    let other = harness.org("catalog-other").await;
    let release = harness.release(org, &manifest("v1", 3)).await;
    let foreign = harness.release(other, &manifest("v1", 3)).await;
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;

    // No binding in the other tenant.
    let (status, _, _) = harness
        .get(&catalog_uri(other, "routes", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Another tenant's release under this tenant's path is simply absent.
    for collection in ["provider-accounts", "deployments", "routes"] {
        let (status, body, _) = harness
            .get(
                &catalog_uri(org, collection, &format!("?release_id={foreign}")),
                Some(&owner),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{collection}");
        assert_eq!(body["code"], "not_found");
    }

    // An organization with no active release reads as empty, not an error.
    let empty = harness.org("catalog-empty").await;
    let empty_owner = harness.user(empty, None, "organization_owner").await;
    let empty_owner = harness.session(empty_owner).await;
    for collection in ["provider-accounts", "deployments", "routes"] {
        let (status, body, _) = harness
            .get(&catalog_uri(empty, collection, ""), Some(&empty_owner))
            .await;
        assert_eq!(status, StatusCode::OK, "{collection}");
        assert_eq!(
            body,
            json!({"release_id": null, "items": [], "next_page_token": null})
        );
    }
    let (_, body, _) = harness
        .get(&catalog_uri(empty, "releases", ""), Some(&empty_owner))
        .await;
    assert_eq!(body["items"], json!([]));

    // Every org-wide role reads the catalog; a team admin does not.
    for role in [
        "organization_admin",
        "operator",
        "auditor",
        "read_only_member",
    ] {
        let user = harness.user(org, None, role).await;
        let session = harness.session(user).await;
        let (status, body, _) = harness
            .get(&catalog_uri(org, "routes", ""), Some(&session))
            .await;
        assert_eq!(status, StatusCode::OK, "{role}");
        assert_eq!(body["release_id"], json!(release), "{role}");
    }
    let team = harness.identity.create_team(org, "team").await.unwrap().id;
    let team_admin = harness.user(org, Some(team), "team_admin").await;
    let team_admin = harness.session(team_admin).await;
    for collection in ["releases", "provider-accounts", "deployments", "routes"] {
        let (status, body, headers) = harness
            .get(&catalog_uri(org, collection, ""), Some(&team_admin))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{collection}");
        assert_eq!(body["code"], "forbidden");
        // Problems are never cached either.
        assert_eq!(headers["cache-control"], "no-store", "{collection}");
    }

    let (status, body, headers) = harness.get(&catalog_uri(org, "routes", ""), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "authentication_required");
    assert_eq!(headers["cache-control"], "no-store");

    for query in [
        "?unknown=1",
        "?release_id=not-a-uuid",
        "?operation=images",
        "?alias=",
    ] {
        let (status, body, headers) = harness
            .get(&catalog_uri(org, "routes", query), Some(&owner))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body["code"], "invalid_query", "{query}");
        assert_eq!(headers["content-type"], "application/problem+json");
        assert_eq!(headers["cache-control"], "no-store", "{query}");
    }
    let (status, _, _) = harness
        .get(&catalog_uri(org, "releases", "?release_id=x"), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    harness.pool.close().await;
}

fn write_headers<'a>(key: &'a str, if_match: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![("idempotency-key", key)];
    if let Some(if_match) = if_match {
        headers.push(("if-match", if_match));
    }
    headers
}

async fn audit_rows(pool: &PgPool, org: OrganizationId) -> Vec<(String, String, Value)> {
    sqlx::query_as::<_, (String, String, Value)>(
        "SELECT action, outcome,
                jsonb_build_array(metadata, before_metadata, after_metadata)
         FROM gateway.audit_event WHERE organization_id = $1
         ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_manifest_streaming_elsewhere_than_its_chat_route_is_refused() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-streaming").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;
    let releases = catalog_uri(org, "releases", "");

    // Streaming takes the chat route (D-020): a `chat_stream` entry that
    // matches its `chat` one is folded away, and one that differs is refused,
    // naming what differs.
    let mut streaming = manifest_json("streaming", 3);
    streaming["routes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"alias": "fast", "operation": "chat_stream", "deployment": "gpt-a"}));
    let (status, folded, _) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-folded", None),
            Some(&streaming),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{folded}");
    streaming["release"]["version"] = "streaming-elsewhere".into();
    streaming["routes"][3]["deployment"] = "gpt-b".into();
    let (status, body, _) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-streaming-elsewhere", None),
            Some(&streaming),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_manifest");
    assert!(
        body["detail"].as_str().unwrap().contains("alias `fast`"),
        "{body}"
    );
}

#[tokio::test]
async fn releases_are_created_inactive_then_activated_and_rolled_back() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-write").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;
    let releases = catalog_uri(org, "releases", "");

    let v1 = manifest_json("v1", 3);
    let (status, first, headers) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-v1", None),
            Some(&v1),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(first["active"], json!(false));
    assert_eq!(first["version"], "v1");
    let first_id = first["id"].as_str().unwrap().to_owned();

    // Replaying the same key returns the same release.
    let (status, replay, _) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-v1", None),
            Some(&v1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["id"], first["id"]);

    // The same manifest under a new key conflicts with the stored release.
    let (status, body, headers) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-v1-again", None),
            Some(&v1),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(headers["content-type"], "application/problem+json");
    assert_eq!(body["code"], "release_exists");
    assert!(body["detail"].as_str().unwrap().contains(&first_id));

    // The same key with a different manifest is an idempotency conflict.
    let (status, body, _) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-v1", None),
            Some(&manifest_json("v2", 9)),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "idempotency_conflict");

    let (status, body, _) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-bad", None),
            Some(&json!({"release": {"version": "broken"}})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_manifest");

    let (status, second, _) = harness
        .send(
            Method::POST,
            &releases,
            Some(&owner),
            &write_headers("create-v2", None),
            Some(&manifest_json("v2", 9)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    let second_id = second["id"].as_str().unwrap().to_owned();

    // Nothing is active until an explicit activation.
    let (status, body, _) = harness
        .get(&catalog_uri(org, "provider-accounts", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["release_id"].is_null());

    // The release list names the activation version to send as `If-Match`:
    // 0 (send `*`) until the first activation.
    let (status, body, _) = harness.get(&releases, Some(&owner)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["activation_version"], json!(0));

    let activate = |id: &str| catalog_uri(org, &format!("releases/{id}/activate"), "");
    let (status, body, _) = harness
        .send(
            Method::POST,
            &activate(&first_id),
            Some(&owner),
            &write_headers("activate-missing-if-match", None),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{body}");

    // "v0" activates only while nothing has been: the version the list
    // reported before any activation.
    let (status, body, headers) = harness
        .send(
            Method::POST,
            &activate(&first_id),
            Some(&owner),
            &write_headers("activate-v1", Some("\"v0\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(body["activation_version"], json!(1));
    assert_eq!(body["release"]["id"], json!(first_id));
    assert_eq!(body["release"]["active"], json!(true));
    // A second client that also saw it unactivated now conflicts.
    let (status, body, _) = harness
        .send(
            Method::POST,
            &activate(&second_id),
            Some(&owner),
            &write_headers("activate-stale-v0", Some("\"v0\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");

    let (status, body, headers) = harness
        .send(
            Method::POST,
            &activate(&second_id),
            Some(&owner),
            &write_headers("activate-v2", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");
    let (status, body, _) = harness.get(&releases, Some(&owner)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["activation_version"], json!(2));

    // A stale version is rejected; the replay of an applied activation is not.
    let (status, body, _) = harness
        .send(
            Method::POST,
            &activate(&first_id),
            Some(&owner),
            &write_headers("rollback-stale", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    let (status, body, headers) = harness
        .send(
            Method::POST,
            &activate(&second_id),
            Some(&owner),
            &write_headers("activate-v2", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");

    // Rollback is activation of an earlier release.
    let (status, body, headers) = harness
        .send(
            Method::POST,
            &activate(&first_id),
            Some(&owner),
            &write_headers("rollback", Some("\"v2\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v3\"");
    let (_, body, _) = harness
        .get(&catalog_uri(org, "provider-accounts", ""), Some(&owner))
        .await;
    assert_eq!(body["release_id"], json!(first_id));

    // A replay returns the recorded response, not the current state.
    let (status, body, headers) = harness
        .send(
            Method::POST,
            &activate(&second_id),
            Some(&owner),
            &write_headers("activate-v2", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(body["release"]["id"], json!(second_id));
    assert_eq!(body["release"]["active"], json!(true));

    // A CLI switch is a new activation version, so a stale API client
    // conflicts instead of overwriting it; re-applying the active release
    // is not a switch.
    let cli = manifest("v3", 11);
    harness.release(org, &cli).await;
    let (status, body, _) = harness
        .send(
            Method::POST,
            &activate(&first_id),
            Some(&owner),
            &write_headers("after-cli-stale", Some("\"v3\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    harness.release(org, &cli).await;
    let (status, body, headers) = harness
        .send(
            Method::POST,
            &activate(&first_id),
            Some(&owner),
            &write_headers("after-cli", Some("\"v4\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v5\"");

    let (status, body, _) = harness
        .send(
            Method::POST,
            &activate(&Uuid::new_v4().to_string()),
            Some(&owner),
            &write_headers("activate-unknown", Some("*")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let actions: Vec<_> = audit_rows(&harness.pool, org)
        .await
        .into_iter()
        .map(|(action, outcome, _)| format!("{action}:{outcome}"))
        .collect();
    assert_eq!(
        actions,
        [
            "catalog_release.created:succeeded",
            "catalog_release.created:succeeded",
            "catalog_release.activated:succeeded",
            "catalog_release.activated:succeeded",
            "catalog_release.activated:succeeded",
            "catalog_release.activated:succeeded",
        ]
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn catalog_writes_are_limited_to_owners_and_admins() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-write-rbac").await;
    let release = harness.release(org, &manifest("v1", 3)).await;
    let team = harness.identity.create_team(org, "team").await.unwrap().id;
    let admin = harness.user(org, None, "organization_admin").await;
    let admin = harness.session(admin).await;
    let credential = catalog_uri(org, "credentials/openai-main", "");
    let secret = json!({"api_key": "sk-rbac-secret-value"});

    for (index, (role, team)) in [
        ("operator", None),
        ("auditor", None),
        ("read_only_member", None),
        ("team_admin", Some(team)),
    ]
    .into_iter()
    .enumerate()
    {
        let user = harness.user(org, team, role).await;
        let session = harness.session(user).await;
        let key = format!("rbac-{index}");
        let (status, body, _) = harness
            .send(
                Method::POST,
                &catalog_uri(org, "releases", ""),
                Some(&session),
                &write_headers(&key, None),
                Some(&manifest_json("rbac", 5)),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        let (status, body, _) = harness
            .send(
                Method::POST,
                &catalog_uri(org, &format!("releases/{release}/activate"), ""),
                Some(&session),
                &write_headers(&key, Some("*")),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &credential,
                Some(&session),
                &write_headers(&key, Some("*")),
                Some(&secret),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        // Undeclared keys and absent credentials look the same as declared
        // ones, so a caller without `credential_write` cannot probe them.
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &catalog_uri(org, "credentials/undeclared", ""),
                Some(&session),
                &write_headers(&key, Some("*")),
                Some(&secret),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        let (status, body, _) = harness
            .send(
                Method::DELETE,
                &catalog_uri(org, "credentials/undeclared", ""),
                Some(&session),
                &write_headers(&key, Some("*")),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        // Reading credential metadata follows catalog read access.
        let (status, body, _) = harness
            .get(&catalog_uri(org, "credentials", ""), Some(&session))
            .await;
        let expected = if role == "team_admin" {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::OK
        };
        assert_eq!(status, expected, "{role}: {body}");
    }

    let (status, body, _) = harness
        .send(
            Method::PUT,
            &credential,
            Some(&admin),
            &write_headers("admin-set", Some("*")),
            Some(&secret),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let rows = audit_rows(&harness.pool, org).await;
    assert_eq!(
        rows.iter()
            .filter(|(_, outcome, _)| outcome == "denied")
            .count(),
        20
    );
    for (_, _, metadata) in &rows {
        assert!(!metadata.to_string().contains("sk-rbac-secret-value"));
    }
    harness.pool.close().await;
}

#[tokio::test]
async fn provider_credentials_are_sealed_and_never_returned() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-credentials").await;
    let mut bedrock = manifest_json("v1", 3);
    bedrock["providers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"adapter_instance_key": "bedrock-main", "family": "bedrock"}));
    bedrock["deployments"].as_array_mut().unwrap().push(json!({
        "name": "titan", "model": "titan", "location": "us-east-1",
        "adapter_instance_key": "bedrock-main", "capabilities": [capability("chat")]
    }));
    bedrock["providers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"adapter_instance_key": "azure-openai-main", "family": "azure-openai"}));
    bedrock["deployments"].as_array_mut().unwrap().push(json!({
        "name": "azure-gpt", "model": "gpt-4o", "location": "eastus",
        "adapter_instance_key": "azure-openai-main", "capabilities": [capability("chat")]
    }));
    bedrock["providers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"adapter_instance_key": "gemini-main", "family": "gemini"}));
    bedrock["deployments"].as_array_mut().unwrap().push(json!({
        "name": "gemini-flash", "model": "gemini-2.5-flash-lite", "location": "global",
        "adapter_instance_key": "gemini-main", "capabilities": [capability("chat")]
    }));
    let manifest: CatalogManifest = serde_json::from_value(bedrock).unwrap();
    harness
        .catalog
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;
    let openai = catalog_uri(org, "credentials/openai-main", "");
    const FIRST: &str = "sk-first-secret-value-0001";
    const SECOND: &str = "sk-second-secret-value-0002";

    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("set-1", Some("*")),
            Some(&json!({"api_key": FIRST})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body["adapter_instance_key"], "openai-main");
    assert_eq!(body["family"], "openai");
    assert_eq!(body["version"], json!(1));
    assert_eq!(
        body.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "adapter_instance_key",
            "created_at",
            "family",
            "key_id",
            "rotated_at",
            "version"
        ]
    );
    let key_id = CredentialCipher::new(&CREDENTIAL_KEY)
        .unwrap()
        .key_id()
        .to_owned();
    assert_eq!(body["key_id"], json!(key_id));

    // Replay returns metadata without re-sealing; a new key with the same
    // secret still needs the current version.
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("set-1", Some("*")),
            Some(&json!({"api_key": FIRST})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("set-1", Some("*")),
            Some(&json!({"api_key": SECOND})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "idempotency_conflict");

    let (status, body, _) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("rotate-stale", Some("\"v7\"")),
            Some(&json!({"api_key": SECOND})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("rotate", Some("\"v1\"")),
            Some(&json!({"api_key": SECOND})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");
    let rotated_at = body["rotated_at"].clone();

    // Replaying the first set after the rotation returns its own result.
    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("set-1", Some("*")),
            Some(&json!({"api_key": FIRST})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(body["version"], json!(1));
    assert_ne!(body["rotated_at"], rotated_at);

    for (input, code) in [
        (json!({}), "invalid_credential"),
        (json!({"api_key": ""}), "invalid_credential"),
        (json!({"api_key": "has space"}), "invalid_credential"),
        (
            json!({"api_key": FIRST, "secret_access_key": FIRST}),
            "invalid_credential",
        ),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &catalog_uri(org, "credentials/anthropic-main", ""),
                Some(&owner),
                &write_headers(&Uuid::new_v4().to_string(), Some("*")),
                Some(&input),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{input}: {body}");
        assert_eq!(body["code"], code);
    }
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &catalog_uri(org, "credentials/anthropic-main", ""),
            Some(&owner),
            &write_headers("unknown-field", Some("*")),
            Some(&json!({"api_key": FIRST, "extra": true})),
        )
        .await;
    assert!(status.is_client_error(), "{body}");

    let bedrock = catalog_uri(org, "credentials/bedrock-main", "");
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &bedrock,
            Some(&owner),
            &write_headers("bedrock-api-key", Some("*")),
            Some(&json!({"api_key": FIRST})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &bedrock,
            Some(&owner),
            &write_headers("bedrock", Some("*")),
            Some(&json!({
                "access_key_id": "AKIAEXAMPLE0001",
                "secret_access_key": "bedrock-secret-access-key",
                "session_token": "bedrock-session-token"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["family"], "bedrock");
    for (key, input) in [
        (
            "bedrock-role-and-keys",
            json!({
                "role_arn": "arn:aws:iam::123456789012:role/gateway-tenant",
                "access_key_id": "AKIAEXAMPLE0001",
                "secret_access_key": "bedrock-secret-access-key"
            }),
        ),
        (
            "bedrock-bad-role",
            json!({"role_arn": "arn:aws:iam::1:role/r"}),
        ),
        (
            "bedrock-orphan-external-id",
            json!({"external_id": "ext-123"}),
        ),
        // The external ID is the gateway's to assign.
        (
            "bedrock-tenant-external-id",
            json!({
                "role_arn": "arn:aws:iam::123456789012:role/gateway-tenant",
                "external_id": "ext-123"
            }),
        ),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &bedrock,
                Some(&owner),
                &write_headers(key, Some("\"v1\"")),
                Some(&input),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{input}: {body}");
    }
    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &bedrock,
            Some(&owner),
            &write_headers("bedrock-role", Some("\"v1\"")),
            Some(&json!({"role_arn": "arn:aws:iam::123456789012:role/gateway-tenant"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert!(!body.to_string().contains("gateway-tenant"), "{body}");
    // An AWS role needs no operator binding.
    assert!(body.get("cloud_principal").is_none(), "{body}");

    // What the organization's cloud accounts must name to trust the gateway.
    let (status, body, _) = harness
        .get(&catalog_uri(org, "cloud-trust", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["aws_external_id"], format!("qucelerate-{}", org.0));
    assert_eq!(
        body["aws_role_session_name"],
        format!("qucelerate-{}", org.0.simple())
    );

    let azure = catalog_uri(org, "credentials/azure-openai-main", "");
    const TENANT: &str = "8f2c1a4e-0000-4000-8000-00000000000a";
    for (key, input) in [
        (
            "azure-tenant-and-key",
            json!({"tenant_id": TENANT, "api_key": FIRST}),
        ),
        (
            "azure-foreign-endpoint",
            json!({"tenant_id": TENANT, "endpoint": "https://evil.example.com"}),
        ),
        (
            "azure-orphan-client",
            json!({"client_id": "11111111-2222-4333-8444-555555555555"}),
        ),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &azure,
                Some(&owner),
                &write_headers(key, Some("*")),
                Some(&input),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{input}: {body}");
    }
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &azure,
            Some(&owner),
            &write_headers("azure-entra", Some("*")),
            Some(&json!({
                "tenant_id": TENANT,
                "client_id": "11111111-2222-4333-8444-555555555555",
                "endpoint": "https://tenant-res.openai.azure.com"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["family"], "azure-openai");
    for hidden in [TENANT, "tenant-res"] {
        assert!(!body.to_string().contains(hidden), "{body}");
    }

    let gemini = catalog_uri(org, "credentials/gemini-main", "");
    const SERVICE_ACCOUNT: &str = "gemini-tenant@tenant-gcp-project.iam.gserviceaccount.com";
    for (key, input) in [
        (
            "gemini-account-and-key",
            json!({"service_account": SERVICE_ACCOUNT, "api_key": FIRST}),
        ),
        (
            "gemini-not-a-service-account",
            json!({"service_account": "person@example.com"}),
        ),
        (
            "gemini-orphan-project",
            json!({"project_id": "tenant-gcp-project"}),
        ),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &gemini,
                Some(&owner),
                &write_headers(key, Some("*")),
                Some(&input),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{input}: {body}");
    }
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &gemini,
            Some(&owner),
            &write_headers("gemini-service-account", Some("*")),
            Some(&json!({
                "service_account": SERVICE_ACCOUNT,
                "project_id": "tenant-gcp-project"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["family"], "gemini");
    for hidden in [SERVICE_ACCOUNT, "tenant-gcp-project"] {
        assert!(!body.to_string().contains(hidden), "{body}");
    }

    let (status, body, _) = harness
        .send(
            Method::PUT,
            &catalog_uri(org, "credentials/undeclared", ""),
            Some(&owner),
            &write_headers("undeclared", Some("*")),
            Some(&json!({"api_key": FIRST})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "provider_account_not_found");

    let (status, body, _) = harness
        .get(&catalog_uri(org, "credentials", ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        field(items, "adapter_instance_key"),
        [
            "azure-openai-main",
            "bedrock-main",
            "gemini-main",
            "openai-main"
        ]
    );
    assert_eq!(items[3]["version"], json!(2));
    // Credentials that act as a tenant's cloud principal wait for an
    // operator's binding; the others need none.
    assert_eq!(items[0]["cloud_principal"], "pending");
    assert!(items[1].get("cloud_principal").is_none());
    assert_eq!(items[2]["cloud_principal"], "pending");
    assert!(items[3].get("cloud_principal").is_none());

    // Nothing plaintext reaches the table or the audit log.
    let ciphertexts: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT ciphertext FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(ciphertexts.len(), 4);
    let secrets = [
        FIRST,
        SECOND,
        "bedrock-secret-access-key",
        "bedrock-session-token",
        TENANT,
        "tenant-res",
        SERVICE_ACCOUNT,
        "tenant-gcp-project",
    ];
    for ciphertext in &ciphertexts {
        for secret in secrets {
            assert!(
                !ciphertext
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
            );
        }
    }
    let rows = audit_rows(&harness.pool, org).await;
    assert!(
        rows.iter()
            .any(|(action, _, _)| action == "provider_credential.set")
    );
    for (_, _, metadata) in &rows {
        let metadata = metadata.to_string();
        for secret in secrets {
            assert!(!metadata.contains(secret), "{metadata}");
        }
    }

    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &openai,
            Some(&owner),
            &write_headers("delete", Some("\"v2\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &openai,
            Some(&owner),
            &write_headers("delete", Some("\"v2\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(true));
    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &openai,
            Some(&owner),
            &write_headers("delete-again", Some("*")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // ... and replaying the rotation still reports what it did.
    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &openai,
            Some(&owner),
            &write_headers("rotate", Some("\"v1\"")),
            Some(&json!({"api_key": SECOND})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(body["rotated_at"], rotated_at);

    // Without an encryption key, credential writes are unavailable.
    let unkeyed = catalog_router(&harness.pool, &harness.crypto, None);
    let (status, body, _) = send(
        &unkeyed,
        Method::PUT,
        &openai,
        Some(&owner),
        &write_headers("unkeyed", Some("*")),
        Some(&json!({"api_key": FIRST})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "credential_encryption_unavailable");
    harness.pool.close().await;
}

#[tokio::test]
async fn credential_family_follows_the_active_release_and_lists_page() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-credential-family").await;
    harness.release(org, &manifest("v1", 3)).await;
    // A newer, inactive release redeclares `openai-main` as another family.
    let mut staged = manifest_json("v2", 9);
    staged["providers"][0]["family"] = json!("azure-openai");
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;
    let (status, body, _) = harness
        .send(
            Method::POST,
            &catalog_uri(org, "releases", ""),
            Some(&owner),
            &write_headers("staged", None),
            Some(&staged),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let staged_id = body["id"].as_str().unwrap().to_owned();
    for key in ["openai-main", "anthropic-main"] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &catalog_uri(org, &format!("credentials/{key}"), ""),
                Some(&owner),
                &write_headers(key, Some("*")),
                Some(&json!({"api_key": "sk-family-secret-value"})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        if key == "openai-main" {
            // The active release decides, not the newest one.
            assert_eq!(body["family"], "openai");
        }
    }

    let (status, first, _) = harness
        .get(
            &catalog_uri(org, "credentials", "?page_size=1"),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(
        field(first["items"].as_array().unwrap(), "adapter_instance_key"),
        ["anthropic-main"]
    );
    assert!(first["next_page_token"].is_string());
    let items = harness
        .collect(&catalog_uri(org, "credentials", "?page_size=1"), &owner)
        .await;
    assert_eq!(
        field(&items, "adapter_instance_key"),
        ["anthropic-main", "openai-main"]
    );
    let (status, body, _) = harness
        .get(&catalog_uri(org, "credentials", "?unknown=1"), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Activating the staged release moves `openai-main` to another family.
    // Retrying the earlier write still replays its recorded result, while a
    // new write seals for the family now active.
    let (status, body, _) = harness
        .send(
            Method::POST,
            &catalog_uri(org, &format!("releases/{staged_id}/activate"), ""),
            Some(&owner),
            &write_headers("activate-staged", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let put = |key: &'static str, if_match: &'static str| {
        let owner = &owner;
        let harness = &harness;
        async move {
            harness
                .send(
                    Method::PUT,
                    &catalog_uri(org, "credentials/openai-main", ""),
                    Some(owner),
                    &write_headers(key, Some(if_match)),
                    Some(&json!({"api_key": "sk-family-secret-value"})),
                )
                .await
        }
    };
    let (status, body, headers) = put("openai-main", "*").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(body["family"], "openai");
    assert_eq!(body["version"], json!(1));
    let (status, body, headers) = put("rotate-after-activation", "\"v1\"").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(body["family"], "azure-openai");
    harness.pool.close().await;
}

fn override_body(deployment: &str, operation: &str, input: i64) -> Value {
    json!({
        "deployment": deployment, "operation": operation,
        "input_minor_per_token": input, "output_minor_per_token": 13,
        "currency": "USD", "confidence": "conservative", "reason": "negotiated rate"
    })
}

/// Inserts a reservation for `route` with the given prices and provenance,
/// as the accounting store does, so the provenance triggers judge it.
async fn reserve_raw(
    pool: &PgPool,
    org: OrganizationId,
    route: &gateway::persistence::catalog::ResolvedCatalogRoute,
    prices: [i64; 5],
    version: &str,
    source: &str,
    price_override: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.accounting_reservation (id,organization_id,idempotency_key,fingerprint,state,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode,catalog_price_override_id) VALUES ($1,$2,$3,$4,'reserved',1,now()+interval '5 minutes',1,$5,$6,$7,$8,$9,$10,$11,'USD',$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,'unsupported',$23)")
        .bind(id)
        .bind(org.0)
        .bind(id.to_string())
        .bind(vec![9_u8; 32])
        .bind(prices[0])
        .bind(prices[1])
        .bind(prices[2])
        .bind(prices[3])
        .bind(prices[4])
        .bind(version)
        .bind(if price_override.is_some() { "conservative" } else { "known" })
        .bind(route.deployment.as_str())
        .bind(route.model.as_str())
        .bind(Uuid::new_v4())
        .bind(route.release_id)
        .bind(route.route_id)
        .bind(route.deployment_id)
        .bind(route.price_id)
        .bind(route.digest.0.to_vec())
        .bind(route.operation.as_str())
        .bind(route.family.as_str())
        .bind(source)
        .bind(price_override)
        .execute(pool)
        .await?;
    Ok(id)
}

#[tokio::test]
async fn price_overrides_apply_to_new_requests_and_never_rewrite_past_reservations() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("price-override").await;
    harness.release(org, &manifest("v1", 3)).await;
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;
    let overrides = catalog_uri(org, "price-overrides", "");
    let resolve = |alias: &'static str, operation: &'static str| {
        let catalog = harness.catalog.clone();
        async move {
            catalog
                .resolve_active(org, alias, operation)
                .await
                .unwrap()
                .unwrap()
        }
    };

    // A reservation made at the catalog price before any override.
    let catalog_route = resolve("fast", "chat").await;
    assert_eq!(catalog_route.price_override_id, None);
    assert_eq!(catalog_route.source, "test");
    let before = reserve_raw(
        &harness.pool,
        org,
        &catalog_route,
        [3, 7, 0, 0, 0],
        "v1",
        "test",
        None,
    )
    .await
    .unwrap();

    let body = override_body("gpt-a", "chat", 11);
    let (status, created, headers) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("override-a", None),
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(created["status"], "active");
    assert_eq!(created["version"], json!(1));
    let override_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(created["price_version"], format!("override:{override_id}"));

    let (status, replay, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("override-a", None),
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, created);
    let (status, conflict, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("override-a", None),
            Some(&override_body("gpt-a", "chat", 12)),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "idempotency_conflict");

    // Only priced deployment/operation pairs, prices the family can charge,
    // and forward windows are accepted.
    // Anthropic counts thinking as output, so reports no reasoning tokens.
    let mut reasoning = override_body("claude-a", "chat", 1);
    reasoning["reasoning_minor_per_token"] = json!(1);
    let mut window = override_body("gpt-a", "chat", 1);
    window["effective_from"] = json!("2100-01-02T00:00:00Z");
    window["effective_until"] = json!("2100-01-01T00:00:00Z");
    let mut unknown_field = override_body("gpt-a", "chat", 1);
    unknown_field["note"] = json!("x");
    for (index, (body, code)) in [
        (
            override_body("gpt-a", "embeddings", 1),
            "unknown_deployment",
        ),
        (override_body("missing", "chat", 1), "unknown_deployment"),
        (reasoning, "invalid_price_override"),
        (window, "invalid_price_override"),
        (override_body("gpt-a", "chat", -1), "invalid_price_override"),
        (unknown_field, "request_body_unprocessable"),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, problem, _) = harness
            .send(
                Method::POST,
                &overrides,
                Some(&owner),
                &write_headers(&format!("invalid-{index}"), None),
                Some(&body),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
        assert_eq!(problem["code"], code, "{problem}");
    }

    // A scheduled override does not apply yet.
    let mut scheduled = override_body("gpt-b", "chat", 21);
    scheduled["effective_from"] = json!("2100-01-01T00:00:00Z");
    let (status, scheduled, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("override-scheduled", None),
            Some(&scheduled),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{scheduled}");
    assert_eq!(scheduled["status"], "scheduled");
    let tiered = resolve("tiered", "chat").await;
    assert_eq!(tiered.price_override_id, None);
    assert_eq!(tiered.price.input_minor_per_token, 4);

    // New requests resolve to the override; other operations do not.
    let route = resolve("fast", "chat").await;
    assert_eq!(route.price_override_id, Some(override_id));
    assert_eq!(route.source, "override");
    assert_eq!(route.price_id, catalog_route.price_id);
    assert_eq!(route.price.input_minor_per_token, 11);
    assert_eq!(route.price.output_minor_per_token, 13);
    assert_eq!(route.price.version, format!("override:{override_id}"));
    assert_eq!(
        route.price.confidence,
        gateway::domain::accounting::PricingConfidence::Conservative
    );
    assert_eq!(resolve("fast", "chat_stream").await.price_override_id, None);

    let version = route.price.version.clone();
    let during = reserve_raw(
        &harness.pool,
        org,
        &route,
        [11, 13, 0, 0, 0],
        &version,
        "override",
        Some(override_id),
    )
    .await
    .unwrap();
    // The provenance triggers hold a reservation to the override it names.
    for (prices, version, source, route) in [
        ([3, 7, 0, 0, 0], version.as_str(), "override", &route),
        ([11, 13, 0, 0, 0], "v1", "override", &route),
        ([11, 13, 0, 0, 0], version.as_str(), "test", &route),
        (
            [11, 13, 0, 0, 0],
            version.as_str(),
            "override",
            &resolve("smart", "chat").await,
        ),
    ] {
        let error = reserve_raw(
            &harness.pool,
            org,
            route,
            prices,
            version,
            source,
            Some(override_id),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("provenance"), "{error}");
    }
    let error = reserve_raw(
        &harness.pool,
        org,
        &route,
        [11, 13, 0, 0, 0],
        &version,
        "override",
        None,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("provenance"), "{error}");

    // Overrides match deployments by name, so they survive a release switch.
    harness.release(org, &manifest("v2", 9)).await;
    let switched = resolve("fast", "chat").await;
    assert_eq!(switched.price_override_id, Some(override_id));
    assert_ne!(switched.release_id, route.release_id);
    assert_eq!(switched.price.input_minor_per_token, 11);

    let revoke = catalog_uri(org, &format!("price-overrides/{override_id}/revoke"), "");
    let reason = json!({"reason": "contract ended"});
    let (status, problem, _) = harness
        .send(
            Method::POST,
            &revoke,
            Some(&owner),
            &write_headers("revoke-a", None),
            Some(&reason),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{problem}");
    let (status, problem, _) = harness
        .send(
            Method::POST,
            &revoke,
            Some(&owner),
            &write_headers("revoke-a", Some("\"v9\"")),
            Some(&reason),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "version_conflict");
    let (status, revoked, headers) = harness
        .send(
            Method::POST,
            &revoke,
            Some(&owner),
            &write_headers("revoke-a", Some("\"v1\"")),
            Some(&reason),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(revoked["status"], "revoked");
    assert_eq!(revoked["revoke_reason"], "contract ended");
    let (status, replay, _) = harness
        .send(
            Method::POST,
            &revoke,
            Some(&owner),
            &write_headers("revoke-a", Some("\"v1\"")),
            Some(&reason),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, revoked);
    let (status, problem, _) = harness
        .send(
            Method::POST,
            &revoke,
            Some(&owner),
            &write_headers("revoke-again", Some("\"v2\"")),
            Some(&reason),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "already_revoked");
    let (status, problem, _) = harness
        .send(
            Method::POST,
            &catalog_uri(
                org,
                &format!("price-overrides/{}/revoke", Uuid::new_v4()),
                "",
            ),
            Some(&owner),
            &write_headers("revoke-unknown", Some("*")),
            Some(&reason),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");

    // Later requests fall back to the catalog price ...
    let after = resolve("fast", "chat").await;
    assert_eq!(after.price_override_id, None);
    assert_eq!(after.source, "test");
    assert_eq!(after.price.input_minor_per_token, 9);

    // ... while past reservations keep the price they were made with, and
    // still pass provenance revalidation when their state changes.
    let accounting = gateway::AccountingStore::new(harness.pool.clone());
    for (id, input, expected_override) in [(before, 3, None), (during, 11, Some(override_id))] {
        sqlx::query("UPDATE gateway.accounting_reservation SET state = 'claimed' WHERE id = $1")
            .bind(id)
            .execute(&harness.pool)
            .await
            .unwrap();
        let reservation = accounting
            .find_reservation(org, gateway::ReservationId(id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reservation.price.input_minor_per_token, input);
        assert_eq!(
            reservation
                .catalog_provenance
                .as_ref()
                .unwrap()
                .price_override_id,
            expected_override
        );
    }

    // The override row itself is immutable once revoked.
    let error = sqlx::query(
        "UPDATE gateway.catalog_price_override SET input_minor_per_token = 1 WHERE id = $1",
    )
    .bind(override_id)
    .execute(&harness.pool)
    .await
    .unwrap_err();
    assert!(error.to_string().contains("immutable"), "{error}");
    let error = sqlx::query(
        "UPDATE gateway.catalog_price_override SET revoke_reason = 'rewritten' WHERE id = $1",
    )
    .bind(override_id)
    .execute(&harness.pool)
    .await
    .unwrap_err();
    assert!(error.to_string().contains("revoked"), "{error}");

    let items = harness
        .collect(&catalog_uri(org, "price-overrides", "?page_size=1"), &owner)
        .await;
    assert_eq!(field(&items, "status"), ["scheduled", "revoked"]);
    assert_eq!(field(&items, "deployment"), ["gpt-b", "gpt-a"]);
    let items = harness
        .collect(
            &catalog_uri(org, "price-overrides", "?deployment=gpt-a&operation=chat"),
            &owner,
        )
        .await;
    assert_eq!(field(&items, "id"), [override_id.to_string()]);
    let (status, problem, _) = harness
        .get(
            &catalog_uri(org, "price-overrides", "?operation=bogus"),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");

    let actions: Vec<_> = audit_rows(&harness.pool, org)
        .await
        .into_iter()
        .map(|(action, outcome, _)| format!("{action}:{outcome}"))
        .collect();
    assert_eq!(
        actions,
        [
            "price_override.created:succeeded",
            "price_override.created:succeeded",
            "price_override.revoked:succeeded",
        ]
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn price_override_writes_are_limited_to_owners_and_admins() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("price-override-rbac").await;
    harness.release(org, &manifest("v1", 3)).await;
    let team = harness.identity.create_team(org, "team").await.unwrap().id;
    let admin = harness.user(org, None, "organization_admin").await;
    let admin = harness.session(admin).await;
    let overrides = catalog_uri(org, "price-overrides", "");
    let (status, created, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&admin),
            &write_headers("admin-create", None),
            Some(&override_body("gpt-a", "chat", 11)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let revoke = catalog_uri(
        org,
        &format!("price-overrides/{}/revoke", created["id"].as_str().unwrap()),
        "",
    );

    for (index, (role, team)) in [
        ("operator", None),
        ("auditor", None),
        ("read_only_member", None),
        ("team_admin", Some(team)),
    ]
    .into_iter()
    .enumerate()
    {
        let user = harness.user(org, team, role).await;
        let session = harness.session(user).await;
        let key = format!("rbac-{index}");
        let (status, body, _) = harness
            .send(
                Method::POST,
                &overrides,
                Some(&session),
                &write_headers(&key, None),
                Some(&override_body("gpt-a", "chat", 1)),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        let (status, body, _) = harness
            .send(
                Method::POST,
                &revoke,
                Some(&session),
                &write_headers(&key, Some("*")),
                Some(&json!({"reason": "no"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        let (status, body, _) = harness.get(&overrides, Some(&session)).await;
        let expected = if role == "team_admin" {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::OK
        };
        assert_eq!(status, expected, "{role}: {body}");
    }

    let (status, body, _) = harness.get(&overrides, Some(&admin)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        field(body["items"].as_array().unwrap(), "status"),
        ["active"]
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn price_override_replays_outlive_the_window_and_guard_activation() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("price-override-guard").await;
    harness.release(org, &manifest("v1", 3)).await;
    let owner = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner).await;
    let overrides = catalog_uri(org, "price-overrides", "");

    // A retry after the window has closed replays the recorded result
    // instead of re-validating the window.
    let mut brief = override_body("gpt-b", "chat", 2);
    brief["effective_until"] =
        json!((chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339());
    let (status, created, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("brief", None),
            Some(&brief),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    tokio::time::sleep(StdDuration::from_millis(1500)).await;
    let (status, replay, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("brief", None),
            Some(&brief),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, created);
    let (_, listed, _) = harness.get(&overrides, Some(&owner)).await;
    assert_eq!(listed["items"][0]["status"], "expired");

    // A reasoning price is fine for OpenAI but not for Anthropic.
    let mut reasoning = override_body("gpt-a", "chat", 11);
    reasoning["reasoning_minor_per_token"] = json!(2);
    let (status, created, _) = harness
        .send(
            Method::POST,
            &overrides,
            Some(&owner),
            &write_headers("reasoning", None),
            Some(&reasoning),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let override_id = created["id"].as_str().unwrap().to_owned();

    // A later release moving `gpt-a` to Anthropic cannot be activated while
    // the override is live, through the API or the CLI.
    let mut moved = manifest_json("moved", 3);
    moved["deployments"][0]["adapter_instance_key"] = json!("anthropic-main");
    moved["deployments"][0]["capabilities"] = json!([capability("chat")]);
    let (status, release, _) = harness
        .send(
            Method::POST,
            &catalog_uri(org, "releases", ""),
            Some(&owner),
            &write_headers("moved", None),
            Some(&moved),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{release}");
    let activate = catalog_uri(
        org,
        &format!("releases/{}/activate", release["id"].as_str().unwrap()),
        "",
    );
    let (status, problem, _) = harness
        .send(
            Method::POST,
            &activate,
            Some(&owner),
            &write_headers("activate-moved", Some("*")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "price_override_incompatible");
    assert!(problem["detail"].as_str().unwrap().contains(&override_id));
    let moved: CatalogManifest = serde_json::from_value(moved).unwrap();
    let error = harness
        .catalog
        .apply(org, &moved, moved.digest().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        gateway::persistence::catalog::CatalogStoreError::IncompatiblePriceOverride(id)
            if id.to_string() == override_id
    ));
    let route = harness
        .catalog
        .resolve_active(org, "fast", "chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.family, "openai");
    assert_eq!(route.price.reasoning_minor_per_token, 2);

    let (status, body, _) = harness
        .send(
            Method::POST,
            &catalog_uri(org, &format!("price-overrides/{override_id}/revoke"), ""),
            Some(&owner),
            &write_headers("revoke-reasoning", Some("\"v1\"")),
            Some(&json!({"reason": "moving to anthropic"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = harness
        .send(
            Method::POST,
            &activate,
            Some(&owner),
            &write_headers("activate-moved-after-revoke", Some("*")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    harness.pool.close().await;
}

#[tokio::test]
async fn a_release_reads_back_as_its_manifest() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("catalog-manifest").await;
    let other = harness.org("catalog-manifest-other").await;
    // Every price dimension and capability limit distinct, so a value
    // dropped or read into another field shows.
    let mut created = manifest_json("v1", 3);
    let price = &mut created["prices"][0];
    assert_eq!(
        (&price["deployment"], &price["operation"]),
        (&json!("gpt-a"), &json!("chat"))
    );
    price["cached_input_minor_per_token"] = json!(11);
    price["reasoning_minor_per_token"] = json!(12);
    price["other_minor_per_token"] = json!(13);
    created["deployments"][0]["capabilities"][0] = json!({
        "operation": "chat", "max_input_bytes": 4097, "max_output_tokens": 33,
        "max_tokens_per_byte": 2, "tokenizer": "configured", "reservation_ttl_seconds": 19,
        "chat_input_count_mode": "unsupported"
    });
    let release = harness
        .release(org, &serde_json::from_value(created.clone()).unwrap())
        .await;
    let foreign = harness.release(other, &manifest("v1", 3)).await;
    let auditor = harness.user(org, None, "auditor").await;
    let auditor = harness.session(auditor).await;
    let uri = |release: Uuid| catalog_uri(org, &format!("releases/{release}/manifest"), "");

    // Every list sorted by its key; the catalog is the one created.
    let (status, body, headers) = harness.get(&uri(release), Some(&auditor)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    let read: CatalogManifest = serde_json::from_value(body.clone()).unwrap();
    let mut expected = created;
    for (list, keys) in [
        ("providers", &["adapter_instance_key"][..]),
        ("deployments", &["name"][..]),
        ("prices", &["deployment", "operation"][..]),
        ("routes", &["alias", "operation"][..]),
    ] {
        expected[list].as_array_mut().unwrap().sort_by_key(|item| {
            keys.iter()
                .map(|key| item[key].to_string())
                .collect::<Vec<_>>()
        });
    }
    for deployment in expected["deployments"].as_array_mut().unwrap() {
        deployment["capabilities"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|capability| capability["operation"].to_string());
    }
    assert_eq!(body, expected);

    // A release created from it reads back unchanged, with its digest.
    let copy_org = harness.org("catalog-manifest-copy").await;
    let copy = harness.release(copy_org, &read).await;
    let copy_owner = harness.user(copy_org, None, "organization_owner").await;
    let copy_owner = harness.session(copy_owner).await;
    let (status, again, _) = harness
        .get(
            &catalog_uri(copy_org, &format!("releases/{copy}/manifest"), ""),
            Some(&copy_owner),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, body);
    let (_, releases, _) = harness
        .get(&catalog_uri(copy_org, "releases", ""), Some(&copy_owner))
        .await;
    assert_eq!(
        releases["items"][0]["digest"],
        json!(read.digest().unwrap().to_string())
    );

    // Another tenant's release, or none, is absent.
    for missing in [foreign, Uuid::new_v4()] {
        let (status, body, _) = harness.get(&uri(missing), Some(&auditor)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "not_found");
    }
    // A user of another organization has no access here.
    let outsider = harness.user(other, None, "organization_owner").await;
    let outsider = harness.session(outsider).await;
    let (status, _, _) = harness.get(&uri(release), Some(&outsider)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = harness.get(&uri(release), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
