#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! High-volume benchmark of the inference path (D-029): several gateway
//! instances, each with its own database pool, against one PostgreSQL, and a
//! local fake provider answering after 300 ms, so what is measured is what
//! the gateway and its database add.
//!
//! Closed-loop steps of rising concurrency find the throughput knee. Each
//! step reports requests per second, the gateway's overhead percentiles,
//! database CPU (from the container's cgroup), commits per request, the
//! database sessions active and waiting (sampled from `pg_stat_activity`
//! every 250 ms) and the most common wait events. After the last step,
//! accounting must still be exact for every organization: every request
//! settled once, budget consumed equal to usage, nothing left reserved, no
//! reconciliation item.
//!
//! Configured from the environment (only `GATEWAY_TEST_DATABASE_URL` is
//! required; the rest have defaults):
//!
//! - `GATEWAY_TEST_DATABASE_URL`: the application role, for setup and checks.
//! - `LOAD_GATEWAY_DATABASE_URL`: what the gateways connect to, for example a
//!   PgBouncer in transaction mode; the URL above when unset.
//! - `LOAD_ADMIN_DATABASE_URL`: a superuser, for sampling every session's
//!   wait events; the application role's own sessions when unset.
//! - `LOAD_GATEWAYS` (1), `LOAD_POOL` (10 connections per gateway),
//!   `LOAD_PROFILE` (`contended`: one organization and key; `spread`: 100
//!   organizations with 10 keys each; `keys`: as `spread`, with a budget on
//!   every key too), `LOAD_STEPS` (concurrency per step,
//!   `50,100,200,400`), `LOAD_STEP_SECONDS` (20), `LOAD_PG_CONTAINER` (the
//!   Docker container whose CPU is reported).
//!
//! ```text
//! LOAD_GATEWAYS=4 LOAD_PROFILE=spread \
//!   cargo test --release --test load_high_volume -- --ignored --nocapture
//! ```

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
use tokio::task::JoinSet;
use uuid::Uuid;

const PEPPER: &str = "load-test-pepper-0000000000000000";
const PROVIDER_LATENCY: Duration = Duration::from_millis(300);
const INPUT_TOKENS: u64 = 5;
const OUTPUT_TOKENS: u64 = 7;

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

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

/// One organization under load: its keys, and how many requests succeeded
/// in all and with each key.
struct Tenant {
    org: OrganizationId,
    bearers: Vec<String>,
    key_ids: Vec<Uuid>,
    succeeded: AtomicU64,
    succeeded_per_key: Vec<AtomicU64>,
}

struct Bench {
    profile: String,
    /// The role and database the gateways connect as, to sample their sessions.
    gateway_role: String,
    gateway_database: String,
    pool: PgPool,
    admin: PgPool,
    gateways: Vec<String>,
    tenants: Vec<Tenant>,
}

async fn tenant(pool: &PgPool, keys: usize, key_budgets: bool) -> Tenant {
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
    let mut bearers = Vec::with_capacity(keys);
    let mut key_ids = Vec::with_capacity(keys);
    for _ in 0..keys {
        let key = identity
            .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
            .await
            .unwrap();
        if key_budgets {
            sqlx::query(
                "INSERT INTO gateway.quota_bucket
                     (id, organization_id, scope_kind, api_key_id, period_start, period_end,
                      currency, limit_minor)
                 VALUES ($1, $2, 'api-key', $3, now() - interval '1 day',
                         now() + interval '1 day', 'USD', 1000000000000)",
            )
            .bind(Uuid::new_v4())
            .bind(org.0)
            .bind(key.id.0)
            .execute(pool)
            .await
            .unwrap();
        }
        bearers.push(key.bearer);
        key_ids.push(key.id.0);
    }
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
    .execute(pool)
    .await
    .unwrap();
    Tenant {
        org,
        succeeded_per_key: (0..bearers.len()).map(|_| AtomicU64::new(0)).collect(),
        bearers,
        key_ids,
        succeeded: AtomicU64::new(0),
    }
}

impl Bench {
    async fn start(database_url: &str) -> Self {
        let gateway_url =
            std::env::var("LOAD_GATEWAY_DATABASE_URL").unwrap_or_else(|_| database_url.to_owned());
        let admin_url =
            std::env::var("LOAD_ADMIN_DATABASE_URL").unwrap_or_else(|_| database_url.to_owned());
        let instances: usize = env_or("LOAD_GATEWAYS", 1);
        let pool_size: u32 = env_or("LOAD_POOL", 10);
        let (orgs, keys, key_budgets) = match std::env::var("LOAD_PROFILE").as_deref() {
            Ok("spread") => (100, 10, false),
            Ok("keys") => (100, 10, true),
            _ => (1, 1, false),
        };

        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await
            .unwrap();
        gateway::persistence::migrations::run(&pool).await.unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&admin_url)
            .await
            .unwrap();

        let provider_base =
            serve(Router::new().route("/v1/chat/completions", post(provider))).await;
        let mut gateways = Vec::with_capacity(instances);
        for _ in 0..instances {
            let vars = [
                ("DATABASE_URL", gateway_url.clone()),
                ("GATEWAY_PROFILE", "api".to_owned()),
                ("BIND_ADDR", "127.0.0.1:0".to_owned()),
                ("API_KEY_PEPPER", PEPPER.to_owned()),
                ("OPENAI_API_KEY", "sk-load".to_owned()),
                ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
                ("GATEWAY_DATABASE_MAX_CONNECTIONS", pool_size.to_string()),
            ];
            let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            })
            .unwrap();
            let state = gateway::application::connect(&config).await.unwrap();
            gateways.push(serve(gateway::transport::router(state)).await);
        }

        let mut tenants = Vec::with_capacity(orgs);
        for _ in 0..orgs {
            tenants.push(tenant(&pool, keys, key_budgets).await);
        }
        let parsed = url::Url::parse(&gateway_url).expect("gateway database URL");
        Self {
            profile: std::env::var("LOAD_PROFILE").unwrap_or_else(|_| "contended".to_owned()),
            gateway_role: parsed.username().to_owned(),
            gateway_database: parsed.path().trim_start_matches('/').to_owned(),
            pool,
            admin,
            gateways,
            tenants,
        }
    }
}

#[derive(Default)]
struct Outcomes {
    latencies: Mutex<Vec<Duration>>,
    failures: Mutex<Vec<String>>,
    succeeded: AtomicU64,
}

/// Request `index`: its organization and key, the gateway it goes to, and
/// whether it streams (30%). The gateway and streaming are chosen
/// independently of the organization, so every organization reaches every
/// gateway in both modes.
async fn request(client: &reqwest::Client, bench: &Bench, index: u64, outcomes: &Outcomes) {
    let tenants = bench.tenants.len() as u64;
    let tenant = &bench.tenants[(index % tenants) as usize];
    let round = index / tenants;
    let gateway = &bench.gateways[(round % bench.gateways.len() as u64) as usize];
    let key = ((round / bench.gateways.len() as u64) % tenant.bearers.len() as u64) as usize;
    let bearer = &tenant.bearers[key];
    // A multiplicative hash spreads the 30% across organizations and gateways.
    let stream = ((index.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) & 0xFFFF) < 0xFFFF * 3 / 10;
    let path = if stream {
        "/api/inference/v1/chat/completions/stream"
    } else {
        "/api/inference/v1/chat/completions"
    };
    let started = Instant::now();
    let result = async {
        let response = client
            .post(format!("{gateway}{path}"))
            .bearer_auth(bearer)
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
            tenant.succeeded.fetch_add(1, Ordering::Relaxed);
            tenant.succeeded_per_key[key].fetch_add(1, Ordering::Relaxed);
            outcomes.latencies.lock().unwrap().push(started.elapsed());
        }
        Err(error) => outcomes.failures.lock().unwrap().push(error),
    }
}

/// Database sessions sampled during a step.
#[derive(Default)]
struct Sessions {
    samples: u64,
    active: u64,
    active_max: u64,
    lock_waits: u64,
    waits: HashMap<String, u64>,
}

async fn sample(admin: &PgPool, role: &str, database: &str, sessions: &Mutex<Sessions>) {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT state, wait_event_type, wait_event FROM pg_stat_activity
         WHERE usename = $1 AND datname = $2 AND backend_type = 'client backend'
           AND pid <> pg_backend_pid() AND state IS NOT NULL",
    )
    .bind(role)
    .bind(database)
    .fetch_all(admin)
    .await
    .unwrap_or_default();
    let mut sessions = sessions.lock().unwrap();
    sessions.samples += 1;
    let mut active = 0;
    for (state, kind, event) in rows {
        if state == "idle" {
            continue;
        }
        active += 1;
        if let (Some(kind), Some(event)) = (kind, event) {
            if kind == "Lock" {
                sessions.lock_waits += 1;
            }
            if kind != "Client" {
                *sessions.waits.entry(format!("{kind}:{event}")).or_default() += 1;
            }
        }
    }
    sessions.active += active;
    sessions.active_max = sessions.active_max.max(active);
}

/// CPU seconds the database container has used, from its cgroup.
fn database_cpu_seconds() -> Option<f64> {
    let container = std::env::var("LOAD_PG_CONTAINER").ok()?;
    let output = std::process::Command::new("docker")
        .args(["exec", &container, "cat", "/sys/fs/cgroup/cpu.stat"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|usec| usec / 1e6)
}

/// CPU seconds this process (gateways, client and fake provider) has used.
fn process_cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    let ticks = |index: usize| fields.get(index).and_then(|v| v.parse::<f64>().ok());
    // utime and stime are fields 14 and 15; after the `)` they are 11 and 12.
    (ticks(11).unwrap_or(0.0) + ticks(12).unwrap_or(0.0)) / 100.0
}

async fn commits(admin: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_one(admin)
    .await
    .unwrap_or(0)
}

fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() * percent).div_ceil(100))
        .saturating_sub(1)
        .min(sorted.len() - 1)]
}

async fn step(
    bench: &Arc<Bench>,
    client: &reqwest::Client,
    next: &Arc<AtomicU64>,
    concurrency: usize,
    duration: Duration,
) -> Outcomes {
    let outcomes = Arc::new(Outcomes::default());
    let sessions = Arc::new(Mutex::new(Sessions::default()));
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let (bench, sessions, sampling) = (bench.clone(), sessions.clone(), sampling.clone());
        tokio::spawn(async move {
            while sampling.load(Ordering::Relaxed) {
                sample(
                    &bench.admin,
                    &bench.gateway_role,
                    &bench.gateway_database,
                    &sessions,
                )
                .await;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    };
    let (db_before, cpu_before, commits_before) = (
        database_cpu_seconds(),
        process_cpu_seconds(),
        commits(&bench.admin).await,
    );
    let started = Instant::now();
    let end = started + duration;
    let mut tasks = JoinSet::new();
    for _ in 0..concurrency {
        let (bench, client, outcomes, next) = (
            bench.clone(),
            client.clone(),
            outcomes.clone(),
            next.clone(),
        );
        tasks.spawn(async move {
            while Instant::now() < end {
                let index = next.fetch_add(1, Ordering::Relaxed);
                request(&client, &bench, index, &outcomes).await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    let elapsed = started.elapsed().as_secs_f64();
    sampling.store(false, Ordering::Relaxed);
    sampler.await.unwrap();
    let db_cores = database_cpu_seconds()
        .zip(db_before)
        .map(|(after, before)| (after - before) / elapsed);
    let process_cores = (process_cpu_seconds() - cpu_before) / elapsed;
    let commit_count = commits(&bench.admin).await - commits_before;

    let ok = outcomes.succeeded.load(Ordering::Relaxed);
    let mut overheads: Vec<Duration> = outcomes
        .latencies
        .lock()
        .unwrap()
        .iter()
        .map(|latency| latency.saturating_sub(PROVIDER_LATENCY))
        .collect();
    overheads.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let sessions = sessions.lock().unwrap();
    let samples = sessions.samples.max(1) as f64;
    let mut waits: Vec<(&String, &u64)> = sessions.waits.iter().collect();
    waits.sort_by(|a, b| b.1.cmp(a.1));
    let top: Vec<String> = waits
        .iter()
        .take(3)
        .map(|(name, count)| format!("{name} {:.1}", **count as f64 / samples))
        .collect();
    println!(
        "RESULT gateways={} profile={} concurrency={concurrency} ok={ok} failed={} rps={:.0} \
         p50={:.1} p95={:.1} p99={:.1} max={:.1} db_cores={} proc_cores={process_cores:.1} \
         commits_per_req={:.1} db_active_avg={:.1} db_active_max={} lock_waits_avg={:.1} top_waits=[{}]",
        bench.gateways.len(),
        bench.profile,
        outcomes.failures.lock().unwrap().len(),
        ok as f64 / elapsed,
        ms(percentile(&overheads, 50)),
        ms(percentile(&overheads, 95)),
        ms(percentile(&overheads, 99)),
        ms(overheads.last().copied().unwrap_or_default()),
        db_cores.map_or("n/a".to_owned(), |cores| format!("{cores:.1}")),
        commit_count as f64 / (ok.max(1)) as f64,
        sessions.active as f64 / samples,
        sessions.active_max,
        sessions.lock_waits as f64 / samples,
        top.join(", "),
    );
    drop(sessions);
    Arc::into_inner(outcomes).unwrap()
}

/// Accounting after the load, per organization: every request settled once.
async fn assert_accounting(bench: &Bench) {
    let per_request = (INPUT_TOKENS * 3 + OUTPUT_TOKENS * 11) as i64;
    for tenant in &bench.tenants {
        let requests = tenant.succeeded.load(Ordering::Relaxed) as i64;
        let org = tenant.org.0;
        let states: Vec<(String, i64)> = sqlx::query_as(
            "SELECT state, count(*) FROM gateway.accounting_reservation
             WHERE organization_id = $1 GROUP BY state ORDER BY state",
        )
        .bind(org)
        .fetch_all(&bench.pool)
        .await
        .unwrap();
        assert_eq!(states, [("settled".to_owned(), requests)], "reservations");
        let (settled, rows, amount): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(DISTINCT reservation_id), count(*), coalesce(sum(amount_minor), 0)::BIGINT
             FROM gateway.accounting_usage WHERE organization_id = $1 AND finality = 'final'",
        )
        .bind(org)
        .fetch_one(&bench.pool)
        .await
        .unwrap();
        assert_eq!(
            (settled, rows),
            (requests, requests),
            "one final usage row each"
        );
        assert_eq!(amount, per_request * requests, "usage charged");
        let (reserved, consumed): (i64, i64) = sqlx::query_as(
            "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket
             WHERE organization_id = $1 AND scope_kind = 'organization'",
        )
        .bind(org)
        .fetch_one(&bench.pool)
        .await
        .unwrap();
        assert_eq!((reserved, consumed), (0, amount), "budget matches usage");
        // Each key's budget, where there is one, is charged for exactly that
        // key's requests.
        let buckets: Vec<(Uuid, i64, i64)> = sqlx::query_as(
            "SELECT api_key_id, reserved_minor, consumed_minor
             FROM gateway.quota_bucket WHERE organization_id = $1 AND scope_kind = 'api-key'",
        )
        .bind(org)
        .fetch_all(&bench.pool)
        .await
        .unwrap();
        if !buckets.is_empty() {
            assert_eq!(buckets.len(), tenant.key_ids.len(), "one budget per key");
            for (key_id, reserved, consumed) in buckets {
                let key = tenant
                    .key_ids
                    .iter()
                    .position(|id| *id == key_id)
                    .expect("budget for a created key");
                let expected =
                    per_request * tenant.succeeded_per_key[key].load(Ordering::Relaxed) as i64;
                assert_eq!((reserved, consumed), (0, expected), "key {key_id} budget");
            }
        }
        let reconciliation: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.accounting_reconciliation WHERE organization_id = $1",
        )
        .bind(org)
        .fetch_one(&bench.pool)
        .await
        .unwrap();
        assert_eq!(reconciliation, 0, "no reconciliation items");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
#[ignore = "a long benchmark; see the module docs"]
async fn load_high_volume() {
    let Ok(url) = std::env::var("GATEWAY_TEST_DATABASE_URL") else {
        return;
    };
    let steps: Vec<usize> = std::env::var("LOAD_STEPS")
        .unwrap_or_else(|_| "50,100,200,400".to_owned())
        .split(',')
        .filter_map(|step| step.trim().parse().ok())
        .collect();
    let seconds: u64 = env_or("LOAD_STEP_SECONDS", 20);
    let bench = Arc::new(Bench::start(&url).await);
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(steps.iter().copied().max().unwrap_or(100) * 2)
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let next = Arc::new(AtomicU64::new(0));
    // Warm every gateway–organization pair once.
    let warm = Outcomes::default();
    let warmups = (bench.gateways.len() * bench.tenants.len()) as u64;
    for _ in 0..warmups {
        let index = next.fetch_add(1, Ordering::Relaxed);
        request(&client, &bench, index, &warm).await;
    }
    assert!(
        warm.failures.lock().unwrap().is_empty(),
        "{:?}",
        warm.failures
    );
    println!(
        "bench: {} gateways, {} organizations x {} keys, steps {steps:?} x {seconds}s, provider {PROVIDER_LATENCY:?}",
        bench.gateways.len(),
        bench.tenants.len(),
        bench.tenants[0].bearers.len()
    );
    let mut failed = 0;
    for concurrency in steps {
        let outcomes = step(
            &bench,
            &client,
            &next,
            concurrency,
            Duration::from_secs(seconds),
        )
        .await;
        let failures = outcomes.failures.lock().unwrap();
        if let Some(first) = failures.first() {
            println!("  first failure: {first}");
        }
        failed += failures.len();
    }
    assert_eq!(failed, 0, "requests failed");
    assert_accounting(&bench).await;
    println!("accounting exact for every organization");
}
