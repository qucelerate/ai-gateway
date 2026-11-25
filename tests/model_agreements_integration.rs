//! Model agreements (D-035) end to end, against a fake of STS and Bedrock's
//! control plane and runtime: an administrator lists the models whose
//! evidence is `agreement_required` with their live offers, accepts named
//! offers (refused whole when an offer changed or the role may read no
//! offer; per model otherwise, idempotent and audited, each recorded before
//! AWS is called), and the worker checks each agreement with the free
//! availability check until it is available, calling the model once only
//! when asked to. Only an account connected with the organization's own
//! role may accept. Discovery afterwards probes only with automatic offers
//! on.
//!
//! No real provider is called. Needs `GATEWAY_TEST_DATABASE_URL`.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use gateway::{
    AppConfig, CatalogManifest, CatalogStore, GatewayCommand, OrganizationId,
    domain::{
        agents::AgentTokenInput,
        identity::MembershipRole,
        model_evidence::{Entitlement, EvidenceKey, Observation},
    },
    jobs::discovery::{DiscoveryRunner, ProviderAccounts},
    persistence::{
        agents::AgentStore,
        auto_offer::AutoOfferStore,
        catalog_sync::CatalogSyncStore,
        model_agreement::{Acceptance, AttemptContext, ModelAgreementStore},
        model_evidence::ModelEvidenceStore,
    },
    provider::InferenceProviderAdapter,
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const PEPPER: &str = "model-agreements-test-pepper-0000000000";
const KEY: &str = "bedrock-main";
const HAIKU: &str = "anthropic.claude-haiku-5-5";
const GROK: &str = "xai.grok-9";
const CONFLICT: &str = "cohere.command-z-v1";
const DENIED: &str = "mistral.large-9-v1";
/// The token Bedrock hands out; it must never leave the gateway.
const TOKEN_MARK: &str = "AQICAH-secret-token";

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn database_url() -> Option<String> {
    match std::env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => None,
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

// ---------------------------------------------------------------- Bedrock

#[derive(Default)]
struct BedrockFake {
    /// Each model's `agreementAvailability.status`.
    availability: HashMap<String, &'static str>,
    /// The role may not read offers.
    deny_offers: bool,
    /// Models whose offers the role may not read.
    deny_offers_for: HashSet<String>,
    /// Every availability check is throttled.
    throttle_availability: bool,
    /// Models the role may not invoke (`authorizationStatus`).
    unauthorized: HashSet<String>,
    /// When set, each `CreateFoundationModelAgreement` waits for a permit.
    create_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Offer ids by model; a model without one has a single offer named
    /// `offer-<first word>`.
    offers: HashMap<String, Vec<String>>,
    /// Models sold under one Marketplace subscription, as Bedrock's
    /// Stability image models are: accepting one makes the others'
    /// agreements pending, and their own creates are then refused.
    shared: HashSet<String>,
    /// Models whose create AWS refuses with a `ValidationException`, each
    /// with the agreement status AWS shows afterwards (`None` leaves it).
    reject: HashMap<String, Option<&'static str>>,
    /// Models and offer tokens agreements were created with.
    creates: Vec<(String, String)>,
    /// Models called through `Converse`, with their `maxTokens` and the
    /// access key that signed the call (each organization's assumed role
    /// has its own).
    converse: Vec<(String, Value, String)>,
}

impl BedrockFake {
    /// The models `key` called.
    fn called_by(&self, key: &str) -> Vec<(String, Value)> {
        let mut called: Vec<(String, Value)> = self
            .converse
            .iter()
            .filter(|(_, _, signer)| signer == key)
            .map(|(model, tokens, _)| (model.clone(), tokens.clone()))
            .collect();
        called.sort_by(|a, b| a.0.cmp(&b.0));
        called
    }
}

/// The access key STS hands an organization's assumed role.
fn assumed_key(org: OrganizationId) -> String {
    format!("ASIA{}", org.0.simple().to_string()[..16].to_uppercase())
}

async fn sts(body: String) -> Response {
    let session = body
        .split('&')
        .find_map(|pair| pair.strip_prefix("RoleSessionName="))
        .unwrap_or_default();
    let org = session.trim_start_matches("qucelerate-");
    let key = format!("ASIA{}", org.get(..16).unwrap_or("UNKNOWN").to_uppercase());
    let expiration = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    (
        StatusCode::OK,
        format!(
            "<AssumeRoleResponse><AssumeRoleResult><Credentials>\
             <AccessKeyId>{key}</AccessKeyId><SecretAccessKey>s</SecretAccessKey>\
             <SessionToken>t</SessionToken><Expiration>{expiration}</Expiration>\
             </Credentials></AssumeRoleResult></AssumeRoleResponse>"
        ),
    )
        .into_response()
}

fn signer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split("Credential=").nth(1))
        .and_then(|value| value.split('/').next())
        .unwrap_or_default()
        .to_owned()
}

type Bedrock = Arc<Mutex<BedrockFake>>;

fn aws_error(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        [("x-amzn-ErrorType", kind.to_owned())],
        Json(json!({ "message": message })),
    )
        .into_response()
}

fn default_offer(model: &str) -> String {
    format!(
        "offer-{}",
        model.split(['.', '-']).nth(1).unwrap_or("x").to_lowercase()
    )
}

async fn foundation_models() -> Json<Value> {
    Json(json!({"modelSummaries": [
        {"modelId": HAIKU, "inferenceTypesSupported": ["INFERENCE_PROFILE"]},
        {"modelId": GROK, "inferenceTypesSupported": ["ON_DEMAND"]},
        {"modelId": CONFLICT, "inferenceTypesSupported": ["ON_DEMAND"]},
        {"modelId": DENIED, "inferenceTypesSupported": ["ON_DEMAND"]}
    ]}))
}

async fn inference_profiles() -> Json<Value> {
    let arn = |region: &str| format!("arn:aws:bedrock:{region}::foundation-model/{HAIKU}");
    Json(json!({"inferenceProfileSummaries": [
        {"inferenceProfileId": format!("us.{HAIKU}"),
         "models": [{"modelArn": arn("us-east-1")}, {"modelArn": arn("us-west-2")}]},
        {"inferenceProfileId": format!("global.{HAIKU}"),
         "models": [{"modelArn": arn("us-east-1")}]}
    ]}))
}

async fn offers(State(fake): State<Bedrock>, Path(model): Path<String>) -> Response {
    let fake = fake.lock().unwrap();
    if fake.deny_offers || fake.deny_offers_for.contains(&model) {
        return aws_error(
            StatusCode::FORBIDDEN,
            "AccessDeniedException",
            "User is not authorized to perform: bedrock:ListFoundationModelAgreementOffers",
        );
    }
    let ids = fake
        .offers
        .get(&model)
        .cloned()
        .unwrap_or_else(|| vec![default_offer(&model)]);
    let offers: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "offerId": id,
                "offerToken": format!("{TOKEN_MARK}-{id}"),
                "termDetails": {
                    "usageBasedPricingTerm": {"rateCard": [
                        {"dimension": "USE1_input_tokens_global_standard", "price": "0.1",
                         "description": "Input Tokens - Standard, Global", "unit": "Units"},
                        {"dimension": "USE2_input_tokens_global_standard", "price": "0.1",
                         "description": "Input Tokens - Standard, Global", "unit": "Units"}
                    ]},
                    "legalTerm": {"url": "https://legal.example/eula"},
                    "supportTerm": {"refundPolicyDescription": "No refunds"}
                }
            })
        })
        .collect();
    Json(json!({ "modelId": model, "offers": offers })).into_response()
}

async fn availability(State(fake): State<Bedrock>, Path(model): Path<String>) -> Response {
    let fake = fake.lock().unwrap();
    if fake.throttle_availability {
        return aws_error(
            StatusCode::TOO_MANY_REQUESTS,
            "ThrottlingException",
            "Too many requests, please wait before trying again.",
        );
    }
    let status = fake
        .availability
        .get(&model)
        .copied()
        .unwrap_or("NOT_AVAILABLE");
    let authorization = if fake.unauthorized.contains(&model) {
        "NOT_AUTHORIZED"
    } else {
        "AUTHORIZED"
    };
    Json(json!({
        "modelId": model,
        "agreementAvailability": {"status": status},
        "authorizationStatus": authorization,
        "entitlementAvailability": "AVAILABLE",
        "regionAvailability": "AVAILABLE"
    }))
    .into_response()
}

async fn create(State(fake): State<Bedrock>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let model = body["modelId"].as_str().unwrap().to_owned();
    let token = body["offerToken"].as_str().unwrap().to_owned();
    let gate = {
        let mut fake = fake.lock().unwrap();
        fake.creates.push((model.clone(), token));
        fake.create_gate.clone()
    };
    if let Some(gate) = gate {
        gate.acquire().await.unwrap().forget();
    }
    let mut fake = fake.lock().unwrap();
    let validation = || {
        aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "The provided offer token is not valid for this model.",
        )
    };
    if let Some(after) = fake.reject.get(&model).copied() {
        if let Some(after) = after {
            fake.availability.insert(model.clone(), after);
        }
        return validation();
    }
    if fake.shared.contains(&model) {
        if fake
            .availability
            .get(&model)
            .is_some_and(|s| *s != "NOT_AVAILABLE")
        {
            return validation();
        }
        for other in fake.shared.clone() {
            fake.availability.insert(other, "PENDING");
        }
        return (StatusCode::ACCEPTED, Json(json!({ "modelId": model }))).into_response();
    }
    match model.as_str() {
        CONFLICT => aws_error(
            StatusCode::BAD_REQUEST,
            "ConflictException",
            "An agreement already exists for this model.",
        ),
        // As AWS answers a role without the Marketplace subscription.
        DENIED => aws_error(
            StatusCode::FORBIDDEN,
            "AccessDeniedException",
            "User is not authorized to perform: aws-marketplace:Subscribe",
        ),
        _ => {
            fake.availability.insert(model.clone(), "PENDING");
            (StatusCode::ACCEPTED, Json(json!({ "modelId": model }))).into_response()
        }
    }
}

async fn converse(
    State(fake): State<Bedrock>,
    Path(model): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    fake.lock().unwrap().converse.push((
        model.clone(),
        body["inferenceConfig"]["maxTokens"].clone(),
        signer(&headers),
    ));
    if model == GROK {
        // As Bedrock refuses a model AWS has not enabled for the account,
        // agreement or not.
        return aws_error(
            StatusCode::FORBIDDEN,
            "AccessDeniedException",
            "You don't have access to the model with the specified model ID.",
        );
    }
    Json(json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "h"}]}},
        "stopReason": "max_tokens",
        "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
    }))
    .into_response()
}

async fn bedrock() -> (String, Bedrock) {
    let fake: Bedrock = Arc::new(Mutex::new(BedrockFake::default()));
    let url = serve(
        Router::new()
            .route("/foundation-models", get(foundation_models))
            .route("/inference-profiles", get(inference_profiles))
            .route(
                "/list-foundation-model-agreement-offers/{model}",
                get(offers),
            )
            .route("/foundation-model-availability/{model}", get(availability))
            .route("/create-foundation-model-agreement", post(create))
            .route("/model/{model}/converse", post(converse))
            .route("/sts/", post(sts))
            .with_state(fake.clone()),
    )
    .await;
    (url, fake)
}

// ---------------------------------------------------------------- stack

struct Stack {
    pool: PgPool,
    api: String,
    worker: gateway::jobs::Worker,
    /// The API's provider accounts, for a discovery run of this
    /// organization's account alone.
    accounts: Arc<dyn ProviderAccounts>,
    org: OrganizationId,
    owner: String,
    reader: String,
    http: reqwest::Client,
}

fn config(url: &str, profile: &str, bedrock: &str) -> AppConfig {
    let vars = [
        ("DATABASE_URL", url.to_owned()),
        ("GATEWAY_PROFILE", profile.to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("BEDROCK_AWS_REGION", "us-east-1".to_owned()),
        ("BEDROCK_AWS_ACCESS_KEY_ID", "AKIDEXAMPLE".to_owned()),
        ("BEDROCK_AWS_SECRET_ACCESS_KEY", "bedrock-secret".to_owned()),
        ("BEDROCK_BASE_URL", bedrock.to_owned()),
        ("BEDROCK_CONTROL_BASE_URL", bedrock.to_owned()),
        ("BEDROCK_STS_ENDPOINT", format!("{bedrock}/sts/")),
        ("GATEWAY_CREDENTIAL_ENCRYPTION_KEY", "A".repeat(43)),
    ];
    AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap()
}

fn manifest() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "agreements-test"},
        "providers": [{"adapter_instance_key": KEY, "family": "bedrock"}],
        "deployments": [],
        "prices": [],
        "routes": []
    }))
    .unwrap()
}

async fn agent(
    pool: &PgPool,
    slug: &str,
    name: &str,
    role: MembershipRole,
) -> (OrganizationId, String) {
    let agent = AgentStore::new(pool.clone())
        .bootstrap(
            slug,
            Some("Agreements test"),
            name,
            role,
            &AgentTokenInput {
                name: "test".into(),
                expires_in_days: Some(1),
                service_account_id: None,
            },
        )
        .await
        .unwrap();
    (agent.organization_id, agent.token.as_str().to_owned())
}

/// A test's stack; with `tenant_role` the organization connects its own
/// role, else the account is called with the process's credential.
async fn stack(url: &str, bedrock: &str, tenant_role: bool) -> Stack {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let state = gateway::application::connect(&config(url, "api", bedrock))
        .await
        .unwrap();
    let accounts = state.provider_accounts().cloned().unwrap();
    let api = serve(gateway::transport::router(state)).await;
    let worker = gateway::application::connect_worker(&config(url, "worker", bedrock))
        .await
        .unwrap();
    let slug = format!("agreements-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let (org, owner) = agent(&pool, &slug, "ops", MembershipRole::Owner).await;
    let (_, reader) = agent(&pool, &slug, "viewer", MembershipRole::Auditor).await;
    // The worker checks every organization's agreements: only this test's
    // are due.
    sqlx::query(
        "UPDATE gateway.model_agreement SET next_check_at = NULL WHERE organization_id <> $1",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    let manifest = manifest();
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    let stack = Stack {
        pool,
        api,
        worker,
        accounts,
        org,
        owner,
        reader,
        http: reqwest::Client::new(),
    };
    if tenant_role {
        let (status, body) = stack
            .put(
                "credentials",
                json!({"role_arn": "arn:aws:iam::123456789012:role/QucelerateGateway"}),
            )
            .await;
        assert_eq!(status, 201, "{body}");
    }
    stack
}

/// Delegates to the API's provider accounts for one organization only, so
/// a discovery run lists no other test's accounts.
struct OneOrganization {
    org: OrganizationId,
    inner: Arc<dyn ProviderAccounts>,
}

#[async_trait::async_trait]
impl ProviderAccounts for OneOrganization {
    async fn adapter(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: gateway::domain::catalog::ProviderFamily,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, gateway::domain::inference::ProviderError>
    {
        if organization_id != self.org {
            return Ok(None);
        }
        self.inner
            .adapter(organization_id, adapter_instance_key, family)
            .await
    }
}

impl Stack {
    /// `PUT .../catalog/{what}/{KEY}` as the owner, creating it.
    async fn put(&self, what: &str, body: Value) -> (u16, Value) {
        let response = self
            .http
            .put(format!(
                "{}/api/admin/v1/organizations/{}/catalog/{what}/{KEY}",
                self.api, self.org.0
            ))
            .bearer_auth(&self.owner)
            .header("Idempotency-Key", Uuid::new_v4().to_string())
            .header("If-Match", "*")
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Runs this organization's queued discovery of the account to its
    /// end, with automatic offers and probes as the worker runs them.
    async fn discover(&self) {
        let runner = DiscoveryRunner::new(
            CatalogSyncStore::new(self.pool.clone()),
            Arc::new(OneOrganization {
                org: self.org,
                inner: self.accounts.clone(),
            }),
        )
        .with_evidence(ModelEvidenceStore::new(self.pool.clone()))
        .with_auto_offers(AutoOfferStore::new(self.pool.clone()));
        for _ in 0..20 {
            runner.run_once().await.unwrap();
            let open: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM gateway.catalog_sync_job
                 WHERE organization_id = $1 AND status IN ('queued', 'running')",
            )
            .bind(self.org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            if open == 0 {
                return;
            }
        }
        panic!("discovery did not finish");
    }

    /// The models this organization's role called.
    fn called(&self, fake: &Bedrock) -> Vec<(String, Value)> {
        fake.lock().unwrap().called_by(&assumed_key(self.org))
    }

    /// Records `models` as discovery's availability check would: an
    /// agreement is required.
    async fn agreement_required(&self, models: &[&str]) {
        let store = ModelEvidenceStore::new(self.pool.clone());
        for model in models {
            store
                .record(&Observation::of_entitlement(
                    self.key(model),
                    Entitlement::AgreementRequired,
                    Utc::now() - chrono::Duration::minutes(5),
                ))
                .await
                .unwrap();
        }
    }

    fn key(&self, model: &str) -> EvidenceKey {
        EvidenceKey {
            organization_id: self.org,
            adapter_instance_key: KEY.into(),
            family: "bedrock".into(),
            model: (*model).into(),
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!(
            "{}/api/admin/v1/organizations/{}/catalog/agreements/{KEY}{suffix}",
            self.api, self.org.0
        )
    }

    async fn list(&self, token: &str) -> (u16, Value) {
        let response = self
            .http
            .get(self.url(""))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        assert!(!text.contains(TOKEN_MARK), "an offer token leaked: {text}");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn accept(&self, token: &str, key: Option<&str>, body: Value) -> (u16, Value) {
        let mut request = self
            .http
            .post(self.url("/accept"))
            .bearer_auth(token)
            .json(&body);
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        assert!(!text.contains(TOKEN_MARK), "an offer token leaked: {text}");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn evidence(&self, model: &str) -> (String, Option<String>, String) {
        sqlx::query_as(
            "SELECT state, reason, source FROM gateway.catalog_model_evidence
             WHERE organization_id = $1 AND adapter_instance_key = $2 AND model = $3",
        )
        .bind(self.org.0)
        .bind(KEY)
        .bind(model)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Runs the worker's agreement checks as if `seconds` had passed.
    async fn check(&self, seconds: i64) -> gateway::jobs::agreements::AgreementReport {
        self.worker
            .agreements
            .run_once(Utc::now() + chrono::Duration::seconds(seconds))
            .await
            .unwrap()
    }
}

fn model<'a>(view: &'a Value, id: &str) -> &'a Value {
    view["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == id)
        .unwrap_or_else(|| panic!("{id} not listed: {view}"))
}

fn result<'a>(results: &'a Value, id: &str) -> &'a Value {
    results["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["model"] == id)
        .unwrap_or_else(|| panic!("no result for {id}: {results}"))
}

fn named(models: &[(&str, &str)], verify: bool) -> Value {
    json!({
        "models": models
            .iter()
            .map(|(model, offer)| json!({"model": model, "offer_id": offer}))
            .collect::<Vec<_>>(),
        "verify": verify,
    })
}

#[tokio::test]
async fn agreements_are_listed_accepted_per_model_and_verified_when_asked() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    stack
        .agreement_required(&[
            &format!("us.{HAIKU}"),
            &format!("global.{HAIKU}"),
            GROK,
            CONFLICT,
            DENIED,
        ])
        .await;

    // Profiles collapse to the foundation model whose agreement they need;
    // offers are read live, with the account's region's rates.
    let (status, view) = stack.list(&stack.reader).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["permission"], "granted");
    let haiku = model(&view, HAIKU);
    assert_eq!(
        haiku["unblocks"],
        json!([format!("global.{HAIKU}"), format!("us.{HAIKU}")])
    );
    assert_eq!(haiku["agreement_required"], true);
    let offer = &haiku["offers"][0];
    assert_eq!(offer["offer_id"], "offer-claude");
    assert_eq!(offer["rate_card_region"], "USE1");
    assert_eq!(offer["rate_card_total"], 2);
    assert_eq!(offer["rate_card"].as_array().unwrap().len(), 1);
    assert_eq!(offer["refund_policy"], "No refunds");
    assert!(offer.get("offer_token").is_none());
    assert_eq!(view["models"].as_array().unwrap().len(), 4);

    // Role gating: reading is not accepting.
    let all = named(&[(HAIKU, "offer-claude")], false);
    let (status, refused) = stack
        .accept(&stack.reader, Some("reader-1"), all.clone())
        .await;
    assert_eq!(status, 403, "{refused}");
    let (status, refused) = stack.accept(&stack.owner, None, all).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["code"], "idempotency_key_required");

    // An offer that changed since it was read refuses the whole request.
    fake.lock()
        .unwrap()
        .offers
        .insert(GROK.into(), vec!["offer-grok-v2".into()]);
    let (status, changed) = stack
        .accept(
            &stack.owner,
            Some("changed-1"),
            named(&[(HAIKU, "offer-claude"), (GROK, "offer-grok")], false),
        )
        .await;
    assert_eq!(status, 409, "{changed}");
    assert_eq!(changed["code"], "agreement_offer_changed");
    assert_eq!(
        changed["changes"],
        json!([{"model": GROK, "requested_offer_id": "offer-grok", "current_offer_ids": ["offer-grok-v2"]}])
    );
    assert!(fake.lock().unwrap().creates.is_empty());

    // Per model: made, already made, refused for the role, and not needed.
    let body = named(
        &[
            (HAIKU, "offer-claude"),
            (GROK, "offer-grok-v2"),
            (CONFLICT, "offer-command"),
            (DENIED, "offer-large"),
            ("meta.llama-free-v1", "offer-llama"),
        ],
        true,
    );
    let (status, results) = stack
        .accept(&stack.owner, Some("accept-1"), body.clone())
        .await;
    assert_eq!(status, 200, "{results}");
    assert_eq!(result(&results, HAIKU)["outcome"], "accepted");
    assert_eq!(result(&results, HAIKU)["status"], "pending");
    assert_eq!(result(&results, GROK)["outcome"], "accepted");
    assert_eq!(result(&results, CONFLICT)["outcome"], "already_accepted");
    let denied = result(&results, DENIED);
    assert_eq!(denied["outcome"], "failed");
    assert_eq!(denied["status"], "failed");
    assert_eq!(denied["reason"], "agreement_permission_missing");
    assert_eq!(denied["provider_code"], "accessDeniedException");
    assert_eq!(
        result(&results, "meta.llama-free-v1")["reason"],
        "not_agreement_required"
    );
    {
        let fake = fake.lock().unwrap();
        let mut created: Vec<&str> = fake.creates.iter().map(|(m, _)| m.as_str()).collect();
        created.sort_unstable();
        assert_eq!(created, [HAIKU, CONFLICT, DENIED, GROK]);
        // Each with the token of the offer it was accepted for.
        let grok = fake.creates.iter().find(|(m, _)| m == GROK).unwrap();
        assert_eq!(grok.1, format!("{TOKEN_MARK}-offer-grok-v2"));
    }

    // A retry replays without calling AWS; another request under the key
    // is refused.
    let (status, replayed) = stack.accept(&stack.owner, Some("accept-1"), body).await;
    assert_eq!(status, 200);
    assert_eq!(replayed, results);
    assert_eq!(fake.lock().unwrap().creates.len(), 4);
    let (status, conflict) = stack
        .accept(
            &stack.owner,
            Some("accept-1"),
            named(&[(HAIKU, "offer-claude")], true),
        )
        .await;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["code"], "idempotency_conflict");

    // Each model's attempt is audited before AWS is called and again with
    // its result; refused requests are audited too.
    let audits: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT e.action, e.outcome,
                COALESCE(to_jsonb(a) - 'id', '{}'::jsonb) AS attempt
         FROM gateway.audit_event e
         LEFT JOIN gateway.model_agreement_acceptance a
           ON a.id = e.entity_id AND a.organization_id = e.organization_id
         WHERE e.organization_id = $1 AND e.entity_type = 'model_agreement'
         ORDER BY e.action, a.model, e.outcome",
    )
    .bind(stack.org.0)
    .fetch_all(&stack.pool)
    .await
    .unwrap();
    let actions: Vec<(&str, &str, &str)> = audits
        .iter()
        .map(|(action, outcome, attempt)| {
            (
                action.as_str(),
                outcome.as_str(),
                attempt["model"].as_str().unwrap_or("-"),
            )
        })
        .collect();
    assert_eq!(
        actions,
        [
            ("model_agreement.accept", "denied", "-"),
            // The offer that changed.
            ("model_agreement.accept", "failed", "-"),
            ("model_agreement.accept", "succeeded", "-"),
            ("model_agreement.accepted", "succeeded", HAIKU),
            ("model_agreement.accepted", "succeeded", GROK),
            ("model_agreement.accepting", "succeeded", HAIKU),
            ("model_agreement.accepting", "succeeded", CONFLICT),
            ("model_agreement.accepting", "succeeded", DENIED),
            ("model_agreement.accepting", "succeeded", GROK),
            ("model_agreement.already_accepted", "succeeded", CONFLICT),
            ("model_agreement.failed", "failed", "meta.llama-free-v1"),
            ("model_agreement.failed", "failed", DENIED),
        ]
    );
    let attempt = |action: &str, id: &str| {
        audits
            .iter()
            .find(|(a, _, m)| a == action && m["model"] == id)
            .map(|(_, _, m)| m.clone())
            .unwrap()
    };
    let haiku_audit = attempt("model_agreement.accepted", HAIKU);
    assert_eq!(haiku_audit["offer_id"], "offer-claude");
    assert_eq!(haiku_audit["adapter_instance_key"], KEY);
    assert_eq!(haiku_audit["outcome"], "accepted");
    assert!(haiku_audit["settled_at"].is_string());
    assert!(haiku_audit["actor_user_id"].is_string());
    assert_eq!(
        haiku_audit["rate_card"][0]["dimension"],
        "USE1_input_tokens_global_standard"
    );
    let denied_audit = attempt("model_agreement.failed", DENIED);
    assert_eq!(denied_audit["reason"], "agreement_permission_missing");
    assert_eq!(denied_audit["provider_code"], "accessDeniedException");
    assert!(
        !audits
            .iter()
            .any(|(_, _, m)| m.to_string().contains(TOKEN_MARK))
    );

    // The worker: still pending.
    let report = stack.check(1).await;
    assert_eq!(report.checked, 3, "{report:?}");
    let (_, view) = stack.list(&stack.owner).await;
    let agreement = &model(&view, HAIKU)["agreement"];
    assert_eq!(agreement["status"], "pending");
    assert_eq!(agreement["checking"], true);
    assert_eq!(agreement["probe"], true);
    assert!(stack.called(&fake).is_empty());

    // Available: one 1-token call per foundation model, through its global
    // profile; a refusal after the agreement is in place points at AWS.
    {
        let mut fake = fake.lock().unwrap();
        for model in [HAIKU, GROK, CONFLICT] {
            fake.availability.insert(model.into(), "AVAILABLE");
        }
    }
    let report = stack.check(25).await;
    assert_eq!(report.settled, 3, "{report:?}");
    {
        let called = stack.called(&fake);
        assert_eq!(
            called,
            [
                (CONFLICT.to_owned(), json!(1)),
                (format!("global.{HAIKU}"), json!(1)),
                (GROK.to_owned(), json!(1)),
            ]
        );
    }
    let (_, view) = stack.list(&stack.owner).await;
    let haiku = &model(&view, HAIKU)["agreement"];
    assert_eq!(haiku["status"], "verified");
    assert_eq!(haiku["probed_model"], format!("global.{HAIKU}"));
    assert!(haiku["verified_at"].is_string());
    assert_eq!(haiku["checking"], false);
    let grok = &model(&view, GROK)["agreement"];
    assert_eq!(grok["status"], "refused");
    assert_eq!(grok["reason"], "access_request_required");
    assert!(
        grok["detail"]
            .as_str()
            .unwrap()
            .contains("request access from AWS"),
        "{grok}"
    );
    assert_eq!(grok["provider_code"], "accessDeniedException");
    // No longer required; the evidence moved on.
    assert_eq!(model(&view, HAIKU)["agreement_required"], false);
    assert_eq!(
        stack.evidence(&format!("global.{HAIKU}")).await,
        ("verified".to_owned(), None, "probe".to_owned())
    );
    assert_eq!(
        stack.evidence(&format!("us.{HAIKU}")).await,
        ("entitled".to_owned(), None, "control_plane".to_owned())
    );
    assert_eq!(
        stack.evidence(GROK).await,
        (
            "refused".to_owned(),
            Some("access_denied".to_owned()),
            "probe".to_owned()
        )
    );
    // A discovery run is queued, so automatic offers pick the models up.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.catalog_sync_job
         WHERE organization_id = $1 AND adapter_instance_key = $2 AND trigger = 'manual'",
    )
    .bind(stack.org.0)
    .bind(KEY)
    .fetch_one(&stack.pool)
    .await
    .unwrap();
    assert_eq!(queued, 1);
    // Nothing is left to check.
    assert_eq!(stack.check(3600).await.checked, 0);
}

#[tokio::test]
async fn only_named_offers_are_accepted_and_a_role_without_the_permission_accepts_nothing() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    stack
        .agreement_required(&[&format!("us.{HAIKU}"), GROK])
        .await;

    // There is no "all": the server accepts only offers someone named.
    let (status, refused) = stack
        .accept(&stack.owner, Some("all-1"), json!({"all": true}))
        .await;
    assert_eq!(status, 422, "{refused}");
    let (status, refused) = stack
        .accept(&stack.owner, Some("none-1"), json!({"models": []}))
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["code"], "invalid_agreement_request");

    // A role without the opt-in permissions: offers cannot be read, and
    // nothing is accepted; the refusal is audited.
    fake.lock().unwrap().deny_offers = true;
    let (status, view) = stack.list(&stack.owner).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["permission"], "missing");
    assert_eq!(
        model(&view, HAIKU)["offers_error"],
        "agreement_permission_missing"
    );
    let (status, refused) = stack
        .accept(
            &stack.owner,
            Some("denied-1"),
            named(&[(HAIKU, "offer-claude"), (GROK, "offer-grok")], false),
        )
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["code"], "agreement_permission_missing");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("bedrock:CreateFoundationModelAgreement")
    );
    assert!(fake.lock().unwrap().creates.is_empty());
    let refusals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'model_agreement.accept' AND outcome = 'failed'",
    )
    .bind(stack.org.0)
    .fetch_one(&stack.pool)
    .await
    .unwrap();
    assert_eq!(refusals, 1);

    // One model's offers refused: the list shows that model unreadable and
    // the others' offers, with the permission granted.
    {
        let mut fake = fake.lock().unwrap();
        fake.deny_offers = false;
        fake.deny_offers_for.insert(HAIKU.into());
    }
    let (status, view) = stack.list(&stack.owner).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["permission"], "granted");
    assert_eq!(
        model(&view, HAIKU)["offers_error"],
        "agreement_permission_missing"
    );
    assert_eq!(model(&view, GROK)["offers_error"], Value::Null);
    assert_eq!(model(&view, GROK)["offers"][0]["offer_id"], "offer-grok");

    // On accepting: that model fails, the other is accepted.
    {
        let mut fake = fake.lock().unwrap();
        fake.deny_offers_for.clear();
        fake.deny_offers_for.insert(GROK.into());
    }
    let (status, results) = stack
        .accept(
            &stack.owner,
            Some("partial-1"),
            named(&[(HAIKU, "offer-claude"), (GROK, "offer-grok")], false),
        )
        .await;
    assert_eq!(status, 200, "{results}");
    assert_eq!(result(&results, HAIKU)["outcome"], "accepted");
    assert_eq!(result(&results, GROK)["outcome"], "failed");
    assert_eq!(
        result(&results, GROK)["reason"],
        "agreement_permission_missing"
    );
    assert_eq!(fake.lock().unwrap().creates.len(), 1);

    // Available per AWS, and never called.
    fake.lock()
        .unwrap()
        .availability
        .insert(HAIKU.into(), "AVAILABLE");
    let report = stack.check(1).await;
    assert_eq!(report.settled, 1, "{report:?}");
    let (_, view) = stack.list(&stack.owner).await;
    let agreement = &model(&view, HAIKU)["agreement"];
    assert_eq!(agreement["status"], "available");
    assert_eq!(agreement["reason"], "not_verified");
    assert!(
        agreement["detail"]
            .as_str()
            .unwrap()
            .contains("not yet called")
    );
    assert!(stack.called(&fake).is_empty());
    assert_eq!(
        stack.evidence(&format!("us.{HAIKU}")).await,
        ("entitled".to_owned(), None, "control_plane".to_owned())
    );

    // Accepting a model whose agreement AWS already has makes none.
    let (status, again) = stack
        .accept(
            &stack.owner,
            Some("again-1"),
            named(&[(HAIKU, "offer-claude")], false),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(result(&again, HAIKU)["outcome"], "already_accepted");
    assert_eq!(fake.lock().unwrap().creates.len(), 1);
}

/// An account the gateway calls with the operator's own credential would
/// subscribe the operator's AWS account: it has no agreements.
#[tokio::test]
async fn an_account_on_the_operators_credential_has_no_agreements() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, false).await;
    stack.agreement_required(&[GROK]).await;
    let (status, refused) = stack.list(&stack.owner).await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["code"], "agreements_need_tenant_credential");
    let (status, refused) = stack
        .accept(
            &stack.owner,
            Some("process-1"),
            named(&[(GROK, "offer-grok")], false),
        )
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["code"], "agreements_need_tenant_credential");
    let fake = fake.lock().unwrap();
    assert!(fake.creates.is_empty());
}

/// Without `verify` the gateway calls nothing when accepting; with
/// automatic offers on, the discovery it queues verifies each identifier it
/// would offer with one 1-token call (one already probed by `verify` is not
/// probed again), and with them off, nothing is called. Models whose
/// entitlement the listing could not check are never probed.
#[tokio::test]
async fn discovery_after_an_agreement_probes_only_what_automatic_offers_would_offer() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    stack
        .agreement_required(&[&format!("us.{HAIKU}"), &format!("global.{HAIKU}"), GROK])
        .await;
    let accept = |key: &'static str, verify: bool| {
        let stack = &stack;
        async move {
            let (status, results) = stack
                .accept(
                    &stack.owner,
                    Some(key),
                    named(&[(HAIKU, "offer-claude")], verify),
                )
                .await;
            assert_eq!(status, 200, "{results}");
        }
    };

    // Automatic offers off: the queued discovery calls nothing.
    accept("off-1", false).await;
    fake.lock()
        .unwrap()
        .availability
        .insert(HAIKU.into(), "AVAILABLE");
    assert_eq!(stack.check(1).await.settled, 1);
    stack.discover().await;
    assert!(stack.called(&fake).is_empty());

    // On: one 1-token call per identifier it would offer (the profiles;
    // the foundation model answers only through them), none for the model
    // still needing an agreement.
    let (status, body) = stack.put("auto-offers", json!({"location": null})).await;
    assert_eq!(status, 201, "{body}");
    stack.discover().await;
    assert_eq!(
        stack.called(&fake),
        [
            (format!("global.{HAIKU}"), json!(1)),
            (format!("us.{HAIKU}"), json!(1)),
        ]
    );

    // With `verify`, the worker's probe counts as discovery's: the
    // identifier it called is not probed again.
    fake.lock().unwrap().converse.clear();
    sqlx::query("DELETE FROM gateway.catalog_model_probe WHERE organization_id = $1")
        .bind(stack.org.0)
        .execute(&stack.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gateway.catalog_model_evidence
         SET state = 'entitled', reason = NULL, verified_at = NULL
         WHERE organization_id = $1 AND model LIKE $2",
    )
    .bind(stack.org.0)
    .bind(format!("%{HAIKU}"))
    .execute(&stack.pool)
    .await
    .unwrap();
    accept("verify-1", true).await;
    assert_eq!(stack.check(1).await.settled, 1);
    assert_eq!(stack.called(&fake), [(format!("global.{HAIKU}"), json!(1))]);
    stack.discover().await;
    assert_eq!(
        stack.called(&fake),
        [
            (format!("global.{HAIKU}"), json!(1)),
            (format!("us.{HAIKU}"), json!(1)),
        ]
    );

    // A listing whose availability checks are throttled settles no
    // entitlement: its models wait for one that does, unprobed.
    fake.lock().unwrap().converse.clear();
    fake.lock().unwrap().throttle_availability = true;
    sqlx::query("DELETE FROM gateway.catalog_model_probe WHERE organization_id = $1")
        .bind(stack.org.0)
        .execute(&stack.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gateway.catalog_model_evidence
         SET state = 'entitled', reason = NULL, verified_at = NULL
         WHERE organization_id = $1 AND model LIKE $2",
    )
    .bind(stack.org.0)
    .bind(format!("%{HAIKU}"))
    .execute(&stack.pool)
    .await
    .unwrap();
    let mut conn = stack.pool.acquire().await.unwrap();
    let owner: Uuid = sqlx::query_scalar(
        "SELECT created_by FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(stack.org.0)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    CatalogSyncStore::insert_manual(
        &mut conn,
        stack.org,
        Uuid::new_v4(),
        KEY,
        "bedrock",
        gateway::domain::identity::UserAccountId(owner),
    )
    .await
    .unwrap();
    drop(conn);
    stack.discover().await;
    assert!(stack.called(&fake).is_empty());
}

/// An acceptance recorded before AWS was called, whose request never
/// settled it, is settled by the worker from AWS's availability; a check
/// stores its result only while its attempt is the latest, and a probe is
/// taken once.
#[tokio::test]
async fn the_worker_settles_interrupted_acceptances_and_never_overwrites_a_newer_one() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    let store = ModelAgreementStore::new(stack.pool.clone());
    let actor: Uuid = sqlx::query_scalar(
        "SELECT created_by FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(stack.org.0)
    .fetch_one(&stack.pool)
    .await
    .unwrap();
    let context = AttemptContext {
        organization_id: stack.org,
        adapter_instance_key: KEY,
        family: "bedrock",
        actor: gateway::domain::identity::UserAccountId(actor),
        request_id: Uuid::new_v4(),
    };
    let acceptance = |model: &str, probe: bool| Acceptance {
        model: model.into(),
        unblocks: vec![model.into()],
        offer_id: Some("offer-x".into()),
        rate_card: vec![],
        rate_card_region: None,
        outcome: gateway::domain::model_agreement::AcceptOutcome::Accepted,
        reason: None,
        provider_code: None,
        probe,
    };
    let begin = |model: &'static str| {
        let pool = stack.pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            let id = ModelAgreementStore::begin_accepting(
                &mut tx,
                context,
                &acceptance(model, false),
                Utc::now(),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            id
        }
    };
    // Two requests that died after recording their attempts: AWS made one
    // agreement and not the other.
    let made = begin(HAIKU).await;
    let lost = begin(GROK).await;
    fake.lock()
        .unwrap()
        .availability
        .insert(HAIKU.into(), "PENDING");
    // Not before the request could still be running.
    assert_eq!(stack.check(60).await.checked, 0);
    stack.check(6 * 60).await;
    let outcome = |id: Uuid| {
        let pool = stack.pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT outcome, reason FROM gateway.model_agreement_acceptance WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(outcome(made).await, ("accepted".to_owned(), None));
    assert_eq!(
        outcome(lost).await,
        (
            "failed".to_owned(),
            Some("acceptance_interrupted".to_owned())
        )
    );
    let (_, view) = stack.list(&stack.owner).await;
    assert_eq!(model(&view, HAIKU)["agreement"]["status"], "pending");
    assert_eq!(model(&view, GROK)["agreement"]["status"], "failed");
    assert_eq!(
        model(&view, GROK)["agreement"]["reason"],
        "acceptance_interrupted"
    );
    let system: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_id = $2 AND action = 'model_agreement.failed'",
    )
    .bind(stack.org.0)
    .bind(lost)
    .fetch_one(&stack.pool)
    .await
    .unwrap();
    assert_eq!(system, 1);

    // A check claimed for one attempt does not store over a newer one, and
    // the newer attempt's own choice of probe stands.
    sqlx::query(
        "UPDATE gateway.model_agreement SET next_check_at = now() - interval '1 minute'
         WHERE organization_id = $1 AND model = $2",
    )
    .bind(stack.org.0)
    .bind(HAIKU)
    .execute(&stack.pool)
    .await
    .unwrap();
    let mut claimed = store
        .claim_due(Utc::now(), 5)
        .await
        .unwrap()
        .into_iter()
        .find(|due| due.organization_id == stack.org && due.model == HAIKU)
        .unwrap();
    let mut conn = stack.pool.acquire().await.unwrap();
    ModelAgreementStore::record(
        &mut conn,
        context,
        &gateway::persistence::model_agreement::Acceptance {
            outcome: gateway::domain::model_agreement::AcceptOutcome::AlreadyAccepted,
            ..acceptance(HAIKU, true)
        },
        Utc::now(),
    )
    .await
    .unwrap();
    drop(conn);
    let stale = gateway::persistence::model_agreement::CheckResult {
        status: gateway::domain::model_agreement::AgreementStatus::Available,
        reason: None,
        detail: None,
        provider_code: None,
        next_check_at: None,
        verified_at: None,
        probed_model: None,
    };
    claimed.probe = true;
    assert!(!store.take_probe(&claimed).await.unwrap());
    assert!(
        !store
            .finish_check(&claimed, &stale, Utc::now())
            .await
            .unwrap()
    );
    let (_, view) = stack.list(&stack.owner).await;
    let haiku = &model(&view, HAIKU)["agreement"];
    assert_eq!(haiku["status"], "pending");
    assert_eq!(haiku["probe"], true);
    // The newer attempt's probe is taken once.
    let current = store
        .claim_due(Utc::now() + chrono::Duration::seconds(1), 5)
        .await
        .unwrap()
        .into_iter()
        .find(|due| due.organization_id == stack.org && due.model == HAIKU)
        .unwrap();
    assert!(store.take_probe(&current).await.unwrap());
    assert!(!store.take_probe(&current).await.unwrap());
}

/// The key is claimed before AWS is called: while one request is still
/// accepting, the same key with the same body is in progress and with
/// another body a conflict, so only one subscription is made.
#[tokio::test]
async fn a_key_is_claimed_before_aws_is_called() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    stack.agreement_required(&[HAIKU, GROK]).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    fake.lock().unwrap().create_gate = Some(gate.clone());
    let haiku = named(&[(HAIKU, "offer-claude")], false);
    let first = stack.accept(&stack.owner, Some("held-1"), haiku.clone());
    let meanwhile = async {
        while fake.lock().unwrap().creates.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let same = stack
            .accept(&stack.owner, Some("held-1"), haiku.clone())
            .await;
        let other = stack
            .accept(
                &stack.owner,
                Some("held-1"),
                named(&[(GROK, "offer-grok")], false),
            )
            .await;
        gate.add_permits(10);
        (same, other)
    };
    let ((status, results), ((same, same_body), (other, other_body))) =
        tokio::join!(first, meanwhile);
    assert_eq!(status, 200, "{results}");
    assert_eq!(same, 409, "{same_body}");
    assert_eq!(same_body["code"], "idempotency_in_progress");
    assert_eq!(other, 409, "{other_body}");
    assert_eq!(other_body["code"], "idempotency_conflict");
    assert_eq!(fake.lock().unwrap().creates.len(), 1);
    // Finished: the same key replays without calling AWS.
    let (status, replayed) = stack.accept(&stack.owner, Some("held-1"), haiku).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(replayed, results);
    assert_eq!(fake.lock().unwrap().creates.len(), 1);
    // A refused request lets its key go: a retry is decided afresh.
    fake.lock().unwrap().create_gate = None;
    fake.lock().unwrap().deny_offers = true;
    let grok = named(&[(GROK, "offer-grok")], false);
    let (status, _) = stack
        .accept(&stack.owner, Some("refused-1"), grok.clone())
        .await;
    assert_eq!(status, 422);
    fake.lock().unwrap().deny_offers = false;
    let (status, retried) = stack.accept(&stack.owner, Some("refused-1"), grok).await;
    assert_eq!(status, 200, "{retried}");
    assert_eq!(result(&retried, GROK)["outcome"], "accepted");
}

/// On an account with no evidence yet, a listing whose availability checks
/// were throttled offers nothing it could not check: those models wait for
/// a listing that checks them.
#[tokio::test]
async fn automatic_offers_defer_models_a_listing_could_not_check() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    fake.lock().unwrap().throttle_availability = true;
    let (status, body) = stack.put("auto-offers", json!({"location": null})).await;
    assert_eq!(status, 201, "{body}");
    let queue = || async {
        let mut conn = stack.pool.acquire().await.unwrap();
        let owner: Uuid = sqlx::query_scalar(
            "SELECT created_by FROM gateway.provider_credential WHERE organization_id = $1",
        )
        .bind(stack.org.0)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        let _ = CatalogSyncStore::insert_manual(
            &mut conn,
            stack.org,
            Uuid::new_v4(),
            KEY,
            "bedrock",
            gateway::domain::identity::UserAccountId(owner),
        )
        .await;
    };
    let statuses = || async {
        sqlx::query_as::<_, (String, String)>(
            "SELECT model, status FROM gateway.catalog_auto_offered_model
             WHERE organization_id = $1 ORDER BY model",
        )
        .bind(stack.org.0)
        .fetch_all(&stack.pool)
        .await
        .unwrap()
    };
    queue().await;
    stack.discover().await;
    let deferred = statuses().await;
    assert!(
        deferred
            .iter()
            .any(|(model, status)| model.contains(HAIKU) && status == "unchecked"),
        "{deferred:?}"
    );
    assert!(
        !deferred.iter().any(|(_, status)| status == "offered"),
        "{deferred:?}"
    );
    assert!(stack.called(&fake).is_empty());

    // A listing that checks them decides them as usual.
    fake.lock().unwrap().throttle_availability = false;
    queue().await;
    stack.discover().await;
    let decided = statuses().await;
    assert!(
        !decided.iter().any(|(_, status)| status == "unchecked"),
        "{decided:?}"
    );
}

/// An available agreement is not a permission to call: a role AWS reports
/// may not invoke the model is recorded as missing the permission, never
/// entitled, and the agreement says so.
#[tokio::test]
async fn an_agreement_the_role_may_not_invoke_is_not_entitled() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    stack.agreement_required(&[GROK]).await;
    let (status, results) = stack
        .accept(
            &stack.owner,
            Some("deny-1"),
            named(&[(GROK, "offer-grok")], true),
        )
        .await;
    assert_eq!(status, 200, "{results}");
    {
        let mut fake = fake.lock().unwrap();
        fake.availability.insert(GROK.into(), "AVAILABLE");
        fake.unauthorized.insert(GROK.into());
    }
    stack.check(1).await;
    let (_, view) = stack.list(&stack.owner).await;
    let agreement = &model(&view, GROK)["agreement"];
    assert_eq!(agreement["status"], "refused", "{agreement}");
    assert_eq!(agreement["reason"], "invoke_permission_missing");
    assert_eq!(
        stack.evidence(GROK).await,
        (
            "requires_action".to_owned(),
            Some("permission_missing".to_owned()),
            "control_plane".to_owned()
        )
    );
    // Not called: the role may not.
    assert!(stack.called(&fake).is_empty());
}

/// The agreement worker and discovery claim a probe on one record: an
/// identifier discovery attempted within its retry window is not probed
/// again by `verify`.
#[tokio::test]
async fn a_verify_probe_yields_to_a_recent_discovery_probe() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    stack
        .agreement_required(&[&format!("us.{HAIKU}"), &format!("global.{HAIKU}")])
        .await;
    // Discovery lists the account (automatic offers off: no probe).
    let mut conn = stack.pool.acquire().await.unwrap();
    let owner: Uuid = sqlx::query_scalar(
        "SELECT created_by FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(stack.org.0)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    CatalogSyncStore::insert_manual(
        &mut conn,
        stack.org,
        Uuid::new_v4(),
        KEY,
        "bedrock",
        gateway::domain::identity::UserAccountId(owner),
    )
    .await
    .unwrap();
    drop(conn);
    stack.discover().await;
    // As discovery claims a probe: just attempted.
    let claimed = sqlx::query(
        "INSERT INTO gateway.catalog_model_probe
             (organization_id, adapter_instance_key, family, model, attempted_at)
         VALUES ($1, $2, 'bedrock', $3, now())
         ON CONFLICT (organization_id, adapter_instance_key, family, model)
         DO UPDATE SET attempted_at = now()",
    )
    .bind(stack.org.0)
    .bind(KEY)
    .bind(format!("global.{HAIKU}"))
    .execute(&stack.pool)
    .await
    .unwrap();
    assert_eq!(claimed.rows_affected(), 1);
    let (status, results) = stack
        .accept(
            &stack.owner,
            Some("yield-1"),
            named(&[(HAIKU, "offer-claude")], true),
        )
        .await;
    assert_eq!(status, 200, "{results}");
    fake.lock()
        .unwrap()
        .availability
        .insert(HAIKU.into(), "AVAILABLE");
    assert_eq!(stack.check(1).await.settled, 1);
    assert!(stack.called(&fake).is_empty());
    let (_, view) = stack.list(&stack.owner).await;
    assert_eq!(model(&view, HAIKU)["agreement"]["reason"], "not_verified");
}

/// Models one Marketplace subscription covers: the first accepted makes the
/// rest pending and AWS refuses their own creates, so each failed create is
/// read again. Pending is accepted (the worker checks it), available is
/// already accepted, and anything else stays failed. A model already
/// pending before its create is not created again.
#[tokio::test]
async fn a_shared_subscription_settles_the_models_it_covers_from_their_availability() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (bedrock, fake) = bedrock().await;
    let stack = stack(&url, &bedrock, true).await;
    const UPSCALE: &str = "stability.stable-creative-upscale-v1:0";
    const CORE: &str = "stability.stable-image-core-v1:1";
    const ULTRA: &str = "stability.stable-image-ultra-v1:1";
    const LATER: &str = "stability.sd3-5-large-v1:0";
    const MADE: &str = "writer.palmyra-x9-v1:0";
    const BROKEN: &str = "luma.ray-v9:0";
    {
        let mut fake = fake.lock().unwrap();
        for model in [UPSCALE, CORE, ULTRA, LATER] {
            fake.shared.insert(model.into());
        }
        fake.reject.insert(MADE.into(), Some("AVAILABLE"));
        fake.reject.insert(BROKEN.into(), None);
    }
    stack
        .agreement_required(&[UPSCALE, CORE, ULTRA, LATER, MADE, BROKEN])
        .await;
    let offer = |model: &str| default_offer(model);
    let (status, results) = stack
        .accept(
            &stack.owner,
            Some("shared-1"),
            named(
                &[
                    (UPSCALE, &offer(UPSCALE)),
                    (CORE, &offer(CORE)),
                    (ULTRA, &offer(ULTRA)),
                    (MADE, &offer(MADE)),
                    (BROKEN, &offer(BROKEN)),
                ],
                false,
            ),
        )
        .await;
    assert_eq!(status, 200, "{results}");
    for model in [UPSCALE, CORE, ULTRA] {
        let one = result(&results, model);
        assert_eq!(one["outcome"], "accepted", "{model}: {results}");
        assert_eq!(one["status"], "pending");
        assert_eq!(one["reason"], Value::Null);
    }
    assert_eq!(result(&results, MADE)["outcome"], "already_accepted");
    let broken = result(&results, BROKEN);
    assert_eq!(broken["outcome"], "failed");
    assert_eq!(broken["reason"], "provider_error");
    assert_eq!(broken["provider_code"], "validationException");
    // One create each; only the first shared model's made the agreement.
    assert_eq!(fake.lock().unwrap().creates.len(), 5);

    // The worker checks the pending ones as for any accepted agreement.
    let report = stack.check(1).await;
    assert_eq!(report.checked, 4, "{report:?}");
    let (_, view) = stack.list(&stack.owner).await;
    assert_eq!(model(&view, CORE)["agreement"]["status"], "pending");
    assert_eq!(model(&view, BROKEN)["agreement"]["status"], "failed");

    // A model the subscription already covers is not created again.
    let (status, later) = stack
        .accept(
            &stack.owner,
            Some("shared-2"),
            named(&[(LATER, &offer(LATER))], false),
        )
        .await;
    assert_eq!(status, 200, "{later}");
    assert_eq!(result(&later, LATER)["outcome"], "already_accepted");
    assert_eq!(fake.lock().unwrap().creates.len(), 5);
}
