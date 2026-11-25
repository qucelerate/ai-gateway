//! The `data-plane` profile end to end: a gateway connected as the
//! least-privilege `gateway_data_plane` role serves chat and streaming chat
//! against a fake OpenAI provider, reserving and settling in the shared
//! database, while the catalog, price overrides and identities are written
//! through the application role. It routes only from configuration
//! versions, picks up new ones from the feed, serves no admin API, and its
//! role cannot read admin data.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL` (the application role) and
//! `GATEWAY_TEST_DATA_PLANE_DATABASE_URL` (the `gateway_data_plane` role on
//! the same database; see `docker/postgres/002-create-gateway-data-plane.sh`).

use std::time::Duration;

use axum::{Json, Router, body::Body, response::Response, routing::post};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    OrganizationId,
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const PEPPER: &str = "data-plane-test-pepper-000000000000";

fn database_urls() -> Option<(String, String)> {
    let app = std::env::var("GATEWAY_TEST_DATABASE_URL").ok();
    let data_plane = std::env::var("GATEWAY_TEST_DATA_PLANE_DATABASE_URL").ok();
    match (app, data_plane) {
        (Some(app), Some(data_plane)) => Some((app, data_plane)),
        _ if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!(
                "GATEWAY_TEST_DATABASE_URL and GATEWAY_TEST_DATA_PLANE_DATABASE_URL must be set in CI"
            )
        }
        _ => None,
    }
}

/// OpenAI chat completions, plain or streamed, reporting 5 input and 7
/// output tokens.
async fn provider(Json(body): Json<Value>) -> Response {
    let usage = json!({"prompt_tokens": 5, "completion_tokens": 7});
    if body["stream"] == true {
        let chunk = |value: Value| format!("data: {value}\n\n");
        let text = [
            chunk(json!({"id": "c", "model": "m", "choices": [
                {"index": 0, "delta": {"content": "hello"}, "finish_reason": null}]})),
            chunk(json!({"id": "c", "model": "m", "choices": [
                {"index": 0, "delta": {}, "finish_reason": "stop"}]})),
            chunk(json!({"id": "c", "model": "m", "choices": [], "usage": usage})),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(text))
            .unwrap()
    } else {
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "id": "c", "model": "m",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"},
                                 "finish_reason": "stop"}],
                    "usage": usage,
                })
                .to_string(),
            ))
            .unwrap()
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn manifest(version: &str, alias: &str) -> CatalogManifest {
    let capability = |operation: &str| {
        json!({
            "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 64,
            "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 60,
            "chat_input_count_mode": "unsupported"
        })
    };
    let price = |operation: &str| {
        json!({
            "deployment": "chat-a", "operation": operation, "input_minor_per_token": 3,
            "output_minor_per_token": 11, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "data-plane-test", "source_version": "v1", "confidence": "known"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": version},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "gpt-data-plane", "location": null,
                         "adapter_instance_key": "openai-main",
                         "capabilities": [capability("chat"), capability("chat_stream"),
                                          capability("chat_batch")]}],
        "prices": [price("chat"), price("chat_stream"), price("chat_batch")],
        "routes": [
            {"alias": alias, "operation": "chat", "deployment": "chat-a"},
            {"alias": alias, "operation": "chat_stream", "deployment": "chat-a"},
            {"alias": alias, "operation": "chat_batch", "deployment": "chat-a"}
        ]
    }))
    .unwrap()
}

async fn apply(pool: &PgPool, org: OrganizationId, manifest: &CatalogManifest) {
    CatalogStore::new(pool.clone())
        .apply(org, manifest, manifest.digest().unwrap())
        .await
        .unwrap();
}

/// An organization with a budget, a rate limit and an inference key,
/// written through the application role.
async fn seed(pool: &PgPool) -> (OrganizationId, String) {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("data-plane-{suffix}"), "Data plane test")
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
        .create_service_account(org, user, None, "data-plane")
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
         VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '1 day',
                 'USD', 1000000000000)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, requests_per_minute, max_concurrent)
         VALUES ($1, $2, 'organization', 1000, 10)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    (org, bearer)
}

async fn chat(
    client: &reqwest::Client,
    base: &str,
    bearer: &str,
    model: &str,
    stream: bool,
) -> (reqwest::StatusCode, String) {
    let path = if stream {
        "/api/inference/v1/chat/completions/stream"
    } else {
        "/api/inference/v1/chat/completions"
    };
    let response = client
        .post(format!("{base}{path}"))
        .bearer_auth(bearer)
        .header("idempotency-key", Uuid::new_v4().to_string())
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 16
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

/// The override the organization's newest reservation was priced with.
async fn last_override(pool: &PgPool, org: OrganizationId) -> Option<Uuid> {
    sqlx::query_scalar(
        "SELECT catalog_price_override_id FROM gateway.accounting_reservation
         WHERE organization_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_data_plane_serves_inference_as_its_own_role() {
    let Some((app_url, data_plane_url)) = database_urls() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .unwrap();
    // Migrating also grants the data plane role its access.
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let (org, bearer) = seed(&pool).await;
    apply(&pool, org, &manifest("dp-1", "chat")).await;

    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let instance_id = format!("data-plane-test-{}", Uuid::new_v4().simple());
    let vars = [
        ("DATABASE_URL", data_plane_url.clone()),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-data-plane".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
        ("GATEWAY_INSTANCE_ID", instance_id.clone()),
        ("GATEWAY_REGION", "test-region".to_owned()),
        ("GATEWAY_CONFIG_POLL_SECONDS", "1".to_owned()),
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
        .with_trace_objects(Some(std::sync::Arc::new(
            gateway::application::trace_objects::MemoryTraceObjects::default(),
        )));
    let base = serve(gateway::transport::data_plane_router(state)).await;
    let client = reqwest::Client::new();

    let ready = client.get(format!("{base}/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 200);

    // Files and batches are inference too: the data plane role stores the
    // file, writes the batch and reserves its lines.
    let line = json!({"custom_id": "a", "method": "POST", "url": "/v1/chat/completions",
                      "body": {"model": "chat", "messages": [{"role": "user", "content": "hi"}],
                               "max_tokens": 8}});
    let form = format!(
        "--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
         --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
         {line}\r\n--b--\r\n"
    );
    let file: Value = client
        .post(format!("{base}/api/inference/v1/files"))
        .bearer_auth(&bearer)
        .header("content-type", "multipart/form-data; boundary=b")
        .body(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batch = client
        .post(format!("{base}/api/inference/v1/batches"))
        .bearer_auth(&bearer)
        .header("idempotency-key", Uuid::new_v4().to_string())
        .json(
            &json!({"input_file_id": file["id"], "endpoint": "/v1/chat/completions",
                      "completion_window": "24h"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(batch.status(), 200, "{}", batch.text().await.unwrap());

    // Chat and streaming chat reserve and settle as the data plane role.
    let (status, body) = chat(&client, &base, &bearer, "chat", false).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = chat(&client, &base, &bearer, "chat", true).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("event: response.completed"), "{body}");
    let settled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND state = 'settled'",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settled, 2);

    // What the traffic showed about the model is recorded as the data plane
    // role, off the request path (D-018).
    let mut evidence = None;
    for _ in 0..50 {
        evidence = sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT state, source, deployment FROM gateway.catalog_model_evidence
             WHERE organization_id = $1 AND adapter_instance_key = 'openai-main'
               AND model = 'gpt-data-plane'",
        )
        .bind(org.0)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if evidence.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        evidence,
        Some((
            "verified".to_owned(),
            "traffic".to_owned(),
            Some("chat-a".to_owned())
        ))
    );

    // A release activated through the application role is served at once:
    // the unknown alias makes the process read the feed.
    apply(&pool, org, &manifest("dp-2", "chat-next")).await;
    let (status, body) = chat(&client, &base, &bearer, "chat-next", false).await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = chat(&client, &base, &bearer, "chat", false).await;
    assert_eq!(status, 404, "the old alias left with its release");

    // A price override reaches it through the feed within a poll.
    let override_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.catalog_price_override
             (id, organization_id, deployment, operation, input_minor_per_token,
              output_minor_per_token, cached_input_minor_per_token, reasoning_minor_per_token,
              other_minor_per_token, currency, confidence, version, reason, effective_from)
         VALUES ($1, $2, 'chat-a', 'chat', 2, 9, 0, 0, 0, 'USD', 'known', $3, 'negotiated',
                 now() - interval '1 minute')",
    )
    .bind(override_id)
    .bind(org.0)
    .bind(format!("override:{override_id}"))
    .execute(&pool)
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let (status, body) = chat(&client, &base, &bearer, "chat-next", false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(last_override(&pool, org).await, Some(override_id));

    // It reports what it applied.
    let (profile, region, applied): (String, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT profile, region, applied_sequence FROM gateway.gateway_instance
         WHERE instance_id = $1",
    )
    .bind(&instance_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (profile.as_str(), region.as_deref()),
        ("data-plane", Some("test-region"))
    );
    let head: i64 = sqlx::query_scalar("SELECT max(sequence) FROM gateway.config_feed")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(applied.is_some_and(|applied| applied <= head));

    // No admin API, and no admin data for its role.
    let me = client
        .get(format!("{base}/api/admin/v1/me"))
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 404);
    let data_plane = PgPoolOptions::new()
        .max_connections(1)
        .connect(&data_plane_url)
        .await
        .unwrap();
    for statement in [
        "SELECT 1 FROM gateway.session LIMIT 1",
        "SELECT 1 FROM gateway.audit_event LIMIT 1",
        "SELECT 1 FROM gateway.catalog_route LIMIT 1",
        "SELECT 1 FROM gateway.webhook_endpoint LIMIT 1",
    ] {
        let error = sqlx::query(statement)
            .execute(&data_plane)
            .await
            .expect_err(statement);
        assert!(
            error.to_string().contains("permission denied"),
            "{statement}: {error}"
        );
    }
    let error = sqlx::query("UPDATE gateway.config_feed SET kind = 'unpin'")
        .execute(&data_plane)
        .await
        .expect_err("the feed is not the data plane's to write");
    assert!(error.to_string().contains("permission denied"), "{error}");
}
