#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for configuration pins (migration 0063): the admin lane
//! that lists versions and instances and pins or unpins an organization,
//! driven through the real router, RBAC, idempotency, versioning and audit;
//! an operator's pin through the store; and the retention sweep.
//!
//! The sweep removes old versions of every organization in the database, so
//! this lives in its own test binary and its tests run one at a time.
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
use chrono::{Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, UserAccountId,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    persistence::{AdminStore, AuthStore, IdentityStore, config_versions::ConfigStore},
    transport::config_versions::router,
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
const TEST_PEPPER: &[u8] = b"config-pin-integration-pepper";
const HMAC_KEY: &[u8] = b"config-pin-integration-hmac-key-0032";

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

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
}

impl Harness {
    async fn new() -> Option<Self> {
        let url = test_database_url()?;
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(5))
            .connect(&url)
            .await
            .unwrap();
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[23_u8; 32]).unwrap();
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            ConfigStore::new(pool.clone()),
        );
        Some(Self {
            pool,
            identity,
            crypto,
            router,
        })
    }

    async fn org(&self) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("pins-{suffix}"), "Pins")
            .await
            .unwrap()
            .id
    }

    async fn user(&self, org: OrganizationId, role: &str) -> (String, String) {
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
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
             VALUES ($1, $2, $3, $4, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        self.session(user).await
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

    async fn send(
        &self,
        method: Method,
        uri: &str,
        session: &(String, String),
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            );
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let body = match body {
            Some(body) => Body::from(serde_json::to_vec(body).unwrap()),
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
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value, headers)
    }

    async fn apply(&self, org: OrganizationId, version: &str, alias: &str) {
        let manifest = manifest(version, alias);
        CatalogStore::new(self.pool.clone())
            .apply(org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap();
    }

    /// The organization's versions, oldest first.
    async fn versions(&self, org: OrganizationId) -> Vec<Uuid> {
        sqlx::query_scalar(
            "SELECT f.version_id FROM gateway.config_feed f
             JOIN gateway.config_version v ON v.id = f.version_id
             WHERE f.organization_id = $1 AND f.kind = 'version'
             ORDER BY f.sequence",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn served(&self, org: OrganizationId) -> Uuid {
        ConfigStore::new(self.pool.clone())
            .served_now(org)
            .await
            .unwrap()
            .unwrap()
            .version_id
    }
}

fn manifest(version: &str, alias: &str) -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": version},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "m", "location": null,
                         "adapter_instance_key": "openai-main",
                         "capabilities": [{
                             "operation": "chat", "max_input_bytes": 4096,
                             "max_output_tokens": 64, "max_tokens_per_byte": 1,
                             "tokenizer": "configured", "reservation_ttl_seconds": 60,
                             "chat_input_count_mode": "unsupported"}]}],
        "prices": [{"deployment": "chat-a", "operation": "chat", "input_minor_per_token": 3,
                    "output_minor_per_token": 11, "cached_input_minor_per_token": 0,
                    "reasoning_minor_per_token": 0, "other_minor_per_token": 0,
                    "currency": "USD", "source": "test", "source_version": "v1",
                    "confidence": "known"}],
        "routes": [{"alias": alias, "operation": "chat", "deployment": "chat-a"}]
    }))
    .unwrap()
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/{path}", org.0)
}

fn etag(headers: &HeaderMap) -> &str {
    headers.get("etag").unwrap().to_str().unwrap()
}

fn id(value: &Value) -> Option<Uuid> {
    value.as_str().map(|value| value.parse().unwrap())
}

#[tokio::test]
async fn pins_are_versioned_audited_and_served() {
    let Some(harness) = Harness::new().await else {
        return;
    };
    let _guard = database_guard();
    let org = harness.org().await;
    harness.apply(org, "1.0.0", "chat").await;
    harness.apply(org, "2.0.0", "chat-two").await;
    let [first, second] = harness.versions(org).await[..] else {
        panic!("two versions expected");
    };
    let owner = harness.user(org, "organization_owner").await;
    let operator = harness.user(org, "operator").await;

    let (status, page, _) = harness
        .send(
            Method::GET,
            &uri(org, "config-versions"),
            &operator,
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(id(&items[0]["id"]), Some(second), "newest first");
    assert_eq!(
        (
            &items[0]["served"],
            &items[0]["newest"],
            &items[0]["pinned"]
        ),
        (&json!(true), &json!(true), &json!(false))
    );
    assert_eq!(items[0]["routes"], 1);
    assert_eq!(items[0]["digest"].as_str().unwrap().len(), 64);

    // Paging, newest first.
    let (_, page, _) = harness
        .send(
            Method::GET,
            &format!("{}?page_size=1", uri(org, "config-versions")),
            &operator,
            &[],
            None,
        )
        .await;
    assert_eq!(id(&page["items"][0]["id"]), Some(second));
    let token = page["next_page_token"].as_str().unwrap().to_owned();
    let (_, page, _) = harness
        .send(
            Method::GET,
            &format!(
                "{}?page_size=1&page_token={token}",
                uri(org, "config-versions")
            ),
            &operator,
            &[],
            None,
        )
        .await;
    assert_eq!(id(&page["items"][0]["id"]), Some(first));
    assert!(page["next_page_token"].is_null());

    let pin_uri = uri(org, "config-versions/pin");
    let (status, pin, headers) = harness
        .send(Method::GET, &pin_uri, &operator, &[], None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag(&headers), "\"v0\"");
    assert!(pin["pinned_version_id"].is_null());
    assert_eq!(id(&pin["served_version_id"]), Some(second));

    let body = json!({ "version_id": first });
    let (status, problem, _) = harness
        .send(
            Method::PUT,
            &pin_uri,
            &operator,
            &[("idempotency-key", "op-pin"), ("if-match", "\"v0\"")],
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    let (status, _, _) = harness
        .send(
            Method::PUT,
            &pin_uri,
            &owner,
            &[("idempotency-key", "pin-no-match")],
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);

    let (status, pin, headers) = harness
        .send(
            Method::PUT,
            &pin_uri,
            &owner,
            &[("idempotency-key", "pin-first"), ("if-match", "\"v0\"")],
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pin}");
    assert_eq!(etag(&headers), "\"v1\"");
    assert_eq!(id(&pin["pinned_version_id"]), Some(first));
    assert_eq!(id(&pin["served_version_id"]), Some(first));
    assert_eq!(id(&pin["newest_version_id"]), Some(second));
    assert_eq!(harness.served(org).await, first);

    // A replay returns the recorded response without a second feed entry.
    let (status, replay, _) = harness
        .send(
            Method::PUT,
            &pin_uri,
            &owner,
            &[("idempotency-key", "pin-first"), ("if-match", "\"v0\"")],
            Some(&body),
        )
        .await;
    assert_eq!((status, &replay), (StatusCode::OK, &pin));
    let pins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.config_feed WHERE organization_id = $1 AND kind = 'pin'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(pins, 1);

    // A stale version, and a version of another organization.
    let (status, problem, _) = harness
        .send(
            Method::PUT,
            &pin_uri,
            &owner,
            &[("idempotency-key", "pin-stale"), ("if-match", "\"v0\"")],
            Some(&json!({ "version_id": second })),
        )
        .await;
    assert_eq!(
        (status, problem["code"].as_str()),
        (StatusCode::CONFLICT, Some("version_conflict"))
    );
    let other = harness.org().await;
    harness.apply(other, "1.0.0", "chat").await;
    let foreign = harness.versions(other).await[0];
    let (status, _, _) = harness
        .send(
            Method::PUT,
            &pin_uri,
            &owner,
            &[("idempotency-key", "pin-foreign"), ("if-match", "*")],
            Some(&json!({ "version_id": foreign })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // While pinned, a new version is published but not served.
    harness.apply(org, "3.0.0", "chat-three").await;
    let third = *harness.versions(org).await.last().unwrap();
    assert_eq!(harness.served(org).await, first);
    let (_, pin, _) = harness
        .send(Method::GET, &pin_uri, &operator, &[], None)
        .await;
    assert_eq!(id(&pin["newest_version_id"]), Some(third));
    assert_eq!(id(&pin["served_version_id"]), Some(first));

    let (status, pin, headers) = harness
        .send(
            Method::DELETE,
            &pin_uri,
            &owner,
            &[("idempotency-key", "unpin"), ("if-match", "\"v1\"")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pin}");
    assert_eq!(etag(&headers), "\"v2\"");
    assert!(pin["pinned_version_id"].is_null());
    assert_eq!(harness.served(org).await, third);
    let (status, problem, _) = harness
        .send(
            Method::DELETE,
            &pin_uri,
            &owner,
            &[("idempotency-key", "unpin-again"), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(
        (status, problem["code"].as_str()),
        (StatusCode::CONFLICT, Some("config_not_pinned"))
    );

    let audit: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT action, outcome, entity_id FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'config_pin'
         ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let audit: Vec<(&str, &str, Uuid)> = audit
        .iter()
        .map(|(action, outcome, id)| (action.as_str(), outcome.as_str(), *id))
        .collect();
    assert_eq!(
        audit,
        vec![
            ("config_pin.set", "denied", first),
            ("config_pin.set", "succeeded", first),
            ("config_pin.deleted", "succeeded", org.0),
        ]
    );
}

#[tokio::test]
async fn an_operator_pin_conflicts_with_a_stale_admin_client() {
    let Some(harness) = Harness::new().await else {
        return;
    };
    let _guard = database_guard();
    let org = harness.org().await;
    harness.apply(org, "1.0.0", "chat").await;
    harness.apply(org, "2.0.0", "chat-two").await;
    let first = harness.versions(org).await[0];
    let owner = harness.user(org, "organization_owner").await;

    // As `gateway config pin` does it.
    let mut tx = harness.pool.begin().await.unwrap();
    ConfigStore::lock_organization(&mut tx, org).await.unwrap();
    ConfigStore::bump_pin_version(&mut tx, org).await.unwrap();
    ConfigStore::set_pin(&mut tx, org, Some(first))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(harness.served(org).await, first);

    let (status, problem, _) = harness
        .send(
            Method::DELETE,
            &uri(org, "config-versions/pin"),
            &owner,
            &[("idempotency-key", "unpin-stale"), ("if-match", "\"v0\"")],
            None,
        )
        .await;
    assert_eq!(
        (status, problem["code"].as_str()),
        (StatusCode::CONFLICT, Some("version_conflict"))
    );
    let mut tx = harness.pool.begin().await.unwrap();
    assert!(matches!(
        ConfigStore::set_pin(&mut tx, org, Some(Uuid::new_v4())).await,
        Err(gateway::persistence::config_versions::PinError::VersionNotFound)
    ));
}

#[tokio::test]
async fn instances_show_the_version_they_serve() {
    let Some(harness) = Harness::new().await else {
        return;
    };
    let _guard = database_guard();
    let org = harness.org().await;
    harness.apply(org, "1.0.0", "chat").await;
    let first = harness.versions(org).await[0];
    let before: (i64, Uuid) = sqlx::query_as(
        "SELECT sequence, entry_id FROM gateway.config_feed ORDER BY sequence DESC LIMIT 1",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    harness.apply(org, "2.0.0", "chat-two").await;
    let second = harness.versions(org).await[1];
    let head: (i64, Uuid) = sqlx::query_as(
        "SELECT sequence, entry_id FROM gateway.config_feed ORDER BY sequence DESC LIMIT 1",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let behind = format!("behind-{suffix}");
    let current = format!("current-{suffix}");
    let gone = format!("gone-{suffix}");
    let worker = format!("worker-{suffix}");
    for (instance, profile, position, seen) in [
        (&behind, "data-plane", Some(before), Duration::seconds(5)),
        (&current, "api", Some(head), Duration::seconds(5)),
        (&gone, "api", Some(head), Duration::minutes(10)),
        (&worker, "worker", None, Duration::seconds(5)),
    ] {
        sqlx::query(
            "INSERT INTO gateway.gateway_instance
                 (instance_id, profile, region, build, applied_sequence, applied_entry_id,
                  loaded_at, started_at, last_seen_at)
             VALUES ($1, $2, 'eu', 'abc123', $3, $4, $5, $5, $5)",
        )
        .bind(instance)
        .bind(profile)
        .bind(position.map(|p| p.0))
        .bind(position.map(|p| p.1))
        .bind(Utc::now() - seen)
        .execute(&harness.pool)
        .await
        .unwrap();
    }
    let owner = harness.user(org, "organization_owner").await;
    let (status, page, _) = harness
        .send(
            Method::GET,
            &uri(org, "config-instances"),
            &owner,
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mine: Vec<(&str, Option<Uuid>, bool)> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["instance_id"].as_str().unwrap().ends_with(&suffix))
        .map(|item| {
            (
                item["instance_id"].as_str().unwrap(),
                id(&item["serving_version_id"]),
                item["stale"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        mine,
        vec![
            (behind.as_str(), Some(first), false),
            (current.as_str(), Some(second), false),
            (gone.as_str(), Some(second), true),
        ],
        "workers serve no configuration"
    );

    // Instance rows unseen for seven days are swept.
    sqlx::query("UPDATE gateway.gateway_instance SET last_seen_at = $2 WHERE instance_id = $1")
        .bind(&gone)
        .bind(Utc::now() - Duration::days(8))
        .execute(&harness.pool)
        .await
        .unwrap();
    let store = ConfigStore::new(harness.pool.clone());
    assert!(
        store
            .sweep_instances(Utc::now() - Duration::days(7), 1000)
            .await
            .unwrap()
            >= 1
    );
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT instance_id FROM gateway.gateway_instance WHERE instance_id LIKE '%' || $1
         ORDER BY instance_id",
    )
    .bind(&suffix)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(left, vec![behind, current, worker]);
}

#[tokio::test]
async fn the_sweep_keeps_the_newest_and_pinned_versions() {
    let Some(harness) = Harness::new().await else {
        return;
    };
    let _guard = database_guard();
    let org = harness.org().await;
    harness.apply(org, "1.0.0", "chat").await;
    harness.apply(org, "2.0.0", "chat-two").await;
    harness.apply(org, "3.0.0", "chat-three").await;
    let [first, second, third] = harness.versions(org).await[..] else {
        panic!("three versions expected");
    };
    let mut tx = harness.pool.begin().await.unwrap();
    ConfigStore::set_pin(&mut tx, org, Some(first))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Everything is "old" with a cutoff in the future.
    let store = ConfigStore::new(harness.pool.clone());
    while store
        .sweep_versions(Utc::now() + Duration::minutes(1), 1000)
        .await
        .unwrap()
        > 0
    {}
    let left: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gateway.config_version WHERE organization_id = $1 ORDER BY created_at",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(left, vec![first, third]);
    // The swept version's feed entry stays, still naming it.
    let entries: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT version_id FROM gateway.config_feed
         WHERE organization_id = $1 AND kind = 'version' ORDER BY sequence",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(entries, vec![Some(first), Some(second), Some(third)]);
    // An instance still at the position where the swept version was served
    // is reported as serving it.
    let at_second: i64 = sqlx::query_scalar(
        "SELECT sequence FROM gateway.config_feed WHERE organization_id = $1 AND version_id = $2",
    )
    .bind(org.0)
    .bind(second)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    let reported: Option<Uuid> = sqlx::query_scalar("SELECT gateway.config_served_version($1, $2)")
        .bind(org.0)
        .bind(at_second)
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(reported, Some(second));
    assert!(!left.contains(&second));
    assert_eq!(harness.served(org).await, first);

    // Feed entries stay immutable.
    let error = sqlx::query(
        "UPDATE gateway.config_feed SET kind = 'unpin', version_id = NULL
         WHERE organization_id = $1 AND kind = 'pin'",
    )
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap_err();
    assert!(error.to_string().contains("immutable"), "{error}");

    let mut tx = harness.pool.begin().await.unwrap();
    ConfigStore::set_pin(&mut tx, org, None).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(harness.served(org).await, third);
    // Unpinned, the first version is no longer kept.
    while store
        .sweep_versions(Utc::now() + Duration::minutes(1), 1000)
        .await
        .unwrap()
        > 0
    {}
    assert_eq!(harness.versions(org).await, vec![third]);
}

#[tokio::test]
async fn an_operator_pin_and_an_admin_pin_at_once_do_not_deadlock() {
    let Some(harness) = Harness::new().await else {
        return;
    };
    let _guard = database_guard();
    let org = harness.org().await;
    harness.apply(org, "1.0.0", "chat").await;
    let first = harness.versions(org).await[0];
    // Seed the pin's version row, so both transactions lock an existing row.
    let mut seed = harness.pool.begin().await.unwrap();
    ConfigStore::lock_organization(&mut seed, org)
        .await
        .unwrap();
    ConfigStore::bump_pin_version(&mut seed, org).await.unwrap();
    seed.commit().await.unwrap();

    // The operator's transaction, as `gateway config pin` runs it, starts
    // first...
    let mut operator = harness.pool.begin().await.unwrap();
    ConfigStore::lock_organization(&mut operator, org)
        .await
        .unwrap();
    ConfigStore::bump_pin_version(&mut operator, org)
        .await
        .unwrap();
    // ...and an admin mutation, in the coordinator's order (organization,
    // then the pin's version row), starts before the operator appends its
    // feed entry. It waits on the organization's row.
    let admin = {
        let pool = harness.pool.clone();
        tokio::spawn(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
                .bind(org.0)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "SELECT version FROM gateway.admin_resource_version
                 WHERE organization_id = $1 AND resource_type = 'config_pin'
                   AND resource_id = $1 FOR UPDATE",
            )
            .bind(org.0)
            .execute(&mut *tx)
            .await?;
            ConfigStore::set_pin(&mut tx, org, None)
                .await
                .map_err(|error| match error {
                    gateway::persistence::config_versions::PinError::Database(error) => error,
                    other => panic!("{other}"),
                })?;
            tx.commit().await
        })
    };
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    ConfigStore::set_pin(&mut operator, org, Some(first))
        .await
        .unwrap();
    operator.commit().await.unwrap();
    tokio::time::timeout(StdDuration::from_secs(10), admin)
        .await
        .expect("the admin mutation finishes")
        .unwrap()
        .expect("no deadlock");
    // The admin's unpin committed last.
    assert_eq!(harness.served(org).await, harness.versions(org).await[0]);
    let pinned: Option<Uuid> = sqlx::query_scalar("SELECT gateway.config_pinned_version($1)")
        .bind(org.0)
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(pinned, None);
}
