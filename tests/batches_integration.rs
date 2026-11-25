//! Files and batches end to end (D-021): a service account uploads a JSONL
//! file and creates a batch from it. The whole file is validated and
//! routed, and every line reserved at the batch price for the completion
//! window, before the batch is accepted; a refused or unaffordable batch
//! holds nothing.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL`.

use std::sync::Arc;

use axum::Router;
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    OrganizationId,
    application::trace_objects::{MemoryTraceObjects, TraceObjects},
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const PEPPER: &str = "batches-test-pepper-0000000000000000";
/// What one test line holds: its input bound (99 tokens) at the batch
/// input price of 2, and its whole output cap of 10 at 6.
/// Held by tests that leave batch files unread for a while and by the one
/// that runs the global batch-file expiry, which would purge them.
static FILE_SWEEP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const LINE_HOLD: i64 = 99 * 2 + 10 * 6;

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

/// One OpenAI deployment that serves chat and chat batches. `fast` has a
/// batch route; `plain` routes chat only.
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
            "output_minor_per_token": output, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "batches-test", "source_version": "v1", "confidence": "known"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": "batches-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "gpt-batches", "location": null,
                         "adapter_instance_key": "openai-main",
                         "capabilities": [capability("chat"), capability("chat_batch"),
                                          capability("embeddings"),
                                          capability("embeddings_batch")]}],
        "prices": [price("chat", 4, 12), price("chat_batch", 2, 6),
                   price("embeddings", 1, 0), price("embeddings_batch", 1, 0)],
        "routes": [
            {"alias": "fast", "operation": "chat", "deployment": "chat-a"},
            {"alias": "fast", "operation": "chat_batch", "deployment": "chat-a"},
            {"alias": "plain", "operation": "chat", "deployment": "chat-a"},
            {"alias": "embed", "operation": "embeddings_batch", "deployment": "chat-a"}
        ]
    }))
    .unwrap()
}

struct Tenant {
    org: OrganizationId,
    user: gateway::UserAccountId,
    bearer: String,
}

/// An organization with the catalog, a budget of `limit` USD minor units
/// and an inference key.
async fn tenant(pool: &PgPool, limit: i64) -> Tenant {
    tenant_with_rate(pool, limit, 1000).await
}

async fn tenant_with_rate(pool: &PgPool, limit: i64, per_minute: i32) -> Tenant {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("batches-{suffix}"), "Batches test")
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
    let bearer = key(&identity, org, user).await;
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, scope_kind, period_start, period_end, currency, limit_minor)
         VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '1 day',
                 'USD', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(limit)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, requests_per_minute, max_concurrent)
         VALUES ($1, $2, 'organization', $3, 2)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(per_minute)
    .execute(pool)
    .await
    .unwrap();
    let manifest = manifest();
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    Tenant { org, user, bearer }
}

/// A key of a new service account in `org`.
async fn key(
    identity: &IdentityStore,
    org: OrganizationId,
    user: gateway::UserAccountId,
) -> String {
    let account = identity
        .create_service_account(
            org,
            user,
            None,
            &format!("batches-{}", Uuid::new_v4().simple()),
        )
        .await
        .unwrap()
        .id;
    identity
        .create_api_key(
            org,
            account,
            &[ApiKeyScope::Inference, ApiKeyScope::Embeddings],
            None,
            user,
        )
        .await
        .unwrap()
        .bearer
}

fn line(custom_id: &str, alias: &str) -> String {
    json!({
        "custom_id": custom_id, "method": "POST", "url": "/v1/chat/completions",
        "body": {"model": alias, "messages": [{"role": "user", "content": "hello"}],
                 "max_tokens": 10}
    })
    .to_string()
}

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    async fn upload(&self, bearer: &str, purpose: &str, content: &str) -> (u16, Value) {
        let boundary = "gateway-batches-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\
             Content-Type: application/jsonl\r\n\r\n{content}\r\n--{boundary}--\r\n"
        );
        let response = self
            .http
            .post(format!("{}/api/inference/v1/files", self.base))
            .bearer_auth(bearer)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn create(&self, bearer: &str, key: &str, body: Value) -> (u16, Value) {
        let response = self
            .http
            .post(format!("{}/api/inference/v1/batches", self.base))
            .bearer_auth(bearer)
            .header("idempotency-key", key)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, bearer: &str, path: &str) -> (u16, String) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(bearer)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn post(&self, bearer: &str, path: &str) -> (u16, Value) {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(bearer)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

async fn reservations(pool: &PgPool, org: OrganizationId) -> Vec<(String, i64, f64, bool)> {
    sqlx::query_as(
        "SELECT state, reserved_minor,
                EXTRACT(EPOCH FROM expires_at - created_at)::float8,
                batch_id IS NOT NULL
         FROM gateway.accounting_reservation WHERE organization_id = $1 ORDER BY created_at",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn reserved_in_budget(pool: &PgPool, org: OrganizationId) -> i64 {
    sqlx::query_scalar("SELECT reserved_minor FROM gateway.quota_bucket WHERE organization_id = $1")
        .bind(org.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_batch_is_validated_routed_and_reserved_whole_before_it_is_accepted() {
    let Some(url) = database_url() else {
        return;
    };
    let _sweep = FILE_SWEEP.lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let vars = [
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batches".to_owned()),
        ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let objects = Arc::new(MemoryTraceObjects::default());
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(objects.clone() as Arc<dyn TraceObjects>));
    let client = Client {
        http: reqwest::Client::new(),
        base: serve(gateway::transport::router(state)).await,
    };
    let tenant = tenant(&pool, 1_000_000).await;
    let bearer = &tenant.bearer;

    // Upload: the file is stored and read back as written.
    let content = [line("a", "fast"), line("b", "fast"), line("c", "fast")].join("\n");
    let (status, file) = client.upload(bearer, "batch", &content).await;
    assert_eq!(status, 200, "{file}");
    let file_id = file["id"].as_str().unwrap().to_owned();
    assert!(file_id.starts_with("file-"));
    assert_eq!(file["purpose"], "batch");
    assert_eq!(file["bytes"], content.len());
    let (status, body) = client
        .get(
            bearer,
            &format!("/api/inference/v1/files/{file_id}/content"),
        )
        .await;
    assert_eq!((status, body.as_str()), (200, content.as_str()));
    assert_eq!(objects.names().len(), 1);
    let (status, _) = client.upload(bearer, "fine-tune", &content).await;
    assert_eq!(status, 400, "only batch input files");

    // Create: every line is reserved at the batch price, for the window
    // plus grace, before the batch is accepted.
    let request = json!({"input_file_id": file_id, "endpoint": "/v1/chat/completions",
                         "completion_window": "24h", "metadata": {"run": "nightly"}});
    let (status, batch) = client.create(bearer, "batch-1", request.clone()).await;
    assert_eq!(status, 200, "{batch}");
    let batch_id = batch["id"].as_str().unwrap().to_owned();
    assert!(batch_id.starts_with("batch_"));
    assert_eq!(batch["status"], "in_progress");
    assert_eq!(
        batch["request_counts"],
        json!({"total": 3, "completed": 0, "failed": 0})
    );
    assert_eq!(batch["metadata"], json!({"run": "nightly"}));
    assert_eq!(batch["input_file_id"], file_id);
    let held = reservations(&pool, tenant.org).await;
    assert_eq!(held.len(), 3);
    // The message's tokenizer bound at 2 and the whole cap of 10 at 6:
    // 258 each.
    for (state, amount, ttl, in_batch) in &held {
        assert_eq!(state, "reserved");
        assert!(*in_batch);
        assert_eq!(*amount, LINE_HOLD);
        assert!((ttl - 30.0 * 3600.0).abs() < 5.0, "{ttl}");
    }
    let total: i64 = held.iter().map(|row| row.1).sum();
    assert_eq!(reserved_in_budget(&pool, tenant.org).await, total);
    let (jobs, lines): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM gateway.batch_job WHERE organization_id = $1 AND state = 'pending'),
                (SELECT count(*) FROM gateway.batch_line WHERE organization_id = $1 AND state = 'pending')",
    )
    .bind(tenant.org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (jobs, lines),
        (1, 3),
        "one provider job for the one deployment"
    );

    // The same key returns the same batch; a different request conflicts.
    let (status, replay) = client.create(bearer, "batch-1", request.clone()).await;
    assert_eq!(
        (status, replay["id"].as_str()),
        (200, Some(batch_id.as_str()))
    );
    let mut changed = request.clone();
    changed["metadata"] = json!({"run": "weekly"});
    let (status, conflict) = client.create(bearer, "batch-1", changed).await;
    assert_eq!(
        (status, conflict["code"].as_str()),
        (409, Some("idempotency_conflict"))
    );
    assert_eq!(reservations(&pool, tenant.org).await.len(), 3);

    // Every refused line is reported by number, and nothing is held.
    let bad = [line("a", "fast"), line("b", "nope"), line("c", "plain")].join("\n");
    let (_, bad_file) = client.upload(bearer, "batch", &bad).await;
    let (status, refused) = client
        .create(
            bearer,
            "batch-2",
            json!({"input_file_id": bad_file["id"], "endpoint": "/v1/chat/completions",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["code"], "batch_validation_failed");
    let errors: Vec<(i64, String)> = refused["errors"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["line"].as_i64().unwrap(),
                e["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        errors,
        vec![
            (2, "model_unavailable".to_owned()),
            (3, "model_unavailable".to_owned())
        ]
    );
    assert_eq!(reservations(&pool, tenant.org).await.len(), 3);
    let (status, wrong) = client
        .create(
            bearer,
            "batch-3",
            json!({"input_file_id": file_id, "endpoint": "/v1/embeddings",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(
        (status, wrong["code"].as_str()),
        (400, Some("batch_validation_failed"))
    );
    let (status, window) = client
        .create(
            bearer,
            "batch-4",
            json!({"input_file_id": file_id, "endpoint": "/v1/chat/completions",
                   "completion_window": "1h"}),
        )
        .await;
    assert_eq!(
        (status, window["code"].as_str()),
        (400, Some("batch_request_invalid"))
    );

    // Read, list and cancel.
    let (status, body) = client
        .get(bearer, &format!("/api/inference/v1/batches/{batch_id}"))
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, second_file) = client.upload(bearer, "batch", &line("x", "fast")).await;
    let (status, second) = client
        .create(
            bearer,
            "batch-5",
            json!({"input_file_id": second_file["id"], "endpoint": "/v1/chat/completions",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(
        status, 200,
        "lines are not counted as concurrent requests: {second}"
    );
    let (status, page) = client
        .get(bearer, "/api/inference/v1/batches?limit=1")
        .await;
    assert_eq!(status, 200, "{page}");
    let page: Value = serde_json::from_str(&page).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["id"], second["id"]);
    assert_eq!(page["has_more"], true);
    let (_, next) = client
        .get(
            bearer,
            &format!(
                "/api/inference/v1/batches?limit=1&after={}",
                second["id"].as_str().unwrap()
            ),
        )
        .await;
    let next: Value = serde_json::from_str(&next).unwrap();
    assert_eq!(next["data"][0]["id"], batch_id.as_str());
    let (status, cancelled) = client
        .post(
            bearer,
            &format!("/api/inference/v1/batches/{batch_id}/cancel"),
        )
        .await;
    assert_eq!(
        (status, cancelled["status"].as_str()),
        (200, Some("cancelling"))
    );
    let requested: bool = sqlx::query_scalar(
        "SELECT bool_and(cancel_requested) FROM gateway.batch_job
         WHERE organization_id = $1 AND batch_id = $2",
    )
    .bind(tenant.org.0)
    .bind(Uuid::parse_str(batch_id.trim_start_matches("batch_")).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(requested);

    // Another service account sees none of it.
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let other = key(&identity, tenant.org, tenant.user).await;
    let (status, _) = client
        .get(&other, &format!("/api/inference/v1/files/{file_id}"))
        .await;
    assert_eq!(status, 404);
    let (status, _) = client
        .get(&other, &format!("/api/inference/v1/batches/{batch_id}"))
        .await;
    assert_eq!(status, 404);
    // Nor can it page from another account's batch.
    let (_, page) = client
        .get(
            &other,
            &format!(
                "/api/inference/v1/batches?after={}",
                second["id"].as_str().unwrap()
            ),
        )
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&page).unwrap()["data"],
        json!([])
    );

    // Content expires with the trace content period, except a file an open
    // batch still reads; the records stay.
    let store = gateway::persistence::batches::BatchStore::new(pool.clone());
    let later = chrono::Utc::now() + chrono::Duration::days(31);
    let as_objects = objects.clone() as Arc<dyn TraceObjects>;
    // A bucket that refuses deletes purges nothing, and the run says so.
    *objects.failing.lock().unwrap() =
        Some(gateway::application::trace_objects::TraceObjectError::Status(503));
    let refused =
        gateway::jobs::trace_content::expire_batch_files(&store, Some(&as_objects), later)
            .await
            .unwrap();
    assert_eq!(refused.purged, 0);
    assert!(refused.failed > 0, "{refused:?}");
    let still_stored: bool = sqlx::query_scalar(
        "SELECT bool_and(content_purged_at IS NULL) FROM gateway.batch_file WHERE organization_id = $1",
    )
    .bind(tenant.org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(still_stored);
    *objects.failing.lock().unwrap() = None;
    let expired =
        gateway::jobs::trace_content::expire_batch_files(&store, Some(&as_objects), later)
            .await
            .unwrap();
    assert_eq!(expired.failed, 0);
    assert!(expired.purged > 0);
    let purged: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, content_purged_at IS NOT NULL FROM gateway.batch_file
         WHERE organization_id = $1 ORDER BY created_at",
    )
    .bind(tenant.org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    let file_uuid = |value: &Value| {
        Uuid::parse_str(value.as_str().unwrap().trim_start_matches("file-")).unwrap()
    };
    let purged_of = |id: Uuid| purged.iter().find(|(file, _)| *file == id).unwrap().1;
    assert!(
        !purged_of(file_uuid(&file["id"])),
        "an open batch's input is kept"
    );
    assert!(
        purged_of(file_uuid(&bad_file["id"])),
        "an unused file expires"
    );
    let (status, _) = client
        .get(
            bearer,
            &format!(
                "/api/inference/v1/files/{}/content",
                bad_file["id"].as_str().unwrap()
            ),
        )
        .await;
    assert_eq!(status, 404);
    // Nor can a batch be made from it any more.
    let (status, refused) = client
        .create(
            bearer,
            "batch-6",
            json!({"input_file_id": bad_file["id"], "endpoint": "/v1/chat/completions",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(status, 404, "{refused}");

    // Usage retention takes a batch's lines with their requests, once the
    // batch has ended, and keeps the batch.
    let batch_uuid = Uuid::parse_str(batch_id.trim_start_matches("batch_")).unwrap();
    sqlx::query(
        "INSERT INTO gateway.organization_retention_policy
             (organization_id, id, trace_days, usage_days)
         VALUES ($1, $2, 30, 30)",
    )
    .bind(tenant.org.0)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    let retention = gateway::persistence::retention::RetentionStore::new(pool.clone());
    let purge = || {
        retention.purge(
            gateway::domain::retention::RetentionKind::Usage,
            tenant.org.0,
            later,
            1000,
            Uuid::new_v4(),
        )
    };
    let lines = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.batch_line WHERE organization_id = $1 AND batch_id = $2",
        )
        .bind(tenant.org.0)
        .bind(batch_uuid)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let held = lines().await;
    assert!(held > 0);
    sqlx::query(
        "UPDATE gateway.accounting_reservation SET state = 'expired'
         WHERE organization_id = $1 AND batch_id = $2",
    )
    .bind(tenant.org.0)
    .bind(batch_uuid)
    .execute(&pool)
    .await
    .unwrap();
    purge().await.unwrap();
    assert_eq!(
        lines().await,
        held,
        "a batch still cancelling keeps its lines"
    );
    sqlx::query(
        "UPDATE gateway.batch SET status = 'cancelled' WHERE organization_id = $1 AND id = $2",
    )
    .bind(tenant.org.0)
    .bind(batch_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let purged = purge().await.unwrap();
    assert_eq!(purged.reservations, held as u64);
    assert_eq!(lines().await, 0);
    let (status, _) = client
        .get(bearer, &format!("/api/inference/v1/batches/{batch_id}"))
        .await;
    assert_eq!(status, 200, "the batch stays");
}

#[tokio::test]
async fn a_batch_is_one_request_against_the_rate_limit() {
    let Some(url) = database_url() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let vars = [
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batches".to_owned()),
        ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(
            Arc::new(MemoryTraceObjects::default()) as Arc<dyn TraceObjects>
        ));
    let client = Client {
        http: reqwest::Client::new(),
        base: serve(gateway::transport::router(state)).await,
    };
    // One request a minute: a three-line batch takes it, and the next is
    // refused.
    let tenant = tenant_with_rate(&pool, 1_000_000, 1).await;
    let content = [line("a", "fast"), line("b", "fast"), line("c", "fast")].join("\n");
    let (_, file) = client.upload(&tenant.bearer, "batch", &content).await;
    let request = json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                         "completion_window": "24h"});
    let (status, body) = client
        .create(&tenant.bearer, "first", request.clone())
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = client
        .create(&tenant.bearer, "second", request.clone())
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (429, Some("rate_limit_exceeded")),
        "{body}"
    );
    // A replay of the first is not a new admission.
    let (status, _) = client.create(&tenant.bearer, "first", request).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_batch_no_budget_covers_is_refused_with_nothing_held() {
    let Some(url) = database_url() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let vars = [
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batches".to_owned()),
        ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(
            Arc::new(MemoryTraceObjects::default()) as Arc<dyn TraceObjects>
        ));
    let client = Client {
        http: reqwest::Client::new(),
        base: serve(gateway::transport::router(state)).await,
    };
    // Enough for two lines, not three: no line is cut to fit.
    let tenant = tenant(&pool, LINE_HOLD * 2 + 10).await;
    let content = [line("a", "fast"), line("b", "fast"), line("c", "fast")].join("\n");
    let (_, file) = client.upload(&tenant.bearer, "batch", &content).await;
    let (status, refused) = client
        .create(
            &tenant.bearer,
            "too-big",
            json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (429, Some("quota_exceeded")),
        "{refused}"
    );
    assert!(reservations(&pool, tenant.org).await.is_empty());
    assert_eq!(reserved_in_budget(&pool, tenant.org).await, 0);
    let batches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.batch WHERE organization_id = $1")
            .bind(tenant.org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(batches, 0);

    // One line fits.
    let (_, small) = client
        .upload(&tenant.bearer, "batch", &line("a", "fast"))
        .await;
    let (status, accepted) = client
        .create(
            &tenant.bearer,
            "small",
            json!({"input_file_id": small["id"], "endpoint": "/v1/chat/completions",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(status, 200, "{accepted}");
}

#[tokio::test]
async fn a_batch_is_split_into_jobs_whose_results_the_worker_can_read() {
    let Some(url) = database_url() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let vars = [
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batches".to_owned()),
        ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(
            Arc::new(MemoryTraceObjects::default()) as Arc<dyn TraceObjects>
        ));
    let client = Client {
        http: reqwest::Client::new(),
        base: serve(gateway::transport::router(state)).await,
    };
    let tenant = tenant(&pool, 1_000_000).await;
    // Each line may return 128 vectors: some 10 MiB of results. Twenty such
    // lines are more than one job's results may come to.
    let embed = |id: usize| {
        json!({"custom_id": format!("e{id}"), "method": "POST", "url": "/v1/embeddings",
               "body": {"model": "embed", "input": vec!["x"; 128]}})
        .to_string()
    };
    let content = (0..20).map(embed).collect::<Vec<_>>().join("\n");
    let (_, file) = client.upload(&tenant.bearer, "batch", &content).await;
    let (status, batch) = client
        .create(
            &tenant.bearer,
            "split",
            json!({"input_file_id": file["id"], "endpoint": "/v1/embeddings",
                   "completion_window": "24h"}),
        )
        .await;
    assert_eq!(status, 200, "{batch}");
    let jobs: Vec<i64> = sqlx::query_scalar(
        "SELECT count(l.*) FROM gateway.batch_job j
         JOIN gateway.batch_line l ON l.organization_id = j.organization_id AND l.job_id = j.id
         WHERE j.organization_id = $1 GROUP BY j.ordinal ORDER BY j.ordinal",
    )
    .bind(tenant.org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(jobs.len() >= 2, "{jobs:?}");
    assert_eq!(jobs.iter().sum::<i64>(), 20);
}

/// Makes the key's service account limit `user` to credits of `credits`
/// and gives the key itself a budget of `key_limit`.
async fn end_user_with_credits_and_key_budget(
    pool: &PgPool,
    org: OrganizationId,
    user: &str,
    credits: i64,
    key_limit: i64,
) {
    let (account, key): (Uuid, Uuid) = sqlx::query_as(
        "SELECT k.service_account_id, k.id FROM gateway.api_key k WHERE k.organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .unwrap();
    let plan = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.end_user_setting (service_account_id, organization_id, id, currency)
         VALUES ($1, $2, $3, 'USD')",
    )
    .bind(account)
    .bind(org.0)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.end_user_plan (id, organization_id, service_account_id, name, allowance_minor)
         VALUES ($1, $2, $3, 'credits-only', 0)",
    )
    .bind(plan)
    .bind(org.0)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    let end_user: Uuid = sqlx::query_scalar(
        "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id, plan_id)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(account)
    .bind(user)
    .bind(plan)
    .fetch_one(pool)
    .await
    .unwrap();
    let (grant, bucket) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, service_account_id, scope_kind, end_user_id, credit_grant_id,
              period_start, period_end, currency, limit_minor)
         VALUES ($1, $2, $3, 'end-user', $4, $5, now() - interval '1 second', $6, 'USD', $7)",
    )
    .bind(bucket)
    .bind(org.0)
    .bind(account)
    .bind(end_user)
    .bind(grant)
    .bind(gateway::domain::end_user::never_expires())
    .bind(credits)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.end_user_credit_grant
             (id, organization_id, service_account_id, end_user_id, reference, amount_minor,
              currency, quota_bucket_id)
         VALUES ($1, $2, $3, $4, 'order-1', $5, 'USD', $6)",
    )
    .bind(grant)
    .bind(org.0)
    .bind(account)
    .bind(end_user)
    .bind(credits)
    .bind(bucket)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, api_key_id, scope_kind, period_start, period_end, currency,
              limit_minor)
         VALUES ($1, $2, $3, 'api-key', now() - interval '1 day', now() + interval '1 day', 'USD', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(key)
    .bind(key_limit)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_batch_the_key_budget_blocks_is_not_blamed_on_the_users_credits() {
    let Some(url) = database_url() else {
        return;
    };
    let _sweep = FILE_SWEEP.lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let vars = [
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batches".to_owned()),
        ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(
            Arc::new(MemoryTraceObjects::default()) as Arc<dyn TraceObjects>
        ));
    let client = reqwest::Client::new();
    let base = serve(gateway::transport::router(state)).await;
    let create = |bearer: String, key: &'static str, file: Value, user: &'static str| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let response = client
                .post(format!("{base}/api/inference/v1/batches"))
                .bearer_auth(bearer)
                .header("idempotency-key", key)
                .header("gateway-end-user", user)
                .json(
                    &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                              "completion_window": "24h"}),
                )
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            (status, response.json::<Value>().await.unwrap())
        }
    };
    let content = [line("a", "fast"), line("b", "fast")].join("\n");
    let upload = Client {
        http: client.clone(),
        base: base.clone(),
    };

    // Three apps: credits that cover one of the two lines and a key with no
    // budget; credits that cover both and the same key; credits that cover
    // one and a key with room.
    let mut files = Vec::new();
    let mut bearers = Vec::new();
    for (credits, key_limit) in [
        (LINE_HOLD, 0),
        (LINE_HOLD * 3, 0),
        (LINE_HOLD, LINE_HOLD * 10),
    ] {
        let tenant = tenant(&pool, LINE_HOLD * 100).await;
        end_user_with_credits_and_key_budget(&pool, tenant.org, "u-1", credits, key_limit).await;
        let (status, file) = upload.upload(&tenant.bearer, "batch", &content).await;
        assert_eq!(status, 200, "{file}");
        files.push(file);
        bearers.push(tenant.bearer);
    }
    // The configuration cache refreshes on a miss at most once a second;
    // let it see the three new catalogs.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    // Buying credits would not help, so the refusal does not name the user.
    let (status, refused) = create(bearers[0].clone(), "short", files[0].clone(), "u-1").await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (429, Some("quota_exceeded")),
        "{refused}"
    );
    assert!(refused.get("end_user").is_none(), "{refused}");

    // Only the key's budget refuses it.
    let (status, refused) = create(bearers[1].clone(), "key", files[1].clone(), "u-1").await;
    assert_eq!(status, 429, "{refused}");
    assert!(refused.get("end_user").is_none(), "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("API key's budget"),
        "{refused}"
    );

    // The user's credits are what run out.
    let (status, refused) = create(bearers[2].clone(), "user", files[2].clone(), "u-1").await;
    assert_eq!(status, 429, "{refused}");
    assert_eq!(refused["end_user"], "u-1", "{refused}");
}

#[tokio::test]
async fn a_batch_is_one_request_against_its_end_users_plan_limit() {
    let Some(url) = database_url() else {
        return;
    };
    let _sweep = FILE_SWEEP.lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let vars = [
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-batches".to_owned()),
        ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(
            Arc::new(MemoryTraceObjects::default()) as Arc<dyn TraceObjects>
        ));
    let client = Client {
        http: reqwest::Client::new(),
        base: serve(gateway::transport::router(state)).await,
    };
    // The user's plan allows one request a minute; the service account's
    // policy allows plenty.
    let tenant = tenant(&pool, LINE_HOLD * 100).await;
    end_user_with_credits_and_key_budget(&pool, tenant.org, "u-1", LINE_HOLD * 20, LINE_HOLD * 100)
        .await;
    sqlx::query(
        "UPDATE gateway.end_user_plan SET requests_per_minute = 1 WHERE organization_id = $1",
    )
    .bind(tenant.org.0)
    .execute(&pool)
    .await
    .unwrap();
    let content = [line("a", "fast"), line("b", "fast"), line("c", "fast")].join("\n");
    let (status, file) = client.upload(&tenant.bearer, "batch", &content).await;
    assert_eq!(status, 200, "{file}");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let create = |key: &'static str| {
        let request = client
            .http
            .post(format!("{}/api/inference/v1/batches", client.base))
            .bearer_auth(&tenant.bearer)
            .header("idempotency-key", key)
            .header("gateway-end-user", "u-1")
            .json(
                &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                          "completion_window": "24h"}),
            );
        async move { request.send().await.unwrap() }
    };
    let first = create("first").await;
    assert_eq!(first.status(), 200, "{}", first.text().await.unwrap());
    // Three lines were one request: the next batch is the user's second.
    let second = create("second").await;
    assert_eq!(second.status(), 429);
    let retry_after: u32 = second.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["code"], "rate_limit_exceeded", "{body}");
    assert_eq!(body["end_user"], "u-1", "{body}");
    assert_eq!(body["limit"], "requests_per_minute", "{body}");
    // A replay of the first is not a new admission.
    assert_eq!(create("first").await.status(), 200);
}
