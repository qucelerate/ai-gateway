#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Load test of the inference path: the real HTTP server, PostgreSQL and the
//! OpenAI adapter, against a local fake provider that answers after a fixed
//! delay, so what is measured is the gateway's own overhead.
//!
//! The profile is a small team (the first deployments targeted): a
//! sustained 10 requests/s, then 25 clients in flight at once, which with
//! the provider's 300 ms reaches about 50 requests/s or more. The mix is 70%
//! chat and 30% streaming chat, all for one organization, so every request
//! contends for the same budget.
//!
//! After the load, accounting must be exact: every request settled once,
//! budget consumed equal to the usage recorded, nothing left reserved and no
//! reconciliation item opened.
//!
//! `load_smoke` runs in CI (short, in a debug build: correctness, with loose
//! latency bounds). The full run asserts the latency and throughput targets
//! and prints a report:
//!
//! ```text
//! cargo test --release --test load_integration -- --ignored --nocapture
//! ```

use std::{
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{Json, Router, body::Body, response::Response, routing::post};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    OrganizationId,
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{sync::Semaphore, task::JoinSet};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

const PEPPER: &str = "load-test-pepper-0000000000000000";
const PROVIDER_LATENCY: Duration = Duration::from_millis(300);
/// Tokens the fake provider reports for every request.
const INPUT_TOKENS: u64 = 5;
const OUTPUT_TOKENS: u64 = 7;

/// One load profile and what it must achieve.
struct Profile {
    sustained_rps: u32,
    sustained: Duration,
    concurrency: usize,
    concurrent: Duration,
    /// Gateway overhead percentiles (latency minus the provider's delay).
    max_overhead_p50: Duration,
    max_overhead_p95: Duration,
    max_overhead_p99: Duration,
    /// Requests per second the concurrent phase must reach.
    min_concurrent_rps: f64,
}

/// The fake provider: OpenAI chat completions, plain or streamed, after
/// [`PROVIDER_LATENCY`].
async fn provider(Json(body): Json<Value>) -> Response {
    tokio::time::sleep(PROVIDER_LATENCY).await;
    let usage = json!({"prompt_tokens": INPUT_TOKENS, "completion_tokens": OUTPUT_TOKENS});
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

fn capability(operation: &str) -> Value {
    json!({
        "operation": operation, "max_input_bytes": 4096, "max_output_tokens": 64,
        "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 60,
        "chat_input_count_mode": "unsupported"
    })
}

fn manifest() -> CatalogManifest {
    let price = |operation: &str| {
        json!({
            "deployment": "chat-a", "operation": operation, "input_minor_per_token": 3,
            "output_minor_per_token": 11, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "load-test", "source_version": "v1", "confidence": "known"
        })
    };
    serde_json::from_value(json!({
        "release": {"version": "load-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "gpt-load", "location": null,
                         "adapter_instance_key": "openai-main",
                         "capabilities": [capability("chat"), capability("chat_stream")]}],
        "prices": [price("chat"), price("chat_stream")],
        "routes": [
            {"alias": "chat", "operation": "chat", "deployment": "chat-a"},
            {"alias": "chat", "operation": "chat_stream", "deployment": "chat-a"}
        ]
    }))
    .unwrap()
}

/// A gateway serving one organization with a budget and an inference key.
struct Gateway {
    pool: PgPool,
    base: String,
    org: OrganizationId,
    bearer: String,
}

impl Gateway {
    async fn start(database_url: &str) -> Self {
        let provider_base =
            serve(Router::new().route("/v1/chat/completions", post(provider))).await;
        let vars = [
            ("DATABASE_URL", database_url.to_owned()),
            ("GATEWAY_PROFILE", "api".to_owned()),
            ("BIND_ADDR", "127.0.0.1:0".to_owned()),
            ("API_KEY_PEPPER", PEPPER.to_owned()),
            ("OPENAI_API_KEY", "sk-load".to_owned()),
            ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
        ];
        let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let state = gateway::application::connect(&config).await.unwrap();
        let base = serve(gateway::transport::router(state)).await;

        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(database_url)
            .await
            .unwrap();
        gateway::persistence::migrations::run(&pool).await.unwrap();
        let identity = IdentityStore::new(
            pool.clone(),
            ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity
            .create_organization(&format!("load-{suffix}"), "Load test")
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
            .create_service_account(org, user, None, "load")
            .await
            .unwrap()
            .id;
        let bearer = identity
            .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
            .await
            .unwrap()
            .bearer;
        let manifest = manifest();
        CatalogStore::new(pool.clone())
            .apply(org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, scope_kind, period_start, period_end, currency, limit_minor)
             VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '1 day',
                     'USD', 1000000000000)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
        Self {
            pool,
            base,
            org,
            bearer,
        }
    }
}

#[derive(Default)]
struct Outcomes {
    latencies: Mutex<Vec<Duration>>,
    failures: Mutex<Vec<String>>,
    succeeded: AtomicU64,
}

/// One request: chat, or a stream read to its end. Records the latency of a
/// success, or what failed.
async fn request(client: &reqwest::Client, gateway: &Gateway, stream: bool, outcomes: &Outcomes) {
    let path = if stream {
        "/api/inference/v1/chat/completions/stream"
    } else {
        "/api/inference/v1/chat/completions"
    };
    let started = Instant::now();
    let result = async {
        let response = client
            .post(format!("{}{path}", gateway.base))
            .bearer_auth(&gateway.bearer)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .json(&json!({
                "model": "chat",
                "messages": [{"role": "user", "content": "hello"}],
                "max_tokens": 16
            }))
            .send()
            .await
            .map_err(|error| format!("transport: {error}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| format!("body: {error}"))?;
        if !status.is_success() {
            return Err(format!("{status}: {body}"));
        }
        if stream && !body.contains("event: response.completed") {
            return Err(format!("stream did not complete: {body}"));
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            outcomes.succeeded.fetch_add(1, Ordering::Relaxed);
            outcomes.latencies.lock().unwrap().push(started.elapsed());
        }
        Err(error) => outcomes.failures.lock().unwrap().push(error),
    }
}

/// Every third or so request streams: a fixed 30% of them.
fn streams(index: u64) -> bool {
    index % 10 < 3
}

/// Open loop: starts `rps` requests a second for `duration`, however long
/// each takes.
async fn sustained(
    gateway: &Arc<Gateway>,
    client: &reqwest::Client,
    rps: u32,
    duration: Duration,
) -> Outcomes {
    let outcomes = Arc::new(Outcomes::default());
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1) / rps);
    let end = Instant::now() + duration;
    let mut index = 0;
    while Instant::now() < end {
        tick.tick().await;
        let (gateway, client, outcomes) = (gateway.clone(), client.clone(), outcomes.clone());
        index += 1;
        let stream = streams(index);
        tasks.spawn(async move { request(&client, &gateway, stream, &outcomes).await });
    }
    while tasks.join_next().await.is_some() {}
    Arc::into_inner(outcomes).unwrap()
}

/// Closed loop: `concurrency` clients each send their next request as soon
/// as the last one returns, for `duration`.
async fn concurrent(
    gateway: &Arc<Gateway>,
    client: &reqwest::Client,
    concurrency: usize,
    duration: Duration,
) -> (Outcomes, Duration) {
    let outcomes = Arc::new(Outcomes::default());
    let started = Instant::now();
    let end = started + duration;
    let next = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(Semaphore::new(concurrency));
    let mut tasks = JoinSet::new();
    for _ in 0..concurrency {
        let (gateway, client, outcomes, next, gate) = (
            gateway.clone(),
            client.clone(),
            outcomes.clone(),
            next.clone(),
            gate.clone(),
        );
        tasks.spawn(async move {
            while Instant::now() < end {
                let _permit = gate.acquire().await.unwrap();
                let index = next.fetch_add(1, Ordering::Relaxed);
                request(&client, &gateway, streams(index), &outcomes).await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    (Arc::into_inner(outcomes).unwrap(), started.elapsed())
}

fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() * percent).div_ceil(100))
        .saturating_sub(1)
        .min(sorted.len() - 1)]
}

/// Overhead percentiles: each latency less the provider's fixed delay.
fn overheads(outcomes: &Outcomes) -> [Duration; 4] {
    let mut overheads: Vec<Duration> = outcomes
        .latencies
        .lock()
        .unwrap()
        .iter()
        .map(|latency| latency.saturating_sub(PROVIDER_LATENCY))
        .collect();
    overheads.sort_unstable();
    [
        percentile(&overheads, 50),
        percentile(&overheads, 95),
        percentile(&overheads, 99),
        overheads.last().copied().unwrap_or_default(),
    ]
}

/// Accounting after the load: every request settled exactly once.
async fn assert_accounting(gateway: &Gateway, requests: u64) {
    let pool = &gateway.pool;
    let org = gateway.org.0;
    let states: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 GROUP BY state ORDER BY state",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        states,
        [("settled".to_owned(), requests as i64)],
        "reservations"
    );
    let (settled, rows, amount): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(DISTINCT reservation_id), count(*), coalesce(sum(amount_minor), 0)::BIGINT
         FROM gateway.accounting_usage WHERE organization_id = $1 AND finality = 'final'",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        (settled, rows),
        (requests as i64, requests as i64),
        "exactly one final usage row per request"
    );
    // Each request: 5 input tokens at 3 and 7 output tokens at 11 money units.
    let per_request = (INPUT_TOKENS * 3 + OUTPUT_TOKENS * 11) as i64;
    assert_eq!(amount, per_request * requests as i64, "usage charged");
    let (reserved, consumed): (i64, i64) = sqlx::query_as(
        "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket WHERE organization_id = $1",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((reserved, consumed), (0, amount), "budget matches usage");
    let reconciliation: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reconciliation WHERE organization_id = $1",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(reconciliation, 0, "no reconciliation items");
    let unfinished: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.inference_trace WHERE organization_id = $1 AND status <> 'finalized'",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(unfinished, 0, "every trace finalized");
}

async fn run(profile: Profile) {
    let Some(url) = test_database_url() else {
        return;
    };
    let gateway = Arc::new(Gateway::start(&url).await);
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(profile.concurrency * 2)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    // Warm up connections and caches.
    let warm = Outcomes::default();
    for index in 0..4 {
        request(&client, &gateway, streams(index), &warm).await;
    }
    assert!(
        warm.failures.lock().unwrap().is_empty(),
        "{:?}",
        warm.failures
    );

    let steady = sustained(&gateway, &client, profile.sustained_rps, profile.sustained).await;
    let (busy, elapsed) =
        concurrent(&gateway, &client, profile.concurrency, profile.concurrent).await;
    let busy_rps = busy.succeeded.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64();

    let report = |name: &str, outcomes: &Outcomes| {
        let [p50, p95, p99, max] = overheads(outcomes);
        println!(
            "{name:<24} ok {:>5}  failed {:>3}  overhead p50 {:>6.1} ms  p95 {:>6.1} ms  p99 {:>6.1} ms  max {:>6.1} ms",
            outcomes.succeeded.load(Ordering::Relaxed),
            outcomes.failures.lock().unwrap().len(),
            p50.as_secs_f64() * 1e3,
            p95.as_secs_f64() * 1e3,
            p99.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3,
        );
    };
    println!(
        "load profile: {} req/s for {:?}, then {} concurrent for {:?}; provider latency {:?}",
        profile.sustained_rps,
        profile.sustained,
        profile.concurrency,
        profile.concurrent,
        PROVIDER_LATENCY
    );
    report(
        &format!("sustained {} req/s", profile.sustained_rps),
        &steady,
    );
    report(&format!("{} concurrent", profile.concurrency), &busy);
    println!("concurrent throughput    {busy_rps:.1} req/s");

    for (name, outcomes) in [("sustained", &steady), ("concurrent", &busy)] {
        let failures = outcomes.failures.lock().unwrap();
        assert!(
            failures.is_empty(),
            "{name}: {} failed, first: {:?}",
            failures.len(),
            failures.first()
        );
        let [p50, p95, p99, _] = overheads(outcomes);
        assert!(
            p50 <= profile.max_overhead_p50,
            "{name} overhead p50 {p50:?}"
        );
        assert!(
            p95 <= profile.max_overhead_p95,
            "{name} overhead p95 {p95:?}"
        );
        assert!(
            p99 <= profile.max_overhead_p99,
            "{name} overhead p99 {p99:?}"
        );
    }
    assert!(
        busy_rps >= profile.min_concurrent_rps,
        "{busy_rps:.1} req/s with {} concurrent",
        profile.concurrency
    );
    let total =
        4 + steady.succeeded.load(Ordering::Relaxed) + busy.succeeded.load(Ordering::Relaxed);
    assert_accounting(&gateway, total).await;
}

/// In CI: a short run, checked for correctness under concurrency. Latency
/// bounds only catch stalls: CI builds are unoptimized and runners are
/// small and shared (a 2-vCPU runner measured p95 0.9 s at 25 concurrent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_smoke() {
    let _guard = database_guard();
    run(Profile {
        sustained_rps: 10,
        sustained: Duration::from_secs(4),
        concurrency: 25,
        concurrent: Duration::from_secs(4),
        max_overhead_p50: Duration::from_secs(2),
        max_overhead_p95: Duration::from_secs(5),
        max_overhead_p99: Duration::from_secs(10),
        min_concurrent_rps: 10.0,
    })
    .await;
}

/// The small-team target, in a release build: 10 req/s sustained for a
/// minute, then 25 concurrent for a minute at 50 req/s or more, with the
/// gateway adding at most 25 ms at the median, 75 ms at p95 and 150 ms at
/// p99.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a minute-long load run; see the module docs"]
async fn load_small_team() {
    let _guard = database_guard();
    run(Profile {
        sustained_rps: 10,
        sustained: Duration::from_secs(60),
        concurrency: 25,
        concurrent: Duration::from_secs(60),
        max_overhead_p50: Duration::from_millis(25),
        max_overhead_p95: Duration::from_millis(75),
        max_overhead_p99: Duration::from_millis(150),
        min_concurrent_rps: 50.0,
    })
    .await;
}

/// Headroom beyond the target: 100 concurrent (four times the profile) for
/// 30 seconds. Only correctness is asserted; the report shows where latency
/// starts to grow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a load run; see the module docs"]
async fn load_headroom() {
    let _guard = database_guard();
    run(Profile {
        sustained_rps: 40,
        sustained: Duration::from_secs(15),
        concurrency: 100,
        concurrent: Duration::from_secs(30),
        max_overhead_p50: Duration::from_secs(30),
        max_overhead_p95: Duration::from_secs(30),
        max_overhead_p99: Duration::from_secs(30),
        min_concurrent_rps: 0.0,
    })
    .await;
}
