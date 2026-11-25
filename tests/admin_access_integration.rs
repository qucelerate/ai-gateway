#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the organization/access administration API.
//!
//! These tests exercise the transport router end-to-end (session auth, RBAC,
//! cursor pagination, coordinator mutations with If-Match/Idempotency-Key,
//! audit, and grant ceilings). They compile once the integration lane registers
//! `transport::admin_access` and `persistence::access`.

use std::{
    env,
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header::COOKIE},
};
use gateway::{
    domain::{
        auth::AuthCrypto,
        identity::{MembershipRole, OrganizationId, UserAccountId},
        keys::ApiKeyPepper,
    },
    persistence::{
        AuditDimension, AuditListFilters, AuditStore, AuthStore, IdentityStore, access::AccessStore,
    },
    transport::admin_access::{AdminAccessState, admin_access_router},
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

struct Fixture {
    crypto: AuthCrypto,
    state: AdminAccessState,
    identity: IdentityStore,
    org: OrganizationId,
    owner: UserAccountId,
    owner_cookie: String,
}

async fn fixture(pool: &PgPool, slug: &str) -> Fixture {
    let crypto = AuthCrypto::new(b"access-test-session-hmac-key-32-bytes", &[12_u8; 32]).unwrap();
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"access-test-pepper").unwrap(),
    );
    let access = AccessStore::new(pool.clone(), Some(crypto.clone()));
    let state = AdminAccessState::new(access, AuthStore::new(pool.clone()), Some(crypto.clone()));
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("{slug}-{suffix}"), slug)
        .await
        .unwrap();
    let owner = identity
        .create_user(org.id, None, None, None, Some("owner"))
        .await
        .unwrap();
    identity
        .add_organization_membership(org.id, owner.id, MembershipRole::Owner)
        .await
        .unwrap();
    backfill(pool).await;
    let owner_cookie = session_cookie(pool, &crypto, owner.id).await;
    Fixture {
        crypto,
        state,
        identity,
        org: org.id,
        owner: owner.id,
        owner_cookie,
    }
}

async fn session_cookie(pool: &PgPool, crypto: &AuthCrypto, user_id: UserAccountId) -> String {
    let session = crypto.new_session(user_id, "route-browser", StdDuration::from_secs(3600));
    sqlx::query(
        "INSERT INTO gateway.session (id, user_id, token_digest, browser_binding_digest, expires_at)
         VALUES ($1, $2, $3, $4, to_timestamp($5))",
    )
    .bind(session.id)
    .bind(user_id.0)
    .bind(&session.token_digest)
    .bind(&session.browser_binding_digest)
    .bind(session.expires_at as i64)
    .execute(pool)
    .await
    .unwrap();
    format!(
        "__Host-gateway_session={}; __Host-gateway_oidc_browser=route-browser",
        session.token
    )
}

/// Deploy-style backfill for test-created memberships, including version rows,
/// so conditional mutations work on backfilled resources too.
async fn backfill(pool: &PgPool) {
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
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
         SELECT id, 'organization', id FROM gateway.organization
         UNION ALL SELECT organization_id, 'team', id FROM gateway.team
         UNION ALL SELECT organization_id, 'membership', user_id FROM gateway.organization_membership
         UNION ALL SELECT organization_id, 'membership', md5(team_id::text || ':' || user_id::text)::uuid FROM gateway.team_membership
         UNION ALL SELECT organization_id, 'role_binding', id FROM gateway.admin_role_binding
         UNION ALL SELECT organization_id, 'service_account', id FROM gateway.service_account
         UNION ALL SELECT organization_id, 'api_key', id FROM gateway.api_key
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn call(
    state: &AdminAccessState,
    method: &str,
    path: &str,
    cookie: &str,
    idempotency_key: Option<&str>,
    if_match: Option<&str>,
    body: Option<Value>,
) -> axum::http::Response<Body> {
    call_full(
        state,
        method,
        path,
        cookie,
        idempotency_key,
        if_match,
        None,
        body,
    )
    .await
}

async fn call_full(
    state: &AdminAccessState,
    method: &str,
    path: &str,
    cookie: &str,
    idempotency_key: Option<&str>,
    if_match: Option<&str>,
    request_id: Option<&str>,
    body: Option<Value>,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .header(COOKIE, cookie);
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    if let Some(etag) = if_match {
        builder = builder.header("if-match", etag);
    }
    if let Some(rid) = request_id {
        builder = builder.header("x-request-id", rid);
    }
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    admin_access_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
}

async fn json(response: axum::http::Response<Body>) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn connect_pool() -> Option<PgPool> {
    let url = env::var("GATEWAY_TEST_DATABASE_URL").ok()?;
    Some(
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(StdDuration::from_secs(2))
            .connect(&url)
            .await
            .unwrap(),
    )
}

async fn apply_migrations(pool: &PgPool) {
    gateway::persistence::migrations::run(pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
}

#[tokio::test]
async fn team_crud_is_conditional_and_versioned() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping team CRUD: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "team-crud").await;
    let org = fx.org.0;
    let teams = format!("/api/admin/v1/organizations/{org}/teams");

    // Mutations require an Idempotency-Key.
    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        None,
        None,
        Some(json!({"name": "No key"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(resp).await["code"], "idempotency_key_required");

    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("t1"),
        None,
        Some(json!({"name": "Alpha"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = json(resp).await;
    assert_eq!(created["status"], "created");
    assert_eq!(created["version"], 1);
    let team_id: Uuid = created["resource_id"].as_str().unwrap().parse().unwrap();
    let team = format!("{teams}/{team_id}");

    // A team-scoped mutation leaves an immutable audit event tied to the team.
    // Deleting the team must not be blocked by that historical scope.
    let team_members = format!("{team}/members");
    let resp = call(
        &fx.state,
        "POST",
        &team_members,
        &fx.owner_cookie,
        Some("team-audit-membership"),
        None,
        Some(json!({"user_id": fx.owner.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Detail carries the version and ETag for conditional mutation.
    let resp = call(&fx.state, "GET", &team, &fx.owner_cookie, None, None, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(etag, "\"v1\"");
    assert_eq!(json(resp).await["name"], "Alpha");

    // PATCH requires If-Match (428, matching the shared admin contract).
    let resp = call(
        &fx.state,
        "PATCH",
        &team,
        &fx.owner_cookie,
        Some("t2"),
        None,
        Some(json!({"name": "Alpha2"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(json(resp).await["code"], "if_match_required");

    // Correct If-Match succeeds and bumps the version.
    let resp = call(
        &fx.state,
        "PATCH",
        &team,
        &fx.owner_cookie,
        Some("t2"),
        Some("\"1\""),
        Some(json!({"name": "Alpha2"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await["version"], 2);

    // Stale If-Match conflicts.
    let resp = call(
        &fx.state,
        "PATCH",
        &team,
        &fx.owner_cookie,
        Some("t3"),
        Some("\"1\""),
        Some(json!({"name": "Stale"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "version_conflict");

    // List reflects the rename.
    let resp = call(&fx.state, "GET", &teams, &fx.owner_cookie, None, None, None).await;
    let page = json(resp).await;
    assert_eq!(page["items"][0]["name"], "Alpha2");
    assert_eq!(page["items"][0]["version"], 2);

    // Conditional delete (204 No Content, shared admin convention) then detail 404.
    let resp = call(
        &fx.state,
        "DELETE",
        &team,
        &fx.owner_cookie,
        Some("t4"),
        Some("\"2\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = call(&fx.state, "GET", &team, &fx.owner_cookie, None, None, None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The same key replays the deletion after the team is gone; a fresh key
    // is a new delete of a missing team and stays 404.
    let resp = call(
        &fx.state,
        "DELETE",
        &team,
        &fx.owner_cookie,
        Some("t4"),
        Some("\"2\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await, json!({"deleted": true}));
    let resp = call(
        &fx.state,
        "DELETE",
        &team,
        &fx.owner_cookie,
        Some("t5"),
        Some("\"2\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let retained_scoped_audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND team_id = $2 AND action = 'membership.created'",
    )
    .bind(org)
    .bind(team_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained_scoped_audits, 1);

    // A missing DELETE is a plain 404 and must not claim any mutation state.
    let missing_team = Uuid::new_v4();
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{teams}/{missing_team}"),
        &fx.owner_cookie,
        Some("missing-team-delete"),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(resp).await["code"], "not_found");
    let seeded_version: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'team' AND resource_id = $2",
    )
    .bind(org)
    .bind(missing_team)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(seeded_version, 0);
    let successful_audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'team' AND entity_id = $2
           AND action = 'team.deleted' AND outcome = 'succeeded'",
    )
    .bind(org)
    .bind(missing_team)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(successful_audits, 0);
    let claimed_idempotency: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_idempotency_record
         WHERE organization_id = $1 AND actor_user_id = $2
           AND operation = 'team.delete' AND idempotency_key = 'missing-team-delete'",
    )
    .bind(org)
    .bind(fx.owner.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(claimed_idempotency, 0);
    pool.close().await;
}

#[tokio::test]
async fn migration_0027_backfills_missing_binding_versions_and_is_rerunnable() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping migration 0027 upgrade coverage: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"migration-0027-test-pepper").unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = identity
        .create_organization(&format!("migration-0027-{suffix}"), "migration-0027")
        .await
        .unwrap();
    let user = identity
        .create_user(organization.id, None, None, None, Some("migration user"))
        .await
        .unwrap();
    identity
        .add_organization_membership(organization.id, user.id, MembershipRole::Member)
        .await
        .unwrap();
    let team = identity
        .create_team(organization.id, "migration-team")
        .await
        .unwrap();
    identity
        .add_team_membership(organization.id, team.id, user.id, MembershipRole::Admin)
        .await
        .unwrap();

    let org_missing_binding_id = Uuid::new_v4();
    let team_missing_binding_id = Uuid::new_v4();
    let existing_binding_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
         VALUES ($1, $2, $3, 'organization_admin')",
    )
    .bind(org_missing_binding_id)
    .bind(organization.id.0)
    .bind(user.id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding
             (id, organization_id, user_id, team_id, role)
         VALUES ($1, $2, $3, $4, 'team_admin')",
    )
    .bind(team_missing_binding_id)
    .bind(organization.id.0)
    .bind(user.id.0)
    .bind(team.id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
         VALUES ($1, $2, $3, 'auditor')",
    )
    .bind(existing_binding_id)
    .bind(organization.id.0)
    .bind(user.id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id IN ($2, $3, $4)",
    )
    .bind(organization.id.0)
    .bind(org_missing_binding_id)
    .bind(team_missing_binding_id)
    .bind(existing_binding_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version
             (organization_id, resource_type, resource_id, version)
         VALUES ($1, 'role_binding', $2, 7)",
    )
    .bind(organization.id.0)
    .bind(existing_binding_id)
    .execute(&pool)
    .await
    .unwrap();

    let migration = include_str!("../migrations/0027_admin_access_team_audit_delete.sql");
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    let versions: (i64, i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT version FROM gateway.admin_resource_version
              WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $2),
             (SELECT version FROM gateway.admin_resource_version
              WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $3),
             (SELECT version FROM gateway.admin_resource_version
              WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $4)",
    )
    .bind(organization.id.0)
    .bind(org_missing_binding_id)
    .bind(team_missing_binding_id)
    .bind(existing_binding_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(versions, (1, 1, 7));

    // Reapplying the forward file must preserve the existing version and remain
    // safe for deployments that have already run the upgrade.
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    let versions_after_reapply: (i64, i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT version FROM gateway.admin_resource_version
              WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $2),
             (SELECT version FROM gateway.admin_resource_version
              WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $3),
             (SELECT version FROM gateway.admin_resource_version
              WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $4)",
    )
    .bind(organization.id.0)
    .bind(org_missing_binding_id)
    .bind(team_missing_binding_id)
    .bind(existing_binding_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(versions_after_reapply, (1, 1, 7));
    pool.close().await;
}

#[tokio::test]
async fn org_and_team_membership_scope_semantics_and_projection() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping membership semantics: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "member-scope").await;
    let org = fx.org.0;
    let members = format!("/api/admin/v1/organizations/{org}/members");
    let alice = fx
        .identity
        .create_user(fx.org, None, None, Some("alice@example.com"), Some("alice"))
        .await
        .unwrap();
    let bob = fx
        .identity
        .create_user(fx.org, None, None, None, Some("bob"))
        .await
        .unwrap();

    // Org membership create with role Admin projects an org-wide binding.
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &fx.owner_cookie,
        Some("m1"),
        None,
        Some(json!({"user_id": alice.id.0, "role": "Admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(json(resp).await["version"], 1);
    let (role, enabled): (String, bool) = sqlx::query_as(
        "SELECT role, enabled FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL",
    )
    .bind(org)
    .bind(alice.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((role.as_str(), enabled), ("organization_admin", true));

    let projected_version: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id = md5($1::text || ':' || $2::text)::uuid",
    )
    .bind(org)
    .bind(alice.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(projected_version, 1);

    let resp = call(
        &fx.state,
        "GET",
        &format!("/api/admin/v1/organizations/{org}/role-bindings"),
        &fx.owner_cookie,
        None,
        None,
        None,
    )
    .await;
    let binding = json(resp).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["user_id"] == alice.id.0.to_string())
        .cloned()
        .expect("projected binding is listed");
    assert_eq!(binding["version"], 1);
    // Bindings and members name the person, not just their id.
    assert_eq!(
        (&binding["display_name"], &binding["email"]),
        (&json!("alice"), &json!("alice@example.com"))
    );
    let binding_id = binding["id"].as_str().unwrap();
    let binding_path = format!("/api/admin/v1/organizations/{org}/role-bindings/{binding_id}");
    let resp = call(
        &fx.state,
        "PATCH",
        &binding_path,
        &fx.owner_cookie,
        Some("projected-binding-edit"),
        Some("\"1\""),
        Some(json!({"role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // List org members.
    let resp = call(
        &fx.state,
        "GET",
        &members,
        &fx.owner_cookie,
        None,
        None,
        None,
    )
    .await;
    let page = json(resp).await;
    let listed = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["user_id"] == alice.id.0.to_string())
        .expect("alice is listed");
    assert_eq!(
        (&listed["display_name"], &listed["email"]),
        (&json!("alice"), &json!("alice@example.com"))
    );

    // Role downgrade disables the projected binding atomically.
    let resp = call(
        &fx.state,
        "PATCH",
        &format!("{members}/{}", alice.id.0),
        &fx.owner_cookie,
        Some("m2"),
        Some("\"1\""),
        Some(json!({"role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await["version"], 2);
    let enabled: bool = sqlx::query_scalar(
        "SELECT enabled FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL",
    )
    .bind(org)
    .bind(alice.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!enabled);

    let disabled_projection_version: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id = md5($1::text || ':' || $2::text)::uuid",
    )
    .bind(org)
    .bind(alice.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(disabled_projection_version, 3);
    let resp = call(
        &fx.state,
        "PATCH",
        &binding_path,
        &fx.owner_cookie,
        Some("stale-after-projection"),
        Some("\"2\""),
        Some(json!({"role": "auditor"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "version_conflict");
    let resp = call(
        &fx.state,
        "DELETE",
        &binding_path,
        &fx.owner_cookie,
        Some("projected-binding-revoke"),
        Some("\"3\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Bob needs an org membership before joining a team.
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &fx.owner_cookie,
        Some("m3"),
        None,
        Some(json!({"user_id": bob.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create a team and add bob as an admin (team_admin projection).
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org}/teams"),
        &fx.owner_cookie,
        Some("t1"),
        None,
        Some(json!({"name": "Eng"})),
    )
    .await;
    let team_id: Uuid = json(resp).await["resource_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let team_members = format!("/api/admin/v1/organizations/{org}/teams/{team_id}/members");
    let resp = call(
        &fx.state,
        "POST",
        &team_members,
        &fx.owner_cookie,
        Some("m4"),
        None,
        Some(json!({"user_id": bob.id.0, "role": "Admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let (role, enabled): (String, bool) = sqlx::query_as(
        "SELECT role, enabled FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id = $3",
    )
    .bind(org)
    .bind(bob.id.0)
    .bind(team_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((role.as_str(), enabled), ("team_admin", true));
    let team_binding_version: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id = md5($1::text || ':' || $2::text || ':' || $3::text)::uuid",
    )
    .bind(org)
    .bind(team_id)
    .bind(bob.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(team_binding_version, 1);

    // Soft-disable the team membership and then remove it.
    let resp = call(
        &fx.state,
        "PATCH",
        &format!("{team_members}/{}", bob.id.0),
        &fx.owner_cookie,
        Some("m5"),
        Some("\"1\""),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let team_binding_version_after_disable: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id = md5($1::text || ':' || $2::text || ':' || $3::text)::uuid",
    )
    .bind(org)
    .bind(team_id)
    .bind(bob.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(team_binding_version_after_disable, 2);
    let resp = call(
        &fx.state,
        "PATCH",
        &format!("{team_members}/{}", bob.id.0),
        &fx.owner_cookie,
        Some("stale-team-membership"),
        Some("\"1\""),
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "version_conflict");
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{team_members}/{}", bob.id.0),
        &fx.owner_cookie,
        Some("m6"),
        Some("\"2\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.team_membership WHERE organization_id = $1 AND user_id = $2",
    )
    .bind(org)
    .bind(bob.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);

    // The same key replays the removal; a fresh key must not delete the
    // already-removed membership again through its retained version row.
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{team_members}/{}", bob.id.0),
        &fx.owner_cookie,
        Some("m6"),
        Some("\"2\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let version_before_fresh_delete: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id = md5($1::text || ':' || $2::text || ':' || $3::text)::uuid",
    )
    .bind(org)
    .bind(team_id)
    .bind(bob.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{team_members}/{}", bob.id.0),
        &fx.owner_cookie,
        Some("m7"),
        Some("*"),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let version_after_fresh_delete: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'role_binding'
           AND resource_id = md5($1::text || ':' || $2::text || ':' || $3::text)::uuid",
    )
    .bind(org)
    .bind(team_id)
    .bind(bob.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(version_after_fresh_delete, version_before_fresh_delete);
    pool.close().await;
}

#[tokio::test]
async fn role_binding_grants_use_ceiling_and_protect_last_owner() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping role binding ceiling: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "rbac-ceiling").await;
    let org = fx.org.0;
    let bindings = format!("/api/admin/v1/organizations/{org}/role-bindings");
    let admin_user = fx
        .identity
        .create_user(fx.org, None, None, None, Some("admin user"))
        .await
        .unwrap();
    let someone = fx
        .identity
        .create_user(fx.org, None, None, None, Some("someone"))
        .await
        .unwrap();

    // Memberships are required before bindings can reference the users.
    for (user, role, key) in [
        (admin_user.id, "Member", "bm-admin"),
        (someone.id, "Member", "bm-someone"),
    ] {
        let resp = call(
            &fx.state,
            "POST",
            &format!("/api/admin/v1/organizations/{org}/members"),
            &fx.owner_cookie,
            Some(key),
            None,
            Some(json!({"user_id": user.0, "role": role})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Owner grants organization_admin to the admin user.
    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &fx.owner_cookie,
        Some("r1"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created_binding = json(resp).await;
    let admin_binding_id: Uuid = created_binding["resource_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Revocation is soft, so a later grant of the same natural binding reuses
    // the disabled row and continues its version history.
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{bindings}/{admin_binding_id}"),
        &fx.owner_cookie,
        Some("r3-revoke"),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &fx.owner_cookie,
        Some("r3-restore"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let restored = json(resp).await;
    assert_eq!(restored["resource_id"], admin_binding_id.to_string());
    assert_eq!(restored["version"], 3);

    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &fx.owner_cookie,
        Some("r3-restore"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let replay = json(resp).await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["resource_id"], admin_binding_id.to_string());

    let (enabled, version): (bool, i64) = sqlx::query_as(
        "SELECT b.enabled, v.version
         FROM gateway.admin_role_binding b
         JOIN gateway.admin_resource_version v
           ON v.organization_id = b.organization_id
          AND v.resource_type = 'role_binding'
          AND v.resource_id = b.id
         WHERE b.organization_id = $1 AND b.id = $2",
    )
    .bind(org)
    .bind(admin_binding_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(enabled);
    assert_eq!(version, 3);
    let restored_audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'role_binding.created'
           AND entity_id = $2 AND outcome = 'succeeded'",
    )
    .bind(org)
    .bind(admin_binding_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(restored_audits, 2);

    // The org-admin ceiling cannot grant owner (escalation denied).
    let admin_cookie = session_cookie(&pool, &fx.crypto, admin_user.id).await;
    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &admin_cookie,
        Some("r2"),
        None,
        Some(json!({"user_id": someone.id.0, "role": "organization_owner"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(resp).await["code"], "escalation_denied");

    // ...but granting organization_admin is within the ceiling.
    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &admin_cookie,
        Some("r3"),
        None,
        Some(json!({"user_id": someone.id.0, "role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The org-admin cannot revoke a binding ranked above their ceiling.
    let owner_binding_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL AND role = 'organization_owner'",
    )
    .bind(org)
    .bind(fx.owner.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{bindings}/{owner_binding_id}"),
        &admin_cookie,
        Some("r4"),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(resp).await["code"], "escalation_denied");

    // The last active owner cannot be revoked even by the owner themselves.
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{bindings}/{owner_binding_id}"),
        &fx.owner_cookie,
        Some("r5"),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "last_owner_required");

    // Granting a second owner makes the first revocable.
    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &fx.owner_cookie,
        Some("r6"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "organization_owner"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{bindings}/{owner_binding_id}"),
        &fx.owner_cookie,
        Some("r7"),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    pool.close().await;
}

#[tokio::test]
async fn cross_tenant_and_team_to_org_denials_are_fail_closed() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping cross-tenant denials: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "cross-a").await;
    let org_a = fx.org.0;
    let teams_a = format!("/api/admin/v1/organizations/{org_a}/teams");
    let members_a = format!("/api/admin/v1/organizations/{org_a}/members");

    // Owner creates a team and promotes a team admin member.
    let resp = call(
        &fx.state,
        "POST",
        &teams_a,
        &fx.owner_cookie,
        Some("t1"),
        None,
        Some(json!({"name": "TeamA"})),
    )
    .await;
    let team_a: Uuid = json(resp).await["resource_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let resp = call(
        &fx.state,
        "POST",
        &teams_a,
        &fx.owner_cookie,
        Some("t2"),
        None,
        Some(json!({"name": "TeamB"})),
    )
    .await;
    let team_b: Uuid = json(resp).await["resource_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let alice = fx
        .identity
        .create_user(fx.org, None, None, None, Some("alice"))
        .await
        .unwrap();
    let resp = call(
        &fx.state,
        "POST",
        &members_a,
        &fx.owner_cookie,
        Some("m1"),
        None,
        Some(json!({"user_id": alice.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org_a}/teams/{team_a}/members"),
        &fx.owner_cookie,
        Some("m2"),
        None,
        Some(json!({"user_id": alice.id.0, "role": "Admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let alice_cookie = session_cookie(&pool, &fx.crypto, alice.id).await;

    // Team admin cannot read org-wide collections or mutate at org scope.
    let resp = call(&fx.state, "GET", &teams_a, &alice_cookie, None, None, None).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = call(
        &fx.state,
        "GET",
        &members_a,
        &alice_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = call(
        &fx.state,
        "POST",
        &teams_a,
        &alice_cookie,
        Some("x1"),
        None,
        Some(json!({"name": "Nope"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org_a}/role-bindings"),
        &alice_cookie,
        Some("x2"),
        None,
        Some(json!({"user_id": alice.id.0, "role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Team admin can read its own team/members but not the other team.
    let resp = call(
        &fx.state,
        "GET",
        &format!("/api/admin/v1/organizations/{org_a}/teams/{team_a}"),
        &alice_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(
        &fx.state,
        "GET",
        &format!("/api/admin/v1/organizations/{org_a}/teams/{team_a}/members"),
        &alice_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(
        &fx.state,
        "GET",
        &format!("/api/admin/v1/organizations/{org_a}/teams/{team_b}"),
        &alice_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org_a}/teams/{team_b}/members"),
        &alice_cookie,
        Some("x3"),
        None,
        Some(json!({"user_id": alice.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Cross-tenant: alice (org A) cannot read or write org B.
    let fx_b = fixture(&pool, "cross-b").await;
    let org_b = fx_b.org.0;
    let resp = call(
        &fx.state,
        "GET",
        &format!("/api/admin/v1/organizations/{org_b}/teams"),
        &alice_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org_b}/teams"),
        &alice_cookie,
        Some("x4"),
        None,
        Some(json!({"name": "Sneak"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    pool.close().await;
}

#[tokio::test]
async fn idempotency_replay_and_conflict_are_deterministic() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping idempotency replay: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "idem-replay").await;
    let org = fx.org.0;
    let teams = format!("/api/admin/v1/organizations/{org}/teams");

    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("k1"),
        None,
        Some(json!({"name": "Same"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let first = json(resp).await;
    let team_id = first["resource_id"].as_str().unwrap().to_owned();
    assert_eq!(first["version"], 1);

    // Exact retry replays the same resource id with no second mutation.
    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("k1"),
        None,
        Some(json!({"name": "Same"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let replay = json(resp).await;
    assert_eq!(replay["status"], "replayed");
    assert_eq!(replay["resource_id"].as_str().unwrap(), team_id);
    assert_eq!(replay["version"], 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway.team WHERE id = $1")
        .bind(Uuid::parse_str(&team_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Same key with a different body conflicts.
    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("k1"),
        None,
        Some(json!({"name": "Different"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "idempotency_conflict");

    // A distinct key creates a second team.
    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("k2"),
        None,
        Some(json!({"name": "Other"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    pool.close().await;
}

#[tokio::test]
async fn cursor_pagination_rejects_tampering_and_scope_mismatch() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping cursor pagination: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    // One global owner in two orgs so a token from A can be presented at B.
    let crypto = AuthCrypto::new(b"access-test-session-hmac-key-32-bytes", &[12_u8; 32]).unwrap();
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"access-test-pepper").unwrap(),
    );
    let access = AccessStore::new(pool.clone(), Some(crypto.clone()));
    let state = AdminAccessState::new(access, AuthStore::new(pool.clone()), Some(crypto.clone()));
    let suffix = Uuid::new_v4().simple().to_string();
    let org_a = identity
        .create_organization(&format!("cursor-a-{suffix}"), "Cursor A")
        .await
        .unwrap();
    let org_b = identity
        .create_organization(&format!("cursor-b-{suffix}"), "Cursor B")
        .await
        .unwrap();
    let owner = identity
        .create_user(org_a.id, None, None, None, Some("owner"))
        .await
        .unwrap();
    identity
        .add_organization_membership(org_a.id, owner.id, MembershipRole::Owner)
        .await
        .unwrap();
    identity
        .add_organization_membership(org_b.id, owner.id, MembershipRole::Owner)
        .await
        .unwrap();
    backfill(&pool).await;
    let cookie = session_cookie(&pool, &crypto, owner.id).await;
    let teams_a = format!("/api/admin/v1/organizations/{}/teams", org_a.id.0);
    for (index, name) in ["Bravo", "Alpha", "Charlie"].iter().enumerate() {
        let resp = call(
            &state,
            "POST",
            &teams_a,
            &cookie,
            Some(&format!("c{index}")),
            None,
            Some(json!({"name": name})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let resp = call(
        &state,
        "GET",
        &format!("{teams_a}?page_size=2"),
        &cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let first_page = json(resp).await;
    assert_eq!(first_page["items"].as_array().unwrap().len(), 2);
    let token = first_page["next_page_token"].as_str().unwrap().to_owned();
    assert!(!token.is_empty());

    // Page 2 returns the remaining item ordered by (name, id).
    let resp = call(
        &state,
        "GET",
        &format!("{teams_a}?page_size=2&page_token={token}"),
        &cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let second_page = json(resp).await;
    assert_eq!(second_page["items"].as_array().unwrap().len(), 1);
    assert!(second_page["next_page_token"].is_null());

    // Tampered token is rejected.
    let tampered = format!("{}x", &token[..token.len() - 1]);
    let resp = call(
        &state,
        "GET",
        &format!("{teams_a}?page_size=2&page_token={tampered}"),
        &cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json(resp).await;
    assert!(
        body["code"]
            .as_str()
            .unwrap()
            .starts_with("invalid_page_token")
    );

    // A token minted for org A is rejected at org B (scope mismatch).
    let teams_b = format!("/api/admin/v1/organizations/{}/teams", org_b.id.0);
    let resp = call(
        &state,
        "GET",
        &format!("{teams_b}?page_size=2&page_token={token}"),
        &cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(resp).await["code"], "page_token_scope_mismatch");
    pool.close().await;
}

#[tokio::test]
async fn mutations_emit_secret_free_audit_and_denials_are_audited() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping audit outcome coverage: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-outcome").await;
    let org = fx.org.0;
    let teams = format!("/api/admin/v1/organizations/{org}/teams");

    // A plain member (no binding) is denied and the denial is audited.
    let member = fx
        .identity
        .create_user(fx.org, None, None, None, Some("member"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, member.id, MembershipRole::Member)
        .await
        .unwrap();
    let member_cookie = session_cookie(&pool, &fx.crypto, member.id).await;
    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &member_cookie,
        Some("d1"),
        None,
        Some(json!({"name": "Sneak"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'team.created' AND outcome = 'denied'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denied, 1);

    // Owner succeeds and the audit row carries only allowlisted identifiers.
    let resp = call(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("ok1"),
        None,
        Some(json!({"name": "Audited"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let team_id = json(resp).await["resource_id"].as_str().unwrap().to_owned();
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT metadata FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'team.created' AND outcome = 'succeeded'",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audit.len(), 1);
    let metadata = &audit[0];
    assert_eq!(metadata["target_id"].as_str().unwrap(), team_id);
    let rendered = metadata.to_string();
    for secret in [
        "secret",
        "bearer",
        "digest",
        "pepper",
        "request_body",
        "idempotency_key",
    ] {
        assert!(
            !rendered.contains(secret),
            "audit metadata must not contain {secret}: {rendered}"
        );
    }
    let immutable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'team'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(immutable, 2, "one succeeded + one denied team.create audit");
    pool.close().await;
}

#[tokio::test]
async fn membership_grant_ceiling_blocks_escalation_and_owner_bypass() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping membership ceiling: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "membership-ceiling").await;
    let org = fx.org.0;
    let members = format!("/api/admin/v1/organizations/{org}/members");
    let bindings = format!("/api/admin/v1/organizations/{org}/role-bindings");

    let admin_user = fx
        .identity
        .create_user(fx.org, None, None, None, Some("admin"))
        .await
        .unwrap();
    let other = fx
        .identity
        .create_user(fx.org, None, None, None, Some("other"))
        .await
        .unwrap();

    // Seed an organization admin through the role-binding path.
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &fx.owner_cookie,
        Some("c1"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = call(
        &fx.state,
        "POST",
        &bindings,
        &fx.owner_cookie,
        Some("c2"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "organization_admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let admin_cookie = session_cookie(&pool, &fx.crypto, admin_user.id).await;

    // Self-escalation to owner through the membership role is denied.
    let resp = call(
        &fx.state,
        "PATCH",
        &format!("{members}/{}", admin_user.id.0),
        &admin_cookie,
        Some("c3"),
        Some("\"1\""),
        Some(json!({"role": "Owner"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(resp).await["code"], "escalation_denied");

    // Escalating another user to owner through a membership create is denied.
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &admin_cookie,
        Some("c4"),
        None,
        Some(json!({"user_id": other.id.0, "role": "Owner"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(resp).await["code"], "escalation_denied");

    // After the failed escalation, the org admin still cannot demote or
    // remove the real owner (their projection outranks the org-admin ceiling).
    let resp = call(
        &fx.state,
        "PATCH",
        &format!("{members}/{}", fx.owner.0),
        &admin_cookie,
        Some("c5"),
        Some("\"1\""),
        Some(json!({"role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(resp).await["code"], "escalation_denied");
    let resp = call(
        &fx.state,
        "DELETE",
        &format!("{members}/{}", fx.owner.0),
        &admin_cookie,
        Some("c6"),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(resp).await["code"], "escalation_denied");

    // Legitimate owner action: the owner can grant owner through the
    // membership path and create an owner membership for another user.
    let resp = call(
        &fx.state,
        "PATCH",
        &format!("{members}/{}", admin_user.id.0),
        &fx.owner_cookie,
        Some("c7"),
        Some("\"1\""),
        Some(json!({"role": "Owner"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &fx.owner_cookie,
        Some("c8"),
        None,
        Some(json!({"user_id": other.id.0, "role": "Owner"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let owner_bindings: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND role = 'organization_owner' AND enabled",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owner_bindings, 3, "owner + admin_user + other are owners");

    // Team boundary consistency: a team admin can manage its own team but
    // cannot cross into another team or the organization.
    let team_a = {
        let resp = call(
            &fx.state,
            "POST",
            &format!("/api/admin/v1/organizations/{org}/teams"),
            &fx.owner_cookie,
            Some("t1"),
            None,
            Some(json!({"name": "TeamA"})),
        )
        .await;
        Uuid::parse_str(json(resp).await["resource_id"].as_str().unwrap()).unwrap()
    };
    let team_b = {
        let resp = call(
            &fx.state,
            "POST",
            &format!("/api/admin/v1/organizations/{org}/teams"),
            &fx.owner_cookie,
            Some("t2"),
            None,
            Some(json!({"name": "TeamB"})),
        )
        .await;
        Uuid::parse_str(json(resp).await["resource_id"].as_str().unwrap()).unwrap()
    };
    let member_a = fx
        .identity
        .create_user(fx.org, None, None, None, Some("member a"))
        .await
        .unwrap();
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &fx.owner_cookie,
        Some("m1"),
        None,
        Some(json!({"user_id": member_a.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org}/teams/{team_a}/members"),
        &fx.owner_cookie,
        Some("m2"),
        None,
        Some(json!({"user_id": admin_user.id.0, "role": "Admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org}/teams/{team_a}/members"),
        &fx.owner_cookie,
        Some("m3"),
        None,
        Some(json!({"user_id": member_a.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // admin_user is org owner now, so use the team-scoped member_a session for
    // team boundary checks: member_a has no admin grant, so grant team_admin to
    // a fresh user and act as them.
    let team_admin_user = fx
        .identity
        .create_user(fx.org, None, None, None, Some("team admin"))
        .await
        .unwrap();
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &fx.owner_cookie,
        Some("m4"),
        None,
        Some(json!({"user_id": team_admin_user.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org}/teams/{team_a}/members"),
        &fx.owner_cookie,
        Some("m5"),
        None,
        Some(json!({"user_id": team_admin_user.id.0, "role": "Admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let team_admin_cookie = session_cookie(&pool, &fx.crypto, team_admin_user.id).await;

    // team_admin can promote a member within its own team.
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org}/teams/{team_a}/members"),
        &team_admin_cookie,
        Some("t3"),
        None,
        Some(json!({"user_id": other.id.0, "role": "Admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    // ...but cannot manage team B or the organization.
    let resp = call(
        &fx.state,
        "POST",
        &format!("/api/admin/v1/organizations/{org}/teams/{team_b}/members"),
        &team_admin_cookie,
        Some("t4"),
        None,
        Some(json!({"user_id": other.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = call(
        &fx.state,
        "POST",
        &members,
        &team_admin_cookie,
        Some("t5"),
        None,
        Some(json!({"user_id": other.id.0, "role": "Member"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    pool.close().await;
}

#[tokio::test]
async fn http_contract_errors_are_problem_json_with_request_id() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping HTTP contract: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "http-contract").await;
    let org = fx.org.0;
    let teams = format!("/api/admin/v1/organizations/{org}/teams");

    // A plain member (no binding) is denied; the problem is problem+json with
    // the echoed request id.
    let member = fx
        .identity
        .create_user(fx.org, None, None, None, Some("member"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, member.id, MembershipRole::Member)
        .await
        .unwrap();
    let member_cookie = session_cookie(&pool, &fx.crypto, member.id).await;
    let request_id = Uuid::new_v4().to_string();
    let resp = call_full(
        &fx.state,
        "POST",
        &teams,
        &member_cookie,
        Some("d1"),
        None,
        Some(&request_id),
        Some(json!({"name": "Sneak"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(request_id.as_str())
    );
    let body = json(resp).await;
    assert_eq!(body["code"], "forbidden");
    assert_eq!(body["request_id"], request_id);
    assert_eq!(body["instance"], "/api/admin/v1/organizations/{org}/teams");
    assert!(body["detail"].as_str().unwrap().contains("permission"));

    // A nil x-request-id is regenerated so it never reaches the audit checks.
    let resp = call_full(
        &fx.state,
        "POST",
        &teams,
        &member_cookie,
        Some("d2"),
        None,
        Some("00000000-0000-0000-0000-000000000000"),
        Some(json!({"name": "Sneak2"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let echoed = resp
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_owned();
    assert_ne!(echoed, "00000000-0000-0000-0000-000000000000");
    let echoed_uuid = Uuid::parse_str(&echoed).unwrap();
    assert!(!echoed_uuid.is_nil());

    // A client request id is fed into the mutation audit reference.
    let request_id = Uuid::new_v4().to_string();
    let resp = call_full(
        &fx.state,
        "POST",
        &teams,
        &fx.owner_cookie,
        Some("ok1"),
        None,
        Some(&request_id),
        Some(json!({"name": "Audited"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let audit_request_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT request_id FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'team.created' AND outcome = 'succeeded'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit_request_id.unwrap().to_string(), request_id);

    // ETags use the `v<version>` format shared with the service-identity lane.
    let team_id = Uuid::parse_str(json(resp).await["resource_id"].as_str().unwrap()).unwrap();
    let resp = call(
        &fx.state,
        "GET",
        &format!("{teams}/{team_id}"),
        &fx.owner_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("etag")
            .and_then(|value| value.to_str().ok()),
        Some("\"v1\"")
    );
    pool.close().await;
}

#[tokio::test]
async fn audit_query_rejections_use_admin_problem_contract() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        eprintln!("skipping audit query contract: GATEWAY_TEST_DATABASE_URL unset");
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-query-contract").await;
    let base = format!("/api/admin/v1/organizations/{}/audit-events", fx.org.0);

    for (suffix, request_id) in [
        ("?page_size=not-a-number", Uuid::new_v4().to_string()),
        ("?unexpected=true", Uuid::new_v4().to_string()),
    ] {
        let response = call_full(
            &fx.state,
            "GET",
            &format!("{base}{suffix}"),
            &fx.owner_cookie,
            None,
            None,
            Some(&request_id),
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        assert_eq!(
            response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id.as_str())
        );

        let body = json(response).await;
        assert_eq!(body["code"], "invalid_audit_query");
        assert_eq!(body["request_id"], request_id);
        assert_eq!(
            body["instance"],
            "/api/admin/v1/organizations/{org}/audit-events"
        );
    }

    pool.close().await;
}

#[tokio::test]
async fn audit_events_are_cross_tenant_denied() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let first = fixture(&pool, "audit-cross-first").await;
    let second = fixture(&pool, "audit-cross-second").await;
    let uri = format!("/api/admin/v1/organizations/{}/audit-events", second.org.0);
    let response = call(
        &first.state,
        "GET",
        &uri,
        &first.owner_cookie,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    pool.close().await;
}

#[tokio::test]
async fn audit_events_require_organization_audit_permission() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-rbac").await;
    let user = fx
        .identity
        .create_user(fx.org, None, None, None, Some("member"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, user.id, MembershipRole::Member)
        .await
        .unwrap();
    backfill(&pool).await;
    let cookie = session_cookie(&pool, &fx.crypto, user.id).await;
    let uri = format!("/api/admin/v1/organizations/{}/audit-events", fx.org.0);
    let response = call(&fx.state, "GET", &uri, &cookie, None, None, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let team = fx.identity.create_team(fx.org, "audit-team").await.unwrap();
    fx.identity
        .add_team_membership(fx.org, team.id, user.id, MembershipRole::Admin)
        .await
        .unwrap();
    backfill(&pool).await;
    let response = call(&fx.state, "GET", &uri, &cookie, None, None, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // The summary is the same read, under the same permission.
    let summary = format!("{uri}/summary?group_by=outcome");
    let response = call(&fx.state, "GET", &summary, &cookie, None, None, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    pool.close().await;
}

#[tokio::test]
async fn audit_events_page_tokens_are_stable_and_filterable() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-pages").await;
    for (action, age) in [
        ("audit.filter", 3_i64),
        ("audit.other", 2),
        ("audit.filter", 1),
    ] {
        sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata, occurred_at) VALUES ($1,$2,$3,$4,'organization',$2,jsonb_build_object('target_id',$2),CURRENT_TIMESTAMP - ($5 * INTERVAL '1 second'))")
            .bind(Uuid::new_v4()).bind(fx.org.0).bind(fx.owner.0).bind(action).bind(age)
            .execute(&pool).await.unwrap();
    }
    let uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?page_size=1&action=audit.filter",
        fx.org.0
    );
    let first = json(call(&fx.state, "GET", &uri, &fx.owner_cookie, None, None, None).await).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let token = first["next_page_token"].as_str().unwrap();
    let next_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?page_size=1&action=audit.filter&page_token={token}",
        fx.org.0
    );
    let second = json(
        call(
            &fx.state,
            "GET",
            &next_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    assert_eq!(second["items"][0]["action"], "audit.filter");
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
    pool.close().await;
}

#[tokio::test]
async fn audit_events_response_excludes_sensitive_metadata() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-safe").await;
    let event_id = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata, before_metadata, after_metadata) VALUES ($1,$2,$3,'audit.safe','service_account',$4,jsonb_build_object('target_id',$4,'resource_type','service_account'),jsonb_build_object('status','active','version',1),jsonb_build_object('status','revoked','version',2))")
        .bind(event_id)
        .bind(fx.org.0)
        .bind(fx.owner.0)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let uri = format!("/api/admin/v1/organizations/{}/audit-events", fx.org.0);
    let response =
        json(call(&fx.state, "GET", &uri, &fx.owner_cookie, None, None, None).await).await;
    let persisted: Value =
        sqlx::query_scalar("SELECT metadata FROM gateway.audit_event WHERE id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(persisted["target_id"].as_str().is_some());
    assert!(
        response["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == event_id.to_string())
    );
    let serialized = response.to_string();
    assert!(!serialized.contains("metadata"));
    assert!(!serialized.contains("prompt"));
    assert!(!serialized.contains("secret"));
    assert!(!serialized.contains("completion"));
    pool.close().await;
}

#[tokio::test]
async fn audit_events_allow_owner_admin_and_auditor_and_emit_read_audit() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-success-roles").await;
    let admin = fx
        .identity
        .create_user(fx.org, None, None, None, Some("admin"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, admin.id, MembershipRole::Admin)
        .await
        .unwrap();
    let auditor = fx
        .identity
        .create_user(fx.org, None, None, None, Some("auditor"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, auditor.id, MembershipRole::Auditor)
        .await
        .unwrap();
    backfill(&pool).await;
    let admin_cookie = session_cookie(&pool, &fx.crypto, admin.id).await;
    let auditor_cookie = session_cookie(&pool, &fx.crypto, auditor.id).await;
    let uri = format!("/api/admin/v1/organizations/{}/audit-events", fx.org.0);
    let request_id = Uuid::new_v4().to_string();
    assert_eq!(
        call_full(
            &fx.state,
            "GET",
            &uri,
            &fx.owner_cookie,
            None,
            None,
            Some(&request_id),
            None,
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&fx.state, "GET", &uri, &admin_cookie, None, None, None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&fx.state, "GET", &uri, &auditor_cookie, None, None, None)
            .await
            .status(),
        StatusCode::OK
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway.audit_event WHERE organization_id = $1 AND actor_user_id = $2 AND action = 'audit.read' AND request_id = $3")
        .bind(fx.org.0).bind(fx.owner.0).bind(Uuid::parse_str(&request_id).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
    pool.close().await;
}

#[tokio::test]
async fn audit_events_reject_operator_read_only_and_team_admin_only_roles() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-denied-roles").await;
    let operator = fx
        .identity
        .create_user(fx.org, None, None, None, Some("operator"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, operator.id, MembershipRole::Member)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled) VALUES ($1,$2,$3,'operator',true)")
        .bind(Uuid::new_v4()).bind(fx.org.0).bind(operator.id.0).execute(&pool).await.unwrap();
    let read_only = fx
        .identity
        .create_user(fx.org, None, None, None, Some("read-only"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, read_only.id, MembershipRole::Member)
        .await
        .unwrap();
    let team_admin = fx
        .identity
        .create_user(fx.org, None, None, None, Some("team-admin"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, team_admin.id, MembershipRole::Member)
        .await
        .unwrap();
    let team = fx
        .identity
        .create_team(fx.org, "audit-denied-team")
        .await
        .unwrap();
    fx.identity
        .add_team_membership(fx.org, team.id, team_admin.id, MembershipRole::Admin)
        .await
        .unwrap();
    backfill(&pool).await;
    let uri = format!("/api/admin/v1/organizations/{}/audit-events", fx.org.0);
    for user in [operator.id, read_only.id, team_admin.id] {
        let cookie = session_cookie(&pool, &fx.crypto, user).await;
        assert_eq!(
            call(&fx.state, "GET", &uri, &cookie, None, None, None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn audit_events_filters_and_same_timestamp_keyset_are_deterministic() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-filter-pages").await;
    let other = fx
        .identity
        .create_user(fx.org, None, None, None, Some("other"))
        .await
        .unwrap();
    fx.identity
        .add_organization_membership(fx.org, other.id, MembershipRole::Auditor)
        .await
        .unwrap();
    backfill(&pool).await;
    let timestamp = "2020-01-02 03:04:05+00";
    let mut ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    ids.sort_unstable();
    for id in ids {
        sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, outcome, metadata, occurred_at) VALUES ($1,$2,$3,'audit.same_timestamp','team',$4,'denied',jsonb_build_object('target_id',$4,'resource_type','team'),$5::timestamptz)")
            .bind(id).bind(fx.org.0).bind(other.id.0).bind(id).bind(timestamp).execute(&pool).await.unwrap();
    }
    let actor_decoy = Uuid::new_v4();
    let target_decoy = Uuid::new_v4();
    let outcome_decoy = Uuid::new_v4();
    let before_decoy = Uuid::new_v4();
    let after_decoy = Uuid::new_v4();
    for (id, actor, target_type, outcome, occurred_at) in [
        (actor_decoy, fx.owner.0, "team", "denied", timestamp),
        (
            target_decoy,
            other.id.0,
            "organization",
            "denied",
            timestamp,
        ),
        (outcome_decoy, other.id.0, "team", "succeeded", timestamp),
        (
            before_decoy,
            other.id.0,
            "team",
            "denied",
            "2018-01-01 00:00:00+00",
        ),
        (
            after_decoy,
            other.id.0,
            "team",
            "denied",
            "2022-01-01 00:00:00+00",
        ),
    ] {
        sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, outcome, metadata, occurred_at) VALUES ($1,$2,$3,'audit.same_timestamp',$4,$1,$5,jsonb_build_object('target_id',$1,'resource_type',$4),$6::timestamptz)")
            .bind(id).bind(fx.org.0).bind(actor).bind(target_type).bind(outcome).bind(occurred_at)
            .execute(&pool).await.unwrap();
    }
    let uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?actor={}&target_type=team&outcome=denied&occurred_after=2019-01-01T00:00:00Z&occurred_before=2021-01-01T00:00:00Z&action=audit.same_timestamp&page_size=1",
        fx.org.0, other.id.0
    );
    let mut page =
        json(call(&fx.state, "GET", &uri, &fx.owner_cookie, None, None, None).await).await;
    let mut seen = Vec::new();
    loop {
        seen.push(Uuid::parse_str(page["items"][0]["id"].as_str().unwrap()).unwrap());
        let Some(token) = page["next_page_token"].as_str() else {
            break;
        };
        let next = format!(
            "/api/admin/v1/organizations/{}/audit-events?actor={}&target_type=team&outcome=denied&occurred_after=2019-01-01T00:00:00Z&occurred_before=2021-01-01T00:00:00Z&action=audit.same_timestamp&page_size=1&page_token={token}",
            fx.org.0, other.id.0
        );
        page = json(call(&fx.state, "GET", &next, &fx.owner_cookie, None, None, None).await).await;
    }
    assert_eq!(seen, vec![ids[2], ids[1], ids[0]]);

    let returned_ids = |page: &Value| {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| Uuid::parse_str(item["id"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };
    let actor_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?actor={}&action=audit.same_timestamp",
        fx.org.0, other.id.0
    );
    let actor_page = json(
        call(
            &fx.state,
            "GET",
            &actor_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    let actor_results = returned_ids(&actor_page);
    assert!(actor_results.contains(&ids[0]));
    assert!(!actor_results.contains(&actor_decoy));
    let target_type_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?target_type=team&action=audit.same_timestamp",
        fx.org.0
    );
    let target_type_page = json(
        call(
            &fx.state,
            "GET",
            &target_type_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    let target_type_results = returned_ids(&target_type_page);
    assert!(target_type_results.contains(&ids[0]));
    assert!(!target_type_results.contains(&target_decoy));
    let outcome_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?outcome=denied&action=audit.same_timestamp",
        fx.org.0
    );
    let outcome_page = json(
        call(
            &fx.state,
            "GET",
            &outcome_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    let outcome_results = returned_ids(&outcome_page);
    assert!(outcome_results.contains(&ids[0]));
    assert!(!outcome_results.contains(&outcome_decoy));
    let after_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?occurred_after=2019-01-01T00:00:00Z&action=audit.same_timestamp",
        fx.org.0
    );
    let after_page = json(
        call(
            &fx.state,
            "GET",
            &after_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    let after_results = returned_ids(&after_page);
    assert!(after_results.contains(&ids[0]));
    assert!(!after_results.contains(&before_decoy));
    let before_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?occurred_before=2021-01-01T00:00:00Z&action=audit.same_timestamp",
        fx.org.0
    );
    let before_page = json(
        call(
            &fx.state,
            "GET",
            &before_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    let before_results = returned_ids(&before_page);
    assert!(before_results.contains(&ids[0]));
    assert!(!before_results.contains(&after_decoy));
    let type_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?target_type=team&target_id={}&outcome=denied&occurred_after=2019-01-01T00:00:00Z&occurred_before=2021-01-01T00:00:00Z",
        fx.org.0, ids[0]
    );
    let filtered = json(
        call(
            &fx.state,
            "GET",
            &type_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);

    let first_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?action=audit.same_timestamp&page_size=1",
        fx.org.0
    );
    let first = json(
        call(
            &fx.state,
            "GET",
            &first_uri,
            &fx.owner_cookie,
            None,
            None,
            None,
        )
        .await,
    )
    .await;
    let token = first["next_page_token"].as_str().unwrap();
    let tampered = format!(
        "{}/api/admin/v1/organizations/{}/audit-events?action=audit.same_timexxx&page_size=1&page_token={token}",
        "", fx.org.0
    );
    assert_eq!(
        call(
            &fx.state,
            "GET",
            &tampered,
            &fx.owner_cookie,
            None,
            None,
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let mut bad_token = token.to_owned();
    bad_token.replace_range(3..4, "x");
    let bad_uri = format!(
        "/api/admin/v1/organizations/{}/audit-events?action=audit.same_timestamp&page_size=1&page_token={bad_token}",
        fx.org.0
    );
    assert_eq!(
        call(
            &fx.state,
            "GET",
            &bad_uri,
            &fx.owner_cookie,
            None,
            None,
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    pool.close().await;
}

#[tokio::test]
async fn audit_events_leave_out_views_on_request() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-views").await;
    backfill(&pool).await;
    for action in ["viewtest.update", "viewtest.read", "viewtest.list"] {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata) VALUES ($1,$2,$3,$4,'team',$1,jsonb_build_object('target_id',$1,'resource_type','team'))")
            .bind(id).bind(fx.org.0).bind(fx.owner.0).bind(action)
            .execute(&pool).await.unwrap();
    }
    let get = |query: &str| {
        let state = fx.state.clone();
        let cookie = fx.owner_cookie.clone();
        let org = fx.org.0;
        let query = query.to_owned();
        async move {
            let response = call(
                &state,
                "GET",
                &format!("/api/admin/v1/organizations/{org}/audit-events{query}"),
                &cookie,
                None,
                None,
                None,
            )
            .await;
            let status = response.status();
            (status, json(response).await)
        }
    };
    let actions = |page: &Value| {
        let mut actions: Vec<String> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["action"].as_str().unwrap().to_owned())
            .collect();
        actions.sort();
        actions
    };
    let (status, page) = get("?q=viewtest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        actions(&page),
        ["viewtest.list", "viewtest.read", "viewtest.update"]
    );
    let (status, page) = get("?q=viewtest&exclude_views=true").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(actions(&page), ["viewtest.update"]);
    // Events name who acted, and a team or service account target.
    let name: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM gateway.user_account WHERE id = $1")
            .bind(fx.owner.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(page["items"][0]["actor"]["name"], json!(name));
    assert_eq!(page["items"][0]["target_name"], Value::Null);
    let team = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.team (id, organization_id, name) VALUES ($1, $2, 'Named team')",
    )
    .bind(team)
    .bind(fx.org.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata) VALUES ($1,$2,$3,'viewtest.rename','team',$4,jsonb_build_object('target_id',$4,'resource_type','team'))")
        .bind(Uuid::new_v4()).bind(fx.org.0).bind(fx.owner.0).bind(team)
        .execute(&pool).await.unwrap();
    let (_, page) = get("?action=viewtest.rename").await;
    assert_eq!(page["items"][0]["target_name"], "Named team");
    let (_, page) =
        get("?action=viewtest.update,viewtest.read,viewtest.list&exclude_views=false").await;
    assert_eq!(actions(&page).len(), 3);
    let (status, summary) = get("/summary?group_by=action&q=viewtest&exclude_views=true").await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    // The update and the rename; not the read or the list.
    assert_eq!(summary["total"], 2);
    let (status, _) = get("?exclude_views=maybe").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn audit_events_take_lists_and_search_and_summarize_by_dimension() {
    let _guard = database_guard();
    let Some(pool) = connect_pool().await else {
        return;
    };
    apply_migrations(&pool).await;
    let fx = fixture(&pool, "audit-summary").await;
    backfill(&pool).await;
    // Three actions, three outcomes, two target types, on two days.
    for (action, target_type, outcome, occurred_at) in [
        (
            "summary.alpha",
            "team",
            "succeeded",
            "2020-01-01 10:00:00+00",
        ),
        (
            "summary.alpha",
            "team",
            "succeeded",
            "2020-01-01 11:00:00+00",
        ),
        ("summary.alpha", "team", "denied", "2020-01-02 10:00:00+00"),
        (
            "summary.beta",
            "api_key",
            "failed",
            "2020-01-02 11:00:00+00",
        ),
        (
            "summary.beta",
            "api_key",
            "succeeded",
            "2020-01-02 12:00:00+00",
        ),
        (
            "summary_gamma",
            "api_key",
            "succeeded",
            "2020-01-02 13:00:00+00",
        ),
    ] {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, outcome, metadata, occurred_at) VALUES ($1,$2,$3,$4,$5,$1,$6,jsonb_build_object('target_id',$1,'resource_type',$5),$7::timestamptz)")
            .bind(id).bind(fx.org.0).bind(fx.owner.0).bind(action).bind(target_type).bind(outcome).bind(occurred_at)
            .execute(&pool).await.unwrap();
    }
    let get = |query: String| {
        let state = fx.state.clone();
        let cookie = fx.owner_cookie.clone();
        let org = fx.org.0;
        async move {
            let response = call(
                &state,
                "GET",
                &format!("/api/admin/v1/organizations/{org}/audit-events{query}"),
                &cookie,
                None,
                None,
                None,
            )
            .await;
            let status = response.status();
            (status, json(response).await)
        }
    };
    let actions = |page: &Value| {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                format!(
                    "{}/{}",
                    item["action"].as_str().unwrap(),
                    item["outcome"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>()
    };

    // A list matches any of its values; lists combine with AND.
    let (status, page) =
        get("?action=summary.beta,summary.alpha&outcome=denied,failed".into()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        actions(&page),
        ["summary.beta/failed", "summary.alpha/denied"]
    );
    let (_, page) =
        get("?actor_type=user&target_type=api_key,organization&action=summary.beta".into()).await;
    assert_eq!(actions(&page).len(), 2);
    let (_, page) = get("?actor_type=system&action=summary.beta".into()).await;
    assert_eq!(actions(&page).len(), 0);

    // Search is a case-insensitive substring of the action or target type,
    // with LIKE's wildcards taken literally.
    let (_, page) = get("?q=SUMMARY.BE".into()).await;
    assert_eq!(
        actions(&page),
        ["summary.beta/succeeded", "summary.beta/failed"]
    );
    let (_, page) = get("?q=summary_".into()).await;
    assert_eq!(actions(&page), ["summary_gamma/succeeded"]);
    let (_, page) = get("?q=summary%25".into()).await;
    assert_eq!(actions(&page).len(), 0);
    let (_, page) = get("?q=API_KEY&outcome=succeeded".into()).await;
    assert_eq!(
        actions(&page),
        ["summary_gamma/succeeded", "summary.beta/succeeded"]
    );

    // A page token continues only the query it came from.
    let (_, page) = get("?q=summary&page_size=1".into()).await;
    let token = page["next_page_token"].as_str().unwrap().to_owned();
    let (status, _) = get(format!("?q=summary&page_size=1&page_token={token}")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(format!("?q=summary.alpha&page_size=1&page_token={token}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Summaries count the matching events by up to two dimensions, largest
    // groups first, with the total across every group.
    let (status, summary) = get("/summary?group_by=outcome&q=summary".into()).await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    assert_eq!(summary["group_by"], json!(["outcome"]));
    assert_eq!(summary["total"], 6);
    assert_eq!(summary["truncated"], false);
    let groups: Vec<(String, i64)> = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["values"]["outcome"].as_str().unwrap().to_owned(),
                group["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("succeeded".into(), 4),
            ("denied".into(), 1),
            ("failed".into(), 1)
        ]
    );

    let (_, summary) = get("/summary?group_by=day,target_type&q=summary".into()).await;
    let first = &summary["groups"][0];
    assert_eq!(
        first["values"],
        json!({ "day": "2020-01-02", "target_type": "api_key" })
    );
    assert_eq!(first["count"], 3);
    assert_eq!(first["first_occurred_at"], "2020-01-02T11:00:00Z");
    assert_eq!(first["last_occurred_at"], "2020-01-02T13:00:00Z");
    let (_, summary) = get("/summary?group_by=actor&action=summary.alpha".into()).await;
    assert_eq!(
        summary["groups"],
        json!([{
            "values": { "actor": fx.owner.0.to_string() },
            "count": 3,
            "first_occurred_at": "2020-01-01T10:00:00Z",
            "last_occurred_at": "2020-01-02T10:00:00Z",
        }])
    );

    // Cut to fewer groups, the total still counts every event, from the
    // same statement as the groups.
    let filters = AuditListFilters {
        search: Some("summary".into()),
        ..AuditListFilters::default()
    };
    let (groups, total, truncated) = AuditStore::new(pool.clone())
        .summarize(fx.org, &filters, &[AuditDimension::Outcome], 1)
        .await
        .unwrap();
    assert_eq!(
        (groups.len(), groups[0].count, total, truncated),
        (1, 4, 6, true)
    );

    // A long filter list still pages: the token binds a digest of the query.
    let long: Vec<String> = (0..40)
        .map(|n| format!("summary.{n:02}.{}", "x".repeat(53)))
        .collect();
    let (status, page) = get(format!(
        "?action={},summary.alpha&page_size=1",
        long.join(",")
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let token = page["next_page_token"].as_str().unwrap().to_owned();
    let (status, page) = get(format!(
        "?action={},summary.alpha&page_size=1&page_token={token}",
        long.join(",")
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(actions(&page).len(), 1);

    for invalid in [
        "/summary",
        "/summary?group_by=nothing",
        "/summary?group_by=outcome,outcome",
        "/summary?group_by=outcome,action,day",
        "/summary?group_by=outcome&page_size=1",
        "?outcome=succeeded,,denied",
        "?actor_type=robot",
    ] {
        let (status, problem) = get(invalid.into()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
        assert_eq!(problem["code"], "invalid_audit_query", "{invalid}");
    }
    let (status, _) = get(format!("?q={}", "x".repeat(201))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
