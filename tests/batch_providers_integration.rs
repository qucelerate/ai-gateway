//! Native batches on Azure OpenAI and Anthropic (D-021, Phase 7 milestone
//! 2), each against a fake of its batch API: the job is submitted once in
//! the provider's own shape, found again after a crash instead of being
//! submitted twice, collected, and every line settled once at the batch
//! price from the usage the provider reports.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL`.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Multipart, Path, Query, State},
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

const PEPPER: &str = "batch-providers-test-pepper-0000000000";

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

fn manifest(family: &str, key: &str, model: &str) -> CatalogManifest {
    let capability = |operation: &str| {
        json!({
            "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 64,
            "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 60,
            "chat_input_count_mode": "unsupported"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": format!("batch-{family}")},
        "providers": [{"adapter_instance_key": key, "family": family}],
        "deployments": [{"name": "batch-dep", "model": model, "location": null,
                         "adapter_instance_key": key,
                         "capabilities": [capability("chat_batch")]}],
        "prices": [{
            "deployment": "batch-dep", "operation": "chat_batch", "input_minor_per_token": 2,
            "output_minor_per_token": 6, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "batch-providers-test", "source_version": "v1", "confidence": "known"
        }],
        "routes": [{"alias": "fast", "operation": "chat_batch", "deployment": "batch-dep"}]
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
        .create_organization(&format!("batch-providers-{suffix}"), "Batch providers test")
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
        .create_service_account(org, user, None, "batch-providers")
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
    worker.batches = worker.batches.with_objects(Some(objects));
    let (org, bearer) = tenant(&pool, &manifest).await;
    Stack {
        pool,
        api,
        worker,
        org,
        bearer,
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

// ---------------------------------------------------------------- Azure

#[derive(Default)]
struct AzureFake {
    files: Vec<(String, String)>,
    batches: Vec<Value>,
    creates: usize,
    /// Each request's path and `api-version`, and whether it carried the key.
    requests: Vec<(String, Option<String>, bool)>,
}

type Azure = Arc<Mutex<AzureFake>>;

fn azure_seen(
    fake: &Azure,
    path: &str,
    query: &std::collections::HashMap<String, String>,
    headers: &HeaderMap,
) {
    fake.lock().unwrap().requests.push((
        path.to_owned(),
        query.get("api-version").cloned(),
        headers.get("api-key").is_some_and(|key| key == "azure-key"),
    ));
}

async fn azure_upload(
    State(fake): State<Azure>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Json<Value> {
    azure_seen(&fake, "files", &query, &headers);
    let mut content = String::new();
    while let Some(field) = form.next_field().await.unwrap() {
        if field.name() == Some("file") {
            content = field.text().await.unwrap();
        }
    }
    let mut fake = fake.lock().unwrap();
    let id = format!("file-{}", fake.files.len());
    fake.files.push((id.clone(), content));
    Json(json!({"id": id, "object": "file", "purpose": "batch", "created_at": 100}))
}

async fn azure_create(
    State(fake): State<Azure>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    azure_seen(&fake, "batches", &query, &headers);
    let mut fake = fake.lock().unwrap();
    fake.creates += 1;
    let batch = json!({
        "id": format!("batch_az{}", fake.batches.len()), "status": "validating",
        "endpoint": body["endpoint"], "input_file_id": body["input_file_id"],
        "metadata": body["metadata"], "created_at": 100,
        "output_file_id": null, "error_file_id": null,
    });
    fake.batches.push(batch.clone());
    Json(batch)
}

async fn azure_list(State(fake): State<Azure>) -> Json<Value> {
    let fake = fake.lock().unwrap();
    let data: Vec<Value> = fake.batches.iter().rev().cloned().collect();
    Json(json!({"object": "list", "data": data, "has_more": false}))
}

async fn azure_one(State(fake): State<Azure>, Path(id): Path<String>) -> Json<Value> {
    let fake = fake.lock().unwrap();
    Json(fake.batches.iter().find(|b| b["id"] == id).unwrap().clone())
}

async fn azure_file(Path(id): Path<String>) -> Json<Value> {
    Json(json!({"id": id, "object": "file", "purpose": "batch", "created_at": 100}))
}

async fn azure_content(State(fake): State<Azure>, Path(id): Path<String>) -> String {
    let fake = fake.lock().unwrap();
    fake.files
        .iter()
        .find(|(file, _)| *file == id)
        .unwrap()
        .1
        .clone()
}

/// Runs the job: every line answers with 5 input and 7 output tokens,
/// except "fail", which Azure refuses.
fn azure_complete(fake: &Azure, index: usize) {
    let mut fake = fake.lock().unwrap();
    let input_id = fake.batches[index]["input_file_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let input = fake
        .files
        .iter()
        .find(|(id, _)| *id == input_id)
        .unwrap()
        .1
        .clone();
    let (mut output, mut errors) = (String::new(), String::new());
    for raw in input.lines() {
        let line: Value = serde_json::from_str(raw).unwrap();
        let custom_id = &line["custom_id"];
        if line["body"]["messages"][0]["content"] == "fail" {
            errors.push_str(&format!(
                "{}\n",
                json!({
                    "custom_id": custom_id,
                    "response": {"status_code": 400, "request_id": "apim-fail",
                                 "body": {"error": {"code": "content_filter", "message": "no"}}},
                    "error": null,
                })
            ));
        } else {
            output.push_str(&format!("{}\n", json!({
                "custom_id": custom_id,
                "response": {"status_code": 200, "request_id": format!("apim-{custom_id}"),
                             "body": {"id": "c", "model": "gpt-4.1",
                                      "choices": [{"index": 0, "finish_reason": "stop",
                                                   "message": {"role": "assistant", "content": "hi"}}],
                                      "usage": {"prompt_tokens": 5, "completion_tokens": 7}}},
                "error": null,
            })));
        }
    }
    fake.files.push((format!("out-{index}"), output));
    fake.files.push((format!("err-{index}"), errors));
    let batch = &mut fake.batches[index];
    batch["status"] = json!("completed");
    batch["output_file_id"] = json!(format!("out-{index}"));
    batch["error_file_id"] = json!(format!("err-{index}"));
}

#[tokio::test]
async fn azure_openai_batches_run_through_the_resources_batch_api() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: Azure = Arc::default();
    let provider = serve(
        Router::new()
            .route("/openai/files", post(azure_upload))
            .route("/openai/files/{id}", get(azure_file))
            .route("/openai/files/{id}/content", get(azure_content))
            .route("/openai/batches", post(azure_create).get(azure_list))
            .route("/openai/batches/{id}", get(azure_one))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        &url,
        manifest("azure-openai", "azure-openai-main", "gpt-4.1-batch"),
        &[
            ("AZURE_OPENAI_BASE_URL", provider.clone()),
            ("AZURE_OPENAI_API_KEY", "azure-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    let runner = &stack.worker.batches;
    let batch = stack
        .submit(
            "azure-batch",
            &[("hello", 10), ("fail", 10), ("hello", 4096)],
        )
        .await;

    let mut now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    {
        let fake = fake.lock().unwrap();
        assert_eq!(fake.creates, 1);
        // The Files and Batch APIs at their own version, signed with the key.
        assert!(
            fake.requests
                .iter()
                .all(|(_, version, keyed)| version.as_deref() == Some("2024-10-21") && *keyed),
            "{:?}",
            fake.requests
        );
        assert_eq!(fake.batches[0]["endpoint"], "/chat/completions");
        assert_eq!(
            fake.batches[0]["metadata"]["gateway_batch"],
            batch.trim_start_matches("batch_")
        );
        // Each line names the batch deployment and is held to its cap.
        let lines: Vec<Value> = fake.files[0]
            .1
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            lines.iter().all(|line| line["url"] == "/chat/completions"
                && line["body"]["model"] == "gpt-4.1-batch")
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line["body"]["max_tokens"].as_u64())
                .collect::<Vec<_>>(),
            [Some(10), Some(10), Some(64)]
        );
    }

    // Found by its tag after a crash, not created again.
    stack.crash_after_create().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);

    // Collected and settled at the batch price: 5 x 2 + 7 x 6 = 52 a line,
    // the refused line at zero.
    azure_complete(&fake, 0);
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
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "completed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 1})
    );
}

// ------------------------------------------------------------ Anthropic

#[derive(Default)]
struct AnthropicFake {
    batches: Vec<Value>,
    /// Each batch's requests.
    requests: Vec<Vec<Value>>,
    creates: usize,
    /// The next create fails before Anthropic takes the batch.
    fail_create: bool,
    /// Whether every call carried the key and the API version.
    signed: bool,
}

type Anthropic = Arc<Mutex<AnthropicFake>>;

fn anthropic_signed(fake: &Anthropic, headers: &HeaderMap) {
    let ok = headers
        .get("x-api-key")
        .is_some_and(|key| key == "anthropic-key")
        && headers
            .get("anthropic-version")
            .is_some_and(|version| version == "2023-06-01");
    let mut fake = fake.lock().unwrap();
    fake.signed = fake.signed && ok;
}

async fn anthropic_create(
    State(fake): State<Anthropic>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    anthropic_signed(&fake, &headers);
    let mut fake = fake.lock().unwrap();
    if std::mem::take(&mut fake.fail_create) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    fake.creates += 1;
    let requests = body["requests"].as_array().unwrap().clone();
    let batch = json!({
        "id": format!("msgbatch_{}", fake.batches.len()), "type": "message_batch",
        "processing_status": "in_progress",
        "request_counts": {"processing": requests.len(), "succeeded": 0, "errored": 0,
                           "canceled": 0, "expired": 0},
        "created_at": chrono::Utc::now().to_rfc3339(), "cancel_initiated_at": null,
        "ended_at": null, "results_url": null,
    });
    fake.batches.push(batch.clone());
    fake.requests.push(requests);
    Ok(Json(batch))
}

async fn anthropic_list(State(fake): State<Anthropic>, headers: HeaderMap) -> Json<Value> {
    anthropic_signed(&fake, &headers);
    let fake = fake.lock().unwrap();
    let data: Vec<Value> = fake.batches.iter().rev().cloned().collect();
    Json(json!({"data": data, "has_more": false, "first_id": null, "last_id": null}))
}

async fn anthropic_one(
    State(fake): State<Anthropic>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Json<Value> {
    anthropic_signed(&fake, &headers);
    let fake = fake.lock().unwrap();
    Json(fake.batches.iter().find(|b| b["id"] == id).unwrap().clone())
}

async fn anthropic_results(
    State(fake): State<Anthropic>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<String, StatusCode> {
    anthropic_signed(&fake, &headers);
    let fake = fake.lock().unwrap();
    let index = fake.batches.iter().position(|b| b["id"] == id).unwrap();
    if fake.batches[index]["processing_status"] != "ended" {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut results = String::new();
    for request in &fake.requests[index] {
        let custom_id = &request["custom_id"];
        let result = if request["params"]["messages"][0]["content"] == "fail" {
            json!({"type": "errored", "error": {"type": "error",
                   "error": {"type": "invalid_request_error", "message": "no"}}})
        } else {
            json!({"type": "succeeded", "message": {
                "id": format!("msg_{custom_id}"), "type": "message", "role": "assistant",
                "model": request["params"]["model"], "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {"input_tokens": 5, "output_tokens": 7}}})
        };
        results.push_str(&format!(
            "{}\n",
            json!({"custom_id": custom_id, "result": result})
        ));
    }
    Ok(results)
}

/// Ends the batch: its results are readable from now on.
fn anthropic_end(fake: &Anthropic, index: usize) {
    let mut fake = fake.lock().unwrap();
    let total = fake.requests[index].len();
    let failed = fake.requests[index]
        .iter()
        .filter(|request| request["params"]["messages"][0]["content"] == "fail")
        .count();
    let batch = &mut fake.batches[index];
    batch["processing_status"] = json!("ended");
    batch["request_counts"] = json!({"processing": 0, "succeeded": total - failed,
                                     "errored": failed, "canceled": 0, "expired": 0});
    batch["ended_at"] = json!(chrono::Utc::now().to_rfc3339());
}

#[tokio::test]
async fn anthropic_batches_send_requests_inline_and_are_found_by_their_results() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let fake: Anthropic = Arc::new(Mutex::new(AnthropicFake {
        signed: true,
        ..AnthropicFake::default()
    }));
    let provider = serve(
        Router::new()
            .route(
                "/v1/messages/batches",
                post(anthropic_create).get(anthropic_list),
            )
            .route("/v1/messages/batches/{id}", get(anthropic_one))
            .route("/v1/messages/batches/{id}/results", get(anthropic_results))
            .with_state(fake.clone()),
    )
    .await;
    let stack = stack(
        &url,
        manifest("anthropic", "anthropic-main", "claude-test"),
        &[
            ("ANTHROPIC_BASE_URL", provider.clone()),
            ("ANTHROPIC_API_KEY", "anthropic-key".to_owned()),
        ],
    )
    .await;
    stack.park_others().await;
    let runner = &stack.worker.batches;
    // A line Anthropic would not take refuses the batch when it is made,
    // as the single endpoint refuses the request, before anything is held.
    let (status, refused) = stack
        .create(
            "anthropic-refused",
            &[
                json!({"messages": [{"role": "user", "content": "hello"}]}),
                json!({"messages": [{"role": "developer", "content": "be brief"},
                                    {"role": "user", "content": "hello"}]}),
            ],
        )
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["errors"]["data"][0]["line"], 2);
    assert_eq!(refused["errors"]["data"][0]["code"], "invalid_body");
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 0));
    let batch = stack
        .submit(
            "anthropic-batch",
            &[("hello", 10), ("fail", 10), ("hello", 4096)],
        )
        .await;

    // One create call carrying every request, each named by the job and
    // its line and held to its cap.
    let mut now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    let job: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.batch_job WHERE organization_id = $1")
            .bind(stack.org)
            .fetch_one(&stack.pool)
            .await
            .unwrap();
    {
        let fake = fake.lock().unwrap();
        assert_eq!(fake.creates, 1);
        let requests = &fake.requests[0];
        assert_eq!(
            requests
                .iter()
                .map(|r| r["custom_id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            (1..=3)
                .map(|line| format!("gw-{}-{line}", job.simple()))
                .collect::<Vec<_>>()
        );
        assert!(
            requests
                .iter()
                .all(|r| r["params"]["model"] == "claude-test"
                    && r["params"].get("stream").is_none())
        );
        assert_eq!(
            requests
                .iter()
                .map(|r| r["params"]["max_tokens"].as_u64())
                .collect::<Vec<_>>(),
            [Some(10), Some(10), Some(64)]
        );
    }

    // After a crash the job may be the running batch of three requests,
    // which cannot be read until it ends: the worker waits, never creating
    // a second one.
    stack.crash_after_create().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.submitted, report.retried), (0, 1), "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);
    // Once it ends, its results name the job: found, not created again,
    // and collected.
    anthropic_end(&fake, 0);
    now += chrono::Duration::minutes(2);
    stack.due_now().await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);
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
    let ended = stack.batch(&batch).await;
    assert_eq!(ended["status"], "completed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 1})
    );

    // A create that failed before Anthropic took it: the next run finds no
    // batch that could be the job's (the only one holds three requests and
    // names another job) and creates it once.
    let second = stack.submit("anthropic-batch-2", &[("hello", 10)]).await;
    fake.lock().unwrap().fail_create = true;
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.submitted, report.retried), (0, 1), "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 1);
    stack.due_now().await;
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(fake.lock().unwrap().creates, 2);
    anthropic_end(&fake, 1);
    stack.due_now().await;
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(stack.batch(&second).await["status"], "completed");
    assert_eq!(consumed(&stack.pool, stack.org).await, (0, 156));
    assert!(fake.lock().unwrap().signed, "every call signed");
}
