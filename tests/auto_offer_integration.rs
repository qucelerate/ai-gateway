#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for automatic model offers: the opt-in lane (driven
//! through the real router, RBAC, idempotency and audit), and the runs the
//! discovery worker makes after listing an opted-in account, against a fake
//! Bedrock account and price sources seeded in the database.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Method, StatusCode, header::COOKIE},
};
use chrono::Utc;
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        catalog::ProviderFamily,
        discovery::DiscoveredModel,
        identity::MembershipRole,
        inference::{
            ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, InferenceContext,
            ProviderError, ProviderErrorCategory,
        },
        keys::ApiKeyPepper,
        pricing::{ComparisonStatus, MatchedBy},
    },
    jobs::discovery::{DiscoveryRunner, ProviderAccounts},
    persistence::{
        AdminStore, AuthStore, IdentityStore,
        auto_offer::{ApplyOutcome, AutoOfferStore},
        catalog_sync::CatalogSyncStore,
        price_adoption::{AdoptionOutcome, PriceAdoptionStore},
        price_sync::PriceSyncStore,
    },
    provider::{ChatEventStream, InferenceProviderAdapter},
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
const TEST_PEPPER: &[u8] = b"auto-offer-integration-pepper";
const HMAC_KEY: &[u8] = b"auto-offer-integration-hmac-00032";

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

/// What a fake account's listing returns.
#[derive(Clone)]
enum Listing {
    Models(Vec<&'static str>),
}

struct FakeAdapter(Listing);

#[async_trait]
impl InferenceProviderAdapter for FakeAdapter {
    async fn chat(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Internal))
    }

    async fn chat_stream(
        &self,
        _: InferenceContext,
        _: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Internal))
    }

    async fn embeddings(
        &self,
        _: InferenceContext,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(ProviderErrorCategory::Internal))
    }

    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let Listing::Models(models) = &self.0;
        Ok(models
            .iter()
            .map(|model| DiscoveredModel::new(model, name(model)).unwrap())
            .collect())
    }
}

/// Accounts of organizations other tests created have no credential here, so
/// their jobs fail as `not_configured` without reaching a provider.
#[derive(Default)]
struct FakeAccounts {
    listings: Mutex<HashMap<(OrganizationId, String), Listing>>,
}

impl FakeAccounts {
    fn set(&self, org: OrganizationId, key: &str, listing: Listing) {
        self.listings
            .lock()
            .unwrap()
            .insert((org, key.to_owned()), listing);
    }
}

#[async_trait]
impl ProviderAccounts for FakeAccounts {
    async fn adapter(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        _family: ProviderFamily,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError> {
        Ok(self
            .listings
            .lock()
            .unwrap()
            .get(&(organization_id, adapter_instance_key.to_owned()))
            .cloned()
            .map(|listing| Arc::new(FakeAdapter(listing)) as Arc<dyn InferenceProviderAdapter>))
    }
}

const KEY: &str = "bedrock-main";
const SONNET: &str = "us.anthropic.claude-sonnet-4-6";
const SONNET_DEPLOYMENT: &str = "bedrock-main-us.anthropic.claude-sonnet-4-6";
const NOVA: &str = "amazon.nova-pro-v1:0";
const NOVA_DEPLOYMENT: &str = "bedrock-main-amazon.nova-pro-v1:0";

const SONNET_FOUNDATION: &str = "anthropic.claude-sonnet-4-6";

/// The names Bedrock's listing gives models.
fn name(model: &str) -> Option<&'static str> {
    match model {
        SONNET_FOUNDATION => Some("Claude Sonnet 4.6"),
        SONNET => Some("US Anthropic Claude Sonnet 4.6"),
        NOVA => Some("Nova Pro"),
        _ => None,
    }
}

/// A catalog that declares the Bedrock account and deploys nothing on it.
fn manifest() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "auto-offer-1"},
        "providers": [{"adapter_instance_key": KEY, "family": "bedrock"}],
        "deployments": [],
        "prices": [],
        "routes": []
    }))
    .unwrap()
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    catalog: CatalogStore,
    accounts: Arc<FakeAccounts>,
    runner: DiscoveryRunner,
    offers: AutoOfferStore,
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
        let crypto = AuthCrypto::new(HMAC_KEY, &[37_u8; 32]).unwrap();
        let store = CatalogSyncStore::new(pool.clone());
        let accounts = Arc::new(FakeAccounts::default());
        let offers = AutoOfferStore::new(pool.clone());
        let router = gateway::transport::auto_offer::router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            offers.clone(),
        )
        .merge(gateway::transport::catalog_sync::router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            store.clone(),
            PriceSyncStore::new(pool.clone()),
        ));
        Self {
            catalog: CatalogStore::new(pool.clone()),
            runner: DiscoveryRunner::new(store, accounts.clone()).with_auto_offers(offers.clone()),
            offers,
            accounts,
            pool,
            identity,
            crypto,
            router,
        }
    }

    async fn org_with_release(&self, slug: &str) -> (OrganizationId, Uuid) {
        let suffix = Uuid::new_v4().simple().to_string();
        let org = self
            .identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id;
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

    async fn send(
        &self,
        method: Method,
        uri: &str,
        session: Option<&(String, String)>,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get(&self, uri: &str, session: &(String, String)) -> (StatusCode, Value) {
        self.send(Method::GET, uri, Some(session), &[], None).await
    }

    /// Runs worker ticks until `org` has no open job.
    async fn drain(&self, org: OrganizationId) {
        for _ in 0..500 {
            self.runner.run_once().await.unwrap();
            let open: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM gateway.catalog_sync_job
                 WHERE organization_id = $1 AND status IN ('queued', 'running')",
            )
            .bind(org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            if open == 0 {
                return;
            }
        }
        panic!("discovery jobs for the organization never finished");
    }

    async fn active(&self, org: OrganizationId) -> Uuid {
        sqlx::query_scalar(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// The active release's routes as (alias, operation, deployment), and
    /// its chat prices as (deployment, input, output, cached input,
    /// confidence).
    async fn catalog(
        &self,
        org: OrganizationId,
    ) -> (
        Vec<(String, String, String)>,
        Vec<(String, i64, i64, i64, String)>,
    ) {
        let routes = sqlx::query_as(
            "SELECT r.alias, r.operation, d.name
             FROM gateway.catalog_active_release a
             JOIN gateway.catalog_route r
               ON r.organization_id = a.organization_id AND r.release_id = a.release_id
             JOIN gateway.catalog_deployment d
               ON d.organization_id = r.organization_id AND d.release_id = r.release_id
              AND d.id = r.deployment_id
             WHERE a.organization_id = $1 ORDER BY r.alias, r.operation",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .unwrap();
        let prices = sqlx::query_as(
            "SELECT d.name, p.input_minor_per_token, p.output_minor_per_token,
                    p.cached_input_minor_per_token, p.confidence
             FROM gateway.catalog_active_release a
             JOIN gateway.catalog_price p
               ON p.organization_id = a.organization_id AND p.release_id = a.release_id
             JOIN gateway.catalog_deployment d
               ON d.organization_id = p.organization_id AND d.release_id = p.release_id
              AND d.id = p.deployment_id
             WHERE a.organization_id = $1 AND p.operation = 'chat' ORDER BY d.name",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .unwrap();
        (routes, prices)
    }
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/catalog/{path}", org.0)
}

/// Bedrock's public price list, which prices products rather than models
/// (Amazon's by usage type, partners' by the name Bedrock lists them
/// under), and an operator price sheet that names models. Sources are
/// gateway-wide, so each test seeds its own snapshot.
async fn seed_prices(pool: &PgPool, nova_input: &str) {
    for (source, kind) in [
        ("aws-price-list:bedrock", "aws-price-list"),
        ("price-sheet:bedrock", "price-sheet"),
    ] {
        sqlx::query(
            "INSERT INTO gateway.price_source (source, kind, family, next_fetch_at, registered_at)
             VALUES ($1, $2, 'bedrock', now() + interval '1 day', now())
             ON CONFLICT (source) DO UPDATE SET retired_at = NULL",
        )
        .bind(source)
        .bind(kind)
        .execute(pool)
        .await
        .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    type Observed<'a> = (&'a str, Option<&'a str>, &'a str, &'a str);
    let snapshots: [(&str, Vec<Observed<'_>>); 2] = [
        (
            "aws-price-list:bedrock",
            vec![
                ("ClaudeSonnet4.6", None, "3", "15"),
                ("Claude Sonnet 4.6", None, "3.3", "16.5"),
            ],
        ),
        (
            "price-sheet:bedrock",
            vec![("nova pro", Some(NOVA), nova_input, "3.2")],
        ),
    ];
    for (source, observations) in snapshots {
        let snapshot = Uuid::new_v4();
        let unmapped = observations.iter().filter(|o| o.1.is_none()).count();
        sqlx::query(
            "INSERT INTO gateway.price_snapshot
                 (id, source, digest, observations, unmapped, fetched_at, confirmed_at)
             VALUES ($1, $2, $3, $4, $5, now(), now())",
        )
        .bind(snapshot)
        .bind(source)
        .bind(Uuid::new_v4().as_bytes().repeat(2))
        .bind(observations.len() as i32)
        .bind(unmapped as i32)
        .execute(&mut *tx)
        .await
        .unwrap();
        for (ordinal, (item, model, input, output)) in observations.into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO gateway.price_observation
                     (snapshot_id, ordinal, item, model, region, currency, per_tokens, input, output)
                 VALUES ($1, $2, $3, $4, 'us-east-1', 'USD', 1000000, $5::numeric, $6::numeric)",
            )
            .bind(snapshot)
            .bind(ordinal as i32)
            .bind(item)
            .bind(model)
            .bind(input)
            .bind(output)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE gateway.price_source SET current_snapshot_id = $2 WHERE source = $1")
            .bind(source)
            .bind(snapshot)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

fn statuses(offer: &Value) -> Vec<(String, String)> {
    offer["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| {
            (
                model["model"].as_str().unwrap().to_owned(),
                model["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn bedrock_models_are_priced_by_the_name_bedrock_lists() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("auto-offer-named").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    seed_prices(&harness.pool, "0.8").await;
    // The foundation model and its US inference profile, which the price
    // list prices under the foundation model's name.
    harness
        .accounts
        .set(org, KEY, Listing::Models(vec![SONNET_FOUNDATION, SONNET]));
    let offer_uri = uri(org, &format!("auto-offers/{KEY}"));
    let (status, offer) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", "n-1"), ("if-match", "*")],
            Some(&json!({ "location": "us-east-1" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{offer}");
    harness.drain(org).await;
    let (status, offer) = harness.get(&offer_uri, &owner).await;
    assert_eq!(status, StatusCode::OK, "{offer}");
    assert_eq!(
        statuses(&offer),
        [
            (SONNET_FOUNDATION.into(), "offered".into()),
            (SONNET.into(), "offered".into()),
        ]
    );
    for model in offer["models"].as_array().unwrap() {
        assert_eq!(model["price_source"], "aws-price-list:bedrock");
        assert_eq!(model["price_item"], "Claude Sonnet 4.6");
    }
    let (routes, prices) = harness.catalog(org).await;
    assert!(routes.contains(&(SONNET.into(), "chat".into(), SONNET_DEPLOYMENT.into())));
    // $3.30 and $16.50 per million; cached input charged as input.
    assert_eq!(
        prices,
        [
            (
                format!("{KEY}-{SONNET_FOUNDATION}"),
                330,
                1650,
                330,
                "conservative".into()
            ),
            (
                SONNET_DEPLOYMENT.into(),
                330,
                1650,
                330,
                "conservative".into()
            ),
        ]
    );

    // Price drift compares them by the same listing name, and automatic
    // price adoption follows the price list (D-024).
    let drift = PriceSyncStore::new(harness.pool.clone())
        .drift(org)
        .await
        .unwrap()
        .unwrap();
    for entry in &drift.prices {
        let aws = entry
            .comparisons
            .iter()
            .find(|c| c.source == "aws-price-list:bedrock")
            .unwrap();
        assert_eq!(
            aws.matched_by,
            MatchedBy::ListingName,
            "{}",
            entry.deployment
        );
        assert_eq!(
            aws.status,
            ComparisonStatus::Matches,
            "{}",
            entry.deployment
        );
    }
    sqlx::query(
        "UPDATE gateway.price_observation SET input = 3.6
         WHERE item = 'Claude Sonnet 4.6' AND snapshot_id = (
             SELECT current_snapshot_id FROM gateway.price_source
             WHERE source = 'aws-price-list:bedrock')",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    let adopted = PriceAdoptionStore::new(harness.pool.clone())
        .apply(org, Utc::now())
        .await
        .unwrap();
    assert!(
        matches!(
            adopted,
            AdoptionOutcome::Activated {
                adopted: 2,
                held: 0,
                ..
            }
        ),
        "{adopted:?}"
    );
    let (_, prices) = harness.catalog(org).await;
    // Cached input stood in at the input price, and follows it.
    assert!(
        prices
            .iter()
            .all(|(_, input, output, cached, _)| (*input, *output, *cached) == (360, 1650, 360)),
        "{prices:?}"
    );
    // The offer's next run leaves the adopted price as it is.
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
}

#[tokio::test]
async fn opted_in_accounts_offer_priced_models_and_keep_them_in_step() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, base) = harness.org_with_release("auto-offer-flow").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    seed_prices(&harness.pool, "0.8").await;
    harness.accounts.set(
        org,
        KEY,
        Listing::Models(vec![
            SONNET,
            NOVA,
            "amazon.titan-embed-text-v2:0",
            "meta.llama3-70b",
        ]),
    );
    let offer_uri = uri(org, &format!("auto-offers/{KEY}"));

    // Opting in needs If-Match and an account of the active release.
    let (status, _) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", "o-1")],
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, problem) = harness
        .send(
            Method::PUT,
            &uri(org, "auto-offers/nobody"),
            Some(&owner),
            &[("idempotency-key", "o-0"), ("if-match", "*")],
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "unknown_provider_account");
    let (status, offer) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", "o-1"), ("if-match", "*")],
            Some(&json!({ "location": "us-east-1" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{offer}");
    assert_eq!(offer["version"], 1);
    assert_eq!(offer["models"], json!([]));
    // It queued a listing of the account.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.catalog_sync_job
         WHERE organization_id = $1 AND adapter_instance_key = $2 AND status = 'queued'",
    )
    .bind(org.0)
    .bind(KEY)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(queued, 1);

    // The listing offers the model a price sheet names; the price list's
    // product is not matched to a model by guesswork.
    harness.drain(org).await;
    let (status, offer) = harness.get(&offer_uri, &owner).await;
    assert_eq!(status, StatusCode::OK, "{offer}");
    assert_eq!(offer["error_code"], Value::Null);
    assert_eq!(
        statuses(&offer),
        [
            ("amazon.nova-pro-v1:0".into(), "offered".into()),
            ("amazon.titan-embed-text-v2:0".into(), "needs_price".into()),
            ("meta.llama3-70b".into(), "needs_price".into()),
            (SONNET.into(), "needs_price".into()),
        ]
    );
    let first = harness.active(org).await;
    assert_ne!(first, base);
    assert_eq!(offer["applied_release_id"], first.to_string());
    let (routes, prices) = harness.catalog(org).await;
    assert_eq!(
        routes,
        [(NOVA.into(), "chat".into(), NOVA_DEPLOYMENT.into()),]
    );
    // $0.80 and $3.20 per million; cached input charged as input.
    assert_eq!(
        prices,
        [(NOVA_DEPLOYMENT.into(), 80, 320, 80, "conservative".into())]
    );

    // The system created and activated the release, audited, and the
    // activation pointer's version moved on.
    let audits: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT actor_type, action, entity_type FROM gateway.audit_event
         WHERE organization_id = $1 AND actor_type = 'system' ORDER BY occurred_at, action",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        audits,
        [
            (
                "system".into(),
                "catalog_release.activated".into(),
                "catalog_activation".into()
            ),
            (
                "system".into(),
                "catalog_release.created".into(),
                "catalog_release".into()
            ),
        ]
    );
    let activation: i64 = sqlx::query_scalar(
        "SELECT version FROM gateway.admin_resource_version
         WHERE organization_id = $1 AND resource_type = 'catalog_activation' AND resource_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(activation >= 1);

    // An admin prices Sonnet by mapping its deployment-to-be to the price
    // list's product; the next run offers it.
    let (status, mapping) = harness
        .send(
            Method::PUT,
            &uri(
                org,
                &format!("price-mappings/{SONNET_DEPLOYMENT}/aws-price-list:bedrock"),
            ),
            Some(&owner),
            &[("idempotency-key", "m-1"), ("if-match", "*")],
            Some(&json!({ "item": "ClaudeSonnet4.6" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{mapping}");
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Activated { added: 1, .. }),
        "{outcome:?}"
    );
    let (routes, prices) = harness.catalog(org).await;
    // Offers declare chat only: streamed requests take the chat route.
    assert!(routes.contains(&(SONNET.into(), "chat".into(), SONNET_DEPLOYMENT.into())));
    assert!(
        !routes
            .iter()
            .any(|(_, operation, _)| operation == "chat_stream")
    );
    assert_eq!(
        prices[1],
        (
            SONNET_DEPLOYMENT.into(),
            300,
            1500,
            300,
            "conservative".into()
        )
    );

    // Nothing changed: no release.
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);

    // The sheet's price changes: the offer leaves it to automatic price
    // adoption (D-024), which adopts it in a release of its own.
    seed_prices(&harness.pool, "0.75").await;
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    let adopted = PriceAdoptionStore::new(harness.pool.clone())
        .apply(org, Utc::now())
        .await
        .unwrap();
    assert!(
        matches!(
            adopted,
            AdoptionOutcome::Activated {
                adopted: 1,
                held: 0,
                ..
            }
        ),
        "{adopted:?}"
    );
    let (_, prices) = harness.catalog(org).await;
    assert_eq!(
        prices[0],
        (NOVA_DEPLOYMENT.into(), 75, 320, 75, "conservative".into())
    );

    // An admin rolls back to the first release, which lacks Sonnet: it is
    // withdrawn rather than offered again, until the opt-in is set again.
    let mut tx = harness.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE gateway.catalog_active_release SET release_id = $2 WHERE organization_id = $1",
    )
    .bind(org.0)
    .bind(first)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    // Sonnet stays out, and the offer leaves the rolled-back release's
    // Nova price for adoption to bring up to date.
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    let (_, offer) = harness.get(&offer_uri, &owner).await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "withdrawn".into())));
    let (status, offer) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", "o-2"), ("if-match", "\"v1\"")],
            Some(&json!({ "location": "us-east-1" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{offer}");
    assert_eq!(offer["version"], 2);
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Activated { added: 1, .. }),
        "{outcome:?}"
    );

    // Saving the opt-in right after a rollback, before any run has
    // withdrawn what the rollback dropped, also offers it again.
    sqlx::query(
        "UPDATE gateway.catalog_active_release SET release_id = $2 WHERE organization_id = $1",
    )
    .bind(org.0)
    .bind(first)
    .execute(&harness.pool)
    .await
    .unwrap();
    let (status, offer) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", "o-3"), ("if-match", "\"v2\"")],
            Some(&json!({ "location": "us-east-1" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{offer}");
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Activated { added: 1, .. }),
        "{outcome:?}"
    );
    let (_, offer) = harness.get(&offer_uri, &owner).await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "offered".into())));

    // The listing drops Nova: its deployment and routes leave.
    harness
        .accounts
        .set(org, KEY, Listing::Models(vec![SONNET]));
    harness.drain(org).await;
    let (status, _) = harness
        .send(
            Method::POST,
            &uri(org, "sync-jobs"),
            Some(&owner),
            &[("idempotency-key", "s-1")],
            Some(&json!({ "adapter_instance_key": KEY })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    harness.drain(org).await;
    let (routes, _) = harness.catalog(org).await;
    assert!(
        routes.iter().all(|(alias, _, _)| alias == SONNET),
        "{routes:?}"
    );

    // Opting out leaves the catalog as it is.
    let before = harness.active(org).await;
    let (status, _) = harness
        .send(
            Method::DELETE,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", "d-1"), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let outcome = harness
        .offers
        .apply(org, KEY, ProviderFamily::Bedrock, &[], Utc::now())
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::NotEnabled);
    assert_eq!(harness.active(org).await, before);
    let (status, _) = harness.get(&offer_uri, &owner).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn opt_ins_are_set_by_catalog_writers_and_read_by_catalog_readers() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("auto-offer-rbac").await;
    let auditor = harness.user(org, "auditor").await;
    let auditor = harness.session(auditor).await;
    let admin = harness.user(org, "organization_admin").await;
    let admin = harness.session(admin).await;
    let offer_uri = uri(org, &format!("auto-offers/{KEY}"));

    let (status, _) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&auditor),
            &[("idempotency-key", "a-1"), ("if-match", "*")],
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&admin),
            &[("idempotency-key", "a-1"), ("if-match", "*")],
            Some(&json!({ "location": " us-east-1" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, offer) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&admin),
            &[("idempotency-key", "a-2"), ("if-match", "*")],
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{offer}");
    // A replay returns the recorded response.
    let (status, replayed) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&admin),
            &[("idempotency-key", "a-2"), ("if-match", "*")],
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["version"], offer["version"]);
    let (status, read) = harness.get(&offer_uri, &auditor).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["location"], Value::Null);
    let (status, _) = harness.send(Method::GET, &offer_uri, None, &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let audits: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'catalog_auto_offer'
         ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        audits,
        [
            ("catalog_auto_offer.set".into(), "denied".into()),
            ("catalog_auto_offer.set".into(), "succeeded".into()),
        ]
    );
}

/// Model availability evidence (D-018): a model a call was refused for is
/// not offered, once another model of the account has been called since (so
/// the refusal is not a broken key); an offered one is removed, the
/// discovered-model list shows why, and a later success offers it again. An
/// older observation never overwrites a newer verdict, but still advances
/// the last success.
#[tokio::test]
async fn refused_models_leave_the_offer_until_verified_again() {
    use gateway::{
        domain::model_evidence::{
            EvidenceKey, EvidenceReason, EvidenceSource, Observation, Verdict,
        },
        persistence::model_evidence::ModelEvidenceStore,
    };
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("auto-offer-evidence").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    seed_prices(&harness.pool, "0.8").await;
    harness
        .accounts
        .set(org, KEY, Listing::Models(vec![SONNET_FOUNDATION, SONNET]));
    let offer_uri = uri(org, &format!("auto-offers/{KEY}"));
    let mut run = 0;
    let mut rerun = || {
        run += 1;
        format!("evidence-{run}")
    };
    let (status, offer) = harness
        .send(
            Method::PUT,
            &offer_uri,
            Some(&owner),
            &[("idempotency-key", &rerun()), ("if-match", "*")],
            Some(&json!({ "location": "us-east-1" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{offer}");
    harness.drain(org).await;
    let (routes, _) = harness.catalog(org).await;
    assert!(routes.iter().any(|(alias, _, _)| alias == SONNET));

    let store = ModelEvidenceStore::new(harness.pool.clone());
    let key = EvidenceKey {
        organization_id: org,
        adapter_instance_key: KEY.into(),
        family: "bedrock".into(),
        model: SONNET.into(),
    };
    let observed = |verdict, at| Observation {
        key: key.clone(),
        verdict,
        source: EvidenceSource::Traffic,
        deployment: Some(SONNET_DEPLOYMENT.into()),
        provider_status: matches!(verdict, Verdict::Refused(_)).then_some(403),
        provider_code: matches!(verdict, Verdict::Refused(_))
            .then(|| "AccessDeniedException".to_owned()),
        observed_at: at,
    };
    let start = Utc::now();
    store
        .record(&observed(Verdict::Verified, start))
        .await
        .unwrap();
    let refused_at = start + chrono::Duration::seconds(5);
    store
        .record(&observed(
            Verdict::Refused(EvidenceReason::AccessDenied),
            refused_at,
        ))
        .await
        .unwrap();
    // A slower process's older success does not undo the refusal, but is
    // the last success.
    let last_success = start + chrono::Duration::seconds(2);
    store
        .record(&observed(Verdict::Verified, last_success))
        .await
        .unwrap();

    let reoffer = |key: String| {
        let harness = &harness;
        let owner = owner.clone();
        let offer_uri = offer_uri.clone();
        async move {
            let (status, offer) = harness
                .send(
                    Method::PUT,
                    &offer_uri,
                    Some(&owner),
                    &[("idempotency-key", &key), ("if-match", "*")],
                    Some(&json!({ "location": "us-east-1" })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{offer}");
            harness.drain(org).await;
            harness.get(&offer_uri, &owner).await.1
        }
    };
    // Nothing shows the account's credentials work since the refusal: it may
    // be a broken key, so the model stays offered.
    let offer = reoffer(rerun()).await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "offered".into())));

    // Another model of the account is called successfully: the refusal is
    // the model's.
    let mut foundation_key = key.clone();
    foundation_key.model = SONNET_FOUNDATION.into();
    store
        .record(&Observation {
            key: foundation_key,
            ..observed(Verdict::Verified, refused_at + chrono::Duration::seconds(1))
        })
        .await
        .unwrap();
    let offer = reoffer(rerun()).await;
    assert_eq!(
        statuses(&offer),
        [
            (SONNET_FOUNDATION.into(), "offered".into()),
            (SONNET.into(), "refused".into()),
        ]
    );
    let (routes, _) = harness.catalog(org).await;
    assert!(!routes.iter().any(|(alias, _, _)| alias == SONNET));

    let (status, listed) = harness
        .get(
            &uri(
                org,
                &format!("discovered-models?adapter_instance_key={KEY}"),
            ),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let items = listed["items"].as_array().unwrap();
    let sonnet = items.iter().find(|m| m["model"] == SONNET).unwrap();
    assert_eq!(sonnet["evidence"]["state"], "refused");
    assert_eq!(sonnet["evidence"]["reason"], "access_denied");
    assert_eq!(sonnet["evidence"]["source"], "traffic");
    assert_eq!(sonnet["evidence"]["provider_status"], 403);
    assert_eq!(sonnet["evidence"]["provider_code"], "AccessDeniedException");
    assert_eq!(sonnet["evidence"]["deployment"], SONNET_DEPLOYMENT);
    // The last success is kept.
    let verified_at: chrono::DateTime<Utc> =
        serde_json::from_value(sonnet["evidence"]["verified_at"].clone()).unwrap();
    assert_eq!(
        verified_at.timestamp_micros(),
        last_success.timestamp_micros()
    );
    let foundation = items
        .iter()
        .find(|m| m["model"] == SONNET_FOUNDATION)
        .unwrap();
    assert_eq!(foundation["evidence"]["state"], "verified");

    // Refused again, after the other model's success: the refusal stays
    // confirmed, so the model is not offered again.
    store
        .record(&observed(
            Verdict::Refused(EvidenceReason::AccessDenied),
            refused_at + chrono::Duration::seconds(2),
        ))
        .await
        .unwrap();
    let offer = reoffer(rerun()).await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "refused".into())));
    let (routes, _) = harness.catalog(org).await;
    assert!(!routes.iter().any(|(alias, _, _)| alias == SONNET));

    // Verified again: offered again.
    store
        .record(&observed(
            Verdict::Verified,
            refused_at + chrono::Duration::seconds(3),
        ))
        .await
        .unwrap();
    let offer = reoffer(rerun()).await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "offered".into())));
    let (routes, _) = harness.catalog(org).await;
    assert!(routes.iter().any(|(alias, _, _)| alias == SONNET));

    // A late success ends the refusal run it follows: refused at T1, the
    // other model succeeds at T2, this one succeeds at T3 (recorded last) and
    // is refused again at T4. The other model's T2 success predates the new
    // run, so it does not confirm the T4 refusal.
    let at = |seconds| refused_at + chrono::Duration::seconds(seconds);
    let mut foundation = observed(Verdict::Verified, at(11));
    foundation.key.model = SONNET_FOUNDATION.into();
    for observation in [
        observed(Verdict::Refused(EvidenceReason::AccessDenied), at(10)),
        foundation,
        observed(Verdict::Refused(EvidenceReason::AccessDenied), at(13)),
        observed(Verdict::Verified, at(12)),
    ] {
        store.record(&observation).await.unwrap();
    }
    let offer = reoffer(rerun()).await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "offered".into())));
}

/// A read-only check during discovery found something the tenant must do
/// before the account can call a model (D-018), such as accept its Bedrock
/// agreement: the model is not offered, an offered one is removed, and a
/// later check that finds nothing in the way offers it again.
#[tokio::test]
async fn models_needing_action_leave_the_offer_until_entitled() {
    use gateway::{
        domain::model_evidence::{
            EvidenceKey, EvidenceReason, EvidenceSource, Observation, Verdict,
        },
        persistence::model_evidence::ModelEvidenceStore,
    };
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("auto-offer-action").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    seed_prices(&harness.pool, "0.8").await;
    harness
        .accounts
        .set(org, KEY, Listing::Models(vec![SONNET_FOUNDATION, SONNET]));
    let offer_uri = uri(org, &format!("auto-offers/{KEY}"));
    let mut run = 0;
    let mut reoffer = || {
        run += 1;
        let key = format!("action-{run}");
        let harness = &harness;
        let owner = owner.clone();
        let offer_uri = offer_uri.clone();
        async move {
            let (status, offer) = harness
                .send(
                    Method::PUT,
                    &offer_uri,
                    Some(&owner),
                    &[("idempotency-key", &key), ("if-match", "*")],
                    Some(&json!({ "location": "us-east-1" })),
                )
                .await;
            assert!(status.is_success(), "{offer}");
            harness.drain(org).await;
            harness.get(&offer_uri, &owner).await.1
        }
    };
    let offer = reoffer().await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "offered".into())));

    let store = ModelEvidenceStore::new(harness.pool.clone());
    let checked = |verdict, seconds| Observation {
        key: EvidenceKey {
            organization_id: org,
            adapter_instance_key: KEY.into(),
            family: "bedrock".into(),
            model: SONNET.into(),
        },
        verdict,
        source: EvidenceSource::ControlPlane,
        deployment: None,
        provider_status: None,
        provider_code: None,
        observed_at: Utc::now() + chrono::Duration::seconds(seconds),
    };
    store
        .record(&checked(
            Verdict::RequiresAction(EvidenceReason::AgreementRequired),
            1,
        ))
        .await
        .unwrap();
    let offer = reoffer().await;
    assert_eq!(
        statuses(&offer),
        [
            (SONNET_FOUNDATION.into(), "offered".into()),
            (SONNET.into(), "requires_action".into()),
        ]
    );
    let (routes, _) = harness.catalog(org).await;
    assert!(!routes.iter().any(|(alias, _, _)| alias == SONNET));

    // The tenant accepts the agreement; the next check finds nothing in the
    // way.
    store.record(&checked(Verdict::Entitled, 2)).await.unwrap();
    let offer = reoffer().await;
    assert!(statuses(&offer).contains(&(SONNET.into(), "offered".into())));
    let (routes, _) = harness.catalog(org).await;
    assert!(routes.iter().any(|(alias, _, _)| alias == SONNET));
}
