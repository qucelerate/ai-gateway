//! The batch worker end to end (D-021), against a fake OpenAI Batch API: a
//! batch's job is uploaded and submitted once, found again by its tag
//! after a crash instead of being submitted twice, collected, and every
//! line settled once, at the batch price, from the usage the provider
//! reports. A worker that dies mid-settlement and runs again charges
//! nothing twice and loses no line.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL`.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Multipart, Path, State},
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

const PEPPER: &str = "batch-worker-test-pepper-00000000000";

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

/// What the fake provider holds.
#[derive(Default)]
struct Fake {
    files: Vec<(String, String)>,
    batches: Vec<Value>,
    creates: usize,
    uploads: usize,
    /// The provider has lost its jobs: polls answer 404.
    gone: bool,
    /// The provider is down for uploads: they answer 503.
    refuse_uploads: bool,
}

type Shared = Arc<Mutex<Fake>>;

/// Input uploads and job creations so far.
fn calls(fake: &Shared) -> (usize, usize) {
    let fake = fake.lock().unwrap();
    (fake.uploads, fake.creates)
}

async fn upload(
    State(fake): State<Shared>,
    mut form: Multipart,
) -> Result<Json<Value>, axum::http::StatusCode> {
    let mut content = String::new();
    while let Some(field) = form.next_field().await.unwrap() {
        if field.name() == Some("file") {
            content = field.text().await.unwrap();
        }
    }
    let mut fake = fake.lock().unwrap();
    if fake.refuse_uploads {
        return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    fake.uploads += 1;
    let id = format!("file-in-{}", fake.files.len());
    fake.files.push((id.clone(), content));
    Ok(Json(
        json!({"id": id, "object": "file", "purpose": "batch"}),
    ))
}

/// Starts cancelling a running job, as OpenAI does.
async fn cancel(State(fake): State<Shared>, Path(id): Path<String>) -> Json<Value> {
    let mut fake = fake.lock().unwrap();
    let batch = fake.batches.iter_mut().find(|b| b["id"] == id).unwrap();
    batch["status"] = json!("cancelling");
    Json(batch.clone())
}

async fn create(State(fake): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    let mut fake = fake.lock().unwrap();
    fake.creates += 1;
    let batch = json!({
        "id": format!("batch_p{}", fake.batches.len()),
        "status": "validating",
        "input_file_id": body["input_file_id"],
        "metadata": body["metadata"],
        "output_file_id": null,
        "error_file_id": null,
    });
    fake.batches.push(batch.clone());
    Json(batch)
}

async fn list(State(fake): State<Shared>) -> Json<Value> {
    let fake = fake.lock().unwrap();
    Json(json!({"object": "list", "data": fake.batches, "has_more": false}))
}

async fn one(
    State(fake): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    let fake = fake.lock().unwrap();
    if fake.gone {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(Json(
        fake.batches.iter().find(|b| b["id"] == id).unwrap().clone(),
    ))
}

async fn file(Path(id): Path<String>) -> Json<Value> {
    Json(json!({"id": id, "object": "file", "purpose": "batch", "created_at": 0}))
}

async fn content(State(fake): State<Shared>, Path(id): Path<String>) -> String {
    let fake = fake.lock().unwrap();
    fake.files
        .iter()
        .find(|(file, _)| *file == id)
        .unwrap()
        .1
        .clone()
}

/// Runs the provider's job: every line succeeds with 5 input and 7 output
/// tokens, except a line saying "fail", which the provider refuses.
fn complete(fake: &Shared, index: usize) {
    end_job(fake, index, "completed", &[]);
}

/// Ends the provider's job with `status`, its output covering every line
/// but those at the (0-based) positions in `missing` of its input.
fn end_job(fake: &Shared, index: usize, status: &str, missing: &[usize]) {
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
    for (position, raw) in input.lines().enumerate() {
        let line: Value = serde_json::from_str(raw).unwrap();
        let custom_id = &line["custom_id"];
        if missing.contains(&position) {
            continue;
        }
        if line["body"]["messages"][0]["content"] == "fail" {
            errors.push_str(&format!(
                "{}\n",
                json!({
                    "id": "r", "custom_id": custom_id,
                    "response": {"status_code": 400, "request_id": "req-fail",
                                 "body": {"error": {"code": "invalid_prompt", "message": "no"}}},
                    "error": null,
                })
            ));
        } else {
            output.push_str(&format!("{}\n", json!({
                "id": "r", "custom_id": custom_id,
                "response": {"status_code": 200, "request_id": format!("req-{custom_id}"),
                             "body": {"id": "c", "model": "gpt-batches",
                                      "choices": [{"index": 0, "finish_reason": "stop",
                                                   "message": {"role": "assistant", "content": "hi"}}],
                                      "usage": {"prompt_tokens": 5, "completion_tokens": 7,
                                                "prompt_tokens_details": {"cached_tokens": 0}}}},
                "error": null,
            })));
        }
    }
    fake.files.push((format!("file-out-{index}"), output));
    fake.files.push((format!("file-err-{index}"), errors));
    let batch = &mut fake.batches[index];
    batch["status"] = json!(status);
    batch["output_file_id"] = json!(format!("file-out-{index}"));
    batch["error_file_id"] = json!(format!("file-err-{index}"));
}

fn manifest() -> CatalogManifest {
    let capability = |operation: &str| {
        json!({
            "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 64,
            "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 60,
            "chat_input_count_mode": "unsupported"
        })
    };
    let price = |operation: &str, input: i64, output: i64| {
        json!({
            "deployment": "chat-a", "operation": operation, "input_minor_per_token": input,
            "output_minor_per_token": output,
            // Batch prices have a cached-input rate, as the clouds' do: a line
            // that charges nothing must still be priced.
            "cached_input_minor_per_token": i64::from(operation.ends_with("_batch")),
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "batch-worker-test", "source_version": "v1", "confidence": "known"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": "batch-worker-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "gpt-batches", "location": null,
                         "adapter_instance_key": "openai-main",
                         "capabilities": [capability("chat"), capability("chat_batch")]}],
        "prices": [price("chat", 4, 12), price("chat_batch", 2, 6)],
        "routes": [
            {"alias": "fast", "operation": "chat", "deployment": "chat-a"},
            {"alias": "fast", "operation": "chat_batch", "deployment": "chat-a"}
        ]
    }))
    .unwrap()
}

async fn tenant(pool: &PgPool) -> (Uuid, String) {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("batch-worker-{suffix}"), "Batch worker test")
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
        .create_service_account(org, user, None, "batch-worker")
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
    let manifest = manifest();
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    (org.0, bearer)
}

fn config(url: &str, profile: &str, provider: &str) -> AppConfig {
    let vars = [
        ("DATABASE_URL", url.to_owned()),
        ("GATEWAY_PROFILE", profile.to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batch-worker".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider}/v1")),
    ];
    AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap()
}

async fn budget(pool: &PgPool, org: Uuid) -> (i64, i64) {
    sqlx::query_as("SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket WHERE organization_id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The worker runs every organization's jobs and each test parks the
/// others': one at a time.
static WORKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn a_batch_runs_once_settles_every_line_once_and_survives_a_crash() {
    let _one_at_a_time = WORKER.lock().await;
    let Some(url) = database_url() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let fake: Shared = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/files", post(upload))
            .route("/v1/files/{id}", get(file))
            .route("/v1/files/{id}/content", get(content))
            .route("/v1/batches", post(create).get(list))
            .route("/v1/batches/{id}", get(one))
            .with_state(fake.clone()),
    )
    .await;
    let memory = Arc::new(MemoryTraceObjects::default());
    let objects: Arc<dyn TraceObjects> = memory.clone();
    let state = gateway::application::connect(&config(&url, "api", &provider))
        .await
        .unwrap()
        .with_trace_objects(Some(objects.clone()));
    let api = serve(gateway::transport::router(state)).await;
    let mut worker = gateway::application::connect_worker(&config(&url, "worker", &provider))
        .await
        .unwrap();
    worker.batches = worker.batches.with_objects(Some(objects.clone()));
    let runner = &worker.batches;
    // The worker runs every organization's batches: park those other tests
    // left in the shared database, so this run sees only its own.
    sqlx::query("UPDATE gateway.batch_job SET next_run_at = 'infinity' WHERE state <> 'finished'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gateway.batch SET lease_token = gen_random_uuid(), lease_expires_at = 'infinity'
         WHERE status IN ('in_progress', 'finalizing', 'cancelling')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (org, bearer) = tenant(&pool).await;
    let http = reqwest::Client::new();

    // A three-line batch; the provider refuses the middle line. The last
    // asks for more output than the route's 64 tokens.
    let line = |id: &str, text: &str, max_tokens: u32| {
        json!({"custom_id": id, "method": "POST", "url": "/v1/chat/completions",
               "body": {"model": "fast", "messages": [{"role": "user", "content": text}],
                        "max_tokens": max_tokens}})
        .to_string()
    };
    let jsonl = [
        line("a", "hello", 10),
        line("b", "fail", 10),
        line("c", "hello", 4096),
    ]
    .join("\n");
    let form = format!(
        "--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
         --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
         {jsonl}\r\n--b--\r\n"
    );
    let file: Value = http
        .post(format!("{api}/api/inference/v1/files"))
        .bearer_auth(&bearer)
        .header("content-type", "multipart/form-data; boundary=b")
        .body(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batch: Value = http
        .post(format!("{api}/api/inference/v1/batches"))
        .bearer_auth(&bearer)
        .header("idempotency-key", "worker-test")
        .json(
            &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                      "completion_window": "24h"}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batch_id = batch["id"].as_str().unwrap().to_owned();
    let (held, _) = budget(&pool, org).await;
    assert!(held > 0);

    // Submitted once, its lines claimed first.
    let mut now = chrono::Utc::now();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(calls(&fake), (1, 1));
    // Each line goes out held to the output its reservation covers.
    let sent: Vec<Value> = fake.lock().unwrap().files[0]
        .1
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        sent.iter()
            .map(|line| line["body"]["max_completion_tokens"].as_u64())
            .collect::<Vec<_>>(),
        [Some(10), Some(10), Some(64)]
    );
    let tag = &fake.lock().unwrap().batches[0]["metadata"].clone();
    assert_eq!(tag["gateway_batch"], batch_id.trim_start_matches("batch_"));
    let claimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation WHERE organization_id = $1 AND state = 'claimed'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(claimed, 3);

    // A worker that died after the provider took the job, before recording
    // it: the next run finds the job by its tag rather than submitting again.
    sqlx::query(
        "UPDATE gateway.batch_job SET state = 'pending', provider_batch_id = NULL,
             next_run_at = now() - interval '1 minute', lease_token = NULL, lease_expires_at = NULL
         WHERE organization_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    assert_eq!(calls(&fake), (1, 1));

    // Still running: polled, nothing settled.
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.jobs, report.collected), (1, 0), "{report:?}");

    // Collected: each line settled from the provider's usage at the batch
    // price (5 x 2 + 7 x 6 = 52), the refused one at zero, and the batch
    // finalized in the same run.
    complete(&fake, 0);
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(budget(&pool, org).await, (0, 104));
    let settled: Vec<(String, i64)> = sqlx::query_as(
        "SELECT r.state, coalesce(u.amount_minor, 0)
         FROM gateway.accounting_reservation r
         LEFT JOIN gateway.accounting_usage u ON u.reservation_id = r.id
         JOIN gateway.batch_line l ON l.reservation_id = r.id
         WHERE r.organization_id = $1 ORDER BY l.line_number",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        settled,
        vec![
            ("settled".to_owned(), 52),
            ("settled".to_owned(), 0),
            ("settled".to_owned(), 52)
        ]
    );
    batch_lines_name_their_batch(&pool, org, &batch_id).await;
    let read = |path: String| {
        let http = http.clone();
        let bearer = bearer.clone();
        async move {
            http.get(path)
                .bearer_auth(bearer)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    let ended: Value =
        serde_json::from_str(&read(format!("{api}/api/inference/v1/batches/{batch_id}")).await)
            .unwrap();
    assert_eq!(ended["status"], "completed");
    assert_eq!(
        ended["request_counts"],
        json!({"total": 3, "completed": 2, "failed": 1})
    );
    let output = read(format!(
        "{api}/api/inference/v1/files/{}/content",
        ended["output_file_id"].as_str().unwrap()
    ))
    .await;
    let output: Vec<Value> = output
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["custom_id"], "a");
    assert_eq!(output[0]["response"]["status_code"], 200);
    assert_eq!(output[0]["response"]["body"]["model"], "fast");
    assert_eq!(output[0]["response"]["body"]["usage"]["input_tokens"], 5);
    let errors = read(format!(
        "{api}/api/inference/v1/files/{}/content",
        ended["error_file_id"].as_str().unwrap()
    ))
    .await;
    let errors: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(errors["custom_id"], "b");
    assert_eq!(errors["error"]["code"], "invalid_prompt");

    // A worker that died mid-settlement: one line not yet marked, the job
    // not finished and the batch not ended. Running again settles that line
    // by replaying its settlement, so nothing is charged twice, and ends
    // the batch with the same files.
    sqlx::query(
        "UPDATE gateway.batch_line SET state = 'pending', finished_at = NULL
         WHERE organization_id = $1 AND line_number = 3",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE gateway.batch_job SET state = 'submitted', next_run_at = now() - interval '1 minute'
         WHERE organization_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE gateway.batch SET status = 'finalizing', completed_at = NULL WHERE organization_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(budget(&pool, org).await, (0, 104));
    let again: Value =
        serde_json::from_str(&read(format!("{api}/api/inference/v1/batches/{batch_id}")).await)
            .unwrap();
    assert_eq!(again["status"], "completed");
    assert_eq!(again["output_file_id"], ended["output_file_id"]);
    assert_eq!(again["request_counts"], ended["request_counts"]);
    assert_eq!(fake.lock().unwrap().creates, 1);

    // Saved results go once the batch ends, under names the bucket takes.
    // A delete the store refuses stays recorded and is retried next run.
    let saved = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.batch_job WHERE organization_id = $1 AND result_object_key IS NOT NULL",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(saved(pool.clone()).await, 0);
    assert!(
        !memory
            .names()
            .iter()
            .any(|name| name.starts_with("batch-results/")),
        "{:?}",
        memory.names()
    );
    let job: Uuid =
        sqlx::query_scalar("SELECT id FROM gateway.batch_job WHERE organization_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    let key = format!(
        "batch-results/{org}/{}/{job}",
        batch_id.trim_start_matches("batch_")
    );
    objects.put(&key, b"left behind".to_vec()).await.unwrap();
    sqlx::query("UPDATE gateway.batch_job SET result_object_key = $2 WHERE organization_id = $1")
        .bind(org)
        .bind(&key)
        .execute(&pool)
        .await
        .unwrap();
    *memory.failing.lock().unwrap() =
        Some(gateway::application::trace_objects::TraceObjectError::Status(503));
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    *memory.failing.lock().unwrap() = None;
    assert_eq!(report.cleaned, 0, "{report:?}");
    assert_eq!(saved(pool.clone()).await, 1);
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert!(report.cleaned >= 1, "{report:?}");
    assert_eq!(saved(pool.clone()).await, 0);
    assert!(memory.object(&key).is_none());

    // A one-line batch, submitted.
    let submit = |key: &'static str| {
        let (http, api, bearer) = (http.clone(), api.clone(), bearer.clone());
        async move {
            let jsonl = line("a", "hello", 10);
            let form = format!(
                "--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
                 --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
                 {jsonl}\r\n--b--\r\n"
            );
            let file: Value = http
                .post(format!("{api}/api/inference/v1/files"))
                .bearer_auth(&bearer)
                .header("content-type", "multipart/form-data; boundary=b")
                .body(form)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let batch: Value = http
                .post(format!("{api}/api/inference/v1/batches"))
                .bearer_auth(&bearer)
                .header("idempotency-key", key)
                .json(
                    &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                              "completion_window": "24h"}),
                )
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            Uuid::parse_str(batch["id"].as_str().unwrap().trim_start_matches("batch_")).unwrap()
        }
    };

    // A line whose saved result is lost before the batch's files are
    // written is reported as an error, never as a success with no answer.
    let lost = submit("worker-test-lost").await;
    now += chrono::Duration::minutes(2);
    assert_eq!(runner.run_once(now).await.unwrap().submitted, 1);
    complete(&fake, 1);
    let hold = |lease: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!(
                "UPDATE gateway.batch SET lease_token = gen_random_uuid(), lease_expires_at = '{lease}'
                 WHERE organization_id = $1 AND id = $2"
            ))
            .bind(org)
            .bind(lost)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    hold("infinity").await;
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 0), "{report:?}");
    let job: Uuid = sqlx::query_scalar(
        "SELECT id FROM gateway.batch_job WHERE organization_id = $1 AND batch_id = $2",
    )
    .bind(org)
    .bind(lost)
    .fetch_one(&pool)
    .await
    .unwrap();
    objects
        .delete(&format!("batch-results/{org}/{lost}/{job}"))
        .await
        .unwrap();
    hold("-infinity").await;
    now += chrono::Duration::minutes(2);
    assert_eq!(runner.run_once(now).await.unwrap().finalized, 1);
    let ended: Value =
        serde_json::from_str(&read(format!("{api}/api/inference/v1/batches/batch_{lost}")).await)
            .unwrap();
    assert_eq!(
        ended["request_counts"],
        json!({"total": 1, "completed": 0, "failed": 1})
    );
    assert!(ended["output_file_id"].is_null());
    let errors = read(format!(
        "{api}/api/inference/v1/files/{}/content",
        ended["error_file_id"].as_str().unwrap()
    ))
    .await;
    let errors: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(errors["error"]["code"], "result_unavailable");
    assert!(errors["response"].is_null());

    // A job given up after its results were saved keeps them: the lines
    // still pending settle from the saved file, and the answers reach the
    // output file.
    let resumed = submit("worker-test-resumed").await;
    now += chrono::Duration::minutes(2);
    assert_eq!(runner.run_once(now).await.unwrap().submitted, 1);
    complete(&fake, 2);
    let hold_resumed = |lease: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!(
                "UPDATE gateway.batch SET lease_token = gen_random_uuid(), lease_expires_at = '{lease}'
                 WHERE organization_id = $1 AND id = $2"
            ))
            .bind(org)
            .bind(resumed)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    hold_resumed("infinity").await;
    now += chrono::Duration::minutes(2);
    assert_eq!(runner.run_once(now).await.unwrap().collected, 1);
    let (_, consumed) = budget(&pool, org).await;
    // Died before the line was marked and the job finished; the provider
    // then stops answering.
    sqlx::query(
        "UPDATE gateway.batch_line SET state = 'pending', finished_at = NULL
         WHERE organization_id = $1 AND batch_id = $2",
    )
    .bind(org)
    .bind(resumed)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE gateway.batch_job SET state = 'submitted', next_run_at = now() - interval '1 minute'
         WHERE organization_id = $1 AND batch_id = $2",
    )
    .bind(org)
    .bind(resumed)
    .execute(&pool)
    .await
    .unwrap();
    hold_resumed("-infinity").await;
    fake.lock().unwrap().gone = true;
    let expires: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT expires_at FROM gateway.batch WHERE organization_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(resumed)
    .fetch_one(&pool)
    .await
    .unwrap();
    let report = runner
        .run_once(expires + chrono::Duration::hours(5) + chrono::Duration::minutes(30))
        .await
        .unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(
        budget(&pool, org).await.1,
        consumed,
        "nothing charged twice"
    );
    let ended: Value = serde_json::from_str(
        &read(format!("{api}/api/inference/v1/batches/batch_{resumed}")).await,
    )
    .unwrap();
    assert_eq!(
        ended["request_counts"],
        json!({"total": 1, "completed": 1, "failed": 0})
    );
    let output = read(format!(
        "{api}/api/inference/v1/files/{}/content",
        ended["output_file_id"].as_str().unwrap()
    ))
    .await;
    let output: Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
    assert_eq!(output["response"]["status_code"], 200);
    assert_eq!(output["response"]["body"]["usage"]["output_tokens"], 7);
    fake.lock().unwrap().gone = false;

    // A job whose provider stops answering is given up five hours after
    // the window: its line goes to reconciliation, not free, and the batch
    // ends instead of retrying for ever.
    let silent = submit("worker-test-silent").await;
    now += chrono::Duration::minutes(2);
    assert_eq!(runner.run_once(now).await.unwrap().submitted, 1);
    fake.lock().unwrap().gone = true;
    let expires: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT expires_at FROM gateway.batch WHERE organization_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(silent)
    .fetch_one(&pool)
    .await
    .unwrap();
    let report = runner
        .run_once(expires + chrono::Duration::minutes(30))
        .await
        .unwrap();
    assert_eq!((report.retried, report.collected), (1, 0), "{report:?}");
    sqlx::query("UPDATE gateway.batch_job SET next_run_at = now() - interval '1 minute' WHERE organization_id = $1 AND batch_id = $2")
        .bind(org)
        .bind(silent)
        .execute(&pool)
        .await
        .unwrap();
    let report = runner
        .run_once(expires + chrono::Duration::hours(5) + chrono::Duration::minutes(30))
        .await
        .unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    let (status, line_state): (String, String) = sqlx::query_as(
        "SELECT b.status, l.state FROM gateway.batch b
         JOIN gateway.batch_line l ON l.organization_id = b.organization_id AND l.batch_id = b.id
         WHERE b.organization_id = $1 AND b.id = $2",
    )
    .bind(org)
    .bind(silent)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), line_state.as_str()),
        ("expired", "unknown")
    );
}

/// A batch accepted before the organization's routing policy forbade its
/// deployment (D-023) is never sent: its job ends unsent, every line fails
/// with `routing_policy_denied`, and nothing is charged.
#[tokio::test]
async fn a_job_the_routing_policy_now_forbids_is_never_sent() {
    let _one_at_a_time = WORKER.lock().await;
    let Some(url) = database_url() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let fake: Shared = Arc::default();
    let provider = serve(
        Router::new()
            .route("/v1/files", post(upload))
            .route("/v1/files/{id}", get(file))
            .route("/v1/files/{id}/content", get(content))
            .route("/v1/batches", post(create).get(list))
            .route("/v1/batches/{id}", get(one))
            .with_state(fake.clone()),
    )
    .await;
    let objects: Arc<dyn TraceObjects> = Arc::new(MemoryTraceObjects::default());
    let state = gateway::application::connect(&config(&url, "api", &provider))
        .await
        .unwrap()
        .with_trace_objects(Some(objects.clone()));
    let api = serve(gateway::transport::router(state)).await;
    let mut worker = gateway::application::connect_worker(&config(&url, "worker", &provider))
        .await
        .unwrap();
    worker.batches = worker.batches.with_objects(Some(objects.clone()));
    let runner = &worker.batches;
    sqlx::query("UPDATE gateway.batch_job SET next_run_at = 'infinity' WHERE state <> 'finished'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gateway.batch SET lease_token = gen_random_uuid(), lease_expires_at = 'infinity'
         WHERE status IN ('in_progress', 'finalizing', 'cancelling')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (org, bearer) = tenant(&pool).await;
    let http = reqwest::Client::new();
    let jsonl = ["a", "b"]
        .map(|id| {
            json!({"custom_id": id, "method": "POST", "url": "/v1/chat/completions",
                   "body": {"model": "fast", "messages": [{"role": "user", "content": "hello"}],
                            "max_tokens": 10}})
            .to_string()
        })
        .join("\n");
    let form = format!(
        "--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
         --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
         {jsonl}\r\n--b--\r\n"
    );
    let file: Value = http
        .post(format!("{api}/api/inference/v1/files"))
        .bearer_auth(&bearer)
        .header("content-type", "multipart/form-data; boundary=b")
        .body(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batch: Value = http
        .post(format!("{api}/api/inference/v1/batches"))
        .bearer_auth(&bearer)
        .header("idempotency-key", "policy-test")
        .json(
            &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                      "completion_window": "24h"}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(batch["id"].is_string(), "{batch}");
    assert!(budget(&pool, org).await.0 > 0);

    // The organization now allows Bedrock only; the batch's route is OpenAI.
    sqlx::query(
        "INSERT INTO gateway.routing_policy (organization_id, id, providers)
         VALUES ($1, gen_random_uuid(), ARRAY['bedrock'])",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let now = chrono::Utc::now();
    runner.run_once(now).await.unwrap();
    assert_eq!(calls(&fake), (0, 0), "nothing reaches the provider");
    let lines: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT state, error_code FROM gateway.batch_line WHERE organization_id = $1
         ORDER BY line_number",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        lines,
        vec![
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
    assert_eq!(budget(&pool, org).await, (0, 0), "nothing held or charged");

    // A run cut short after recording the denial and settling a line's
    // reservation, before marking any line or finishing the job, and the
    // restriction lifted meanwhile: the next run resumes the denial rather
    // than sending lines whose reservations were settled at zero.
    sqlx::query(
        "UPDATE gateway.batch_job SET state = 'pending', next_run_at = now() - interval '1 minute',
             lease_token = NULL, lease_expires_at = NULL
         WHERE organization_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE gateway.batch_line SET state = 'pending', error_code = NULL, status_code = NULL,
             finished_at = NULL
         WHERE organization_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM gateway.routing_policy WHERE organization_id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    runner.run_once(chrono::Utc::now()).await.unwrap();
    assert_eq!(calls(&fake), (0, 0), "still nothing reaches the provider");
    let state: String =
        sqlx::query_scalar("SELECT state FROM gateway.batch_job WHERE organization_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "finished");
    assert_eq!(budget(&pool, org).await, (0, 0));
}

/// Each line's usage record, trace and export names its batch and
/// `custom_id`, and every usage view filters by batch (D-021).
async fn batch_lines_name_their_batch(pool: &PgPool, org: Uuid, batch_id: &str) {
    use gateway::domain::{
        identity::OrganizationId,
        reporting::{TraceFilterInput, TraceQuery, UsageFilterInput, UsageGroupBy, UsageQuery},
        retention::ExportQuery,
        usage_analytics::{UsageSlice, UsageSliceInput},
    };
    use gateway::persistence::OutboxSink;
    let organization = OrganizationId(org);
    let reporting = gateway::persistence::reporting::ReportingStore::new(pool.clone());
    let (from, to) = (
        chrono::Utc::now() - chrono::Duration::hours(1),
        chrono::Utc::now() + chrono::Duration::hours(1),
    );
    let other = format!("batch_{}", Uuid::new_v4());
    let bare = batch_id.trim_start_matches("batch_").to_owned();

    // The dashboard's requests, by the public id or the bare UUID.
    for named in [batch_id.to_owned(), bare.clone()] {
        let slice = UsageSlice::new(
            from,
            to,
            UsageSliceInput {
                batch_ids: vec![named],
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let (events, _) = reporting
            .analytics()
            .events(organization, None, &slice, 50, None)
            .await
            .unwrap();
        let mut lines: Vec<(Option<String>, Option<String>)> = events
            .into_iter()
            .map(|event| (event.batch_id, event.custom_id))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            ["a", "b", "c"]
                .map(|id| (Some(batch_id.to_owned()), Some(id.to_owned())))
                .to_vec()
        );
    }
    let slice = UsageSlice::new(
        from,
        to,
        UsageSliceInput {
            batch_ids: vec![other.clone()],
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let (events, _) = reporting
        .analytics()
        .events(organization, None, &slice, 50, None)
        .await
        .unwrap();
    assert!(events.is_empty());

    // Traces.
    let query = TraceQuery::new(
        None,
        None,
        TraceFilterInput {
            batch_ids: vec![batch_id.to_owned()],
            ..Default::default()
        },
    )
    .unwrap();
    let (traces, _) = reporting
        .list_traces(organization, None, &query, 50, None)
        .await
        .unwrap();
    assert_eq!(traces.len(), 3);
    assert!(
        traces
            .iter()
            .all(|trace| trace.batch_id.as_deref() == Some(batch_id))
    );
    let mut custom: Vec<_> = traces.iter().filter_map(|t| t.custom_id.clone()).collect();
    custom.sort();
    assert_eq!(custom, ["a", "b", "c"]);

    // The usage report: three lines, 104 charged.
    let usage = |batch: String| {
        UsageQuery::new(
            from,
            to,
            vec![UsageGroupBy::Day],
            UsageFilterInput {
                batch_ids: vec![batch],
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };
    let totals = reporting
        .usage_totals(organization, None, &usage(batch_id.to_owned()))
        .await
        .unwrap();
    assert_eq!(totals.iter().map(|t| t.request_count).sum::<i64>(), 3);
    assert_eq!(totals.iter().map(|t| t.amount_minor).sum::<i64>(), 104);
    assert!(
        reporting
            .usage_totals(organization, None, &usage(other.clone()))
            .await
            .unwrap()
            .is_empty()
    );

    // Data exports end with batch_id, custom_id and model_master, and
    // filter by batch.
    let retention = gateway::persistence::retention::RetentionStore::new(pool.clone());
    for dataset in ["usage", "traces"] {
        let query = ExportQuery::new(dataset, None, None, None, to)
            .unwrap()
            .with_batch(Some(batch_id))
            .unwrap();
        let rows = retention
            .export_page(organization, &query, None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3, "{dataset}");
        let columns = query.dataset.columns();
        assert_eq!(
            &columns[columns.len() - 3..],
            ["batch_id", "custom_id", "model_master"]
        );
        for (_, _, values) in &rows {
            assert_eq!(values.len(), columns.len());
            assert_eq!(values[columns.len() - 3], json!(batch_id));
        }
        let query = query.with_batch(Some(&other)).unwrap();
        assert!(
            retention
                .export_page(organization, &query, None, 100)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        ExportQuery::new("audit-events", None, None, None, to)
            .unwrap()
            .with_batch(Some(batch_id))
            .is_err()
    );

    // A usage export's record of a line.
    let (trace, reservation): (Uuid, Uuid) = sqlx::query_as(
        "SELECT r.trace_id, r.id FROM gateway.accounting_reservation r
         JOIN gateway.batch_line l ON l.reservation_id = r.id
         WHERE r.organization_id = $1 AND l.custom_id = 'a'",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.usage_export
             (organization_id, id, kind, connection, bucket, enabled_at, verified_at)
         VALUES ($1, gen_random_uuid(), 'gcs', 'worker-test', 'worker-test-bucket',
                 now() - interval '1 hour', now())
         ON CONFLICT (organization_id) DO NOTHING",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let event = gateway::persistence::outbox::OutboxEvent {
        id: Uuid::new_v4(),
        sequence: 1,
        organization_id: organization,
        payload: gateway::domain::outbox::OutboxPayload::InferenceFinalized(
            gateway::domain::outbox::InferenceFinalized {
                trace_id: trace,
                reservation_id: reservation,
                team_id: None,
                service_account_id: None,
                model: "fast".into(),
                currency: "USD".into(),
                outcome: "settled".into(),
                amount_minor: Some(52),
                finalized_at: chrono::Utc::now(),
            },
        ),
        occurred_at: chrono::Utc::now(),
        attempts: 0,
    };
    let mut conn = pool.acquire().await.unwrap();
    gateway::persistence::usage_export::UsageExportSink
        .deliver(&mut conn, &event)
        .await
        .unwrap();
    let record: sqlx::types::Json<Value> = sqlx::query_scalar(
        "SELECT record::jsonb FROM gateway.usage_export_record
         WHERE organization_id = $1 AND event_id = $2",
    )
    .bind(org)
    .bind(event.id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(record.0["batch_id"], json!(batch_id));
    assert_eq!(record.0["custom_id"], "a");
}

/// A worker and an API over the test database and a fake OpenAI, with the
/// batches other tests left parked.
struct Rig {
    pool: PgPool,
    fake: Shared,
    api: String,
    worker: gateway::jobs::Worker,
    http: reqwest::Client,
    /// Organizations made before the API started, so its configuration
    /// holds their catalogs from the first load.
    tenants: Vec<(Uuid, String)>,
}

impl Rig {
    async fn new(url: &str, tenants: usize) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await
            .unwrap();
        gateway::persistence::migrations::run(&pool).await.unwrap();
        let mut made = Vec::new();
        for _ in 0..tenants {
            made.push(tenant(&pool).await);
        }
        let fake: Shared = Arc::default();
        let provider = serve(
            Router::new()
                .route("/v1/files", post(upload))
                .route("/v1/files/{id}", get(file))
                .route("/v1/files/{id}/content", get(content))
                .route("/v1/batches", post(create).get(list))
                .route("/v1/batches/{id}", get(one))
                .route("/v1/batches/{id}/cancel", post(cancel))
                .with_state(fake.clone()),
        )
        .await;
        let objects: Arc<dyn TraceObjects> = Arc::new(MemoryTraceObjects::default());
        let state = gateway::application::connect(&config(url, "api", &provider))
            .await
            .unwrap()
            .with_trace_objects(Some(objects.clone()));
        let api = serve(gateway::transport::router(state)).await;
        let mut worker = gateway::application::connect_worker(&config(url, "worker", &provider))
            .await
            .unwrap();
        worker.batches = worker.batches.with_objects(Some(objects));
        sqlx::query(
            "UPDATE gateway.batch_job SET next_run_at = 'infinity' WHERE state <> 'finished'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE gateway.batch SET lease_token = gen_random_uuid(), lease_expires_at = 'infinity'
             WHERE status IN ('in_progress', 'finalizing', 'cancelling')",
        )
        .execute(&pool)
        .await
        .unwrap();
        Self {
            pool,
            fake,
            api,
            worker,
            http: reqwest::Client::new(),
            tenants: made,
        }
    }

    /// Organization `tenant`'s batch of lines saying "hello", one per id.
    async fn batch(&self, tenant: usize, ids: &[&str]) -> (Uuid, String, String) {
        let (org, bearer) = self.tenants[tenant].clone();
        let jsonl = ids
            .iter()
            .map(|id| {
                json!({"custom_id": id, "method": "POST", "url": "/v1/chat/completions",
                       "body": {"model": "fast", "messages": [{"role": "user", "content": "hello"}],
                                "max_tokens": 10}})
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let form = format!(
            "--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
             --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
             {jsonl}\r\n--b--\r\n"
        );
        let file: Value = self
            .http
            .post(format!("{}/api/inference/v1/files", self.api))
            .bearer_auth(&bearer)
            .header("content-type", "multipart/form-data; boundary=b")
            .body(form)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let batch: Value = self
            .http
            .post(format!("{}/api/inference/v1/batches", self.api))
            .bearer_auth(&bearer)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .json(
                &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                          "completion_window": "24h"}),
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = batch["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{batch}"))
            .to_owned();
        (org, bearer, id)
    }

    async fn cancel(&self, bearer: &str, batch: &str) {
        let response = self
            .http
            .post(format!(
                "{}/api/inference/v1/batches/{batch}/cancel",
                self.api
            ))
            .bearer_auth(bearer)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }

    async fn status(&self, bearer: &str, batch: &str) -> String {
        let body: Value = self
            .http
            .get(format!("{}/api/inference/v1/batches/{batch}", self.api))
            .bearer_auth(bearer)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["status"].as_str().unwrap().to_owned()
    }

    async fn lines(&self, org: Uuid) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT l.state, r.state FROM gateway.batch_line l
             JOIN gateway.accounting_reservation r ON r.id = l.reservation_id
             WHERE l.organization_id = $1 ORDER BY l.line_number",
        )
        .bind(org)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn reconciliations(&self, org: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM gateway.accounting_reconciliation WHERE organization_id = $1",
        )
        .bind(org)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// As a new worker sees the organization's jobs after the last one died:
    /// no lease, due now.
    async fn restart(&self, org: Uuid) {
        sqlx::query(
            "UPDATE gateway.batch_job SET lease_token = NULL, lease_expires_at = NULL,
                 next_run_at = now() - interval '1 minute'
             WHERE organization_id = $1 AND state <> 'finished'",
        )
        .bind(org)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

/// The gate's wedges (D-021): lines that never reached the provider, or a
/// job cancelled while running, end at zero and release their holds, a
/// line the provider's output leaves out goes to reconciliation without
/// stopping the others, and no run is retried for a line it settled. Batch
/// prices here have a cached-input rate, as the clouds' do.
#[tokio::test]
async fn cancelled_restarted_and_incomplete_jobs_settle_and_end() {
    let _one_at_a_time = WORKER.lock().await;
    let Some(url) = database_url() else {
        return;
    };
    let rig = Rig::new(&url, 3).await;
    let runner = &rig.worker.batches;

    // A worker restarts before the provider ever took the job (its upload
    // failed), and the batch is cancelled meanwhile: its claimed lines end
    // cancelled at zero, and the batch ends with nothing held.
    let (org, bearer, batch) = rig.batch(0, &["a", "b"]).await;
    let mut now = chrono::Utc::now();
    rig.fake.lock().unwrap().refuse_uploads = true;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.retried, 1, "{report:?}");
    rig.fake.lock().unwrap().refuse_uploads = false;
    rig.cancel(&bearer, &batch).await;
    rig.restart(org).await;
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.retried, report.finalized), (0, 1), "{report:?}");
    assert_eq!(calls(&rig.fake), (0, 0), "nothing reached the provider");
    assert_eq!(rig.status(&bearer, &batch).await, "cancelled");
    assert_eq!(
        rig.lines(org).await,
        vec![
            ("cancelled".to_owned(), "settled".to_owned()),
            ("cancelled".to_owned(), "settled".to_owned())
        ]
    );
    assert_eq!(budget(&rig.pool, org).await, (0, 0));
    assert_eq!(rig.reconciliations(org).await, 0);

    // A running native job cancelled: the provider is asked to cancel, the
    // line it ran first is charged, the other ends cancelled at zero, and
    // the batch reaches `cancelled` with every hold released.
    let (org, bearer, batch) = rig.batch(1, &["a", "b"]).await;
    now = now.max(chrono::Utc::now());
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.submitted, 1, "{report:?}");
    let index = rig.fake.lock().unwrap().batches.len() - 1;
    rig.cancel(&bearer, &batch).await;
    assert_eq!(rig.status(&bearer, &batch).await, "cancelling");
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.retried, 0, "{report:?}");
    assert_eq!(
        rig.fake.lock().unwrap().batches[index]["status"],
        "cancelling"
    );
    end_job(&rig.fake, index, "cancelled", &[1]);
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!((report.collected, report.finalized), (1, 1), "{report:?}");
    assert_eq!(rig.status(&bearer, &batch).await, "cancelled");
    assert_eq!(
        rig.lines(org).await,
        vec![
            ("succeeded".to_owned(), "settled".to_owned()),
            ("cancelled".to_owned(), "settled".to_owned())
        ]
    );
    assert_eq!(budget(&rig.pool, org).await, (0, 52));

    // A completed job whose output leaves a line out: the others settle,
    // the missing one keeps its hold for reconciliation, and the batch
    // ends; the next run retries nothing.
    let (org, bearer, batch) = rig.batch(2, &["a", "b", "c"]).await;
    now = now.max(chrono::Utc::now());
    assert_eq!(runner.run_once(now).await.unwrap().submitted, 1);
    let index = rig.fake.lock().unwrap().batches.len() - 1;
    end_job(&rig.fake, index, "completed", &[2]);
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(
        (report.collected, report.finalized, report.retried),
        (1, 1, 0),
        "{report:?}"
    );
    assert_eq!(rig.status(&bearer, &batch).await, "completed");
    assert_eq!(
        rig.lines(org).await,
        vec![
            ("succeeded".to_owned(), "settled".to_owned()),
            ("succeeded".to_owned(), "settled".to_owned()),
            ("unknown".to_owned(), "unknown".to_owned())
        ]
    );
    let (held, consumed) = budget(&rig.pool, org).await;
    assert!(held > 0, "the missing line keeps its hold");
    assert_eq!(consumed, 104);
    assert_eq!(rig.reconciliations(org).await, 1);
    now += chrono::Duration::minutes(2);
    let report = runner.run_once(now).await.unwrap();
    assert_eq!(report.retried, 0, "{report:?}");
}
