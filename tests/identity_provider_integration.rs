#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for organizations' own identity providers: the admin
//! lane (save, test-and-activate, disable), and sign-in, invitation
//! acceptance and session scope through the real router, against mock OIDC
//! providers that serve discovery, keys and a token endpoint and sign ID
//! tokens with the test key in `src/application/testdata`.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Form, Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gateway::{
    AppState, OrganizationId, UserAccountId,
    application::oidc::HttpOidcBackend,
    config::OidcConfig,
    domain::{
        auth::AuthCrypto, credentials::CredentialCipher, identity::MembershipRole,
        keys::ApiKeyPepper,
    },
    jobs::egress::DeliveryPolicy,
    persistence::IdentityStore,
};
use http_body_util::BodyExt;
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use url::Url;
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
const CSRF_COOKIE: &str = "__Host-gateway_csrf";
const TEST_PEPPER: &[u8] = b"identity-provider-integration-pepper";
const HMAC_KEY: &[u8] = b"identity-provider-integration-hmac-32";
const ORIGIN: &str = "https://gateway.idp.test";
const PROCESS_CLIENT: &str = "process-client";
const ORG_CLIENT: &str = "org-client";
/// A throwaway 2048-bit RSA key generated for tests only.
const RSA_TEST_KEY: &[u8] = include_bytes!("../src/application/testdata/oidc-test-rsa.pk8");

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

// ---------------------------------------------------------------------------
// A mock OIDC provider
// ---------------------------------------------------------------------------

/// How the next ID token differs from a valid one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tweak {
    None,
    Issuer,
    Audience,
    Nonce,
    Signature,
}

struct Idp {
    issuer: String,
    rsa: RsaKeyPair,
    /// The subject, nonce and tweak of the next ID token.
    next: Mutex<Option<(String, String, Tweak)>>,
    /// The issuer the discovery document names, when it should differ.
    discovered_issuer: Mutex<Option<String>>,
    /// Every token request: its form and Authorization header.
    token_requests: Mutex<Vec<(HashMap<String, String>, Option<String>)>>,
    /// When set, the token endpoint signals `held` and waits for this
    /// before answering, so a test can change things mid-sign-in.
    hold: Mutex<Option<Arc<tokio::sync::Notify>>>,
    held: Arc<tokio::sync::Notify>,
}

impl Idp {
    async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idp = Arc::new(Self {
            issuer: format!("http://{}", listener.local_addr().unwrap()),
            rsa: RsaKeyPair::from_pkcs8(RSA_TEST_KEY).unwrap(),
            next: Mutex::new(None),
            discovered_issuer: Mutex::new(None),
            token_requests: Mutex::new(Vec::new()),
            hold: Mutex::new(None),
            held: Arc::new(tokio::sync::Notify::new()),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/keys", get(keys))
            .route("/oauth2/token", post(token))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        idp
    }

    fn sign(&self, claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(
            json!({"alg": "RS256", "kid": "rsa-1", "typ": "JWT"})
                .to_string()
                .as_bytes(),
        );
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        let input = format!("{header}.{payload}");
        let mut signature = vec![0; self.rsa.public().modulus_len()];
        self.rsa
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn expect(&self, subject: &str, nonce: &str, tweak: Tweak) {
        *self.next.lock().unwrap() = Some((subject.to_owned(), nonce.to_owned(), tweak));
    }

    fn token_requests(&self) -> usize {
        self.token_requests.lock().unwrap().len()
    }
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let issuer = idp
        .discovered_issuer
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| idp.issuer.clone());
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{}/oauth2/authorize", idp.issuer),
        "token_endpoint": format!("{}/oauth2/token", idp.issuer),
        "jwks_uri": format!("{}/keys", idp.issuer),
    }))
}

async fn keys(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let rsa = RsaPublicKeyComponents::<Vec<u8>>::from(idp.rsa.public());
    Json(json!({"keys": [{
        "kty": "RSA", "kid": "rsa-1", "use": "sig", "alg": "RS256",
        "n": URL_SAFE_NO_PAD.encode(&rsa.n), "e": URL_SAFE_NO_PAD.encode(&rsa.e),
    }]}))
}

async fn token(
    State(idp): State<Arc<Idp>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let authorization = headers
        .get("authorization")
        .map(|value| value.to_str().unwrap().to_owned());
    idp.token_requests
        .lock()
        .unwrap()
        .push((form, authorization.clone()));
    let hold = idp.hold.lock().unwrap().clone();
    if let Some(release) = hold {
        idp.held.notify_one();
        release.notified().await;
    }
    let Some((subject, nonce, tweak)) = idp.next.lock().unwrap().clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        );
    };
    let mut claims = json!({
        "iss": idp.issuer, "sub": subject, "aud": ORG_CLIENT, "nonce": nonce,
        "exp": 4_000_000_000_u64, "iat": 1_700_000_000_u64,
        "name": "Test Person", "email": "person@example.com",
    });
    // A provider serving the process-wide client too names that audience.
    if authorization.is_none() {
        claims["aud"] = json!(PROCESS_CLIENT);
    }
    match tweak {
        Tweak::Issuer => claims["iss"] = json!("https://impostor.example.com"),
        Tweak::Audience => claims["aud"] = json!("someone-else"),
        Tweak::Nonce => claims["nonce"] = json!("not-the-nonce"),
        Tweak::None | Tweak::Signature => {}
    }
    let mut id_token = idp.sign(&claims);
    if tweak == Tweak::Signature {
        // Claims changed after signing: the signature no longer matches.
        let mut parts: Vec<String> = id_token.split('.').map(str::to_owned).collect();
        claims["sub"] = json!("someone-important");
        parts[1] = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        id_token = parts.join(".");
    }
    (
        StatusCode::OK,
        Json(json!({"id_token": id_token, "access_token": "at", "token_type": "Bearer"})),
    )
}

// ---------------------------------------------------------------------------
// The gateway under test
// ---------------------------------------------------------------------------

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
    /// The process-wide provider.
    process: Arc<Idp>,
}

/// A browser: its cookies, and the CSRF header the admin UI sends.
#[derive(Clone, Debug, Default)]
struct Browser {
    cookies: HashMap<String, String>,
}

impl Browser {
    fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn absorb(&mut self, headers: &HeaderMap) {
        for value in headers.get_all("set-cookie") {
            let pair = value.to_str().unwrap().split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            if value.is_empty() {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers["location"].to_str().unwrap()
    }
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        Self::build(database_url, true, false, false).await
    }

    /// The process-wide provider and local accounts both on, as most
    /// deployments with an identity provider run.
    async fn with_passwords(database_url: &str) -> Self {
        Self::build(database_url, true, false, true).await
    }

    /// A gateway that lets signed-in people create organizations.
    async fn self_serve(database_url: &str) -> Self {
        Self::build(database_url, true, true, false).await
    }

    /// A gateway with only local accounts: no process-wide provider, the
    /// callback derived from the origin (as configuration does).
    async fn local_only(database_url: &str) -> Self {
        Self::build(database_url, false, false, false).await
    }

    async fn build(
        database_url: &str,
        process_wide: bool,
        self_serve: bool,
        passwords: bool,
    ) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[31_u8; 32]).unwrap();
        let process = Idp::start().await;
        let state = AppState::for_sign_in_tests(
            pool.clone(),
            ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap(),
            crypto.clone(),
            OidcConfig {
                enabled: process_wide,
                origin: Some(ORIGIN.into()),
                issuer: process_wide.then(|| process.issuer.clone()),
                audience: process_wide.then(|| PROCESS_CLIENT.into()),
                client_id: process_wide.then(|| PROCESS_CLIENT.into()),
                redirect_uri: Some(format!("{ORIGIN}/api/admin/v1/oidc/callback")),
                microsoft_graph_photo: false,
            },
            Arc::new(HttpOidcBackend::insecure_for_tests(None).unwrap()),
            Some(CredentialCipher::new(&[41_u8; 32]).unwrap()),
            DeliveryPolicy::allow_private_targets_for_tests(),
        )
        .with_local_accounts(gateway::config::LocalAccountsConfig {
            enabled: passwords,
            self_serve_organizations: self_serve,
            ..Default::default()
        });
        Self {
            router: gateway::transport::http::router(state),
            pool,
            identity,
            crypto,
            process,
        }
    }

    async fn org(&self, name: &str) -> (OrganizationId, String) {
        let suffix = Uuid::new_v4().simple().to_string();
        let slug = format!("{name}-{}", &suffix[..12]);
        let id = self
            .identity
            .create_organization(&slug, name)
            .await
            .unwrap()
            .id;
        (id, slug)
    }

    /// A person keyed on `(issuer, subject)`, a member of `org` with the
    /// admin role `binding`.
    async fn person(
        &self,
        org: OrganizationId,
        issuer: &str,
        subject: &str,
        binding: &str,
    ) -> UserAccountId {
        let user = match sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM gateway.user_account WHERE oidc_issuer = $1 AND oidc_subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
        {
            Some(id) => UserAccountId(id),
            None => {
                self.identity
                    .create_user(org, Some(issuer), Some(subject), None, None)
                    .await
                    .unwrap()
                    .id
            }
        };
        self.add_member(org, user, binding).await;
        user
    }

    async fn add_member(&self, org: OrganizationId, user: UserAccountId, binding: &str) {
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
        .bind(binding)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// A process-wide session for `user`, as a browser holds it.
    async fn session(&self, user: UserAccountId) -> Browser {
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
        let mut cookies = HashMap::new();
        cookies.insert(SESSION_COOKIE.to_owned(), session.token.clone());
        cookies.insert(BROWSER_COOKIE.to_owned(), browser);
        cookies.insert(
            CSRF_COOKIE.to_owned(),
            self.crypto.opaque_token().as_str().to_owned(),
        );
        Browser { cookies }
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        browser: Option<&Browser>,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Reply {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if let Some(browser) = browser {
            builder = builder.header(COOKIE, browser.header());
            if let Some(csrf) = browser.cookies.get(CSRF_COOKIE) {
                builder = builder
                    .header("x-csrf-token", csrf)
                    .header("origin", ORIGIN);
            }
        }
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
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn get_provider(&self, org: OrganizationId, browser: &Browser) -> Reply {
        self.send(
            Method::GET,
            &provider_uri(org, ""),
            Some(browser),
            &[],
            None,
        )
        .await
    }

    async fn put_provider(
        &self,
        org: OrganizationId,
        browser: &Browser,
        key: &str,
        if_match: &str,
        body: &Value,
    ) -> Reply {
        self.send(
            Method::PUT,
            &provider_uri(org, ""),
            Some(browser),
            &[("idempotency-key", key), ("if-match", if_match)],
            Some(body),
        )
        .await
    }

    async fn disable_provider(
        &self,
        org: OrganizationId,
        browser: &Browser,
        key: &str,
        if_match: &str,
    ) -> Reply {
        self.send(
            Method::DELETE,
            &provider_uri(org, ""),
            Some(browser),
            &[("idempotency-key", key), ("if-match", if_match)],
            None,
        )
        .await
    }

    /// Follows an authorization URL to `idp` and back: the provider signs
    /// `subject` in (with `tweak`) and the browser returns to the callback.
    async fn complete(
        &self,
        idp: &Idp,
        authorization_url: &str,
        browser: &mut Browser,
        subject: &str,
        tweak: Tweak,
    ) -> Reply {
        let url = Url::parse(authorization_url).unwrap();
        assert!(
            authorization_url.starts_with(&format!("{}/oauth2/authorize?", idp.issuer)),
            "{authorization_url}"
        );
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap()
        };
        assert_eq!(
            param("redirect_uri"),
            format!("{ORIGIN}/api/admin/v1/oidc/callback")
        );
        idp.expect(subject, &param("nonce"), tweak);
        let reply = self
            .send(
                Method::GET,
                &format!(
                    "/api/admin/v1/oidc/callback?state={}&code=code-1",
                    param("state")
                ),
                Some(browser),
                &[],
                None,
            )
            .await;
        browser.absorb(&reply.headers);
        reply
    }

    /// Starts a sign-in (optionally to an organization) and completes it.
    async fn sign_in(
        &self,
        idp: &Idp,
        organization: Option<&str>,
        subject: &str,
        tweak: Tweak,
    ) -> (Reply, Browser) {
        let mut browser = Browser::default();
        let uri = match organization {
            Some(slug) => format!("/api/admin/v1/oidc/start?organization={slug}"),
            None => "/api/admin/v1/oidc/start".to_owned(),
        };
        let start = self.send(Method::GET, &uri, None, &[], None).await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.body);
        browser.absorb(&start.headers);
        let url = start.body["authorization_url"].as_str().unwrap().to_owned();
        let reply = self.complete(idp, &url, &mut browser, subject, tweak).await;
        (reply, browser)
    }

    /// Saves a configuration for `idp` as `owner` and activates it with the
    /// owner's test sign-in. Returns the provider's version.
    async fn activate(&self, org: OrganizationId, owner: &Browser, idp: &Idp) -> u64 {
        let current = self.get_provider(org, owner).await;
        let if_match = match current.body["version"].as_u64().unwrap() {
            0 => "*".to_owned(),
            version => format!("\"v{version}\""),
        };
        let saved = self
            .put_provider(
                org,
                owner,
                &Uuid::new_v4().to_string(),
                &if_match,
                &json!({"issuer": idp.issuer, "client_id": ORG_CLIENT, "client_secret": "org-secret"}),
            )
            .await;
        assert!(saved.status.is_success(), "{}", saved.body);
        let reply = self.test(org, owner, idp, "tester").await;
        assert_eq!(
            reply.location(),
            format!(
                "/admin/organization?organization={}&sign_in_test=activated",
                org.0
            )
        );
        self.get_provider(org, owner).await.body["version"]
            .as_u64()
            .unwrap()
    }

    /// The owner's test sign-in through the pending configuration.
    async fn test(&self, org: OrganizationId, owner: &Browser, idp: &Idp, subject: &str) -> Reply {
        let start = self
            .send(
                Method::POST,
                &provider_uri(org, "/test"),
                Some(owner),
                &[],
                None,
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.body);
        // The owner's session cookies are left alone.
        assert!(start.headers.get("set-cookie").is_none());
        let url = start.body["authorization_url"].as_str().unwrap().to_owned();
        let mut browser = owner.clone();
        let reply = self
            .complete(idp, &url, &mut browser, subject, Tweak::None)
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply
    }

    /// A member invitation's link, made by `owner`.
    async fn invite(&self, org: OrganizationId, owner: &Browser, key: &str) -> String {
        let reply = self
            .send(
                Method::POST,
                &format!("/api/admin/v1/organizations/{}/invitations", org.0),
                Some(owner),
                &[("idempotency-key", key)],
                Some(&json!({"role": "member"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["link"].as_str().unwrap().to_owned()
    }

    /// Opens an invitation link.
    async fn accept(&self, link: &str) -> Reply {
        let reply = self
            .send(
                Method::GET,
                link.strip_prefix(ORIGIN).unwrap(),
                None,
                &[],
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply
    }

    async fn audit_actions(&self, org: OrganizationId) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT action FROM gateway.audit_event
             WHERE organization_id = $1 AND entity_type = 'identity_provider'
               AND outcome = 'succeeded'
             ORDER BY occurred_at, id",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn provider_uri(org: OrganizationId, path: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/identity-provider{path}",
        org.0
    )
}

// ---------------------------------------------------------------------------
// The admin lane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_is_saved_updated_and_disabled_with_etags_and_idempotent_replay() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("idp-lane").await;
    let owner = harness
        .person(
            org,
            &harness.process.issuer,
            "lane-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    let secret = format!("TOP-SECRET-{}", Uuid::new_v4().simple());
    let issuer = "https://login.tenant-a.example.com/v2.0";

    // Nothing yet.
    let reply = harness.get_provider(org, &owner).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["status"], "not_configured");
    assert_eq!(reply.body["version"], 0);
    assert_eq!(
        reply.body["redirect_uri"],
        format!("{ORIGIN}/api/admin/v1/oidc/callback")
    );
    assert!(reply.headers.get("etag").is_none());
    assert_eq!(reply.headers["cache-control"], "no-store");

    // If-Match is required; `*` creates.
    let body = json!({"issuer": issuer, "client_id": "client-a", "client_secret": secret});
    let reply = harness
        .send(
            Method::PUT,
            &provider_uri(org, ""),
            Some(&owner),
            &[("idempotency-key", "save-1")],
            Some(&body),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PRECONDITION_REQUIRED);
    let created = harness
        .put_provider(org, &owner, "save-1", "*", &body)
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.headers["etag"], "\"v1\"");
    assert_eq!(created.body["status"], "pending");
    assert_eq!(created.body["enabled"], false);
    assert_eq!(created.body["pending"]["issuer"], issuer);
    assert_eq!(created.body["pending"]["client_secret_set"], true);
    assert_eq!(created.body["pending"]["configuration_version"], 1);
    assert_eq!(created.body["active"], Value::Null);

    // The same request again replays the original response; the same key
    // with another secret conflicts.
    let replay = harness
        .put_provider(org, &owner, "save-1", "*", &body)
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body, created.body);
    let mut other_secret = body.clone();
    other_secret["client_secret"] = json!("another-secret");
    let reply = harness
        .put_provider(org, &owner, "save-1", "*", &other_secret)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "idempotency_conflict");

    // `*` never overwrites an existing provider.
    let reply = harness
        .put_provider(org, &owner, "save-2", "*", &body)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.body["code"], "version_conflict");

    // An update without the secret keeps it, for the same issuer and client.
    let update = json!({"issuer": issuer, "client_id": "client-a", "extra_scopes": ["groups"]});
    let updated = harness
        .put_provider(org, &owner, "save-3", "\"v1\"", &update)
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.headers["etag"], "\"v2\"");
    assert_eq!(updated.body["pending"]["configuration_version"], 2);
    assert_eq!(updated.body["pending"]["extra_scopes"], json!(["groups"]));
    assert_eq!(updated.body["pending"]["client_secret_set"], true);

    // A stale version, another issuer without the secret, and bad values.
    let reply = harness
        .put_provider(org, &owner, "save-4", "\"v1\"", &update)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "version_conflict");
    for (bad, words) in [
        (
            json!({"issuer": "https://evil.example.com", "client_id": "client-a"}),
            "client_secret is required",
        ),
        (
            json!({"issuer": issuer, "client_id": "client-b"}),
            "client_secret is required",
        ),
        (
            json!({"issuer": "http://login.tenant-a.example.com", "client_id": "c", "client_secret": "s"}),
            "https",
        ),
        (
            json!({"issuer": "https://169.254.169.254", "client_id": "c", "client_secret": "s"}),
            "public host name",
        ),
        (
            json!({"issuer": harness.process.issuer, "client_id": "c", "client_secret": "s"}),
            "own sign-in provider",
        ),
        (
            json!({"issuer": issuer, "client_id": "c", "client_secret": "s", "extra_scopes": ["openid"]}),
            "extra_scopes",
        ),
    ] {
        let reply = harness
            .put_provider(org, &owner, &Uuid::new_v4().to_string(), "\"v2\"", &bad)
            .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(reply.body["code"], "invalid_identity_provider");
        assert!(
            reply.body["detail"].as_str().unwrap().contains(words),
            "{}",
            reply.body
        );
        assert!(reply.body["request_id"].is_string());
    }
    let reply = harness
        .put_provider(
            org,
            &owner,
            "save-5",
            "\"v2\"",
            &json!({"issuer": issuer, "client_id": "c", "client_secret": "s", "unknown": 1}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Disable needs If-Match; then it is off and nothing is pending.
    let reply = harness
        .send(
            Method::DELETE,
            &provider_uri(org, ""),
            Some(&owner),
            &[("idempotency-key", "off-1")],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::PRECONDITION_REQUIRED);
    let disabled = harness
        .disable_provider(org, &owner, "off-1", "\"v2\"")
        .await;
    assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.body);
    assert_eq!(disabled.headers["etag"], "\"v3\"");
    assert_eq!(disabled.body["enabled"], false);
    assert_eq!(disabled.body["pending"], Value::Null);
    // Saved and disabled is not the same as never set up.
    assert_eq!(disabled.body["status"], "disabled");
    let replay = harness
        .disable_provider(org, &owner, "off-1", "\"v2\"")
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body, disabled.body);

    assert_eq!(
        harness.audit_actions(org).await,
        [
            "identity_provider.created",
            "identity_provider.updated",
            "identity_provider.disabled"
        ]
    );

    // The secret is in no response, audit event or idempotency record, and
    // is stored only sealed.
    for reply in [&created, &replay, &updated, &disabled] {
        assert!(!reply.body.to_string().contains(&secret));
    }
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(a)::TEXT FROM gateway.audit_event a WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let idempotency: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(r)::TEXT FROM gateway.admin_idempotency_record r
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert!(!audit.is_empty() && !idempotency.is_empty());
    for row in audit.iter().chain(&idempotency) {
        assert!(!row.contains(&secret), "{row}");
        assert!(!row.contains("TOP-SECRET"), "{row}");
    }
    let sealed: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT secret_ciphertext FROM gateway.identity_provider_configuration
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(sealed.len(), 2);
    for ciphertext in sealed {
        assert!(
            !ciphertext
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
    }
}

#[tokio::test]
async fn permissions_are_owner_write_admin_and_auditor_read_and_others_see_nothing() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("idp-perms").await;
    let (other, _) = harness.org("idp-perms-other").await;
    let issuer = &harness.process.issuer;
    let owner = harness
        .person(org, issuer, "p-owner", "organization_owner")
        .await;
    let owner = harness.session(owner).await;
    let admin = harness
        .person(org, issuer, "p-admin", "organization_admin")
        .await;
    let admin = harness.session(admin).await;
    let auditor = harness.person(org, issuer, "p-auditor", "auditor").await;
    let auditor = harness.session(auditor).await;
    let member = harness
        .person(org, issuer, "p-member", "read_only_member")
        .await;
    let member = harness.session(member).await;
    let outsider = harness
        .person(other, issuer, "p-outsider", "organization_owner")
        .await;
    let outsider = harness.session(outsider).await;
    let body =
        json!({"issuer": "https://idp.perms.example.com", "client_id": "c", "client_secret": "s"});
    let saved = harness.put_provider(org, &owner, "p-1", "*", &body).await;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.body);

    for reader in [&admin, &auditor] {
        let reply = harness.get_provider(org, reader).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert!(!reply.body.to_string().contains("\"s\""));
    }
    let reply = harness.get_provider(org, &member).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.body["code"], "forbidden");

    // Admins read but never write.
    let reply = harness
        .put_provider(org, &admin, "p-2", "\"v1\"", &body)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    let reply = harness.disable_provider(org, &admin, "p-3", "\"v1\"").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = harness
        .send(
            Method::POST,
            &provider_uri(org, "/test"),
            Some(&admin),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);

    // Another organization's owner: not found, whatever they try.
    let reply = harness.get_provider(org, &outsider).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    let reply = harness
        .put_provider(org, &outsider, "p-4", "\"v1\"", &body)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = harness
        .disable_provider(org, &outsider, "p-5", "\"v1\"")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // No session; no CSRF token for the test start.
    let reply = harness
        .send(Method::GET, &provider_uri(org, ""), None, &[], None)
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let mut no_csrf = owner.clone();
    no_csrf.cookies.remove(CSRF_COOKIE);
    let reply = harness
        .send(
            Method::POST,
            &provider_uri(org, "/test"),
            Some(&no_csrf),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.body["code"], "csrf_origin_or_token_required");

    // Nothing changed.
    assert_eq!(harness.get_provider(org, &owner).await.body["version"], 1);
}

// ---------------------------------------------------------------------------
// Test-and-activate, and signing in
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_test_sign_in_activates_and_members_sign_in_through_the_organizations_provider() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let (org, slug) = harness.org("idp-signin").await;
    let (other, _) = harness.org("idp-signin-other").await;
    let process = harness.process.issuer.clone();
    let owner_id = harness
        .person(org, &process, "s-owner", "organization_owner")
        .await;
    let owner = harness.session(owner_id).await;
    let co_owner = harness
        .person(org, &process, "s-co-owner", "organization_owner")
        .await;
    let co_owner = harness.session(co_owner).await;

    let saved = harness
        .put_provider(
            org,
            &owner,
            "s-1",
            "*",
            &json!({"issuer": idp.issuer, "client_id": ORG_CLIENT, "client_secret": "org-s3cret",
                    "extra_scopes": ["groups"]}),
        )
        .await;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.body);

    // While pending, sign-in to the organization still uses the process-wide
    // provider.
    let start = harness
        .send(
            Method::GET,
            &format!("/api/admin/v1/oidc/start?organization={slug}"),
            None,
            &[],
            None,
        )
        .await;
    assert!(
        start.body["authorization_url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{process}/oauth2/authorize?"))
    );

    // Only the owner who saved it may test it.
    let reply = harness
        .send(
            Method::POST,
            &provider_uri(org, "/test"),
            Some(&co_owner),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "tested_by_another_owner");

    // The test: through the pending configuration, with its client and
    // scopes, bound to the owner's session.
    let start = harness
        .send(
            Method::POST,
            &provider_uri(org, "/test"),
            Some(&owner),
            &[],
            None,
        )
        .await;
    let url = Url::parse(start.body["authorization_url"].as_str().unwrap()).unwrap();
    let pairs: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["client_id"], ORG_CLIENT);
    assert_eq!(pairs["scope"], "openid profile email groups");
    assert_eq!(pairs["code_challenge_method"], "S256");
    // Finished in another browser (no owner session): refused, and the code
    // is never sent.
    let mut stranger = Browser::default();
    stranger
        .cookies
        .insert(BROWSER_COOKIE.into(), owner.cookies[BROWSER_COOKIE].clone());
    let requests = idp.token_requests();
    let reply = harness
        .complete(&idp, url.as_str(), &mut stranger, "tester", Tweak::None)
        .await;
    assert_eq!(
        reply.location(),
        format!(
            "/admin/organization?organization={}&sign_in_test=provider_test_session_required",
            org.0
        )
    );
    assert_eq!(idp.token_requests(), requests);
    assert_eq!(
        harness.get_provider(org, &owner).await.body["status"],
        "pending"
    );

    let reply = harness.test(org, &owner, &idp, "tester").await;
    assert_eq!(
        reply.location(),
        format!(
            "/admin/organization?organization={}&sign_in_test=activated",
            org.0
        )
    );
    // The code exchange authenticated the organization's client with its
    // secret.
    let (form, authorization) = idp.token_requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(form["grant_type"], "authorization_code");
    assert!(form.contains_key("code_verifier"));
    assert_eq!(
        authorization,
        Some(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{ORG_CLIENT}:org-s3cret"))
        ))
    );
    let provider = harness.get_provider(org, &owner).await.body;
    assert_eq!(provider["status"], "active");
    assert_eq!(provider["enabled"], true);
    assert_eq!(provider["active"]["issuer"], idp.issuer);
    assert_eq!(provider["last_tested_by"], json!(owner_id.0));
    assert_eq!(provider["pending"], Value::Null);
    assert_eq!(
        harness.audit_actions(org).await,
        ["identity_provider.created", "identity_provider.activated"]
    );
    // The test signed nobody in and provisioned nobody.
    let tester: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.user_account WHERE oidc_issuer = $1")
            .bind(&idp.issuer)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(tester, 0);

    // Someone the organization never admitted is refused.
    let (reply, _) = harness
        .sign_in(&idp, Some(&slug), "nobody", Tweak::None)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.body["error"], "preprovisioned_membership_required");

    // A person invited now is invited for the organization's provider, and
    // accepts through it.
    let invite = harness
        .send(
            Method::POST,
            &format!("/api/admin/v1/organizations/{}/invitations", org.0),
            Some(&owner),
            &[("idempotency-key", "s-invite")],
            Some(&json!({"role": "auditor"})),
        )
        .await;
    assert_eq!(invite.status, StatusCode::CREATED, "{}", invite.body);
    let named: String =
        sqlx::query_scalar("SELECT issuer FROM gateway.organization_invitation WHERE id = $1")
            .bind(Uuid::parse_str(invite.body["invitation"]["id"].as_str().unwrap()).unwrap())
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(named, idp.issuer);
    let link = invite.body["link"].as_str().unwrap();
    let accept = harness
        .send(
            Method::GET,
            link.strip_prefix(ORIGIN).unwrap(),
            None,
            &[],
            None,
        )
        .await;
    assert_eq!(accept.status, StatusCode::SEE_OTHER);
    let mut alice = Browser::default();
    alice.absorb(&accept.headers);
    let location = accept.location().to_owned();
    let reply = harness
        .complete(&idp, &location, &mut alice, "alice", Tweak::None)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/admin/");
    // Alice is keyed on the organization's issuer, never linked by email
    // (the provider says person@example.com, like everyone here).
    let (alice_id, email): (Uuid, Option<String>) = sqlx::query_as(
        "SELECT id, email FROM gateway.user_account WHERE oidc_issuer = $1 AND oidc_subject = 'alice'",
    )
    .bind(&idp.issuer)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(email.as_deref(), Some("person@example.com"));
    let scoped: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM gateway.session WHERE user_id = $1")
            .bind(alice_id)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(scoped, Some(org.0));

    // Signing in again by the organization's slug.
    let (reply, alice) = harness
        .sign_in(&idp, Some(&slug), "alice", Tweak::None)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/admin/");

    // Even if alice is also an owner of another organization, this session
    // acts only in this one.
    harness
        .add_member(other, UserAccountId(alice_id), "organization_owner")
        .await;
    let reply = harness
        .send(
            Method::GET,
            "/api/admin/v1/organizations",
            Some(&alice),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let ids: Vec<&str> = reply
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [org.0.to_string()]);
    for path in [
        "members",
        "invitations",
        "identity-provider",
        "audit-events",
    ] {
        let reply = harness
            .send(
                Method::GET,
                &format!("/api/admin/v1/organizations/{}/{path}", other.0),
                Some(&alice),
                &[],
                None,
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{path}: {}",
            reply.body
        );
    }
    let reply = harness
        .send(
            Method::POST,
            "/api/admin/v1/context",
            Some(&alice),
            &[],
            Some(&json!({"organization_id": other.0})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = harness
        .send(
            Method::POST,
            "/api/admin/v1/context",
            Some(&alice),
            &[],
            Some(&json!({"organization_id": org.0})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    // In its own organization it works as her role allows.
    let reply = harness
        .send(
            Method::GET,
            &format!("/api/admin/v1/organizations/{}/members", org.0),
            Some(&alice),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let reply = harness
        .send(Method::GET, "/api/admin/v1/me", Some(&alice), &[], None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);

    // Disabling falls back to the process-wide provider and ends the
    // organization's sessions; alice herself is kept.
    let version = harness.get_provider(org, &owner).await.body["version"]
        .as_u64()
        .unwrap();
    let reply = harness
        .disable_provider(org, &owner, "s-off", &format!("\"v{version}\""))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let reply = harness
        .send(Method::GET, "/api/admin/v1/me", Some(&alice), &[], None)
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let start = harness
        .send(
            Method::GET,
            &format!("/api/admin/v1/oidc/start?organization={slug}"),
            None,
            &[],
            None,
        )
        .await;
    assert!(
        start.body["authorization_url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{process}/oauth2/authorize?"))
    );
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.organization_membership WHERE user_id = $1 AND enabled",
    )
    .bind(alice_id)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(kept, 2);
    // The owner's own process-wide session was never affected.
    assert_eq!(
        harness.get_provider(org, &owner).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn wrong_issuer_audience_nonce_or_signature_is_rejected() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let (org, slug) = harness.org("idp-reject").await;
    let owner = harness
        .person(
            org,
            &harness.process.issuer,
            "r-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    harness.activate(org, &owner, &idp).await;
    harness
        .person(org, &idp.issuer, "bob", "read_only_member")
        .await;

    for (tweak, code) in [
        (Tweak::Issuer, "oidc_backend_rejected"),
        (Tweak::Audience, "oidc_backend_rejected"),
        (Tweak::Signature, "oidc_backend_rejected"),
        (Tweak::Nonce, "claim_validation_failed"),
    ] {
        let (reply, browser) = harness.sign_in(&idp, Some(&slug), "bob", tweak).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{tweak:?}");
        assert_eq!(reply.body["error"], code, "{tweak:?}");
        assert!(!browser.cookies.contains_key(SESSION_COOKIE), "{tweak:?}");
    }
    let (reply, _) = harness.sign_in(&idp, Some(&slug), "bob", Tweak::None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.session s JOIN gateway.user_account u ON u.id = s.user_id
         WHERE u.oidc_issuer = $1 AND u.oidc_subject = 'bob'",
    )
    .bind(&idp.issuer)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(sessions, 1);

    // A provider whose discovery document names another issuer is never
    // used: its test cannot even start.
    let impostor = Idp::start().await;
    *impostor.discovered_issuer.lock().unwrap() = Some(format!("{}/", impostor.issuer));
    let (other, _) = harness.org("idp-reject-impostor").await;
    let other_owner = harness
        .person(
            other,
            &harness.process.issuer,
            "r-owner-2",
            "organization_owner",
        )
        .await;
    let other_owner = harness.session(other_owner).await;
    let saved = harness
        .put_provider(
            other,
            &other_owner,
            "r-1",
            "*",
            &json!({"issuer": impostor.issuer, "client_id": ORG_CLIENT, "client_secret": "s"}),
        )
        .await;
    assert_eq!(saved.status, StatusCode::CREATED);
    let reply = harness
        .send(
            Method::POST,
            &provider_uri(other, "/test"),
            Some(&other_owner),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["error"], "oidc_provider_unavailable");
    assert_eq!(impostor.token_requests(), 0);
}

#[tokio::test]
async fn a_configuration_changed_mid_sign_in_fails_safely() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let (org, slug) = harness.org("idp-change").await;
    let owner = harness
        .person(
            org,
            &harness.process.issuer,
            "c-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    let version = harness.activate(org, &owner, &idp).await;
    harness
        .person(org, &idp.issuer, "carol", "read_only_member")
        .await;

    // A sign-in starts; the provider is disabled before it comes back.
    let start = harness
        .send(
            Method::GET,
            &format!("/api/admin/v1/oidc/start?organization={slug}"),
            None,
            &[],
            None,
        )
        .await;
    let mut browser = Browser::default();
    browser.absorb(&start.headers);
    let reply = harness
        .disable_provider(org, &owner, "c-off", &format!("\"v{version}\""))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let requests = idp.token_requests();
    let reply = harness
        .complete(
            &idp,
            start.body["authorization_url"].as_str().unwrap(),
            &mut browser,
            "carol",
            Tweak::None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["error"], "identity_provider_changed");
    // The code was never sent to the provider.
    assert_eq!(idp.token_requests(), requests);

    // A test starts; the configuration is saved again before it comes back:
    // the newer one is not activated by the older one's test.
    let saved = harness
        .put_provider(
            org,
            &owner,
            "c-save-1",
            &format!("\"v{}\"", version + 1),
            &json!({"issuer": idp.issuer, "client_id": ORG_CLIENT, "client_secret": "one"}),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let start = harness
        .send(
            Method::POST,
            &provider_uri(org, "/test"),
            Some(&owner),
            &[],
            None,
        )
        .await;
    let resaved = harness
        .put_provider(
            org,
            &owner,
            "c-save-2",
            &format!("\"v{}\"", version + 2),
            &json!({"issuer": idp.issuer, "client_id": ORG_CLIENT, "client_secret": "two"}),
        )
        .await;
    assert_eq!(resaved.status, StatusCode::OK, "{}", resaved.body);
    let mut browser = owner.clone();
    let reply = harness
        .complete(
            &idp,
            start.body["authorization_url"].as_str().unwrap(),
            &mut browser,
            "tester",
            Tweak::None,
        )
        .await;
    assert_eq!(
        reply.location(),
        format!(
            "/admin/organization?organization={}&sign_in_test=identity_provider_changed",
            org.0
        )
    );
    let provider = harness.get_provider(org, &owner).await.body;
    assert_eq!(provider["enabled"], false);
    assert_eq!(provider["pending"]["configuration_version"], 3);
    assert!(
        !harness
            .audit_actions(org)
            .await
            .ends_with(&["identity_provider.activated".to_owned()])
    );
}

#[tokio::test]
async fn an_invitation_made_while_the_provider_is_enabled_is_accepted_only_through_it() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let (org, _) = harness.org("idp-invite").await;
    let owner = harness
        .person(
            org,
            &harness.process.issuer,
            "i-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    // Made before: for the process-wide provider, and it stays so.
    let before = harness.invite(org, &owner, "i-before").await;
    let version = harness.activate(org, &owner, &idp).await;
    let during = harness.invite(org, &owner, "i-during").await;

    let reply = harness.accept(&before).await;
    assert!(
        reply
            .location()
            .starts_with(&format!("{}/oauth2/authorize?", harness.process.issuer))
    );
    let reply = harness.accept(&during).await;
    assert!(
        reply
            .location()
            .starts_with(&format!("{}/oauth2/authorize?", idp.issuer)),
        "{}",
        reply.location()
    );

    // Once the provider is disabled, that invitation goes to the
    // process-wide provider, whose sign-in it does not admit.
    let reply = harness
        .disable_provider(org, &owner, "i-off", &format!("\"v{version}\""))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = harness.accept(&during).await;
    let mut browser = Browser::default();
    browser.absorb(&reply.headers);
    let location = reply.location().to_owned();
    assert!(location.starts_with(&format!("{}/oauth2/authorize?", harness.process.issuer)));
    let reply = harness
        .complete(
            &harness.process,
            &location,
            &mut browser,
            "dave",
            Tweak::None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.body["error"], "invitation_invalid");
    let dave: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.user_account WHERE oidc_subject = 'dave'")
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(dave, 0);
}

#[tokio::test]
async fn an_invitation_is_not_used_up_when_the_provider_changes_during_its_sign_in() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let (org, _) = harness.org("idp-race").await;
    let owner = harness
        .person(
            org,
            &harness.process.issuer,
            "x-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    let version = harness.activate(org, &owner, &idp).await;
    let link = harness.invite(org, &owner, "x-invite").await;
    let accept = harness.accept(&link).await;
    let mut browser = Browser::default();
    browser.absorb(&accept.headers);
    let url = Url::parse(accept.location()).unwrap();
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    };
    idp.expect("frank", &param("nonce"), Tweak::None);

    // The provider is disabled while the code is being exchanged.
    let release = Arc::new(tokio::sync::Notify::new());
    *idp.hold.lock().unwrap() = Some(release.clone());
    let callback = {
        let router = harness.router.clone();
        let request = axum::http::Request::builder()
            .uri(format!(
                "/api/admin/v1/oidc/callback?state={}&code=code-1",
                param("state")
            ))
            .header(COOKIE, browser.header())
            .body(Body::empty())
            .unwrap();
        tokio::spawn(async move { router.oneshot(request).await.unwrap() })
    };
    idp.held.notified().await;
    *idp.hold.lock().unwrap() = None;
    let reply = harness
        .disable_provider(org, &owner, "x-off", &format!("\"v{version}\""))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    release.notify_one();
    let response = callback.await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"], "identity_provider_changed");

    // Nothing was accepted or provisioned: the invitation is still pending.
    let (accepted, frank): (bool, i64) = sqlx::query_as(
        "SELECT i.accepted_at IS NOT NULL,
                (SELECT count(*) FROM gateway.user_account WHERE oidc_subject = 'frank')
         FROM gateway.organization_invitation i
         WHERE i.organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!((accepted, frank), (false, 0));
}

#[tokio::test]
async fn a_local_only_gateway_connects_and_signs_in_through_an_organizations_provider() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::local_only(&database_url).await;
    let idp = Idp::start().await;
    let (org, slug) = harness.org("idp-local-only").await;
    // The owner signed in some other way (a local account, here a session).
    let owner = harness
        .person(
            org,
            "https://owner.local-only.example",
            "s-local-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    // Testing (and so activating) the organization's provider works.
    harness.activate(org, &owner, &idp).await;
    // Its members then sign in through it.
    harness
        .person(org, &idp.issuer, "local-only-member", "read_only_member")
        .await;
    let (reply, _) = harness
        .sign_in(&idp, Some(&slug), "local-only-member", Tweak::None)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/admin/");
    // The process-wide provider stays off.
    let start = harness
        .send(Method::GET, "/api/admin/v1/oidc/start", None, &[], None)
        .await;
    assert_eq!(
        (start.status, start.body["error"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("oidc_disabled"))
    );
}

/// Self-serve creation needs the gateway's own sign-in: a session from one
/// organization's provider acts only there, so it could neither list nor
/// select the organization it made.
#[tokio::test]
async fn an_organization_session_cannot_create_organizations() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::self_serve(&database_url).await;
    let idp = Idp::start().await;
    let process = harness.process.issuer.clone();
    let (org, slug) = harness.org("idp-self-serve").await;
    let owner = harness
        .person(org, &process, "self-serve-owner", "organization_owner")
        .await;
    let owner = harness.session(owner).await;
    harness.activate(org, &owner, &idp).await;
    harness
        .person(org, &idp.issuer, "self-serve-member", "read_only_member")
        .await;
    let (reply, mut member) = harness
        .sign_in(&idp, Some(&slug), "self-serve-member", Tweak::None)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    member.absorb(&reply.headers);
    let me = harness
        .send(Method::GET, "/api/admin/v1/me", Some(&member), &[], None)
        .await;
    assert_eq!(me.body["organization_session"], true, "{}", me.body);
    assert_eq!(me.body["self_serve_organizations"], true);
    let body = json!({ "name": "Mine", "slug": format!("mine-{}", &Uuid::new_v4().simple().to_string()[..12]) });
    let reply = harness
        .send(
            Method::POST,
            "/api/admin/v1/organizations",
            Some(&member),
            &[],
            Some(&body),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.body["code"], "deployment_sign_in_required");
    // The gateway's own sign-in may.
    let me = harness
        .send(Method::GET, "/api/admin/v1/me", Some(&owner), &[], None)
        .await;
    assert_eq!(me.body["organization_session"], false);
    let reply = harness
        .send(
            Method::POST,
            "/api/admin/v1/organizations",
            Some(&owner),
            &[],
            Some(&body),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
}

/// Deleting an organization keeps its provider and client secret as they
/// were; only its state refuses sign-in. Restored and unsuspended, owners
/// who sign in only through it get back in, with the same secret.
#[tokio::test]
async fn a_restored_organizations_provider_signs_in_again_once_unsuspended() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let process = harness.process.issuer.clone();
    let (org, slug) = harness.org("idp-restore").await;
    let (home, _) = harness.org("idp-restore-admin").await;
    let tester = harness
        .person(org, &process, "restore-tester", "organization_owner")
        .await;
    let tester = harness.session(tester).await;
    harness.activate(org, &tester, &idp).await;
    // The owner who signs in only through the organization's provider.
    harness
        .person(org, &idp.issuer, "sso-only-owner", "organization_owner")
        .await;
    let admin = harness
        .person(home, &process, "restore-admin", "organization_owner")
        .await;
    sqlx::query("INSERT INTO gateway.deployment_admin (user_id) VALUES ($1)")
        .bind(admin.0)
        .execute(&harness.pool)
        .await
        .unwrap();
    let admin = harness.session(admin).await;
    let at = |action: &str| format!("/api/admin/v1/deployment/organizations/{}/{action}", org.0);
    let post = |action: &'static str, body: Option<Value>| {
        let uri = at(action);
        let admin = &admin;
        let harness = &harness;
        async move {
            harness
                .send(Method::POST, &uri, Some(admin), &[], body.as_ref())
                .await
        }
    };
    let provider = || async {
        sqlx::query_as::<_, (bool, Vec<u8>)>(
            "SELECT p.enabled, c.secret_ciphertext
             FROM gateway.organization_identity_provider p
             JOIN gateway.identity_provider_configuration c
               ON c.organization_id = p.organization_id AND c.id = p.active_configuration_id
             WHERE p.organization_id = $1",
        )
        .bind(org.0)
        .fetch_one(&harness.pool)
        .await
        .unwrap()
    };
    // Whether sign-in for the organization goes to its own provider.
    let routed_to_own = || async {
        let start = harness
            .send(
                Method::GET,
                &format!("/api/admin/v1/oidc/start?organization={slug}"),
                None,
                &[],
                None,
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.body);
        start.body["authorization_url"]
            .as_str()
            .unwrap()
            .starts_with(&idp.issuer)
    };
    let before = provider().await;
    assert!(before.0);
    assert!(routed_to_own().await);

    assert_eq!(post("suspend", None).await.status, StatusCode::OK);
    assert!(
        !routed_to_own().await,
        "a suspended organization takes no sign-ins"
    );
    let deleted = post("delete", Some(json!({ "confirm_slug": slug }))).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(
        deleted.body["stored"]["identity_provider_client_secret"],
        true
    );
    // Kept exactly as it was: still enabled, the same sealed secret.
    assert_eq!(provider().await, before);
    assert!(!routed_to_own().await);
    let restored = post("restore", None).await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);
    assert_eq!(restored.body["status"], "suspended");
    assert!(!routed_to_own().await, "restored, it is still suspended");

    // Unsuspended: its owner signs in through it, with the original secret.
    let reply = post("unsuspend", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(provider().await, before);
    let (reply, _) = harness
        .sign_in(&idp, Some(&slug), "sso-only-owner", Tweak::None)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/admin/");
    let (_, authorization) = idp.token_requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        authorization,
        Some(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{ORG_CLIENT}:org-secret"))
        ))
    );
}

#[tokio::test]
async fn the_process_wide_sign_in_is_unchanged() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let process = harness.process.clone();
    let (org, slug) = harness.org("idp-process").await;
    let (with_provider, with_provider_slug) = harness.org("idp-process-own").await;
    let erin = harness
        .person(org, &process.issuer, "erin", "organization_owner")
        .await;
    harness
        .add_member(with_provider, erin, "organization_owner")
        .await;
    let erin_session = harness.session(erin).await;
    harness.activate(with_provider, &erin_session, &idp).await;

    // No organization, one without a provider, or an unknown one: the
    // process-wide provider, its client and scope, exactly as before.
    for organization in [None, Some(slug.as_str()), Some("no-such-organization")] {
        let uri = match organization {
            Some(slug) => format!("/api/admin/v1/oidc/start?organization={slug}"),
            None => "/api/admin/v1/oidc/start".to_owned(),
        };
        let start = harness.send(Method::GET, &uri, None, &[], None).await;
        assert_eq!(start.status, StatusCode::OK);
        let url = Url::parse(start.body["authorization_url"].as_str().unwrap()).unwrap();
        let pairs: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert!(
            url.as_str()
                .starts_with(&format!("{}/oauth2/authorize?", process.issuer))
        );
        assert_eq!(pairs["client_id"], PROCESS_CLIENT);
        assert_eq!(pairs["scope"], "openid profile email");
    }
    let (reply, browser) = harness.sign_in(&process, None, "erin", Tweak::None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/admin/");
    // A process-wide session is not limited to an organization, including
    // one with its own provider.
    let reply = harness
        .send(
            Method::GET,
            "/api/admin/v1/organizations",
            Some(&browser),
            &[],
            None,
        )
        .await;
    assert_eq!(reply.body.as_array().unwrap().len(), 2);
    for organization in [org, with_provider] {
        let reply = harness
            .send(
                Method::GET,
                &format!("/api/admin/v1/organizations/{}/members", organization.0),
                Some(&browser),
                &[],
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }
    let scoped: Option<Uuid> = sqlx::query_scalar(
        "SELECT s.organization_id FROM gateway.session s
         JOIN gateway.user_account u ON u.id = s.user_id
         WHERE u.oidc_subject = 'erin' ORDER BY s.created_at DESC LIMIT 1",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(scoped, None);
    // The process-wide checks still refuse a token for another audience.
    let (reply, _) = harness
        .sign_in(&process, None, "erin", Tweak::Audience)
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // And the organization's slug leads to its own provider instead.
    let start = harness
        .send(
            Method::GET,
            &format!("/api/admin/v1/oidc/start?organization={with_provider_slug}"),
            None,
            &[],
            None,
        )
        .await;
    assert!(
        start.body["authorization_url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}/oauth2/authorize?", idp.issuer))
    );
}

/// Email-first sign-in with local accounts on as well: an address no
/// organization claims goes to the process-wide provider, not to a password
/// its owner may never have set, and the answer says a password would also
/// do.
#[tokio::test]
async fn email_first_sign_in_prefers_the_process_wide_provider_to_a_password() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let harness = Harness::with_passwords(&database_url).await;
    let process = harness.process.clone();
    let email = format!("Ada@{}.example", &Uuid::new_v4().simple().to_string()[..12]);
    let reply = harness
        .send(
            Method::POST,
            "/api/admin/v1/sign-in/start",
            None,
            &[("origin", ORIGIN)],
            Some(&json!({ "email": email })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["next"], "redirect");
    assert_eq!(reply.body["password"], true);
    let url = Url::parse(reply.body["authorization_url"].as_str().unwrap()).unwrap();
    assert!(
        url.as_str()
            .starts_with(&format!("{}/oauth2/authorize?", process.issuer))
    );
    let hint = url
        .query_pairs()
        .find(|(key, _)| key == "login_hint")
        .map(|(_, value)| value.into_owned());
    assert_eq!(hint.as_deref(), Some(email.to_lowercase().as_str()));
}

/// Email-first sign-in: an address whose domain the organization verified
/// goes to its own provider, with the address as the login hint; any other
/// address to the process-wide provider (there are no local accounts here).
#[tokio::test]
async fn email_first_sign_in_routes_a_verified_domain_to_its_organizations_provider() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let idp = Idp::start().await;
    let process = harness.process.clone();
    let (org, _) = harness.org("idp-email-first").await;
    let owner = harness
        .person(
            org,
            &process.issuer,
            "email-first-owner",
            "organization_owner",
        )
        .await;
    let owner = harness.session(owner).await;
    harness.activate(org, &owner, &idp).await;
    let domain = format!("{}.example", &Uuid::new_v4().simple().to_string()[..12]);
    let email = format!("Grace@{}", domain.to_uppercase());
    let start = |email: String| {
        let harness = &harness;
        async move {
            harness
                .send(
                    Method::POST,
                    "/api/admin/v1/sign-in/start",
                    None,
                    &[("origin", ORIGIN)],
                    Some(&json!({ "email": email })),
                )
                .await
        }
    };
    let hint = |reply: &Reply| {
        let url = Url::parse(reply.body["authorization_url"].as_str().unwrap()).unwrap();
        let hint = url
            .query_pairs()
            .find(|(key, _)| key == "login_hint")
            .map(|(_, value)| value.into_owned());
        (url, hint)
    };

    // Before the domain is verified: the process-wide provider.
    let reply = start(email.clone()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["next"], "redirect");
    let (url, login_hint) = hint(&reply);
    assert!(
        url.as_str()
            .starts_with(&format!("{}/oauth2/authorize?", process.issuer))
    );
    assert_eq!(login_hint.as_deref(), Some(email.to_lowercase().as_str()));

    sqlx::query(
        "INSERT INTO gateway.organization_domain
             (id, organization_id, domain, verification_token, verified_at)
         VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(&domain)
    .bind(Uuid::new_v4().simple().to_string())
    .execute(&harness.pool)
    .await
    .unwrap();

    // Verified: the organization's provider, and the sign-in completes there.
    let reply = start(email.clone()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["password"], false);
    let (url, login_hint) = hint(&reply);
    assert!(
        url.as_str()
            .starts_with(&format!("{}/oauth2/authorize?", idp.issuer))
    );
    assert_eq!(login_hint.as_deref(), Some(email.to_lowercase().as_str()));
    let mut browser = Browser::default();
    browser.absorb(&reply.headers);
    let member = harness
        .person(org, &idp.issuer, "grace", "read_only_member")
        .await;
    let done = harness
        .complete(&idp, url.as_str(), &mut browser, "grace", Tweak::None)
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    assert_eq!(done.location(), "/admin/");
    let me = harness
        .send(Method::GET, "/api/admin/v1/me", Some(&browser), &[], None)
        .await;
    assert_eq!(me.body["user_id"], member.0.to_string());

    // Another site cannot start it; nonsense is refused.
    let reply = harness
        .send(
            Method::POST,
            "/api/admin/v1/sign-in/start",
            None,
            &[("origin", "https://evil.example")],
            Some(&json!({ "email": email })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = start("not-an-email".to_owned()).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["code"], "invalid_request");
}
