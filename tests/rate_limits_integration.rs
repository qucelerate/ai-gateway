#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the rate-limit admin lane: list, set and delete
//! policies of the organization, a team, or a service account, driven through
//! the real router, RBAC, idempotency, versioning, and audit.
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
    OrganizationId, TeamId, UserAccountId,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    persistence::{AdminStore, AuthStore, IdentityStore, rate_limits::RateLimitStore},
    transport::rate_limits::router,
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
const TEST_PEPPER: &[u8] = b"rate-limit-integration-pepper";
const HMAC_KEY: &[u8] = b"rate-limit-integration-hmac-key-0032";

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

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
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
        let crypto = AuthCrypto::new(HMAC_KEY, &[19_u8; 32]).unwrap();
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            RateLimitStore::new(pool.clone()),
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

    async fn send(
        &self,
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
        let response = self
            .router
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

    async fn get(&self, uri: &str, session: &(String, String)) -> (StatusCode, Value, HeaderMap) {
        self.send(Method::GET, uri, Some(session), &[], None).await
    }
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/rate-limits{path}", org.0)
}

fn write_headers<'a>(key: &'a str, if_match: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![("idempotency-key", key)];
    if let Some(if_match) = if_match {
        headers.push(("if-match", if_match));
    }
    headers
}

async fn audit_rows(pool: &PgPool, org: OrganizationId) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT action, outcome, entity_type FROM gateway.audit_event
         WHERE organization_id = $1 ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn policies_are_set_versioned_listed_and_deleted_per_scope() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("rate-limits").await;
    let owner_id = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner_id).await;
    let team = harness
        .identity
        .create_team(org, "limited")
        .await
        .unwrap()
        .id;
    let account = harness
        .identity
        .create_service_account(org, owner_id, None, "limited-service")
        .await
        .unwrap()
        .id;

    let organization = uri(org, "/organization");
    let body = json!({"requests_per_minute": 600, "max_concurrent": 20});
    let (status, created, headers) = harness
        .send(
            Method::PUT,
            &organization,
            Some(&owner),
            &write_headers("org-1", Some("*")),
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(created["scope"], "organization");
    assert_eq!(created["requests_per_minute"], json!(600));
    assert_eq!(created["max_concurrent"], json!(20));
    assert_eq!(created["version"], json!(1));

    // A replay returns the recorded response.
    let (status, replay, _) = harness
        .send(
            Method::PUT,
            &organization,
            Some(&owner),
            &write_headers("org-1", Some("*")),
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, created);

    // A stale version conflicts; the current one replaces the policy, and a
    // limit left out is removed.
    let (status, conflict, _) = harness
        .send(
            Method::PUT,
            &organization,
            Some(&owner),
            &write_headers("org-stale", Some("\"v7\"")),
            Some(&json!({"requests_per_minute": 60})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "version_conflict");
    let (status, updated, headers) = harness
        .send(
            Method::PUT,
            &organization,
            Some(&owner),
            &write_headers("org-2", Some("\"v1\"")),
            Some(&json!({"requests_per_minute": 60})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(updated["id"], created["id"]);
    assert_eq!(updated["max_concurrent"], Value::Null);

    for (key, body) in [
        ("empty", json!({})),
        ("zero", json!({"max_concurrent": 0})),
        ("huge", json!({"requests_per_minute": 1_000_001})),
    ] {
        let (status, problem, _) = harness
            .send(
                Method::PUT,
                &organization,
                Some(&owner),
                &write_headers(key, Some("*")),
                Some(&body),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{key}: {problem}");
        assert_eq!(problem["code"], "invalid_rate_limit");
    }
    let (status, problem, _) = harness
        .send(
            Method::PUT,
            &organization,
            Some(&owner),
            &write_headers("unknown-field", Some("*")),
            Some(&json!({"requests_per_minute": 5, "burst": 3})),
        )
        .await;
    assert!(status.is_client_error(), "{problem}");

    let (status, team_policy, _) = harness
        .send(
            Method::PUT,
            &uri(org, &format!("/teams/{}", team.0)),
            Some(&owner),
            &write_headers("team-1", Some("*")),
            Some(&json!({"max_concurrent": 4})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{team_policy}");
    assert_eq!(team_policy["scope"], "team");
    assert_eq!(team_policy["team_id"], json!(team.0));
    let (status, account_policy, _) = harness
        .send(
            Method::PUT,
            &uri(org, &format!("/service-accounts/{}", account.0)),
            Some(&owner),
            &write_headers("account-1", Some("*")),
            Some(&json!({"requests_per_minute": 30})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{account_policy}");
    assert_eq!(account_policy["scope"], "service-account");
    assert_eq!(account_policy["service_account_id"], json!(account.0));

    // A team or service account of another organization, or none at all, is
    // not found.
    let other = harness.org("rate-limits-other").await;
    let foreign_team = harness
        .identity
        .create_team(other, "foreign")
        .await
        .unwrap()
        .id;
    for path in [
        format!("/teams/{}", foreign_team.0),
        format!("/teams/{}", Uuid::new_v4()),
        format!("/service-accounts/{}", Uuid::new_v4()),
    ] {
        let (status, problem, _) = harness
            .send(
                Method::PUT,
                &uri(org, &path),
                Some(&owner),
                &write_headers(&path, Some("*")),
                Some(&json!({"max_concurrent": 1})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {problem}");
    }

    // The list pages through every policy of the organization.
    let (status, first, _) = harness.get(&uri(org, "?page_size=2"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let token = first["next_page_token"].as_str().unwrap();
    let (status, second, _) = harness
        .get(
            &uri(org, &format!("?page_size=2&page_token={token}")),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert!(second["next_page_token"].is_null());
    let mut scopes: Vec<String> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["items"].as_array().unwrap())
        .map(|item| item["scope"].as_str().unwrap().to_owned())
        .collect();
    scopes.sort();
    assert_eq!(scopes, ["organization", "service-account", "team"]);

    // Delete needs the current version, then the policy is gone; a replay
    // reports the recorded deletion.
    let team_uri = uri(org, &format!("/teams/{}", team.0));
    let (status, _, _) = harness
        .send(
            Method::DELETE,
            &team_uri,
            Some(&owner),
            &write_headers("team-delete", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &team_uri,
            Some(&owner),
            &write_headers("team-delete", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(true));
    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &team_uri,
            Some(&owner),
            &write_headers("team-delete-again", Some("*")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (_, list, _) = harness.get(&uri(org, ""), &owner).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);

    // Re-creating the policy continues its version history.
    let (status, recreated, headers) = harness
        .send(
            Method::PUT,
            &team_uri,
            Some(&owner),
            &write_headers("team-2", Some("*")),
            Some(&json!({"max_concurrent": 2})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{recreated}");
    assert_eq!(recreated["id"], team_policy["id"]);
    assert_eq!(headers["etag"], "\"v3\"");

    let audit = audit_rows(&harness.pool, org).await;
    let actions: Vec<(&str, &str)> = audit
        .iter()
        .filter(|(_, outcome, _)| outcome == "succeeded")
        .map(|(action, _, entity)| (action.as_str(), entity.as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            ("rate_limit_policy.set", "rate_limit_policy"),
            ("rate_limit_policy.set", "rate_limit_policy"),
            ("rate_limit_policy.set", "rate_limit_policy"),
            ("rate_limit_policy.set", "rate_limit_policy"),
            ("rate_limit_policy.deleted", "rate_limit_policy"),
            ("rate_limit_policy.set", "rate_limit_policy"),
        ]
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn an_api_keys_policy_limits_tokens_too_and_outlives_the_keys_revocation() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("rate-limits-keys").await;
    let owner_id = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner_id).await;
    sqlx::query(
        "UPDATE gateway.organization_membership SET role = 'owner'
         WHERE organization_id = $1 AND user_id = $2",
    )
    .bind(org.0)
    .bind(owner_id.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    let account = harness
        .identity
        .create_service_account(org, owner_id, None, "keyed-service")
        .await
        .unwrap()
        .id;
    let key = harness
        .identity
        .create_api_key(
            org,
            account,
            &[gateway::domain::identity::ApiKeyScope::Inference],
            None,
            owner_id,
        )
        .await
        .unwrap()
        .id;
    let key_uri = uri(org, &format!("/api-keys/{}", key.0));
    let body = json!({"requests_per_minute": 10, "tokens_per_minute": 50_000, "max_concurrent": 2});
    let (status, created, headers) = harness
        .send(
            Method::PUT,
            &key_uri,
            Some(&owner),
            &write_headers("key-1", Some("*")),
            Some(&body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(created["scope"], "api-key");
    assert_eq!(created["api_key_id"], json!(key.0));
    assert_eq!(created["team_id"], Value::Null);
    assert_eq!(created["service_account_id"], Value::Null);
    assert_eq!(created["tokens_per_minute"], json!(50_000));
    assert_eq!(created["max_concurrent"], json!(2));

    // Tokens per minute alone is a policy; out of range it is not.
    let organization = uri(org, "/organization");
    let (status, tokens_only, _) = harness
        .send(
            Method::PUT,
            &organization,
            Some(&owner),
            &write_headers("org-tokens", Some("*")),
            Some(&json!({"tokens_per_minute": 1_000_000_000})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{tokens_only}");
    assert_eq!(tokens_only["requests_per_minute"], Value::Null);
    for (name, body) in [
        ("tokens-zero", json!({"tokens_per_minute": 0})),
        ("tokens-huge", json!({"tokens_per_minute": 1_000_000_001})),
    ] {
        let (status, problem, _) = harness
            .send(
                Method::PUT,
                &key_uri,
                Some(&owner),
                &write_headers(name, Some("*")),
                Some(&body),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{name}: {problem}"
        );
        assert_eq!(problem["code"], "invalid_rate_limit");
    }

    // A key of another organization, or none at all, is not found.
    let other = harness.org("rate-limits-keys-other").await;
    let other_owner = harness.user(other, None, "organization_owner").await;
    sqlx::query(
        "UPDATE gateway.organization_membership SET role = 'owner'
         WHERE organization_id = $1 AND user_id = $2",
    )
    .bind(other.0)
    .bind(other_owner.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    let other_account = harness
        .identity
        .create_service_account(other, other_owner, None, "foreign")
        .await
        .unwrap()
        .id;
    let foreign = harness
        .identity
        .create_api_key(
            other,
            other_account,
            &[gateway::domain::identity::ApiKeyScope::Inference],
            None,
            other_owner,
        )
        .await
        .unwrap()
        .id;
    for path in [
        format!("/api-keys/{}", foreign.0),
        format!("/api-keys/{}", Uuid::new_v4()),
    ] {
        let (status, problem, _) = harness
            .send(
                Method::PUT,
                &uri(org, &path),
                Some(&owner),
                &write_headers(&path, Some("*")),
                Some(&json!({"max_concurrent": 1})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {problem}");
    }

    // A revoked key keeps its policy, listed, changeable and removable.
    assert!(
        harness
            .identity
            .revoke_api_key(org, key, owner_id)
            .await
            .unwrap()
    );
    let (_, list, _) = harness.get(&uri(org, ""), &owner).await;
    let listed: Vec<&Value> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["scope"] == "api-key")
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["api_key_id"], json!(key.0));
    let (status, updated, headers) = harness
        .send(
            Method::PUT,
            &key_uri,
            Some(&owner),
            &write_headers("key-2", Some("\"v1\"")),
            Some(&json!({"tokens_per_minute": 1_000})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(updated["id"], created["id"]);
    assert_eq!(updated["requests_per_minute"], Value::Null);
    assert_eq!(updated["tokens_per_minute"], json!(1_000));
    let (status, _, _) = harness
        .send(
            Method::DELETE,
            &key_uri,
            Some(&owner),
            &write_headers("key-delete", Some("\"v2\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let audit = audit_rows(&harness.pool, org).await;
    let actions: Vec<&str> = audit
        .iter()
        .filter(|(_, outcome, entity)| outcome == "succeeded" && entity == "rate_limit_policy")
        .map(|(action, _, _)| action.as_str())
        .collect();
    assert_eq!(
        actions,
        [
            "rate_limit_policy.set",
            "rate_limit_policy.set",
            "rate_limit_policy.set",
            "rate_limit_policy.deleted",
        ]
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn reads_are_org_wide_and_writes_are_owner_and_admin_only() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("rate-limit-rbac").await;
    let team = harness.identity.create_team(org, "rbac").await.unwrap().id;
    let organization = uri(org, "/organization");

    for (index, (role, team_binding, read, write)) in [
        ("organization_owner", None, true, true),
        ("organization_admin", None, true, true),
        ("operator", None, true, false),
        ("auditor", None, true, false),
        ("read_only_member", None, true, false),
        ("team_admin", Some(team), false, false),
    ]
    .into_iter()
    .enumerate()
    {
        let user = harness.user(org, team_binding, role).await;
        let session = harness.session(user).await;
        let (status, body, _) = harness.get(&uri(org, ""), &session).await;
        let expected = if read {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, expected, "{role} read: {body}");

        let key = format!("rbac-{index}");
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &organization,
                Some(&session),
                &write_headers(&key, Some("*")),
                Some(&json!({"requests_per_minute": 10 + index})),
            )
            .await;
        if write {
            assert!(status.is_success(), "{role} write: {body}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "{role} write: {body}");
            assert_eq!(body["code"], "forbidden");
        }
        // A team admin cannot manage even its own team's policy.
        if team_binding.is_some() {
            let (status, _, _) = harness
                .send(
                    Method::PUT,
                    &uri(org, &format!("/teams/{}", team.0)),
                    Some(&session),
                    &write_headers("team-own", Some("*")),
                    Some(&json!({"max_concurrent": 1})),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
    }

    // Unauthenticated requests are rejected before anything else, and
    // another organization's owner sees nothing here.
    let (status, _, _) = harness
        .send(Method::GET, &uri(org, ""), None, &[], None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let other = harness.org("rate-limit-rbac-other").await;
    let outsider = harness.user(other, None, "organization_owner").await;
    let outsider = harness.session(outsider).await;
    let (status, _, _) = harness.get(&uri(org, ""), &outsider).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Denied writes are audited.
    let denied = audit_rows(&harness.pool, org)
        .await
        .into_iter()
        .filter(|(_, outcome, entity)| outcome == "denied" && entity == "rate_limit_policy")
        .count();
    assert!(denied >= 3, "denied writes are audited: {denied}");
    harness.pool.close().await;
}
