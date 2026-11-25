#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for model cards (Phase 8 milestone 1): the card built
//! from the active release, availability evidence, usage, request outcomes
//! and activation history, driven through the real router, RBAC and schema.
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
    http::{HeaderMap, StatusCode, header::COOKIE},
};
use chrono::{DateTime, Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, TeamId, UserAccountId,
    domain::{
        audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
        auth::AuthCrypto,
        identity::MembershipRole,
        keys::ApiKeyPepper,
        reporting::{UsageGroupBy, UsageQuery},
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore, model_cards::ModelCardStore,
        reporting::ReportingStore,
    },
    transport::model_cards::router,
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
const TEST_PEPPER: &[u8] = b"model-cards-integration-pepper";
const HMAC_KEY: &[u8] = b"model-cards-integration-hmac-key-at-least-32";
const MODEL: &str = "us.amazon.nova-2-lite-v1:0";
const MODEL_PATH: &str = "us.amazon.nova-2-lite-v1%3A0";

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

/// While set, the stand-in Hub answers 503 to everything.
static HUB_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A stand-in for the Hugging Face Hub: one public repository,
/// `openai/gpt-oss-x`; any other is not found.
async fn fake_hub() -> url::Url {
    use axum::{extract::Path, routing::get};
    let app = Router::new().route(
        "/api/models/{owner}/{name}",
        get(|Path((owner, name)): Path<(String, String)>| async move {
            if HUB_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({"error": "down"})),
                );
            }
            if (owner.as_str(), name.as_str()) != ("openai", "gpt-oss-x") {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"error": "not found"})),
                );
            }
            (
                StatusCode::OK,
                axum::Json(json!({
                    "id": "openai/gpt-oss-x", "author": "openai",
                    "safetensors": {"total": 20_000_000_000_u64},
                    "cardData": {"license": "apache-2.0", "model-index": [{"results": [
                        {"dataset": {"name": "MMLU"}, "metrics": [{"type": "acc", "value": 85.5}]}
                    ]}]}
                })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url::Url::parse(&format!("http://{address}/")).unwrap()
}

/// One recorded request and how it went.
struct Seed<'a> {
    team: Option<TeamId>,
    model: &'a str,
    amount: i64,
    recorded_at: DateTime<Utc>,
    error: Option<&'a str>,
    latency_ms: Option<i32>,
    first_event_ms: Option<i32>,
    finish_reason: Option<&'a str>,
}

impl Default for Seed<'_> {
    fn default() -> Self {
        Self {
            team: None,
            model: MODEL,
            amount: 10,
            recorded_at: Utc::now() - Duration::days(1),
            error: None,
            latency_ms: None,
            first_event_ms: None,
            finish_reason: None,
        }
    }
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
        let crypto = AuthCrypto::new(HMAC_KEY, &[17_u8; 32]).unwrap();
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            ModelCardStore::new(pool.clone()),
            ReportingStore::new(pool.clone()),
            gateway::persistence::trace_policy::TracePolicyStore::new(pool.clone()),
            std::sync::Arc::new(card_library()),
            Some(gateway::application::model_cards::HubSource::for_tests(
                fake_hub().await,
            )),
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

    async fn team(&self, org: OrganizationId, name: &str) -> TeamId {
        self.identity.create_team(org, name).await.unwrap().id
    }

    /// A user holding exactly one admin role binding: org-wide when `team` is
    /// `None`, otherwise of that team.
    async fn user(&self, org: OrganizationId, team: Option<TeamId>, role: &str) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some("Ada Admin"))
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

    /// Applies `manifest` and records its activation by `actor`, as the
    /// catalog admin API does.
    async fn activate(
        &self,
        org: OrganizationId,
        actor: UserAccountId,
        manifest: &CatalogManifest,
    ) -> Uuid {
        let release = CatalogStore::new(self.pool.clone())
            .apply(org, manifest, manifest.digest().unwrap())
            .await
            .unwrap();
        AdminStore::new(self.pool.clone())
            .record_audit(AdminAuditEvent::new(
                AuditActor::User(actor),
                org,
                None,
                AuditTargetResource::CatalogActivation,
                AuditAction::new("catalog_release.activated").unwrap(),
                release.id,
                AuditOutcome::Succeeded,
                Uuid::new_v4(),
                None,
                None,
            ))
            .await
            .unwrap();
        // Activations one after another keep their order.
        tokio::time::sleep(StdDuration::from_millis(5)).await;
        release.id
    }

    /// A settled reservation, its finalized trace with `seed`'s outcome and
    /// timings, and one exact usage row.
    async fn record(&self, org: OrganizationId, seed: Seed<'_>) {
        let reservation = Uuid::new_v4();
        let trace = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, team_id, idempotency_key, fingerprint, state,
                  reserved_minor, expires_at, output_token_cap, input_price_minor,
                  output_price_minor, cached_input_price_minor, reasoning_price_minor,
                  other_price_minor, price_version, pricing_confidence, currency, deployment,
                  model, trace_id)
             VALUES ($1, $2, $3, $4, $5, 'settled', 500, $6, 100, 1, 1, 1, 1, 1, 'v1',
                     'known', 'USD', 'bedrock-nova', $7, $8)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(seed.team.map(|team| team.0))
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(seed.recorded_at + Duration::minutes(5))
        .bind(seed.model)
        .bind(trace)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, team_id, reservation_id, correlation_id,
                  operation, status, outcome, provider_error_category, provider_request_id,
                  model, input_bound, output_cap, price_version, pricing_confidence, currency,
                  amount_minor, metadata, recorded_at, finalized_at)
             VALUES ($1, $2, $3, $4, $5, 'inference', 'finalized',
                     CASE WHEN $6::text IS NULL THEN 'succeeded' ELSE 'failed' END, $6,
                     'req_opaque', $7, 64, 100, 'v1', 'known', 'USD', $8,
                     jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency', 'USD'),
                     $9, $9)",
        )
        .bind(trace)
        .bind(org.0)
        .bind(seed.team.map(|team| team.0))
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(seed.error)
        .bind(seed.model)
        .bind(seed.amount)
        .bind(seed.recorded_at)
        .execute(&self.pool)
        .await
        .unwrap();
        // Timings arrive after the trace is written, as accounting records
        // them.
        sqlx::query(
            "UPDATE gateway.inference_trace
             SET provider_latency_ms = $2, first_event_ms = $3, finish_reason = $4
             WHERE id = $1",
        )
        .bind(trace)
        .bind(seed.latency_ms)
        .bind(seed.first_event_ms)
        .bind(seed.finish_reason)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.accounting_usage
                 (id, reservation_id, organization_id, currency, confidence, input_tokens,
                  output_tokens, amount_minor, completeness, finality, recorded_at)
             VALUES ($1, $2, $3, 'USD', 'exact', 10, 5, $4, 'complete', 'final', $5)",
        )
        .bind(Uuid::new_v4())
        .bind(reservation)
        .bind(org.0)
        .bind(seed.amount)
        .bind(seed.recorded_at)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn evidence(&self, org: OrganizationId, account: &str, family: &str, state: &str) {
        let refused = matches!(state, "refused" | "requires_action");
        sqlx::query(
            "INSERT INTO gateway.catalog_model_evidence
                 (organization_id, adapter_instance_key, family, model, state, reason, source,
                  deployment, observed_at, verified_at, refused_since)
             VALUES ($1, $2, $3, $4, $5, $6, 'traffic', NULL, now(),
                     CASE WHEN $5 = 'verified' THEN now() END,
                     CASE WHEN $7 THEN now() END)",
        )
        .bind(org.0)
        .bind(account)
        .bind(family)
        .bind(MODEL)
        .bind(state)
        .bind(refused.then_some("access_denied"))
        .bind(refused)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn card(
        &self,
        org: OrganizationId,
        model: &str,
        session: Option<&(String, String)>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder().method("GET").uri(format!(
            "/api/admin/v1/organizations/{}/catalog/models/{model}/card",
            org.0
        ));
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
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body, headers)
    }

    async fn usage_reads(&self, org: OrganizationId) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM gateway.audit_event
             WHERE organization_id = $1 AND action = 'usage.read'",
        )
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

/// A release serving [`MODEL`] from Bedrock at `price` per input token and,
/// with `azure`, from an Azure AI Foundry deployment too; another model rides
/// along to show the card ignores it.
/// The shipped card files plus an operator card for [`MODEL`] on Bedrock
/// (not on Azure), as `GATEWAY_MODEL_CARDS_DIR` would add it.
fn card_library() -> gateway::domain::model_card_files::CardLibrary {
    let dir = std::env::temp_dir().join(format!("model-cards-{}", Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("amazon")).unwrap();
    let fact = |value: Value| {
        json!({"value": value, "source_url": "https://example.com/nova-2-lite",
               "retrieved_at": "2026-11-14", "reported_by": "provider"})
    };
    std::fs::write(
        dir.join("amazon/nova-2-lite.json"),
        json!({
            "schema": "qucelerate.model-card.v1",
            "id": "amazon/nova-2-lite",
            "name": "Nova 2 Lite",
            "vendor": "Amazon",
            "matches": {"bedrock": [MODEL]},
            "overview": {"context_window_tokens": fact(json!(1000)), "open_weight": fact(json!(false))},
            "evaluations": [{"name": "Bench", "score": 50, "evaluator": "Amazon",
                             "source_url": "https://example.com/bench",
                             "retrieved_at": "2026-11-14", "reported_by": "provider"}],
            "safety": {"model_card_url": fact(json!("https://example.com/card"))},
        })
        .to_string(),
    )
    .unwrap();
    let library = gateway::domain::model_card_files::CardLibrary::load(Some(&dir)).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    library
}

fn manifest(version: &str, price: i64, azure: bool) -> CatalogManifest {
    let capability = |operation: &str| {
        json!({"operation": operation, "max_input_bytes": 4096, "max_output_tokens": 32,
               "max_tokens_per_byte": 1, "tokenizer": "configured",
               "reservation_ttl_seconds": 30, "chat_input_count_mode": "unsupported"})
    };
    let price_of = |deployment: &str, input: i64| {
        json!({"deployment": deployment, "operation": "chat",
               "input_minor_per_token": input, "output_minor_per_token": 4,
               "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
               "other_minor_per_token": 0, "currency": "USD", "source": "test",
               "source_version": "v1", "confidence": "known"})
    };
    let mut providers = vec![json!({"adapter_instance_key": "bedrock-main", "family": "bedrock"})];
    let mut deployments = vec![
        json!({"name": "bedrock-nova", "model": MODEL, "location": "us-east-1",
               "adapter_instance_key": "bedrock-main", "capabilities": [capability("chat")]}),
        json!({"name": "bedrock-other", "model": "other-model", "location": "us-east-1",
               "adapter_instance_key": "bedrock-main", "capabilities": [capability("chat")]}),
    ];
    let mut prices = vec![
        price_of("bedrock-nova", price),
        price_of("bedrock-other", 9),
    ];
    let routes = vec![
        json!({"alias": "nova", "operation": "chat", "deployment": "bedrock-nova"}),
        json!({"alias": "other", "operation": "chat", "deployment": "bedrock-other"}),
    ];
    if azure {
        providers
            .push(json!({"adapter_instance_key": "foundry-main", "family": "azure-ai-foundry"}));
        deployments.push(
            json!({"name": "foundry-nova", "model": MODEL, "location": "eastus2",
                                "adapter_instance_key": "foundry-main",
                                // Embeddings are unpriced and unrouted: shown without a price.
                                "capabilities": [capability("chat"), capability("embeddings")]}),
        );
        prices.push(price_of("foundry-nova", 3));
    }
    serde_json::from_value(json!({
        "release": {"version": version},
        "providers": providers,
        "deployments": deployments,
        "prices": prices,
        "routes": routes,
    }))
    .unwrap()
}

#[tokio::test]
async fn a_card_gathers_deployments_usage_outcomes_and_history() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;

    let first = harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    // Unchanged for the model: no history entry.
    harness
        .activate(org, owner, &manifest("v1b", 2, false))
        .await;
    let current = harness.activate(org, owner, &manifest("v2", 5, true)).await;
    harness
        .evidence(org, "bedrock-main", "bedrock", "verified")
        .await;

    let now = Utc::now();
    for (latency, finish) in [(100, "stop"), (200, "stop"), (300, "content_filter")] {
        harness
            .record(
                org,
                Seed {
                    latency_ms: Some(latency),
                    first_event_ms: Some(latency / 4),
                    finish_reason: Some(finish),
                    ..Seed::default()
                },
            )
            .await;
    }
    // Before timings were recorded; a failure; one 60 days ago; another model.
    harness.record(org, Seed::default()).await;
    harness
        .record(
            org,
            Seed {
                error: Some("rate_limited"),
                amount: 0,
                ..Seed::default()
            },
        )
        .await;
    harness
        .record(
            org,
            Seed {
                recorded_at: now - Duration::days(60),
                amount: 1000,
                ..Seed::default()
            },
        )
        .await;
    harness
        .record(
            org,
            Seed {
                model: "other-model",
                ..Seed::default()
            },
        )
        .await;

    let (status, card, headers) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(card["schema"], "qucelerate.model-card.v1");
    assert_eq!(card["model"], MODEL);
    assert_eq!(card["release_id"], current.to_string());
    // The operator's card matches the Bedrock deployment exactly; the Azure
    // one serves the same identifier but the card lists it only for Bedrock.
    let public = &card["public_card"];
    assert_eq!(public["status"], "on_file", "{public}");
    assert_eq!(public["card"]["id"], "amazon/nova-2-lite");
    assert_eq!(public["card"]["origin"], "operator");
    assert_eq!(
        public["card"]["matched_deployments"],
        json!(["bedrock-nova"])
    );
    assert_eq!(
        public["card"]["overview"]["context_window_tokens"],
        json!({"value": 1000, "source_url": "https://example.com/nova-2-lite",
               "retrieved_at": "2026-11-14", "reported_by": "provider"})
    );
    assert_eq!(public["card"]["evaluations"][0]["name"], "Bench");
    // Both families' shipped data-handling terms, each with its sources.
    // Nothing lists the Foundry deployment's model format, so both Foundry
    // terms (Models sold by Azure, and Claude's) may apply.
    let families: Vec<(&str, Option<&str>)> = public["data_handling"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["provider_family"].as_str().unwrap(),
                d["service"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        families,
        [
            ("azure-ai-foundry", None),
            ("azure-ai-foundry", Some("anthropic")),
            ("bedrock", None)
        ]
    );
    assert!(public["data_handling"].as_array().unwrap().iter().all(|d| {
        d["origin"] == "builtin"
            && d["facts"].as_object().unwrap().values().all(|f| {
                f["source_url"].as_str().unwrap().starts_with("https://")
                    && f["retrieved_at"].is_string()
            })
    }));
    // No trace policy set: the gateway traces nothing.
    assert_eq!(
        card["gateway_data_handling"],
        json!({"trace_mode": "off", "trace_content": true,
               "trace_content_retention_days": 30})
    );
    assert_eq!(card["omitted"], json!([]));
    assert_eq!(
        card["overview"]["provider_families"],
        json!(["azure-ai-foundry", "bedrock"])
    );
    assert_eq!(
        card["overview"]["locations"],
        json!(["eastus2", "us-east-1"])
    );
    assert_eq!(
        card["overview"]["operations"],
        json!(["chat", "embeddings"])
    );

    let deployments = card["deployments"].as_array().unwrap();
    let names: Vec<_> = deployments
        .iter()
        .map(|d| d["deployment"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bedrock-nova", "foundry-nova"]);
    let bedrock = &deployments[0];
    assert_eq!(
        bedrock["routes"],
        json!([{"alias": "nova", "operation": "chat"}])
    );
    let price = &bedrock["operations"][0]["price"];
    assert_eq!(price["input_minor_per_token"], 5);
    assert_eq!(price["billed_by"], "release");
    assert_eq!(price["source"], "test");
    assert_eq!(bedrock["availability"]["state"], "verified");
    assert_eq!(bedrock["availability"]["source"], "traffic");
    assert_eq!(deployments[1]["availability"], Value::Null);
    assert_eq!(deployments[1]["routes"], json!([]));
    let foundry = deployments[1]["operations"].as_array().unwrap();
    assert_eq!(foundry[1]["operation"], "embeddings");
    assert_eq!(foundry[1]["price"], Value::Null);

    // Each window's totals are the usage report's for the model.
    let reporting = ReportingStore::new(harness.pool.clone());
    let windows = card["usage"]["windows"].as_array().unwrap();
    assert_eq!(
        windows
            .iter()
            .map(|w| w["days"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [7, 30, 90]
    );
    for window in windows {
        let parse = |field: &str| {
            DateTime::parse_from_rfc3339(window[field].as_str().unwrap())
                .unwrap()
                .with_timezone(&Utc)
        };
        let report = reporting
            .usage_totals(
                org,
                None,
                &UsageQuery {
                    from: parse("from"),
                    to: parse("to"),
                    group_by: vec![UsageGroupBy::Model],
                    team_ids: Vec::new(),
                    service_account_ids: Vec::new(),
                    models: vec![MODEL.to_owned()],
                    deployments: Vec::new(),
                    provider_families: Vec::new(),
                    locations: Vec::new(),
                    batch_ids: Vec::new(),
                    model_masters: Vec::new(),
                    currency: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(window["totals"], serde_json::to_value(&report).unwrap());
    }
    assert_eq!(windows[1]["totals"][0]["request_count"], 5);
    assert_eq!(windows[1]["totals"][0]["amount_minor"], 40);
    assert_eq!(windows[2]["totals"][0]["request_count"], 6);

    let requests = &card["usage"]["requests"];
    assert_eq!(requests["days"], 30);
    assert_eq!(requests["request_count"], 5);
    assert_eq!(requests["failed_count"], 1);
    assert_eq!(
        requests["errors"],
        json!([{"key": "rate_limited", "count": 1}])
    );
    assert_eq!(requests["timed_request_count"], 3);
    assert!(requests["timed_since"].is_string());
    assert_eq!(requests["latency_p50_ms"], 200);
    assert_eq!(requests["latency_p95_ms"], 300);
    assert_eq!(requests["first_event_p50_ms"], 50);
    assert_eq!(
        requests["finish_reasons"],
        json!([
            {"reason": "stop", "count": 2, "safety": false},
            {"reason": "content_filter", "count": 1, "safety": true},
        ])
    );
    assert_eq!(harness.usage_reads(org).await, 1);

    let governance = &card["governance"];
    assert_eq!(governance["truncated"], false);
    assert_eq!(governance["first_release"]["release_id"], first.to_string());
    assert_eq!(
        governance["first_release"]["added"],
        json!(["bedrock-nova"])
    );
    let history = governance["history"].as_array().unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(history[0]["release_version"], "v2");
    assert_eq!(history[0]["added"], json!(["foundry-nova"]));
    assert_eq!(history[0]["changed"], json!(["bedrock-nova"]));
    assert_eq!(history[0]["actor"]["actor_type"], "user");
    assert_eq!(history[0]["actor"]["id"], owner.0.to_string());
    assert_eq!(history[0]["actor"]["name"], "Ada Admin");
    assert_eq!(history[1]["release_version"], "v1");

    // An effective price override is what bills now.
    let override_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.catalog_price_override
             (id, organization_id, deployment, operation, currency, input_minor_per_token,
              output_minor_per_token, cached_input_minor_per_token, reasoning_minor_per_token,
              other_minor_per_token, version, confidence, reason, effective_from, created_by)
         VALUES ($1, $2, 'bedrock-nova', 'chat', 'USD', 7, 8, 0, 0, 0, 'override:' || $1::text,
                 'known', 'contract', now(), $3)",
    )
    .bind(override_id)
    .bind(org.0)
    .bind(owner.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    let price = &card["deployments"][0]["operations"][0]["price"];
    assert_eq!(price["input_minor_per_token"], 7);
    assert_eq!(price["billed_by"], "override");
    assert_eq!(price["source_version"], format!("override:{override_id}"));
    harness.pool.close().await;
}

#[tokio::test]
async fn sections_follow_the_callers_permissions() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-rbac").await;
    let owner = harness.user(org, None, "organization_owner").await;
    harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    harness.record(org, Seed::default()).await;

    // Operators read usage but not the audit log.
    let operator = harness.user(org, None, "operator").await;
    let (status, card, _) = harness
        .card(org, MODEL_PATH, Some(&harness.session(operator).await))
        .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert!(card["usage"].is_object());
    assert!(card.get("governance").is_none());
    assert_eq!(
        card["omitted"],
        json!([{"section": "governance", "requires": "audit_read"}])
    );
    assert_eq!(harness.usage_reads(org).await, 1);

    // Auditors read both.
    let auditor = harness.user(org, None, "auditor").await;
    let (status, card, _) = harness
        .card(org, MODEL_PATH, Some(&harness.session(auditor).await))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(card["usage"].is_object() && card["governance"].is_object());

    // Team admins have no catalog read.
    let team = harness.team(org, "a").await;
    let team_admin = harness.user(org, Some(team), "team_admin").await;
    let (status, body, _) = harness
        .card(org, MODEL_PATH, Some(&harness.session(team_admin).await))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");

    // Another organization's owner cannot read this one's card.
    let other = harness.org("model-cards-other").await;
    let other_owner = harness.user(other, None, "organization_owner").await;
    let (status, _, _) = harness
        .card(org, MODEL_PATH, Some(&harness.session(other_owner).await))
        .await;
    assert!(matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));

    let (status, body, _) = harness.card(org, MODEL_PATH, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "authentication_required");
    assert_eq!(harness.usage_reads(org).await, 2);
    harness.pool.close().await;
}

#[tokio::test]
async fn unknown_and_invalid_models_have_no_card() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-missing").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;

    // No active release yet.
    let (status, body, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");

    harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    let (status, _, _) = harness.card(org, "not-served", Some(&session)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The unencoded form of an identifier with `:` is the same identifier.
    let (status, _, _) = harness
        .card(org, "us.amazon.nova-2-lite-v1:0", Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body, _) = harness.card(org, "bad%0Amodel", Some(&session)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_model");
    // A card that is not returned records no usage read.
    assert_eq!(harness.usage_reads(org).await, 1);
    harness.pool.close().await;
}

#[tokio::test]
async fn history_is_complete_unless_an_unread_release_served_the_model() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let catalog = CatalogStore::new(harness.pool.clone());
    let unrelated: CatalogManifest = serde_json::from_value(json!({
        "release": {"version": "other-only"},
        "providers": [{"adapter_instance_key": "bedrock-main", "family": "bedrock"}],
        "deployments": [{"name": "bedrock-other", "model": "other-model", "location": "us-east-1",
                         "adapter_instance_key": "bedrock-main",
                         "capabilities": [{"operation": "chat", "max_input_bytes": 4096,
                                           "max_output_tokens": 32, "max_tokens_per_byte": 1,
                                           "tokenizer": "configured", "reservation_ttl_seconds": 30,
                                           "chat_input_count_mode": "unsupported"}]}],
        "prices": [{"deployment": "bedrock-other", "operation": "chat",
                    "input_minor_per_token": 1, "output_minor_per_token": 1,
                    "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
                    "other_minor_per_token": 0, "currency": "USD", "source": "test",
                    "source_version": "v1", "confidence": "known"}],
        "routes": [{"alias": "other", "operation": "chat", "deployment": "bedrock-other"}],
    }))
    .unwrap();

    // An earlier release without an audited activation that never served
    // the model leaves its history complete.
    let org = harness.org("model-cards-history").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    catalog
        .apply(org, &unrelated, unrelated.digest().unwrap())
        .await
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(5)).await;
    let first = harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(
        card["governance"]["first_release"]["release_id"],
        first.to_string()
    );
    assert_eq!(
        card["governance"]["history"][0]["added"],
        json!(["bedrock-nova"])
    );
    assert_eq!(card["governance"]["truncated"], false);

    // One that served it, applied without an audit event, leaves the start
    // unknown: the oldest activation read is only a baseline.
    let org = harness.org("model-cards-history-cli").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    let earlier = manifest("cli", 2, false);
    catalog
        .apply(org, &earlier, earlier.digest().unwrap())
        .await
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(5)).await;
    harness
        .activate(org, owner, &manifest("v1", 3, false))
        .await;
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(card["governance"]["first_release"], Value::Null);
    assert_eq!(card["governance"]["history"], json!([]));
    assert_eq!(card["governance"]["truncated"], true);
    harness.pool.close().await;
}

impl Harness {
    async fn send(
        &self,
        method: &str,
        uri: String,
        session: &(String, String),
        if_match: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        self.send_keyed(
            method,
            uri,
            session,
            if_match,
            body,
            &Uuid::new_v4().to_string(),
        )
        .await
    }

    /// As `send`, with this idempotency key on a write.
    async fn send_keyed(
        &self,
        method: &str,
        uri: String,
        session: &(String, String),
        if_match: Option<&str>,
        body: Option<Value>,
        key: &str,
    ) -> (StatusCode, Value, HeaderMap) {
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
        if method != "GET" {
            builder = builder
                .header("idempotency-key", key)
                .header("content-type", "application/json");
        }
        if let Some(if_match) = if_match {
            builder = builder.header("if-match", if_match);
        }
        let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    /// What Azure's listing of `foundry-main` says of the deployment serving
    /// [`MODEL`], and the account model catalog's terms for that model.
    async fn azure_listing(&self, org: OrganizationId, format: &str, retires_at: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO gateway.catalog_discovered_model
                 (organization_id, adapter_instance_key, family, model, first_seen_at,
                  last_seen_at, deployment_format, deployment_model, deployment_version,
                  deployment_sku, deployment_content_filter, deployment_content_filter_policy)
             VALUES ($1, 'foundry-main', 'azure-ai-foundry', $2, now(), now(), $3,
                     'claude-opus-5-5', '1', 'GlobalStandard', 'strict',
                     $4)",
        )
        .bind(org.0)
        .bind(MODEL)
        .bind(format)
        .bind(json!({"name": "strict", "mode": "Default", "filters": [
            {"name": "Hate", "source": "Prompt", "blocking": true, "severity_threshold": "Low"}
        ]}))
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.catalog_deployable_model
                 (organization_id, adapter_instance_key, family, format, model, version,
                  lifecycle_status, default_version, skus, retires_at, listed_at)
             VALUES ($1, 'foundry-main', 'azure-ai-foundry', $2, 'claude-opus-5-5', '1',
                     'Deprecating', true, ARRAY['GlobalStandard'], $3, now())",
        )
        .bind(org.0)
        .bind(format.to_ascii_lowercase())
        .bind(retires_at)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn listings_give_retirement_dates_terms_and_the_models_table_facts() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-listings").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    harness.activate(org, owner, &manifest("v1", 2, true)).await;
    let retires_at = DateTime::parse_from_rfc3339("2027-03-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    harness.azure_listing(org, "Anthropic", retires_at).await;

    let (status, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{card}");
    let deployments = card["deployments"].as_array().unwrap();
    let foundry = deployments
        .iter()
        .find(|d| d["deployment"] == "foundry-nova")
        .unwrap();
    let listing = &foundry["provider_listing"];
    assert_eq!(listing["format"], "Anthropic");
    assert_eq!(listing["model"], "claude-opus-5-5");
    assert_eq!(listing["lifecycle_status"], "Deprecating");
    assert_eq!(listing["retires_at"], "2027-03-01T00:00:00Z");
    assert_eq!(listing["content_filter"], "strict");
    assert_eq!(
        listing["content_filter_policy"]["filters"][0],
        json!({"name": "Hate", "source": "Prompt", "blocking": true, "severity_threshold": "Low"})
    );
    let bedrock = deployments
        .iter()
        .find(|d| d["deployment"] == "bedrock-nova")
        .unwrap();
    assert!(bedrock["provider_listing"].is_null());
    // The Foundry deployment sells Claude, so Anthropic's terms on Foundry
    // cover it, and Bedrock's the Bedrock one.
    let terms: Vec<(String, Value, Value)> = card["public_card"]["data_handling"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["provider_family"].as_str().unwrap().to_owned(),
                t["service"].clone(),
                t["deployments"].clone(),
            )
        })
        .collect();
    assert_eq!(
        terms,
        [
            (
                "azure-ai-foundry".to_owned(),
                json!("anthropic"),
                json!(["foundry-nova"])
            ),
            ("bedrock".to_owned(), Value::Null, json!(["bedrock-nova"])),
        ]
    );

    // The Models table's facts, per model of the release.
    let (status, list, _) = harness
        .send(
            "GET",
            format!("/api/admin/v1/organizations/{}/catalog/model-cards", org.0),
            &session,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let nova = items.iter().find(|i| i["model"] == MODEL).unwrap();
    assert_eq!(nova["card"]["id"], "amazon/nova-2-lite");
    assert_eq!(nova["context_window_tokens"]["value"], 1000);
    assert_eq!(nova["retires_at"], "2027-03-01T00:00:00Z");
    assert_eq!(
        nova["provider_families"],
        json!(["azure-ai-foundry", "bedrock"])
    );
    let other = items.iter().find(|i| i["model"] == "other-model").unwrap();
    assert!(other["card"].is_null() && other["retires_at"].is_null());

    // An organization without a release lists nothing.
    let empty = harness.org("model-cards-empty").await;
    let empty_owner = harness.user(empty, None, "organization_owner").await;
    let (status, list, _) = harness
        .send(
            "GET",
            format!(
                "/api/admin/v1/organizations/{}/catalog/model-cards",
                empty.0
            ),
            &harness.session(empty_owner).await,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list, json!({"release_id": null, "items": []}));
    harness.pool.close().await;
}

#[tokio::test]
async fn the_organization_note_is_versioned_audited_and_shown_on_the_card() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-note").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    let path = format!(
        "/api/admin/v1/organizations/{}/catalog/models/{MODEL_PATH}/card/note",
        org.0
    );
    let (status, body, _) = harness
        .send("GET", path.clone(), &session, None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, note, headers) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"note": "  Approved for internal tools.\r\nNot for PII.  "})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(note["note"], "Approved for internal tools.\nNot for PII.");
    assert_eq!(note["version"], 1);
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(note["updated_by"]["id"], owner.0.to_string());

    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(card["organization_note"]["note"], note["note"]);

    // A stale version is refused; the current one replaces the text.
    let (status, body, _) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("\"v7\""),
            Some(json!({"note": "x"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    let (status, cleared, _) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("\"v1\""),
            Some(json!({"note": ""})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["version"], 2);
    // A cleared note keeps its version on the card, the If-Match for
    // setting it again.
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(card["organization_note"]["note"], "");
    assert_eq!(card["organization_note"]["version"], 2);

    // Every text is kept; the audit log records the change, not the text.
    let revisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.model_card_note_revision WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(revisions, 2);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'model_card_note.set'
           AND outcome = 'succeeded' AND entity_type = 'model_card_note'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, 2);

    // Invalid notes and roles without catalog_write are refused.
    let (status, body, _) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"note": "a\u{7}b"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_note");
    let auditor = harness.user(org, None, "auditor").await;
    let auditor_session = harness.session(auditor).await;
    let (status, _, _) = harness
        .send(
            "PUT",
            path.clone(),
            &auditor_session,
            Some("*"),
            Some(json!({"note": "mine"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Auditors still read it.
    let (status, _, _) = harness
        .send("GET", path, &auditor_session, None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.pool.close().await;
}

#[tokio::test]
async fn a_hub_import_stands_in_where_no_card_file_matches() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-import").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    let path = format!(
        "/api/admin/v1/organizations/{}/catalog/models/other-model/card/import",
        org.0
    );
    let (status, body, _) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"repository": "openai/missing", "provider_repository": true})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "repository_not_found");
    let (status, body, _) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"repository": "../etc", "provider_repository": true})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_repository");

    let key = Uuid::new_v4().to_string();
    let (status, import, headers) = harness
        .send_keyed(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"repository": "openai/gpt-oss-x", "provider_repository": true})),
            &key,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    // A retry of the recorded import replays even while the Hub is down;
    // a new import then fails on the Hub.
    HUB_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, replayed, _) = harness
        .send_keyed(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"repository": "openai/gpt-oss-x", "provider_repository": true})),
            &key,
        )
        .await;
    let (fresh, _, _) = harness
        .send(
            "PUT",
            path.clone(),
            &session,
            Some("*"),
            Some(json!({"repository": "openai/gpt-oss-x", "provider_repository": true})),
        )
        .await;
    HUB_DOWN.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(status.is_success(), "{status} {replayed}");
    assert_eq!(replayed["card"]["id"], import["card"]["id"]);
    assert!(
        fresh.is_server_error() || fresh == StatusCode::BAD_GATEWAY,
        "{fresh}"
    );
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(import["card"]["id"], "huggingface/openai--gpt-oss-x");
    assert_eq!(import["imported_by"]["id"], owner.0.to_string());

    let (_, card, _) = harness.card(org, "other-model", Some(&session)).await;
    let public = &card["public_card"]["card"];
    assert_eq!(card["public_card"]["status"], "on_file");
    assert_eq!(public["origin"], "imported");
    assert_eq!(public["matched_deployments"], json!(["bedrock-other"]));
    assert_eq!(public["overview"]["license"]["value"], "apache-2.0");
    assert_eq!(public["overview"]["license"]["reported_by"], "provider");
    assert_eq!(
        public["overview"]["parameters"]["reported_by"],
        "independent"
    );
    assert_eq!(public["evaluations"][0]["score"], 85.5);
    let (_, list, _) = harness
        .send(
            "GET",
            format!("/api/admin/v1/organizations/{}/catalog/model-cards", org.0),
            &session,
            None,
            None,
        )
        .await;
    let other = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["model"] == "other-model")
        .unwrap();
    assert_eq!(other["card"]["origin"], "imported");
    // A card file still wins: the operator's Nova card is not replaced.
    let nova_path = format!(
        "/api/admin/v1/organizations/{}/catalog/models/{MODEL_PATH}/card/import",
        org.0
    );
    let (status, _, _) = harness
        .send(
            "PUT",
            nova_path,
            &session,
            Some("*"),
            Some(json!({"repository": "openai/gpt-oss-x", "provider_repository": false})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(card["public_card"]["card"]["id"], "amazon/nova-2-lite");

    // Only catalog writers import; readers read.
    let auditor = harness.user(org, None, "auditor").await;
    let auditor_session = harness.session(auditor).await;
    let (status, _, _) = harness
        .send(
            "PUT",
            path.clone(),
            &auditor_session,
            Some("*"),
            Some(json!({"repository": "openai/gpt-oss-x", "provider_repository": true})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = harness
        .send("GET", path.clone(), &auditor_session, None, None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // Removing it is soft and versioned.
    let (status, body, _) = harness
        .send("DELETE", path.clone(), &session, Some("\"v1\""), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, _) = harness
        .send("GET", path.clone(), &session, None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, card, _) = harness.card(org, "other-model", Some(&session)).await;
    assert_eq!(card["public_card"]["status"], "not_on_file");
    let (status, _, _) = harness
        .send("DELETE", path, &session, Some("*"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (kept, revisions): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM gateway.model_card_import
                 WHERE organization_id = $1 AND deleted_at IS NOT NULL),
                (SELECT count(*) FROM gateway.model_card_import_revision
                 WHERE organization_id = $1)",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!((kept, revisions), (1, 3));
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'model_card_import.set'
           AND outcome = 'succeeded'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, 3);
    harness.pool.close().await;
}

/// A completed chat batch of `org` whose lines ran on `model-a` (q1:a, q2:a)
/// and `model-b` (q1:b), with its output in `objects`.
async fn completed_batch(
    harness: &Harness,
    org: OrganizationId,
    owner: UserAccountId,
    objects: &gateway::application::trace_objects::MemoryTraceObjects,
) -> Uuid {
    use gateway::application::trace_objects::TraceObjects;
    let account = harness
        .identity
        .create_service_account(org, owner, None, "evaluator")
        .await
        .unwrap()
        .id;
    let (batch, input, output) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let key = format!("batch-files/{}/{output}", org.0);
    for (file, purpose) in [(input, "batch"), (output, "batch_output")] {
        sqlx::query(
            "INSERT INTO gateway.batch_file
                 (id, organization_id, service_account_id, purpose, filename, bytes, sha256,
                  object_key)
             VALUES ($1, $2, $3, $4, 'f.jsonl', 10, decode(repeat('00', 32), 'hex'), $5)",
        )
        .bind(file)
        .bind(org.0)
        .bind(account.0)
        .bind(purpose)
        .bind(format!("batch-files/{}/{file}", org.0))
        .execute(&harness.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO gateway.batch
             (id, organization_id, service_account_id, idempotency_key, fingerprint, endpoint,
              operation, input_file_id, output_file_id, completion_window, status,
              total_lines, completed_lines, expires_at, completed_at)
         VALUES ($1, $2, $3, 'eval', decode(repeat('00', 32), 'hex'), '/v1/chat/completions',
                 'chat_batch', $4, $5, '24h', 'completed', 3, 3, now() + interval '1 day', now())",
    )
    .bind(batch)
    .bind(org.0)
    .bind(account.0)
    .bind(input)
    .bind(output)
    .execute(&harness.pool)
    .await
    .unwrap();
    let mut jobs = std::collections::HashMap::new();
    for (ordinal, model) in ["model-a", "model-b"].into_iter().enumerate() {
        let job = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.batch_job
                 (id, organization_id, batch_id, ordinal, deployment, model,
                  adapter_instance_key, family, mode, state, next_run_at)
             VALUES ($1, $2, $3, $4, $5, $5, 'bedrock-main', 'bedrock', 'native', 'finished',
                     now())",
        )
        .bind(job)
        .bind(org.0)
        .bind(batch)
        .bind(ordinal as i32)
        .bind(model)
        .execute(&harness.pool)
        .await
        .unwrap();
        jobs.insert(model, job);
    }
    let answer = |custom_id: &str, text: &str| {
        json!({"id": "x", "custom_id": custom_id, "error": null, "response": {"status_code": 200,
               "body": {"choices": [{"message": {"role": "assistant", "content": text}}]}}})
        .to_string()
    };
    for (line, (custom_id, model)) in [
        ("q1:a", "model-a"),
        ("q2:a", "model-a"),
        ("q1:b", "model-b"),
    ]
    .into_iter()
    .enumerate()
    {
        let reservation = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
               (id, organization_id, service_account_id, idempotency_key, fingerprint, state,
                reserved_minor, expires_at, output_token_cap, input_price_minor,
                output_price_minor, cached_input_price_minor, reasoning_price_minor,
                other_price_minor, price_version, pricing_confidence, currency, deployment,
                model, trace_id)
             VALUES ($1, $2, $3, $4, decode(repeat('00', 32), 'hex'), 'settled', 0,
                     CURRENT_TIMESTAMP, 16, 0, 0, 0, 0, 0, 'v1', 'known', 'USD', $5, $5, $6)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(account.0)
        .bind(format!("eval-{custom_id}"))
        .bind(model)
        .bind(Uuid::new_v4())
        .execute(&harness.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.batch_line
                 (organization_id, batch_id, line_number, custom_id, job_id, reservation_id, state)
             VALUES ($1, $2, $3, $4, $5, $6, 'succeeded')",
        )
        .bind(org.0)
        .bind(batch)
        .bind(line as i32 + 1)
        .bind(custom_id)
        .bind(jobs[model])
        .bind(reservation)
        .execute(&harness.pool)
        .await
        .unwrap();
    }
    let content = [
        answer("q1:a", "Paris"),
        answer("q2:a", "It is 4."),
        answer("q1:b", "Lyon"),
    ]
    .join("\n");
    objects.put(&key, content.into_bytes()).await.unwrap();
    batch
}

#[tokio::test]
async fn organization_evaluations_score_models_from_a_batch() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-evaluations").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    let objects =
        std::sync::Arc::new(gateway::application::trace_objects::MemoryTraceObjects::default());
    let evaluations = gateway::transport::model_evaluations::router(
        AuthStore::new(harness.pool.clone()),
        AdminStore::new(harness.pool.clone()),
        Some(harness.crypto.clone()),
        gateway::persistence::model_evaluations::ModelEvaluationStore::new(harness.pool.clone()),
        Some(objects.clone()),
    );
    let call = |method: &'static str, uri: String, body: Option<Value>| {
        let builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            )
            .header("idempotency-key", Uuid::new_v4().to_string())
            .header("content-type", "application/json");
        let router = evaluations.clone();
        async move {
            let response = router
                .oneshot(
                    builder
                        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let sets = format!(
        "/api/admin/v1/organizations/{}/catalog/evaluation-sets",
        org.0
    );
    let (status, body) = call(
        "POST",
        sets.clone(),
        Some(json!({"name": "Bad", "items": [{"id": "q 1", "expected": "x", "match": "exact"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, set) = call(
        "POST",
        sets.clone(),
        Some(json!({"name": "Basics", "items": [
            {"id": "q1", "expected": "Paris", "match": "exact"},
            {"id": "q2", "expected": "4", "match": "contains"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{set}");
    let set_id = set["id"].as_str().unwrap().to_owned();
    let runs = format!("{sets}/{set_id}/runs");

    // Only a completed batch of this organization can be scored.
    let (status, body) = call(
        "POST",
        runs.clone(),
        Some(json!({"batch_id": Uuid::new_v4()})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "batch_not_found");
    let batch = completed_batch(&harness, org, owner, &objects).await;
    let (status, run) = call("POST", runs.clone(), Some(json!({"batch_id": batch}))).await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    assert_eq!(
        run["results"],
        json!([
            {"model": "model-a", "items": 2, "answered": 2, "correct": 2},
            {"model": "model-b", "items": 1, "answered": 1, "correct": 0}
        ])
    );
    let (status, listed) = call("GET", runs.clone(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    // Only counts are stored; the card shows the model's latest score.
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.model_evaluation_result WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(stored, 2);
    harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    sqlx::query(
        "UPDATE gateway.model_evaluation_result SET model = $2
         WHERE organization_id = $1 AND model = 'model-a'",
    )
    .bind(org.0)
    .bind(MODEL)
    .execute(&harness.pool)
    .await
    .unwrap();
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    let evaluation = &card["organization_evaluations"][0];
    assert_eq!(evaluation["set_name"], "Basics");
    assert_eq!(evaluation["correct"], 2);
    assert_eq!(evaluation["batch_id"], batch.to_string());

    // Readers read; only catalog writers create.
    let auditor = harness.user(org, None, "auditor").await;
    let auditor_session = harness.session(auditor).await;
    let response = evaluations
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(sets.clone())
                .header(
                    COOKIE,
                    format!(
                        "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                        auditor_session.0, auditor_session.1
                    ),
                )
                .header("idempotency-key", "k")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"name": "N", "items": [{"id": "a", "expected": "b", "match": "exact"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND outcome = 'succeeded'
           AND action IN ('model_evaluation_set.create', 'model_evaluation_run.create')",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, 2);
    harness.pool.close().await;
}

#[tokio::test]
async fn evaluation_sets_are_deleted_softly() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("model-cards-evaluation-delete").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let session = harness.session(owner).await;
    let objects =
        std::sync::Arc::new(gateway::application::trace_objects::MemoryTraceObjects::default());
    let evaluations = gateway::transport::model_evaluations::router(
        AuthStore::new(harness.pool.clone()),
        AdminStore::new(harness.pool.clone()),
        Some(harness.crypto.clone()),
        gateway::persistence::model_evaluations::ModelEvaluationStore::new(harness.pool.clone()),
        Some(objects.clone()),
    );
    // (method, uri, body, idempotency key, If-Match) to (status, ETag, body).
    let call = |method: &'static str,
                uri: String,
                body: Option<Value>,
                key: String,
                if_match: Option<&'static str>| {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            )
            .header("idempotency-key", key)
            .header("content-type", "application/json");
        if let Some(if_match) = if_match {
            builder = builder.header("if-match", if_match);
        }
        let router = evaluations.clone();
        async move {
            let response = router
                .oneshot(
                    builder
                        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let etag = response
                .headers()
                .get("etag")
                .map(|value| value.to_str().unwrap().to_owned());
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                etag,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let key = || Uuid::new_v4().to_string();
    let sets = format!(
        "/api/admin/v1/organizations/{}/catalog/evaluation-sets",
        org.0
    );
    let (status, etag, set) = call(
        "POST",
        sets.clone(),
        Some(json!({"name": "Basics", "items": [
            {"id": "q1", "expected": "Paris", "match": "exact"},
            {"id": "q2", "expected": "4", "match": "contains"}]})),
        key(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{set}");
    assert_eq!(set["version"], 1);
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    let set_id = set["id"].as_str().unwrap().to_owned();
    let one = format!("{sets}/{set_id}");
    let runs = format!("{one}/runs");
    let batch = completed_batch(&harness, org, owner, &objects).await;
    // Named as the batches APIs name it.
    let (status, _, run) = call(
        "POST",
        runs.clone(),
        Some(json!({"batch_id": format!("batch_{batch}")})),
        key(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let (status, etag, _) = call("GET", one.clone(), None, key(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"v1\""));

    // A delete needs If-Match, and the current version.
    let (status, _, body) = call("DELETE", one.clone(), None, key(), None).await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{body}");
    let (status, _, body) = call("DELETE", one.clone(), None, key(), Some("\"v2\"")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "version_conflict");
    // A reader may not delete.
    let auditor = harness.user(org, None, "auditor").await;
    let auditor_session = harness.session(auditor).await;
    let response = evaluations
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri(one.clone())
                .header(
                    COOKIE,
                    format!(
                        "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                        auditor_session.0, auditor_session.1
                    ),
                )
                .header("idempotency-key", "k")
                .header("if-match", "\"v1\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let delete_key = key();
    let (status, _, _) = call(
        "DELETE",
        one.clone(),
        None,
        delete_key.clone(),
        Some("\"v1\""),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A retry replays; a new delete finds nothing.
    let (status, _, _) = call(
        "DELETE",
        one.clone(),
        None,
        delete_key.clone(),
        Some("\"v1\""),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, body) = call("DELETE", one.clone(), None, key(), Some("*")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Hidden from lists, reads, runs and cards.
    let (status, _, listed) = call("GET", sets.clone(), None, key(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"], json!([]));
    let (status, _, _) = call("GET", one.clone(), None, key(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call("GET", runs.clone(), None, key(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(
        "POST",
        runs.clone(),
        Some(json!({"batch_id": batch})),
        key(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    harness
        .activate(org, owner, &manifest("v1", 2, false))
        .await;
    sqlx::query(
        "UPDATE gateway.model_evaluation_result SET model = $2
         WHERE organization_id = $1 AND model = 'model-a'",
    )
    .bind(org.0)
    .bind(MODEL)
    .execute(&harness.pool)
    .await
    .unwrap();
    let (_, card, _) = harness.card(org, MODEL_PATH, Some(&session)).await;
    assert_eq!(card["organization_evaluations"], json!([]));

    // Kept: the set, who deleted it, its run and scores, and the audit.
    let (deleted_by, kept_items): (Option<Uuid>, i32) = sqlx::query_as(
        "SELECT deleted_by, jsonb_array_length(items) FROM gateway.model_evaluation_set
         WHERE organization_id = $1 AND id = $2 AND deleted_at IS NOT NULL",
    )
    .bind(org.0)
    .bind(Uuid::parse_str(&set_id).unwrap())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(deleted_by, Some(owner.0));
    assert_eq!(kept_items, 2);
    let scores: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.model_evaluation_result WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(scores, 2);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND outcome = 'succeeded'
           AND action = 'model_evaluation_set.delete'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    harness.pool.close().await;
}
