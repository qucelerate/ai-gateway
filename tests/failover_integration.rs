//! Failover across a route's deployments (D-023, Phase 9 milestone 3): a
//! request the provider refused before any work (429) is tried on the next
//! deployment of its route, within its one reservation, which holds the
//! route's highest price and settles at the serving deployment's; a 503 is
//! not proof that no work was done and never fails over. A retry under the
//! request's key replays its last attempt.
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

/// OpenAI chat completions: `gpt-limited` is rate limited for two minutes
/// (429), `gpt-broken` unavailable (503); every other model answers.
async fn provider(Json(body): Json<Value>) -> Response {
    let refuse = |status: StatusCode, kind: &str| {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("retry-after", "120")
            .body(Body::from(
                json!({"error": {"message": "no", "type": kind}}).to_string(),
            ))
            .unwrap()
    };
    match body["model"].as_str() {
        Some("gpt-limited") => return refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
        Some("gpt-broken") => return refuse(StatusCode::SERVICE_UNAVAILABLE, "server_error"),
        _ => {}
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

/// `rated` prefers `chat-limited` (3 and 11 a token) over `chat-dear` (5
/// and 20); `broken` prefers `chat-broken` over `chat-dear`; `refused` goes
/// to `chat-limited` and then `chat-limited-2`, both refusing with 429, and
/// `refused-alone` to `chat-limited-3` only, which refuses too.
fn manifest() -> CatalogManifest {
    let deployment = |name: &str, model: &str| {
        json!({
            "name": name, "model": model, "location": null, "adapter_instance_key": "openai-main",
            "capabilities": [{
                "operation": "chat", "max_input_bytes": 4096,
                "max_output_tokens": 64, "max_tokens_per_byte": 1,
                "tokenizer": "configured", "reservation_ttl_seconds": 60,
                "chat_input_count_mode": "unsupported"
            }]
        })
    };
    let price = |deployment: &str, input: i64, output: i64| {
        json!({
            "deployment": deployment, "operation": "chat", "input_minor_per_token": input,
            "output_minor_per_token": output, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "failover-test", "source_version": "v1", "confidence": "known"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": "failover-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [
            deployment("chat-limited", "gpt-limited"),
            deployment("chat-broken", "gpt-broken"),
            deployment("chat-dear", "gpt-dear"),
            deployment("chat-limited-2", "gpt-limited"),
            deployment("chat-limited-3", "gpt-limited")
        ],
        "prices": [
            price("chat-limited", 3, 11),
            price("chat-broken", 3, 11),
            price("chat-dear", 5, 20),
            price("chat-limited-2", 5, 20),
            price("chat-limited-3", 3, 11)
        ],
        "routes": [
            {"alias": "rated", "operation": "chat", "deployment": "chat-limited"},
            {"alias": "rated", "operation": "chat", "deployment": "chat-dear", "priority": 2},
            {"alias": "broken", "operation": "chat", "deployment": "chat-broken"},
            {"alias": "broken", "operation": "chat", "deployment": "chat-dear", "priority": 2},
            {"alias": "refused", "operation": "chat", "deployment": "chat-limited"},
            {"alias": "refused", "operation": "chat", "deployment": "chat-limited-2", "priority": 2},
            {"alias": "refused-alone", "operation": "chat", "deployment": "chat-limited-3"}
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

#[derive(Debug, sqlx::FromRow)]
struct Attempt {
    id: Uuid,
    state: String,
    attempt: i16,
    failover_of: Option<Uuid>,
    deployment: String,
    reserved_minor: i64,
    input_price_minor: i64,
    trace_id: Uuid,
    trace_failover_of: Option<Uuid>,
    trace_attempt: i16,
    outcome: Option<String>,
    provider_error_status: Option<i16>,
    amount_minor: Option<i64>,
    input_bound: i64,
}

async fn attempts(pool: &PgPool, org: OrganizationId) -> Vec<Attempt> {
    sqlx::query_as(
        "SELECT r.id, r.state, r.attempt, r.failover_of, r.deployment, r.reserved_minor,
                r.input_price_minor, r.trace_id, t.failover_of AS trace_failover_of,
                t.attempt AS trace_attempt, t.outcome, t.provider_error_status,
                t.amount_minor, t.input_bound
         FROM gateway.accounting_reservation r
         JOIN gateway.inference_trace t ON t.id = r.trace_id
         WHERE r.organization_id = $1
         ORDER BY r.created_at, r.attempt",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_refused_request_fails_over_within_its_reservation() {
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

    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let vars = [
        ("DATABASE_URL", data_plane_url.clone()),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-failover".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
        (
            "GATEWAY_INSTANCE_ID",
            format!("failover-test-{}", Uuid::new_v4().simple()),
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

    // chat-limited refuses with 429, so chat-dear serves the same request.
    let key = Uuid::new_v4().to_string();
    assert_eq!(
        chat_as(&client, &base, &bearer, "rated", &key).await,
        StatusCode::OK
    );
    let rows = attempts(&pool, org).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (first, second) = (&rows[0], &rows[1]);
    assert_eq!(
        (
            first.deployment.as_str(),
            first.state.as_str(),
            first.attempt
        ),
        ("chat-limited", "released", 1)
    );
    assert_eq!(
        (first.outcome.as_deref(), first.provider_error_status),
        (Some("failed-over"), Some(429))
    );
    assert_eq!(
        (
            second.deployment.as_str(),
            second.state.as_str(),
            second.attempt
        ),
        ("chat-dear", "settled", 2)
    );
    assert_eq!(second.failover_of, Some(first.id));
    assert_eq!(
        (
            second.trace_failover_of,
            second.trace_attempt,
            first.trace_attempt
        ),
        (Some(first.trace_id), 2, 1)
    );
    // Both attempts hold the route's highest price (5 and 20 a token) for
    // the request's bounds, an output token reserved as output or reasoning
    // (20 + 20); each keeps its own deployment's price.
    let held = first.input_bound * 5 + 16 * (20 + 20);
    assert_eq!((first.reserved_minor, second.reserved_minor), (held, held));
    assert_eq!((first.input_price_minor, second.input_price_minor), (3, 5));
    // It settles at chat-dear's price: 5 input and 7 output tokens.
    assert_eq!(second.amount_minor, Some(5 * 5 + 7 * 20));
    let (reserved, consumed): (i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed), (0, 5 * 5 + 7 * 20));

    // A retry under the request's key replays its last attempt: finished.
    let retry = client
        .post(format!("{base}/api/inference/v1/chat/completions"))
        .bearer_auth(&bearer)
        .header("idempotency-key", &key)
        .json(
            &json!({"model": "rated", "messages": [{"role": "user", "content": "hello"}],
                      "max_tokens": 16}),
        )
        .send()
        .await
        .unwrap();
    let body: Value = retry.json().await.unwrap();
    assert_eq!(body["code"], "idempotency_terminal", "{body}");

    // chat-limited's retry-after is running, so the next request goes to
    // chat-dear directly, without failing over.
    assert_eq!(chat(&client, &base, &bearer, "rated").await, StatusCode::OK);
    let rows = attempts(&pool, org).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(
        (rows[2].deployment.as_str(), rows[2].attempt),
        ("chat-dear", 1)
    );

    // An end user on a plan and the key's own budget: the request is
    // admitted against both at the route's highest price, and its failover
    // holds them for the next attempt, which settles on both.
    let (account, key_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT service_account_id, id FROM gateway.api_key WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, api_key_id, scope_kind, period_start, period_end, currency,
              limit_minor)
         VALUES ($1, $2, $3, 'api-key', now() - interval '1 day', now() + interval '1 day',
                 'USD', 1000000)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(key_id)
    .execute(&pool)
    .await
    .unwrap();
    let plan = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.end_user_setting
             (service_account_id, organization_id, id, requirement, currency)
         VALUES ($1, $2, $3, 'optional', 'USD')",
    )
    .bind(account)
    .bind(org.0)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.end_user_plan
             (id, organization_id, service_account_id, name, allowance_minor, tokens_per_minute)
         VALUES ($1, $2, $3, 'basic', 1000000, 100000)",
    )
    .bind(plan)
    .bind(org.0)
    .bind(account)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id, plan_id)
         VALUES ($1, $2, $3, 'failover-user', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(account)
    .bind(plan)
    .execute(&pool)
    .await
    .unwrap();
    // chat-limited's retry-after is held in this process; a second data
    // plane, which has not seen it, sends the request there first.
    let fresh = serve(gateway::transport::data_plane_router(
        gateway::application::connect(&config).await.unwrap(),
    ))
    .await;
    let response = client
        .post(format!("{fresh}/api/inference/v1/chat/completions"))
        .bearer_auth(&bearer)
        .header("idempotency-key", Uuid::new_v4().to_string())
        .header("gateway-end-user", "failover-user")
        .json(
            &json!({"model": "rated", "messages": [{"role": "user", "content": "hello"}],
                      "max_tokens": 16}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let rows = attempts(&pool, org).await;
    assert_eq!(rows.len(), 5, "{rows:?}");
    let (left, served) = (&rows[3], &rows[4]);
    assert_eq!(
        (
            left.state.as_str(),
            served.state.as_str(),
            served.failover_of
        ),
        ("released", "settled", Some(left.id))
    );
    let named: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT end_user_id FROM gateway.accounting_reservation WHERE id = ANY($1) ORDER BY attempt",
    )
    .bind(vec![left.id, served.id])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(named[0].is_some() && named[0] == named[1], "{named:?}");
    // Under the plan's tokens per minute the next attempt holds no more
    // tokens than the one it replaced.
    let bounds: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT token_bound FROM gateway.accounting_reservation WHERE id = ANY($1) ORDER BY attempt",
    )
    .bind(vec![left.id, served.id])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(bounds[0].is_some() && bounds[1] <= bounds[0], "{bounds:?}");
    // The key's budget and the user's allowance each hold nothing now and
    // were charged once, at chat-dear's price.
    let charged: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT scope_kind, reserved_minor, consumed_minor FROM gateway.quota_bucket
         WHERE organization_id = $1 AND scope_kind IN ('api-key', 'end-user')
         ORDER BY scope_kind",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    let cost = 5 * 5 + 7 * 20;
    assert_eq!(
        charged,
        vec![
            ("api-key".to_owned(), 0, cost),
            ("end-user".to_owned(), 0, cost)
        ]
    );
    let held: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT reservation_id, state, reserved_minor FROM gateway.accounting_allocation
         WHERE reservation_id = ANY($1) ORDER BY reservation_id, quota_bucket_id",
    )
    .bind(vec![left.id, served.id])
    .fetch_all(&pool)
    .await
    .unwrap();
    // Organization, key and end user, for each attempt.
    assert_eq!(held.len(), 6, "{held:?}");
    assert!(
        held.iter()
            .all(|(_, _, amount)| *amount == left.reserved_minor),
        "{held:?}"
    );

    // A budget that affords only part of the asked output: the first attempt
    // is cut to what fits (20 of 64 tokens), and so is the attempt it fails
    // over to, yet a retry of the request as it was sent replays it rather
    // than conflicting.
    let (tight, tight_bearer) = seed(&pool).await;
    let bound = first.input_bound;
    sqlx::query("UPDATE gateway.quota_bucket SET limit_minor = $2 WHERE organization_id = $1")
        .bind(tight.0)
        .bind(bound * 5 + 20 * 40)
        .execute(&pool)
        .await
        .unwrap();
    let send = |key: String| {
        client
            .post(format!("{base}/api/inference/v1/chat/completions"))
            .bearer_auth(&tight_bearer)
            .header("idempotency-key", key)
            .json(
                &json!({"model": "rated", "messages": [{"role": "user", "content": "hello"}],
                          "max_tokens": 64}),
            )
            .send()
    };
    let key = Uuid::new_v4().to_string();
    let response = send(key.clone()).await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let caps: Vec<(i16, i64)> = sqlx::query_as(
        "SELECT attempt, output_token_cap::BIGINT FROM gateway.accounting_reservation
         WHERE organization_id = $1 ORDER BY attempt",
    )
    .bind(tight.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(caps, vec![(1, 20), (2, 20)]);
    let body: Value = send(key).await.unwrap().json().await.unwrap();
    assert_eq!(body["code"], "idempotency_terminal", "{body}");

    // A 503 may follow work, so it never fails over: the request fails on
    // chat-broken and nothing reaches chat-dear.
    assert_ne!(
        chat(&client, &base, &bearer, "broken").await,
        StatusCode::OK
    );
    let rows = attempts(&pool, org).await;
    assert_eq!(rows.len(), 6, "{rows:?}");
    assert_eq!(
        (
            rows[5].deployment.as_str(),
            rows[5].attempt,
            rows[5].state.as_str()
        ),
        ("chat-broken", 1, "unknown")
    );
}

/// Under a policy's tokens-per-minute limit a failover attempt is the
/// request it continues: counted at the second that request was, holding
/// no more tokens than the attempt it replaces, so the window comes out as
/// the reservations say, and it names the request's API key.
#[tokio::test]
async fn a_failover_under_a_token_limit_counts_as_the_request_it_continues() {
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
    let policy: Uuid = sqlx::query_scalar(
        "UPDATE gateway.rate_limit_policy SET tokens_per_minute = 1000000
         WHERE organization_id = $1 RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("SELECT gateway.seed_rate_limit_counter($1)")
        .bind(policy)
        .execute(&pool)
        .await
        .unwrap();
    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let base = data_plane(&data_plane_url, &provider_base).await;
    let client = reqwest::Client::new();
    assert_eq!(chat(&client, &base, &bearer, "rated").await, StatusCode::OK);
    let rows: Vec<(
        Uuid,
        String,
        Option<Uuid>,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<i64>,
        Option<Uuid>,
    )> = sqlx::query_as(
        "SELECT id, state, failover_of, created_at, tokens_counted_at, token_bound, api_key_id
             FROM gateway.accounting_reservation WHERE organization_id = $1
             ORDER BY created_at, attempt",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (first, second) = (&rows[0], &rows[1]);
    assert_eq!(
        (first.1.as_str(), second.1.as_str()),
        ("released", "settled")
    );
    assert_eq!(second.2, Some(first.0));
    assert_eq!(first.4, None);
    assert_eq!(second.4, Some(first.3));
    assert!(second.5.unwrap() <= first.5.unwrap(), "{rows:?}");
    assert!(first.6.is_some());
    assert_eq!(second.6, first.6);
    // The window as admission reads it, and as the reservations count it:
    // the released attempt nothing, the settled one what it reported.
    let (counted, expected): (i64, i64) = sqlx::query_as(
        "WITH clock AS (SELECT floor(extract(epoch FROM clock_timestamp()))::BIGINT AS top)
         SELECT
             (SELECT sum(gateway.rate_limit_ring_at(c.token_slots, c.token_last_second, g.second)
                  + COALESCE((SELECT sum(gateway.rate_limit_ring_at(
                                  x.token_slots, x.token_last_second, g.second))
                              FROM gateway.rate_limit_release x WHERE x.policy_id = c.policy_id),
                             0))::BIGINT
              FROM gateway.rate_limit_counter c, clock,
                   generate_series(clock.top - 60, clock.top) AS g(second)
              WHERE c.policy_id = $1),
             (SELECT sum(gateway.rate_limit_reservation_tokens(
                         r.organization_id, r.id, r.state, r.token_bound))::BIGINT
              FROM gateway.accounting_reservation r WHERE r.organization_id = $2)",
    )
    .bind(policy)
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counted, expected);
    assert!(expected > 0);
}

/// A data plane on `data_plane_url` calling the provider at `provider_base`.
async fn data_plane(data_plane_url: &str, provider_base: &str) -> String {
    let vars = [
        ("DATABASE_URL", data_plane_url.to_owned()),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-failover".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
        (
            "GATEWAY_INSTANCE_ID",
            format!("failover-test-{}", Uuid::new_v4().simple()),
        ),
        ("GATEWAY_CONFIG_POLL_SECONDS", "1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    serve(gateway::transport::data_plane_router(
        gateway::application::connect(&config).await.unwrap(),
    ))
    .await
}

/// A request every deployment refused with 429 used nothing: each attempt
/// is released, the last closing as `refused` at zero, and nothing is left
/// held or sent to reconciliation (D-030, load and resilience testing).
/// The same holds on a route with no other deployment to fail over to.
#[tokio::test]
async fn a_request_every_deployment_refuses_is_released_at_zero() {
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
    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let base = data_plane(&data_plane_url, &provider_base).await;
    let client = reqwest::Client::new();

    assert_eq!(
        chat(&client, &base, &bearer, "refused").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let rows = attempts(&pool, org).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (first, last) = (&rows[0], &rows[1]);
    assert_eq!(
        (first.deployment.as_str(), first.state.as_str()),
        ("chat-limited", "released")
    );
    assert_eq!(first.outcome.as_deref(), Some("failed-over"));
    assert_eq!(
        (
            last.deployment.as_str(),
            last.state.as_str(),
            last.failover_of
        ),
        ("chat-limited-2", "released", Some(first.id))
    );
    assert_eq!(
        (
            last.outcome.as_deref(),
            last.provider_error_status,
            last.amount_minor
        ),
        (Some("refused"), Some(429), Some(0))
    );

    // Alone on its route, a refusal is released the same way.
    assert_eq!(
        chat(&client, &base, &bearer, "refused-alone").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let rows = attempts(&pool, org).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(
        (
            rows[2].deployment.as_str(),
            rows[2].state.as_str(),
            rows[2].outcome.as_deref(),
            rows[2].amount_minor
        ),
        ("chat-limited-3", "released", Some("refused"), Some(0))
    );

    // Nothing held, nothing charged, nothing to reconcile, no usage, and
    // each refusal recorded as one finalization.
    let (reserved, consumed): (i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed), (0, 0));
    let held: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT a.state FROM gateway.accounting_allocation a
         JOIN gateway.accounting_reservation r ON r.id = a.reservation_id
         WHERE r.organization_id = $1",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(held, vec!["released".to_owned()]);
    let (items, usage, refused): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM gateway.accounting_reconciliation WHERE organization_id = $1),
                (SELECT count(*) FROM gateway.accounting_usage WHERE organization_id = $1),
                (SELECT count(*) FROM gateway.accounting_finalization
                 WHERE organization_id = $1 AND outcome = 'refused')",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((items, usage, refused), (0, 0, 2));
}

/// Inference bodies may reach `GATEWAY_INFERENCE_MAX_BODY_BYTES` (4 MiB by
/// default, room for a prompt near a million tokens), far past the 256 KiB
/// other routes accept; a body past it is refused as `request_too_large`.
#[tokio::test]
async fn inference_accepts_bodies_up_to_its_own_limit() {
    let Some((app_url, data_plane_url)) = database_urls() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let (_, bearer) = seed(&pool).await;
    let provider_base = serve(Router::new().route("/v1/chat/completions", post(provider))).await;
    let base = data_plane(&data_plane_url, &provider_base).await;
    let client = reqwest::Client::new();
    let send = |bytes: usize| {
        client
            .post(format!("{base}/api/inference/v1/chat/completions"))
            .bearer_auth(&bearer)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .json(&json!({
                "model": "rated",
                "messages": [{"role": "user", "content": "x".repeat(bytes)}],
                "max_tokens": 16
            }))
            .send()
    };
    // 1 MiB passes the body limit and the request bounds, and is refused
    // only by the deployment's own input limit (4 KiB in this catalog).
    let response = send(1024 * 1024).await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_ne!(status, 413, "{body}");
    assert_ne!(body["code"], "request_too_large", "{body}");
    assert_ne!(body["code"], "chat_input_bound_exceeded", "{body}");
    let response = send(4 * 1024 * 1024 + 1).await.unwrap();
    assert_eq!(response.status(), 413);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "request_too_large", "{body}");
}
