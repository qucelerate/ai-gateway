#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the budget admin lane: list, set and delete budgets
//! of the organization, a team, a service account, or an API key, driven through the real
//! router, RBAC, idempotency, versioning, audit, and the quota buckets that
//! enforce them.
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
    persistence::{AdminStore, AuthStore, IdentityStore, budgets::BudgetStore},
    transport::budgets::router,
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
const TEST_PEPPER: &[u8] = b"budget-integration-pepper";
const HMAC_KEY: &[u8] = b"budget-integration-hmac-key-000032";

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
            BudgetStore::new(pool.clone()),
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
    format!("/api/admin/v1/organizations/{}/budgets{path}", org.0)
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

const DOLLAR: i64 = 100_000_000;

#[tokio::test]
async fn budgets_are_set_versioned_listed_and_deleted_per_scope() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("budgets").await;
    let owner_id = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner_id).await;
    let team = harness
        .identity
        .create_team(org, "budgeted")
        .await
        .unwrap()
        .id;
    harness
        .identity
        .add_team_membership(org, team, owner_id, MembershipRole::Admin)
        .await
        .unwrap();
    let account = harness
        .identity
        .create_service_account(org, owner_id, Some(team), "budgeted-service")
        .await
        .unwrap()
        .id;

    // $100 a month for the organization; USD by default.
    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &uri(org, "/organization"),
            Some(&owner),
            &write_headers("org-1", Some("*")),
            Some(&json!({"limit_minor": 100 * DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(body["currency"], "USD");
    assert_eq!(body["scope"], "organization");
    assert_eq!(body["current"]["consumed_minor"], 0);
    let month = gateway::domain::budget::month_of(chrono::Utc::now());
    assert_eq!(
        body["current"]["period_start"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap(),
        month.0
    );
    // The bucket for this month exists already, with the limit.
    let limit: i64 = sqlx::query_scalar(
        "SELECT limit_minor FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'organization' AND retired_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(limit, 100 * DOLLAR);

    // A replay returns the recorded response; a stale version conflicts.
    let (status, replay, _) = harness
        .send(
            Method::PUT,
            &uri(org, "/organization"),
            Some(&owner),
            &write_headers("org-1", Some("*")),
            Some(&json!({"limit_minor": 100 * DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["id"], body["id"]);
    let (status, conflict, _) = harness
        .send(
            Method::PUT,
            &uri(org, "/organization"),
            Some(&owner),
            &write_headers("org-stale", Some("\"v7\"")),
            Some(&json!({"limit_minor": DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "version_conflict");

    // Raising the limit keeps the month's bucket; a fixed window that has
    // begun replaces it with a new one and retires the month's.
    let (status, raised, headers) = harness
        .send(
            Method::PUT,
            &uri(org, "/organization"),
            Some(&owner),
            &write_headers("org-2", Some("\"v1\"")),
            Some(&json!({"limit_minor": 250 * DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{raised}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(raised["limit_minor"], 250 * DOLLAR);
    let now = chrono::Utc::now();
    let starts_at = (now - chrono::Duration::days(1)).to_rfc3339();
    let ends_at = (now + chrono::Duration::days(6)).to_rfc3339();
    let (status, fixed, _) = harness
        .send(
            Method::PUT,
            &uri(org, "/organization"),
            Some(&owner),
            &write_headers("org-3", Some("\"v2\"")),
            Some(&json!({"limit_minor": 10 * DOLLAR, "period": "fixed",
                         "starts_at": starts_at, "ends_at": ends_at})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{fixed}");
    assert_eq!(fixed["period"], "fixed");
    let active: Vec<(i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT limit_minor, period_start FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'organization' AND retired_at IS NULL",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(active[0].0, 10 * DOLLAR);
    assert_ne!(active[0].1, month.0);

    // Team and service-account budgets; the service account's bucket carries
    // its team, as reservations match it.
    for (path, key) in [
        (format!("/teams/{}", team.0), "team-1"),
        (format!("/service-accounts/{}", account.0), "account-1"),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &uri(org, &path),
                Some(&owner),
                &write_headers(key, Some("*")),
                Some(&json!({"limit_minor": 5 * DOLLAR, "period": "monthly"})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let account_team: Option<Uuid> = sqlx::query_scalar(
        "SELECT team_id FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'service-account' AND retired_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(account_team, Some(team.0));

    let (status, list, _) = harness.get(&uri(org, ""), &owner).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let scopes: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["scope"].as_str().unwrap())
        .collect();
    assert_eq!(scopes.len(), 3, "{list}");
    assert!(
        scopes.contains(&"organization")
            && scopes.contains(&"team")
            && scopes.contains(&"service-account")
    );

    // Invalid input and unknown scopes.
    for invalid in [
        json!({"limit_minor": -1, "period": "monthly"}),
        json!({"limit_minor": 1, "period": "monthly", "starts_at": starts_at}),
        json!({"limit_minor": 1, "period": "fixed"}),
        json!({"limit_minor": 1, "period": "monthly", "currency": "usd"}),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &uri(org, "/organization"),
                Some(&owner),
                &write_headers("invalid", Some("*")),
                Some(&invalid),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid} {body}");
        assert_eq!(body["code"], "invalid_budget");
    }
    let (status, _, _) = harness
        .send(
            Method::PUT,
            &uri(org, &format!("/teams/{}", Uuid::new_v4())),
            Some(&owner),
            &write_headers("missing-team", Some("*")),
            Some(&json!({"limit_minor": 1, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Removing the team's budget retires its bucket.
    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &uri(org, &format!("/teams/{}", team.0)),
            Some(&owner),
            &write_headers("team-delete", Some("\"v1\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let team_active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'team' AND retired_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(team_active, 0);
    let (status, _, _) = harness
        .send(
            Method::DELETE,
            &uri(org, &format!("/teams/{}", team.0)),
            Some(&owner),
            &write_headers("team-delete-again", Some("*")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let audit = audit_rows(&harness.pool, org).await;
    let actions: Vec<&str> = audit
        .iter()
        .filter(|(_, outcome, entity)| outcome == "succeeded" && entity == "budget_policy")
        .map(|(action, ..)| action.as_str())
        .collect();
    assert_eq!(
        actions,
        [
            "budget_policy.set",
            "budget_policy.set",
            "budget_policy.set",
            "budget_policy.set",
            "budget_policy.set",
            "budget_policy.deleted"
        ]
    );
}

#[tokio::test]
async fn an_api_key_gets_a_budget_of_its_own_that_outlives_its_revocation() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("key-budgets").await;
    let owner_id = harness.user(org, None, "organization_owner").await;
    let owner = harness.session(owner_id).await;
    // Creating and revoking keys is audited as an organization owner.
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
        .create_service_account(org, owner_id, None, "shared-service")
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
    let path = format!("/api-keys/{}", key.0);
    let (status, body, headers) = harness
        .send(
            Method::PUT,
            &uri(org, &path),
            Some(&owner),
            &write_headers("key-1", Some("*")),
            Some(&json!({"limit_minor": 3 * DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(body["scope"], "api-key");
    assert_eq!(body["api_key_id"], key.0.to_string());
    assert_eq!(body["service_account_id"], Value::Null);
    // Its bucket names the key alone.
    let bucket: (Option<Uuid>, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT service_account_id, api_key_id, limit_minor FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'api-key' AND retired_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(bucket, (None, Some(key.0), 3 * DOLLAR));
    // The service account and its key are two budgets.
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &uri(org, &format!("/service-accounts/{}", account.0)),
            Some(&owner),
            &write_headers("account-1", Some("*")),
            Some(&json!({"limit_minor": 10 * DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A revoked key keeps its budget, readable and still changeable.
    assert!(
        harness
            .identity
            .revoke_api_key(org, key, owner_id)
            .await
            .unwrap()
    );
    let (status, list, _) = harness.get(&uri(org, ""), &owner).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let listed = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["scope"] == "api-key")
        .cloned()
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(listed["api_key_id"], key.0.to_string());
    let (status, body, _) = harness
        .send(
            Method::PUT,
            &uri(org, &path),
            Some(&owner),
            &write_headers("key-2", Some("\"v1\"")),
            Some(&json!({"limit_minor": DOLLAR, "period": "monthly"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An unknown key, or another organization's, is not found.
    let other = harness.org("key-budgets-other").await;
    let other_owner = harness.user(other, None, "organization_owner").await;
    let other_session = harness.session(other_owner).await;
    for (org, session, id) in [
        (org, &owner, Uuid::new_v4()),
        (other, &other_session, key.0),
    ] {
        let (status, body, _) = harness
            .send(
                Method::PUT,
                &uri(org, &format!("/api-keys/{id}")),
                Some(session),
                &write_headers("missing-key", Some("*")),
                Some(&json!({"limit_minor": 1, "period": "monthly"})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    let (status, body, _) = harness
        .send(
            Method::DELETE,
            &uri(org, &path),
            Some(&owner),
            &write_headers("key-delete", Some("\"v2\"")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind = 'api-key' AND retired_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(active, 0);
}

#[tokio::test]
async fn budget_reads_are_org_wide_and_writes_are_owner_and_admin_only() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("budget-rbac").await;
    let team = harness.identity.create_team(org, "rbac").await.unwrap().id;
    let body = json!({"limit_minor": DOLLAR, "period": "monthly"});
    for (role, team_scoped, read, write) in [
        ("organization_owner", false, true, true),
        ("organization_admin", false, true, true),
        ("operator", false, true, false),
        ("auditor", false, true, false),
        ("read_only_member", false, true, false),
        ("team_admin", true, false, false),
    ] {
        let user = harness.user(org, team_scoped.then_some(team), role).await;
        let session = harness.session(user).await;
        let (status, _, _) = harness.get(&uri(org, ""), &session).await;
        assert_eq!(status == StatusCode::OK, read, "{role} read {status}");
        let key = format!("rbac-{role}");
        let (status, response, _) = harness
            .send(
                Method::PUT,
                &uri(org, "/organization"),
                Some(&session),
                &write_headers(&key, Some("*")),
                Some(&body),
            )
            .await;
        assert_eq!(
            status.is_success(),
            write,
            "{role} write {status} {response}"
        );
    }
    let (status, _, _) = harness
        .send(Method::GET, &uri(org, ""), None, &[], None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
