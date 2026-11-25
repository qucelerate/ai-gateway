//! An organization's routing policy (D-023, Phase 9): the provider families
//! and locations its requests may be sent to. Set through the admin API, it
//! reaches data planes in the configuration version it publishes; a route
//! keeps only the deployments it permits, and an alias with none left is
//! refused with `routing_policy_denied`, never sent somewhere else.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL` and `GATEWAY_TEST_DATA_PLANE_DATABASE_URL`
//! (as `routing_integration` does).

use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    http::{Method, StatusCode, header::COOKIE},
    response::Response,
    routing::post,
};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        identity::{ApiKeyScope, MembershipRole},
    },
    persistence::{AdminStore, AuthStore, routing_policy::RoutingPolicyStore},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

const PEPPER: &str = "routing-policy-test-pepper-00000000000";
const HMAC_KEY: &[u8] = b"routing-policy-integration-hmac-1";
const SESSION_COOKIE: &str = "__Host-gateway_session";
const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";

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

/// OpenAI chat completions that always succeed.
async fn provider(Json(body): Json<Value>) -> Response {
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

/// `both` splits between `chat-us` (us-east-1) and `chat-eu` (eu-west-1);
/// `eu` routes to `chat-eu` alone and `nowhere` to `chat-nowhere`, which
/// has no location.
fn manifest() -> CatalogManifest {
    let deployment = |name: &str, model: &str, location: Value| {
        json!({
            "name": name, "model": model, "location": location,
            "adapter_instance_key": "openai-main",
            "capabilities": [{
                "operation": "chat", "max_input_bytes": 4096, "max_output_tokens": 64,
                "max_tokens_per_byte": 1, "tokenizer": "configured",
                "reservation_ttl_seconds": 60, "chat_input_count_mode": "unsupported"
            }]
        })
    };
    let price = |deployment: &str| {
        json!({
            "deployment": deployment, "operation": "chat", "input_minor_per_token": 3,
            "output_minor_per_token": 11, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "routing-policy-test", "source_version": "v1", "confidence": "known"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": "routing-policy-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [
            deployment("chat-us", "gpt-us", json!("us-east-1")),
            deployment("chat-eu", "gpt-eu", json!("eu-west-1")),
            deployment("chat-nowhere", "gpt-nowhere", Value::Null)
        ],
        "prices": [price("chat-us"), price("chat-eu"), price("chat-nowhere")],
        "routes": [
            {"alias": "both", "operation": "chat", "deployment": "chat-us"},
            {"alias": "both", "operation": "chat", "deployment": "chat-eu"},
            {"alias": "eu", "operation": "chat", "deployment": "chat-eu"},
            {"alias": "nowhere", "operation": "chat", "deployment": "chat-nowhere"}
        ]
    }))
    .unwrap()
}

struct Seeded {
    org: OrganizationId,
    bearer: String,
    admin: UserAccountId,
    reader: UserAccountId,
}

async fn seed(pool: &PgPool) -> Seeded {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("routing-policy-{suffix}"), "Routing policy test")
        .await
        .unwrap()
        .id;
    let user = |role: &'static str| {
        let identity = identity.clone();
        async move {
            let user = identity
                .create_user(org, None, None, None, Some(role))
                .await
                .unwrap()
                .id;
            identity
                .add_organization_membership(org, user, MembershipRole::Member)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO gateway.admin_role_binding
                     (id, organization_id, user_id, team_id, role, enabled)
                 VALUES ($1, $2, $3, NULL, $4, true)",
            )
            .bind(Uuid::new_v4())
            .bind(org.0)
            .bind(user.0)
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
            user
        }
    };
    let admin = user("organization_admin").await;
    let reader = user("read_only_member").await;
    let owner = identity
        .create_user(org, None, None, None, Some("organization_owner"))
        .await
        .unwrap()
        .id;
    identity
        .add_organization_membership(org, owner, MembershipRole::Owner)
        .await
        .unwrap();
    let account = identity
        .create_service_account(org, owner, None, "routing-policy")
        .await
        .unwrap()
        .id;
    let bearer = identity
        .create_api_key(org, account, &[ApiKeyScope::Inference], None, owner)
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
    Seeded {
        org,
        bearer,
        admin,
        reader,
    }
}

async fn session(pool: &PgPool, crypto: &AuthCrypto, user: UserAccountId) -> (String, String) {
    let browser = crypto.opaque_token().as_str().to_owned();
    let session = crypto.new_session(user, &browser, Duration::from_secs(3600));
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
    .execute(pool)
    .await
    .unwrap();
    (session.token.clone(), browser)
}

/// An admin request; a write sends `if_match` and a fresh idempotency key.
async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    session: &(String, String),
    if_match: &str,
    body: Option<Value>,
) -> (StatusCode, Option<String>, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method.clone())
        .uri(uri)
        .header(
            COOKIE,
            format!(
                "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                session.0, session.1
            ),
        );
    if method != Method::GET {
        builder = builder
            .header("idempotency-key", Uuid::new_v4().to_string())
            .header("if-match", if_match)
            .header("origin", "https://gateway.example");
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A chat request's status and problem code, if any.
async fn chat(
    client: &reqwest::Client,
    base: &str,
    bearer: &str,
    model: &str,
) -> (u16, Option<String>) {
    chat_as(client, base, bearer, model, &Uuid::new_v4().to_string()).await
}

async fn chat_as(
    client: &reqwest::Client,
    base: &str,
    bearer: &str,
    model: &str,
    idempotency_key: &str,
) -> (u16, Option<String>) {
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
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    (status, body["code"].as_str().map(str::to_owned))
}

/// Sends `model` until it answers `status` (the data plane polls its
/// configuration every second), at most for 20 seconds.
async fn until(
    client: &reqwest::Client,
    base: &str,
    bearer: &str,
    model: &str,
    status: u16,
) -> (u16, Option<String>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let answer = chat(client, base, bearer, model).await;
        if answer.0 == status || tokio::time::Instant::now() > deadline {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn latest_document(pool: &PgPool, org: OrganizationId) -> Value {
    sqlx::query_scalar(
        "SELECT v.document FROM gateway.config_feed f
         JOIN gateway.config_version v
           ON v.organization_id = f.organization_id AND v.id = f.version_id
         WHERE f.organization_id = $1 AND f.kind = 'version'
         ORDER BY f.sequence DESC LIMIT 1",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Reservations of `org` by the deployment that served them.
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
async fn requests_reach_only_the_deployments_the_routing_policy_permits() {
    let Some((app_url, data_plane_url)) = database_urls() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let seeded = seed(&pool).await;
    let org = seeded.org;
    let crypto = AuthCrypto::new(HMAC_KEY, &[41_u8; 32]).unwrap();
    let admin = session(&pool, &crypto, seeded.admin).await;
    let reader = session(&pool, &crypto, seeded.reader).await;
    let router = gateway::transport::routing_policy::router(
        AuthStore::new(pool.clone()),
        AdminStore::new(pool.clone()),
        Some(crypto.clone()),
        RoutingPolicyStore::new(pool.clone()),
    );
    let path = format!(
        "/api/admin/v1/organizations/{}/catalog/routing-policy",
        org.0
    );

    // Never set: any deployment, version 0, no ETag; and the configuration
    // version carries neither the policy nor locations.
    let (status, etag, policy) = send(&router, Method::GET, &path, &reader, "", None).await;
    assert_eq!(status, StatusCode::OK, "{policy}");
    assert_eq!(etag, None);
    assert_eq!(
        policy,
        json!({"providers": null, "locations": null, "version": 0, "updated_at": null})
    );
    let document = latest_document(&pool, org).await;
    assert!(document.get("routing_policy").is_none(), "{document}");
    assert!(
        document["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|route| route.get("location").is_none()),
        "{document}"
    );

    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let vars = [
        ("DATABASE_URL", data_plane_url.clone()),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-routing-policy".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
        (
            "GATEWAY_INSTANCE_ID",
            format!("routing-policy-test-{}", Uuid::new_v4().simple()),
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
    for model in ["both", "eu", "nowhere"] {
        assert_eq!(chat(&client, &base, &seeded.bearer, model).await.0, 200);
    }
    // A request of `both` that went to the EU, to retry once the EU is
    // forbidden.
    let mut eu_key = None;
    for n in 0..64 {
        let key = format!("eu-retry-{}-{n}", Uuid::new_v4().simple());
        let before = served(&pool, org).await;
        assert_eq!(
            chat_as(&client, &base, &seeded.bearer, "both", &key)
                .await
                .0,
            200
        );
        let after = served(&pool, org).await;
        if after.iter().find(|(name, _)| name == "chat-eu")
            != before.iter().find(|(name, _)| name == "chat-eu")
        {
            eu_key = Some(key);
            break;
        }
    }
    let eu_key = eu_key.expect("one of 64 requests went to the EU");

    // A reader cannot set it.
    let (status, _, body) = send(
        &router,
        Method::PUT,
        &path,
        &reader,
        "*",
        Some(json!({"locations": ["us-east-1"]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Invalid policies are refused.
    for (body, code) in [
        (json!({"providers": []}), "invalid_routing_policy"),
        (
            json!({"providers": ["openrouter"]}),
            "invalid_routing_policy",
        ),
        (
            json!({"locations": [" us-east-1"]}),
            "invalid_routing_policy",
        ),
        (
            json!({"regions": ["us-east-1"]}),
            "request_body_unprocessable",
        ),
    ] {
        let (status, _, problem) =
            send(&router, Method::PUT, &path, &admin, "*", Some(body.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} {problem}");
        assert_eq!(problem["code"], code, "{body} {problem}");
    }

    // US only: lists come back sorted and without duplicates.
    let (status, etag, policy) = send(
        &router,
        Method::PUT,
        &path,
        &admin,
        "*",
        Some(json!({"locations": ["us-east-1", "us-east-1"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    assert_eq!(policy["locations"], json!(["us-east-1"]));
    assert_eq!(policy["providers"], Value::Null);
    // Audited, as the reader's refused attempt was.
    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'routing_policy.set'
           AND entity_type = 'routing_policy'
         ORDER BY outcome",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audited, ["denied", "succeeded"]);

    // The configuration version carries it, and each route's location.
    let document = latest_document(&pool, org).await;
    assert_eq!(
        document["routing_policy"],
        json!({"locations": ["us-east-1"]})
    );
    let locations: Vec<(String, Value)> = document["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| {
            (
                route["deployment"].as_str().unwrap().to_owned(),
                route["location"].clone(),
            )
        })
        .collect();
    assert!(locations.contains(&("chat-us".into(), json!("us-east-1"))));
    assert!(locations.contains(&("chat-eu".into(), json!("eu-west-1"))));
    assert!(locations.contains(&("chat-nowhere".into(), Value::Null)));

    // Once the data plane loads it, `eu` is refused, and so is `nowhere`,
    // whose deployment says nowhere where it runs; `both` goes to the US.
    assert_eq!(
        until(&client, &base, &seeded.bearer, "eu", 403).await,
        (403, Some("routing_policy_denied".into()))
    );
    assert_eq!(
        chat(&client, &base, &seeded.bearer, "nowhere").await,
        (403, Some("routing_policy_denied".into()))
    );
    // Its retry is refused rather than replayed in the US under its key.
    assert_eq!(
        chat_as(&client, &base, &seeded.bearer, "both", &eu_key).await,
        (403, Some("routing_policy_denied".into()))
    );
    let before = served(&pool, org).await;
    for _ in 0..30 {
        assert_eq!(chat(&client, &base, &seeded.bearer, "both").await.0, 200);
    }
    let after = served(&pool, org).await;
    let count = |counts: &[(String, i64)], name: &str| {
        counts
            .iter()
            .find(|(deployment, _)| deployment == name)
            .map_or(0, |(_, count)| *count)
    };
    assert_eq!(
        count(&after, "chat-us") - count(&before, "chat-us"),
        30,
        "{after:?}"
    );
    assert_eq!(count(&after, "chat-eu"), count(&before, "chat-eu"));

    // A stale If-Match is refused.
    let (status, _, problem) = send(
        &router,
        Method::PUT,
        &path,
        &admin,
        "\"v7\"",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "version_conflict");

    // Providers: Bedrock only refuses every OpenAI deployment.
    let (status, etag, policy) = send(
        &router,
        Method::PUT,
        &path,
        &admin,
        "\"v1\"",
        Some(json!({"providers": ["bedrock"], "locations": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{policy}");
    assert_eq!(etag.as_deref(), Some("\"v2\""));
    assert_eq!(
        until(&client, &base, &seeded.bearer, "both", 403).await,
        (403, Some("routing_policy_denied".into()))
    );

    // Lifted: everything is served again.
    let (status, _, policy) = send(&router, Method::PUT, &path, &admin, "*", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{policy}");
    assert_eq!(
        policy,
        json!({"providers": null, "locations": null, "version": 3,
               "updated_at": policy["updated_at"]})
    );
    assert_eq!(
        until(&client, &base, &seeded.bearer, "eu", 200).await.0,
        200
    );
    assert_eq!(chat(&client, &base, &seeded.bearer, "nowhere").await.0, 200);
    let (status, etag, _) = send(&router, Method::GET, &path, &reader, "", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"v3\""));
}
