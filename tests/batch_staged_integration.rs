//! Batches staged through the tenant's bucket (Bedrock, Gemini on Agent
//! Platform) and emulated batches (D-021, Phase 7 milestones 3 to 5), each
//! against fakes of the provider's APIs: a native job is staged, submitted
//! once, found again after a crash, collected and settled at the batch
//! price; a group too small for a native job, or a family with no batch
//! API, runs line by line at the on-demand price only where the
//! organization allows it, and never sends a line twice.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    application::trace_objects::{MemoryTraceObjects, TraceObjects},
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const PEPPER: &str = "batch-staged-test-pepper-000000000000";

/// The worker runs every organization's batches, so each test parks the
/// others' while it runs: one at a time.
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

/// A catalog whose alias `fast` runs on `model`: as a batch route when
/// `batch`, and always as an on-demand chat route, at twice the batch price.
fn manifest(family: &str, key: &str, model: &str, batch: bool) -> CatalogManifest {
    let capability = |operation: &str| {
        json!({
            "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 64,
            "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 60,
            "chat_input_count_mode": "unsupported"
        })
    };
    let price = |operation: &str, input: u32, output: u32| {
        json!({
            "deployment": "batch-dep", "operation": operation, "input_minor_per_token": input,
            "output_minor_per_token": output,
            // Batch prices have a cached-input rate, as the clouds' do: a line
            // that charges nothing must still be priced.
            "cached_input_minor_per_token": i64::from(operation.ends_with("_batch")),
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "batch-staged-test", "source_version": "v1", "confidence": "known"
        })
    };
    let mut capabilities = vec![capability("chat")];
    let mut prices = vec![price("chat", 4, 12)];
    let mut routes = vec![json!({"alias": "fast", "operation": "chat", "deployment": "batch-dep"})];
    if batch {
        capabilities.push(capability("chat_batch"));
        prices.push(price("chat_batch", 2, 6));
        routes.push(json!({"alias": "fast", "operation": "chat_batch", "deployment": "batch-dep"}));
    }
    serde_json::from_value(json!({
        "release": {"version": format!("batch-{family}-{batch}")},
        "providers": [{"adapter_instance_key": key, "family": family}],
        "deployments": [{"name": "batch-dep", "model": model, "location": null,
                         "adapter_instance_key": key,
                         "capabilities": capabilities}],
        "prices": prices,
        "routes": routes
    }))
    .unwrap()
}

async fn tenant(pool: &PgPool, manifest: &CatalogManifest) -> (Uuid, String) {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("batch-staged-{suffix}"), "Batch providers test")
        .await
        .unwrap()
        .id;
    let user = identity
        .create_user(org, None, None, None, Some("organization_owner"))
        .await
        .unwrap()
        .id;
    identity
        .add_organization_membership(org, user, MembershipRole::Owner)
        .await
        .unwrap();
    let account = identity
        .create_service_account(org, user, None, "batch-staged")
        .await
        .unwrap()
        .id;
    let bearer = identity
        .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
        .await
        .unwrap()
        .bearer;
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, scope_kind, period_start, period_end, currency, limit_minor)
         VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '2 days',
                 'USD', 1000000)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    CatalogStore::new(pool.clone())
        .apply(org, manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    (org.0, bearer)
}

fn config(url: &str, profile: &str, provider: &[(&str, String)]) -> AppConfig {
    let mut vars = vec![
        ("DATABASE_URL", url.to_owned()),
        ("GATEWAY_PROFILE", profile.to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
    ];
    vars.extend(provider.iter().cloned());
    AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap()
}

async fn consumed(pool: &PgPool, org: Uuid) -> (i64, i64) {
    sqlx::query_as(
        "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket WHERE organization_id = $1",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The gateway, its worker and a tenant whose batches go to `provider`.
struct Stack {
    pool: PgPool,
    api: String,
    worker: gateway::jobs::Worker,
    org: Uuid,
    bearer: String,
    objects: Arc<dyn TraceObjects>,
}

async fn stack(url: &str, manifest: CatalogManifest, provider: &[(&str, String)]) -> Stack {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let objects: Arc<dyn TraceObjects> = Arc::new(MemoryTraceObjects::default());
    let state = gateway::application::connect(&config(url, "api", provider))
        .await
        .unwrap()
        .with_trace_objects(Some(objects.clone()));
    let api = serve(gateway::transport::router(state)).await;
    let mut worker = gateway::application::connect_worker(&config(url, "worker", provider))
        .await
        .unwrap();
    worker.batches = worker.batches.with_objects(Some(objects.clone()));
    let (org, bearer) = tenant(&pool, &manifest).await;
    Stack {
        pool,
        api,
        worker,
        org,
        bearer,
        objects,
    }
}

impl Stack {
    /// Parks the jobs and batches other tests left in the shared database,
    /// so the worker runs only this test's.
    async fn park_others(&self) {
        sqlx::query(
            "UPDATE gateway.batch_job SET next_run_at = 'infinity'
             WHERE state <> 'finished' AND organization_id <> $1",
        )
        .bind(self.org)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE gateway.batch SET lease_token = gen_random_uuid(), lease_expires_at = 'infinity'
             WHERE status IN ('in_progress', 'finalizing', 'cancelling') AND organization_id <> $1",
        )
        .bind(self.org)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Submits a batch of `texts`, one chat line each, through the API.
    async fn submit(&self, key: &str, texts: &[(&str, u32)]) -> String {
        let lines: Vec<Value> = texts
            .iter()
            .map(|(text, max_tokens)| {
                json!({"messages": [{"role": "user", "content": text}], "max_tokens": max_tokens})
            })
            .collect();
        let (status, batch) = self.create(key, &lines).await;
        assert_eq!(status, 200, "{batch}");
        batch["id"].as_str().unwrap().to_owned()
    }

    /// Creates a batch of one chat line per body (its model `fast`); the
    /// status and the answer.
    async fn create(&self, key: &str, bodies: &[Value]) -> (u16, Value) {
        let http = reqwest::Client::new();
        let jsonl = bodies
            .iter()
            .enumerate()
            .map(|(index, body)| {
                let mut body = body.clone();
                body["model"] = json!("fast");
                json!({"custom_id": format!("line-{index}"), "method": "POST",
                       "url": "/v1/chat/completions", "body": body})
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let form = format!(
            "--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
             --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
             {jsonl}\r\n--b--\r\n"
        );
        let file: Value = http
            .post(format!("{}/api/inference/v1/files", self.api))
            .bearer_auth(&self.bearer)
            .header("content-type", "multipart/form-data; boundary=b")
            .body(form)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let response = http
            .post(format!("{}/api/inference/v1/batches", self.api))
            .bearer_auth(&self.bearer)
            .header("idempotency-key", key)
            .json(
                &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                          "completion_window": "24h"}),
            )
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }

    async fn batch(&self, id: &str) -> Value {
        reqwest::Client::new()
            .get(format!("{}/api/inference/v1/batches/{id}", self.api))
            .bearer_auth(&self.bearer)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// A worker that died after the provider took the job, before recording
    /// it: the job is pending again, with its input still recorded.
    async fn crash_after_create(&self) {
        sqlx::query(
            "UPDATE gateway.batch_job SET state = 'pending', provider_batch_id = NULL,
                 next_run_at = now() - interval '1 minute', lease_token = NULL,
                 lease_expires_at = NULL
             WHERE organization_id = $1",
        )
        .bind(self.org)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn due_now(&self) {
        sqlx::query(
            "UPDATE gateway.batch_job SET next_run_at = now() - interval '1 minute'
             WHERE organization_id = $1 AND state <> 'finished'",
        )
        .bind(self.org)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn line_charges(&self) -> Vec<(String, i64)> {
        sqlx::query_as(
            "SELECT r.state, coalesce(u.amount_minor, 0)
             FROM gateway.accounting_reservation r
             LEFT JOIN gateway.accounting_usage u ON u.reservation_id = r.id
             JOIN gateway.batch_line l ON l.reservation_id = r.id
             WHERE r.organization_id = $1 ORDER BY l.line_number",
        )
        .bind(self.org)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

impl Stack {
    async fn content(&self, file: &str) -> String {
        reqwest::Client::new()
            .get(format!(
                "{}/api/inference/v1/files/{file}/content",
                self.api
            ))
            .bearer_auth(&self.bearer)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// Whether the line's own saved result is still kept.
    async fn line_result_kept(&self, batch: &str, line: i32) -> bool {
        let job: Uuid =
            sqlx::query_scalar("SELECT id FROM gateway.batch_job WHERE organization_id = $1")
                .bind(self.org)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        self.objects
            .get(&format!(
                "batch-results/{}/{}/{job}/lines/{line}",
                self.org,
                batch.trim_start_matches("batch_")
            ))
            .await
            .unwrap()
            .is_some()
    }

    /// Allows, or forbids, emulated batches in the organization.
    async fn emulation(&self, allowed: bool) {
        sqlx::query(
            "INSERT INTO gateway.organization_batch_settings
                 (organization_id, id, emulated_allowed, emulated_concurrency)
             VALUES ($1, gen_random_uuid(), $2, 2)
             ON CONFLICT (organization_id) DO UPDATE SET emulated_allowed = EXCLUDED.emulated_allowed",
        )
        .bind(self.org)
        .bind(allowed)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Stages `connection`'s batch jobs in `bucket`.
    async fn staging(&self, connection: &str, kind: &str, bucket: &str) {
        let (region, role) = if kind == "s3" {
            (
                Some("us-east-1"),
                Some("arn:aws:iam::123456789012:role/gateway-batch"),
            )
        } else {
            (None, None)
        };
        sqlx::query(
            "INSERT INTO gateway.batch_staging
                 (organization_id, id, connection, kind, bucket, prefix, region, role_arn)
             VALUES ($1, gen_random_uuid(), $2, $3, $4, 'gw/', $5, $6)",
        )
        .bind(self.org)
        .bind(connection)
        .bind(kind)
        .bind(bucket)
        .bind(region)
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Each line's state and how far its sent mark is set.
    async fn lines(&self) -> Vec<(String, bool)> {
        sqlx::query_as(
            "SELECT state, sent_at IS NOT NULL FROM gateway.batch_line
             WHERE organization_id = $1 ORDER BY line_number",
        )
        .bind(self.org)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

/// `count` lines of `hello` held to 10 tokens, the `fail` lines' numbers
/// (from 1) asking the provider to refuse.
fn texts(count: usize, fail: &[usize]) -> Vec<(&'static str, u32)> {
    (1..=count)
        .map(|line| {
            (
                if fail.contains(&line) {
                    "fail"
                } else {
                    "hello"
                },
                10,
            )
        })
        .collect()
}

/// The refusals' codes, by line.
fn refusals(body: &Value) -> Vec<(u64, String)> {
    body["errors"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| {
            (
                error["line"].as_u64().unwrap(),
                error["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------- Bedrock

#[derive(Default)]
struct BedrockFake {
    /// S3 objects by `bucket/key`.
    objects: HashMap<String, Vec<u8>>,
    jobs: Vec<Value>,
    creates: usize,
    /// Converse requests, for lines run emulated.
    converse: Vec<Value>,
    /// Whether every S3 call was signed for S3 and carried its digest.
    s3_signed: bool,
    /// The connection's role may not create jobs: creates answer 403, as
    /// for a role without `bedrock:TagResource`.
    deny_create: bool,
    /// The jobs asked to stop, as the path named them.
    stops: Vec<String>,
}

type Bedrock = Arc<Mutex<BedrockFake>>;

async fn s3_put(
    State(fake): State<Bedrock>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    s3_seen(&fake, &headers);
    fake.lock().unwrap().objects.insert(path, body.to_vec());
    StatusCode::OK
}

async fn s3_get(
    State(fake): State<Bedrock>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Result<Vec<u8>, StatusCode> {
    s3_seen(&fake, &headers);
    fake.lock()
        .unwrap()
        .objects
        .get(&path)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

fn s3_seen(fake: &Bedrock, headers: &HeaderMap) {
    let signed = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.starts_with("AWS4-HMAC-SHA256") && value.contains("/us-east-1/s3/aws4_request")
        })
        && headers.contains_key("x-amz-content-sha256");
    let mut fake = fake.lock().unwrap();
    fake.s3_signed &= signed;
}

async fn bedrock_create(
    State(fake): State<Bedrock>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut fake = fake.lock().unwrap();
    fake.creates += 1;
    if fake.deny_create {
        return (
            StatusCode::FORBIDDEN,
            [("x-amzn-errortype", "AccessDeniedException")],
            Json(json!({"message": "User is not authorized to perform: bedrock:TagResource"})),
        )
            .into_response();
    }
    // A create repeated with its token is the same job.
    if let Some(job) = fake
        .jobs
        .iter()
        .find(|job| job["clientRequestToken"] == body["clientRequestToken"])
    {
        return Json(json!({"jobArn": job["jobArn"]})).into_response();
    }
    let arn = format!(
        "arn:aws:bedrock:us-east-1:123456789012:model-invocation-job/job{:09}",
        fake.jobs.len() + 1
    );
    let mut job = body;
    job["jobArn"] = json!(arn);
    job["status"] = json!("Submitted");
    fake.jobs.push(job);
    Json(json!({"jobArn": arn})).into_response()
}

async fn bedrock_list(
    State(fake): State<Bedrock>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let fake = fake.lock().unwrap();
    let name = query.get("nameContains").cloned().unwrap_or_default();
    let found: Vec<Value> = fake
        .jobs
        .iter()
        .filter(|job| job["jobName"].as_str().unwrap().contains(&name))
        .cloned()
        .collect();
    Json(json!({"invocationJobSummaries": found}))
}

async fn bedrock_one(
    State(fake): State<Bedrock>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let fake = fake.lock().unwrap();
    // As Bedrock: a job is named by its whole ARN, never its id alone.
    match fake.jobs.iter().find(|job| job["jobArn"] == id) {
        Some(job) => Json(job.clone()).into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            [("x-amzn-errortype", "ValidationException")],
            Json(json!({"message": "The provided ARN is invalid."})),
        )
            .into_response(),
    }
}

async fn bedrock_stop(
    State(fake): State<Bedrock>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut fake = fake.lock().unwrap();
    fake.stops.push(id.clone());
    // As Bedrock: a job is named by its whole ARN, never its id alone.
    match fake.jobs.iter_mut().find(|job| job["jobArn"] == id) {
        Some(job) => {
            job["status"] = json!("Stopped");
            Json(json!({})).into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            [("x-amzn-errortype", "ValidationException")],
            Json(json!({"message": "The provided ARN is invalid."})),
        )
            .into_response(),
    }
}

async fn bedrock_converse(
    State(fake): State<Bedrock>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut fake = fake.lock().unwrap();
    fake.converse.push(body.clone());
    if body["messages"][0]["content"][0]["text"] == "fail" {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"message": "no"}))));
    }
    Ok(Json(converse_output()))
}

fn converse_output() -> Value {
    json!({"output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
           "stopReason": "end_turn",
           "usage": {"inputTokens": 5, "outputTokens": 7, "totalTokens": 12}})
}

/// A record's `modelOutput` as Bedrock's batch output writes it: every
/// field of its response types, `null` where Converse would leave it out.
fn batch_output() -> Value {
    let block = |text: &str| {
        json!({"image": null, "searchResult": null, "citationsContent": null, "toolUse": null,
               "toolRemoval": null, "guardContent": null, "document": null, "cachePoint": null,
               "toolAddition": null, "video": null, "text": text, "audio": null,
               "toolResult": null, "reasoningContent": null})
    };
    json!({"output": {"message": {"role": "assistant", "content": [block("hi")]}},
           "stopReason": "end_turn", "trace": null, "performanceConfig": null,
           "additionalModelResponseFields": null, "metrics": null, "serviceTier": null,
           "dependencyValidations": null,
           "usage": {"serverToolUsage": {"webSearchRequests": null}, "cacheReadInputTokens": null,
                     "cacheWriteInputTokens": null, "cacheDetails": null, "inputTokens": 5,
                     "outputTokens": 7, "totalTokens": 12, "cacheWriteInputTokenCount": null,
                     "cacheReadInputTokenCount": null}})
}

/// Ends every job, writing its output where Bedrock does: under the
/// output prefix, in a folder named for the job.
fn bedrock_complete(fake: &Bedrock) {
    let mut fake = fake.lock().unwrap();
    let jobs = fake.jobs.clone();
    for (index, job) in jobs.iter().enumerate() {
        let input = job["inputDataConfig"]["s3InputDataConfig"]["s3Uri"]
            .as_str()
            .unwrap()
            .trim_start_matches("s3://")
            .to_owned();
        let output = job["outputDataConfig"]["s3OutputDataConfig"]["s3Uri"]
            .as_str()
            .unwrap()
            .trim_start_matches("s3://")
            .to_owned();
        let id = job["jobArn"].as_str().unwrap().rsplit('/').next().unwrap();
        let mut records = String::new();
        for line in String::from_utf8(fake.objects[&input].clone())
            .unwrap()
            .lines()
        {
            let record: Value = serde_json::from_str(line).unwrap();
            let result = if record["modelInput"]["messages"][0]["content"][0]["text"] == "fail" {
                json!({"recordId": record["recordId"], "modelInput": record["modelInput"],
                       "error": {"errorCode": 400, "errorMessage": "no"}})
            } else {
                json!({"recordId": record["recordId"], "modelInput": record["modelInput"],
                       "modelOutput": batch_output()})
            };
            records.push_str(&format!("{result}\n"));
        }
        fake.objects.insert(
            format!("{output}{id}/input.jsonl.out"),
            records.into_bytes(),
        );
        fake.jobs[index]["status"] = json!("Completed");
    }
}

async fn bedrock_stack(url: &str) -> (Stack, Bedrock) {
    bedrock_stack_with(url, true).await
}

/// [`bedrock_stack`], its alias with a batch route only when `batch`.
async fn bedrock_stack_with(url: &str, batch: bool) -> (Stack, Bedrock) {
    let fake: Bedrock = Arc::new(Mutex::new(BedrockFake {
        s3_signed: true,
        ..BedrockFake::default()
    }));
    let s3 = serve(
        Router::new()
            .route("/{*path}", axum::routing::put(s3_put).get(s3_get))
            .with_state(fake.clone()),
    )
    .await;
    let bedrock = serve(
        Router::new()
            .route("/model-invocation-job", post(bedrock_create))
            .route("/model-invocation-jobs", get(bedrock_list))
            .route("/model-invocation-job/{id}", get(bedrock_one))
            .route("/model-invocation-job/{id}/stop", post(bedrock_stop))
            .route("/model/{model}/converse", post(bedrock_converse))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        url,
        manifest(
            "bedrock",
            "bedrock-main",
            "anthropic.claude-test-v1:0",
            batch,
        ),
        &[
            ("BEDROCK_AWS_REGION", "us-east-1".to_owned()),
            ("BEDROCK_AWS_ACCESS_KEY_ID", "AKIDEXAMPLE".to_owned()),
            ("BEDROCK_AWS_SECRET_ACCESS_KEY", "bedrock-secret".to_owned()),
            ("BEDROCK_BASE_URL", bedrock.clone()),
            ("BEDROCK_CONTROL_BASE_URL", bedrock),
            ("BEDROCK_S3_BASE_URL", s3),
        ],
    )
    .await;
    (stack, fake)
}

#[tokio::test]
async fn bedrock_batches_are_staged_in_the_tenants_bucket_and_run_once() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (stack, fake) = bedrock_stack(&url).await;
    stack.park_others().await;

    // No staging bucket: no native job, and emulation is off.
    let lines = texts(100, &[2]);
    let bodies: Vec<Value> = lines
        .iter()
        .map(|(text, max)| json!({"messages": [{"role": "user", "content": text}], "max_tokens": max}))
        .collect();
    let (status, refused) = stack.create("bedrock-unstaged", &bodies).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(
        refusals(&refused)[0],
        (1, "batch_staging_missing".to_owned())
    );
    stack.staging("bedrock-main", "s3", "tenant-batches").await;

    // Fewer lines than Bedrock's smallest job, and emulation is off.
    let (status, refused) = stack.create("bedrock-small", &bodies[..3]).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(
        refusals(&refused),
        (1..=3)
            .map(|line| (line, "batch_too_small".to_owned()))
            .collect::<Vec<_>>()
    );
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));

    let batch = stack.submit("bedrock-batch", &lines).await;
    let runner = &stack.worker.batches;
    let mut now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    let job: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.batch_job WHERE organization_id = $1")
            .bind(stack.org)
            .fetch_one(&stack.pool)
            .await
            .unwrap();
    let folder = format!(
        "gw/gateway-batches/{}/{}/",
        batch.trim_start_matches("batch_"),
        job
    );
    {
        let fake = fake.lock().unwrap();
        assert_eq!(fake.creates, 1);
        assert!(fake.s3_signed);
        let created = &fake.jobs[0];
        assert_eq!(created["modelId"], "anthropic.claude-test-v1:0");
        assert_eq!(created["modelInvocationType"], "Converse");
        assert_eq!(
            created["roleArn"],
            "arn:aws:iam::123456789012:role/gateway-batch"
        );
        assert_eq!(created["jobName"], format!("gateway-{}", job.simple()));
        assert_eq!(created["clientRequestToken"], job.simple().to_string());
        assert_eq!(
            created["inputDataConfig"]["s3InputDataConfig"]["s3Uri"],
            format!("s3://tenant-batches/{folder}input.jsonl")
        );
        assert_eq!(
            created["outputDataConfig"]["s3OutputDataConfig"]["s3Uri"],
            format!("s3://tenant-batches/{folder}output/")
        );
        // Each record is a Converse body named by its line, held to its cap.
        let input =
            String::from_utf8(fake.objects[&format!("tenant-batches/{folder}input.jsonl")].clone())
                .unwrap();
        let records: Vec<Value> = input
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 100);
        assert_eq!(records[0]["recordId"], "L0000000001");
        assert_eq!(records[99]["recordId"], "L0000000100");
        assert_eq!(records[0]["modelInput"]["inferenceConfig"]["maxTokens"], 10);
        assert_eq!(
            records[0]["modelInput"]["messages"][0]["content"][0]["text"],
            "hello"
        );
    }

    // The connection's staging moves to another bucket and is then
    // deleted: the job, sent through the first, is still found and
    // collected through it.
    sqlx::query(
        "UPDATE gateway.batch_staging SET bucket = 'moved-batches', prefix = 'other/',
                deleted_at = now()
         WHERE organization_id = $1",
    )
    .bind(stack.org)
    .execute(&stack.pool)
    .await
    .unwrap();

    // Found by its name after a crash, not created again.
    stack.crash_after_create().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);

    // Collected and settled at the batch price: 5 x 2 + 7 x 6 = 52 a line,
    // the refused line at zero.
    bedrock_complete(&fake);
    now += chrono::Duration::minutes(2);
    stack.due_now().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 99 * 52));
    let charges = stack.line_charges().await;
    assert_eq!(charges[1], ("settled".to_owned(), 0));
    assert!(
        charges
            .iter()
            .enumerate()
            .all(|(index, charge)| index == 1 || *charge == ("settled".to_owned(), 52))
    );
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "completed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 100, "completed": 99, "failed": 1})
    );
    assert_eq!(ended["gateway"]["jobs"][0]["mode"], "native");
    assert_eq!(ended["gateway"]["jobs"][0]["price_source"], "batch");
    assert_eq!(
        ended["gateway"]["cost"],
        json!([{"currency": "USD", "consumed_minor": 99 * 52, "reserved_minor": 0}])
    );
    assert!(fake.lock().unwrap().converse.is_empty());
}

#[tokio::test]
async fn a_cancelled_bedrock_batch_stops_its_job_by_arn_and_charges_nothing() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (stack, fake) = bedrock_stack(&url).await;
    stack.park_others().await;
    stack.staging("bedrock-main", "s3", "tenant-batches").await;
    let batch = stack.submit("bedrock-cancel", &texts(100, &[])).await;
    let runner = &stack.worker.batches;
    let now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    let response = reqwest::Client::new()
        .post(format!(
            "{}/api/inference/v1/batches/{batch}/cancel",
            stack.api
        ))
        .bearer_auth(&stack.bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // The running job is stopped by its whole ARN; Bedrock wrote no
    // output, so every line ends cancelled at zero.
    stack.due_now().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    {
        let fake = fake.lock().unwrap();
        assert_eq!(fake.stops, [fake.jobs[0]["jobArn"].as_str().unwrap()]);
        assert_eq!(fake.jobs[0]["status"], "Stopped");
    }
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "cancelled");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 100, "completed": 0, "failed": 100})
    );
    assert_eq!(
        ended["gateway"]["cost"],
        json!([{"currency": "USD", "consumed_minor": 0, "reserved_minor": 0}])
    );
}

#[tokio::test]
async fn small_bedrock_groups_run_emulated_at_the_on_demand_price_where_allowed() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (stack, fake) = bedrock_stack(&url).await;
    stack.park_others().await;
    stack.staging("bedrock-main", "s3", "tenant-batches").await;
    stack.emulation(true).await;

    let batch = stack.submit("bedrock-emulated", &texts(3, &[2])).await;
    let runner = &stack.worker.batches;
    let now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.finalized, 1, "{report:?}");
    {
        let fake = fake.lock().unwrap();
        // No Bedrock job; each line sent once as a Converse request.
        assert_eq!(fake.creates, 0);
        assert!(fake.objects.is_empty());
        assert_eq!(fake.converse.len(), 3);
        assert!(
            fake.converse
                .iter()
                .all(|body| body["inferenceConfig"]["maxTokens"] == 10)
        );
    }
    // On-demand: 5 x 4 + 7 x 12 = 104 a line. The line Bedrock refused
    // goes to reconciliation, as an ordinary request's failure does, and
    // keeps its hold until it is resolved.
    assert_eq!(
        stack.line_charges().await,
        [
            ("settled".to_owned(), 104),
            ("unknown".to_owned(), 0),
            ("settled".to_owned(), 104)
        ]
    );
    let (held, charged) = consumed(&stack.pool, stack.org).await;
    assert!(held > 0);
    assert_eq!(charged, 208);
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "completed", "{ended}");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 1})
    );
    assert_eq!(ended["gateway"]["jobs"][0]["mode"], "emulated");
    assert_eq!(ended["gateway"]["jobs"][0]["price_source"], "on_demand");
    // The unknown line's hold is still reported as held, as the budget
    // still holds it.
    assert_eq!(
        ended["gateway"]["cost"],
        json!([{"currency": "USD", "consumed_minor": 208, "reserved_minor": held}])
    );
    // The output holds the answers in the chat shape, under the alias.
    let output = stack
        .content(ended["output_file_id"].as_str().unwrap())
        .await;
    let first: Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
    assert_eq!(first["custom_id"], "line-0");
    assert_eq!(first["response"]["status_code"], 200);
    assert_eq!(first["response"]["body"]["model"], "fast");
    assert_eq!(
        first["response"]["body"]["choices"][0]["message"]["content"],
        "hi"
    );
    // Every line was sent, and no line's own result is left behind.
    assert!(stack.lines().await.iter().all(|(_, sent)| *sent));
    for line in 1..=3 {
        assert!(!stack.line_result_kept(&batch, line).await);
    }
}

// ---------------------------------------------------------------- Gemini

const JOBS: &str = "/v1/projects/proj-test/locations/us-central1/batchPredictionJobs";

#[derive(Default)]
struct GeminiFake {
    /// Cloud Storage objects by `bucket/name`.
    objects: HashMap<String, Vec<u8>>,
    jobs: Vec<Value>,
    creates: usize,
    /// Whether every call carried the token.
    authorized: bool,
}

type Gemini = Arc<Mutex<GeminiFake>>;

fn gemini_seen(fake: &Gemini, headers: &HeaderMap) {
    let authorized = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some("Bearer gcp-token");
    fake.lock().unwrap().authorized &= authorized;
}

async fn google_token() -> Json<Value> {
    Json(json!({"access_token": "gcp-token", "expires_in": 3600, "token_type": "Bearer"}))
}

async fn gcs_upload(
    State(fake): State<Gemini>,
    Path(bucket): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    gemini_seen(&fake, &headers);
    assert_eq!(query["uploadType"], "media");
    let name = query["name"].clone();
    fake.lock()
        .unwrap()
        .objects
        .insert(format!("{bucket}/{name}"), body.to_vec());
    Json(json!({"bucket": bucket, "name": name}))
}

async fn gcs_list(
    State(fake): State<Gemini>,
    Path(bucket): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    gemini_seen(&fake, &headers);
    let prefix = format!("{bucket}/{}", query["prefix"]);
    let fake = fake.lock().unwrap();
    let items: Vec<Value> = fake
        .objects
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .map(|key| json!({"name": key.split_once('/').unwrap().1}))
        .collect();
    Json(json!({"kind": "storage#objects", "items": items}))
}

async fn gcs_object(
    State(fake): State<Gemini>,
    Path((bucket, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Vec<u8>, StatusCode> {
    gemini_seen(&fake, &headers);
    fake.lock()
        .unwrap()
        .objects
        .get(&format!("{bucket}/{name}"))
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

async fn vertex_create(
    State(fake): State<Gemini>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    gemini_seen(&fake, &headers);
    let mut fake = fake.lock().unwrap();
    fake.creates += 1;
    let mut job = body;
    // Google names the project by its number.
    job["name"] = json!(format!(
        "projects/123456789/locations/us-central1/batchPredictionJobs/{}",
        9000 + fake.jobs.len()
    ));
    job["state"] = json!("JOB_STATE_PENDING");
    fake.jobs.push(job.clone());
    Json(job)
}

async fn vertex_list(
    State(fake): State<Gemini>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    gemini_seen(&fake, &headers);
    let fake = fake.lock().unwrap();
    let wanted = query["filter"]
        .strip_prefix("labels.gateway_job=\"")
        .unwrap()
        .trim_end_matches('"')
        .to_owned();
    let found: Vec<Value> = fake
        .jobs
        .iter()
        .filter(|job| job["labels"]["gateway_job"] == wanted)
        .cloned()
        .collect();
    Json(json!({"batchPredictionJobs": found}))
}

async fn vertex_one(
    State(fake): State<Gemini>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Json<Value> {
    gemini_seen(&fake, &headers);
    let fake = fake.lock().unwrap();
    Json(
        fake.jobs
            .iter()
            .find(|job| job["name"].as_str().unwrap().ends_with(&format!("/{id}")))
            .unwrap()
            .clone(),
    )
}

/// Ends every job, writing its predictions in a folder under the output
/// prefix, as Agent Platform does.
fn vertex_complete(fake: &Gemini) {
    let mut fake = fake.lock().unwrap();
    let jobs = fake.jobs.clone();
    for (index, job) in jobs.iter().enumerate() {
        let input = job["inputConfig"]["gcsSource"]["uris"][0]
            .as_str()
            .unwrap()
            .trim_start_matches("gs://")
            .to_owned();
        let prefix = job["outputConfig"]["gcsDestination"]["outputUriPrefix"]
            .as_str()
            .unwrap()
            .to_owned();
        let directory = format!("{prefix}/prediction-model-2026-11-17T00:00:00.000000Z");
        let mut predictions = String::new();
        for line in String::from_utf8(fake.objects[&input].clone())
            .unwrap()
            .lines()
        {
            let instance: Value = serde_json::from_str(line).unwrap();
            let request = &instance["request"];
            // As Vertex does: a label value that reads as a number is taken
            // for one, and the request is refused.
            let numeric = request["labels"]
                .as_object()
                .into_iter()
                .flat_map(|labels| labels.values())
                .any(|value| value.as_str().is_some_and(|v| v.parse::<f64>().is_ok()));
            let prediction = if numeric {
                json!({"status": "Failed to parse JSON into proto google.cloud.aiplatform.v1.GenerateContentRequest: LabelsEntry",
                       "processed_time": "2026-11-17T00:00:00Z", "request": request})
            } else if request["contents"][0]["parts"][0]["text"] == "fail" {
                json!({"status": "Bad Request: no", "processed_time": "2026-11-17T00:00:00Z",
                       "request": request, "response": {}})
            } else {
                json!({"status": "", "processed_time": "2026-11-17T00:00:00Z",
                       "request": request, "response": {
                    "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]},
                                    "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 7,
                                      "totalTokenCount": 12},
                    "modelVersion": "gemini-test", "responseId": "resp-1"}})
            };
            predictions.push_str(&format!("{prediction}\n"));
        }
        let folder = directory.trim_start_matches("gs://").to_owned();
        fake.objects.insert(
            format!("{folder}/predictions.jsonl"),
            predictions.into_bytes(),
        );
        // Not a prediction file: never read.
        fake.objects
            .insert(format!("{folder}/errors_stats.txt"), b"none".to_vec());
        fake.jobs[index]["state"] = json!("JOB_STATE_SUCCEEDED");
        fake.jobs[index]["outputInfo"] = json!({"gcsOutputDirectory": directory});
    }
}

#[tokio::test]
async fn gemini_batches_are_staged_in_the_tenants_cloud_storage_and_run_once() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: Gemini = Arc::new(Mutex::new(GeminiFake {
        authorized: true,
        ..GeminiFake::default()
    }));
    let google = serve(
        Router::new()
            .route("/token", post(google_token))
            .route("/upload/storage/v1/b/{bucket}/o", post(gcs_upload))
            .route("/storage/v1/b/{bucket}/o", get(gcs_list))
            .route("/storage/v1/b/{bucket}/o/{name}", get(gcs_object))
            .route(JOBS, post(vertex_create).get(vertex_list))
            .route(&format!("{JOBS}/{{id}}"), get(vertex_one))
            .with_state(fake.clone()),
    )
    .await;
    let credentials =
        std::env::temp_dir().join(format!("batch-staged-adc-{}.json", Uuid::new_v4()));
    std::fs::write(
        &credentials,
        json!({"type": "authorized_user", "client_id": "c", "client_secret": "s",
               "refresh_token": "r", "token_uri": format!("{google}/token")})
        .to_string(),
    )
    .unwrap();
    let stack = stack(
        &url,
        manifest("gemini", "gemini-main", "gemini-test", true),
        &[
            ("GEMINI_AGENT_PLATFORM_PROJECT", "proj-test".to_owned()),
            ("GEMINI_AGENT_PLATFORM_LOCATION", "us-central1".to_owned()),
            (
                "GEMINI_BASE_URL",
                format!("{google}/v1/projects/proj-test/locations/us-central1/publishers/google"),
            ),
            ("GEMINI_STORAGE_BASE_URL", google.clone()),
            (
                "GOOGLE_APPLICATION_CREDENTIALS",
                credentials.to_string_lossy().into_owned(),
            ),
        ],
    )
    .await;
    stack.park_others().await;
    stack.staging("gemini-main", "gcs", "tenant-batches").await;

    let batch = stack
        .submit(
            "gemini-batch",
            &[("hello", 10), ("fail", 10), ("hello", 10)],
        )
        .await;
    let runner = &stack.worker.batches;
    let mut now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    let job: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.batch_job WHERE organization_id = $1")
            .bind(stack.org)
            .fetch_one(&stack.pool)
            .await
            .unwrap();
    let folder = format!(
        "gw/gateway-batches/{}/{job}/",
        batch.trim_start_matches("batch_")
    );
    {
        let fake = fake.lock().unwrap();
        assert!(fake.authorized);
        assert_eq!(fake.creates, 1);
        let created = &fake.jobs[0];
        assert_eq!(created["model"], "publishers/google/models/gemini-test");
        assert_eq!(created["displayName"], format!("gateway-{}", job.simple()));
        assert_eq!(created["labels"]["gateway_job"], job.to_string());
        assert_eq!(
            created["inputConfig"],
            json!({"instancesFormat": "jsonl",
                   "gcsSource": {"uris": [format!("gs://tenant-batches/{folder}input.jsonl")]}})
        );
        assert_eq!(
            created["outputConfig"]["gcsDestination"]["outputUriPrefix"],
            format!("gs://tenant-batches/{folder}output")
        );
        let input =
            String::from_utf8(fake.objects[&format!("tenant-batches/{folder}input.jsonl")].clone())
                .unwrap();
        let instances: Vec<Value> = input
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(instances.len(), 3);
        assert_eq!(instances[0]["request"]["labels"]["gateway_line"], "line-1");
        assert_eq!(
            instances[0]["request"]["generationConfig"]["maxOutputTokens"],
            10
        );
    }

    // Found by its labels after a crash, not created again.
    stack.crash_after_create().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);

    vertex_complete(&fake);
    now += chrono::Duration::minutes(2);
    stack.due_now().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 104));
    assert_eq!(
        stack.line_charges().await,
        [
            ("settled".to_owned(), 52),
            ("settled".to_owned(), 0),
            ("settled".to_owned(), 52)
        ]
    );
    // The line Vertex refused is failed, and its request says so: settled
    // at zero with the provider's error, not succeeded with no usage.
    let states: Vec<String> = stack
        .lines()
        .await
        .into_iter()
        .map(|(state, _)| state)
        .collect();
    assert_eq!(states, ["succeeded", "failed", "succeeded"]);
    let traces: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT t.outcome, t.provider_error_category FROM gateway.batch_line l
         JOIN gateway.inference_trace t ON t.reservation_id = l.reservation_id
         WHERE l.organization_id = $1 ORDER BY l.line_number",
    )
    .bind(stack.org)
    .fetch_all(&stack.pool)
    .await
    .unwrap();
    assert_eq!(traces[1].0, "settled");
    assert!(traces[1].1.is_some(), "{traces:?}");
    assert!(traces[0].1.is_none() && traces[2].1.is_none(), "{traces:?}");
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "completed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 1})
    );
    let output = stack
        .content(ended["output_file_id"].as_str().unwrap())
        .await;
    let first: Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
    assert_eq!(
        first["response"]["body"]["choices"][0]["message"]["content"],
        "hi"
    );
    std::fs::remove_file(credentials).ok();
}

// ---------------------------------------------------------------- Emulated

#[derive(Default)]
struct OpenAiFake {
    /// Each chat request's first message.
    sent: Vec<String>,
}

type OpenAi = Arc<Mutex<OpenAiFake>>;

async fn openai_chat(State(fake): State<OpenAi>, Json(body): Json<Value>) -> Json<Value> {
    let text = body["messages"][0]["content"].as_str().unwrap().to_owned();
    fake.lock().unwrap().sent.push(text);
    Json(json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 0, "model": body["model"],
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "hi"}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
    }))
}

/// What happens while an emulated job's first group is with the provider.
#[derive(Clone, Copy)]
enum Meanwhile {
    /// The batch is cancelled through the inference API.
    Cancel,
    /// The routing policy comes to forbid the job's provider.
    Forbid,
}

type Acting = Arc<Mutex<Option<(Meanwhile, String, String, String, PgPool, Uuid)>>>;

/// [`openai_chat`], doing `Meanwhile` once, on the first request.
async fn openai_acting(
    State((fake, acting)): State<(OpenAi, Acting)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let action = acting.lock().unwrap().take();
    if let Some((meanwhile, api, bearer, batch, pool, org)) = action {
        match meanwhile {
            Meanwhile::Cancel => {
                let response = reqwest::Client::new()
                    .post(format!("{api}/api/inference/v1/batches/{batch}/cancel"))
                    .bearer_auth(bearer)
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
            }
            Meanwhile::Forbid => {
                sqlx::query(
                    "INSERT INTO gateway.routing_policy (organization_id, id, providers)
                     VALUES ($1, gen_random_uuid(), ARRAY['bedrock'])",
                )
                .bind(org)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
    }
    openai_chat(State(fake), Json(body)).await
}

/// A four-line emulated batch, two lines at once, with `meanwhile` done
/// while the first two are in flight: the lines and the batch after one run.
async fn emulated_meanwhile(url: &str, meanwhile: Meanwhile) -> (Stack, OpenAi, Value) {
    let fake: OpenAi = Arc::default();
    let acting: Acting = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai_acting))
            .with_state((fake.clone(), acting.clone())),
    )
    .await;
    let stack = stack(
        url,
        manifest("openai", "openai-main", "gpt-test", false),
        &[
            ("OPENAI_BASE_URL", format!("{provider}/v1")),
            ("OPENAI_API_KEY", "openai-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    stack.emulation(true).await;
    let batch = stack
        .submit(
            "emulated-meanwhile",
            &[("one", 10), ("two", 10), ("three", 10), ("four", 10)],
        )
        .await;
    *acting.lock().unwrap() = Some((
        meanwhile,
        stack.api.clone(),
        stack.bearer.clone(),
        batch.clone(),
        stack.pool.clone(),
        stack.org,
    ));
    stack
        .worker
        .batches
        .run_once(chrono::Utc::now())
        .await
        .unwrap();
    let ended = stack.batch(&batch).await;
    (stack, fake, ended)
}

#[tokio::test]
async fn an_emulated_batch_cancelled_mid_run_sends_no_further_group() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (stack, fake, ended) = emulated_meanwhile(&url, Meanwhile::Cancel).await;
    // The group in flight finishes; the cancellation is seen before the
    // next one, though the job was leased before it.
    assert_eq!(fake.lock().unwrap().sent.len(), 2);
    assert_eq!(
        stack.line_charges().await,
        [
            ("settled".to_owned(), 104),
            ("settled".to_owned(), 104),
            ("settled".to_owned(), 0),
            ("settled".to_owned(), 0)
        ]
    );
    let states: Vec<String> = stack
        .lines()
        .await
        .into_iter()
        .map(|(state, _)| state)
        .collect();
    assert_eq!(states, ["succeeded", "succeeded", "cancelled", "cancelled"]);
    assert_eq!(ended["status"], "cancelled", "{ended}");
}

#[tokio::test]
async fn an_emulated_batch_the_routing_policy_comes_to_forbid_stops_sending() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (stack, fake, ended) = emulated_meanwhile(&url, Meanwhile::Forbid).await;
    assert_eq!(fake.lock().unwrap().sent.len(), 2);
    // The lines already sent keep their results; the rest are denied at no
    // charge.
    let lines: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT state, error_code FROM gateway.batch_line WHERE organization_id = $1
         ORDER BY line_number",
    )
    .bind(stack.org)
    .fetch_all(&stack.pool)
    .await
    .unwrap();
    assert_eq!(
        lines,
        [
            ("succeeded".to_owned(), None),
            ("succeeded".to_owned(), None),
            (
                "failed".to_owned(),
                Some("routing_policy_denied".to_owned())
            ),
            (
                "failed".to_owned(),
                Some("routing_policy_denied".to_owned())
            )
        ]
    );
    assert_eq!(
        stack
            .line_charges()
            .await
            .iter()
            .map(|(_, amount)| *amount)
            .collect::<Vec<_>>(),
        [104, 104, 0, 0]
    );
    assert_eq!(ended["status"], "completed", "{ended}");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 4, "completed": 2, "failed": 2})
    );
    sqlx::query("DELETE FROM gateway.routing_policy WHERE organization_id = $1")
        .bind(stack.org)
        .execute(&stack.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_emulated_group_is_counted_under_the_rate_limit_lock() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: OpenAi = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai_chat))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        &url,
        manifest("openai", "openai-main", "gpt-test", false),
        &[
            ("OPENAI_BASE_URL", format!("{provider}/v1")),
            ("OPENAI_API_KEY", "openai-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    stack.emulation(true).await;
    // Three a minute: the two batches take two, leaving room for one line.
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy (id, organization_id, scope_kind, requests_per_minute)
         VALUES (gen_random_uuid(), $1, 'organization', 3)",
    )
    .bind(stack.org)
    .execute(&stack.pool)
    .await
    .unwrap();
    let first = stack.submit("locked-a", &[("one", 10)]).await;
    stack.submit("locked-b", &[("two", 10)]).await;
    let store = gateway::persistence::batches::BatchStore::new(stack.pool.clone());
    let jobs = store
        .lease_jobs(chrono::Utc::now(), chrono::Duration::minutes(10), 10)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2);
    let (mine, other): (Vec<_>, Vec<_>) = jobs
        .into_iter()
        .partition(|job| format!("batch_{}", job.batch_id) == first);
    let (mine, other) = (mine[0].clone(), other[0].clone());

    // Another worker holds the scope's lock and takes the last room for
    // the other job's line: this one waits for it, then sees no room.
    let mut rival = stack.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway.lock_rate_limit_policies($1, NULL, $2)")
        .bind(stack.org)
        .bind(mine.service_account_id)
        .fetch_all(&mut *rival)
        .await
        .unwrap();
    let waiting = tokio::spawn({
        let store = store.clone();
        async move { store.send_group(&mine, 2).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !waiting.is_finished(),
        "the group waits for the scope's lock"
    );
    sqlx::query(
        "UPDATE gateway.batch_line SET sent_at = clock_timestamp()
         WHERE organization_id = $1 AND job_id = $2",
    )
    .bind(stack.org)
    .bind(other.id)
    .execute(&mut *rival)
    .await
    .unwrap();
    rival.commit().await.unwrap();
    assert_eq!(
        waiting.await.unwrap(),
        gateway::persistence::batches::SendGroup::NoRoom
    );
    sqlx::query("DELETE FROM gateway.rate_limit_policy WHERE organization_id = $1")
        .bind(stack.org)
        .execute(&stack.pool)
        .await
        .unwrap();
}

/// An emulated job's lines count against the policy of the key its batch
/// was created with, as that key's requests do.
#[tokio::test]
async fn an_emulated_group_is_limited_by_its_keys_own_policy() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: OpenAi = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai_chat))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        &url,
        manifest("openai", "openai-main", "gpt-test", false),
        &[
            ("OPENAI_BASE_URL", format!("{provider}/v1")),
            ("OPENAI_API_KEY", "openai-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    stack.emulation(true).await;
    let first = stack.submit("keyed-a", &[("one", 10), ("two", 10)]).await;
    let store = gateway::persistence::batches::BatchStore::new(stack.pool.clone());
    let jobs = store
        .lease_jobs(chrono::Utc::now(), chrono::Duration::minutes(10), 10)
        .await
        .unwrap();
    let job = jobs
        .into_iter()
        .find(|job| format!("batch_{}", job.batch_id) == first)
        .unwrap();
    let key = job.api_key_id.expect("the batch names its key");
    // Two a minute for the key: its batch took one, leaving one line.
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, api_key_id, requests_per_minute)
         VALUES (gen_random_uuid(), $1, 'api-key', $2, 2)",
    )
    .bind(stack.org)
    .bind(key)
    .execute(&stack.pool)
    .await
    .unwrap();
    assert_eq!(
        store.send_group(&job, 2).await.unwrap(),
        gateway::persistence::batches::SendGroup::Sent(vec![1])
    );
    assert_eq!(
        store.send_group(&job, 2).await.unwrap(),
        gateway::persistence::batches::SendGroup::NoRoom
    );
    sqlx::query("DELETE FROM gateway.rate_limit_policy WHERE organization_id = $1")
        .bind(stack.org)
        .execute(&stack.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_emulated_line_its_adapter_would_refuse_refuses_the_batch() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // No batch route: every line runs emulated, as a Converse request.
    let (stack, fake) = bedrock_stack_with(&url, false).await;
    stack.park_others().await;
    stack.emulation(true).await;
    let bodies = [
        json!({"messages": [{"role": "user", "content": "hello"}], "max_tokens": 10}),
        // Converse takes no developer turn: Bedrock's adapter refuses it.
        json!({"messages": [{"role": "developer", "content": "be brief"},
                            {"role": "user", "content": "hello"}], "max_tokens": 10}),
    ];
    let (status, refused) = stack.create("emulated-refused", &bodies).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refusals(&refused), [(2, "invalid_body".to_owned())]);
    // Nothing held, nothing sent.
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));
    assert!(fake.lock().unwrap().converse.is_empty());
    // The line its adapter takes alone is accepted.
    let (status, batch) = stack.create("emulated-taken", &bodies[..1]).await;
    assert_eq!(status, 200, "{batch}");
}

#[tokio::test]
async fn emulated_batches_send_each_line_once_within_the_rate_limits() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: OpenAi = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai_chat))
            .with_state(fake.clone()),
    )
    .await;
    // The alias has no batch route.
    let stack = stack(
        &url,
        manifest("openai", "openai-main", "gpt-test", false),
        &[
            ("OPENAI_BASE_URL", format!("{provider}/v1")),
            ("OPENAI_API_KEY", "openai-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    let lines = [("one", 10), ("two", 10), ("three", 10)];

    // Off by default: refused, nothing held.
    let bodies: Vec<Value> = lines
        .iter()
        .map(|(text, max)| json!({"messages": [{"role": "user", "content": text}], "max_tokens": max}))
        .collect();
    let (status, refused) = stack.create("emulated-off", &bodies).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refusals(&refused)[0], (1, "model_unavailable".to_owned()));
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));

    stack.emulation(true).await;
    // Three requests a minute: the batch takes one, so two lines go now.
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy (id, organization_id, scope_kind, requests_per_minute)
         VALUES (gen_random_uuid(), $1, 'organization', 3)",
    )
    .bind(stack.org)
    .execute(&stack.pool)
    .await
    .unwrap();
    let batch = stack.submit("emulated-on", &lines).await;
    let runner = &stack.worker.batches;
    let now = chrono::Utc::now();
    runner.run_once(now).await.unwrap();
    assert_eq!(fake.lock().unwrap().sent, ["one", "two"]);
    assert_eq!(
        stack.lines().await,
        [
            ("succeeded".to_owned(), true),
            ("succeeded".to_owned(), true),
            ("pending".to_owned(), false)
        ]
    );
    let running = stack.batch(&batch).await;
    assert_eq!(running["status"], "in_progress");
    // Counted as the lines end, not only when the batch does.
    assert_eq!(
        running["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 0})
    );
    // Emulated lines name their batch as native ones do (D-021).
    let unnamed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.batch_line l
         JOIN gateway.accounting_reservation r ON r.id = l.reservation_id
         WHERE l.organization_id = $1 AND r.batch_id IS DISTINCT FROM l.batch_id",
    )
    .bind(stack.org)
    .fetch_one(&stack.pool)
    .await
    .unwrap();
    assert_eq!(unnamed, 0);

    // A worker that died with the last line in flight: it is never sent
    // again, and, with no result saved, goes to reconciliation.
    sqlx::query(
        "UPDATE gateway.batch_line SET sent_at = now() WHERE organization_id = $1 AND line_number = 3",
    )
    .bind(stack.org)
    .execute(&stack.pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM gateway.rate_limit_policy WHERE organization_id = $1")
        .bind(stack.org)
        .execute(&stack.pool)
        .await
        .unwrap();
    stack.due_now().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.finalized, 1, "{report:?}");
    assert_eq!(fake.lock().unwrap().sent, ["one", "two"]);
    assert_eq!(
        stack.line_charges().await,
        [
            ("settled".to_owned(), 104),
            ("settled".to_owned(), 104),
            ("unknown".to_owned(), 0)
        ]
    );
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "completed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 1})
    );
    assert_eq!(ended["gateway"]["jobs"][0]["price_source"], "on_demand");
    let errors = stack
        .content(ended["error_file_id"].as_str().unwrap())
        .await;
    let error: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(error["custom_id"], "line-2");
}

#[tokio::test]
async fn a_cancelled_emulated_batch_sends_nothing_more_and_charges_nothing_unsent() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: OpenAi = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai_chat))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        &url,
        manifest("openai", "openai-main", "gpt-test", false),
        &[
            ("OPENAI_BASE_URL", format!("{provider}/v1")),
            ("OPENAI_API_KEY", "openai-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    stack.emulation(true).await;
    let batch = stack
        .submit("emulated-cancel", &[("one", 10), ("two", 10)])
        .await;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/api/inference/v1/batches/{batch}/cancel",
            stack.api
        ))
        .bearer_auth(&stack.bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let runner = &stack.worker.batches;
    let now = chrono::Utc::now();
    runner.run_once(now).await.unwrap();
    stack.due_now().await;
    runner.run_once(now).await.unwrap();
    assert!(fake.lock().unwrap().sent.is_empty());
    assert_eq!(
        stack.line_charges().await,
        [("settled".to_owned(), 0), ("settled".to_owned(), 0)]
    );
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "cancelled", "{ended}");
}

/// Whether a provider is reachable is no matter for creating a batch: an
/// emulated line whose connection cannot be opened now (here a stored
/// credential that does not open, as an endpoint that does not resolve) is
/// accepted, and checked when the worker sends it, rather than the whole
/// batch refused with `provider_unavailable`.
#[tokio::test]
async fn an_emulated_batch_is_created_while_its_provider_is_unreachable() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: OpenAi = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai_chat))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        &url,
        manifest("openai", "openai-main", "gpt-test", false),
        &[
            ("OPENAI_BASE_URL", format!("{provider}/v1")),
            ("OPENAI_API_KEY", "openai-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    stack.emulation(true).await;
    sqlx::query(
        "INSERT INTO gateway.provider_credential
             (id, organization_id, adapter_instance_key, family, key_id, ciphertext)
         VALUES (gen_random_uuid(), $1, 'openai-main', 'openai', '00000000000000aa',
                 decode(repeat('00', 40), 'hex'))",
    )
    .bind(stack.org)
    .execute(&stack.pool)
    .await
    .unwrap();
    let bodies: Vec<Value> = ["one", "two"]
        .iter()
        .map(|text| json!({"messages": [{"role": "user", "content": text}], "max_tokens": 10}))
        .collect();
    let (status, created) = stack.create("unreachable", &bodies).await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["status"], "in_progress");
    assert!(fake.lock().unwrap().sent.is_empty());
}

/// A connection whose role may not create Bedrock jobs (the gate's missing
/// `bedrock:TagResource`): the job ends at once, every line failed at zero
/// with `provider_permission_denied`, rather than retried for the window.
#[tokio::test]
async fn a_bedrock_job_the_role_may_not_create_fails_at_once() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let (stack, fake) = bedrock_stack(&url).await;
    stack.park_others().await;
    stack.staging("bedrock-main", "s3", "tenant-batches").await;
    fake.lock().unwrap().deny_create = true;
    let batch = stack.submit("bedrock-denied", &texts(100, &[])).await;
    let runner = &stack.worker.batches;
    let now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.retried, report.finalized), (0, 1), "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);
    let (state, error): (String, Option<String>) = sqlx::query_as(
        "SELECT state, error_code FROM gateway.batch_job WHERE organization_id = $1",
    )
    .bind(stack.org)
    .fetch_one(&stack.pool)
    .await
    .unwrap();
    assert_eq!(state, "finished");
    assert_eq!(error.as_deref(), Some("provider_permission_denied"));
    let codes: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT state, error_code FROM gateway.batch_line WHERE organization_id = $1",
    )
    .bind(stack.org)
    .fetch_all(&stack.pool)
    .await
    .unwrap();
    assert_eq!(
        codes,
        [(
            "failed".to_owned(),
            Some("provider_permission_denied".to_owned())
        )]
    );
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "failed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 100, "completed": 0, "failed": 100})
    );
    let errors = stack
        .content(ended["error_file_id"].as_str().unwrap())
        .await;
    let first: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(
        first["error"]["code"], "provider_permission_denied",
        "{first}"
    );
}
