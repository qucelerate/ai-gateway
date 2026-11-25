#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for local accounts, driven through the real router:
//! accepting an invitation with a password, password sign-in and its
//! limits, changing and resetting a password, an owner's reset link,
//! "Require SSO", turning local accounts off, and first-run setup.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite). First-run setup
//! needs an organization-free database of its own,
//! `GATEWAY_FIRST_RUN_TEST_DATABASE_URL`, which the test empties.

use std::{
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::Body,
    http::{Method, StatusCode, header},
    response::Response,
};
use gateway::{
    application::{AppState, oidc::HttpOidcBackend},
    config::{LocalAccountsConfig, OidcConfig},
    domain::{
        auth::AuthCrypto, email_domains::StaticTxt, identity::MembershipRole, keys::ApiKeyPepper,
        password::LOCAL_ISSUER,
    },
    jobs::egress::DeliveryPolicy,
    persistence::{invitations::InvitationStore, local_accounts::LocalAccountStore},
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

const TEST_PEPPER: &[u8] = b"local-accounts-integration-pepper";
const HMAC_KEY: &[u8] = b"local-accounts-integration-hmac-0032";
const ORIGIN: &str = "http://localhost:8080";
const PASSWORD: &str = "correct horse battery staple";

fn env_url(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(_)
            if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() =>
        {
            panic!("{name} must be set in CI")
        }
        Err(_) => None,
    }
}

async fn connect(database_url: &str) -> PgPool {
    let mut last_error = None;
    for _ in 0..15 {
        match PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(2))
            .connect(database_url)
            .await
        {
            Ok(pool) => {
                gateway::persistence::migrations::run(&pool)
                    .await
                    .unwrap_or_else(|error| panic!("test database migration failed: {error}"));
                return pool;
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(StdDuration::from_secs(1)).await;
            }
        }
    }
    panic!("test database remained unreachable: {last_error:?}");
}

/// A gateway without OIDC, on plain http, with local accounts as given.
fn router(pool: &PgPool, local: LocalAccountsConfig) -> Router {
    router_with_dns(pool, local, StaticTxt::shared())
}

/// As [`router`], with domain verification records answered from `txt`.
fn router_with_dns(pool: &PgPool, local: LocalAccountsConfig, txt: Arc<StaticTxt>) -> Router {
    let state = AppState::for_sign_in_tests(
        pool.clone(),
        ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap(),
        AuthCrypto::new(HMAC_KEY, &[29_u8; 32]).unwrap(),
        OidcConfig {
            enabled: false,
            origin: Some(ORIGIN.into()),
            issuer: None,
            audience: None,
            client_id: None,
            redirect_uri: None,
            microsoft_graph_photo: false,
        },
        Arc::new(HttpOidcBackend::insecure_for_tests(None).unwrap()),
        None,
        DeliveryPolicy::allow_private_targets_for_tests(),
    )
    .with_local_accounts(local)
    .with_txt_lookup(txt);
    gateway::transport::http::router(state)
}

fn local_on() -> LocalAccountsConfig {
    LocalAccountsConfig {
        enabled: true,
        trust_forwarded_for: true,
        self_serve_organizations: false,
    }
}

/// A browser's cookies: the jar a response's `Set-Cookie`s fill.
#[derive(Clone, Default)]
struct Browser {
    cookies: Vec<(String, String)>,
    /// The client address the proxy reports.
    address: String,
}

impl Browser {
    fn at(address: &str) -> Self {
        Self {
            cookies: Vec::new(),
            address: address.into(),
        }
    }

    fn keep(&mut self, response: &Response) {
        for value in response.headers().get_all(header::SET_COOKIE) {
            let value = value.to_str().unwrap();
            let pair = value.split(';').next().unwrap();
            let (name, content) = pair.split_once('=').unwrap();
            self.cookies.retain(|(existing, _)| existing != name);
            if !content.is_empty() {
                self.cookies.push((name.into(), content.into()));
            }
        }
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
    }

    async fn send(
        &mut self,
        router: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, Response) {
        self.send_from(router, method, path, body, Some(ORIGIN))
            .await
    }

    async fn send_from(
        &mut self,
        router: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
        origin: Option<&str>,
    ) -> (StatusCode, Value, Response) {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(
                "x-forwarded-for",
                format!("203.0.113.250, {}", self.address),
            );
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if !self.cookies.is_empty() {
            let cookies = self
                .cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            request = request.header(header::COOKIE, cookies);
        }
        if let Some(csrf) = self.cookie("__Host-gateway_csrf") {
            request = request.header("x-csrf-token", csrf.to_owned());
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        self.keep(&response);
        let status = response.status();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, Response::from_parts(parts, Body::empty()))
    }
}

fn email(name: &str) -> String {
    format!(
        "{name}-{}@example.com",
        &Uuid::new_v4().simple().to_string()[..10]
    )
}

/// A new organization, and an invitation link token to it.
async fn invitation(pool: &PgPool, slug: &str, role: MembershipRole) -> String {
    let (_, _, token) = InvitationStore::new(pool.clone())
        .create(
            slug,
            Some("Acme"),
            role,
            LOCAL_ISSUER,
            chrono::Duration::hours(1),
            None,
        )
        .await
        .unwrap();
    token.as_str().to_owned()
}

/// Accepts the invitation as a new local account; the browser is then
/// signed in.
async fn accept(router: &Router, browser: &mut Browser, token: &str, email: &str) -> Value {
    let (status, body, _) = browser
        .send(
            router,
            Method::POST,
            "/api/admin/v1/invitations/accept-password",
            Some(
                json!({ "token": token, "email": email, "password": PASSWORD,
                         "display_name": "Ada Lovelace" }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn organization_id(pool: &PgPool, slug: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM gateway.organization WHERE slug = $1")
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn slug(name: &str) -> String {
    format!("{name}-{}", &Uuid::new_v4().simple().to_string()[..12])
}

#[tokio::test]
async fn invitations_sign_in_and_passwords() {
    let Some(url) = env_url("GATEWAY_TEST_DATABASE_URL") else {
        return;
    };
    let _guard = database_guard();
    let pool = connect(&url).await;
    let router = router(&pool, local_on());
    let acme = slug("acme");
    let owner_email = email("owner");
    let token = invitation(&pool, &acme, MembershipRole::Owner).await;
    let mut owner = Browser::at("198.51.100.1");

    // The link opens the admin UI's page, the token in the fragment.
    let (status, _, response) = owner
        .send(
            &router,
            Method::GET,
            &format!("/api/admin/v1/invitations/accept?token={token}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("/admin/accept-invitation#token={token}").as_str()
    );
    let (status, offer, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/inspect",
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        offer,
        json!({ "organization_name": "Acme", "role": "owner", "password": true, "sso": false })
    );

    // Another site cannot accept for the visitor; a weak password is refused.
    let (status, body, _) = owner
        .send_from(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/accept-password",
            Some(json!({ "token": token, "email": owner_email, "password": PASSWORD })),
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/accept-password",
            Some(json!({ "token": token, "email": owner_email, "password": "Password2024!!" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "password_too_common");

    accept(&router, &mut owner, &token, &owner_email).await;
    let (status, me, _) = owner
        .send(&router, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["sign_in_method"], "password");
    assert_eq!(me["deployment_admin"], false);
    assert_eq!(me["display_name"], "Ada Lovelace");
    // The session's end, so the console can warn before it: in the future,
    // within the configured lifetime.
    let now = chrono::Utc::now().timestamp();
    let expires_at = me["session_expires_at"].as_i64().unwrap();
    assert!(expires_at > now && expires_at <= now + 86_400, "{me}");
    let owner_id: Uuid = me["user_id"].as_str().unwrap().parse().unwrap();
    let (_, organizations, _) = owner
        .send(&router, Method::GET, "/api/admin/v1/organizations", None)
        .await;
    assert!(
        organizations
            .as_array()
            .unwrap()
            .iter()
            .any(|organization| organization["slug"] == acme.as_str())
    );
    // The account is a user like any other: (local issuer, its own id).
    let (issuer, subject): (String, String) =
        sqlx::query_as("SELECT oidc_issuer, oidc_subject FROM gateway.user_account WHERE id = $1")
            .bind(owner_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (issuer.as_str(), subject),
        (LOCAL_ISSUER, owner_id.to_string())
    );

    // A used invitation is gone.
    let (status, _, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/inspect",
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Signing in: wrong passwords lock the account after five.
    let mut stranger = Browser::at("198.51.100.2");
    for attempt in 0..5 {
        let (status, body, _) = stranger
            .send(
                &router,
                Method::POST,
                "/api/admin/v1/password/sign-in",
                Some(json!({ "email": owner_email, "password": "wrong password here" })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}: {body}"
        );
        assert_eq!(body["code"], "invalid_credentials");
    }
    let (status, body, response) = stranger
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": owner_email, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    sqlx::query("UPDATE gateway.local_credential SET locked_until = NULL WHERE user_id = $1")
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    // An unknown address answers as a wrong password does.
    let (status, body, _) = stranger
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": email("nobody"), "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("invalid_credentials"))
    );
    let mut second = Browser::at("198.51.100.3");
    let (status, body, _) = second
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": owner_email.to_uppercase(), "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let failures: i32 = sqlx::query_scalar(
        "SELECT failed_attempts FROM gateway.local_credential WHERE user_id = $1",
    )
    .bind(owner_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failures, 0, "a successful sign-in clears the count");

    // Changing the password ends the account's other sessions.
    let (status, body, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/me/password",
            Some(json!({ "current_password": "not it at all", "new_password": "a brand new passphrase" })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("current_password_incorrect"))
    );
    let (status, _, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/me/password",
            Some(json!({ "current_password": PASSWORD, "new_password": "a brand new passphrase" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = owner
        .send(&router, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(status, StatusCode::OK, "the session that changed it stays");
    let (status, _, _) = second
        .send(&router, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "other sessions end");

    // An owner's reset link for a member.
    let member_email = email("member");
    let member_token = invitation(&pool, &acme, MembershipRole::Member).await;
    let mut member = Browser::at("198.51.100.4");
    let member_id: Uuid =
        accept(&router, &mut member, &member_token, &member_email).await["user_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
    let acme_id = organization_id(&pool, &acme).await;
    let reset_path =
        format!("/api/admin/v1/organizations/{acme_id}/members/{member_id}/password-reset-link");
    // A member may not; the owner may.
    let (status, _, _) = member
        .send(
            &router,
            Method::POST,
            &format!(
                "/api/admin/v1/organizations/{acme_id}/members/{owner_id}/password-reset-link"
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, link, _) = owner.send(&router, Method::POST, &reset_path, None).await;
    assert_eq!(status, StatusCode::CREATED, "{link}");
    let link = link["link"].as_str().unwrap();
    let reset_token = link
        .strip_prefix(&format!("{ORIGIN}/admin/reset-password#token="))
        .unwrap()
        .to_owned();
    let mut anyone = Browser::at("198.51.100.5");
    let (_, check, _) = anyone
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset/check",
            Some(json!({ "token": reset_token })),
        )
        .await;
    assert_eq!(check, json!({ "valid": true, "email": member_email }));
    let (status, body, _) = anyone
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset",
            Some(json!({ "token": reset_token, "password": format!("{member_email}!!") })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!("password_contains_email")
        )
    );
    let (status, _, _) = anyone
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset",
            Some(json!({ "token": reset_token, "password": "the member's new passphrase" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = member
        .send(&router, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a reset ends every session"
    );
    let (status, _, _) = anyone
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset",
            Some(json!({ "token": reset_token, "password": "yet another passphrase" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a link works once");
    let (status, _, _) = member
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": member_email, "password": "the member's new passphrase" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'membership.password_reset_link_created'
           AND entity_id = $2",
    )
    .bind(acme_id)
    .bind(member_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // A password change ends the links made before it.
    let token_of = |link: &Value| {
        link["link"]
            .as_str()
            .unwrap()
            .strip_prefix(&format!("{ORIGIN}/admin/reset-password#token="))
            .unwrap()
            .to_owned()
    };
    let (_, before_change, _) = owner.send(&router, Method::POST, &reset_path, None).await;
    for (current, new) in [
        (
            "the member's new passphrase",
            "the member's newer passphrase",
        ),
        (
            "the member's newer passphrase",
            "the member's new passphrase",
        ),
    ] {
        let (status, body, _) = member
            .send(
                &router,
                Method::POST,
                "/api/admin/v1/me/password",
                Some(json!({ "current_password": current, "new_password": new })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let (status, _, _) = anyone
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset",
            Some(json!({ "token": token_of(&before_change), "password": "an attacker's passphrase" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a change ends earlier links");
    // An owner's link stops working once its account is out of the owner's
    // reach (below: it joins another organization).
    let (_, before_joining, _) = owner.send(&router, Method::POST, &reset_path, None).await;

    // A member who also belongs elsewhere is the operator's to reset.
    let other = slug("other");
    let other_token = invitation(&pool, &other, MembershipRole::Member).await;
    let mut member_elsewhere = Browser::at("198.51.100.6");
    let (status, _, _) = member_elsewhere
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/accept-password",
            Some(json!({ "token": other_token, "email": member_email,
                         "password": "the member's new passphrase" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an existing account accepts with its password"
    );
    let (status, body, _) = owner.send(&router, Method::POST, &reset_path, None).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("reset_requires_operator"))
    );
    let (status, _, _) = anyone
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset",
            Some(json!({ "token": token_of(&before_joining), "password": "an attacker's passphrase" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the owner's link no longer reaches it"
    );

    // Require SSO: only with an enabled provider; then password sessions
    // cannot act in the organization.
    let policy = format!("/api/admin/v1/organizations/{acme_id}/sign-in-policy");
    let (status, body, _) = owner
        .send(
            &router,
            Method::PUT,
            &policy,
            Some(json!({ "require_sso": true })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("identity_provider_required"))
    );
    enable_provider(&pool, acme_id).await;
    let (status, body, _) = owner
        .send(
            &router,
            Method::PUT,
            &policy,
            Some(json!({ "require_sso": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["passwords_allowed"], false);
    // The router refuses it before any route: the shape of its refusals.
    let (status, body, _) = owner.send(&router, Method::GET, &policy, None).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("sso_required"))
    );
    // Percent-encoding the id does not get around it.
    let id = acme_id.to_string();
    let encoded = policy.replacen(&id, &format!("%{:02X}{}", id.as_bytes()[0], &id[1..]), 1);
    assert_ne!(encoded, policy);
    let (status, body, _) = owner.send(&router, Method::GET, &encoded, None).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found"))
    );
    let (_, organizations, _) = owner
        .send(&router, Method::GET, "/api/admin/v1/organizations", None)
        .await;
    assert!(
        organizations
            .as_array()
            .unwrap()
            .iter()
            .all(|organization| organization["slug"] != acme.as_str())
    );
    let (status, body, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/context",
            Some(json!({ "organization_id": acme_id })),
        )
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("sso_required"))
    );
    let mut fresh = Browser::at("198.51.100.7");
    let (status, body, _) = fresh
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": owner_email, "password": "a brand new passphrase" })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("sso_required"))
    );
    // An invitation there now needs the provider.
    let late = invitation(&pool, &acme, MembershipRole::Member).await;
    let (_, offer, _) = fresh
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/inspect",
            Some(json!({ "token": late })),
        )
        .await;
    assert_eq!(offer["password"], false);
    // The member still works in the other organization.
    let (_, organizations, _) = member_elsewhere
        .send(&router, Method::GET, "/api/admin/v1/organizations", None)
        .await;
    assert_eq!(organizations.as_array().unwrap().len(), 1);

    // With local accounts off, password sessions no longer count.
    let off = gateway_off(&pool);
    let (status, _, _) = member_elsewhere
        .send(&off, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body, _) = fresh
        .send(
            &off,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": member_email, "password": "the member's new passphrase" })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("local_accounts_disabled"))
    );
    let (_, options, _) = fresh
        .send(&off, Method::GET, "/api/admin/v1/sign-in/options", None)
        .await;
    assert_eq!(
        options,
        json!({ "password": false, "oidc": false, "setup_required": false })
    );
}

fn gateway_off(pool: &PgPool) -> Router {
    router(
        pool,
        LocalAccountsConfig {
            enabled: false,
            trust_forwarded_for: true,
            self_serve_organizations: false,
        },
    )
}

/// An enabled identity provider for the organization, as a tested
/// configuration leaves it.
async fn enable_provider(pool: &PgPool, organization_id: Uuid) {
    let configuration = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.identity_provider_configuration
             (id, organization_id, version, issuer, client_id, secret_key_id, secret_ciphertext)
         VALUES ($1, $2, 1, 'https://login.acme.example', 'client', 'key', $3)",
    )
    .bind(configuration)
    .bind(organization_id)
    .bind(vec![7_u8; 32])
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.organization_identity_provider
             (organization_id, id, enabled, active_configuration_id)
         VALUES ($1, $2, true, $3)",
    )
    .bind(organization_id)
    .bind(Uuid::new_v4())
    .bind(configuration)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_client_address_is_limited_across_accounts() {
    let Some(url) = env_url("GATEWAY_TEST_DATABASE_URL") else {
        return;
    };
    let _guard = database_guard();
    let pool = connect(&url).await;
    let router = router(&pool, local_on());
    let mut attacker = Browser::at("192.0.2.99");
    for attempt in 0..20 {
        let (status, _, _) = attacker
            .send(
                &router,
                Method::POST,
                "/api/admin/v1/password/sign-in",
                Some(json!({ "email": email(&format!("guess{attempt}")), "password": PASSWORD })),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, body, _) = attacker
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": email("guess"), "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::TOO_MANY_REQUESTS, json!("too_many_attempts"))
    );
    // Another address is not.
    let (status, _, _) = Browser::at("192.0.2.100")
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": email("guess"), "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn first_run_setup_creates_the_first_organization_and_its_owner() {
    let Some(url) = env_url("GATEWAY_FIRST_RUN_TEST_DATABASE_URL") else {
        return;
    };
    let _guard = database_guard();
    let pool = connect(&url).await;
    // A database of its own, emptied: setup is for a fresh install.
    sqlx::query(
        "TRUNCATE gateway.organization, gateway.user_account, gateway.password_link CASCADE",
    )
    .execute(&pool)
    .await
    .unwrap();
    let router = router(&pool, local_on());
    let store = LocalAccountStore::new(pool.clone());
    let mut operator = Browser::at("198.51.100.20");

    let (_, options, _) = operator
        .send(&router, Method::GET, "/api/admin/v1/sign-in/options", None)
        .await;
    assert_eq!(
        options,
        json!({ "password": true, "oidc": false, "setup_required": true })
    );
    let first = store
        .create_setup_link(chrono::Duration::hours(24))
        .await
        .unwrap()
        .unwrap();
    let second = store
        .create_setup_link(chrono::Duration::hours(24))
        .await
        .unwrap()
        .unwrap();
    let (_, check, _) = operator
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/setup/check",
            Some(json!({ "token": first.as_str() })),
        )
        .await;
    assert_eq!(check, json!({ "valid": true }));
    let form = |token: &str| {
        json!({
            "token": token,
            "organization_name": "Acme Labs",
            "organization_slug": "acme-labs",
            "email": "Founder@Acme.example",
            "display_name": "Grace Hopper",
            "password": PASSWORD,
        })
    };
    let (status, body, _) = operator
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/setup",
            Some(form("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("setup_link_invalid"))
    );
    let (status, body, _) = operator
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/setup",
            Some(form(second.as_str())),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, me, _) = operator
        .send(&router, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(me["deployment_admin"], true);
    assert_eq!(me["sign_in_method"], "password");
    assert_eq!(me["email"], "founder@acme.example");
    let (_, organizations, _) = operator
        .send(&router, Method::GET, "/api/admin/v1/organizations", None)
        .await;
    assert_eq!(organizations[0]["slug"], "acme-labs");
    let role: String =
        sqlx::query_scalar("SELECT role FROM gateway.organization_membership WHERE user_id = $1")
            .bind(me["user_id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(role, "owner");

    // Done: the other link, and new ones, no longer work.
    let (status, body, _) = Browser::at("198.51.100.21")
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/setup",
            Some(form(first.as_str())),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("setup_already_done"))
    );
    assert!(
        store
            .create_setup_link(chrono::Duration::hours(24))
            .await
            .unwrap()
            .is_none()
    );
    let (_, options, _) = operator
        .send(&router, Method::GET, "/api/admin/v1/sign-in/options", None)
        .await;
    assert_eq!(options["setup_required"], false);
    // The owner signs in again with the password.
    let (status, _, _) = Browser::at("198.51.100.22")
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": "founder@acme.example", "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The setup account administers the deployment; its only organization
    // cannot be deleted.
    let organization = organizations[0]["id"].as_str().unwrap().to_owned();
    let lifecycle =
        |action: &str| format!("/api/admin/v1/deployment/organizations/{organization}/{action}");
    let (status, body, _) = operator
        .send(&router, Method::POST, &lifecycle("suspend"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // With its only organization suspended, the administrator still signs
    // in, and can lift the suspension.
    let mut returning = Browser::at("198.51.100.23");
    let (status, body, _) = returning
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": "founder@acme.example", "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = returning
        .send(
            &router,
            Method::GET,
            "/api/admin/v1/deployment/organizations",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = operator
        .send(
            &router,
            Method::POST,
            &lifecycle("delete"),
            Some(json!({ "confirm_slug": "acme-labs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "last_organization");
    let (status, _, _) = operator
        .send(&router, Method::POST, &lifecycle("unsuspend"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, audit, _) = operator
        .send(&router, Method::GET, "/api/admin/v1/deployment/audit", None)
        .await;
    let actions: Vec<&str> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        [
            "organization.unsuspended",
            "organization.suspended",
            "setup.completed"
        ]
    );
}

/// Reset links made, used and outrun by password changes at once never
/// deadlock: every path locks the account, its credential, then its links.
#[tokio::test]
async fn concurrent_password_writes_keep_one_lock_order() {
    let Some(database_url) = env_url("GATEWAY_TEST_DATABASE_URL") else {
        return;
    };
    let _guard = database_guard();
    let pool = connect(&database_url).await;
    let router = router(&pool, local_on());
    let acme = slug("lock-order");
    let token = invitation(&pool, &acme, MembershipRole::Member).await;
    let address = email("lock-order");
    let mut browser = Browser::at("198.51.100.91");
    let user: Uuid = accept(&router, &mut browser, &token, &address).await["user_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let user = gateway::domain::identity::UserAccountId(user);
    let store = LocalAccountStore::new(pool.clone());
    for round in 0..20 {
        let link = store
            .create_reset_link(user, chrono::Duration::hours(1), None)
            .await
            .unwrap();
        let current = store
            .credential(&address)
            .await
            .unwrap()
            .unwrap()
            .password_hash;
        let (by_reset, by_change) = (
            gateway::domain::password::hash_password(&format!("reset {round} passphrase")).unwrap(),
            gateway::domain::password::hash_password(&format!("changed {round} passphrase"))
                .unwrap(),
        );
        let (reset, changed, made) = tokio::join!(
            store.reset_with_link(link.as_str(), &by_reset),
            store.change_password(
                user,
                &current,
                &by_change,
                None,
                gateway::domain::auth::SecurityEvent::PasswordChanged,
            ),
            store.create_reset_link(user, chrono::Duration::hours(1), None),
        );
        let (reset, changed) = (reset.unwrap(), changed.unwrap());
        made.unwrap();
        // Whichever locks the account first wins: a change ends the link,
        // and a reset leaves the change's expected password stale.
        assert!(!(reset && changed), "round {round}: both applied");
    }
}

/// Owners claim email domains with a DNS TXT record; the first to verify
/// owns the domain, removal frees it, and sign-in by email asks for the
/// password here (no organization provider without OIDC).
#[tokio::test]
async fn domains_are_verified_by_dns_and_route_email_sign_in() {
    let Some(url) = env_url("GATEWAY_TEST_DATABASE_URL") else {
        return;
    };
    let _guard = database_guard();
    let pool = connect(&url).await;
    let txt = StaticTxt::shared();
    let router = router_with_dns(&pool, local_on(), txt.clone());
    let acme = slug("acme-domains");
    let rival = slug("rival-domains");
    let mut owner = Browser::at("198.51.100.31");
    let mut other = Browser::at("198.51.100.32");
    let mut member = Browser::at("198.51.100.33");
    let acme_token = invitation(&pool, &acme, MembershipRole::Owner).await;
    accept(&router, &mut owner, &acme_token, &email("domain-owner")).await;
    let rival_token = invitation(&pool, &rival, MembershipRole::Owner).await;
    accept(&router, &mut other, &rival_token, &email("rival-owner")).await;
    let member_token = invitation(&pool, &acme, MembershipRole::Admin).await;
    accept(&router, &mut member, &member_token, &email("domain-admin")).await;
    let acme_id = organization_id(&pool, &acme).await;
    let rival_id = organization_id(&pool, &rival).await;
    let domains = |org: Uuid| format!("/api/admin/v1/organizations/{org}/domains");
    let domain = format!("{}.example", &Uuid::new_v4().simple().to_string()[..12]);

    // Only an owner adds one; the domain is normalized and pending.
    let (status, body, _) = member
        .send(
            &router,
            Method::POST,
            &domains(acme_id),
            Some(json!({ "domain": domain })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body, _) = owner
        .send(
            &router,
            Method::POST,
            &domains(acme_id),
            Some(json!({ "domain": "localhost" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, added, _) = owner
        .send(
            &router,
            Method::POST,
            &domains(acme_id),
            Some(json!({ "domain": format!("{}.", domain.to_uppercase()) })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["domain"], domain.as_str());
    assert_eq!(added["status"], "pending");
    assert_eq!(added["record"]["type"], "TXT");
    let name = format!("_gateway-verification.{domain}");
    assert_eq!(added["record"]["name"], name.as_str());
    let value = added["record"]["value"].as_str().unwrap().to_owned();
    assert!(value.starts_with("gateway-domain-verification="));
    assert!(added.get("verification_token").is_none());
    let (status, body, _) = owner
        .send(
            &router,
            Method::POST,
            &domains(acme_id),
            Some(json!({ "domain": domain })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "domain_exists");
    let acme_domain = added["id"].as_str().unwrap().to_owned();

    // The rival lists it too; neither has published the record yet.
    let (status, rival_added, _) = other
        .send(
            &router,
            Method::POST,
            &domains(rival_id),
            Some(json!({ "domain": domain })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{rival_added}");
    assert_ne!(rival_added["record"]["value"], value.as_str());
    let verify =
        |org: Uuid, id: &str| format!("/api/admin/v1/organizations/{org}/domains/{id}/verify");
    let (status, body, _) = owner
        .send(&router, Method::POST, &verify(acme_id, &acme_domain), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pending");
    assert!(body["last_checked_at"].is_string());

    // Acme publishes its record and verifies; the rival's record (had it
    // one) cannot then win the domain.
    txt.publish(&name, "v=spf1 -all");
    txt.publish(&name, &value);
    let (status, body, _) = owner
        .send(&router, Method::POST, &verify(acme_id, &acme_domain), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");
    txt.publish(&name, rival_added["record"]["value"].as_str().unwrap());
    let rival_domain = rival_added["id"].as_str().unwrap().to_owned();
    let (status, body, _) = other
        .send(
            &router,
            Method::POST,
            &verify(rival_id, &rival_domain),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "domain_claimed");
    let (status, body, _) = member
        .send(&router, Method::GET, &domains(acme_id), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["status"], "verified");

    // Sign-in by email: the password step, wherever the domain points. The
    // organization's provider cannot be started here (this gateway keeps no
    // credential key to open its secret), so a password still works rather
    // than the sign-in failing.
    enable_provider(&pool, acme_id).await;
    let mut visitor = Browser::at("198.51.100.34");
    for address in [format!("ada@{domain}"), "ada@unclaimed.example".to_owned()] {
        let (status, body, _) = visitor
            .send(
                &router,
                Method::POST,
                "/api/admin/v1/sign-in/start",
                Some(json!({ "email": address })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({ "next": "password" }));
    }
    // Where passwords are not allowed, the failure is the answer.
    sqlx::query("UPDATE gateway.organization SET require_sso = true WHERE id = $1")
        .bind(acme_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body, _) = visitor
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/sign-in/start",
            Some(json!({ "email": format!("ada@{domain}") })),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    sqlx::query("UPDATE gateway.organization SET require_sso = false WHERE id = $1")
        .bind(acme_id)
        .execute(&pool)
        .await
        .unwrap();

    // Removed: kept but hidden, and the rival may now verify it.
    let (status, _, _) = owner
        .send(
            &router,
            Method::DELETE,
            &format!("/api/admin/v1/organizations/{acme_id}/domains/{acme_domain}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body, _) = owner
        .send(&router, Method::GET, &domains(acme_id), None)
        .await;
    assert_eq!(body["items"], json!([]));
    let kept: bool = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM gateway.organization_domain WHERE id = $1::uuid",
    )
    .bind(&acme_domain)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(kept);
    let (status, body, _) = other
        .send(
            &router,
            Method::POST,
            &verify(rival_id, &rival_domain),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'organization_domain' ORDER BY occurred_at, action",
    )
    .bind(acme_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        actions,
        [
            "organization_domain.added",
            "organization_domain.verified",
            "organization_domain.removed"
        ]
    );

    // Neither local accounts nor OIDC: nowhere to go.
    let (status, body, _) = Browser::at("198.51.100.35")
        .send(
            &gateway_off(&pool),
            Method::POST,
            "/api/admin/v1/sign-in/start",
            Some(json!({ "email": "ada@example.com" })),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "sign_in_unavailable");
}

/// A deployment admin creates organizations (with a first owner by
/// invitation or local account), suspends one (every way in refused),
/// deletes it (secrets kept, slug freed, hidden) and restores it whole.
#[tokio::test]
async fn deployment_admins_suspend_delete_and_restore_organizations() {
    let Some(url) = env_url("GATEWAY_TEST_DATABASE_URL") else {
        return;
    };
    let _guard = database_guard();
    let pool = connect(&url).await;
    let router = router(&pool, local_on());
    let home = slug("deploy-home");
    let admin_email = email("deploy-admin");
    let mut admin = Browser::at("198.51.100.41");
    let token = invitation(&pool, &home, MembershipRole::Owner).await;
    accept(&router, &mut admin, &token, &admin_email).await;
    let mut outsider = Browser::at("198.51.100.42");
    let token = invitation(&pool, &slug("deploy-outsider"), MembershipRole::Owner).await;
    accept(&router, &mut outsider, &token, &email("deploy-outsider")).await;

    // Only deployment admins.
    let (status, body, _) = outsider
        .send(
            &router,
            Method::GET,
            "/api/admin/v1/deployment/organizations",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "deployment_admin_required");
    LocalAccountStore::new(pool.clone())
        .grant_deployment_admin(gateway::persistence::local_accounts::AdminAccount::Email(
            &admin_email,
        ))
        .await
        .unwrap()
        .unwrap();
    let (_, me, _) = admin
        .send(&router, Method::GET, "/api/admin/v1/me", None)
        .await;
    assert_eq!(me["deployment_admin"], true);

    // A new organization whose first owner gets a local account and a
    // set-password link.
    let target = slug("deploy-target");
    let owner_email = email("deploy-owner");
    let (status, created, _) = admin
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/deployment/organizations",
            Some(json!({ "name": "Target", "slug": target,
                         "owner": { "email": owner_email, "method": "local_account" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["organization"]["status"], "active");
    assert_eq!(created["organization"]["members"], 1);
    assert_eq!(created["owner_link"]["kind"], "set_password");
    let link = created["owner_link"]["link"].as_str().unwrap();
    let reset = link.split("#token=").nth(1).unwrap().to_owned();
    let mut owner = Browser::at("198.51.100.43");
    let (status, _, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/reset",
            Some(json!({ "token": reset, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/password/sign-in",
            Some(json!({ "email": owner_email, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let target_id = organization_id(&pool, &target).await;
    let policy = format!("/api/admin/v1/organizations/{target_id}/sign-in-policy");
    let (status, _, _) = owner.send(&router, Method::GET, &policy, None).await;
    assert_eq!(status, StatusCode::OK);
    // The admin is not a member: it sees no contents.
    let (status, _, _) = admin.send(&router, Method::GET, &policy, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A second organization by invitation; the slug must be free.
    let (status, body, _) = admin
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/deployment/organizations",
            Some(json!({ "name": "Clash", "slug": target })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "slug_taken");
    let (status, invited, _) = admin
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/deployment/organizations",
            Some(json!({ "name": "Invited", "slug": slug("deploy-invited"),
                           "owner": { "email": email("invitee"), "method": "invitation" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{invited}");
    assert_eq!(invited["owner_link"]["kind"], "invitation");
    assert!(
        invited["owner_link"]["link"]
            .as_str()
            .unwrap()
            .contains("/api/admin/v1/invitations/accept?token=")
    );

    // What suspension must refuse: an API key, an invitation, a session.
    let identity = gateway::persistence::IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap(),
    );
    let owner_id = gateway::UserAccountId(me_id(&router, &mut owner).await);
    let account = identity
        .create_service_account(
            gateway::OrganizationId(target_id),
            owner_id,
            None,
            "suspended-bot",
        )
        .await
        .unwrap()
        .id;
    let bearer = identity
        .create_api_key(
            gateway::OrganizationId(target_id),
            account,
            &[gateway::domain::identity::ApiKeyScope::Inference],
            None,
            owner_id,
        )
        .await
        .unwrap()
        .bearer;
    assert!(
        identity
            .authenticate_bearer_key(&bearer)
            .await
            .unwrap()
            .is_some()
    );
    let pending = invitation(&pool, &target, MembershipRole::Member).await;
    sqlx::query(
        "INSERT INTO gateway.provider_credential
             (id, organization_id, adapter_instance_key, family, key_id, ciphertext)
         VALUES ($1, $2, 'openai-main', 'openai', '0123456789abcdef', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(target_id)
    .bind(vec![9_u8; 40])
    .execute(&pool)
    .await
    .unwrap();
    enable_provider(&pool, target_id).await;

    let lifecycle =
        |action: &str| format!("/api/admin/v1/deployment/organizations/{target_id}/{action}");
    // Delete needs a suspension first.
    let (status, body, _) = admin
        .send(
            &router,
            Method::POST,
            &lifecycle("delete"),
            Some(json!({ "confirm_slug": target })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("organization_active"))
    );
    // The owner asks for deletion; the admin sees it.
    let deletion = format!("/api/admin/v1/organizations/{target_id}/deletion-request");
    let (status, body, _) = owner.send(&router, Method::PUT, &deletion, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["requested_at"].is_string());

    let (status, body, _) = admin
        .send(&router, Method::POST, &lifecycle("suspend"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "suspended");
    assert!(body["deletion_requested_at"].is_string());
    assert!(
        identity
            .authenticate_bearer_key(&bearer)
            .await
            .unwrap()
            .is_none()
    );
    let (status, body, _) = owner.send(&router, Method::GET, &policy, None).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("organization_suspended"))
    );
    // Only its members learn it is suspended.
    let (status, body, _) = admin.send(&router, Method::GET, &policy, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_ne!(body["error"], "organization_suspended");
    // Nor can a session switch into it.
    let (status, body, _) = owner
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/context",
            Some(json!({ "organization_id": target_id })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (_, organizations, _) = owner
        .send(&router, Method::GET, "/api/admin/v1/organizations", None)
        .await;
    assert!(
        organizations
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["id"] != target_id.to_string())
    );
    let (status, _, _) = Browser::at("198.51.100.44")
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/invitations/inspect",
            Some(json!({ "token": pending })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let active_provider: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gateway.organization_identity_provider p
         JOIN gateway.organization o ON o.id = p.organization_id AND o.suspended_at IS NULL
         WHERE p.organization_id = $1 AND p.enabled)",
    )
    .bind(target_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!active_provider);

    // Unsuspended, the key works again; then suspend and delete for real.
    admin
        .send(&router, Method::POST, &lifecycle("unsuspend"), None)
        .await;
    assert!(
        identity
            .authenticate_bearer_key(&bearer)
            .await
            .unwrap()
            .is_some()
    );
    admin
        .send(&router, Method::POST, &lifecycle("suspend"), None)
        .await;
    let (status, body, _) = admin
        .send(
            &router,
            Method::POST,
            &lifecycle("delete"),
            Some(json!({ "confirm_slug": "wrong" })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!("confirmation_mismatch")
        )
    );
    let (status, body, _) = admin
        .send(
            &router,
            Method::POST,
            &lifecycle("delete"),
            Some(json!({ "confirm_slug": target })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "deleted");

    // Secrets kept as they were, sealed; nothing inside switched off;
    // hidden; slug free.
    let credential: Vec<u8> = sqlx::query_scalar(
        "SELECT ciphertext FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(target_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(credential, vec![9_u8; 40]);
    let (idp_secret, idp_enabled): (Vec<u8>, bool) = sqlx::query_as(
        "SELECT c.secret_ciphertext, p.enabled
         FROM gateway.organization_identity_provider p
         JOIN gateway.identity_provider_configuration c
           ON c.organization_id = p.organization_id AND c.id = p.active_configuration_id
         WHERE p.organization_id = $1",
    )
    .bind(target_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((idp_secret, idp_enabled), (vec![7_u8; 32], true));
    let stored = json!({
        "provider_credentials": 1,
        "cloud_connections": 0,
        "webhook_signing_secrets": 0,
        "identity_provider_client_secret": true,
    });
    assert_eq!(body["stored"], stored);
    let (status, _, _) = owner.send(&router, Method::GET, &policy, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listed, _) = admin
        .send(
            &router,
            Method::GET,
            "/api/admin/v1/deployment/organizations",
            None,
        )
        .await;
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["id"] != target_id.to_string())
    );
    let (_, listed, _) = admin
        .send(
            &router,
            Method::GET,
            "/api/admin/v1/deployment/organizations?deleted=true",
            None,
        )
        .await;
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == target_id.to_string() && o["stored"] == stored)
    );
    let (status, reused, _) = admin
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/deployment/organizations",
            Some(json!({ "name": "Reuse", "slug": target })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{reused}");
    // A provider saved and then disabled before it was activated holds
    // neither an active nor a pending configuration, but its secret is
    // still stored, so it is reported.
    let reused_id: Uuid = reused["organization"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    async fn client_secret_reported(router: &Router, admin: &mut Browser, id: Uuid) -> Value {
        let (_, listed, _) = admin
            .send(
                router,
                Method::GET,
                "/api/admin/v1/deployment/organizations",
                None,
            )
            .await;
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == id.to_string())
            .unwrap()["stored"]["identity_provider_client_secret"]
            .clone()
    }
    assert_eq!(
        client_secret_reported(&router, &mut admin, reused_id).await,
        json!(false)
    );
    sqlx::query(
        "INSERT INTO gateway.identity_provider_configuration
             (id, organization_id, version, issuer, client_id, secret_key_id, secret_ciphertext)
         VALUES ($1, $2, 1, 'https://login.reuse.example', 'client', 'key', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(reused_id)
    .bind(vec![5_u8; 32])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.organization_identity_provider
             (organization_id, id, enabled, disabled_at)
         VALUES ($1, $2, false, now())",
    )
    .bind(reused_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        client_secret_reported(&router, &mut admin, reused_id).await,
        json!(true)
    );

    // Restore by id: the slug is taken now, so it needs another.
    let (status, body, _) = admin
        .send(
            &router,
            Method::POST,
            &lifecycle("restore"),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("slug_taken"))
    );
    let renamed = slug("deploy-restored");
    let (status, body, _) = admin
        .send(
            &router,
            Method::POST,
            &lifecycle("restore"),
            Some(json!({ "slug": renamed })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "suspended");
    assert_eq!(body["slug"], renamed.as_str());

    let (_, audit, _) = admin
        .send(
            &router,
            Method::GET,
            "/api/admin/v1/deployment/audit?limit=500",
            None,
        )
        .await;
    let mine: Vec<&str> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["organization_id"] == target_id.to_string())
        .map(|event| event["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        mine,
        [
            "organization.restored",
            "organization.deleted",
            "organization.suspended",
            "organization.unsuspended",
            "organization.suspended",
            "organization.deletion_requested",
            "organization.created",
        ]
    );
    let deleted = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| {
            event["action"] == "organization.deleted"
                && event["organization_id"] == target_id.to_string()
        })
        .unwrap();
    assert_eq!(deleted["detail"]["secrets_kept"], stored);
    assert_eq!(deleted["actor_email"], admin_email.as_str());

    // Restored, it is still suspended: nothing works yet.
    assert!(
        identity
            .authenticate_bearer_key(&bearer)
            .await
            .unwrap()
            .is_none()
    );
    // Unsuspended, everything works again as it was before the deletion:
    // the key, the owner's session, and its own identity provider.
    let (status, _, _) = admin
        .send(&router, Method::POST, &lifecycle("unsuspend"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        identity
            .authenticate_bearer_key(&bearer)
            .await
            .unwrap()
            .is_some()
    );
    let (status, body, _) = owner.send(&router, Method::GET, &policy, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let active_provider: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gateway.organization_identity_provider p
         JOIN gateway.organization o ON o.id = p.organization_id AND o.suspended_at IS NULL
         WHERE p.organization_id = $1 AND p.enabled)",
    )
    .bind(target_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(active_provider);

    // Self-serve creation is off by default.
    let (status, body, _) = outsider
        .send(
            &router,
            Method::POST,
            "/api/admin/v1/organizations",
            Some(json!({ "name": "Mine", "slug": slug("deploy-mine") })),
        )
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (
            StatusCode::FORBIDDEN,
            json!("self_serve_organizations_disabled")
        )
    );
    let self_serve = router_with_dns(
        &pool,
        LocalAccountsConfig {
            self_serve_organizations: true,
            ..local_on()
        },
        StaticTxt::shared(),
    );
    let mine = slug("deploy-mine");
    let (status, body, _) = outsider
        .send(
            &self_serve,
            Method::POST,
            "/api/admin/v1/organizations",
            Some(json!({ "name": "Mine", "slug": mine })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let role: String = sqlx::query_scalar(
        "SELECT m.role FROM gateway.organization_membership m
         JOIN gateway.organization o ON o.id = m.organization_id WHERE o.slug = $1",
    )
    .bind(&mine)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "owner");
}

async fn me_id(router: &Router, browser: &mut Browser) -> Uuid {
    let (_, me, _) = browser
        .send(router, Method::GET, "/api/admin/v1/me", None)
        .await;
    me["user_id"].as_str().unwrap().parse().unwrap()
}
