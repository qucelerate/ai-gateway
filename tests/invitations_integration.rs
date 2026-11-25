#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the invitation admin lane: list, create and revoke
//! an organization's invitations, driven through the real router, RBAC,
//! idempotency, versioning and audit, and accepted through the store calls
//! the sign-in flow makes.
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
    http::{Method, StatusCode, header::COOKIE},
};
use gateway::{
    OrganizationId, UserAccountId,
    application::invitations::InvitationLinks,
    config::OidcConfig,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    persistence::{AdminStore, AuthStore, IdentityStore, invitations::InvitationStore},
    transport::invitations::router,
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
const TEST_PEPPER: &[u8] = b"invitation-integration-pepper";
const HMAC_KEY: &[u8] = b"invitation-integration-hmac-key-0032";
const ORIGIN: &str = "https://gateway.invite.test";
const ISSUER: &str = "https://login.invite.test/v2.0";

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

fn oidc(origin: Option<&str>) -> OidcConfig {
    OidcConfig {
        enabled: true,
        origin: origin.map(str::to_owned),
        issuer: Some(ISSUER.to_owned()),
        audience: Some("invite-client".into()),
        client_id: Some("invite-client".into()),
        redirect_uri: Some(format!("{ORIGIN}/api/admin/v1/oidc/callback")),
        microsoft_graph_photo: false,
    }
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
    /// The same lane on a gateway without `GATEWAY_ORIGIN`.
    unlinked: Router,
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[23_u8; 32]).unwrap();
        let lane = |links: Option<InvitationLinks>| {
            router(
                AuthStore::new(pool.clone()),
                AdminStore::new(pool.clone()),
                Some(crypto.clone()),
                InvitationStore::new(pool.clone()),
                links,
            )
        };
        let links = InvitationLinks::from_config(&oidc(Some(ORIGIN))).unwrap();
        assert!(InvitationLinks::from_config(&oidc(None)).is_err());
        Self {
            router: lane(Some(links)),
            unlinked: lane(None),
            pool,
            identity,
            crypto,
        }
    }

    /// A new organization and its slug.
    async fn org(&self, name: &str) -> (OrganizationId, String) {
        let suffix = Uuid::new_v4().simple().to_string();
        let slug = format!("{name}-{}", &suffix[..12]);
        let org = self
            .identity
            .create_organization(&slug, name)
            .await
            .unwrap()
            .id;
        (org, slug)
    }

    /// A member of `org` holding the admin role `binding`, if any.
    async fn user(&self, org: OrganizationId, binding: Option<&str>) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, binding)
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, MembershipRole::Member)
            .await
            .unwrap();
        if let Some(role) = binding {
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
        }
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

    async fn send_to(
        router: &Router,
        method: Method,
        uri: &str,
        session: &(String, String),
        key: Option<&str>,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
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
        if let Some(key) = key {
            builder = builder.header("idempotency-key", key);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
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
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn list(&self, org: OrganizationId, session: &(String, String)) -> (StatusCode, Value) {
        Self::send_to(
            &self.router,
            Method::GET,
            &uri(org, ""),
            session,
            None,
            None,
        )
        .await
    }

    async fn invite(
        &self,
        org: OrganizationId,
        session: &(String, String),
        key: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        Self::send_to(
            &self.router,
            Method::POST,
            &uri(org, ""),
            session,
            Some(key),
            Some(&body),
        )
        .await
    }

    async fn revoke(
        &self,
        org: OrganizationId,
        session: &(String, String),
        id: &str,
        key: &str,
    ) -> (StatusCode, Value) {
        Self::send_to(
            &self.router,
            Method::POST,
            &uri(org, &format!("/{id}/revoke")),
            session,
            Some(key),
            None,
        )
        .await
    }
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/invitations{path}", org.0)
}

/// The token a link carries.
fn token_of(link: &str) -> String {
    let (base, token) = link.split_once("?token=").unwrap();
    assert_eq!(base, format!("{ORIGIN}/api/admin/v1/invitations/accept"));
    assert_eq!(token.len(), 43);
    token.to_owned()
}

async fn audit_actions(pool: &PgPool, org: OrganizationId) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT action, outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'invitation'
         ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn invitations_are_created_listed_and_revoked_and_the_link_is_shown_once() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("invites").await;
    let owner_id = harness.user(org, Some("organization_owner")).await;
    let owner = harness.session(owner_id).await;

    // Nothing yet; an owner may invite in every role.
    let (status, page) = harness.list(org, &owner).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["next_page_token"], Value::Null);
    assert_eq!(
        page["grantable_roles"],
        json!(["owner", "admin", "auditor", "member", "read-only"])
    );

    let (status, created) = harness
        .invite(org, &owner, "invite-1", json!({"role": "member"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let link = created["link"].as_str().unwrap().to_owned();
    let token = token_of(&link);
    let invitation = &created["invitation"];
    let id = invitation["id"].as_str().unwrap().to_owned();
    assert_eq!(invitation["role"], "member");
    assert_eq!(invitation["status"], "pending");
    assert_eq!(invitation["created_by"], owner_id.0.to_string());
    assert_eq!(invitation["accepted_at"], Value::Null);
    assert_eq!(created["replay"], false);
    assert_eq!(created["error"], Value::Null);
    // Valid for 72 hours by default.
    let at = |value: &Value| {
        value
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
    };
    assert_eq!(
        at(&invitation["expires_at"]) - at(&invitation["created_at"]),
        chrono::Duration::hours(72)
    );

    // A replay returns the invitation without its link.
    let (status, replay) = harness
        .invite(org, &owner, "invite-1", json!({"role": "member"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["invitation"], created["invitation"]);
    assert_eq!(replay["link"], Value::Null);
    assert_eq!(replay["replay"], true);
    assert_eq!(replay["error"], "secret_unavailable_on_replay");
    // The same key for another request conflicts.
    let (status, conflict) = harness
        .invite(org, &owner, "invite-1", json!({"role": "admin"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "idempotency_conflict");

    // Listed without the token or its digest.
    let (status, page) = harness.list(org, &owner).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0], created["invitation"]);
    let listed = page.to_string();
    assert!(!listed.contains(&token));
    let mut keys: Vec<&str> = page["items"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "accepted_at",
            "accepted_by",
            "created_at",
            "created_by",
            "expires_at",
            "id",
            "revoked_at",
            "role",
            "status"
        ]
    );

    // Revoked once; the link stops working.
    let (status, revoked) = harness.revoke(org, &owner, &id, "revoke-1").await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["status"], "revoked");
    assert!(revoked["revoked_at"].is_string());
    let store = InvitationStore::new(harness.pool.clone());
    assert_eq!(store.pending(&token).await.unwrap(), None);
    // A replay returns the recorded result; revoking again is a conflict.
    let (status, again) = harness.revoke(org, &owner, &id, "revoke-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, revoked);
    let (status, twice) = harness.revoke(org, &owner, &id, "revoke-2").await;
    assert_eq!(status, StatusCode::CONFLICT, "{twice}");
    assert_eq!(twice["code"], "invitation_not_pending");
    assert!(twice["request_id"].is_string());
    let (status, missing) = harness
        .revoke(org, &owner, &Uuid::new_v4().to_string(), "revoke-3")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");

    // An expired invitation is not pending either.
    let (_, expiring) = harness
        .invite(
            org,
            &owner,
            "invite-2",
            json!({"role": "auditor", "valid_hours": 1}),
        )
        .await;
    let expiring_id = expiring["invitation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        at(&expiring["invitation"]["expires_at"]) - at(&expiring["invitation"]["created_at"]),
        chrono::Duration::hours(1)
    );
    sqlx::query(
        "UPDATE gateway.organization_invitation
         SET created_at = created_at - INTERVAL '2 hours',
             expires_at = expires_at - INTERVAL '2 hours'
         WHERE id = $1",
    )
    .bind(Uuid::parse_str(&expiring_id).unwrap())
    .execute(&harness.pool)
    .await
    .unwrap();
    let (status, expired) = harness.revoke(org, &owner, &expiring_id, "revoke-4").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(expired["code"], "invitation_not_pending");

    // Bad input, and writes without an idempotency key.
    for (body, code) in [
        (json!({"role": "boss"}), "invalid_invitation"),
        (
            json!({"role": "member", "valid_hours": 0}),
            "invalid_invitation",
        ),
        (
            json!({"role": "member", "valid_hours": 721}),
            "invalid_invitation",
        ),
    ] {
        let (status, problem) = harness
            .invite(org, &owner, &Uuid::new_v4().to_string(), body.clone())
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(problem["code"], code, "{body}");
    }
    let (status, unknown_field) = harness
        .invite(
            org,
            &owner,
            "invite-x",
            json!({"role": "member", "token": "x"}),
        )
        .await;
    assert!(status.is_client_error(), "{unknown_field}");
    let (status, keyless) = Harness::send_to(
        &harness.router,
        Method::POST,
        &uri(org, ""),
        &owner,
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(keyless["code"], "idempotency_key_required");

    // Paged newest first.
    for n in 0..3 {
        let (status, _) = harness
            .invite(
                org,
                &owner,
                &format!("page-{n}"),
                json!({"role": "read-only"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let mut seen = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let query = match &next {
            Some(token) => format!("?page_size=2&page_token={token}"),
            None => "?page_size=2".to_owned(),
        };
        let (status, page) = Harness::send_to(
            &harness.router,
            Method::GET,
            &uri(org, &query),
            &owner,
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        seen.extend(items.iter().cloned());
        match page["next_page_token"].as_str() {
            Some(token) => next = Some(token.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    assert!(
        seen.windows(2)
            .all(|pair| at(&pair[0]["created_at"]) >= at(&pair[1]["created_at"]))
    );
    let mut ids: Vec<&str> = seen
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5);
    let statuses: Vec<&str> = seen
        .iter()
        .map(|item| item["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses.iter().filter(|s| **s == "pending").count(), 3);
    assert!(statuses.contains(&"revoked") && statuses.contains(&"expired"));

    // Audited in the same transaction: created five times, revoked once.
    let audited = audit_actions(&harness.pool, org).await;
    let count = |action: &str| {
        audited
            .iter()
            .filter(|(a, outcome)| a == action && outcome == "succeeded")
            .count()
    };
    assert_eq!(count("invitation.created"), 5);
    assert_eq!(count("invitation.revoked"), 1);

    // The token and the link are never stored: not in the audit log, not in
    // the idempotency record the replay was answered from.
    for table in ["audit_event", "admin_idempotency_record"] {
        let leaked: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM gateway.{table} t
             WHERE t.organization_id = $1 AND (strpos(t::text, $2) > 0 OR strpos(t::text, $3) > 0)"
        ))
        .bind(org.0)
        .bind(&token)
        .bind("invitations/accept")
        .fetch_one(&harness.pool)
        .await
        .unwrap();
        assert_eq!(leaked, 0, "{table}");
    }
    let recorded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_idempotency_record
         WHERE organization_id = $1 AND operation = 'invitation.create'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(recorded, 5);
}

#[tokio::test]
async fn callers_invite_only_at_or_below_their_rank_and_only_with_permission() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("invite-ranks").await;
    let admin_id = harness.user(org, Some("organization_admin")).await;
    let admin = harness.session(admin_id).await;
    let auditor = harness
        .session(harness.user(org, Some("auditor")).await)
        .await;
    let member = harness.session(harness.user(org, None).await).await;
    let operator = harness
        .session(harness.user(org, Some("operator")).await)
        .await;

    // An admin may invite anyone but an owner.
    let (status, page) = harness.list(org, &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        page["grantable_roles"],
        json!(["admin", "auditor", "member", "read-only"])
    );
    let (status, denied) = harness
        .invite(org, &admin, "owner-1", json!({"role": "owner"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(denied["code"], "escalation_denied");
    // Nothing was created, and the key is free for another request.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.organization_invitation WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(rows, 0);
    let (status, created) = harness
        .invite(org, &admin, "owner-1", json!({"role": "admin"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["invitation"]["id"].as_str().unwrap().to_owned();

    // An auditor reads but may not invite or revoke.
    let (status, page) = harness.list(org, &auditor).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["grantable_roles"], json!([]));
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let (status, denied) = harness
        .invite(org, &auditor, "a-1", json!({"role": "read-only"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "forbidden");
    let (status, _) = harness.revoke(org, &auditor, &id, "a-2").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A member, or an operator, neither reads nor invites.
    for session in [&member, &operator] {
        let (status, _) = harness.list(org, session).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, denied) = harness
            .invite(org, session, "m-1", json!({"role": "member"}))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        assert_eq!(denied["code"], "forbidden");
        let (status, _) = harness.revoke(org, session, &id, "m-2").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Denials are audited.
    assert!(
        audit_actions(&harness.pool, org)
            .await
            .iter()
            .any(|(action, outcome)| action == "invitation.created" && outcome == "denied")
    );
}

#[tokio::test]
async fn another_organizations_invitation_is_not_found() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org_a, _) = harness.org("invite-a").await;
    let (org_b, _) = harness.org("invite-b").await;
    let owner_a = harness
        .session(harness.user(org_a, Some("organization_owner")).await)
        .await;
    let owner_b = harness
        .session(harness.user(org_b, Some("organization_owner")).await)
        .await;
    let (_, created) = harness
        .invite(org_b, &owner_b, "b-1", json!({"role": "member"}))
        .await;
    let id = created["invitation"]["id"].as_str().unwrap().to_owned();

    // Through A's path, B's invitation does not exist.
    let (status, missing) = harness.revoke(org_a, &owner_a, &id, "a-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["code"], "not_found");
    let (_, page) = harness.list(org_a, &owner_a).await;
    assert_eq!(page["items"], json!([]));
    // And B's own paths refuse A's owner.
    let (status, _) = harness.list(org_b, &owner_a).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = harness.revoke(org_b, &owner_a, &id, "a-2").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, page) = harness.list(org_b, &owner_b).await;
    assert_eq!(page["items"][0]["status"], "pending");
}

#[tokio::test]
async fn an_invitation_made_through_the_api_is_accepted_by_the_sign_in_flow() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, slug) = harness.org("invite-accept").await;
    let owner = harness
        .session(harness.user(org, Some("organization_owner")).await)
        .await;
    let (status, created) = harness
        .invite(org, &owner, "accept-1", json!({"role": "admin"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = token_of(created["link"].as_str().unwrap());
    let id = Uuid::parse_str(created["invitation"]["id"].as_str().unwrap()).unwrap();

    // What `GET /api/admin/v1/invitations/accept` and the OIDC callback do.
    let store = InvitationStore::new(harness.pool.clone());
    assert_eq!(store.pending(&token).await.unwrap(), Some(id));
    let subject = format!("invitee-{}", Uuid::new_v4().simple());
    let invitee = store.accept(id, ISSUER, &subject).await.unwrap();
    let role: String = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND enabled",
    )
    .bind(org.0)
    .bind(invitee.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(role, "organization_admin");

    // Listed as accepted by them, and no longer revocable.
    let (_, page) = harness.list(org, &owner).await;
    let item = &page["items"][0];
    assert_eq!(item["status"], "accepted");
    assert_eq!(item["accepted_by"], invitee.0.to_string());
    let (status, conflict) = harness
        .revoke(org, &owner, &id.to_string(), "accept-2")
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "invitation_not_pending");
    // The invitee, now an admin, can invite in turn.
    let invitee_session = harness.session(invitee).await;
    let (status, _) = harness
        .invite(org, &invitee_session, "accept-3", json!({"role": "member"}))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // An invitation the operator made with the CLI is listed (created by
    // nobody) and revocable through the API, though it has no version yet.
    let (cli_id, _, cli_token) = store
        .create(
            &slug,
            None,
            MembershipRole::Member,
            ISSUER,
            chrono::Duration::hours(2),
            None,
        )
        .await
        .unwrap();
    let (_, page) = harness.list(org, &owner).await;
    assert_eq!(page["items"][0]["id"], cli_id.to_string());
    assert_eq!(page["items"][0]["created_by"], Value::Null);
    let (status, revoked) = harness
        .revoke(org, &owner, &cli_id.to_string(), "cli-1")
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["status"], "revoked");
    assert_eq!(store.pending(cli_token.as_str()).await.unwrap(), None);
    // The CLI sees the same invitations.
    assert_eq!(store.list(&slug).await.unwrap().len(), 3);
}

#[tokio::test]
async fn without_an_origin_invitations_are_listed_but_not_created() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("invite-unlinked").await;
    let owner = harness
        .session(harness.user(org, Some("organization_owner")).await)
        .await;
    let (status, problem) = Harness::send_to(
        &harness.unlinked,
        Method::POST,
        &uri(org, ""),
        &owner,
        Some("unlinked-1"),
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(problem["code"], "invitations_unavailable");
    let (status, page) = Harness::send_to(
        &harness.unlinked,
        Method::GET,
        &uri(org, ""),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
}
