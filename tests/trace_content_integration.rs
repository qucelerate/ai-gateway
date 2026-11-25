#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for reading trace content: the permission, audit of
//! every read (refusals too), each state, and decryption, through the real
//! router.
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
use std::sync::Arc;

use chrono::Utc;
use gateway::{
    OrganizationId, UserAccountId,
    application::trace_objects::{MemoryTraceObjects, TraceObjects},
    domain::{
        auth::AuthCrypto,
        credentials::CredentialCipher,
        identity::MembershipRole,
        inference::{ChatMessage, ChatRequest, ChatRole},
        keys::ApiKeyPepper,
        trace_content::{TraceDocument, content_aad, new_content_key, seal_content},
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore,
        trace_content::{ContentState, NewTraceContent, StoredObject, TraceContentStore},
    },
    transport::trace_content::router,
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
const TEST_PEPPER: &[u8] = b"trace-content-integration-pepper";
const HMAC_KEY: &[u8] = b"trace-content-integration-hmac-032";

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
    objects: Arc<MemoryTraceObjects>,
    cipher: CredentialCipher,
    content: TraceContentStore,
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
        let objects = Arc::new(MemoryTraceObjects::default());
        let cipher = CredentialCipher::new(&[5_u8; 32]).unwrap();
        let content = TraceContentStore::new(pool.clone());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            content.clone(),
            Some(objects.clone() as Arc<dyn TraceObjects>),
            Some(cipher.clone()),
        );
        Self {
            pool,
            identity,
            crypto,
            router,
            objects,
            cipher,
            content,
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

    /// A finalized trace with its reservation; returns the trace id.
    async fn trace(&self, org: OrganizationId) -> Uuid {
        let (reservation, trace) = (Uuid::new_v4(), Uuid::new_v4());
        let at = Utc::now();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, idempotency_key, fingerprint, state, reserved_minor,
                  expires_at, output_token_cap, input_price_minor, output_price_minor,
                  cached_input_price_minor, reasoning_price_minor, other_price_minor,
                  price_version, pricing_confidence, currency, deployment, model, trace_id,
                  created_at)
             VALUES ($1, $2, $3, $4, 'settled', 500, $5, 100, 1, 1, 1, 1, 1, 'v1', 'known',
                     'USD', 'deploy-a', 'model-a', $6, $7)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(at + chrono::Duration::minutes(5))
        .bind(trace)
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, reservation_id, correlation_id, operation, status,
                  outcome, model, input_bound, output_cap, price_version, pricing_confidence,
                  currency, amount_minor, metadata, recorded_at, finalized_at)
             VALUES ($1, $2, $3, $4, 'inference', 'finalized', 'succeeded', 'model-a', 64, 100,
                     'v1', 'known', 'USD', 120,
                     jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency', 'USD'),
                     $5, $5)",
        )
        .bind(trace)
        .bind(org.0)
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        trace
    }

    /// Records `trace`'s content in `state`; stored content is sealed and
    /// written as capture writes it.
    async fn content(&self, org: OrganizationId, trace: Uuid, state: ContentState) {
        let object = if matches!(state, ContentState::Stored | ContentState::Writing) {
            let request = ChatRequest {
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: "hello".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                }],
                temperature: None,
                max_tokens: None,
                ..Default::default()
            };
            let document = TraceDocument::new(&request, None, false, 1024 * 1024);
            let key = new_content_key();
            let sealed = seal_content(
                &key,
                &content_aad(org, trace),
                &serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
            let name = format!("{}/{trace}", org.0);
            self.objects.put(&name, sealed).await.unwrap();
            let sealed_key = self
                .cipher
                .seal_trace_content_key(org, trace, &key)
                .unwrap();
            Some(StoredObject {
                name,
                key_id: sealed_key.key_id,
                sealed_key: sealed_key.ciphertext,
            })
        } else {
            None
        };
        let now = Utc::now();
        self.content
            .insert(&NewTraceContent {
                organization_id: org,
                trace_id: trace,
                state,
                object,
                request_bytes: Some(10),
                response_bytes: Some(4),
                request_truncated: false,
                response_truncated: false,
                started_at: now,
                provider_started_at: Some(now),
                first_event_at: None,
                finished_at: now,
            })
            .await
            .unwrap();
    }

    async fn audit_actions(&self, org: OrganizationId, action: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, outcome FROM gateway.audit_event
             WHERE organization_id = $1 AND action = $2 ORDER BY occurred_at, id",
        )
        .bind(org.0)
        .bind(action)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn content_uri(org: OrganizationId, trace: Uuid) -> String {
    format!(
        "/api/admin/v1/organizations/{}/traces/{trace}/content",
        org.0
    )
}

#[tokio::test]
async fn owners_and_admins_read_content_and_every_read_is_audited() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("trace-content").await;
    let other = harness.org("trace-content-other").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let operator = harness.as_role(org, "operator").await;

    let stored = harness.trace(org).await;
    harness.content(org, stored, ContentState::Stored).await;
    let response = harness
        .send(Method::GET, &content_uri(org, stored), &admin, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.headers["cache-control"], "no-store");
    let view = response.json();
    assert_eq!(view["state"], "stored");
    assert_eq!(
        view["request"],
        json!({"messages": [{"role": "user", "content": "hello"}]})
    );
    assert_eq!(view["response"], Value::Null);
    assert_eq!(
        view["truncated"],
        json!({"request": false, "response": false})
    );
    assert!(view["provider_started_at"].is_string());

    // Operators read request records, not their content.
    let response = harness
        .send(Method::GET, &content_uri(org, stored), &operator, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    // Without stored content, the state says why. A request just finished
    // whose content is not recorded yet, or still being written, is
    // pending; one long finished without a row was not traced.
    let untraced = harness.trace(org).await;
    sqlx::query(
        "UPDATE gateway.inference_trace SET finalized_at = finalized_at - interval '10 minutes'
         WHERE id = $1",
    )
    .bind(untraced)
    .execute(&harness.pool)
    .await
    .unwrap();
    let just_finished = harness.trace(org).await;
    let writing = harness.trace(org).await;
    harness.content(org, writing, ContentState::Writing).await;
    let off = harness.trace(org).await;
    harness.content(org, off, ContentState::Off).await;
    for (trace, state) in [
        (untraced, "not_traced"),
        (just_finished, "pending"),
        (writing, "pending"),
        (off, "off"),
    ] {
        let response = harness
            .send(Method::GET, &content_uri(org, trace), &admin, &[], None)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(response.json()["state"], state);
        assert_eq!(response.json()["request"], Value::Null);
    }

    // Someone outside the organization is refused, not failed: their
    // denial cannot be recorded against it, and is not required to be.
    let outsider = harness.as_role(other, "organization_owner").await;
    let response = harness
        .send(Method::GET, &content_uri(org, stored), &outsider, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);

    // Another organization's trace, or none at all.
    let foreign = harness.trace(other).await;
    for trace in [foreign, Uuid::new_v4()] {
        let response = harness
            .send(Method::GET, &content_uri(org, trace), &admin, &[], None)
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{}", response.body);
    }

    // Stored content whose object is gone is unavailable, not empty.
    let lost = harness.trace(org).await;
    harness.content(org, lost, ContentState::Stored).await;
    harness
        .objects
        .delete(&format!("{}/{lost}", org.0))
        .await
        .unwrap();
    let response = harness
        .send(Method::GET, &content_uri(org, lost), &admin, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.json()["code"], "trace_content_unavailable");

    let read = |outcome: &str| ("trace_content.read".to_owned(), outcome.to_owned());
    assert_eq!(
        harness.audit_actions(org, "trace_content.read").await,
        [
            read("succeeded"),
            read("denied"),
            read("succeeded"),
            read("succeeded"),
            read("succeeded"),
            read("succeeded"),
            read("failed"),
        ]
    );
}

#[tokio::test]
async fn content_is_withheld_when_its_read_cannot_be_audited() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("trace-content-unaudited").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let stored = harness.trace(org).await;
    harness.content(org, stored, ContentState::Stored).await;

    // Audit writes for this organization's reads fail.
    let trigger = format!("trace_content_audit_fails_{}", org.0.simple());
    sqlx::query(&format!(
        "CREATE FUNCTION gateway.{trigger}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.organization_id = '{}' AND NEW.action = 'trace_content.read' THEN
             RAISE EXCEPTION 'audit unavailable';
           END IF;
           RETURN NEW;
         END $$",
        org.0
    ))
    .execute(&harness.pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON gateway.audit_event
         FOR EACH ROW EXECUTE FUNCTION gateway.{trigger}()"
    ))
    .execute(&harness.pool)
    .await
    .unwrap();
    let response = harness
        .send(Method::GET, &content_uri(org, stored), &admin, &[], None)
        .await;
    for statement in [
        format!("DROP TRIGGER {trigger} ON gateway.audit_event"),
        format!("DROP FUNCTION gateway.{trigger}()"),
    ] {
        sqlx::query(&statement)
            .execute(&harness.pool)
            .await
            .unwrap();
    }

    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.json()["code"], "service_unavailable");
    assert!(!response.body.contains("hello"), "{}", response.body);
    assert!(
        harness
            .audit_actions(org, "trace_content.read")
            .await
            .is_empty()
    );
}
