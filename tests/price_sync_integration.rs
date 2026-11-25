#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for price sync: the worker fetching operator price
//! sheets and public price lists from a local server (versioned snapshots,
//! confirmation, backoff, the lease fence, retention), and the admin drift
//! report, observation listing and price mappings, driven through the real
//! router.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, Uri, header::COOKIE},
    response::IntoResponse,
    routing::get,
};
use chrono::{DateTime, Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        catalog::ProviderFamily,
        identity::MembershipRole,
        keys::ApiKeyPepper,
        notifications::{NewNotification, NotificationKind},
        pricing::parse_price_sheet,
    },
    jobs::{
        egress::DeliveryPolicy,
        pricing::{
            AwsPriceListSource, AzureRetailSource, PriceSheetSource, PriceSource, PricingReport,
            PricingRunner,
        },
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore,
        catalog_sync::CatalogSyncStore,
        notifications::NotificationChange,
        price_sync::{ClaimedSource, FetchOutcome, PriceSyncStore},
    },
    transport::catalog_sync::router,
};
use http_body_util::BodyExt;
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
const TEST_PEPPER: &[u8] = b"price-sync-integration-pepper";
const HMAC_KEY: &[u8] = b"price-sync-integration-hmac-00032";

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

fn capability(operation: &str) -> Value {
    json!({
        "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 32,
        "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 17,
        "chat_input_count_mode": "unsupported"
    })
}

/// A USD chat price in money units per token.
fn price(deployment: &str, input: i64, output: i64, cached_input: i64, reasoning: i64) -> Value {
    json!({
        "deployment": deployment, "operation": "chat",
        "input_minor_per_token": input, "output_minor_per_token": output,
        "cached_input_minor_per_token": cached_input, "reasoning_minor_per_token": reasoning,
        "other_minor_per_token": 0, "currency": "USD", "source": "test",
        "source_version": "v1", "confidence": "known"
    })
}

/// `gpt` deployed globally and in `eu`, `gpt-old` once, and `claude` on
/// Anthropic. $2.50 per million tokens is 250 money units per token.
fn manifest() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "pricing-1"},
        "providers": [
            {"adapter_instance_key": "openai-main", "family": "openai"},
            {"adapter_instance_key": "anthropic-main", "family": "anthropic"}
        ],
        "deployments": [
            {"name": "gpt-a", "model": "gpt", "location": null,
             "adapter_instance_key": "openai-main", "capabilities": [capability("chat")]},
            {"name": "gpt-b", "model": "gpt", "location": "eu",
             "adapter_instance_key": "openai-main", "capabilities": [capability("chat")]},
            {"name": "gpt-legacy", "model": "gpt-old", "location": null,
             "adapter_instance_key": "openai-main", "capabilities": [capability("chat")]},
            {"name": "claude-a", "model": "claude", "location": null,
             "adapter_instance_key": "anthropic-main", "capabilities": [capability("chat")]}
        ],
        "prices": [
            price("gpt-a", 250, 1000, 125, 1000),
            price("gpt-b", 250, 1000, 125, 1000),
            price("gpt-legacy", 50, 150, 0, 0),
            price("claude-a", 300, 1500, 0, 0)
        ],
        "routes": [{"alias": "fast", "operation": "chat", "deployment": "gpt-a"}]
    }))
    .unwrap()
}

fn sheet(prices: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "format": "qucelerate.price-sheet.v1",
        "version": "2026-09",
        "prices": prices
    }))
    .unwrap()
}

fn openai_sheet(gpt_input: &str) -> Vec<u8> {
    sheet(json!([
        {"model": "gpt", "currency": "USD", "per_tokens": 1000000,
         "input": gpt_input, "output": "10", "cached_input": "1.25", "reasoning": "10.00"},
        {"model": "gpt", "region": "eu", "currency": "USD", "per_tokens": 1000000,
         "input": "2.75", "output": "10", "cached_input": "1.25", "reasoning": "10"},
        {"model": "gpt-old", "currency": "EUR", "per_tokens": 1000, "input": "0.0005"}
    ]))
}

/// `body` with its `version` replaced.
fn relabel(body: &[u8], version: &str) -> Vec<u8> {
    let mut document: Value = serde_json::from_slice(body).unwrap();
    document["version"] = json!(version);
    serde_json::to_vec(&document).unwrap()
}

type Responses = Arc<Mutex<HashMap<String, (StatusCode, Vec<u8>)>>>;

/// A local server answering `/{name}`, or any other path, with whatever was
/// last set for it (the query is ignored).
struct SheetServer {
    base: String,
    responses: Responses,
}

impl SheetServer {
    async fn start() -> Self {
        async fn serve(
            State(responses): State<Responses>,
            Path(name): Path<String>,
        ) -> axum::response::Response {
            match responses.lock().unwrap().get(&name) {
                Some((status, body)) => (*status, body.clone()).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        async fn serve_path(
            State(responses): State<Responses>,
            uri: Uri,
        ) -> axum::response::Response {
            match responses.lock().unwrap().get(uri.path()) {
                Some((status, body)) => (*status, body.clone()).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        let responses = Responses::default();
        let app = Router::new()
            .route("/{name}", get(serve))
            .fallback(serve_path)
            .with_state(responses.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{address}"),
            responses,
        }
    }

    fn url(&self, name: &str) -> Url {
        Url::parse(&format!("{}/{name}", self.base)).unwrap()
    }

    fn respond(&self, name: &str, status: StatusCode, body: Vec<u8>) {
        self.responses
            .lock()
            .unwrap()
            .insert(name.to_owned(), (status, body));
    }

    fn source(&self, family: ProviderFamily, name: &str) -> Arc<dyn PriceSource> {
        Arc::new(PriceSheetSource::new(
            family,
            self.url(name),
            DeliveryPolicy::allow_private_targets_for_tests(),
        ))
    }
}

#[derive(Debug, sqlx::FromRow)]
struct SourceState {
    failures: i32,
    last_error_code: Option<String>,
    next_fetch_at: DateTime<Utc>,
    current_snapshot_id: Option<Uuid>,
    lease_token: Option<Uuid>,
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    catalog: CatalogStore,
    prices: PriceSyncStore,
    router: Router,
}

impl Harness {
    /// Sources are gateway-wide, so every test starts without any.
    async fn new(database_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        sqlx::query("DELETE FROM gateway.price_source")
            .execute(&pool)
            .await
            .unwrap();
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[37_u8; 32]).unwrap();
        let prices = PriceSyncStore::new(pool.clone());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            CatalogSyncStore::new(pool.clone()),
            prices.clone(),
        );
        Self {
            catalog: CatalogStore::new(pool.clone()),
            prices,
            pool,
            identity,
            crypto,
            router,
        }
    }

    fn runner(&self, sources: Vec<Arc<dyn PriceSource>>) -> PricingRunner {
        PricingRunner::new(self.prices.clone(), sources)
            .with_fetch_timeout(StdDuration::from_secs(10))
    }

    async fn state(&self, source: &str) -> SourceState {
        sqlx::query_as(
            "SELECT failures, last_error_code, next_fetch_at, current_snapshot_id, lease_token
             FROM gateway.price_source WHERE source = $1",
        )
        .bind(source)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Makes every source due now.
    async fn make_due(&self) {
        sqlx::query("UPDATE gateway.price_source SET next_fetch_at = now() - interval '1 second'")
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn snapshots(&self, source: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM gateway.price_snapshot WHERE source = $1")
            .bind(source)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn org(&self, slug: &str) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id
    }

    async fn org_with_release(&self, slug: &str) -> (OrganizationId, Uuid) {
        let org = self.org(slug).await;
        let manifest = manifest();
        let release = self
            .catalog
            .apply(org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap()
            .id;
        (org, release)
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

    async fn get(&self, uri: &str, session: Option<&(String, String)>) -> (StatusCode, Value) {
        let (status, _, body) = self.send(Method::GET, uri, session, &[], None).await;
        (status, body)
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        session: Option<&(String, String)>,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
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
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/catalog/{path}", org.0)
}

fn report(changed: u64, unchanged: u64, failed: u64) -> PricingReport {
    PricingReport {
        changed,
        unchanged,
        failed,
    }
}

/// Asserts `at` lies within a few seconds of `now + offset`.
fn assert_near(at: DateTime<Utc>, offset: Duration) {
    let expected = Utc::now() + offset;
    assert!(
        (at - expected).num_seconds().abs() <= 10,
        "{at} is not near {expected}"
    );
}

#[tokio::test]
async fn price_sheets_are_versioned_confirmed_and_swept() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    let runner = harness.runner(vec![server.source(ProviderFamily::OpenAi, "openai.json")]);

    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    let first = harness.state("price-sheet:openai").await;
    let snapshot = first.current_snapshot_id.unwrap();
    assert_eq!(first.last_error_code, None);
    assert_eq!(first.lease_token, None);
    assert_near(first.next_fetch_at, Duration::hours(24));

    // Not due again until tomorrow.
    assert_eq!(runner.run_once().await.unwrap(), report(0, 0, 0));

    // The same prices, spelled differently, confirm the current snapshot.
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.5000"));
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(0, 1, 0));
    assert_eq!(
        harness
            .state("price-sheet:openai")
            .await
            .current_snapshot_id,
        Some(snapshot)
    );
    let (fetched, confirmed): (DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("SELECT fetched_at, confirmed_at FROM gateway.price_snapshot WHERE id = $1")
            .bind(snapshot)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert!(confirmed > fetched);

    // A new version label alone is not a new version of the prices.
    server.respond(
        "openai.json",
        StatusCode::OK,
        relabel(&openai_sheet("2.50"), "2026-10"),
    );
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(0, 1, 0));
    let label: Option<String> =
        sqlx::query_scalar("SELECT source_version FROM gateway.price_snapshot WHERE id = $1")
            .bind(snapshot)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(label.as_deref(), Some("2026-10"));
    assert_eq!(harness.snapshots("price-sheet:openai").await, 1);

    // Changed prices start a new version and keep the old one.
    server.respond("openai.json", StatusCode::OK, openai_sheet("3"));
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    let current = harness
        .state("price-sheet:openai")
        .await
        .current_snapshot_id
        .unwrap();
    assert_ne!(current, snapshot);
    assert_eq!(harness.snapshots("price-sheet:openai").await, 2);
    let stored: Vec<(String, Option<String>, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT item, region, currency::TEXT, per_tokens, input::TEXT
         FROM gateway.price_observation WHERE snapshot_id = $1 ORDER BY ordinal",
    )
    .bind(current)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 3);
    assert!(stored.contains(&(
        "gpt".into(),
        None,
        "USD".into(),
        1_000_000,
        Some("3.000000000000".into())
    )));

    let (status, sources) = {
        let org = harness.org("pricing-sources").await;
        let reader = harness.user(org, "read_only_member").await;
        let session = harness.session(reader).await;
        harness
            .get(&uri(org, "price-sources"), Some(&session))
            .await
    };
    assert_eq!(status, StatusCode::OK, "{sources}");
    let items = sources["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], "price-sheet:openai");
    assert_eq!(items[0]["kind"], "price-sheet");
    assert_eq!(items[0]["provider_family"], "openai");
    assert_eq!(items[0]["snapshot"]["id"], current.to_string());
    assert_eq!(items[0]["snapshot"]["source_version"], "2026-09");
    assert_eq!(items[0]["snapshot"]["observations"], 3);
    assert_eq!(items[0]["snapshot"]["unmapped"], 0);
    // The configured URL may carry a token and is never returned.
    assert!(!sources.to_string().contains(&server.base));

    // Only superseded snapshots are swept.
    let cutoff = Utc::now() + Duration::days(1);
    assert_eq!(harness.prices.sweep_snapshots(cutoff, 10).await.unwrap(), 1);
    assert_eq!(harness.snapshots("price-sheet:openai").await, 1);
    assert_eq!(harness.prices.sweep_snapshots(cutoff, 10).await.unwrap(), 0);
    let orphaned: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.price_observation WHERE snapshot_id = $1")
            .bind(snapshot)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(orphaned, 0);

    // A model's batch price is saved beside its on-demand one.
    server.respond(
        "openai.json",
        StatusCode::OK,
        sheet(json!([
            {"model": "gpt", "currency": "USD", "per_tokens": 1000000,
             "input": "2.50", "output": "10"},
            {"model": "gpt", "batch": true, "currency": "USD", "per_tokens": 1000000,
             "input": "1.25", "output": "5"}
        ])),
    );
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    let current = harness
        .state("price-sheet:openai")
        .await
        .current_snapshot_id
        .unwrap();
    let stored: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT item, model, input::TEXT FROM gateway.price_observation
         WHERE snapshot_id = $1 ORDER BY item",
    )
    .bind(current)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        [
            (
                "gpt".into(),
                Some("gpt".into()),
                Some("2.500000000000".into())
            ),
            (
                "gpt · batch".into(),
                Some("gpt".into()),
                Some("1.250000000000".into())
            ),
        ]
    );
}

#[tokio::test]
async fn failed_fetches_back_off_then_wait_for_the_next_day() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    let runner = harness.runner(vec![server.source(ProviderFamily::OpenAi, "openai.json")]);
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    let snapshot = harness
        .state("price-sheet:openai")
        .await
        .current_snapshot_id;

    let failures = [
        (
            StatusCode::SERVICE_UNAVAILABLE,
            b"upstream secret detail".to_vec(),
            "http_status",
            1,
            Duration::minutes(15),
        ),
        (
            StatusCode::OK,
            b"{\"format\": \"something-else\"}".to_vec(),
            "invalid_price_sheet",
            2,
            Duration::minutes(30),
        ),
        (
            StatusCode::OK,
            vec![b' '; 8 * 1024 * 1024 + 1],
            "response_too_large",
            0,
            Duration::hours(24),
        ),
    ];
    for (status, body, code, count, delay) in failures {
        server.respond("openai.json", status, body);
        harness.make_due().await;
        assert_eq!(runner.run_once().await.unwrap(), report(0, 0, 1), "{code}");
        let state = harness.state("price-sheet:openai").await;
        assert_eq!(state.last_error_code.as_deref(), Some(code));
        assert_eq!(state.failures, count, "{code}");
        assert_near(state.next_fetch_at, delay);
        assert_eq!(state.lease_token, None);
        // A failure keeps the last good prices.
        assert_eq!(state.current_snapshot_id, snapshot);
    }

    // A success clears the error.
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(0, 1, 0));
    let state = harness.state("price-sheet:openai").await;
    assert_eq!((state.failures, state.last_error_code), (0, None));

    // Real sources may only use HTTPS to public addresses.
    let public_only: Arc<dyn PriceSource> = Arc::new(PriceSheetSource::new(
        ProviderFamily::Anthropic,
        server.url("openai.json"),
        DeliveryPolicy::public_only(),
    ));
    let runner = harness.runner(vec![public_only]);
    assert_eq!(runner.run_once().await.unwrap(), report(0, 0, 1));
    let state = harness.state("price-sheet:anthropic").await;
    assert_eq!(state.last_error_code.as_deref(), Some("invalid_url"));
    assert_eq!(state.current_snapshot_id, None);
}

/// A failing source raises a warning in every organization that uses the
/// catalog, once however often it fails, resolved when it fetches again.
#[tokio::test]
async fn a_failing_source_is_notified_until_it_fetches_again() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let (org, _) = harness.org_with_release("price-notify").await;
    let without_catalog = harness.org("price-notify-none").await;
    let server = SheetServer::start().await;
    let runner = harness
        .runner(vec![server.source(ProviderFamily::OpenAi, "openai.json")])
        .with_notifications();
    let open = |organization: OrganizationId| {
        let pool = harness.pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<DateTime<Utc>>, Value)>(
                "SELECT severity, resolved_at, data FROM gateway.notification
                 WHERE organization_id = $1 AND kind = 'price_source.failing'
                   AND subject = 'price-sheet:openai'
                 ORDER BY created_at",
            )
            .bind(organization.0)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    for _ in 0..2 {
        server.respond(
            "openai.json",
            StatusCode::SERVICE_UNAVAILABLE,
            b"down".to_vec(),
        );
        harness.make_due().await;
        assert_eq!(runner.run_once().await.unwrap(), report(0, 0, 1));
    }
    let raised = open(org).await;
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].0, "warning");
    assert_eq!(raised[0].1, None);
    assert_eq!(raised[0].2["error_code"], "http_status");
    assert!(open(without_catalog).await.is_empty());

    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    let resolved = open(org).await;
    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].1.is_some());

    // An older fetch whose lease was lost records nothing, so its failure
    // cannot reopen what the newer fetch resolved.
    let stale = ClaimedSource {
        source: "price-sheet:openai".into(),
        lease_token: uuid::Uuid::new_v4(),
    };
    let raise = NotificationChange::RaiseForCatalogOrganizations(NewNotification::new(
        NotificationKind::PriceSourceFailing,
        "price-sheet:openai",
        "Price source price-sheet:openai is failing",
        "",
    ));
    let now = Utc::now();
    assert!(
        !harness
            .prices
            .record_failure(
                &stale,
                "timed_out",
                now,
                Duration::minutes(15),
                now + Duration::hours(24),
                Some(&raise),
            )
            .await
            .unwrap()
    );
    assert!(open(org).await.iter().all(|row| row.1.is_some()));

    // Failing again, then retired (no worker configures it any more): the
    // warning is resolved, since nothing fetches the source again.
    server.respond(
        "openai.json",
        StatusCode::SERVICE_UNAVAILABLE,
        b"down".to_vec(),
    );
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(0, 0, 1));
    assert!(open(org).await.iter().any(|row| row.1.is_none()));
    let later = Utc::now() + Duration::seconds(1);
    harness.prices.register(&[], later, later).await.unwrap();
    assert!(open(org).await.iter().all(|row| row.1.is_some()));
}

#[tokio::test]
async fn a_lost_lease_records_nothing() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let store = &harness.prices;
    let id = "price-sheet:openai".to_owned();
    let now = Utc::now();
    store
        .register(
            &[(id.clone(), "price-sheet", ProviderFamily::OpenAi)],
            now,
            now - Duration::hours(1),
        )
        .await
        .unwrap();
    let ids = [id.clone()];
    let stale = store
        .claim(&ids, now, now + Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    // A live lease is not claimed twice.
    assert_eq!(
        store
            .claim(&ids, now, now + Duration::minutes(5))
            .await
            .unwrap(),
        None
    );
    // Once it expires another worker takes over.
    let later = now + Duration::minutes(6);
    let fresh = store
        .claim(&ids, later, later + Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(stale.lease_token, fresh.lease_token);

    let set = parse_price_sheet(&openai_sheet("2.50")).unwrap();
    let next = later + Duration::hours(24);
    assert_eq!(
        store
            .record_success(&stale, &set, later, next)
            .await
            .unwrap(),
        None
    );
    assert!(
        !store
            .record_failure(
                &stale,
                "timed_out",
                later,
                Duration::minutes(15),
                next,
                None
            )
            .await
            .unwrap()
    );
    let state = harness.state(&id).await;
    assert_eq!(state.lease_token, Some(fresh.lease_token));
    assert_eq!((state.failures, state.last_error_code), (0, None));
    assert_eq!(harness.snapshots(&id).await, 0);

    let outcome = store
        .record_success(&fresh, &set, later, next)
        .await
        .unwrap()
        .unwrap();
    let FetchOutcome::Changed(snapshot) = outcome else {
        panic!("first fetch must be a new snapshot: {outcome:?}");
    };
    assert_eq!(harness.state(&id).await.current_snapshot_id, Some(snapshot));
    // The fence holds after completion too.
    assert_eq!(
        store
            .record_success(&fresh, &set, later, next)
            .await
            .unwrap(),
        None
    );
}

fn entry<'a>(drift: &'a Value, deployment: &str) -> &'a Value {
    drift["prices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["deployment"] == deployment)
        .unwrap_or_else(|| panic!("no drift entry for {deployment}: {drift}"))
}

fn dimension<'a>(comparison: &'a Value, name: &str) -> &'a Value {
    comparison["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|dimension| dimension["dimension"] == name)
        .unwrap()
}

#[tokio::test]
async fn drift_compares_billed_prices_with_each_source() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    server.respond(
        "anthropic.json",
        StatusCode::OK,
        sheet(json!([
            {"model": "claude", "currency": "USD", "per_tokens": 1000000,
             "input": "3", "output": "15"}
        ])),
    );
    let (org, release) = harness.org_with_release("pricing-drift").await;
    let reader = harness.user(org, "read_only_member").await;
    let reader = harness.session(reader).await;

    // Before any fetch, every price has no comparison.
    let (status, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    assert_eq!(status, StatusCode::OK, "{drift}");
    assert_eq!(drift["release_id"], release.to_string());
    assert_eq!(drift["prices"].as_array().unwrap().len(), 4);
    assert_eq!(entry(&drift, "gpt-a")["comparisons"], json!([]));

    let runner = harness.runner(vec![
        server.source(ProviderFamily::OpenAi, "openai.json"),
        server.source(ProviderFamily::Anthropic, "anthropic.json"),
    ]);
    assert_eq!(runner.run_once().await.unwrap(), report(2, 0, 0));

    // The billed price for claude-a is an override.
    let override_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.catalog_price_override
             (id, organization_id, deployment, operation, input_minor_per_token,
              output_minor_per_token, cached_input_minor_per_token, reasoning_minor_per_token,
              other_minor_per_token, currency, confidence, version, reason, effective_from)
         VALUES ($1, $2, 'claude-a', 'chat', 350, 1500, 0, 0, 0, 'USD', 'known', $3,
                 'negotiated', now() - interval '1 minute')",
    )
    .bind(override_id)
    .bind(org.0)
    .bind(format!("override:{override_id}"))
    .execute(&harness.pool)
    .await
    .unwrap();

    let (status, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    assert_eq!(status, StatusCode::OK, "{drift}");
    assert_eq!(drift["sources"].as_array().unwrap().len(), 2);

    // The global price matches; `other` is not published.
    let gpt_a = entry(&drift, "gpt-a");
    assert_eq!(gpt_a["price_source"], "release");
    let comparisons = gpt_a["comparisons"].as_array().unwrap();
    assert_eq!(comparisons.len(), 1, "only the family's own source");
    let comparison = &comparisons[0];
    assert_eq!(comparison["source"], "price-sheet:openai");
    assert_eq!(comparison["status"], "matches");
    assert_eq!(comparison["observed_region"], Value::Null);
    assert_eq!(comparison["per_tokens"], 1_000_000);
    let input = dimension(comparison, "input");
    assert_eq!(
        (&input["catalog"], &input["observed"], &input["published"]),
        (&json!(250), &json!("250"), &json!("2.5"))
    );
    assert_eq!(dimension(comparison, "reasoning")["status"], "matches");
    assert_eq!(dimension(comparison, "other")["status"], "unpublished");

    // The regional price wins for the eu deployment.
    let comparison = &entry(&drift, "gpt-b")["comparisons"][0];
    assert_eq!(comparison["status"], "differs");
    assert_eq!(comparison["observed_region"], "eu");
    let input = dimension(comparison, "input");
    assert_eq!(
        (&input["status"], &input["observed"]),
        (&json!("differs"), &json!("275"))
    );
    assert_eq!(dimension(comparison, "output")["status"], "matches");

    // No conversion between currencies.
    let comparison = &entry(&drift, "gpt-legacy")["comparisons"][0];
    assert_eq!(comparison["status"], "currency_mismatch");
    assert_eq!(comparison["observed_currencies"], json!(["EUR"]));
    assert_eq!(comparison["dimensions"], json!([]));

    // The override is what is compared.
    let claude = entry(&drift, "claude-a");
    assert_eq!(claude["price_source"], "override");
    let comparison = &claude["comparisons"][0];
    assert_eq!(comparison["source"], "price-sheet:anthropic");
    assert_eq!(comparison["status"], "differs");
    let input = dimension(comparison, "input");
    assert_eq!(
        (&input["catalog"], &input["observed"], &input["status"]),
        (&json!(350), &json!("300"), &json!("differs"))
    );
    assert_eq!(dimension(comparison, "output")["status"], "matches");
    // Anthropic reports cache reads (migration 0144), so a sheet without a
    // cached price leaves it unpublished; it reports no reasoning.
    assert_eq!(
        dimension(comparison, "cached_input")["status"],
        "unpublished"
    );
    assert_eq!(dimension(comparison, "reasoning")["status"], "not_reported");
    assert!(!drift.to_string().contains(&server.base));

    // Other readers, and who may not read.
    for role in ["auditor", "operator", "organization_admin"] {
        let user = harness.user(org, role).await;
        let session = harness.session(user).await;
        for path in ["price-drift", "price-sources"] {
            let (status, body) = harness.get(&uri(org, path), Some(&session)).await;
            assert_eq!(status, StatusCode::OK, "{role} {path}: {body}");
        }
    }
    let (status, _) = harness.get(&uri(org, "price-drift"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let other = harness.org("pricing-other").await;
    let outsider = harness.user(other, "organization_owner").await;
    let outsider = harness.session(outsider).await;
    for path in ["price-drift", "price-sources"] {
        let (status, _) = harness.get(&uri(org, path), Some(&outsider)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }

    // Without an active release there is nothing to compare.
    let (status, body) = harness
        .get(&uri(other, "price-drift"), Some(&outsider))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn unconfigured_sources_are_retired_until_configured_again() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    server.respond(
        "anthropic.json",
        StatusCode::OK,
        sheet(json!([
            {"model": "claude", "currency": "USD", "per_tokens": 1000000, "input": "3"}
        ])),
    );
    let openai = server.source(ProviderFamily::OpenAi, "openai.json");
    let anthropic = server.source(ProviderFamily::Anthropic, "anthropic.json");
    let both = harness.runner(vec![openai.clone(), anthropic.clone()]);
    assert_eq!(both.run_once().await.unwrap(), report(2, 0, 0));
    let (org, _) = harness.org_with_release("pricing-retired").await;
    let reader = harness.user(org, "read_only_member").await;
    let reader = harness.session(reader).await;
    let listed = || async {
        let (status, sources) = harness.get(&uri(org, "price-sources"), Some(&reader)).await;
        assert_eq!(status, StatusCode::OK, "{sources}");
        sources["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["source"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    // A worker without Anthropic's sheet keeps a source another worker
    // registered recently, as during a rolling change.
    let openai_only = harness.runner(vec![openai.clone()]);
    harness.make_due().await;
    assert_eq!(openai_only.run_once().await.unwrap(), report(0, 1, 0));
    assert_eq!(
        listed().await,
        ["price-sheet:anthropic", "price-sheet:openai"]
    );

    // Once no worker has registered it for an hour, it is retired: not
    // claimed while leased, and then not fetched, listed or compared.
    let stale = |source: &'static str| {
        sqlx::query(
            "UPDATE gateway.price_source SET registered_at = now() - interval '2 hours'
             WHERE source = $1",
        )
        .bind(source)
        .execute(&harness.pool)
    };
    stale("price-sheet:anthropic").await.unwrap();
    sqlx::query(
        "UPDATE gateway.price_source
         SET lease_token = gen_random_uuid(), lease_expires_at = now() + interval '1 minute'
         WHERE source = 'price-sheet:anthropic'",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    openai_only.run_once().await.unwrap();
    assert_eq!(
        listed().await,
        ["price-sheet:anthropic", "price-sheet:openai"]
    );
    sqlx::query(
        "UPDATE gateway.price_source SET lease_token = NULL, lease_expires_at = NULL
         WHERE source = 'price-sheet:anthropic'",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    openai_only.run_once().await.unwrap();
    assert_eq!(listed().await, ["price-sheet:openai"]);
    let (_, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    assert_eq!(drift["sources"].as_array().unwrap().len(), 1);
    assert_eq!(entry(&drift, "claude-a")["comparisons"], json!([]));

    // Its last snapshot ages out like any superseded one.
    let cutoff = Utc::now() + Duration::days(1);
    assert_eq!(harness.prices.sweep_snapshots(cutoff, 10).await.unwrap(), 1);
    assert_eq!(harness.snapshots("price-sheet:anthropic").await, 0);
    assert_eq!(harness.snapshots("price-sheet:openai").await, 1);
    let retired = harness.state("price-sheet:anthropic").await;
    assert_eq!(
        (retired.current_snapshot_id, retired.lease_token),
        (None, None)
    );

    // With no sources configured anywhere, everything is retired.
    stale("price-sheet:openai").await.unwrap();
    harness.runner(Vec::new()).run_once().await.unwrap();
    assert!(listed().await.is_empty());

    // Configured again, a source is due at once and shows no prices from
    // before its retirement until a fetch succeeds.
    server.respond("openai.json", StatusCode::BAD_GATEWAY, Vec::new());
    assert_eq!(both.run_once().await.unwrap(), report(1, 0, 1));
    let (_, sources) = harness.get(&uri(org, "price-sources"), Some(&reader)).await;
    let items = sources["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["source"], "price-sheet:openai");
    assert_eq!(items[1]["snapshot"], Value::Null);
    assert_eq!(items[1]["last_error_code"], "http_status");
    assert_ne!(items[0]["snapshot"], Value::Null);
}

const AWS_INDEX: &str = "/offers/v1.0/aws/AmazonBedrock/current/region_index.json";
const AWS_OFFER: &str = "/offers/v1.0/aws/AmazonBedrock/20260901/us-east-1/index.json";
const AWS_FM_INDEX: &str =
    "/offers/v1.0/aws/AmazonBedrockFoundationModels/current/region_index.json";
const AWS_FM_OFFER: &str =
    "/offers/v1.0/aws/AmazonBedrockFoundationModels/20261012/us-east-1/index.json";

/// Claude Sonnet 4.6's on-demand prices per million tokens, in the region
/// and globally.
fn aws_foundation_model_offer() -> Vec<u8> {
    let mut offer = json!({"products": {}, "terms": {"OnDemand": {}}});
    for (sku, usage_type, inference, price) in [
        (
            "A",
            "USE1-MP:USE1_InputTokenCount-Units",
            "Input tokens",
            "3.3000000000",
        ),
        (
            "B",
            "USE1-MP:USE1_OutputTokenCount-Units",
            "Output tokens",
            "16.5000000000",
        ),
        (
            "C",
            "USE1-MP:USE1_CacheReadInputTokenCount-Units",
            "Prompt cache read input tokens",
            "0.3300000000",
        ),
        (
            "D",
            "USE1-MP:USE1_CacheWriteInputTokenCount-Units",
            "Prompt cache write input tokens",
            "4.1250000000",
        ),
        (
            "E",
            "USE1-MP:USE1_InputTokenCount_Global-Units",
            "Input tokens",
            "3.0000000000",
        ),
        (
            "F",
            "USE1-MP:USE1_OutputTokenCount_Global-Units",
            "Output tokens",
            "15.0000000000",
        ),
    ] {
        offer["products"][sku] = json!({"attributes": {
            "usagetype": usage_type, "inferenceType": inference, "regionCode": "us-east-1",
            "feature": "On-demand Inference", "provider": "Anthropic", "model": "Claude Sonnet 4.6"
        }});
        offer["terms"]["OnDemand"][sku] = json!({"T": {
            "effectiveDate": "2026-10-01T00:00:00Z",
            "priceDimensions": {"P": {
                "unit": "1M tokens", "beginRange": "0", "endRange": "Inf",
                "pricePerUnit": {"USD": price}
            }}
        }});
    }
    serde_json::to_vec(&offer).unwrap()
}

fn azure_meter(name: &str, price: f64, unit: &str, region: &str) -> Value {
    json!({
        "currencyCode": "USD", "tierMinimumUnits": 0.0, "retailPrice": price,
        "armRegionName": region, "meterName": name, "unitOfMeasure": unit,
        "type": "Consumption", "effectiveStartDate": "2026-01-01T00:00:00Z"
    })
}

fn aws_offer() -> Vec<u8> {
    aws_offer_priced("0.0008000000")
}

/// NovaPro's on-demand prices, with this input price per thousand tokens.
fn aws_offer_priced(input: &str) -> Vec<u8> {
    let product = |usage_type: &str, inference: &str, price: &str| {
        (
            json!({"attributes": {
                "usagetype": usage_type, "inferenceType": inference,
                "regionCode": "us-east-1", "feature": "On-demand Inference"
            }}),
            json!({"T": {
                "effectiveDate": "2026-09-01T00:00:00Z",
                "priceDimensions": {"P": {
                    "unit": "1K tokens", "beginRange": "0", "endRange": "Inf",
                    "pricePerUnit": {"USD": price}
                }}
            }}),
        )
    };
    let mut offer = json!({"products": {}, "terms": {"OnDemand": {}}});
    for (sku, usage_type, inference, price) in [
        ("A", "USE1-NovaPro-input-tokens", "Input tokens", input),
        (
            "B",
            "USE1-NovaPro-output-tokens",
            "Output tokens",
            "0.0032000000",
        ),
        (
            "C",
            "USE1-NovaPro-input-tokens-batch",
            "Input tokens",
            "0.0004000000",
        ),
    ] {
        let (product, term) = product(usage_type, inference, price);
        offer["products"][sku] = product;
        offer["terms"]["OnDemand"][sku] = term;
    }
    serde_json::to_vec(&offer).unwrap()
}

/// Azure OpenAI and Bedrock deployments; neither list names their models.
fn public_manifest() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "public-1"},
        "providers": [
            {"adapter_instance_key": "azure-main", "family": "azure-openai"},
            {"adapter_instance_key": "bedrock-main", "family": "bedrock"}
        ],
        "deployments": [
            {"name": "gpt4o-eus", "model": "gpt-4o", "location": "eastus",
             "adapter_instance_key": "azure-main", "capabilities": [capability("chat")]},
            {"name": "nova", "model": "amazon.nova-pro-v1:0", "location": "us-east-1",
             "adapter_instance_key": "bedrock-main", "capabilities": [capability("chat")]}
        ],
        "prices": [price("gpt4o-eus", 250, 1000, 125, 0), price("nova", 80, 320, 0, 0)],
        "routes": [{"alias": "fast", "operation": "chat", "deployment": "gpt4o-eus"}]
    }))
    .unwrap()
}

impl SheetServer {
    /// Two pages of Azure OpenAI meters and one Bedrock region.
    fn publish_public_lists(&self) {
        self.respond(
            "/api/retail/prices",
            StatusCode::OK,
            serde_json::to_vec(&json!({
                "Items": [
                    azure_meter("gpt 4o 0806 Inp glbl Tokens", 0.0025, "1K", "eastus"),
                    azure_meter("gpt 4o 0806 Outp glbl Tokens", 0.01, "1K", "eastus"),
                    azure_meter("gpt 4o 0806 Inp Batch glbl Tokens", 0.00125, "1K", "eastus"),
                ],
                "NextPageLink": format!("{}/api/retail/prices/next?$skip=3", self.base)
            }))
            .unwrap(),
        );
        self.respond(
            "/api/retail/prices/next",
            StatusCode::OK,
            serde_json::to_vec(&json!({
                "Items": [
                    azure_meter("gpt 4o 0806 cached Inp glbl Tokens", 1.25, "1M", "eastus"),
                    azure_meter("gpt 4o 0806 Inp glbl Tokens", 0.00275, "1K", "westeurope"),
                    azure_meter("gpt 4o 0806 Inp regnl Tokens", 0.00275, "1K", "eastus"),
                ],
                "NextPageLink": null
            }))
            .unwrap(),
        );
        self.respond(
            AWS_INDEX,
            StatusCode::OK,
            serde_json::to_vec(&json!({
                "publicationDate": "2026-09-01T00:00:00Z",
                "regions": {"us-east-1": {"regionCode": "us-east-1", "currentVersionUrl": AWS_OFFER}}
            }))
            .unwrap(),
        );
        self.respond(AWS_OFFER, StatusCode::OK, aws_offer());
        self.respond(
            AWS_FM_INDEX,
            StatusCode::OK,
            serde_json::to_vec(&json!({
                "publicationDate": "2026-10-12T00:00:00Z",
                "regions": {"us-east-1": {"regionCode": "us-east-1", "currentVersionUrl": AWS_FM_OFFER}}
            }))
            .unwrap(),
        );
        self.respond(AWS_FM_OFFER, StatusCode::OK, aws_foundation_model_offer());
    }

    fn public_sources(&self) -> Vec<Arc<dyn PriceSource>> {
        let policy = DeliveryPolicy::allow_private_targets_for_tests();
        vec![
            Arc::new(AzureRetailSource::new(
                self.url("api/retail/prices"),
                vec!["USD".into()],
                policy,
            )),
            Arc::new(AwsPriceListSource::new(
                Url::parse(&self.base).unwrap(),
                policy,
            )),
        ]
    }
}

#[tokio::test]
async fn public_price_lists_are_listed_unmapped() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.publish_public_lists();
    let runner = harness.runner(server.public_sources());
    assert_eq!(runner.run_once().await.unwrap(), report(2, 0, 0));

    let org = harness.org("pricing-public").await;
    let reader = harness.user(org, "read_only_member").await;
    let reader = harness.session(reader).await;
    let (status, sources) = harness.get(&uri(org, "price-sources"), Some(&reader)).await;
    assert_eq!(status, StatusCode::OK, "{sources}");
    let items = sources["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["source"], "aws-price-list:bedrock");
    assert_eq!(items[0]["kind"], "aws-price-list");
    // Both offers' publication dates, Amazon's models' first.
    assert_eq!(
        items[0]["snapshot"]["source_version"],
        "2026-09-01T00:00:00Z + 2026-10-12T00:00:00Z"
    );
    // NovaPro on demand and in batches, and Claude Sonnet 4.6 in the region
    // and globally.
    assert_eq!(items[0]["snapshot"]["observations"], 4);
    assert_eq!(items[0]["snapshot"]["unmapped"], 4);
    assert_eq!(items[1]["source"], "azure-retail:azure-openai");
    assert_eq!(items[1]["snapshot"]["observations"], 4);
    assert_eq!(items[1]["snapshot"]["unmapped"], 4);

    // Every item, across both pages, restated per million tokens.
    let path = uri(org, "price-observations");
    let (status, listed) = harness
        .get(
            &format!("{path}?source=azure-retail:azure-openai&q=4O%200806&region=eastus"),
            Some(&reader),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "{listed}");
    let global = items
        .iter()
        .find(|item| item["item"] == "gpt 4o 0806 · global")
        .unwrap();
    // The batch meter is the item's batch price, not dropped.
    let batch = items
        .iter()
        .find(|item| item["item"] == "gpt 4o 0806 · global · batch")
        .unwrap();
    assert_eq!(batch["input"], "1.25");
    assert_eq!(global["model"], Value::Null);
    assert_eq!(global["region"], "eastus");
    assert_eq!(global["currency"], "USD");
    assert_eq!(global["per_tokens"], 1_000_000);
    assert_eq!(
        (&global["input"], &global["output"], &global["cached_input"]),
        (&json!("2.5"), &json!("10"), &json!("1.25"))
    );
    assert!(listed["next_page_token"].is_null());

    // Pages follow the source and its order.
    let mut seen = Vec::new();
    let mut next = format!("{path}?page_size=2");
    loop {
        let (status, page) = harness.get(&next, Some(&reader)).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for item in page["items"].as_array().unwrap() {
            seen.push(format!("{} {}", item["source"], item["item"]));
        }
        let Some(token) = page["next_page_token"].as_str() else {
            break;
        };
        next = format!("{path}?page_size=2&page_token={token}");
    }
    assert_eq!(seen.len(), 8, "{seen:?}");
    assert_eq!(
        seen[..3],
        [
            "\"aws-price-list:bedrock\" \"Claude Sonnet 4.6\"",
            "\"aws-price-list:bedrock\" \"Claude Sonnet 4.6 · global\"",
            "\"aws-price-list:bedrock\" \"NovaPro\"",
        ]
    );

    // A partner model's item names it as Bedrock lists it, with its cache
    // writes as other usage.
    let (status, listed) = harness
        .get(
            &format!("{path}?source=aws-price-list:bedrock&q=sonnet"),
            Some(&reader),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let sonnet = &listed["items"][0];
    assert_eq!(sonnet["item"], "Claude Sonnet 4.6");
    assert_eq!(sonnet["region"], "us-east-1");
    assert_eq!(
        (
            &sonnet["input"],
            &sonnet["output"],
            &sonnet["cached_input"],
            &sonnet["other"]
        ),
        (
            &json!("3.3"),
            &json!("16.5"),
            &json!("0.33"),
            &json!("4.125")
        )
    );
    // A token only continues the query it was issued for.
    let (_, first) = harness
        .get(&format!("{path}?page_size=1"), Some(&reader))
        .await;
    let token = first["next_page_token"].as_str().unwrap();
    let (status, _) = harness
        .get(
            &format!("{path}?page_size=1&page_token={token}&region=eastus"),
            Some(&reader),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for query in ["unknown=1", "source=", "region=%00"] {
        let (status, body) = harness.get(&format!("{path}?{query}"), Some(&reader)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }

    // Unchanged lists confirm their snapshots, and a page token stays valid.
    let (_, first) = harness
        .get(&format!("{path}?page_size=1"), Some(&reader))
        .await;
    let token = first["next_page_token"].as_str().unwrap().to_owned();
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(0, 2, 0));
    let next = format!("{path}?page_size=1&page_token={token}");
    let (status, _) = harness.get(&next, Some(&reader)).await;
    assert_eq!(status, StatusCode::OK);
    // Once the source it ended in publishes new prices, it does not.
    server.respond(AWS_OFFER, StatusCode::OK, aws_offer_priced("0.0009000000"));
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(1, 1, 0));
    let (status, problem) = harness.get(&next, Some(&reader)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "page_token_stale");

    // Links and offers are only followed within the configured origin and
    // offer path.
    server.respond(
        "/api/retail/prices",
        StatusCode::OK,
        serde_json::to_vec(&json!({
            "Items": [], "NextPageLink": "https://elsewhere.example/api/retail/prices?$skip=1"
        }))
        .unwrap(),
    );
    server.respond(
        AWS_INDEX,
        StatusCode::OK,
        serde_json::to_vec(&json!({"regions": {"r": {
            "regionCode": "r", "currentVersionUrl": "https://elsewhere.example/offers/v1.0/aws/AmazonBedrock/r/index.json"
        }}}))
        .unwrap(),
    );
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(0, 0, 2));
    for source in ["azure-retail:azure-openai", "aws-price-list:bedrock"] {
        let state = harness.state(source).await;
        assert_eq!(
            state.last_error_code.as_deref(),
            Some("invalid_price_list"),
            "{source}"
        );
        assert!(state.current_snapshot_id.is_some());
    }
}

#[tokio::test]
async fn price_mappings_select_the_published_item_for_drift() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.publish_public_lists();
    harness
        .runner(server.public_sources())
        .run_once()
        .await
        .unwrap();
    let org = harness.org("pricing-mappings").await;
    let manifest = public_manifest();
    harness
        .catalog
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    let admin = harness.user(org, "organization_admin").await;
    let admin = harness.session(admin).await;
    let reader = harness.user(org, "read_only_member").await;
    let reader = harness.session(reader).await;
    let mapping =
        |deployment: &str, source: &str| uri(org, &format!("price-mappings/{deployment}/{source}"));
    let put = |path: String, key: &'static str, if_match: &'static str, body: Value| {
        let session = admin.clone();
        let harness = &harness;
        async move {
            harness
                .send(
                    Method::PUT,
                    &path,
                    Some(&session),
                    &[("idempotency-key", key), ("if-match", if_match)],
                    Some(body),
                )
                .await
        }
    };

    // Unmapped, neither list names the deployment's model.
    let (_, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    let comparison = &entry(&drift, "gpt4o-eus")["comparisons"][0];
    assert_eq!(comparison["source"], "azure-retail:azure-openai");
    assert_eq!(comparison["status"], "no_observation");
    assert_eq!(comparison["matched_by"], "model");

    // Readers may list mappings but not write them.
    let (status, listed) = harness
        .get(&uri(org, "price-mappings"), Some(&reader))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["items"], json!([]));
    let (status, _, _) = harness
        .send(
            Method::PUT,
            &mapping("gpt4o-eus", "azure-retail:azure-openai"),
            Some(&reader),
            &[("idempotency-key", "reader-1"), ("if-match", "*")],
            Some(json!({"item": "gpt 4o 0806 · global"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let body = json!({"item": "gpt 4o 0806 · global"});
    let azure = mapping("gpt4o-eus", "azure-retail:azure-openai");
    let (status, headers, created) = put(azure.clone(), "map-1", "*", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(created["item"], "gpt 4o 0806 · global");
    assert_eq!(created["region"], Value::Null);
    // A replay returns the recorded response.
    let (status, _, replayed) = put(azure.clone(), "map-1", "*", body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed, created);
    // Updates are conditional.
    let (status, _, conflict) = put(
        azure.clone(),
        "map-2",
        "\"v7\"",
        json!({"item": "gpt 4o 0806 · regional"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");

    let (_, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    let comparison = &entry(&drift, "gpt4o-eus")["comparisons"][0];
    assert_eq!(comparison["matched_by"], "mapping");
    assert_eq!(comparison["observed_item"], "gpt 4o 0806 · global");
    // The deployment's own region is preferred.
    assert_eq!(comparison["observed_region"], "eastus");
    assert_eq!(comparison["status"], "matches", "{comparison}");
    assert_eq!(dimension(comparison, "cached_input")["status"], "matches");

    // A pinned region, and a Bedrock mapping.
    let (status, headers, updated) = put(
        azure.clone(),
        "map-3",
        "\"v1\"",
        json!({"item": "gpt 4o 0806 · global", "region": "westeurope"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(headers["etag"], "\"v2\"");
    let bedrock = mapping("nova", "aws-price-list:bedrock");
    let (status, _, _) = put(bedrock.clone(), "map-4", "*", json!({"item": "NovaPro"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    let comparison = &entry(&drift, "gpt4o-eus")["comparisons"][0];
    assert_eq!(comparison["observed_region"], "westeurope");
    assert_eq!(comparison["status"], "differs");
    let comparison = &entry(&drift, "nova")["comparisons"][0];
    assert_eq!(comparison["observed_item"], "NovaPro");
    assert_eq!(comparison["status"], "matches", "{comparison}");

    let (_, listed) = harness
        .get(&uri(org, "price-mappings"), Some(&reader))
        .await;
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["deployment"], "gpt4o-eus");
    assert_eq!(items[0]["version"], 2);
    assert_eq!(items[1]["deployment"], "nova");

    // Mappings name a live source of the deployment's own family.
    for (path, body, code) in [
        (
            mapping("nova", "azure-retail:azure-openai"),
            json!({"item": "NovaPro"}),
            "unknown_deployment",
        ),
        (
            mapping("missing", "aws-price-list:bedrock"),
            json!({"item": "NovaPro"}),
            "unknown_deployment",
        ),
        (
            mapping("nova", "price-sheet:bedrock"),
            json!({"item": "NovaPro"}),
            "unknown_price_source",
        ),
        (
            bedrock.clone(),
            json!({"item": ""}),
            "invalid_price_mapping",
        ),
    ] {
        let (status, _, problem) = put(path, "bad", "*", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
        assert_eq!(problem["code"], code, "{problem}");
    }
    let (status, _, _) = put(
        bedrock.clone(),
        "extra",
        "*",
        json!({"item": "x", "model": "y"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Deleting returns drift to model matching.
    let (status, _, _) = harness
        .send(
            Method::DELETE,
            &azure,
            Some(&admin),
            &[("idempotency-key", "unmap-1"), ("if-match", "\"v2\"")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, replayed) = harness
        .send(
            Method::DELETE,
            &azure,
            Some(&admin),
            &[("idempotency-key", "unmap-1"), ("if-match", "\"v2\"")],
            None,
        )
        .await;
    assert_eq!(
        (status, replayed),
        (StatusCode::OK, json!({"deleted": true}))
    );
    let (_, drift) = harness.get(&uri(org, "price-drift"), Some(&reader)).await;
    let comparison = &entry(&drift, "gpt4o-eus")["comparisons"][0];
    assert_eq!(comparison["matched_by"], "model");
    assert_eq!(comparison["status"], "no_observation");

    let audited: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'price_mapping' ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let audited: Vec<_> = audited
        .iter()
        .map(|(action, outcome)| (action.as_str(), outcome.as_str()))
        .collect();
    assert_eq!(
        audited,
        [
            ("price_mapping.set", "denied"),
            ("price_mapping.set", "succeeded"),
            ("price_mapping.set", "succeeded"),
            ("price_mapping.set", "succeeded"),
            ("price_mapping.deleted", "succeeded"),
        ]
    );

    // A source that is removed takes its mappings with it.
    sqlx::query("DELETE FROM gateway.price_source WHERE source = 'aws-price-list:bedrock'")
        .execute(&harness.pool)
        .await
        .unwrap();
    let (_, listed) = harness
        .get(&uri(org, "price-mappings"), Some(&reader))
        .await;
    assert_eq!(listed["items"], json!([]));
}

#[tokio::test]
async fn an_administrator_can_ask_for_a_fetch_now() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    server.respond("openai.json", StatusCode::OK, openai_sheet("2.50"));
    let runner = harness.runner(vec![server.source(ProviderFamily::OpenAi, "openai.json")]);
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    let (org, _) = harness.org_with_release("fetch-now").await;
    let admin = harness
        .session(harness.user(org, "organization_admin").await)
        .await;
    let reader = harness
        .session(harness.user(org, "read_only_member").await)
        .await;
    let fetch = uri(org, "price-sources/price-sheet:openai/fetch");
    let post = |session: &(String, String), key: &'static str| {
        let session = session.clone();
        let fetch = fetch.clone();
        let harness = &harness;
        async move {
            harness
                .send(
                    Method::POST,
                    &fetch,
                    Some(&session),
                    &[("idempotency-key", key)],
                    None,
                )
                .await
        }
    };

    // Just fetched, so not again yet.
    let (status, headers, body) = post(&admin, "fetch-1").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "price_fetch_too_soon");
    let retry_after: i64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((590..=600).contains(&retry_after), "{retry_after}");

    sqlx::query("UPDATE gateway.price_source SET last_attempt_at = now() - interval '11 minutes'")
        .execute(&harness.pool)
        .await
        .unwrap();
    let (status, _, _) = post(&reader, "fetch-1").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = harness
        .send(Method::POST, &fetch, Some(&admin), &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = harness
        .send(
            Method::POST,
            &uri(org, "price-sources/price-sheet:anthropic/fetch"),
            Some(&admin),
            &[("idempotency-key", "fetch-1")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The key was released by each refusal, so it is still free.
    let (status, _, requested) = post(&admin, "fetch-1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{requested}");
    assert_eq!(requested["source"], "price-sheet:openai");
    assert_near(
        requested["next_fetch_at"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        Duration::zero(),
    );
    let (status, _, replayed) = post(&admin, "fetch-1").await;
    assert_eq!((status, &replayed), (StatusCode::OK, &requested));
    // The reader's refusal is audited as denied.
    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'price_source.fetch_requested'
           AND entity_type = 'price_source_fetch'
         ORDER BY outcome",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, ["denied", "succeeded"]);

    // The worker fetches it on its next tick, then schedules the next day.
    assert_eq!(runner.run_once().await.unwrap(), report(0, 1, 0));
    assert_near(
        harness.state("price-sheet:openai").await.next_fetch_at,
        Duration::hours(24),
    );
}

/// Sends `method uri` to the price adoption routes.
async fn adoption_send(
    router: &Router,
    method: Method,
    uri: &str,
    session: &(String, String),
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method.clone())
        .uri(uri)
        .header(
            COOKIE,
            format!(
                "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                session.0, session.1
            ),
        );
    if method != Method::GET {
        builder = builder
            .header("idempotency-key", Uuid::new_v4().to_string())
            .header("if-match", "*")
            .header("origin", "https://gateway.example");
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Published prices are adopted into a new release after a fetch (D-024):
/// only what differs, in the deployment's own region, never an override or
/// another currency; an implausible change is held, a dismissed hold stays
/// dismissed, and an organization that turned adoption off is left alone.
#[tokio::test]
async fn published_prices_are_adopted_held_or_left_as_the_policy_says() {
    use gateway::persistence::price_adoption::{AdoptionOutcome, PriceAdoptionStore};
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let server = SheetServer::start().await;
    // gpt: global input $3.00 (was $2.50), eu $2.75; the rest as billed.
    server.respond("openai.json", StatusCode::OK, openai_sheet("3.00"));
    let (org, release) = harness.org_with_release("pricing-adopt").await;
    let admin = harness.user(org, "organization_admin").await;
    let admin = harness.session(admin).await;
    let reader = harness.user(org, "read_only_member").await;
    let reader = harness.session(reader).await;
    let adoption = PriceAdoptionStore::new(harness.pool.clone());
    let router = gateway::transport::price_adoption::router(
        AuthStore::new(harness.pool.clone()),
        AdminStore::new(harness.pool.clone()),
        Some(harness.crypto.clone()),
        adoption.clone(),
    );
    let path = |tail: &str| format!("/api/admin/v1/organizations/{}/catalog/{tail}", org.0);
    let runner = harness.runner(vec![server.source(ProviderFamily::OpenAi, "openai.json")]);
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));

    // On by default.
    let (status, policy) =
        adoption_send(&router, Method::GET, &path("price-policy"), &reader, None).await;
    assert_eq!(status, StatusCode::OK, "{policy}");
    assert_eq!(policy["auto_adopt"], true);
    assert_eq!(policy["version"], 0);
    assert!(adoption.organizations().await.unwrap().contains(&org));

    let AdoptionOutcome::Activated {
        release_id: adopted_release,
        adopted,
        held,
    } = adoption.apply(org, Utc::now()).await.unwrap()
    else {
        panic!("a release adopts the new prices");
    };
    assert_eq!((adopted, held), (2, 0));
    assert_ne!(adopted_release, release);
    let active: Uuid = sqlx::query_scalar(
        "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(active, adopted_release);
    let billed: Vec<(String, i64, i64, String)> = sqlx::query_as(
        "SELECT d.name, p.input_minor_per_token, p.output_minor_per_token, p.source
         FROM gateway.catalog_price p
         JOIN gateway.catalog_deployment d
           ON d.organization_id = p.organization_id AND d.release_id = p.release_id
          AND d.id = p.deployment_id
         WHERE p.organization_id = $1 AND p.release_id = $2 ORDER BY d.name",
    )
    .bind(org.0)
    .bind(adopted_release)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let price_of = |name: &str| billed.iter().find(|row| row.0 == name).unwrap().clone();
    assert_eq!(price_of("gpt-a").1, 300);
    assert_eq!(price_of("gpt-a").2, 1000, "only what differs changes");
    assert_eq!(price_of("gpt-a").3, "price-sheet:openai");
    assert_eq!(price_of("gpt-b").1, 275, "the deployment's own region");
    assert_eq!(
        price_of("gpt-legacy").1,
        50,
        "another currency is not adopted"
    );
    assert_eq!(price_of("claude-a").1, 300, "no source publishes it");
    let recorded: Vec<(String, Value)> = sqlx::query_as(
        "SELECT deployment, changes FROM gateway.catalog_price_adoption
         WHERE organization_id = $1 ORDER BY deployment",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].0, "gpt-a");
    assert_eq!(recorded[0].1[0]["before"], 250);
    assert_eq!(recorded[0].1[0]["after"], 300);
    let actions: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, actor_type FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_id = $2 ORDER BY occurred_at, action",
    )
    .bind(org.0)
    .bind(adopted_release)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    for action in [
        "catalog_release.created",
        "catalog_release.activated",
        "catalog_release.prices_adopted",
    ] {
        assert!(
            actions
                .iter()
                .any(|(a, actor)| a == action && actor == "system"),
            "{action}: {actions:?}"
        );
    }
    // Members are told, once per release.
    let notified = |kind: &'static str| {
        let pool = harness.pool.clone();
        async move {
            sqlx::query_as::<_, (String, String, String, bool)>(
                "SELECT subject, severity, title, resolved_at IS NOT NULL
                 FROM gateway.notification
                 WHERE organization_id = $1 AND kind = $2 ORDER BY created_at",
            )
            .bind(org.0)
            .bind(kind)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        notified("price.adopted").await,
        [(
            adopted_release.to_string(),
            "info".to_owned(),
            "Adopted 2 published prices".to_owned(),
            false
        )]
    );
    // Nothing differs now.
    assert_eq!(
        adoption.apply(org, Utc::now()).await.unwrap(),
        AdoptionOutcome::Unchanged { held: 0 }
    );
    assert_eq!(notified("price.adopted").await.len(), 1);

    // More than double: held, not adopted, and listed.
    server.respond(
        "openai.json",
        StatusCode::OK,
        relabel(&openai_sheet("7.00"), "2026-10"),
    );
    harness.make_due().await;
    assert_eq!(runner.run_once().await.unwrap(), report(1, 0, 0));
    assert_eq!(
        adoption.apply(org, Utc::now()).await.unwrap(),
        AdoptionOutcome::Unchanged { held: 1 }
    );
    let (status, holds) =
        adoption_send(&router, Method::GET, &path("price-holds"), &reader, None).await;
    assert_eq!(status, StatusCode::OK, "{holds}");
    let items = holds["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{holds}");
    assert_eq!(items[0]["deployment"], "gpt-a");
    assert_eq!(items[0]["reason"], "large_increase");
    assert_eq!(items[0]["changes"][0]["proposed"], 700);
    // The same proposal keeps its hold.
    assert_eq!(
        adoption.apply(org, Utc::now()).await.unwrap(),
        AdoptionOutcome::Unchanged { held: 1 }
    );
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.catalog_price_hold WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(open, 1);
    // One open action notification for the held price, however often held.
    let operation = items[0]["operation"].as_str().unwrap();
    let held = notified("price.held").await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].0, format!("gpt-a/{operation}"));
    assert_eq!((held[0].1.as_str(), held[0].3), ("action", false));
    // A hold from before notifications existed is notified on the next run,
    // once however many runs follow.
    sqlx::query(
        "DELETE FROM gateway.notification WHERE organization_id = $1 AND kind = 'price.held'",
    )
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            adoption.apply(org, Utc::now()).await.unwrap(),
            AdoptionOutcome::Unchanged { held: 1 }
        );
        let held = notified("price.held").await;
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(!held[0].3);
    }

    // A reader cannot dismiss it; an admin can, and it stays dismissed.
    let hold_id = items[0]["id"].as_str().unwrap().to_owned();
    let dismiss = path(&format!("price-holds/{hold_id}/dismiss"));
    let (status, _) = adoption_send(&router, Method::POST, &dismiss, &reader, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, dismissed) = adoption_send(&router, Method::POST, &dismiss, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{dismissed}");
    assert_eq!(dismissed["status"], "dismissed");
    let (status, again) = adoption_send(&router, Method::POST, &dismiss, &admin, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    assert_eq!(again["code"], "not_held");
    // The admin acted on it, so its notification is resolved.
    assert!(notified("price.held").await.iter().all(|row| row.3));
    adoption.apply(org, Utc::now()).await.unwrap();
    let (_, holds) = adoption_send(&router, Method::GET, &path("price-holds"), &reader, None).await;
    assert_eq!(
        holds["items"],
        json!([]),
        "a dismissed proposal is not held again"
    );
    let (_, all) = adoption_send(
        &router,
        Method::GET,
        &path("price-holds?all=true"),
        &reader,
        None,
    )
    .await;
    assert_eq!(all["items"].as_array().unwrap().len(), 1);

    // Turned off: left alone.
    let (status, _) = adoption_send(
        &router,
        Method::PUT,
        &path("price-policy"),
        &reader,
        Some(json!({"auto_adopt": false})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, policy) = adoption_send(
        &router,
        Method::PUT,
        &path("price-policy"),
        &admin,
        Some(json!({"auto_adopt": false})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    assert_eq!(policy["auto_adopt"], false);
    assert_eq!(policy["version"], 1);
    assert!(!adoption.organizations().await.unwrap().contains(&org));
    server.respond(
        "openai.json",
        StatusCode::OK,
        relabel(&openai_sheet("3.25"), "2026-11"),
    );
    harness.make_due().await;
    runner.run_once().await.unwrap();
    assert_eq!(
        adoption.apply(org, Utc::now()).await.unwrap(),
        AdoptionOutcome::NotEnabled
    );
    let still: Uuid = sqlx::query_scalar(
        "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(still, adopted_release);
}
