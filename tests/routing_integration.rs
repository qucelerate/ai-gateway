//! Routing across deployments (D-023, Phase 9 milestones 1 and 2): one alias
//! routed to several deployments splits its traffic by weight, prefers its
//! lowest priority while it is healthy, and skips a deployment whose circuit
//! is open or whose availability evidence refuses it. Each reservation names
//! the deployment that served it.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL` and `GATEWAY_TEST_DATA_PLANE_DATABASE_URL`
//! (as `data_plane_integration` does).

use axum::{Json, Router, body::Body, http::StatusCode, response::Response, routing::post};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    OrganizationId,
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const PEPPER: &str = "routing-test-pepper-0000000000000000";

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

/// OpenAI chat completions; the model `gpt-broken` is unavailable (503).
async fn provider(Json(body): Json<Value>) -> Response {
    if body["model"] == "gpt-broken" {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"error": {"message": "down", "type": "server_error"}}).to_string(),
            ))
            .unwrap();
    }
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": "c", "model": body["model"],
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 7},
            })
            .to_string(),
        ))
        .unwrap()
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

/// `spread` sends three quarters of its chat to `chat-a` and a quarter to
/// `chat-b`; `fallback` prefers `chat-broken` (priority 1) over `chat-b`
/// (priority 2).
fn manifest() -> CatalogManifest {
    let deployment = |name: &str, model: &str, max_output_tokens: u32| {
        json!({
            "name": name, "model": model, "location": null, "adapter_instance_key": "openai-main",
            "capabilities": [{
                "operation": "chat", "max_input_bytes": 4096,
                "max_output_tokens": max_output_tokens, "max_tokens_per_byte": 1,
                "tokenizer": "configured", "reservation_ttl_seconds": 60,
                "chat_input_count_mode": "unsupported"
            }]
        })
    };
    let price = |deployment: &str, input: i64| {
        json!({
            "deployment": deployment, "operation": "chat", "input_minor_per_token": input,
            "output_minor_per_token": 11, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "routing-test", "source_version": "v1",
            // Deployments of one route may differ in confidence; each
            // reservation carries its own deployment's.
            "confidence": if deployment == "chat-b" { "conservative" } else { "known" }
        })
    };
    serde_json::from_value(json!({
        "release": {"version": "routing-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [
            deployment("chat-a", "gpt-a", 64),
            deployment("chat-b", "gpt-b", 32),
            deployment("chat-broken", "gpt-broken", 64)
        ],
        "prices": [price("chat-a", 3), price("chat-b", 4), price("chat-broken", 3)],
        "routes": [
            {"alias": "spread", "operation": "chat", "deployment": "chat-a", "weight": 3},
            {"alias": "spread", "operation": "chat", "deployment": "chat-b"},
            {"alias": "fallback", "operation": "chat", "deployment": "chat-broken"},
            {"alias": "fallback", "operation": "chat", "deployment": "chat-b", "priority": 2}
        ]
    }))
    .unwrap()
}

async fn seed(pool: &PgPool) -> (OrganizationId, String) {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("routing-{suffix}"), "Routing test")
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
        .create_service_account(org, user, None, "routing")
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
         VALUES ($1, $2, 'organization', 100000, 50)",
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
    (org, bearer)
}

async fn chat(client: &reqwest::Client, base: &str, bearer: &str, model: &str) -> StatusCode {
    chat_as(client, base, bearer, model, &Uuid::new_v4().to_string()).await
}

async fn chat_as(
    client: &reqwest::Client,
    base: &str,
    bearer: &str,
    model: &str,
    idempotency_key: &str,
) -> StatusCode {
    let response = client
        .post(format!("{base}/api/inference/v1/chat/completions"))
        .bearer_auth(bearer)
        .header("idempotency-key", idempotency_key)
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 16
        }))
        .send()
        .await
        .unwrap();
    StatusCode::from_u16(response.status().as_u16()).unwrap()
}

/// Reservations of `org` by the deployment that served them, as
/// `(deployment name, count)`.
async fn served(pool: &PgPool, org: OrganizationId) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT d.name, count(*)
         FROM gateway.accounting_reservation r
         JOIN gateway.catalog_deployment d
           ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
          AND d.id = r.catalog_deployment_id
         WHERE r.organization_id = $1
         GROUP BY d.name ORDER BY d.name",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn an_alias_spreads_by_weight_and_skips_unhealthy_deployments() {
    let Some((app_url, data_plane_url)) = database_urls() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let (org, bearer) = seed(&pool).await;
    // A second organization whose evidence says `gpt-a` is refused.
    let (refusing, refusing_bearer) = seed(&pool).await;
    sqlx::query(
        "INSERT INTO gateway.catalog_model_evidence
             (organization_id, adapter_instance_key, family, model, state, reason, source,
              observed_at, refused_since)
         VALUES ($1, 'openai-main', 'openai', 'gpt-a', 'refused', 'access_denied', 'probe',
                 now(), now())",
    )
    .bind(refusing.0)
    .execute(&pool)
    .await
    .unwrap();

    // Weighted routes reach data planes in their configuration version.
    let document: Value = sqlx::query_scalar(
        "SELECT v.document FROM gateway.config_version v
         WHERE v.organization_id = $1 ORDER BY v.created_at DESC LIMIT 1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let shares: Vec<(String, Value, Value)> = document["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| {
            (
                format!(
                    "{} {}",
                    route["alias"].as_str().unwrap(),
                    route["deployment"].as_str().unwrap()
                ),
                route["weight"].clone(),
                route["priority"].clone(),
            )
        })
        .collect();
    assert!(
        shares.contains(&("spread chat-a".into(), json!(3), Value::Null)),
        "{shares:?}"
    );
    assert!(
        shares.contains(&("spread chat-b".into(), Value::Null, Value::Null)),
        "{shares:?}"
    );
    assert!(
        shares.contains(&("fallback chat-b".into(), Value::Null, json!(2))),
        "{shares:?}"
    );

    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let vars = [
        ("DATABASE_URL", data_plane_url.clone()),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-routing".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
        (
            "GATEWAY_INSTANCE_ID",
            format!("routing-test-{}", Uuid::new_v4().simple()),
        ),
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

    // By weight: 3 to 1. Sixty requests at 3:1 put chat-a outside 30..=55
    // with odds far below one in a million.
    for _ in 0..60 {
        assert_eq!(
            chat(&client, &base, &bearer, "spread").await,
            StatusCode::OK
        );
    }
    let counts = served(&pool, org).await;
    let count = |name: &str| {
        counts
            .iter()
            .find(|(deployment, _)| deployment == name)
            .map_or(0, |(_, count)| *count)
    };
    assert_eq!(count("chat-a") + count("chat-b"), 60, "{counts:?}");
    assert!((30..=55).contains(&count("chat-a")), "{counts:?}");

    // A retry under the same idempotency key goes where the first attempt
    // went, so it is the same request (a finished one is reported terminal)
    // rather than a different one under a reused key.
    let key = Uuid::new_v4().to_string();
    assert_eq!(
        chat_as(&client, &base, &bearer, "spread", &key).await,
        StatusCode::OK
    );
    for _ in 0..10 {
        let retry = client
            .post(format!("{base}/api/inference/v1/chat/completions"))
            .bearer_auth(&bearer)
            .header("idempotency-key", &key)
            .json(
                &json!({"model": "spread", "messages": [{"role": "user", "content": "hello"}],
                          "max_tokens": 16}),
            )
            .send()
            .await
            .unwrap();
        let body: Value = retry.json().await.unwrap();
        assert_eq!(body["code"], "idempotency_terminal", "{body}");
    }

    // Two attempts sent at once under one key, before either has reserved,
    // pick the same deployment: one is served, the other replays it or
    // waits, and neither is refused as a different request.
    for _ in 0..10 {
        let key = Uuid::new_v4().to_string();
        let send = || {
            client
                .post(format!("{base}/api/inference/v1/chat/completions"))
                .bearer_auth(&bearer)
                .header("idempotency-key", &key)
                .json(&json!({"model": "spread",
                              "messages": [{"role": "user", "content": "hello"}],
                              "max_tokens": 16}))
                .send()
        };
        let (first, second) = tokio::join!(send(), send());
        for response in [first.unwrap(), second.unwrap()] {
            let body = response.text().await.unwrap();
            assert!(!body.contains("idempotency_conflict"), "{body}");
        }
    }

    // Every deployment of a route answers within its tightest cap: chat-b's
    // 32 output tokens bound requests that reach chat-a too.
    for _ in 0..4 {
        let response = client
            .post(format!("{base}/api/inference/v1/chat/completions"))
            .bearer_auth(&bearer)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .json(
                &json!({"model": "spread", "messages": [{"role": "user", "content": "hi"}],
                          "max_tokens": 48}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    let caps: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT output_token_cap::BIGINT FROM gateway.accounting_reservation
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(caps.iter().copied().max(), Some(32), "{caps:?}");
    let counts = served(&pool, org).await;
    let count = |name: &str| {
        counts
            .iter()
            .find(|(deployment, _)| deployment == name)
            .map_or(0, |(_, count)| *count)
    };

    // Priority: chat-broken serves `fallback` until its circuit opens (five
    // failures; failover itself is milestone 3), then chat-b does.
    let mut failures = 0;
    for _ in 0..5 {
        if chat(&client, &base, &bearer, "fallback").await != StatusCode::OK {
            failures += 1;
        }
    }
    assert_eq!(failures, 5);
    for _ in 0..10 {
        assert_eq!(
            chat(&client, &base, &bearer, "fallback").await,
            StatusCode::OK
        );
    }
    let after = served(&pool, org).await;
    let b_now = after
        .iter()
        .find(|(deployment, _)| deployment == "chat-b")
        .map_or(0, |(_, count)| *count);
    assert_eq!(b_now, count("chat-b") + 10, "{after:?}");

    // Evidence: the refusing organization never reaches chat-a.
    for _ in 0..12 {
        assert_eq!(
            chat(&client, &base, &refusing_bearer, "spread").await,
            StatusCode::OK
        );
    }
    assert_eq!(
        served(&pool, refusing).await,
        vec![("chat-b".to_owned(), 12)]
    );

    // A routed request names its end user (D-028) like any other: the
    // reservation of whichever deployment serves it records them.
    let response = client
        .post(format!("{base}/api/inference/v1/chat/completions"))
        .bearer_auth(&bearer)
        .header("idempotency-key", Uuid::new_v4().to_string())
        .header("gateway-end-user", "routed-user")
        .json(
            &json!({"model": "spread", "messages": [{"role": "user", "content": "hi"}],
                      "max_tokens": 16}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let named: (String, String) = sqlx::query_as(
        "SELECT u.external_id, r.deployment FROM gateway.accounting_reservation r
         JOIN gateway.end_user u ON u.id = r.end_user_id
         WHERE r.organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(named.0, "routed-user");
    assert!(
        ["chat-a", "chat-b"].contains(&named.1.as_str()),
        "{named:?}"
    );
}
