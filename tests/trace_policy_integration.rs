#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the tracing policy API: RBAC, validation of the
//! selection, versioning, idempotency and audit, through the real router.
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
    OrganizationId, UserAccountId,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    persistence::{AdminStore, AuthStore, IdentityStore, trace_policy::TracePolicyStore},
    transport::trace_policy::router,
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
const TEST_PEPPER: &[u8] = b"trace-policy-integration-pepper";
const HMAC_KEY: &[u8] = b"trace-policy-integration-hmac-k032";

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

struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(5))
            .connect(database_url)
            .await
            .unwrap_or_else(|error| panic!("GATEWAY_TEST_DATABASE_URL is unreachable: {error}"));
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[41_u8; 32]).unwrap();
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            TracePolicyStore::new(pool.clone()),
        );
        Self {
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

    /// A user with one admin role binding: org-wide, or `team_admin` of a
    /// new team when `role` is `team_admin`.
    async fn user(&self, org: OrganizationId, role: &str) -> UserAccountId {
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
        let team = if role == "team_admin" {
            let team = self.identity.create_team(org, "team").await.unwrap().id;
            self.identity
                .add_team_membership(org, team, user, MembershipRole::Admin)
                .await
                .unwrap();
            Some(team.0)
        } else {
            None
        };
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(team)
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> String {
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
        format!(
            "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={browser}",
            session.token
        )
    }

    async fn as_role(&self, org: OrganizationId, role: &str) -> String {
        let user = self.user(org, role).await;
        self.session(user).await
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        cookie: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(COOKIE, cookie);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let response = self
            .router
            .clone()
            .oneshot(
                builder
                    .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Response {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn put_policy(
        &self,
        org: OrganizationId,
        cookie: &str,
        key: &str,
        if_match: &str,
        body: Value,
    ) -> Response {
        self.send(
            Method::PUT,
            &policy_uri(org),
            cookie,
            &[("idempotency-key", key), ("if-match", if_match)],
            Some(body),
        )
        .await
    }

    async fn audit_actions(&self, org: OrganizationId, entity_type: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, outcome FROM gateway.audit_event
             WHERE organization_id = $1 AND entity_type = $2 ORDER BY occurred_at, id",
        )
        .bind(org.0)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn policy_uri(org: OrganizationId) -> String {
    format!("/api/admin/v1/organizations/{}/trace-policy", org.0)
}

#[tokio::test]
async fn tenant_admins_choose_what_is_traced() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("trace-policy").await;
    let other = harness.org("trace-policy-other").await;
    let admin_user = harness.user(org, "organization_admin").await;
    let admin = harness.session(admin_user).await;
    let reader = harness.as_role(org, "read_only_member").await;

    // Unset: nothing is traced; content would be kept 30 days once it is.
    let response = harness
        .send(Method::GET, &policy_uri(org), &reader, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let policy = response.json();
    assert_eq!(policy["mode"], "off");
    assert_eq!(policy["content"], true);
    assert_eq!(policy["content_retention_days"], 30);
    assert_eq!(policy["version"], 0);
    assert!(response.headers.get("etag").is_none());
    assert_eq!(response.headers["cache-control"], "no-store");

    let body = json!({"mode": "organization"});
    let response = harness
        .put_policy(org, &reader, "reader", "*", body.clone())
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    let created = harness
        .put_policy(org, &admin, "set-1", "*", body.clone())
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.headers["etag"], "\"v1\"");
    let policy = created.json();
    assert_eq!(policy["mode"], "organization");
    assert_eq!(policy["content"], true);
    assert_eq!(policy["content_retention_days"], 30);
    let replayed = harness
        .put_policy(org, &admin, "set-1", "*", body.clone())
        .await;
    assert_eq!(replayed.status, StatusCode::OK);
    assert_eq!(replayed.json(), created.json());

    let team = harness
        .identity
        .create_team(org, "traced")
        .await
        .unwrap()
        .id;
    let foreign = harness
        .identity
        .create_team(other, "elsewhere")
        .await
        .unwrap()
        .id;
    let account = harness
        .identity
        .create_service_account(org, admin_user, None, "traced-bot")
        .await
        .unwrap()
        .id;
    for (body, code) in [
        (json!({"mode": "selected"}), "invalid_trace_policy"),
        (
            json!({"mode": "organization", "team_ids": [team.0]}),
            "invalid_trace_policy",
        ),
        (
            json!({"mode": "selected", "team_ids": [foreign.0]}),
            "invalid_trace_policy",
        ),
        (
            json!({"mode": "selected", "service_account_ids": [Uuid::new_v4()]}),
            "invalid_trace_policy",
        ),
        (
            json!({"mode": "organization", "content_retention_days": 0}),
            "invalid_trace_policy",
        ),
        (json!({"mode": "sometimes"}), "request_body_unprocessable"),
        (
            json!({"mode": "organization", "sample": 0.5}),
            "request_body_unprocessable",
        ),
    ] {
        let response = harness.put_policy(org, &admin, "bad", "*", body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            response.body
        );
        assert_eq!(response.json()["code"], code, "{}", response.body);
    }
    let stale = harness
        .put_policy(org, &admin, "set-2", "\"v7\"", json!({"mode": "off"}))
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);

    let updated = harness
        .put_policy(
            org,
            &admin,
            "set-3",
            "\"v1\"",
            json!({
                "mode": "selected",
                "team_ids": [team.0, team.0],
                "service_account_ids": [account.0],
                "content": false,
                "content_retention_days": 7,
            }),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.headers["etag"], "\"v2\"");
    let response = harness
        .send(Method::GET, &policy_uri(org), &reader, &[], None)
        .await;
    assert_eq!(response.headers["etag"], "\"v2\"");
    let policy = response.json();
    assert_eq!(policy["mode"], "selected");
    assert_eq!(policy["team_ids"], json!([team.0]));
    assert_eq!(policy["service_account_ids"], json!([account.0]));
    assert_eq!(policy["content"], false);
    assert_eq!(policy["content_retention_days"], 7);

    // A team binding cannot read an organization-wide policy.
    let team_admin = harness.as_role(org, "team_admin").await;
    let response = harness
        .send(Method::GET, &policy_uri(org), &team_admin, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    assert_eq!(
        harness.audit_actions(org, "trace_policy").await,
        [
            ("trace_policy.set".to_owned(), "denied".to_owned()),
            ("trace_policy.set".to_owned(), "succeeded".to_owned()),
            ("trace_policy.set".to_owned(), "succeeded".to_owned()),
        ]
    );
}
