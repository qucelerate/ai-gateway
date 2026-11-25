#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the service-identity admin module.
//!
//! The module is registered in the gateway crate; this test crate drives the
//! real `gateway::transport::admin_identities::router` with the real gateway
//! stores, coordinator, RBAC, and schema.
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
    http::{HeaderMap, StatusCode, header::COOKIE},
};
use gateway::{
    domain::{
        auth::AuthCrypto,
        identity::{ApiKeyId, ApiKeyScope, MembershipRole},
        keys::ApiKeyPepper,
    },
    persistence::{AdminStore, AuthStore, IdentityStore},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Registered modules.
// ---------------------------------------------------------------------------

use gateway::persistence::service_identities::ServiceIdentityStore;
use gateway::transport::admin_identities::router;

// ---------------------------------------------------------------------------
// Shared test harness.
// ---------------------------------------------------------------------------

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SESSION_COOKIE: &str = "__Host-gateway_session";
const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";
const TEST_PEPPER: &[u8] = b"admin-identities-integration-pepper";
const HMAC_KEY: &[u8] = b"admin-identities-integration-hmac-key-32";

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

async fn apply_migrations(pool: &PgPool) {
    gateway::persistence::migrations::run(pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
}

/// Re-run the migration's compatibility backfill for memberships created after
/// migrations were applied.
async fn backfill_admin_role_bindings(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
         SELECT md5(organization_id::text || ':' || user_id::text)::uuid, organization_id, user_id,
                CASE role
                    WHEN 'owner' THEN 'organization_owner'
                    WHEN 'admin' THEN 'organization_admin'
                    ELSE 'auditor'
                END,
                enabled
         FROM gateway.organization_membership
         WHERE role IN ('owner', 'admin', 'auditor')
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
         SELECT md5(organization_id::text || ':' || team_id::text || ':' || user_id::text)::uuid,
                organization_id, user_id, team_id, 'team_admin', enabled
         FROM gateway.team_membership
         WHERE role IN ('owner', 'admin')
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_version(
    pool: &PgPool,
    organization_id: Uuid,
    resource_type: &str,
    resource_id: Uuid,
) {
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
         VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .execute(pool)
    .await
    .unwrap();
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
}

impl Harness {
    fn new(pool: PgPool) -> Self {
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let admin = AdminStore::new(pool.clone());
        let auth = AuthStore::new(pool.clone());
        let crypto = AuthCrypto::new(HMAC_KEY, &[11_u8; 32]).unwrap();
        let store = ServiceIdentityStore::new(pool.clone(), identity.clone());
        let router = router(auth.clone(), admin.clone(), Some(crypto.clone()), store);
        Self {
            pool,
            identity,
            crypto,
            router,
        }
    }

    async fn create_org(&self, slug: &str) -> gateway::OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id
    }

    async fn create_user(
        &self,
        org: gateway::OrganizationId,
        name: &str,
    ) -> gateway::UserAccountId {
        self.identity
            .create_user(org, None, None, None, Some(name))
            .await
            .unwrap()
            .id
    }

    async fn add_org_membership(
        &self,
        org: gateway::OrganizationId,
        user: gateway::UserAccountId,
        role: MembershipRole,
    ) {
        self.identity
            .add_organization_membership(org, user, role)
            .await
            .unwrap();
    }

    async fn add_team_membership(
        &self,
        org: gateway::OrganizationId,
        team: gateway::TeamId,
        user: gateway::UserAccountId,
        role: MembershipRole,
    ) {
        self.identity
            .add_team_membership(org, team, user, role)
            .await
            .unwrap();
    }

    async fn create_team(&self, org: gateway::OrganizationId, name: &str) -> gateway::TeamId {
        self.identity.create_team(org, name).await.unwrap().id
    }

    /// Create a session for a user and return the cookie header value pair
    /// `(session_token, browser_token)`.
    async fn session(&self, user: gateway::UserAccountId) -> (String, String) {
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

    /// Seed a service account directly (skipping the HTTP path) so fixtures can
    /// create rows the acting user would not be able to see.
    async fn seed_service_account(
        &self,
        org: gateway::OrganizationId,
        owner: gateway::UserAccountId,
        team: Option<gateway::TeamId>,
        name: &str,
    ) -> gateway::ServiceAccountId {
        let account = self
            .identity
            .create_service_account(org, owner, team, name)
            .await
            .unwrap();
        seed_version(&self.pool, org.0, "service_account", account.id.0).await;
        account.id
    }

    /// Seed an API key directly, with its version row, so fixtures can create
    /// keys outside the acting user's reach.
    async fn seed_api_key(
        &self,
        org: gateway::OrganizationId,
        service_account: gateway::ServiceAccountId,
        actor: gateway::UserAccountId,
    ) -> (ApiKeyId, String) {
        let key = self
            .identity
            .create_api_key(org, service_account, &[ApiKeyScope::Inference], None, actor)
            .await
            .unwrap();
        seed_version(&self.pool, org.0, "api_key", key.id.0).await;
        (key.id, key.bearer)
    }
}

fn request(
    method: &str,
    uri: &str,
    session: Option<&(String, String)>,
    idempotency_key: Option<&str>,
    if_match: Option<&str>,
    body: Option<Value>,
) -> axum::http::Request<Body> {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        builder = builder.header(
            COOKIE,
            format!(
                "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                session.0, session.1
            ),
        );
    }
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    if let Some(version) = if_match {
        builder = builder.header("if-match", version);
    }
    if let Some(value) = body {
        builder = builder.header("content-type", "application/json");
        builder.body(Body::from(value.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

async fn send(
    router: &Router,
    request: axum::http::Request<Body>,
) -> (StatusCode, Value, HeaderMap) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body, headers)
}

fn uri_service_accounts(org: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts")
}
fn uri_service_account(org: Uuid, account: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts/{account}")
}
fn uri_deactivate(org: Uuid, account: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts/{account}/deactivate")
}
fn uri_keys(org: Uuid, account: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts/{account}/keys")
}
fn uri_key(org: Uuid, account: Uuid, key: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts/{account}/keys/{key}")
}
fn uri_rotate(org: Uuid, account: Uuid, key: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts/{account}/keys/{key}/rotate")
}
fn uri_revoke(org: Uuid, account: Uuid, key: Uuid) -> String {
    format!("/api/admin/v1/organizations/{org}/service-accounts/{account}/keys/{key}/revoke")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn human_session_only_and_authentication_required() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("auth").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;

    // No cookies: 401.
    let (status, body, _) = send(
        &harness.router,
        request("GET", &uri_service_accounts(org.0), None, None, None, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "authentication_required");
    assert!(body.get("request_id").is_some());

    // A valid gateway API-key bearer must NOT authenticate the control plane.
    let service_account = harness.seed_service_account(org, owner, None, "seed").await;
    let (_, bearer) = harness.seed_api_key(org, service_account, owner).await;
    let mut bearer_request = request("GET", &uri_service_accounts(org.0), None, None, None, None);
    bearer_request
        .headers_mut()
        .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    let (status, body, _) = send(&harness.router, bearer_request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "authentication_required");

    pool.close().await;
}

#[tokio::test]
async fn cross_tenant_and_cross_team_denials_leak_nothing() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org_a = harness.create_org("deny-a").await;
    let org_b = harness.create_org("deny-b").await;
    let owner_a = harness.create_user(org_a, "owner-a").await;
    harness
        .add_org_membership(org_a, owner_a, MembershipRole::Owner)
        .await;
    let owner_b = harness.create_user(org_b, "owner-b").await;
    harness
        .add_org_membership(org_b, owner_b, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session_a = harness.session(owner_a).await;
    let session_b = harness.session(owner_b).await;

    // Org A has one org-level SA and two team-scoped SAs (team A, team B).
    let org_sa = harness
        .seed_service_account(org_a, owner_a, None, "org-wide")
        .await;
    let team_a = harness.create_team(org_a, "Team A").await;
    let team_b = harness.create_team(org_a, "Team B").await;
    // The owner must hold team memberships to own team-scoped service accounts.
    harness
        .add_team_membership(org_a, team_a, owner_a, MembershipRole::Owner)
        .await;
    harness
        .add_team_membership(org_a, team_b, owner_a, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let team_a_sa = harness
        .seed_service_account(org_a, owner_a, Some(team_a), "team-a")
        .await;
    let team_b_sa = harness
        .seed_service_account(org_a, owner_a, Some(team_b), "team-b")
        .await;
    let (team_b_key, team_b_bearer) = harness.seed_api_key(org_a, team_b_sa, owner_a).await;

    // Cross-tenant: owner B has no bindings in org A -> every access is denied.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_accounts(org_a.0),
            Some(&session_b),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_account(org_a.0, org_sa.0),
            Some(&session_b),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org_a.0),
            Some(&session_b),
            Some("deny-create"),
            None,
            Some(json!({ "name": "cross-tenant" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org_a.0, org_sa.0),
            Some(&session_b),
            Some("deny-delete"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A team-scoped admin of team A cannot see or mutate team B rows.
    let team_admin = harness.create_user(org_a, "team-admin").await;
    harness
        .add_org_membership(org_a, team_admin, MembershipRole::Member)
        .await;
    harness
        .add_team_membership(org_a, team_a, team_admin, MembershipRole::Admin)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session_team = harness.session(team_admin).await;

    // Team-scoped list only surfaces the team's own rows, never org-level rows.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_accounts(org_a.0),
            Some(&session_team),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "team-scoped list must contain only the bound team's rows: {body}"
    );
    assert_eq!(items[0]["id"], json!(team_a_sa.0.to_string()));
    assert!(
        items
            .iter()
            .all(|item| item["team_id"] == json!(team_a.0.to_string())),
        "team-scoped list must be clamped to the bound team: {body}"
    );

    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_account(org_a.0, team_b_sa.0),
            Some(&session_team),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-team detail must not leak existence: {body}"
    );
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_account(org_a.0, org_sa.0),
            Some(&session_team),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "PATCH",
            &uri_service_account(org_a.0, team_b_sa.0),
            Some(&session_team),
            Some("deny-patch"),
            Some("\"1\""),
            Some(json!({ "name": "hijack" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_keys(org_a.0, team_b_sa.0),
            Some(&session_team),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org_a.0, team_b_sa.0, team_b_key.0),
            Some(&session_team),
            Some("deny-revoke"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // The team-B key must remain active and its bearer must still verify.
    let (status, _, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_key(org_a.0, team_b_sa.0, team_b_key.0),
            Some(&session_a),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let verified = harness
        .identity
        .verify_bearer_key(&team_b_bearer, ApiKeyScope::Inference)
        .await
        .unwrap();
    assert!(
        verified.is_some(),
        "cross-team denial must not disable the key"
    );

    // The team admin CAN create inside their own team; scope is server-derived.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org_a.0),
            Some(&session_team),
            Some("team-create"),
            None,
            Some(json!({ "name": "own-team-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["team_id"], json!(team_a.0.to_string()));
    assert_eq!(body["name"], "own-team-bot");

    pool.close().await;
}

#[tokio::test]
async fn conditional_mutations_require_if_match_and_reject_stale_versions() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("stale").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa"),
            None,
            Some(json!({ "name": "stale-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["version"], 1);
    assert_eq!(body["enabled"], true);

    // Missing If-Match on a conditional mutation -> 428.
    let (status, body, _) = send(
        &harness.router,
        request(
            "PATCH",
            &uri_service_account(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            Some("patch-no-ifmatch"),
            None,
            Some(json!({ "name": "new-name" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{body}");
    assert_eq!(body["code"], "if_match_required");

    // Successful PATCH bumps the version.
    let (status, body, _) = send(
        &harness.router,
        request(
            "PATCH",
            &uri_service_account(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            Some("patch-1"),
            Some("\"1\""),
            Some(json!({ "name": "new-name" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "new-name");
    assert_eq!(body["version"], 2);

    // Stale If-Match -> 409 version conflict, nothing changes.
    let (status, body, _) = send(
        &harness.router,
        request(
            "PATCH",
            &uri_service_account(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            Some("patch-stale"),
            Some("\"1\""),
            Some(json!({ "name": "overwrite" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_deactivate(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            Some("deactivate-stale"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_account(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["name"], "new-name",
        "stale PATCH must not have applied"
    );
    assert_eq!(
        body["enabled"], true,
        "stale deactivate must not have applied"
    );

    // Deactivate with the current version succeeds.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_deactivate(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            Some("deactivate-2"),
            Some("\"2\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["version"], 3);

    // Key revoke honors If-Match and rejects a stale version.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, Uuid::parse_str(&account_id).unwrap()),
            Some(&session),
            Some("create-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key_id = body["metadata"]["id"].as_str().unwrap().to_owned();
    let key_uuid = Uuid::parse_str(&key_id).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org.0, Uuid::parse_str(&account_id).unwrap(), key_uuid),
            Some(&session),
            Some("revoke-1"),
            Some("\"2\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org.0, Uuid::parse_str(&account_id).unwrap(), key_uuid),
            Some(&session),
            Some("revoke-ok"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "revoked");
    assert_eq!(body["version"], 2);

    pool.close().await;
}

#[tokio::test]
async fn pagination_cursor_binds_to_scope_and_rejects_tampering() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("cursor").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    for (index, name) in ["bot-a", "bot-b", "bot-c"].iter().enumerate() {
        let (status, body, _) = send(
            &harness.router,
            request(
                "POST",
                &uri_service_accounts(org.0),
                Some(&session),
                Some(&format!("cursor-create-{index}")),
                None,
                Some(json!({ "name": name })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // First page.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!("{}?page_size=2", uri_service_accounts(org.0)),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first = body["items"].as_array().unwrap().clone();
    assert_eq!(first.len(), 2);
    let token = body["next_page_token"].as_str().unwrap().to_owned();

    // Second page continues where the first left off.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!(
                "{}?page_size=2&page_token={}",
                uri_service_accounts(org.0),
                token
            ),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = body["items"].as_array().unwrap();
    assert_eq!(second.len(), 1);
    assert!(
        body.get("next_page_token")
            .map(Value::is_null)
            .unwrap_or(true),
        "the final page must carry no continuation: {body}"
    );
    let first_ids: Vec<String> = first
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    let second_ids: Vec<String> = second
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        first_ids.iter().all(|id| !second_ids.contains(id)),
        "pages must not overlap"
    );

    // Tampering is rejected with a signature problem.
    let mut tampered = token.clone();
    let last = tampered.len() - 1;
    let replacement = if tampered.ends_with('A') { "B" } else { "A" };
    tampered.replace_range(last..last + 1, replacement);
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!(
                "{}?page_size=2&page_token={tampered}",
                uri_service_accounts(org.0)
            ),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_page_token_signature");

    // Cursor scope is bound to the organization: reuse from another tenant
    // fails with a scope mismatch even when the token is valid.
    let org_b = harness.create_org("cursor-b").await;
    let owner_b = harness.create_user(org_b, "owner-b").await;
    harness
        .add_org_membership(org_b, owner_b, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session_b = harness.session(owner_b).await;
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!(
                "{}?page_size=2&page_token={token}",
                uri_service_accounts(org_b.0)
            ),
            Some(&session_b),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "page_token_scope_mismatch");

    // An API-key collection cursor is bound to the parent service account.
    let account_id = first[0]["id"].as_str().unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, Uuid::parse_str(account_id).unwrap()),
            Some(&session),
            Some("key-one"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, Uuid::parse_str(account_id).unwrap()),
            Some(&session),
            Some("key-two"),
            None,
            Some(json!({ "scopes": ["Embeddings"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!(
                "{}?page_size=1",
                uri_keys(org.0, Uuid::parse_str(account_id).unwrap())
            ),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let key_token = body["next_page_token"].as_str().unwrap().to_owned();

    let other_account = Uuid::parse_str(&second[0]["id"].as_str().unwrap()).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!(
                "{}?page_size=1&page_token={key_token}",
                uri_keys(org.0, other_account)
            ),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "page_token_query_mismatch");

    // Out-of-bounds page sizes are rejected at extraction.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!("{}?page_size=0", uri_service_accounts(org.0)),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "pagination_limit_invalid");

    pool.close().await;
}

#[tokio::test]
async fn key_plaintext_is_one_time_and_redacted_everywhere() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("secret").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa"),
            None,
            Some(json!({ "name": "key-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let (status, body, headers) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("create-key"),
            None,
            Some(json!({ "scopes": ["Inference", "CatalogRead"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store, no-cache, private")
    );
    let secret = body["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("gw_"), "bearer must be a gateway bearer");
    assert_eq!(body["replay"], false);
    let key_id = body["metadata"]["id"].as_str().unwrap().to_owned();
    let key_uuid = Uuid::parse_str(&key_id).unwrap();
    let scopes = body["metadata"]["scopes"].as_array().unwrap().clone();
    assert_eq!(scopes.len(), 2);

    // List must not expose the bearer or a secret field.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_keys(org.0, account_id),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rendered = body.to_string();
    assert!(
        !rendered.contains(&secret),
        "list must never contain the bearer"
    );
    assert!(
        !rendered.contains("\"secret\""),
        "list must not carry a secret field: {rendered}"
    );
    assert!(rendered.contains("public_prefix"), "{rendered}");

    // Detail must not expose the bearer either.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_key(org.0, account_id, key_uuid),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rendered = body.to_string();
    assert!(
        !rendered.contains(&secret),
        "detail must never contain the bearer"
    );

    // An idempotent replay returns metadata with secret_unavailable_on_replay
    // and no plaintext; it must not create a second key.
    let (status, body, headers) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("create-key"),
            None,
            Some(json!({ "scopes": ["Inference", "CatalogRead"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["replay"], true);
    assert_eq!(body["error"], "secret_unavailable_on_replay");
    assert!(body["secret"].is_null());
    assert_eq!(body["metadata"]["id"], json!(key_id));
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store, no-cache, private")
    );

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.api_key WHERE organization_id = $1 AND service_account_id = $2",
    )
    .bind(org.0)
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "replay must not create another key");

    pool.close().await;
}

#[tokio::test]
async fn rotation_keeps_predecessor_active_and_revoke_is_explicit() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("rotate").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa"),
            None,
            Some(json!({ "name": "rotate-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("create-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let predecessor_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let predecessor_bearer = body["secret"].as_str().unwrap().to_owned();

    // Rotate: no If-Match needed (create-mode), returns a fresh secret.
    let (status, body, headers) = send(
        &harness.router,
        request(
            "POST",
            &uri_rotate(org.0, account_id, predecessor_id),
            Some(&session),
            Some("rotate-1"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store, no-cache, private")
    );
    let successor_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let successor_bearer = body["secret"].as_str().unwrap().to_owned();
    assert_ne!(successor_id, predecessor_id);
    assert_eq!(
        body["metadata"]["successor_of"],
        json!(predecessor_id.to_string())
    );
    assert_eq!(body["metadata"]["scopes"], json!(["Inference"]));

    // The predecessor is still active (non-revoking rotation).
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_key(org.0, account_id, predecessor_id),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "active");
    assert!(body["revoked_at"].is_null());

    // Both keys authenticate.
    let verified = harness
        .identity
        .verify_bearer_key(&predecessor_bearer, ApiKeyScope::Inference)
        .await
        .unwrap();
    assert!(verified.is_some());
    let verified = harness
        .identity
        .verify_bearer_key(&successor_bearer, ApiKeyScope::Inference)
        .await
        .unwrap();
    assert!(verified.is_some());

    // Explicit revoke of the successor leaves the predecessor untouched.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org.0, account_id, successor_id),
            Some(&session),
            Some("revoke-succ"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "revoked");
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_key(org.0, account_id, predecessor_id),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "active");

    // Revoking an already-revoked key is rejected as invalid state.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org.0, account_id, successor_id),
            Some(&session),
            Some("revoke-again"),
            Some("\"2\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "invalid_state");

    // Rotating a revoked key is also rejected.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_rotate(org.0, account_id, successor_id),
            Some(&session),
            Some("rotate-revoked"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "invalid_state");

    pool.close().await;
}

#[tokio::test]
async fn idempotency_replays_metadata_and_conflicts_on_change() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("idem").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    // Service-account create is idempotent: a replay returns the same row.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("sa-idem-key"),
            None,
            Some(json!({ "name": "idem-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sa_id = body["id"].as_str().unwrap().to_owned();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("sa-idem-key"),
            None,
            Some(json!({ "name": "idem-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(sa_id));
    let sa_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.service_account WHERE organization_id = $1 AND name = 'idem-bot'",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        sa_count, 1,
        "replay must not create a second service account"
    );

    // Key create replay returns the same resource id and secret_unavailable.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, Uuid::parse_str(&sa_id).unwrap()),
            Some(&session),
            Some("key-idem-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key_id = body["metadata"]["id"].as_str().unwrap().to_owned();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, Uuid::parse_str(&sa_id).unwrap()),
            Some(&session),
            Some("key-idem-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["replay"], true);
    assert_eq!(body["error"], "secret_unavailable_on_replay");
    assert_eq!(body["metadata"]["id"], json!(key_id));

    // The same Idempotency-Key with a different body is a conflict.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, Uuid::parse_str(&sa_id).unwrap()),
            Some(&session),
            Some("key-idem-key"),
            None,
            Some(json!({ "scopes": ["Embeddings"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "idempotency_conflict");

    // Reusing the create key against a different service account is a conflict
    // because the canonical fingerprint contains the parent service-account.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("sa-two"),
            None,
            Some(json!({ "name": "second-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let second_sa = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, second_sa),
            Some(&session),
            Some("key-idem-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "idempotency_conflict");

    pool.close().await;
}

#[tokio::test]
async fn audit_and_idempotency_persistence_is_secret_free() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("audit").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("audit-sa"),
            None,
            Some(json!({ "name": "audit-bot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("audit-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["secret"].as_str().unwrap().to_owned();
    let key_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let (status, _, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org.0, account_id, key_id),
            Some(&session),
            Some("audit-revoke"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Audit events must never persist the bearer, digest, or plaintext.
    let forbidden = [
        "bearer",
        "secret",
        "digest",
        "pepper",
        "plaintext_key",
        "idempotency_key",
    ];
    for column in ["metadata", "before_metadata", "after_metadata"] {
        let rows: Vec<Value> = sqlx::query_scalar(
            &format!(
                "SELECT COALESCE({column}, '{{}}'::JSONB) FROM gateway.audit_event WHERE organization_id = $1"
            ),
        )
        .bind(org.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        for row in rows {
            let rendered = row.to_string();
            assert!(
                !rendered.contains(&secret),
                "{column} must never contain the bearer"
            );
            for key in forbidden {
                assert!(
                    !rendered.contains(&format!("\"{key}\"")),
                    "{column} must not contain {key}: {rendered}"
                );
            }
        }
    }

    // Idempotency response metadata is likewise secret-free.
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT COALESCE(response_metadata, '{}'::JSONB) FROM gateway.admin_idempotency_record WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    for row in rows {
        let rendered = row.to_string();
        assert!(
            !rendered.contains(&secret),
            "idempotency metadata must never contain the bearer"
        );
        for key in forbidden {
            assert!(
                !rendered.contains(&format!("\"{key}\"")),
                "idempotency metadata must not contain {key}: {rendered}"
            );
        }
    }

    pool.close().await;
}

#[tokio::test]
async fn ambiguous_multi_team_create_is_denied_without_guessing() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("multi").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    let team_a = harness.create_team(org, "Multi A").await;
    let team_b = harness.create_team(org, "Multi B").await;
    let multi = harness.create_user(org, "multi").await;
    harness
        .add_org_membership(org, multi, MembershipRole::Member)
        .await;
    harness
        .add_team_membership(org, team_a, multi, MembershipRole::Admin)
        .await;
    harness
        .add_team_membership(org, team_b, multi, MembershipRole::Admin)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(multi).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("multi-create"),
            None,
            Some(json!({ "name": "which-team" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ambiguous_team_scope");

    pool.close().await;
}

#[tokio::test]
async fn service_account_delete_removes_row_and_is_idempotent() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("delete").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa"),
            None,
            Some(json!({ "name": "doomed" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let version = body["version"].as_u64().unwrap();

    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa"),
            Some(&format!("\"{version}\"")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.service_account WHERE organization_id = $1 AND id = $2",
    )
    .bind(org.0)
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);

    // A replayed delete (same Idempotency-Key) reports success without error.
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa"),
            Some(&format!("\"{version}\"")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);

    // A fresh delete of the removed account is not a replay and stays 404.
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa-again"),
            Some(&format!("\"{version}\"")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Detail after delete is 404.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &uri_service_account(org.0, account_id),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    pool.close().await;
}

#[tokio::test]
async fn service_account_delete_rejects_active_keys_and_survives_revoke() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("delete-keys").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    // Create a service account and an active API key through the API.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa"),
            None,
            Some(json!({ "name": "guarded" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("create-key"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let bearer = body["secret"].as_str().unwrap().to_owned();

    // Deletion is rejected with a conflict while the key is active, and the
    // key remains valid (never cascade-deleted).
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "active_api_keys_exist");
    let verified = harness
        .identity
        .verify_bearer_key(&bearer, ApiKeyScope::Inference)
        .await
        .unwrap();
    assert!(
        verified.is_some(),
        "active key must survive the rejected delete"
    );

    // Revoking the key makes the deletion safe.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_revoke(org.0, account_id, key_id),
            Some(&session),
            Some("revoke-1"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa-2"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.api_key WHERE service_account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "revoked keys must be removed with the account"
    );

    // Successor keys are covered too: rotate then attempt deletion.
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa2"),
            None,
            Some(json!({ "name": "guarded-rotate" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("create-key2"),
            None,
            Some(json!({ "scopes": ["Inference"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let predecessor_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_rotate(org.0, account_id, predecessor_id),
            Some(&session),
            Some("rotate-2"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let successor_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    // Both predecessor and successor are active -> deletion must conflict.
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa3"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "active_api_keys_exist");
    for key_id in [predecessor_id, successor_id] {
        let (status, body, _) = send(
            &harness.router,
            request(
                "POST",
                &uri_revoke(org.0, account_id, key_id),
                Some(&session),
                Some(&format!("revoke-{key_id}")),
                Some("\"1\""),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body, _) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-sa4"),
            Some("\"1\""),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    pool.close().await;
}

#[tokio::test]
async fn rotate_expired_key_is_a_conflict_not_a_server_error() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("expire-rotate").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-sa"),
            None,
            Some(json!({ "name": "expiring" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_keys(org.0, account_id),
            Some(&session),
            Some("create-key"),
            None,
            Some(json!({ "scopes": ["Inference"], "expires_at": expires_at })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key_id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();

    // Wait for the key to expire, then rotate: this is a client-state conflict
    // (409 invalid_state), never a 5xx from the shared key primitive.
    tokio::time::sleep(StdDuration::from_millis(2300)).await;
    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_rotate(org.0, account_id, key_id),
            Some(&session),
            Some("rotate-expired"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "invalid_state");
    assert!(
        status.as_u16() < 500,
        "expired-key rotation must not be a 5xx"
    );

    pool.close().await;
}

#[tokio::test]
async fn service_account_with_usage_is_disabled_instead_of_deleted() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("delete-used").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;

    let (status, body, _) = send(
        &harness.router,
        request(
            "POST",
            &uri_service_accounts(org.0),
            Some(&session),
            Some("create-used-sa"),
            None,
            Some(json!({ "name": "used" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let version = body["version"].as_u64().unwrap();

    // One request's accounting: its reservation and the append-only usage.
    let reservation = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.accounting_reservation
           (id, organization_id, service_account_id, idempotency_key, fingerprint, state,
            reserved_minor, expires_at, output_token_cap, input_price_minor, output_price_minor,
            cached_input_price_minor, reasoning_price_minor, other_price_minor, price_version,
            pricing_confidence, currency, deployment, model, trace_id)
         VALUES ($1, $2, $3, 'used-request', decode(repeat('00', 32), 'hex'), 'settled',
                 0, CURRENT_TIMESTAMP, 16, 0, 0, 0, 0, 0, 'v1', 'known', 'USD',
                 'deployment', 'model', $4)",
    )
    .bind(reservation)
    .bind(org.0)
    .bind(account_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.accounting_usage
           (id, reservation_id, organization_id, currency, confidence, input_tokens,
            output_tokens, amount_minor, completeness, finality)
         VALUES ($1, $2, $3, 'USD', 'exact', 8, 2, 0, 'complete', 'final')",
    )
    .bind(Uuid::new_v4())
    .bind(reservation)
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body, headers) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-used-sa"),
            Some(&format!("\"{version}\"")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], false);
    assert_eq!(body["disabled"], true);
    assert_eq!(body["reason"], "usage_history");
    assert_eq!(body["service_account"]["enabled"], false);
    let disabled_version = body["service_account"]["version"].as_u64().unwrap();
    assert!(disabled_version > version);
    assert_eq!(
        headers.get("etag").unwrap().to_str().unwrap(),
        format!("\"v{disabled_version}\"")
    );
    let disabled = body["service_account"].clone();

    let (enabled, usage): (bool, i64) = sqlx::query_as(
        "SELECT a.enabled,
                (SELECT count(*) FROM gateway.accounting_usage WHERE reservation_id = $3)
         FROM gateway.service_account a WHERE a.organization_id = $1 AND a.id = $2",
    )
    .bind(org.0)
    .bind(account_id)
    .bind(reservation)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!enabled);
    assert_eq!(usage, 1);
    let (action, before, after): (String, Value, Value) = sqlx::query_as(
        "SELECT action, before_metadata, after_metadata FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_id = $2 ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(org.0)
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(action, "service_account.deactivated");
    assert_eq!(before["enabled"], true);
    assert_eq!(before["version"], version);
    assert_eq!(after["enabled"], false);
    assert_eq!(after["version"], disabled_version);

    // A rename between them does not change what a replay answers.
    let (status, body, _) = send(
        &harness.router,
        request(
            "PATCH",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("rename-used-sa"),
            Some(&format!("\"{disabled_version}\"")),
            Some(json!({ "name": "renamed" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A replay answers what the first request did, from its saved result.
    let (status, body, headers) = send(
        &harness.router,
        request(
            "DELETE",
            &uri_service_account(org.0, account_id),
            Some(&session),
            Some("delete-used-sa"),
            Some(&format!("\"{version}\"")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], false);
    assert_eq!(body["disabled"], true);
    assert_eq!(body["service_account"], disabled);
    assert_eq!(
        headers.get("etag").unwrap().to_str().unwrap(),
        format!("\"v{disabled_version}\"")
    );

    pool.close().await;
}

/// The console's lists: service accounts filtered by `enabled`, with each
/// one's usable keys and last use; API keys filtered to the usable ones.
#[tokio::test]
async fn lists_filter_disabled_accounts_and_unusable_keys() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let harness = Harness::new(pool.clone());
    let org = harness.create_org("filters").await;
    let owner = harness.create_user(org, "owner").await;
    harness
        .add_org_membership(org, owner, MembershipRole::Owner)
        .await;
    backfill_admin_role_bindings(&pool).await;
    let session = harness.session(owner).await;
    let get = |uri: String| {
        let router = harness.router.clone();
        let session = session.clone();
        async move {
            let (status, body, _) = send(
                &router,
                request("GET", &uri, Some(&session), None, None, None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }
    };

    let mut accounts = Vec::new();
    for name in ["live-bot", "old-bot"] {
        let (status, body, _) = send(
            &harness.router,
            request(
                "POST",
                &uri_service_accounts(org.0),
                Some(&session),
                Some(&format!("create-{name}")),
                None,
                Some(json!({ "name": name })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        accounts.push(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap());
    }
    let live = accounts[0];
    let mut keys = Vec::new();
    for index in 0..3 {
        let (status, body, _) = send(
            &harness.router,
            request(
                "POST",
                &uri_keys(org.0, live),
                Some(&session),
                Some(&format!("create-key-{index}")),
                None,
                Some(json!({ "scopes": ["Inference"] })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        keys.push(Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap());
    }
    // One key revoked, one expired, one used; the second account disabled.
    sqlx::query("UPDATE gateway.api_key SET status = 'revoked', revoked_at = now() WHERE id = $1")
        .bind(keys[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.api_key SET expires_at = now() - interval '1 day' WHERE id = $1")
        .bind(keys[1])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.api_key SET last_used_at = now() WHERE id = $1")
        .bind(keys[2])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.service_account SET enabled = false WHERE id = $1")
        .bind(accounts[1])
        .execute(&pool)
        .await
        .unwrap();

    let names = |body: &Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let all = get(uri_service_accounts(org.0)).await;
    assert_eq!(names(&all), ["live-bot", "old-bot"]);
    let enabled = get(format!("{}?enabled=true", uri_service_accounts(org.0))).await;
    assert_eq!(names(&enabled), ["live-bot"]);
    // Only the key neither revoked nor expired counts as usable.
    assert_eq!(enabled["items"][0]["active_keys"], json!(1));
    assert!(enabled["items"][0]["last_used_at"].is_string());
    let disabled = get(format!("{}?enabled=false", uri_service_accounts(org.0))).await;
    assert_eq!(names(&disabled), ["old-bot"]);
    assert_eq!(disabled["items"][0]["active_keys"], json!(0));
    assert!(disabled["items"][0].get("last_used_at").is_none());

    let ids = |body: &Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&get(uri_keys(org.0, live)).await).len(), 3);
    assert_eq!(
        ids(&get(format!("{}?usable=true", uri_keys(org.0, live))).await),
        [keys[2].to_string()]
    );

    // A malformed filter is refused, not ignored.
    let (status, body, _) = send(
        &harness.router,
        request(
            "GET",
            &format!("{}?enabled=maybe", uri_service_accounts(org.0)),
            Some(&session),
            None,
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_query");
}
