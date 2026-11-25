#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for model discovery: the worker's scheduled and manual
//! jobs against fake provider accounts, the lease fence, and the admin lane
//! (driven through the real router, RBAC, idempotency, and audit).
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
use chrono::{Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        catalog::ProviderFamily,
        discovery::{DeployableModel, DeploymentDetails, DiscoveredModel, SyncStatus},
        identity::MembershipRole,
        inference::{
            ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, InferenceContext,
            ProviderError, ProviderErrorCategory,
        },
        keys::ApiKeyPepper,
        notifications::{NewNotification, NotificationKind},
    },
    jobs::discovery::{DiscoveryRunner, ProviderAccounts},
    persistence::{
        AdminStore, AuthStore, IdentityStore,
        catalog_sync::{CatalogSyncStore, MAX_SYNC_ATTEMPTS},
        notifications::NotificationChange,
        price_sync::PriceSyncStore,
    },
    provider::{ChatEventStream, InferenceProviderAdapter},
    transport::catalog_sync::router,
};
use gateway::{
    domain::model_evidence::{Entitlement, EvidenceKey, EvidenceReason, Observation, Verdict},
    persistence::model_evidence::ModelEvidenceStore,
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
const TEST_PEPPER: &[u8] = b"catalog-sync-integration-pepper";
const HMAC_KEY: &[u8] = b"catalog-sync-integration-hmac-0032";

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

fn price(deployment: &str) -> Value {
    json!({
        "deployment": deployment, "operation": "chat",
        "input_minor_per_token": 1, "output_minor_per_token": 2,
        "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
        "other_minor_per_token": 0, "currency": "USD", "source": "test",
        "source_version": "v1", "confidence": "known"
    })
}

/// Two accounts: `openai-main` deploys `gpt` twice and `gpt-old` once;
/// `anthropic-main` deploys `claude`.
fn manifest() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "discovery-1"},
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
        "prices": [price("gpt-a"), price("gpt-b"), price("gpt-legacy"), price("claude-a")],
        "routes": [{"alias": "fast", "operation": "chat", "deployment": "gpt-a"}]
    }))
    .unwrap()
}

/// What a fake account's listing returns.
#[derive(Clone)]
enum Listing {
    Models(Vec<&'static str>),
    /// Models with what the listing's control-plane check found.
    Checked(Vec<(&'static str, Entitlement)>),
    /// Deployments with their details.
    Deployments(Vec<(&'static str, Option<DeploymentDetails>)>),
    /// Deployments with their details, and the account's catalog of
    /// deployable models or why it could not be read.
    Catalog(
        Vec<(&'static str, Option<DeploymentDetails>)>,
        Result<Vec<DeployableModel>, ProviderErrorCategory>,
    ),
    /// Deployments listed at once, and a catalog read that never returns.
    CatalogHang(Vec<(&'static str, Option<DeploymentDetails>)>),
    /// Models with their check, and what probing each returns; probes are
    /// logged in `PROBES` under the organization.
    Probed(
        OrganizationId,
        Vec<(&'static str, Entitlement, Result<(), ProviderErrorCategory>)>,
    ),
    Error(ProviderErrorCategory),
    Hang,
}

struct FakeAdapter(Listing);

/// The models each organization's fake accounts were probed for.
static PROBES: Mutex<Vec<(OrganizationId, String)>> = Mutex::new(Vec::new());

fn probed(org: OrganizationId) -> Vec<String> {
    let mut models = PROBES
        .lock()
        .unwrap()
        .iter()
        .filter(|(probed, _)| *probed == org)
        .map(|(_, model)| model.clone())
        .collect::<Vec<_>>();
    models.sort();
    models
}

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
        match &self.0 {
            Listing::Models(models) => Ok(models
                .iter()
                .map(|model| DiscoveredModel::new(model, Some(&model.to_uppercase())).unwrap())
                .collect()),
            Listing::Checked(models) => Ok(models
                .iter()
                .map(|(model, entitlement)| {
                    DiscoveredModel::new(model, None)
                        .unwrap()
                        .with_entitlement(Some(*entitlement))
                })
                .collect()),
            Listing::Deployments(models)
            | Listing::Catalog(models, _)
            | Listing::CatalogHang(models) => Ok(models
                .iter()
                .map(|(model, details)| {
                    DiscoveredModel::new(model, None)
                        .unwrap()
                        .with_deployment(details.clone())
                })
                .collect()),
            Listing::Probed(_, models) => Ok(models
                .iter()
                .map(|(model, entitlement, _)| {
                    DiscoveredModel::new(model, None)
                        .unwrap()
                        .with_entitlement(Some(*entitlement))
                })
                .collect()),
            Listing::Error(category) => Err(ProviderError::new(*category)),
            Listing::Hang => std::future::pending().await,
        }
    }

    fn probes_models(&self) -> bool {
        matches!(self.0, Listing::Probed(..))
    }

    async fn probe_model(&self, model: &str) -> Result<(), ProviderError> {
        let Listing::Probed(org, models) = &self.0 else {
            return Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ));
        };
        PROBES.lock().unwrap().push((*org, model.to_owned()));
        models
            .iter()
            .find(|(listed, _, _)| *listed == model)
            .map_or(Ok(()), |(_, _, result)| result.map_err(ProviderError::new))
    }

    async fn list_deployable_models(&self) -> Result<Option<Vec<DeployableModel>>, ProviderError> {
        match &self.0 {
            Listing::Catalog(_, Ok(models)) => Ok(Some(models.clone())),
            Listing::Catalog(_, Err(category)) => Err(ProviderError::new(*category)),
            Listing::CatalogHang(_) => std::future::pending().await,
            _ => Ok(None),
        }
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

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    catalog: CatalogStore,
    store: CatalogSyncStore,
    accounts: Arc<FakeAccounts>,
    runner: DiscoveryRunner,
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
        let crypto = AuthCrypto::new(HMAC_KEY, &[31_u8; 32]).unwrap();
        let store = CatalogSyncStore::new(pool.clone());
        let accounts = Arc::new(FakeAccounts::default());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            store.clone(),
            PriceSyncStore::new(pool.clone()),
        );
        Self {
            catalog: CatalogStore::new(pool.clone()),
            runner: DiscoveryRunner::new(store.clone(), accounts.clone())
                .with_evidence(ModelEvidenceStore::new(pool.clone()))
                .with_notifications(),
            accounts,
            store,
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

    /// An organization with the test manifest active.
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

    async fn request_sync(
        &self,
        org: OrganizationId,
        session: &(String, String),
        idempotency_key: &str,
        adapter_instance_key: &str,
    ) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &uri(org, "sync-jobs"),
            Some(session),
            &[("idempotency-key", idempotency_key)],
            Some(&json!({ "adapter_instance_key": adapter_instance_key })),
        )
        .await
    }

    /// Queues a manual listing of `openai`-family `adapter_instance_key`.
    async fn queue(&self, org: OrganizationId, adapter_instance_key: &str) {
        let user = self.user(org, "organization_owner").await;
        let mut conn = self.pool.acquire().await.unwrap();
        CatalogSyncStore::insert_manual(
            &mut conn,
            org,
            Uuid::new_v4(),
            adapter_instance_key,
            "openai",
            user,
        )
        .await
        .unwrap()
        .expect("no open job");
    }

    /// Runs worker ticks until `org` has a job and none is open. Accounts
    /// of other tests' organizations are scheduled too, so this may take
    /// several ticks.
    /// Listing notifications by account, and whether each is resolved.
    async fn sync_notifications(&self, org: OrganizationId) -> Vec<(String, bool)> {
        sqlx::query_as(
            "SELECT subject, resolved_at IS NOT NULL FROM gateway.notification
             WHERE organization_id = $1 AND kind = 'provider_account.sync_failing'
             ORDER BY subject, created_at",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn drain(&self, org: OrganizationId) {
        for _ in 0..500 {
            self.runner.run_once().await.unwrap();
            let (jobs, open): (i64, i64) = sqlx::query_as(
                "SELECT count(*), count(*) FILTER (WHERE status IN ('queued', 'running'))
                 FROM gateway.catalog_sync_job WHERE organization_id = $1",
            )
            .bind(org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            if jobs > 0 && open == 0 {
                return;
            }
        }
        panic!("discovery jobs for the organization never finished");
    }

    async fn jobs(&self, org: OrganizationId) -> Vec<(String, String, String, Option<String>)> {
        sqlx::query_as(
            "SELECT adapter_instance_key, trigger, status, error_code
             FROM gateway.catalog_sync_job WHERE organization_id = $1
             ORDER BY created_at, adapter_instance_key",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/catalog/{path}", org.0)
}

fn models(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["model"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn scheduled_and_manual_listings_record_models_and_drift() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, release) = harness.org_with_release("discovery-flow").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Models(vec!["gpt", "gpt-new", "gpt-old"]),
    );
    harness.accounts.set(
        org,
        "anthropic-main",
        Listing::Error(ProviderErrorCategory::Authentication),
    );

    // Drift is unknown until a listing succeeds.
    let (status, drift) = harness.get(&uri(org, "discovery-drift"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{drift}");
    assert_eq!(drift["release_id"], release.to_string());
    for account in drift["accounts"].as_array().unwrap() {
        assert_eq!(account["last_job"], Value::Null);
        assert_eq!(account["undeployed_models"], Value::Null);
        assert_eq!(account["unlisted_deployments"], Value::Null);
    }

    harness.drain(org).await;
    assert_eq!(
        harness.jobs(org).await,
        vec![
            (
                "anthropic-main".into(),
                "scheduled".into(),
                "failed".into(),
                Some("authentication".into())
            ),
            (
                "openai-main".into(),
                "scheduled".into(),
                "succeeded".into(),
                None
            ),
        ]
    );
    // The failed account is a warning for the organization; the listed one
    // is not.
    assert_eq!(
        harness.sync_notifications(org).await,
        [("anthropic-main".to_owned(), false)]
    );
    // A second tick inside the interval schedules nothing for the org.
    harness.runner.run_once().await.unwrap();
    assert_eq!(harness.jobs(org).await.len(), 2);

    let (status, page) = harness.get(&uri(org, "discovered-models"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(models(&page), ["gpt", "gpt-new", "gpt-old"]);
    assert_eq!(page["items"][0]["display_name"], "GPT");
    assert_eq!(page["items"][0]["deployed"], true);
    assert_eq!(page["items"][1]["deployed"], false);
    assert_eq!(page["items"][0]["provider_family"], "openai");
    // OpenAI's listing states no operations, and is the account's own.
    assert_eq!(page["items"][0]["operations"], Value::Null);
    assert_eq!(page["items"][0]["availability"], "listed");

    let (_, drift) = harness.get(&uri(org, "discovery-drift"), &owner).await;
    let accounts = drift["accounts"].as_array().unwrap();
    assert_eq!(accounts[0]["adapter_instance_key"], "anthropic-main");
    assert_eq!(accounts[0]["last_job"]["status"], "failed");
    assert_eq!(accounts[0]["undeployed_models"], Value::Null);
    assert_eq!(accounts[1]["adapter_instance_key"], "openai-main");
    assert_eq!(accounts[1]["last_job"]["models_listed"], 3);
    assert_eq!(accounts[1]["last_job"]["models_added"], 3);
    assert_eq!(accounts[1]["undeployed_models"], 1);
    assert_eq!(accounts[1]["unlisted_deployments"], json!([]));

    // A manual sync sees `gpt-old` gone and `claude` listed.
    harness
        .accounts
        .set(org, "openai-main", Listing::Models(vec!["gpt", "gpt-new"]));
    harness
        .accounts
        .set(org, "anthropic-main", Listing::Models(vec!["claude-next"]));
    let (status, job) = harness
        .request_sync(org, &owner, "sync-1", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["status"], "queued");
    assert_eq!(job["trigger"], "manual");
    assert_eq!(job["provider_family"], "openai");
    // One open job per account; the replay returns the recorded response.
    let (status, conflict) = harness
        .request_sync(org, &owner, "sync-2", "openai-main")
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "catalog_sync_in_progress");
    let (status, replay) = harness
        .request_sync(org, &owner, "sync-1", "openai-main")
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, job);
    let (status, _) = harness
        .request_sync(org, &owner, "sync-3", "anthropic-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    harness.drain(org).await;
    // Listed again: resolved.
    assert_eq!(
        harness.sync_notifications(org).await,
        [("anthropic-main".to_owned(), true)]
    );
    let job_id = job["id"].as_str().unwrap();
    let (status, job) = harness
        .get(&uri(org, &format!("sync-jobs/{job_id}")), &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["status"], "succeeded");
    assert_eq!(job["attempts"], 1);
    assert_eq!(job["models_listed"], 2);
    assert_eq!(job["models_added"], 0);
    assert_eq!(job["models_missing"], 1);

    let (_, missing) = harness
        .get(&uri(org, "discovered-models?state=missing"), &owner)
        .await;
    assert_eq!(models(&missing), ["gpt-old"]);
    assert!(missing["items"][0]["missing_since"].is_string());
    let (_, undeployed) = harness
        .get(
            &uri(org, "discovered-models?state=listed&deployed=false"),
            &owner,
        )
        .await;
    assert_eq!(models(&undeployed), ["claude-next", "gpt-new"]);

    let (_, drift) = harness.get(&uri(org, "discovery-drift"), &owner).await;
    let accounts = drift["accounts"].as_array().unwrap();
    assert_eq!(accounts[0]["undeployed_models"], 1);
    assert_eq!(
        accounts[0]["unlisted_deployments"],
        json!([{"deployment": "claude-a", "model": "claude", "missing_since": null}])
    );
    assert_eq!(accounts[1]["undeployed_models"], 1);
    let unlisted = accounts[1]["unlisted_deployments"].as_array().unwrap();
    assert_eq!(unlisted.len(), 1);
    assert_eq!(unlisted[0]["deployment"], "gpt-legacy");
    assert!(unlisted[0]["missing_since"].is_string());

    // A model listed again is no longer missing.
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Models(vec!["gpt", "gpt-new", "gpt-old"]),
    );
    let (status, _) = harness
        .request_sync(org, &owner, "sync-4", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    harness.drain(org).await;
    let (_, missing) = harness
        .get(&uri(org, "discovered-models?state=missing"), &owner)
        .await;
    assert!(models(&missing).is_empty());

    // Newest first, filterable, and paged.
    let (status, page) = harness
        .get(
            &uri(
                org,
                "sync-jobs?adapter_instance_key=openai-main&page_size=2",
            ),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["id"], job_id);
    let token = page["next_page_token"].as_str().unwrap();
    let (status, rest) = harness
        .get(
            &uri(
                org,
                &format!(
                    "sync-jobs?adapter_instance_key=openai-main&page_size=2&page_token={token}"
                ),
            ),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert_eq!(rest["items"][0]["trigger"], "scheduled");
    assert_eq!(rest["next_page_token"], Value::Null);
    // A token is bound to its filters.
    let (status, mismatch) = harness
        .get(
            &uri(org, &format!("sync-jobs?page_size=2&page_token={token}")),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{mismatch}");

    let (status, first) = harness
        .get(&uri(org, "discovered-models?page_size=2"), &owner)
        .await;
    assert_eq!(status, StatusCode::OK);
    let token = first["next_page_token"].as_str().unwrap();
    let (_, second) = harness
        .get(
            &uri(
                org,
                &format!("discovered-models?page_size=2&page_token={token}"),
            ),
            &owner,
        )
        .await;
    assert_eq!(
        [models(&first), models(&second)].concat(),
        ["claude-next", "gpt", "gpt-new", "gpt-old"]
    );

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'catalog_sync_job' AND outcome = 'succeeded'",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(actions, vec!["catalog_sync_job.requested"; 3]);
}

#[tokio::test]
async fn unsupported_unconfigured_and_timed_out_accounts_record_fixed_codes() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-codes").await;
    harness.accounts.set(
        org,
        "anthropic-main",
        Listing::Error(ProviderErrorCategory::UnsupportedFeature),
    );
    // `openai-main` has no credential.
    harness.drain(org).await;
    let jobs = harness.jobs(org).await;
    assert_eq!(
        jobs,
        vec![
            (
                "anthropic-main".into(),
                "scheduled".into(),
                "unsupported".into(),
                Some("unsupported_feature".into())
            ),
            (
                "openai-main".into(),
                "scheduled".into(),
                "failed".into(),
                Some("not_configured".into())
            ),
        ]
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.catalog_discovered_model WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(stored, 0);
    // Only the failure warns.
    assert_eq!(
        harness.sync_notifications(org).await,
        [("openai-main".to_owned(), false)]
    );
    // Then configured, but listing is unsupported for it: no failure now,
    // so the earlier warning is resolved, not left open.
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Error(ProviderErrorCategory::UnsupportedFeature),
    );
    let admin = harness.user(org, "organization_owner").await;
    let admin = harness.session(admin).await;
    let (status, _) = harness
        .request_sync(org, &admin, "unsupported", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    harness.drain(org).await;
    assert_eq!(
        harness.sync_notifications(org).await,
        [("openai-main".to_owned(), true)]
    );

    // A listing that never returns is abandoned at the timeout.
    let runner = DiscoveryRunner::new(harness.store.clone(), harness.accounts.clone())
        .with_listing_timeout(StdDuration::from_millis(50));
    harness.accounts.set(org, "openai-main", Listing::Hang);
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    let (status, job) = harness
        .request_sync(org, &owner, "hang", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    for _ in 0..500 {
        runner.run_once().await.unwrap();
        let (_, job) = harness
            .get(
                &uri(org, &format!("sync-jobs/{}", job["id"].as_str().unwrap())),
                &owner,
            )
            .await;
        if job["status"] != "queued" && job["status"] != "running" {
            assert_eq!(job["status"], "failed");
            assert_eq!(job["error_code"], "timeout");
            return;
        }
    }
    panic!("the hanging listing never timed out");
}

#[tokio::test]
async fn an_expired_claim_is_retried_fenced_then_exhausted() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-lease").await;
    // An even older job of a suspended organization waits: its credentials
    // are not used.
    let (suspended, _) = harness.org_with_release("discovery-suspended").await;
    sqlx::query("UPDATE gateway.organization SET suspended_at = now() WHERE id = $1")
        .bind(suspended.0)
        .execute(&harness.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.catalog_sync_job
             (id, organization_id, adapter_instance_key, family, trigger, created_at)
         VALUES ($1, $2, 'openai-main', 'openai', 'manual', '1999-01-01T00:00:00Z')",
    )
    .bind(Uuid::new_v4())
    .bind(suspended.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    // Created long ago, so it is the oldest open job and claimed first.
    let job = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.catalog_sync_job
             (id, organization_id, adapter_instance_key, family, trigger, created_at)
         VALUES ($1, $2, 'openai-main', 'openai', 'manual', '2000-01-01T00:00:00Z')",
    )
    .bind(job)
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap();

    let now = Utc::now();
    let first = harness
        .store
        .claim(now, now + Duration::minutes(5), 1)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, job);
    assert_eq!(first[0].attempt, 1);
    // An unexpired claim is not taken again.
    let later = now + Duration::minutes(1);
    assert!(
        harness
            .store
            .claim(later, later + Duration::minutes(5), 1)
            .await
            .unwrap()
            .iter()
            .all(|claimed| claimed.id != job)
    );

    let expired = now + Duration::minutes(6);
    let second = harness
        .store
        .claim(expired, expired + Duration::minutes(5), 1)
        .await
        .unwrap();
    assert_eq!(second[0].id, job);
    assert_eq!(second[0].attempt, 2);
    // The first worker's result is fenced off.
    let listed = [DiscoveredModel::new("gpt", None).unwrap()];
    assert!(
        !harness
            .store
            .finish_listed(&first[0], &listed, None, expired, None)
            .await
            .unwrap()
    );
    // So is its failure, and the notification it would raise with it: an
    // older result never reopens what a newer one recorded.
    let raise = NotificationChange::Raise(
        org,
        NewNotification::new(
            NotificationKind::ProviderAccountSyncFailing,
            first[0].adapter_instance_key.clone(),
            "Listing failed",
            "",
        ),
    );
    assert!(
        !harness
            .store
            .finish_unlisted(
                &first[0],
                SyncStatus::Failed,
                "timeout",
                expired,
                Some(&raise)
            )
            .await
            .unwrap()
    );
    assert!(harness.sync_notifications(org).await.is_empty());

    // Past the last attempt's lease, the job fails instead of running again.
    let mut at = expired;
    for attempt in 3..=MAX_SYNC_ATTEMPTS {
        at += Duration::minutes(6);
        let claimed = harness
            .store
            .claim(at, at + Duration::minutes(5), 1)
            .await
            .unwrap();
        assert_eq!(claimed[0].attempt, attempt);
    }
    at += Duration::minutes(6);
    assert!(
        harness
            .store
            .claim(at, at + Duration::minutes(5), 1)
            .await
            .unwrap()
            .iter()
            .all(|claimed| claimed.id != job)
    );
    let (status, error_code): (String, Option<String>) =
        sqlx::query_as("SELECT status, error_code FROM gateway.catalog_sync_job WHERE id = $1")
            .bind(job)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(error_code.as_deref(), Some("attempts_exhausted"));
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.catalog_discovered_model WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn reads_need_catalog_read_and_requests_need_catalog_write() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-rbac").await;

    for (role, can_write) in [
        ("organization_admin", true),
        ("operator", false),
        ("auditor", false),
        ("read_only_member", false),
    ] {
        let user = harness.user(org, role).await;
        let session = harness.session(user).await;
        for path in [
            "sync-jobs",
            "discovered-models",
            "deployable-models",
            "discovery-drift",
        ] {
            let (status, body) = harness.get(&uri(org, path), &session).await;
            assert_eq!(status, StatusCode::OK, "{role} {path}: {body}");
        }
        let (status, body) = harness
            .request_sync(org, &session, &format!("rbac-{role}"), "openai-main")
            .await;
        if can_write {
            assert_eq!(status, StatusCode::ACCEPTED, "{role}: {body}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "{role}: {body}");
        }
    }

    // Another organization's member sees nothing here.
    let (other, _) = harness.org_with_release("discovery-rbac-other").await;
    let outsider = harness.user(other, "organization_owner").await;
    let outsider = harness.session(outsider).await;
    let (status, _) = harness.get(&uri(org, "sync-jobs"), &outsider).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    let (status, body) = harness
        .request_sync(org, &owner, "unknown", "gemini-main")
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_provider_account");
    let (status, body) = harness.request_sync(org, &owner, "bad", "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = harness
        .send(
            Method::POST,
            &uri(org, "sync-jobs"),
            Some(&owner),
            &[],
            Some(&json!({"adapter_instance_key": "openai-main"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = harness
        .send(
            Method::POST,
            &uri(org, "sync-jobs"),
            Some(&owner),
            &[("idempotency-key", "extra")],
            Some(&json!({"adapter_instance_key": "openai-main", "force": true})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = harness
        .get(&uri(org, &format!("sync-jobs/{}", Uuid::new_v4())), &owner)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for query in [
        "state=gone",
        "deployed=maybe",
        "adapter_instance_key=",
        "extra=1",
    ] {
        let (status, body) = harness
            .get(&uri(org, &format!("discovered-models?{query}")), &owner)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    let (status, _) = harness
        .send(Method::GET, &uri(org, "sync-jobs"), None, &[], None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Without an active release there is no drift to report.
    let bare = harness.org("discovery-bare").await;
    let bare_owner = harness.user(bare, "organization_owner").await;
    let bare_owner = harness.session(bare_owner).await;
    let (status, _) = harness
        .get(&uri(bare, "discovery-drift"), &bare_owner)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = harness
        .request_sync(bare, &bare_owner, "bare", "openai-main")
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn a_key_redeclared_with_another_family_is_synced_at_once() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-family").await;
    // A recent sync of `openai-main` from when it was another family's key.
    sqlx::query(
        "INSERT INTO gateway.catalog_sync_job
             (id, organization_id, adapter_instance_key, family, trigger, status,
              models_listed, models_added, models_missing, started_at, finished_at)
         VALUES ($1, $2, 'openai-main', 'openai-compatible', 'scheduled', 'succeeded',
                 0, 0, 0, now(), now())",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    harness
        .accounts
        .set(org, "openai-main", Listing::Models(vec!["gpt"]));
    harness.drain(org).await;
    let scheduled: Vec<(String, String)> = sqlx::query_as(
        "SELECT family, status FROM gateway.catalog_sync_job
         WHERE organization_id = $1 AND adapter_instance_key = 'openai-main'
           AND family = 'openai'",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(scheduled, vec![("openai".into(), "succeeded".into())]);
}

/// A listing's control-plane checks are recorded as evidence (D-018), but
/// never replace what a call showed: a model verified by traffic stays
/// verified when a later check finds the permission missing.
#[tokio::test]
async fn listed_deployments_keep_their_latest_details() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-details").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    let details = |sku: &str, capacity: i64| {
        DeploymentDetails::new(
            Some("OpenAI"),
            Some("gpt-6.1"),
            Some("2026-09-29"),
            Some(sku),
            Some(capacity),
        )
    };
    let listed = |page: &Value| {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| {
                (
                    model["model"].as_str().unwrap().to_owned(),
                    model["deployment"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Deployments(vec![
            ("gpt", details("GlobalStandard", 125)),
            ("other", None),
        ]),
    );
    harness.drain(org).await;
    let (status, page) = harness.get(&uri(org, "discovered-models"), &owner).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed(&page),
        [
            (
                "gpt".to_owned(),
                json!({"format": "OpenAI", "model": "gpt-6.1", "version": "2026-09-29",
                       "sku": "GlobalStandard", "capacity": 125})
            ),
            ("other".to_owned(), Value::Null),
        ]
    );

    // The next listing's details replace them, and a deployment that
    // reports none has none.
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Deployments(vec![
            ("gpt", None),
            ("other", details("ProvisionedManaged", 15)),
        ]),
    );
    let (status, job) = harness
        .request_sync(org, &owner, "sync-details", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    harness.drain(org).await;
    let (_, page) = harness.get(&uri(org, "discovered-models"), &owner).await;
    assert_eq!(listed(&page)[0], ("gpt".to_owned(), Value::Null));
    assert_eq!(listed(&page)[1].1["sku"], "ProvisionedManaged");
    assert_eq!(listed(&page)[1].1["capacity"], 15);
}

#[tokio::test]
async fn deployable_models_show_the_deployments_serving_them() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-deployable").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    let deployable = |format: &str, model: &str, version: &str| {
        DeployableModel::new(format, model, version)
            .unwrap()
            .with_terms(
                Some("GenerallyAvailable"),
                version == "2",
                &["GlobalStandard"],
                Some(5000),
                None,
            )
    };
    let opus = DeploymentDetails::new(
        Some("anthropic"),
        Some("claude-opus-5-5"),
        Some("2"),
        Some("GlobalStandard"),
        Some(50),
    );
    let deployments = vec![("opus", opus.clone()), ("opus-copy", opus), ("plain", None)];
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Catalog(
            deployments.clone(),
            Ok(vec![
                deployable("Anthropic", "claude-opus-5-5", "2"),
                deployable("Anthropic", "claude-opus-5-5", "1"),
                DeployableModel::new("OpenAI", "gpt-6", "1")
                    .unwrap()
                    .with_terms(Some("Preview"), false, &["GlobalStandard"], None, None)
                    .with_operations(&[
                        gateway::domain::catalog::CatalogOperation::Chat,
                        gateway::domain::catalog::CatalogOperation::ChatStream,
                    ]),
            ]),
        ),
    );
    harness.drain(org).await;
    let summary = |page: &Value| {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| {
                (
                    format!(
                        "{} {}",
                        model["model"].as_str().unwrap(),
                        model["version"].as_str().unwrap()
                    ),
                    model["deployments"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let (status, page) = harness.get(&uri(org, "deployable-models"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        summary(&page),
        [
            ("claude-opus-5-5 1".to_owned(), json!([])),
            ("claude-opus-5-5 2".to_owned(), json!(["opus", "opus-copy"])),
            ("gpt-6 1".to_owned(), json!([])),
        ]
    );
    let item = &page["items"][1];
    assert_eq!(item["provider_family"], "openai");
    assert_eq!(item["lifecycle_status"], "GenerallyAvailable");
    assert_eq!(item["default_version"], true);
    assert_eq!(item["skus"], json!(["GlobalStandard"]));
    assert_eq!(item["max_capacity"], 5000);

    let (_, page) = harness
        .get(
            &uri(org, "deployable-models?deployed=false&page_size=1"),
            &owner,
        )
        .await;
    assert_eq!(
        summary(&page),
        [("claude-opus-5-5 1".to_owned(), json!([]))]
    );
    let token = page["next_page_token"].as_str().unwrap().to_owned();
    let (_, page) = harness
        .get(
            &uri(
                org,
                &format!("deployable-models?deployed=false&page_size=1&page_token={token}"),
            ),
            &owner,
        )
        .await;
    assert_eq!(summary(&page), [("gpt-6 1".to_owned(), json!([]))]);
    assert_eq!(page["next_page_token"], Value::Null);

    // Lifecycle (without case) and operation filters.
    for (filters, expected) in [
        ("lifecycle_status=preview", vec!["gpt-6 1"]),
        (
            "lifecycle_status=GenerallyAvailable",
            vec!["claude-opus-5-5 1", "claude-opus-5-5 2"],
        ),
        ("operation=chat_stream", vec!["gpt-6 1"]),
        ("operation=embeddings", vec![]),
    ] {
        let (status, page) = harness
            .get(&uri(org, &format!("deployable-models?{filters}")), &owner)
            .await;
        assert_eq!(status, StatusCode::OK, "{filters}: {page}");
        let models: Vec<String> = summary(&page).into_iter().map(|(model, _)| model).collect();
        assert_eq!(models, expected, "{filters}");
    }
    for invalid in ["operation=generate", "lifecycle_status="] {
        let (status, _) = harness
            .get(&uri(org, &format!("deployable-models?{invalid}")), &owner)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    }

    // A catalog that cannot be read leaves the last one.
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Catalog(
            deployments.clone(),
            Err(ProviderErrorCategory::Authorization),
        ),
    );
    let (status, job) = harness
        .request_sync(org, &owner, "sync-deployable-1", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    harness.drain(org).await;
    let (_, page) = harness.get(&uri(org, "deployable-models"), &owner).await;
    assert_eq!(summary(&page).len(), 3);

    // A catalog read replaces it.
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Catalog(deployments, Ok(vec![deployable("OpenAI", "gpt-6", "1")])),
    );
    let (status, job) = harness
        .request_sync(org, &owner, "sync-deployable-2", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    harness.drain(org).await;
    let (_, page) = harness
        .get(&uri(org, "deployable-models?deployed=true"), &owner)
        .await;
    assert_eq!(summary(&page), []);
    let (_, page) = harness.get(&uri(org, "deployable-models"), &owner).await;
    assert_eq!(summary(&page), [("gpt-6 1".to_owned(), json!([]))]);

    // A catalog read that runs out of time keeps the listing it follows,
    // and the last catalog.
    let runner = DiscoveryRunner::new(harness.store.clone(), harness.accounts.clone())
        .with_listing_timeout(StdDuration::from_millis(50));
    harness.accounts.set(
        org,
        "openai-main",
        Listing::CatalogHang(vec![("gpt-new", None)]),
    );
    let (status, job) = harness
        .request_sync(org, &owner, "sync-deployable-3", "openai-main")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    runner.run_once().await.unwrap();
    let (_, job) = harness
        .get(
            &uri(org, &format!("sync-jobs/{}", job["id"].as_str().unwrap())),
            &owner,
        )
        .await;
    assert_eq!(job["status"], "succeeded", "{job}");
    let (_, page) = harness
        .get(&uri(org, "discovered-models?state=listed"), &owner)
        .await;
    assert_eq!(page["items"][0]["model"], "gpt-new");
    let (_, page) = harness.get(&uri(org, "deployable-models"), &owner).await;
    assert_eq!(summary(&page), [("gpt-6 1".to_owned(), json!([]))]);
}

#[tokio::test]
async fn listed_entitlements_are_recorded_beneath_traffic_evidence() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-entitlement").await;
    let evidence = |model: &'static str| {
        let pool = harness.pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, String)>(
                "SELECT state, reason, source FROM gateway.catalog_model_evidence
                 WHERE organization_id = $1 AND adapter_instance_key = 'openai-main'
                   AND family = 'openai' AND model = $2",
            )
            .bind(org.0)
            .bind(model)
            .fetch_optional(&pool)
            .await
            .unwrap()
        }
    };
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Checked(vec![
            ("gpt", Entitlement::Entitled),
            ("gpt-new", Entitlement::Entitled),
            ("gpt-old", Entitlement::PermissionMissing),
            ("gpt-terms", Entitlement::AgreementRequired),
            ("gpt-restricted", Entitlement::NotEntitled),
        ]),
    );
    harness.drain(org).await;
    // Bedrock's availability answers (migration 0077).
    for (model, reason) in [
        ("gpt-terms", "agreement_required"),
        ("gpt-restricted", "not_entitled"),
    ] {
        assert_eq!(
            evidence(model).await,
            Some((
                "requires_action".into(),
                Some(reason.into()),
                "control_plane".into()
            ))
        );
    }
    assert_eq!(
        evidence("gpt").await,
        Some(("entitled".into(), None, "control_plane".into()))
    );
    assert_eq!(
        evidence("gpt-old").await,
        Some((
            "requires_action".into(),
            Some("permission_missing".into()),
            "control_plane".into()
        ))
    );

    // Traffic verifies `gpt` and refuses `gpt-new`.
    let store = ModelEvidenceStore::new(harness.pool.clone());
    let key = |model: &str| EvidenceKey {
        organization_id: org,
        adapter_instance_key: "openai-main".into(),
        family: "openai".into(),
        model: model.into(),
    };
    let now = Utc::now();
    for (model, verdict) in [
        ("gpt", Verdict::Verified),
        ("gpt-new", Verdict::Refused(EvidenceReason::AccessDenied)),
    ] {
        store
            .record(&Observation {
                key: key(model),
                verdict,
                source: gateway::domain::model_evidence::EvidenceSource::Traffic,
                deployment: Some(model.into()),
                provider_status: None,
                provider_code: None,
                observed_at: now,
            })
            .await
            .unwrap();
    }
    // A later check: the role is gone for every model.
    for model in ["gpt", "gpt-new", "gpt-old"] {
        store
            .record(&Observation::of_entitlement(
                key(model),
                Entitlement::PermissionMissing,
                now + Duration::seconds(5),
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        evidence("gpt").await,
        Some(("verified".into(), None, "traffic".into()))
    );
    assert_eq!(
        evidence("gpt-new").await,
        Some((
            "refused".into(),
            Some("access_denied".into()),
            "traffic".into()
        ))
    );
    // A check replaces a check.
    store
        .record(&Observation::of_entitlement(
            key("gpt-old"),
            Entitlement::Entitled,
            now + Duration::seconds(10),
        ))
        .await
        .unwrap();
    assert_eq!(
        evidence("gpt-old").await,
        Some(("entitled".into(), None, "control_plane".into()))
    );
    // An organization policy that forbids a model outranks what calls
    // showed: the provider enforces it on every call.
    for model in ["gpt", "gpt-new"] {
        store
            .record(&Observation::of_entitlement(
                key(model),
                Entitlement::PolicyDenied,
                now + Duration::seconds(15),
            ))
            .await
            .unwrap();
        assert_eq!(
            evidence(model).await,
            Some((
                "requires_action".into(),
                Some("policy_denied".into()),
                "control_plane".into()
            ))
        );
    }
    // Once the policy allows it again, the check replaces its own denial.
    store
        .record(&Observation::of_entitlement(
            key("gpt"),
            Entitlement::Entitled,
            now + Duration::seconds(20),
        ))
        .await
        .unwrap();
    assert_eq!(
        evidence("gpt").await,
        Some(("entitled".into(), None, "control_plane".into()))
    );
}

#[tokio::test]
async fn deployed_models_are_probed_until_a_call_settles_them() {
    let _guard = database_guard();
    let Some(database_url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-probes").await;
    let evidence = |model: &'static str| {
        let pool = harness.pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, String)>(
                "SELECT state, reason, source FROM gateway.catalog_model_evidence
                 WHERE organization_id = $1 AND adapter_instance_key = 'openai-main'
                   AND family = 'openai' AND model = $2",
            )
            .bind(org.0)
            .bind(model)
            .fetch_optional(&pool)
            .await
            .unwrap()
        }
    };
    // `gpt` and `gpt-old` are deployed; `gpt-new` is not, and the account
    // has no automatic offers, so it is never probed.
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Probed(
            org,
            vec![
                ("gpt", Entitlement::Entitled, Ok(())),
                (
                    "gpt-old",
                    Entitlement::Entitled,
                    Err(ProviderErrorCategory::Authorization),
                ),
                ("gpt-new", Entitlement::Entitled, Ok(())),
            ],
        ),
    );
    harness.drain(org).await;
    assert_eq!(probed(org), ["gpt", "gpt-old"]);
    assert_eq!(
        evidence("gpt").await,
        Some(("verified".into(), None, "probe".into()))
    );
    assert_eq!(
        evidence("gpt-old").await,
        Some((
            "refused".into(),
            Some("access_denied".into()),
            "probe".into()
        ))
    );
    assert_eq!(
        evidence("gpt-new").await,
        Some(("entitled".into(), None, "control_plane".into()))
    );
    // The collection filters by evidence.
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    for (filter, expected) in [
        ("verified", vec!["gpt"]),
        ("refused", vec!["gpt-old"]),
        ("entitled", vec!["gpt-new"]),
        ("requires_action", vec![]),
        ("none", vec![]),
    ] {
        let (status, page) = harness
            .get(
                &uri(
                    org,
                    &format!(
                        "discovered-models?adapter_instance_key=openai-main&evidence={filter}"
                    ),
                ),
                &owner,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{filter}: {page}");
        let models: Vec<&str> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["model"].as_str().unwrap())
            .collect();
        assert_eq!(models, expected, "{filter}");
    }
    let (status, _) = harness
        .get(&uri(org, "discovered-models?evidence=maybe"), &owner)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Settled models are not probed again, nor is a recent refusal.
    harness.queue(org, "openai-main").await;
    harness.drain(org).await;
    assert_eq!(probed(org), ["gpt", "gpt-old"]);

    // A day-old refusal is probed again.
    for statement in [
        "UPDATE gateway.catalog_model_evidence
         SET observed_at = observed_at - interval '2 days',
             refused_since = refused_since - interval '2 days'
         WHERE organization_id = $1 AND model = 'gpt-old'",
        "UPDATE gateway.catalog_model_probe
         SET attempted_at = attempted_at - interval '2 days'
         WHERE organization_id = $1 AND model = 'gpt-old'",
    ] {
        sqlx::query(statement)
            .bind(org.0)
            .execute(&harness.pool)
            .await
            .unwrap();
    }
    harness.queue(org, "openai-main").await;
    harness.drain(org).await;
    assert_eq!(probed(org), ["gpt", "gpt-old", "gpt-old"]);

    // Once a refusal is due again, the account's current check outranks it:
    // a model the listing finds blocked is not probed.
    for statement in [
        "UPDATE gateway.catalog_model_evidence
         SET observed_at = observed_at - interval '2 days',
             refused_since = refused_since - interval '2 days'
         WHERE organization_id = $1 AND model = 'gpt-old'",
        "UPDATE gateway.catalog_model_probe
         SET attempted_at = attempted_at - interval '2 days'
         WHERE organization_id = $1 AND model = 'gpt-old'",
    ] {
        sqlx::query(statement)
            .bind(org.0)
            .execute(&harness.pool)
            .await
            .unwrap();
    }
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Probed(
            org,
            vec![
                ("gpt", Entitlement::Entitled, Ok(())),
                ("gpt-old", Entitlement::AgreementRequired, Ok(())),
            ],
        ),
    );
    harness.queue(org, "openai-main").await;
    harness.drain(org).await;
    assert_eq!(probed(org), ["gpt", "gpt-old", "gpt-old"]);
}

#[tokio::test]
async fn a_model_is_claimed_for_one_probe_at_a_time() {
    let _guard = database_guard();
    let Some(database_url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-probe-claims").await;
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Checked(vec![
            ("gpt", Entitlement::Entitled),
            ("gpt-old", Entitlement::Entitled),
        ]),
    );
    harness.drain(org).await;
    let store = ModelEvidenceStore::new(harness.pool.clone());
    let now = Utc::now();
    let claim = |limit: i64, at: chrono::DateTime<Utc>| {
        let store = store.clone();
        async move {
            store
                .claim_probes(
                    org,
                    "openai-main",
                    "openai",
                    &[],
                    &[],
                    at,
                    at - Duration::hours(1),
                    at - Duration::days(1),
                    limit,
                )
                .await
                .unwrap()
        }
    };
    // The listing's family does not probe, so discovery claimed nothing.
    // Two workers split the models.
    let first = claim(1, now).await;
    let second = claim(5, now).await;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(first, second);
    assert!(claim(5, now).await.is_empty());
    // An hour later, neither settled: both are tried again, the one tried
    // longest ago first.
    assert_eq!(claim(1, now + Duration::minutes(61)).await, first);
}

#[tokio::test]
async fn probes_that_cost_the_tenant_wait_for_a_deployment() {
    let _guard = database_guard();
    let Some(database_url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness
        .org_with_release("discovery-probes-deployed-only")
        .await;
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Checked(vec![
            ("gpt", Entitlement::Entitled),
            ("gpt-new", Entitlement::Entitled),
        ]),
    );
    harness.drain(org).await;
    // Automatic offers make every listed model a candidate.
    sqlx::query(
        "INSERT INTO gateway.catalog_auto_offer (organization_id, adapter_instance_key, id)
         VALUES ($1, 'openai-main', $2)",
    )
    .bind(org.0)
    .bind(uuid::Uuid::new_v4())
    .execute(&harness.pool)
    .await
    .unwrap();
    let store = ModelEvidenceStore::new(harness.pool.clone());
    let now = Utc::now();
    let claim = |deployed_only: Vec<String>, at: chrono::DateTime<Utc>| {
        let store = store.clone();
        async move {
            let mut claimed = store
                .claim_probes(
                    org,
                    "openai-main",
                    "openai",
                    &[],
                    &deployed_only,
                    at,
                    at - Duration::hours(1),
                    at - Duration::days(1),
                    10,
                )
                .await
                .unwrap();
            claimed.sort();
            claimed
        }
    };
    // Billed probes: only the deployed model, not the one only offered.
    let both = vec!["gpt".to_owned(), "gpt-new".to_owned()];
    assert_eq!(claim(both, now).await, ["gpt"]);
    // A free probe of the offered model is claimed.
    assert_eq!(
        claim(vec![], now + Duration::minutes(61)).await,
        ["gpt", "gpt-new"]
    );
}

#[tokio::test]
async fn models_needing_action_are_not_probed() {
    let _guard = database_guard();
    let Some(database_url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-probes-action").await;
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Probed(
            org,
            vec![
                ("gpt", Entitlement::AgreementRequired, Ok(())),
                // A throttle ends the round without recording anything.
                (
                    "gpt-old",
                    Entitlement::Entitled,
                    Err(ProviderErrorCategory::RateLimited),
                ),
            ],
        ),
    );
    harness.drain(org).await;
    assert_eq!(probed(org), ["gpt-old"]);
    let sources = sqlx::query_scalar::<_, String>(
        "SELECT source FROM gateway.catalog_model_evidence
         WHERE organization_id = $1 ORDER BY model",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(sources, ["control_plane", "control_plane"]);
}

#[tokio::test]
async fn discovered_models_filter_by_evidence() {
    let _guard = database_guard();
    let Some(database_url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org_with_release("discovery-evidence-filter").await;
    let owner = harness.user(org, "organization_owner").await;
    let owner = harness.session(owner).await;
    harness.accounts.set(
        org,
        "openai-main",
        Listing::Models(vec!["gpt", "gpt-new", "gpt-old", "gpt-terms", "gpt-x"]),
    );
    harness.drain(org).await;
    let store = ModelEvidenceStore::new(harness.pool.clone());
    let key = |model: &str| EvidenceKey {
        organization_id: org,
        adapter_instance_key: "openai-main".into(),
        family: "openai".into(),
        model: model.into(),
    };
    let now = Utc::now();
    let call = |model: &str, verdict| Observation {
        key: key(model),
        verdict,
        source: gateway::domain::model_evidence::EvidenceSource::Traffic,
        deployment: Some(model.into()),
        provider_status: None,
        provider_code: None,
        observed_at: now,
    };
    for observation in [
        call("gpt", Verdict::Verified),
        call("gpt-old", Verdict::Refused(EvidenceReason::AccessDenied)),
        Observation::of_entitlement(key("gpt-new"), Entitlement::Entitled, now),
        Observation::of_entitlement(key("gpt-terms"), Entitlement::AgreementRequired, now),
    ] {
        store.record(&observation).await.unwrap();
    }
    // `gpt-x` has no evidence.
    for (filter, expected) in [
        ("verified", vec!["gpt"]),
        ("refused", vec!["gpt-old"]),
        ("entitled", vec!["gpt-new"]),
        ("requires_action", vec!["gpt-terms"]),
        ("none", vec!["gpt-x"]),
        ("verified,entitled,none", vec!["gpt", "gpt-new", "gpt-x"]),
        ("refused,requires_action", vec!["gpt-old", "gpt-terms"]),
    ] {
        let (status, page) = harness
            .get(
                &uri(
                    org,
                    &format!("discovered-models?state=listed&evidence={filter}"),
                ),
                &owner,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{filter}: {page}");
        let models: Vec<&str> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["model"].as_str().unwrap())
            .collect();
        assert_eq!(models, expected, "{filter}");
    }
    // Paging keeps the set.
    let (_, page) = harness
        .get(
            &uri(
                org,
                "discovered-models?evidence=none,verified,entitled&page_size=2",
            ),
            &owner,
        )
        .await;
    let token = page["next_page_token"].as_str().unwrap().to_owned();
    let (status, page) = harness
        .get(
            &uri(
                org,
                &format!("discovered-models?evidence=entitled,verified,none&page_size=2&page_token={token}"),
            ),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["model"], "gpt-x");
    for invalid in ["maybe", "verified,", ""] {
        let (status, _) = harness
            .get(
                &uri(org, &format!("discovered-models?evidence={invalid}")),
                &owner,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    }
}
