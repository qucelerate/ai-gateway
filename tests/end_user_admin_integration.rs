#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the end-user admin lane (D-028): settings, plans,
//! end users, credit grants and reversals, driven through the real router,
//! RBAC, idempotency, versioning and audit.
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
    OrganizationId, ServiceAccountId, TeamId, UserAccountId,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    persistence::{AdminStore, AuthStore, IdentityStore, end_users::EndUserStore},
    transport::end_users::router,
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
const TEST_PEPPER: &[u8] = b"end-user-admin-pepper";
const HMAC_KEY: &[u8] = b"end-user-admin-hmac-key-000000032";

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
            EndUserStore::new(pool.clone()),
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

impl Harness {
    async fn app(&self, org: OrganizationId, owner: UserAccountId) -> ServiceAccountId {
        self.identity
            .create_service_account(org, owner, None, "app")
            .await
            .unwrap()
            .id
    }
}

fn uri(org: OrganizationId, account: ServiceAccountId, path: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/service-accounts/{}{path}",
        org.0, account.0
    )
}

fn write(key: &str) -> Vec<(&str, &str)> {
    vec![("idempotency-key", key), ("if-match", "*")]
}

async fn audit_actions(pool: &PgPool, org: OrganizationId) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT action FROM gateway.audit_event
         WHERE organization_id = $1 AND outcome = 'succeeded' ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn settings_plans_and_users_are_versioned_and_guarded() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let h = Harness::new(&database_url).await;
    let org = h.org("end-users").await;
    let owner_id = h.user(org, None, "organization_owner").await;
    let owner = h.session(owner_id).await;
    let app = h.app(org, owner_id).await;

    // Nothing set yet.
    let (status, _, _) = h.get(&uri(org, app, "/end-user-settings"), &owner).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A plan needs the settings first.
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-plans/free"),
            Some(&owner),
            &write("plan-early"),
            Some(&json!({ "allowance_minor": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "end_user_settings_missing");

    let (status, body, headers) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings-1"),
            Some(&json!({ "currency": "USD" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["requirement"], "optional");
    assert_eq!(body["default_plan"], Value::Null);
    assert_eq!(headers["etag"], "\"v1\"");
    // The same key replays.
    let (status, replay, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings-1"),
            Some(&json!({ "currency": "USD" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, body);

    for (name, allowance) in [("free", 100), ("pro", 1_000)] {
        let (status, body, _) = h
            .send(
                Method::PUT,
                &uri(org, app, &format!("/end-user-plans/{name}")),
                Some(&owner),
                &write(&format!("plan-{name}")),
                Some(&json!({ "allowance_minor": allowance })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["currency"], "USD");
    }
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-plans/Bad%20Name"),
            Some(&owner),
            &write("plan-bad"),
            Some(&json!({ "allowance_minor": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_end_user_plan");

    // An unknown default plan, then the right one; the currency is locked
    // once a plan exists.
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings-2"),
            Some(&json!({ "currency": "USD", "default_plan": "gold" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_end_user_plan");
    let (status, body, headers) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &[("idempotency-key", "settings-3"), ("if-match", "\"v1\"")],
            Some(&json!({ "currency": "USD", "default_plan": "free", "requirement": "required" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["default_plan"], "free");
    assert_eq!(headers["etag"], "\"v2\"");
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings-4"),
            Some(&json!({ "currency": "EUR" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "currency_locked");

    let (status, body, _) = h.get(&uri(org, app, "/end-user-plans"), &owner).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plan| plan["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["free", "pro"]);

    // An operator assigns plans but cannot change plans or settings.
    let operator = h.user(org, None, "operator").await;
    let operator = h.session(operator).await;
    let (status, _, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-plans/pro"),
            Some(&operator),
            &write("operator-plan"),
            Some(&json!({ "allowance_minor": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body, headers) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-users/alice%2F1"),
            Some(&operator),
            &write("alice-pro"),
            Some(&json!({ "plan": "pro" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], "alice/1");
    assert_eq!(body["effective_plan"], "pro");
    assert_eq!(body["allowance_minor"], 1_000);
    assert_eq!(body["allowance_remaining_minor"], 1_000);
    assert_eq!(headers["etag"], "\"v1\"");
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-users/alice%2F1"),
            Some(&operator),
            &write("alice-gold"),
            Some(&json!({ "plan": "gold" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "unknown_end_user_plan");

    // A user only requests have seen has version 0 until an admin write.
    sqlx::query(
        "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id)
         VALUES ($1, $2, $3, 'bob')",
    )
    .bind(gateway::domain::end_user::end_user_id(app.0, "bob"))
    .bind(org.0)
    .bind(app.0)
    .execute(&h.pool)
    .await
    .unwrap();
    let (status, body, headers) = h.get(&uri(org, app, "/end-users/bob"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["plan"], Value::Null);
    assert_eq!(body["effective_plan"], "free");
    assert_eq!(headers["etag"], "\"v0\"");
    let (status, body, headers) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-users/bob"),
            Some(&owner),
            &[("idempotency-key", "bob-1"), ("if-match", "\"v0\"")],
            Some(&json!({ "plan": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["etag"], "\"v1\"");
    let (status, _, _) = h.get(&uri(org, app, "/end-users/carol"), &owner).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Paged by ID.
    let (status, first, _) = h
        .get(&uri(org, app, "/end-users?page_size=1"), &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["data"][0]["id"], "alice/1", "{first}");
    let token = first["next_page_token"].as_str().unwrap();
    let (_, second, _) = h
        .get(
            &uri(
                org,
                app,
                &format!("/end-users?page_size=1&page_token={token}"),
            ),
            &owner,
        )
        .await;
    assert_eq!(second["data"][0]["id"], "bob");
    assert_eq!(second["next_page_token"], Value::Null);

    // A plan in use (the default, or a user's) is kept; an unused one goes,
    // and comes back under its name.
    for plan in ["free", "pro"] {
        let (status, body, _) = h
            .send(
                Method::DELETE,
                &uri(org, app, &format!("/end-user-plans/{plan}")),
                Some(&owner),
                &write(&format!("delete-{plan}")),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "plan_in_use");
    }
    let (status, _, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-users/alice%2F1"),
            Some(&owner),
            &write("alice-default"),
            Some(&json!({ "plan": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .send(
            Method::DELETE,
            &uri(org, app, "/end-user-plans/pro"),
            Some(&owner),
            &write("delete-pro-2"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body, _) = h.get(&uri(org, app, "/end-user-plans"), &owner).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-plans/pro"),
            Some(&owner),
            &write("plan-pro-again"),
            Some(&json!({ "allowance_minor": 2_000 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["version"].as_u64().unwrap() > 1);

    let actions = audit_actions(&h.pool, org).await;
    for action in [
        "end_user_setting.set",
        "end_user_plan.set",
        "end_user_plan.deleted",
        "end_user.set",
    ] {
        assert!(actions.iter().any(|a| a == action), "{action}: {actions:?}");
    }
}

#[tokio::test]
async fn grants_are_once_per_reference_and_reversals_take_what_is_unspent() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let h = Harness::new(&database_url).await;
    let org = h.org("end-user-credits").await;
    let owner_id = h.user(org, None, "organization_owner").await;
    let owner = h.session(owner_id).await;
    let app = h.app(org, owner_id).await;
    let grants = uri(org, app, "/end-users/u-1/credit-grants");

    // Credits need the settings' currency.
    let (status, body, _) = h
        .send(
            Method::POST,
            &grants,
            Some(&owner),
            &[],
            Some(&json!({ "reference": "order-1", "amount_minor": 50 })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "end_user_settings_missing");
    let (status, _, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings"),
            Some(&json!({ "currency": "USD" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // Granted once; the same purchase again, with or without a key, is the
    // same grant; another purchase under the reference is refused.
    let purchase = json!({ "reference": "order-1", "amount_minor": 50 });
    let (status, granted, _) = h
        .send(Method::POST, &grants, Some(&owner), &[], Some(&purchase))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{granted}");
    assert_eq!(granted["remaining_minor"], 50);
    assert_eq!(granted["currency"], "USD");
    assert_eq!(granted["expires_at"], Value::Null);
    let (status, again, _) = h
        .send(Method::POST, &grants, Some(&owner), &[], Some(&purchase))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], granted["id"]);
    let (status, again, _) = h
        .send(
            Method::POST,
            &grants,
            Some(&owner),
            &[("idempotency-key", "webhook-retry-7")],
            Some(&purchase),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], granted["id"]);
    for (path, body) in [
        (
            grants.clone(),
            json!({ "reference": "order-1", "amount_minor": 51 }),
        ),
        (
            uri(org, app, "/end-users/u-2/credit-grants"),
            json!({ "reference": "order-1", "amount_minor": 50 }),
        ),
    ] {
        let (status, problem, _) = h
            .send(Method::POST, &path, Some(&owner), &[], Some(&body))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{problem}");
        assert_eq!(problem["code"], "credit_reference_conflict");
    }
    let (status, problem, _) = h
        .send(
            Method::POST,
            &grants,
            Some(&owner),
            &[],
            Some(&json!({
                "reference": "order-2", "amount_minor": 5,
                "expires_at": "2020-01-01T00:00:00Z"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "invalid_credit_grant");

    // The grant shows in the user's balance (no plan: no default, so the
    // user is refused requests, but holds the credit).
    let (status, user, _) = h.get(&uri(org, app, "/end-users/u-1"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{user}");
    assert_eq!(user["credit_balance_minor"], 50);
    assert_eq!(user["effective_plan"], Value::Null);

    // 30 back, then a second refund for more than is left takes only 20.
    let grant_id = granted["id"].as_str().unwrap();
    let reversals = format!("{grants}/{grant_id}/reversals");
    let refund = json!({ "reference": "refund-1", "amount_minor": 30, "reason": "refund" });
    let (status, reversed, _) = h
        .send(Method::POST, &reversals, Some(&owner), &[], Some(&refund))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{reversed}");
    assert_eq!(reversed["reversed_minor"], 30);
    let (status, again, _) = h
        .send(Method::POST, &reversals, Some(&owner), &[], Some(&refund))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], reversed["id"]);
    let (status, problem, _) = h
        .send(
            Method::POST,
            &reversals,
            Some(&owner),
            &[],
            Some(&json!({ "reference": "refund-1", "amount_minor": 31, "reason": "refund" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(problem["code"], "reversal_reference_conflict");
    let (status, reversed, _) = h
        .send(
            Method::POST,
            &reversals,
            Some(&owner),
            &[],
            Some(&json!({ "reference": "chargeback-1", "amount_minor": 100, "reason": "chargeback" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{reversed}");
    assert_eq!(reversed["requested_minor"], 100);
    assert_eq!(reversed["reversed_minor"], 20);

    let (status, list, _) = h.get(&grants, &owner).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let grant = &list["data"][0];
    assert_eq!(grant["amount_minor"], 50);
    assert_eq!(grant["reversed_minor"], 50);
    assert_eq!(grant["remaining_minor"], 0);
    let (_, user, _) = h.get(&uri(org, app, "/end-users/u-1"), &owner).await;
    assert_eq!(user["credit_balance_minor"], 0);

    // A grant of another user is not found under this one.
    let (status, _, _) = h
        .send(
            Method::POST,
            &format!(
                "{}/{grant_id}/reversals",
                uri(org, app, "/end-users/u-2/credit-grants")
            ),
            Some(&owner),
            &[],
            Some(&json!({ "reference": "x", "amount_minor": 1, "reason": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A read-only member reads nothing it cannot read and writes nothing.
    let auditor = h.user(org, None, "auditor").await;
    let auditor = h.session(auditor).await;
    let (status, _, _) = h
        .send(
            Method::POST,
            &grants,
            Some(&auditor),
            &[],
            Some(&json!({ "reference": "order-9", "amount_minor": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let actions = audit_actions(&h.pool, org).await;
    assert_eq!(
        actions
            .iter()
            .filter(|a| *a == "end_user_credit_grant.created")
            .count(),
        1
    );
    assert_eq!(
        actions
            .iter()
            .filter(|a| *a == "end_user_credit_grant.reversed")
            .count(),
        2
    );
}

#[tokio::test]
async fn a_purchase_retried_after_its_credits_expired_still_returns_its_grant() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let h = Harness::new(&database_url).await;
    let org = h.org("end-user-expiry").await;
    let owner_id = h.user(org, None, "organization_owner").await;
    let owner = h.session(owner_id).await;
    let app = h.app(org, owner_id).await;
    let (status, _, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings"),
            Some(&json!({ "currency": "USD" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let grants = uri(org, app, "/end-users/u-1/credit-grants");
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339();
    let purchase = json!({ "reference": "order-1", "amount_minor": 5, "expires_at": expires_at });
    let (status, granted, _) = h
        .send(Method::POST, &grants, Some(&owner), &[], Some(&purchase))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{granted}");
    tokio::time::sleep(StdDuration::from_millis(2_500)).await;
    // Another key, so the coordinator does not replay it: the reference does.
    let (status, again, _) = h
        .send(
            Method::POST,
            &grants,
            Some(&owner),
            &[("idempotency-key", "late-retry")],
            Some(&purchase),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], granted["id"]);
    assert_eq!(again["expired"], true);
    assert_eq!(again["remaining_minor"], 0);
    // A new purchase that has already expired is refused.
    let (status, problem, _) = h
        .send(
            Method::POST,
            &grants,
            Some(&owner),
            &[],
            Some(&json!({ "reference": "order-2", "amount_minor": 5, "expires_at": expires_at })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "invalid_credit_grant");
}

/// A request of `end_user` settled now at `amount`, as the ledger records it.
async fn settled_request(
    pool: &PgPool,
    org: OrganizationId,
    account: ServiceAccountId,
    end_user: &str,
    amount: i64,
) {
    let reservation = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.accounting_reservation
             (id, organization_id, service_account_id, idempotency_key, fingerprint, state,
              reserved_minor, expires_at, output_token_cap, input_price_minor,
              output_price_minor, cached_input_price_minor, reasoning_price_minor,
              other_price_minor, price_version, pricing_confidence, currency, deployment,
              model, trace_id, end_user_id, settled_at)
         SELECT $1, $2, $3, $1::text, $4, 'settled', $5, now() + interval '5 minutes', 4,
                1, 1, 1, 1, 1, 'v1', 'known', 'USD', 'deployment', 'model', $6, u.id, now()
         FROM gateway.end_user u WHERE u.service_account_id = $3 AND u.external_id = $7",
    )
    .bind(reservation)
    .bind(org.0)
    .bind(account.0)
    .bind(vec![7_u8; 32])
    .bind(amount)
    .bind(Uuid::new_v4())
    .bind(end_user)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.accounting_usage
             (id, reservation_id, organization_id, currency, confidence, amount_minor,
              completeness, finality)
         VALUES ($1, $2, $3, 'USD', 'exact', $4, 'complete', 'final')",
    )
    .bind(Uuid::new_v4())
    .bind(reservation)
    .bind(org.0)
    .bind(amount)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn plans_carry_periods_and_rate_limits_and_report_their_users_and_spend() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let h = Harness::new(&database_url).await;
    let org = h.org("end-user-plan-limits").await;
    let owner_id = h.user(org, None, "organization_owner").await;
    let owner = h.session(owner_id).await;
    let app = h.app(org, owner_id).await;
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings"),
            Some(&json!({ "currency": "USD" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A plain plan is monthly and unlimited.
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-plans/free"),
            Some(&owner),
            &write("plan-free"),
            Some(&json!({ "allowance_minor": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["period"], "monthly");
    assert_eq!(body["period_days"], Value::Null);
    assert_eq!(body["requests_per_minute"], Value::Null);
    assert_eq!(body["tokens_per_minute"], Value::Null);
    assert_eq!(body["max_concurrent"], Value::Null);
    assert_eq!(body["users"], 0);
    assert_eq!(body["spent_minor"], 0);
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-plans/pro"),
            Some(&owner),
            &write("plan-pro"),
            Some(
                &json!({ "allowance_minor": 1_000, "period": "days", "period_days": 30,
                          "requests_per_minute": 60, "tokens_per_minute": 100_000,
                          "max_concurrent": 8 }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["period"], "days");
    assert_eq!(body["period_days"], 30);
    assert_eq!(body["requests_per_minute"], 60);
    assert_eq!(body["tokens_per_minute"], 100_000);
    assert_eq!(body["max_concurrent"], 8);
    let start: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(body["period_start"].clone()).unwrap();
    let end: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(body["period_end"].clone()).unwrap();
    assert_eq!(end - start, chrono::Duration::days(30));
    assert!(start <= chrono::Utc::now() && chrono::Utc::now() < end);
    // Out of range or inconsistent.
    for (key, plan) in [
        (
            "bad-days",
            json!({ "allowance_minor": 1, "period_days": 7 }),
        ),
        (
            "bad-days-2",
            json!({ "allowance_minor": 1, "period": "days" }),
        ),
        (
            "bad-days-3",
            json!({ "allowance_minor": 1, "period": "days", "period_days": 367 }),
        ),
        (
            "bad-period",
            json!({ "allowance_minor": 1, "period": "yearly" }),
        ),
        (
            "bad-rpm",
            json!({ "allowance_minor": 1, "requests_per_minute": 0 }),
        ),
        (
            "bad-tpm",
            json!({ "allowance_minor": 1, "tokens_per_minute": 1_000_000_001 }),
        ),
        (
            "bad-concurrent",
            json!({ "allowance_minor": 1, "max_concurrent": 0 }),
        ),
    ] {
        let (status, body, _) = h
            .send(
                Method::PUT,
                &uri(org, app, "/end-user-plans/bad"),
                Some(&owner),
                &write(key),
                Some(&plan),
            )
            .await;
        assert!(status.is_client_error(), "{key}: {status} {body}");
        if status == StatusCode::UNPROCESSABLE_ENTITY && body["code"] == "invalid_end_user_plan" {
            continue;
        }
        // Serde refuses an unknown period before validation.
        assert_eq!(key, "bad-period", "{body}");
    }

    // Free is the default: unassigned users count towards it.
    let (status, body, _) = h
        .send(
            Method::PUT,
            &uri(org, app, "/end-user-settings"),
            Some(&owner),
            &write("settings-default"),
            Some(&json!({ "currency": "USD", "default_plan": "free" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (user, plan) in [
        ("u-pro", json!("pro")),
        ("u-a", Value::Null),
        ("u-b", Value::Null),
    ] {
        let (status, body, _) = h
            .send(
                Method::PUT,
                &uri(org, app, &format!("/end-users/{user}")),
                Some(&owner),
                &write(&format!("user-{user}")),
                Some(&json!({ "plan": plan })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    settled_request(&h.pool, org, app, "u-pro", 70).await;
    settled_request(&h.pool, org, app, "u-a", 5).await;
    settled_request(&h.pool, org, app, "u-b", 7).await;
    settled_request(&h.pool, org, app, "u-b", 1).await;
    let (status, body, _) = h.get(&uri(org, app, "/end-user-plans"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let totals: Vec<(String, u64, i64)> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plan| {
            (
                plan["name"].as_str().unwrap().to_owned(),
                plan["users"].as_u64().unwrap(),
                plan["spent_minor"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        totals,
        [("free".to_owned(), 2, 13), ("pro".to_owned(), 1, 70)]
    );
    // A user reports the period of their plan.
    let (_, body, _) = h.get(&uri(org, app, "/end-users/u-pro"), &owner).await;
    assert_eq!(body["period_start"], json!(start));
    assert_eq!(body["spent_minor"], 70);
}
