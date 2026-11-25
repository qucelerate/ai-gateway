#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for webhooks: the admin lane (driven through the real
//! router, RBAC, idempotency, versioning, and audit), outbox fan-out into
//! deliveries, and signed delivery to a local listener with retry, parking,
//! and the attempt fence.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use gateway::{
    OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        credentials::CredentialCipher,
        identity::MembershipRole,
        keys::ApiKeyPepper,
        outbox::{OutboxPayload, ReconciliationOpened, ReconciliationUpdated},
        webhook::sign,
    },
    jobs::webhooks::{DeliveryPolicy, MAX_DELIVERY_ATTEMPTS, WebhookDeliverer},
    persistence::{
        AdminStore, AuthStore, IdentityStore, OutboxStore, outbox,
        webhooks::{WebhookFanout, WebhookStore},
    },
    transport::webhooks::router,
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
const TEST_PEPPER: &[u8] = b"webhook-integration-pepper";
const HMAC_KEY: &[u8] = b"webhook-integration-hmac-key-000032";
const CREDENTIAL_KEY: [u8; 32] = [23; 32];
const HOOK_URL: &str = "https://hooks.example.com/gateway";

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
    store: WebhookStore,
    outbox: OutboxStore,
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        Self::with_cipher(database_url, Some(cipher())).await
    }

    async fn with_cipher(database_url: &str, cipher: Option<CredentialCipher>) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[29_u8; 32]).unwrap();
        let store = WebhookStore::new(pool.clone());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            store.clone(),
            cipher,
        );
        Self {
            outbox: OutboxStore::new(pool.clone()),
            pool,
            identity,
            crypto,
            router,
            store,
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

    /// A user holding exactly one org-wide admin role binding.
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
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, NULL, $4, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
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

    async fn owner(&self, slug: &str) -> (OrganizationId, (String, String)) {
        let org = self.org(slug).await;
        let owner = self.user(org, "organization_owner").await;
        (org, self.session(owner).await)
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

    async fn create(
        &self,
        org: OrganizationId,
        session: &(String, String),
        key: &str,
        body: &Value,
    ) -> (StatusCode, Value, HeaderMap) {
        self.send(
            Method::POST,
            &uri(org, ""),
            Some(session),
            &[("idempotency-key", key)],
            Some(body),
        )
        .await
    }

    /// Creates an endpoint through the API, then points it at `url`, which
    /// the API would refuse (a local listener).
    async fn endpoint_at(
        &self,
        org: OrganizationId,
        session: &(String, String),
        url: &str,
    ) -> (Uuid, String) {
        let (status, created, _) = self
            .create(
                org,
                session,
                &Uuid::new_v4().to_string(),
                &json!({"url": HOOK_URL, "event_types": ["reconciliation.opened"]}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let id = Uuid::parse_str(created["endpoint"]["id"].as_str().unwrap()).unwrap();
        sqlx::query("UPDATE gateway.webhook_endpoint SET url = $2 WHERE id = $1")
            .bind(id)
            .bind(url)
            .execute(&self.pool)
            .await
            .unwrap();
        (id, created["secret"].as_str().unwrap().to_owned())
    }

    /// Emits a `reconciliation.opened` event and fans every due event out.
    async fn emit_and_fan_out(&self, org: OrganizationId) -> Uuid {
        let mut conn = self.pool.acquire().await.unwrap();
        let event = outbox::emit(&mut conn, org, &opened()).await.unwrap();
        drop(conn);
        while self
            .outbox
            .dispatch_batch(&WebhookFanout, Utc::now(), 500)
            .await
            .unwrap()
            .dispatched
            > 0
        {}
        event
    }

    async fn delivery(&self, endpoint: Uuid, event: Uuid) -> DeliveryRow {
        sqlx::query_as(
            "SELECT status, attempts, last_status_code, last_error,
                    next_attempt_at > now() AS backing_off
             FROM gateway.webhook_delivery WHERE endpoint_id = $1 AND event_id = $2",
        )
        .bind(endpoint)
        .bind(event)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Keeps other tests' deliveries out of this test's claims.
    async fn defer_other_deliveries(&self) {
        sqlx::query(
            "UPDATE gateway.webhook_delivery SET next_attempt_at = now() + interval '1 day'
             WHERE status = 'pending'",
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }

    fn deliverer(&self, policy: DeliveryPolicy) -> WebhookDeliverer {
        WebhookDeliverer::new(self.store.clone(), Some(cipher()), policy)
    }
}

#[derive(Debug, sqlx::FromRow)]
struct DeliveryRow {
    status: String,
    attempts: i32,
    last_status_code: Option<i32>,
    last_error: Option<String>,
    backing_off: bool,
}

fn cipher() -> CredentialCipher {
    CredentialCipher::new(&CREDENTIAL_KEY).unwrap()
}

fn opened() -> OutboxPayload {
    OutboxPayload::ReconciliationOpened(ReconciliationOpened {
        item_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
        reason: "provider-unknown".to_owned(),
        created_at: Utc::now(),
    })
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/webhook-endpoints{path}",
        org.0
    )
}

#[derive(Clone, Debug)]
struct Received {
    headers: HeaderMap,
    body: Bytes,
}

/// A local HTTP listener that records every request and answers with the
/// current `status`.
struct Listener {
    url: String,
    received: Arc<Mutex<Vec<Received>>>,
    status: Arc<Mutex<StatusCode>>,
}

impl Listener {
    async fn start() -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(StatusCode::NO_CONTENT));
        let app = Router::new().route(
            "/hook",
            axum::routing::post({
                let received = received.clone();
                let status = status.clone();
                move |headers: HeaderMap, body: Bytes| async move {
                    received.lock().unwrap().push(Received { headers, body });
                    *status.lock().unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{address}/hook"),
            received,
            status,
        }
    }

    fn respond_with(&self, status: StatusCode) {
        *self.status.lock().unwrap() = status;
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn endpoints_are_created_with_a_one_time_secret_then_updated_rotated_and_deleted() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, owner) = harness.owner("webhooks").await;

    let body = json!({
        "url": HOOK_URL,
        "description": "billing sync",
        "event_types": ["reconciliation.opened", "inference.finalized"],
    });
    let (status, created, headers) = harness.create(org, &owner, "create-1", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["etag"], "\"v1\"");
    let secret = created["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("whsec_"));
    assert_eq!(created["replay"], json!(false));
    assert_eq!(created["error"], Value::Null);
    let endpoint = &created["endpoint"];
    assert_eq!(endpoint["url"], HOOK_URL);
    assert_eq!(endpoint["enabled"], json!(true));
    assert_eq!(endpoint["version"], json!(1));
    // Sorted.
    assert_eq!(
        endpoint["event_types"],
        json!(["inference.finalized", "reconciliation.opened"])
    );
    let id = endpoint["id"].as_str().unwrap().to_owned();

    // The secret is stored sealed, never in the clear.
    let (key_id, ciphertext): (String, Vec<u8>) = sqlx::query_as(
        "SELECT secret_key_id, secret_ciphertext FROM gateway.webhook_endpoint WHERE id = $1",
    )
    .bind(Uuid::parse_str(&id).unwrap())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(!key_id.is_empty());
    let key = STANDARD.decode(&secret["whsec_".len()..]).unwrap();
    assert!(!ciphertext.windows(key.len()).any(|window| window == key));
    assert!(!String::from_utf8_lossy(&ciphertext).contains(&secret));

    // A replay returns the endpoint but not the secret.
    let (status, replay, _) = harness.create(org, &owner, "create-1", &body).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["endpoint"], created["endpoint"]);
    assert_eq!(replay["secret"], Value::Null);
    assert_eq!(replay["replay"], json!(true));
    assert_eq!(replay["error"], "secret_unavailable_on_replay");

    // Reads never carry the secret.
    let (status, fetched, headers) = harness.get(&uri(org, &format!("/{id}")), &owner).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(&fetched, endpoint);
    let (status, listed, _) = harness.get(&uri(org, ""), &owner).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["items"], json!([endpoint]));
    assert!(!listed.to_string().contains("whsec_"));

    // Updates need a current version and change only what is present.
    let endpoint_uri = uri(org, &format!("/{id}"));
    let (status, conflict, _) = harness
        .send(
            Method::PATCH,
            &endpoint_uri,
            Some(&owner),
            &[("idempotency-key", "stale"), ("if-match", "\"v9\"")],
            Some(&json!({"enabled": false})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "version_conflict");
    let (status, updated, headers) = harness
        .send(
            Method::PATCH,
            &endpoint_uri,
            Some(&owner),
            &[("idempotency-key", "update-1"), ("if-match", "\"v1\"")],
            Some(&json!({"enabled": false, "description": ""})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(headers["etag"], "\"v2\"");
    assert_eq!(updated["enabled"], json!(false));
    assert_eq!(updated["description"], Value::Null);
    assert_eq!(updated["url"], HOOK_URL);
    let (status, empty, _) = harness
        .send(
            Method::PATCH,
            &endpoint_uri,
            Some(&owner),
            &[("idempotency-key", "update-empty"), ("if-match", "*")],
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{empty}");

    // Rotation issues a new secret, once.
    let rotate_uri = uri(org, &format!("/{id}/rotate-secret"));
    let rotate_headers = [("idempotency-key", "rotate-1"), ("if-match", "\"v2\"")];
    let (status, rotated, headers) = harness
        .send(
            Method::POST,
            &rotate_uri,
            Some(&owner),
            &rotate_headers,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    assert_eq!(headers["etag"], "\"v3\"");
    let rotated_secret = rotated["secret"].as_str().unwrap();
    assert!(rotated_secret.starts_with("whsec_"));
    assert_ne!(rotated_secret, secret);
    let (status, rotate_replay, _) = harness
        .send(
            Method::POST,
            &rotate_uri,
            Some(&owner),
            &rotate_headers,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rotate_replay}");
    assert_eq!(rotate_replay["secret"], Value::Null);
    assert_eq!(rotate_replay["error"], "secret_unavailable_on_replay");

    // Delete, replay, and the endpoint is gone.
    let delete_headers = [("idempotency-key", "delete-1"), ("if-match", "\"v3\"")];
    let (status, _, _) = harness
        .send(
            Method::DELETE,
            &endpoint_uri,
            Some(&owner),
            &delete_headers,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, replay, _) = harness
        .send(
            Method::DELETE,
            &endpoint_uri,
            Some(&owner),
            &delete_headers,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    let (status, missing, _) = harness.get(&endpoint_uri, &owner).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");

    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'webhook_endpoint' AND outcome = 'succeeded'
         ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        audit,
        [
            "webhook_endpoint.created",
            "webhook_endpoint.updated",
            "webhook_endpoint.secret_rotated",
            "webhook_endpoint.deleted",
        ]
    );
    let audit_text: String = sqlx::query_scalar(
        "SELECT coalesce(string_agg(to_jsonb(a)::text, ''), '') FROM gateway.audit_event a
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(!audit_text.contains(&secret));
    assert!(!audit_text.contains(rotated_secret));
}

#[tokio::test]
async fn reads_are_org_wide_and_writes_are_owner_and_admin_only() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, owner) = harness.owner("webhook-rbac").await;
    let (status, created, _) = harness
        .create(
            org,
            &owner,
            "rbac-1",
            &json!({"url": HOOK_URL, "event_types": ["inference.finalized"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["endpoint"]["id"].as_str().unwrap().to_owned();

    for (role, can_write) in [
        ("organization_admin", true),
        ("operator", false),
        ("auditor", false),
    ] {
        let user = harness.user(org, role).await;
        let session = harness.session(user).await;
        let (status, listed, _) = harness.get(&uri(org, ""), &session).await;
        assert_eq!(status, StatusCode::OK, "{role}: {listed}");
        let (status, _, _) = harness
            .get(&uri(org, &format!("/{id}/deliveries")), &session)
            .await;
        assert_eq!(status, StatusCode::OK, "{role}");
        let (status, body, _) = harness
            .create(
                org,
                &session,
                &format!("{role}-create"),
                &json!({"url": HOOK_URL, "event_types": ["inference.finalized"]}),
            )
            .await;
        let expected = if can_write {
            StatusCode::CREATED
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, expected, "{role}: {body}");
    }

    // No session, no access; another organization's endpoint is not found.
    let (status, _, _) = harness
        .send(Method::GET, &uri(org, ""), None, &[], None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (other, other_owner) = harness.owner("webhook-other").await;
    let (status, _, _) = harness
        .get(&uri(other, &format!("/{id}")), &other_owner)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = harness
        .send(
            Method::DELETE,
            &uri(other, &format!("/{id}")),
            Some(&other_owner),
            &[("idempotency-key", "cross-org"), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = harness
        .get(&uri(org, &format!("/{id}")), &other_owner)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unsafe_urls_bad_input_and_excess_endpoints_are_rejected() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, owner) = harness.owner("webhook-ssrf").await;

    for url in [
        "http://hooks.example.com/in",
        "ftp://hooks.example.com/in",
        "https://127.0.0.1/in",
        "https://10.1.2.3/in",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/in",
        "https://[::ffff:192.168.0.1]/in",
        "https://localhost/in",
        "https://metadata/in",
        "https://user:pass@hooks.example.com/in",
        "https://hooks.example.com/in#fragment",
        "not a url",
    ] {
        let (status, problem, _) = harness
            .create(
                org,
                &owner,
                &Uuid::new_v4().to_string(),
                &json!({"url": url, "event_types": ["inference.finalized"]}),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {problem}");
        assert_eq!(problem["code"], "invalid_webhook_endpoint", "{url}");
    }
    for body in [
        json!({"url": HOOK_URL, "event_types": []}),
        json!({"url": HOOK_URL, "event_types": ["inference.finalized", "inference.finalized"]}),
        json!({"url": HOOK_URL, "event_types": ["inference.finalized"], "description": ""}),
    ] {
        let (status, problem, _) = harness
            .create(org, &owner, &Uuid::new_v4().to_string(), &body)
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    }
    let (status, _, _) = harness
        .create(
            org,
            &owner,
            "unknown-field",
            &json!({"url": HOOK_URL, "event_types": ["inference.finalized"], "secret": "x"}),
        )
        .await;
    assert!(status.is_client_error(), "{status}");
    let (status, _, _) = harness
        .create(
            org,
            &owner,
            "unknown-event",
            &json!({"url": HOOK_URL, "event_types": ["everything"]}),
        )
        .await;
    assert!(status.is_client_error(), "{status}");

    for n in 0..20 {
        let (status, body, _) = harness
            .create(
                org,
                &owner,
                &format!("limit-{n}"),
                &json!({"url": HOOK_URL, "event_types": ["inference.finalized"]}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{n}: {body}");
    }
    let (status, problem, _) = harness
        .create(
            org,
            &owner,
            "limit-21",
            &json!({"url": HOOK_URL, "event_types": ["inference.finalized"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "webhook_endpoint_limit_reached");
}

#[tokio::test]
async fn without_a_credential_key_secrets_cannot_be_issued() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::with_cipher(&database_url, None).await;
    let (org, owner) = harness.owner("webhook-nokey").await;
    let (status, problem, _) = harness
        .create(
            org,
            &owner,
            "no-key",
            &json!({"url": HOOK_URL, "event_types": ["inference.finalized"]}),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(problem["code"], "credential_encryption_unavailable");
    let (status, _, _) = harness.get(&uri(org, ""), &owner).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn events_fan_out_once_to_endpoints_subscribed_when_they_occurred() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, owner) = harness.owner("webhook-fanout").await;
    let mut ids = Vec::new();
    for (key, event_types, enabled) in [
        ("subscribed", json!(["reconciliation.opened"]), true),
        ("disabled", json!(["reconciliation.opened"]), false),
        ("other-type", json!(["inference.finalized"]), true),
    ] {
        let (status, created, _) = harness
            .create(
                org,
                &owner,
                key,
                &json!({"url": HOOK_URL, "event_types": event_types, "enabled": enabled}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        ids.push(Uuid::parse_str(created["endpoint"]["id"].as_str().unwrap()).unwrap());
    }
    // Another organization's endpoint never sees this organization's events.
    let (other, other_owner) = harness.owner("webhook-fanout-other").await;
    let (status, _, _) = harness
        .create(
            other,
            &other_owner,
            "other-org",
            &json!({"url": HOOK_URL, "event_types": ["reconciliation.opened"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let event = harness.emit_and_fan_out(org).await;
    let deliveries: Vec<(Uuid, String, Value)> = sqlx::query_as(
        "SELECT endpoint_id, status, payload FROM gateway.webhook_delivery WHERE event_id = $1",
    )
    .bind(event)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    assert_eq!(deliveries[0].0, ids[0]);
    assert_eq!(deliveries[0].1, "pending");
    assert_eq!(deliveries[0].2["reason"], "provider-unknown");

    // Fanning the same event out again adds nothing.
    let stored: (Uuid, i64, Uuid, String, Value, chrono::DateTime<Utc>, i32) = sqlx::query_as(
        "SELECT id, sequence, organization_id, event_type, payload, occurred_at, attempts
         FROM gateway.outbox_event WHERE id = $1",
    )
    .bind(event)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    let replayed = gateway::persistence::OutboxEvent {
        id: stored.0,
        sequence: stored.1,
        organization_id: OrganizationId(stored.2),
        payload: OutboxPayload::from_stored(
            gateway::domain::outbox::OutboxEventType::parse(&stored.3).unwrap(),
            stored.4,
        )
        .unwrap(),
        occurred_at: stored.5,
        attempts: stored.6,
    };
    // An endpoint created after the event occurred never receives it, even
    // when the event is fanned out again.
    let (status, _, _) = harness
        .create(
            org,
            &owner,
            "late",
            &json!({"url": HOOK_URL, "event_types": ["reconciliation.opened"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let mut conn = harness.pool.acquire().await.unwrap();
    gateway::persistence::OutboxSink::deliver(&WebhookFanout, &mut conn, &replayed)
        .await
        .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.webhook_delivery WHERE event_id = $1")
            .bind(event)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // Reason updates fan out to endpoints subscribed to them.
    let (status, created, _) = harness
        .create(
            org,
            &owner,
            "updates",
            &json!({"url": HOOK_URL, "event_types": ["reconciliation.updated"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let updates = Uuid::parse_str(created["endpoint"]["id"].as_str().unwrap()).unwrap();
    let event = outbox::emit(
        &mut conn,
        org,
        &OutboxPayload::ReconciliationUpdated(ReconciliationUpdated {
            item_id: Uuid::new_v4(),
            reservation_id: Uuid::new_v4(),
            previous_reason: "expired-after-claim".to_owned(),
            reason: "input-count-drift".to_owned(),
            updated_at: Utc::now(),
        }),
    )
    .await
    .unwrap();
    drop(conn);
    while harness
        .outbox
        .dispatch_batch(&WebhookFanout, Utc::now(), 500)
        .await
        .unwrap()
        .dispatched
        > 0
    {}
    let deliveries: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT endpoint_id, event_type FROM gateway.webhook_delivery WHERE event_id = $1",
    )
    .bind(event)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(deliveries, [(updates, "reconciliation.updated".to_owned())]);
}

#[tokio::test]
async fn deliveries_are_signed_posted_and_recorded() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let listener = Listener::start().await;
    let (org, owner) = harness.owner("webhook-deliver").await;
    let (endpoint, secret) = harness.endpoint_at(org, &owner, &listener.url).await;
    harness.defer_other_deliveries().await;
    let event = harness.emit_and_fan_out(org).await;

    let report = harness
        .deliverer(DeliveryPolicy::allow_private_targets_for_tests())
        .deliver_due()
        .await
        .unwrap();
    assert_eq!(report.delivered, 1, "{report:?}");
    let received = listener.received();
    assert_eq!(received.len(), 1);
    let request = &received[0];
    let header = |name: &str| request.headers[name].to_str().unwrap().to_owned();
    assert_eq!(header("content-type"), "application/json");
    assert_eq!(header("webhook-id"), event.to_string());
    let timestamp: i64 = header("webhook-timestamp").parse().unwrap();
    assert!((Utc::now().timestamp() - timestamp).abs() < 60);
    let key = STANDARD.decode(&secret["whsec_".len()..]).unwrap();
    assert_eq!(
        header("webhook-signature"),
        sign(&key, &event.to_string(), timestamp, &request.body)
    );
    let envelope: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(envelope["id"], event.to_string());
    assert_eq!(envelope["type"], "reconciliation.opened");
    assert_eq!(envelope["organization_id"], org.0.to_string());
    assert_eq!(envelope["data"]["reason"], "provider-unknown");

    let row = harness.delivery(endpoint, event).await;
    assert_eq!(row.status, "delivered");
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_status_code, Some(204));

    // Delivered once; nothing is due again.
    let report = harness
        .deliverer(DeliveryPolicy::allow_private_targets_for_tests())
        .deliver_due()
        .await
        .unwrap();
    assert_eq!(report.claimed, 0);

    let (status, page, _) = harness
        .get(&uri(org, &format!("/{endpoint}/deliveries")), &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["event_id"], event.to_string());
    assert_eq!(page["items"][0]["status"], "delivered");
    assert_eq!(page["items"][0]["next_attempt_at"], Value::Null);
}

#[tokio::test]
async fn failed_deliveries_back_off_then_park_and_stale_attempts_are_fenced() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let listener = Listener::start().await;
    listener.respond_with(StatusCode::INTERNAL_SERVER_ERROR);
    let (org, owner) = harness.owner("webhook-retry").await;
    let (endpoint, _) = harness.endpoint_at(org, &owner, &listener.url).await;
    harness.defer_other_deliveries().await;
    let event = harness.emit_and_fan_out(org).await;
    let deliverer = harness.deliverer(DeliveryPolicy::allow_private_targets_for_tests());

    let report = deliverer.deliver_due().await.unwrap();
    assert_eq!(report.failed, 1, "{report:?}");
    let row = harness.delivery(endpoint, event).await;
    assert_eq!(row.status, "pending");
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_status_code, Some(500));
    assert_eq!(
        row.last_error.as_deref(),
        Some("endpoint returned a non-success status")
    );
    assert!(row.backing_off);
    // Backing off: not due again yet.
    assert_eq!(deliverer.deliver_due().await.unwrap().claimed, 0);

    // The last allowed attempt fails too, and the delivery is parked.
    sqlx::query(
        "UPDATE gateway.webhook_delivery SET attempts = $3, next_attempt_at = now()
         WHERE endpoint_id = $1 AND event_id = $2",
    )
    .bind(endpoint)
    .bind(event)
    .bind(MAX_DELIVERY_ATTEMPTS - 1)
    .execute(&harness.pool)
    .await
    .unwrap();
    let report = deliverer.deliver_due().await.unwrap();
    assert_eq!(report.parked, 1, "{report:?}");
    let row = harness.delivery(endpoint, event).await;
    assert_eq!(row.status, "parked");
    assert_eq!(row.attempts, MAX_DELIVERY_ATTEMPTS);
    assert_eq!(deliverer.deliver_due().await.unwrap().claimed, 0);

    // A worker whose lease lapsed cannot record over the newer attempt.
    listener.respond_with(StatusCode::OK);
    let event = harness.emit_and_fan_out(org).await;
    // A suspended organization's deliveries wait, kept, until it is lifted.
    let suspend = |suspended: bool| {
        sqlx::query(
            "UPDATE gateway.organization
             SET suspended_at = CASE WHEN $2 THEN now() END WHERE id = $1",
        )
        .bind(org.0)
        .bind(suspended)
        .execute(&harness.pool)
    };
    suspend(true).await.unwrap();
    assert!(
        harness
            .store
            .claim_due(Utc::now(), Utc::now() - chrono::Duration::seconds(1), 16)
            .await
            .unwrap()
            .is_empty()
    );
    suspend(false).await.unwrap();
    let first = harness
        .store
        .claim_due(Utc::now(), Utc::now() - chrono::Duration::seconds(1), 16)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let second = harness
        .store
        .claim_due(Utc::now(), Utc::now() + chrono::Duration::minutes(1), 16)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].attempts, first[0].attempts + 1);
    harness
        .store
        .record_failed(&first[0], Some(503), "stale", None)
        .await
        .unwrap();
    let row = harness.delivery(endpoint, event).await;
    assert_eq!(row.status, "pending", "a stale attempt must not park");
    harness
        .store
        .record_delivered(&second[0], Utc::now(), 200)
        .await
        .unwrap();
    assert_eq!(harness.delivery(endpoint, event).await.status, "delivered");

    // A disabled endpoint's deliveries wait instead of being attempted.
    let event = harness.emit_and_fan_out(org).await;
    sqlx::query("UPDATE gateway.webhook_endpoint SET enabled = false WHERE id = $1")
        .bind(endpoint)
        .execute(&harness.pool)
        .await
        .unwrap();
    assert_eq!(deliverer.deliver_due().await.unwrap().claimed, 0);
    assert_eq!(harness.delivery(endpoint, event).await.status, "pending");
}

#[tokio::test]
async fn parked_deliveries_are_redelivered_by_writers_and_kept_after_their_last_attempt() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let listener = Listener::start().await;
    listener.respond_with(StatusCode::INTERNAL_SERVER_ERROR);
    let (org, owner) = harness.owner("webhook-redeliver").await;
    let (endpoint, _) = harness.endpoint_at(org, &owner, &listener.url).await;
    let (other_endpoint, _) = harness.endpoint_at(org, &owner, &listener.url).await;
    harness.defer_other_deliveries().await;
    let event = harness.emit_and_fan_out(org).await;
    sqlx::query(
        "UPDATE gateway.webhook_delivery SET next_attempt_at = now() + interval '1 day'
         WHERE endpoint_id = $1",
    )
    .bind(other_endpoint)
    .execute(&harness.pool)
    .await
    .unwrap();
    let deliverer = harness.deliverer(DeliveryPolicy::allow_private_targets_for_tests());
    // An attempt that outlives its lease, still running across the park and
    // redelivery below.
    let stale = harness
        .store
        .claim_due(Utc::now(), Utc::now() - chrono::Duration::seconds(1), 16)
        .await
        .unwrap();
    assert_eq!((stale.len(), stale[0].attempts), (1, 1));
    // Brings the current run to its last attempt, which fails.
    let park = || async {
        sqlx::query(
            "UPDATE gateway.webhook_delivery
             SET attempts = attempts_before_run + $3, next_attempt_at = now()
             WHERE endpoint_id = $1 AND event_id = $2",
        )
        .bind(endpoint)
        .bind(event)
        .bind(MAX_DELIVERY_ATTEMPTS - 1)
        .execute(&harness.pool)
        .await
        .unwrap();
        assert_eq!(deliverer.deliver_due().await.unwrap().parked, 1);
    };
    park().await;
    let delivery: Uuid = sqlx::query_scalar(
        "SELECT id FROM gateway.webhook_delivery WHERE endpoint_id = $1 AND event_id = $2",
    )
    .bind(endpoint)
    .bind(event)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    let path = format!("/{endpoint}/deliveries/{delivery}/redeliver");
    let redeliver = |org: OrganizationId, path: String, session: (String, String), key: String| {
        let harness = &harness;
        async move {
            harness
                .send(
                    Method::POST,
                    &uri(org, &path),
                    Some(&session),
                    &[("idempotency-key", &key)],
                    None,
                )
                .await
        }
    };

    // Readers cannot redeliver; the key is required; the delivery must
    // belong to the endpoint and organization named.
    let operator = harness.user(org, "operator").await;
    let operator = harness.session(operator).await;
    let (status, _, _) = redeliver(org, path.clone(), operator, "denied".into()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = harness
        .send(Method::POST, &uri(org, &path), Some(&owner), &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for wrong in [
        format!("/{other_endpoint}/deliveries/{delivery}/redeliver"),
        format!("/{endpoint}/deliveries/{}/redeliver", Uuid::new_v4()),
    ] {
        let (status, body, _) = redeliver(org, wrong, owner.clone(), "wrong".into()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
    let (other, other_owner) = harness.owner("webhook-redeliver-other").await;
    let (status, _, _) = redeliver(other, path.clone(), other_owner, "cross-org".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(harness.delivery(endpoint, event).await.status, "parked");

    // Redelivery queues it for a fresh run without resetting the attempt
    // count, and keeps the last outcome; a replay returns the same response.
    let (status, queued, _) = redeliver(org, path.clone(), owner.clone(), "again-1".into()).await;
    assert_eq!(status, StatusCode::OK, "{queued}");
    assert_eq!(queued["id"], json!(delivery));
    assert_eq!(queued["status"], "pending");
    assert_eq!(queued["attempts"], MAX_DELIVERY_ATTEMPTS);
    assert_eq!(queued["last_status_code"], 500);
    assert!(queued["next_attempt_at"].is_string());
    let (status, replay, _) = redeliver(org, path.clone(), owner.clone(), "again-1".into()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, queued);
    let (status, body, _) = redeliver(org, path.clone(), owner.clone(), "again-2".into()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "webhook_delivery_not_parked", "{body}");

    // The stale attempt cannot record over the new run.
    harness
        .store
        .record_delivered(&stale[0], Utc::now(), 200)
        .await
        .unwrap();
    assert_eq!(harness.delivery(endpoint, event).await.status, "pending");

    // The new run backs off and parks on its own count; it can be redelivered
    // again, and is then delivered.
    let report = deliverer.deliver_due().await.unwrap();
    assert_eq!((report.claimed, report.failed), (1, 1), "{report:?}");
    let row = harness.delivery(endpoint, event).await;
    assert_eq!(
        (row.status.as_str(), row.attempts),
        ("pending", MAX_DELIVERY_ATTEMPTS + 1)
    );
    park().await;
    let (status, _, _) = redeliver(org, path.clone(), owner.clone(), "again-3".into()).await;
    assert_eq!(status, StatusCode::OK);
    listener.respond_with(StatusCode::OK);
    assert_eq!(deliverer.deliver_due().await.unwrap().delivered, 1);
    let row = harness.delivery(endpoint, event).await;
    assert_eq!(
        (row.status.as_str(), row.attempts),
        ("delivered", 2 * MAX_DELIVERY_ATTEMPTS + 1)
    );
    let (status, body, _) = redeliver(org, path.clone(), owner.clone(), "again-4".into()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let audited: Vec<(String, String, Option<Value>)> = sqlx::query_as(
        "SELECT action, outcome, after_metadata FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'webhook_delivery' AND entity_id = $2
         ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .bind(delivery)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let audited: Vec<_> = audited
        .into_iter()
        .map(|(action, outcome, after)| {
            (action, outcome, after.map(|after| after["version"].clone()))
        })
        .collect();
    assert_eq!(
        audited,
        [
            (
                "webhook_delivery.redelivered".to_owned(),
                "denied".to_owned(),
                None
            ),
            (
                "webhook_delivery.redelivered".to_owned(),
                "succeeded".to_owned(),
                Some(json!(1))
            ),
            (
                "webhook_delivery.redelivered".to_owned(),
                "succeeded".to_owned(),
                Some(json!(2))
            ),
        ]
    );

    // Retention counts from the last attempt, not from when it was created.
    sqlx::query(
        "UPDATE gateway.webhook_delivery SET created_at = now() - interval '40 days'
         WHERE id = $1",
    )
    .bind(delivery)
    .execute(&harness.pool)
    .await
    .unwrap();
    harness
        .store
        .sweep_finished(Utc::now() - chrono::Duration::days(30), 10_000)
        .await
        .unwrap();
    assert_eq!(harness.delivery(endpoint, event).await.status, "delivered");
}

#[tokio::test]
async fn the_public_only_policy_never_connects_to_private_addresses() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let listener = Listener::start().await;
    let (org, owner) = harness.owner("webhook-private").await;
    let https_loopback = listener.url.replacen("http://", "https://", 1);
    let (endpoint, _) = harness.endpoint_at(org, &owner, &https_loopback).await;
    let (plain_endpoint, _) = harness.endpoint_at(org, &owner, &listener.url).await;
    harness.defer_other_deliveries().await;
    let event = harness.emit_and_fan_out(org).await;

    let report = harness
        .deliverer(DeliveryPolicy::public_only())
        .deliver_due()
        .await
        .unwrap();
    assert_eq!(report.failed, 2, "{report:?}");
    assert!(listener.received().is_empty());
    assert_eq!(
        harness
            .delivery(endpoint, event)
            .await
            .last_error
            .as_deref(),
        Some("destination address not allowed")
    );
    assert_eq!(
        harness
            .delivery(plain_endpoint, event)
            .await
            .last_error
            .as_deref(),
        Some("endpoint url is invalid")
    );
}
