#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the ClickHouse analytics sink (D-030 milestone 5):
//! delivery from the outbox, idempotent writes, retention deletes, the
//! backfill and cutover, falling back to PostgreSQL, and read parity: every
//! usage series, usage event, usage report and trace query is run against
//! PostgreSQL and ClickHouse on the same data and the answers compared.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL` and `GATEWAY_TEST_CLICKHOUSE_URL`
//! (with `GATEWAY_TEST_CLICKHOUSE_USER` and `GATEWAY_TEST_CLICKHOUSE_PASSWORD`
//! when the server wants them); skipped locally without them, required in
//! CI. Each test uses its own ClickHouse database.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, ProviderFamily, ServiceAccountId, TeamId,
    UserAccountId,
    clickhouse::{ClickHouse, ClickHouseConfig, Param},
    domain::{
        accounting::Currency,
        identity::{ApiKeyScope, MembershipRole},
        keys::ApiKeyPepper,
        outbox::{InferenceFinalized, OutboxPayload},
        reporting::{TraceDimension, TraceQuery, TraceStatus, UsageGroupBy, UsageQuery},
        usage_analytics::{
            Granularity, RankBy, RequestStatus, UsageDimension, UsageSeriesQuery, UsageSlice,
        },
    },
    jobs::analytics_sink::AnalyticsSinkRunner,
    persistence::{
        IdentityStore, OutboxStore,
        analytics_sink::{AnalyticsSink, AnalyticsSinkStore, OFF_CACHE, Phase},
        catalog::ResolvedCatalogRoute,
        clickhouse_analytics::ClickHouseAnalytics,
        outbox::{self, Sinks},
        reporting::{ReportingStore, TimeKeyset, UsageKeyset},
        usage_analytics::UsageAnalyticsStore,
    },
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const TEST_PEPPER: &[u8] = b"clickhouse-analytics-integration-pepper";

fn required(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!("{name} must be set in CI")
        }
        _ => None,
    }
}

fn test_urls() -> Option<(String, String)> {
    let database = required("GATEWAY_TEST_DATABASE_URL")?;
    let clickhouse = required("GATEWAY_TEST_CLICKHOUSE_URL")?;
    Some((database, clickhouse))
}

async fn connect_test_pool(database_url: &str) -> PgPool {
    let mut last_error = None;
    for _ in 0..15 {
        match PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(2))
            .connect(database_url)
            .await
        {
            Ok(pool) => return pool,
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(StdDuration::from_secs(1)).await;
            }
        }
    }
    panic!("GATEWAY_TEST_DATABASE_URL remained unreachable: {last_error:?}");
}

/// A ClickHouse client for `url` and `database`, with the test server's
/// credentials.
fn clickhouse_client(url: &str, database: &str) -> ClickHouse {
    let user = std::env::var("GATEWAY_TEST_CLICKHOUSE_USER").ok();
    let password = std::env::var("GATEWAY_TEST_CLICKHOUSE_PASSWORD").ok();
    let config = ClickHouseConfig::from_lookup(&|name: &str| match name {
        "GATEWAY_CLICKHOUSE_URL" => Some(url.to_owned()),
        "GATEWAY_CLICKHOUSE_DATABASE" => Some(database.to_owned()),
        "GATEWAY_CLICKHOUSE_USER" => user.clone(),
        "GATEWAY_CLICKHOUSE_PASSWORD" => password.clone(),
        _ => None,
    })
    .unwrap()
    .unwrap();
    ClickHouse::new(&config).unwrap()
}

/// A small deterministic generator, so every run seeds the same mix.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[derive(Clone, Copy)]
enum Closing {
    Succeeded,
    Pending,
    Failed,
    Expired,
    Unknown,
    FailedOver,
}

/// One usage row: amount, input, output, cached, reasoning, completeness,
/// confidence and how long after the request it was recorded.
#[derive(Clone)]
struct Usage {
    amount: Option<i64>,
    tokens: [i64; 5],
    completeness: &'static str,
    confidence: &'static str,
    after: Duration,
    currency: Option<&'static str>,
}

struct Request {
    team: Option<TeamId>,
    account: ServiceAccountId,
    api_key: Option<Uuid>,
    end_user: Option<Uuid>,
    currency: &'static str,
    recorded_at: DateTime<Utc>,
    duration_ms: i64,
    closing: Closing,
    route: Option<ResolvedCatalogRoute>,
    batch: Option<Uuid>,
    usage: Vec<Usage>,
    trace_amount: Option<i64>,
}

/// What a recorded request's ids are.
#[derive(Clone, Copy, Debug)]
struct Recorded {
    reservation: Uuid,
    trace: Uuid,
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    database: String,
    clickhouse: ClickHouse,
    sink: AnalyticsSinkStore,
    analytics: UsageAnalyticsStore,
    reporting: ReportingStore,
    reader: ClickHouseAnalytics,
}

impl Harness {
    /// A fresh ClickHouse database and the sink turned off.
    async fn new(database_url: &str, clickhouse_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let database = format!("gateway_test_{}", Uuid::new_v4().simple());
        let clickhouse = clickhouse_client(clickhouse_url, &database);
        let harness = Self {
            identity,
            database,
            sink: AnalyticsSinkStore::new(pool.clone()),
            analytics: UsageAnalyticsStore::new(pool.clone()),
            reporting: ReportingStore::new(pool.clone()),
            reader: ClickHouseAnalytics::new(clickhouse.clone(), pool.clone()),
            clickhouse,
            pool,
        };
        harness.turn_off().await;
        harness
    }

    async fn turn_off(&self) {
        sqlx::query("DELETE FROM gateway.analytics_sink")
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM gateway.analytics_sink_queue")
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// Turns the sink off and drops this test's ClickHouse database.
    async fn finish(self) {
        self.turn_off().await;
        let _ = self
            .clickhouse
            .execute(&format!("DROP DATABASE IF EXISTS {}", self.database), &[])
            .await;
    }

    fn runner(&self) -> AnalyticsSinkRunner {
        AnalyticsSinkRunner::new(self.sink.clone(), Some(self.clickhouse.clone()))
    }

    /// Ticks until the sink is live: turned on, emptied, history copied.
    async fn go_live(&self, runner: &AnalyticsSinkRunner) {
        runner.tick().await.unwrap();
        // Every process stops trusting an "off" it cached (production waits
        // `ENABLE_SETTLE` for this before the copy; the test skips ahead).
        tokio::time::sleep(OFF_CACHE + StdDuration::from_millis(100)).await;
        // Past the settle period every process waits out before the copy.
        sqlx::query(
            "UPDATE gateway.analytics_sink SET enabled_at = enabled_at - interval '1 minute'",
        )
        .execute(&self.pool)
        .await
        .unwrap();
        for _ in 0..20 {
            runner.tick().await.unwrap();
            if self.phase().await == Some(Phase::Live) {
                return;
            }
        }
        panic!("the sink never went live");
    }

    async fn phase(&self) -> Option<Phase> {
        self.sink.state().await.unwrap().map(|state| state.phase)
    }

    /// Writes every queued request.
    async fn drain(&self, runner: &AnalyticsSinkRunner) {
        for _ in 0..20 {
            runner.tick().await.unwrap();
            if self.sink.queue_stats().await.unwrap().depth == 0 {
                return;
            }
        }
        let left: Vec<(Uuid, i32, Option<String>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT reservation_id, attempts, last_error, next_attempt_at
             FROM gateway.analytics_sink_queue",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap();
        panic!("the queue never drained: {left:?}");
    }

    /// Dispatches every due event with the analytics sink alone.
    async fn dispatch(&self) {
        let outbox = OutboxStore::new(self.pool.clone());
        let sinks = [&AnalyticsSink as &dyn gateway::persistence::OutboxSink];
        while outbox
            .dispatch_batch(&Sinks(&sinks), Utc::now() + Duration::days(1), 500)
            .await
            .unwrap()
            .dispatched
            > 0
        {}
    }

    async fn org(&self, slug: &str) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id
    }

    async fn user(&self, org: OrganizationId, name: &str) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some(name))
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, MembershipRole::Owner)
            .await
            .unwrap();
        user
    }

    async fn end_user(&self, org: OrganizationId, account: ServiceAccountId, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(org.0)
        .bind(account.0)
        .bind(name)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    async fn batch(&self, org: OrganizationId, account: ServiceAccountId) -> Uuid {
        let file = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.batch_file
                 (id, organization_id, service_account_id, purpose, filename, bytes, sha256,
                  object_key)
             VALUES ($1, $2, $3, 'batch', 'f.jsonl', 1, $4, $5)",
        )
        .bind(file)
        .bind(org.0)
        .bind(account.0)
        .bind(vec![0_u8; 32])
        .bind(format!("batch-files/{}/{file}", org.0))
        .execute(&self.pool)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.batch
                 (id, organization_id, service_account_id, idempotency_key, fingerprint, endpoint,
                  operation, input_file_id, completion_window, status, total_lines, expires_at)
             VALUES ($1, $2, $3, $4, $5, '/v1/chat/completions', 'chat_batch', $6, '24h',
                     'completed', 2, now() + interval '1 day')",
        )
        .bind(id)
        .bind(org.0)
        .bind(account.0)
        .bind(id.to_string())
        .bind(vec![0_u8; 32])
        .bind(file)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    /// Records a request: its reservation, trace, usage rows and (when
    /// `emit`) its `inference.finalized` event.
    async fn record(&self, org: OrganizationId, request: &Request, emit: bool) -> Recorded {
        let reservation = Uuid::new_v4();
        let trace = Uuid::new_v4();
        let route = request.route.as_ref();
        let deployment = route.map_or("raw-deploy", |route| route.deployment.as_str());
        let model = route.map_or("raw-model", |route| route.model.as_str());
        let pending = matches!(request.closing, Closing::Pending);
        let state = match request.closing {
            Closing::Pending => "claimed",
            Closing::Expired => "expired",
            // Awaiting an operator (`accounting_reconciliation` below).
            Closing::Unknown => "unknown",
            _ => "settled",
        };
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, team_id, service_account_id, idempotency_key, fingerprint,
                  state, reserved_minor, expires_at, output_token_cap, input_price_minor,
                  output_price_minor, cached_input_price_minor, reasoning_price_minor,
                  other_price_minor, price_version, pricing_confidence, currency, deployment,
                  model, trace_id, catalog_release_id, catalog_route_id, catalog_deployment_id,
                  catalog_price_id, catalog_digest, catalog_operation, catalog_family,
                  catalog_price_source, catalog_chat_input_count_mode, batch_id, end_user_id,
                  created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $20, 500, $7, 100, 1, 1, $25, $25, $25, 'v1',
                     'known', $8, $9, $21, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19,
                     $22, $23, $24)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(request.team.map(|team| team.0))
        .bind(request.account.0)
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        // An in-flight request's reservation never expires, so the expiry
        // worker of another test never closes it (or skips its own for it).
        .bind(if pending {
            DateTime::parse_from_rfc3339("2100-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        } else {
            request.recorded_at + Duration::minutes(5)
        })
        .bind(request.currency)
        .bind(deployment)
        .bind(trace)
        .bind(route.map(|route| route.release_id))
        .bind(route.map(|route| route.route_id))
        .bind(route.map(|route| route.deployment_id))
        .bind(route.map(|route| route.price_id))
        .bind(route.map(|route| route.digest.0.to_vec()))
        .bind(route.map(|route| route.operation.as_str()))
        .bind(route.map(|route| route.family.as_str()))
        .bind(route.map(|route| route.source.as_str()))
        .bind(route.map(|route| route.chat_input_count_mode.as_str()))
        .bind(state)
        .bind(model)
        .bind(request.batch)
        .bind(request.end_user)
        .bind(request.recorded_at)
        .bind(route.map_or(1, |route| route.price.cached_input_minor_per_token))
        .execute(&self.pool)
        .await
        .unwrap();
        let outcome = match request.closing {
            Closing::Pending => None,
            Closing::Succeeded | Closing::Failed => Some("settled"),
            Closing::Expired => Some("expired"),
            Closing::Unknown => Some("unknown"),
            Closing::FailedOver => Some("failed-over"),
        };
        let error = match request.closing {
            Closing::Failed => Some("rate_limited"),
            Closing::FailedOver => Some("unavailable"),
            _ => None,
        };
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, team_id, service_account_id, reservation_id, correlation_id,
                  operation, status, outcome, model, input_bound, output_cap, price_version,
                  pricing_confidence, currency, amount_minor, metadata, recorded_at, finalized_at,
                  api_key_id, provider_error_category, catalog_release_id, catalog_route_id,
                  catalog_deployment_id, catalog_price_id, catalog_digest, catalog_operation,
                  catalog_family, catalog_price_source, catalog_chat_input_count_mode,
                  end_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, 'inference', $7, $8, $24, 64, 100, 'v1',
                     'known', $9, $10,
                     jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency', $9::text),
                     $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $25)",
        )
        .bind(trace)
        .bind(org.0)
        .bind(request.team.map(|team| team.0))
        .bind(request.account.0)
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(if pending { "pending" } else { "finalized" })
        .bind(outcome)
        .bind(request.currency)
        .bind(request.trace_amount)
        .bind(request.recorded_at)
        .bind((!pending).then(|| request.recorded_at + Duration::milliseconds(request.duration_ms)))
        .bind(request.api_key)
        .bind(error)
        .bind(route.map(|route| route.release_id))
        .bind(route.map(|route| route.route_id))
        .bind(route.map(|route| route.deployment_id))
        .bind(route.map(|route| route.price_id))
        .bind(route.map(|route| route.digest.0.to_vec()))
        .bind(route.map(|route| route.operation.as_str()))
        .bind(route.map(|route| route.family.as_str()))
        .bind(route.map(|route| route.source.as_str()))
        .bind(route.map(|route| route.chat_input_count_mode.as_str()))
        .bind(model)
        .bind(request.end_user)
        .execute(&self.pool)
        .await
        .unwrap();
        if matches!(request.closing, Closing::Unknown) {
            sqlx::query(
                "INSERT INTO gateway.accounting_reconciliation
                     (id, organization_id, reservation_id, reason)
                 VALUES ($1, $2, $3, 'expired-after-claim')",
            )
            .bind(Uuid::new_v4())
            .bind(org.0)
            .bind(reservation)
            .execute(&self.pool)
            .await
            .unwrap();
        }
        for usage in &request.usage {
            self.add_usage(
                org,
                reservation,
                request.recorded_at,
                request.currency,
                usage,
            )
            .await;
        }
        if emit {
            self.emit(org, reservation, trace, request.recorded_at)
                .await;
        }
        Recorded { reservation, trace }
    }

    async fn add_usage(
        &self,
        org: OrganizationId,
        reservation: Uuid,
        at: DateTime<Utc>,
        currency: &str,
        usage: &Usage,
    ) {
        let [input, output, cached, reasoning, other] = usage.tokens;
        sqlx::query(
            "INSERT INTO gateway.accounting_usage
                 (id, reservation_id, organization_id, currency, confidence, input_tokens,
                  output_tokens, cached_input_tokens, reasoning_tokens, other_tokens,
                  amount_minor, completeness, finality, recorded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'final', $13)",
        )
        .bind(Uuid::new_v4())
        .bind(reservation)
        .bind(org.0)
        .bind(usage.currency.unwrap_or(currency))
        .bind(usage.confidence)
        .bind(input)
        .bind(output)
        .bind(cached)
        .bind(reasoning)
        .bind(other)
        .bind(usage.amount)
        .bind(usage.completeness)
        .bind(at + usage.after)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn emit(&self, org: OrganizationId, reservation: Uuid, trace: Uuid, at: DateTime<Utc>) {
        let mut conn = self.pool.acquire().await.unwrap();
        outbox::emit(
            &mut conn,
            org,
            &OutboxPayload::InferenceFinalized(InferenceFinalized {
                trace_id: trace,
                reservation_id: reservation,
                team_id: None,
                service_account_id: None,
                model: "m".into(),
                currency: "USD".into(),
                outcome: "settled".into(),
                amount_minor: None,
                finalized_at: at,
            }),
        )
        .await
        .unwrap();
    }

    /// The organization's live rows in ClickHouse.
    async fn stored(&self, org: OrganizationId) -> Vec<StoredRow> {
        self.clickhouse
            .query(
                "SELECT toString(reservation_id) AS reservation_id, has_trace,
                        toInt64(version) AS version, input_tokens
                 FROM requests FINAL WHERE organization_id = {org:UUID}
                 ORDER BY reservation_id",
                &[Param::uuid("org", org.0)],
                Vec::new(),
            )
            .await
            .unwrap()
    }
}

#[derive(Debug, Deserialize)]
struct StoredRow {
    reservation_id: Uuid,
    has_trace: u8,
    version: i64,
    input_tokens: Option<i64>,
}

fn manifest() -> CatalogManifest {
    let capability = json!({"operation": "chat", "max_input_bytes": 4096, "max_output_tokens": 32,
        "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 30,
        "chat_input_count_mode": "unsupported"});
    let deployments = [
        ("az-gpt", "gpt-4o", "azure-main", "eastus"),
        ("oa-gpt", "gpt-4o", "openai-main", "global"),
        ("br-claude", "claude-x", "bedrock-main", "us-east-1"),
        ("local-qwen", "Qwen-Ünïcode", "local", "rack-1"),
    ];
    serde_json::from_value(json!({
        "release": {"version": "clickhouse-parity"},
        "providers": [
            {"adapter_instance_key": "azure-main", "family": "azure-openai"},
            {"adapter_instance_key": "openai-main", "family": "openai"},
            {"adapter_instance_key": "bedrock-main", "family": "bedrock"},
            {"adapter_instance_key": "local", "family": "openai-compatible"}
        ],
        "deployments": deployments.iter().map(|(name, model, account, location)| json!({
            "name": name, "model": model, "location": location, "adapter_instance_key": account,
            "capabilities": [capability.clone()]
        })).collect::<Vec<_>>(),
        "prices": deployments.iter().map(|(name, _, _, _)| json!({
            "deployment": name, "operation": "chat",
            "input_minor_per_token": 1, "output_minor_per_token": 1,
            "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
            "other_minor_per_token": 0, "currency": "USD", "source": "test",
            "source_version": "v1", "confidence": "known"
        })).collect::<Vec<_>>(),
        "routes": deployments.iter().map(|(name, _, _, _)| json!({
            "alias": format!("{name}-alias"), "operation": "chat", "deployment": name
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

/// The values a fixture's queries filter by.
struct Fixture {
    org: OrganizationId,
    other_org: OrganizationId,
    team_a: TeamId,
    team_b: TeamId,
    account_a: ServiceAccountId,
    account_b: ServiceAccountId,
    owner_b: UserAccountId,
    key_a: Uuid,
    batch: Uuid,
    some_trace: Uuid,
    some_correlation: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    /// Builds more requests like the fixture's.
    rng: Rng,
    routes: Vec<ResolvedCatalogRoute>,
    end_users: Vec<Uuid>,
    key_b: Uuid,
    account_org: ServiceAccountId,
}

const DAY: &str = "2026-06-10T00:00:00Z";

fn day() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(DAY)
        .unwrap()
        .with_timezone(&Utc)
}

impl Fixture {
    /// A random request of the fixture's organization.
    fn request(&mut self, batch: Option<Uuid>) -> Request {
        let rng = &mut self.rng;
        let (team, account, api_key) = match rng.below(3) {
            0 => (Some(self.team_a), self.account_a, Some(self.key_a)),
            1 => (Some(self.team_b), self.account_b, Some(self.key_b)),
            _ => (None, self.account_org, None),
        };
        let closing = *rng.pick(&[
            Closing::Succeeded,
            Closing::Succeeded,
            Closing::Succeeded,
            Closing::Succeeded,
            Closing::Pending,
            Closing::Failed,
            Closing::Expired,
            Closing::Unknown,
            Closing::FailedOver,
        ]);
        let route = match rng.below(5) {
            4 => None,
            index => Some(self.routes[index as usize].clone()),
        };
        // Catalog prices are in USD; uncatalogued requests vary.
        let currency = if route.is_none() && rng.below(2) == 0 {
            "EUR"
        } else {
            "USD"
        };
        let usage_rows = if matches!(closing, Closing::Pending | Closing::Expired) {
            0
        } else {
            rng.below(3)
        };
        let mut usage = Vec::new();
        for index in 0..usage_rows {
            usage.push(Usage {
                amount: (rng.below(6) != 0).then(|| rng.below(500) as i64),
                tokens: [
                    rng.below(100) as i64,
                    rng.below(50) as i64,
                    rng.below(3) as i64,
                    rng.below(2) as i64,
                    rng.below(2) as i64,
                ],
                completeness: *rng.pick(&["complete", "complete", "partial", "unavailable"]),
                confidence: *rng.pick(&["exact", "exact", "estimated", "unknown"]),
                // A late row may land in another bucket or window.
                after: Duration::milliseconds(10)
                    + Duration::hours(index as i64 * rng.below(30) as i64),
                currency: None,
            });
        }
        // Many requests share a second (and some the same microsecond), so
        // keyset order is tested on ties.
        let recorded_at = self.from
            + Duration::minutes(rng.below(3 * 24 * 60) as i64 - 60)
            + Duration::seconds(rng.below(3) as i64);
        Request {
            team,
            account,
            api_key,
            end_user: rng
                .pick(&[None, Some(0), Some(1)])
                .map(|index| self.end_users[index]),
            currency,
            recorded_at,
            duration_ms: *rng.pick(&[0, 1, 2, 20, 50, 100, 100, 250, 999, 1500, 3000]),
            closing,
            route,
            batch,
            usage,
            trace_amount: (rng.below(5) != 0).then(|| rng.below(400) as i64),
        }
    }
}

/// An organization with a few hundred varied requests over three days,
/// another organization with the same shape, and requests outside the
/// window.
async fn fixture(harness: &Harness) -> Fixture {
    let org = harness.org("clickhouse-parity").await;
    let other_org = harness.org("clickhouse-parity-other").await;
    let owner = harness.user(org, "Owner").await;
    let owner_b = harness.user(org, "Owner B").await;
    let team_a = harness.identity.create_team(org, "a").await.unwrap().id;
    let team_b = harness.identity.create_team(org, "b").await.unwrap().id;
    for (team, user) in [(team_a, owner), (team_b, owner_b)] {
        harness
            .identity
            .add_team_membership(org, team, user, MembershipRole::Admin)
            .await
            .unwrap();
    }
    let account = |owner, team, name: &'static str| {
        let identity = harness.identity.clone();
        async move {
            identity
                .create_service_account(org, owner, team, name)
                .await
                .unwrap()
                .id
        }
    };
    let account_a = account(owner, Some(team_a), "service-a").await;
    let account_b = account(owner_b, Some(team_b), "service-b").await;
    let account_org = account(owner, None, "service-org").await;
    let key = |account| {
        let identity = harness.identity.clone();
        async move {
            identity
                .create_api_key(org, account, &[ApiKeyScope::Inference], None, owner)
                .await
                .unwrap()
                .id
                .0
        }
    };
    let key_a = key(account_a).await;
    let key_b = key(account_b).await;
    let end_users = vec![
        harness.end_user(org, account_a, "alice").await,
        harness.end_user(org, account_a, "bob\\o'neil").await,
    ];
    let catalog = CatalogStore::new(harness.pool.clone());
    let manifest = manifest();
    catalog
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    // Both GPT deployments to one master, Claude to none, Qwen unlinked.
    for (family, model, master) in [
        ("azure-openai", "gpt-4o", Some("org/gpt")),
        ("openai", "gpt-4o", Some("org/gpt")),
        ("bedrock", "claude-x", None),
    ] {
        sqlx::query(
            "INSERT INTO gateway.model_master_link (organization_id, id, family, model, master)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(org.0)
        .bind(Uuid::new_v4())
        .bind(family)
        .bind(model)
        .bind(master)
        .execute(&harness.pool)
        .await
        .unwrap();
    }
    let mut routes = Vec::new();
    for alias in [
        "az-gpt-alias",
        "oa-gpt-alias",
        "br-claude-alias",
        "local-qwen-alias",
    ] {
        routes.push(
            catalog
                .resolve_active(org, alias, "chat")
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let batch = harness.batch(org, account_a).await;
    let from = day();
    let mut fixture = Fixture {
        org,
        other_org,
        team_a,
        team_b,
        account_a,
        account_b,
        owner_b,
        key_a,
        batch,
        some_trace: Uuid::nil(),
        some_correlation: Uuid::nil(),
        from,
        to: from + Duration::days(2),
        rng: Rng(0x9E37_79B9_7F4A_7C15),
        routes,
        end_users,
        key_b,
        account_org,
    };
    let mut first = None;
    for index in 0..320 {
        let batch = (index % 9 == 0).then_some(batch);
        let mut request = fixture.request(batch);
        if index % 17 == 0 {
            // Identical recorded_at for a run of requests.
            request.recorded_at = from + Duration::hours(5);
        }
        let recorded = harness.record(org, &request, true).await;
        first.get_or_insert(recorded);
    }
    let first = first.unwrap();
    fixture.some_trace = first.trace;
    fixture.some_correlation =
        sqlx::query_scalar("SELECT correlation_id FROM gateway.inference_trace WHERE id = $1")
            .bind(first.trace)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    // Another tenant: same deployments' names, its own accounts.
    let other_owner = harness.user(other_org, "Other").await;
    let other_account = harness
        .identity
        .create_service_account(other_org, other_owner, None, "other")
        .await
        .unwrap()
        .id;
    for hour in 0..30 {
        harness
            .record(
                other_org,
                &Request {
                    team: None,
                    account: other_account,
                    api_key: None,
                    end_user: None,
                    currency: "USD",
                    recorded_at: from + Duration::hours(hour),
                    duration_ms: 10,
                    closing: Closing::Succeeded,
                    route: None,
                    batch: None,
                    usage: vec![Usage {
                        amount: Some(999),
                        tokens: [9, 9, 0, 0, 0],
                        completeness: "complete",
                        confidence: "exact",
                        after: Duration::milliseconds(5),
                        currency: None,
                    }],
                    trace_amount: Some(999),
                },
                true,
            )
            .await;
    }
    fixture
}

fn slice(fx: &Fixture) -> UsageSlice {
    slice_between(fx.from, fx.to)
}

fn slice_between(from: DateTime<Utc>, to: DateTime<Utc>) -> UsageSlice {
    UsageSlice {
        from,
        to,
        team_ids: Vec::new(),
        service_account_ids: Vec::new(),
        api_key_ids: Vec::new(),
        user_ids: Vec::new(),
        end_users: Vec::new(),
        models: Vec::new(),
        aliases: Vec::new(),
        deployments: Vec::new(),
        provider_families: Vec::new(),
        locations: Vec::new(),
        operations: Vec::new(),
        statuses: Vec::new(),
        outcomes: Vec::new(),
        error_categories: Vec::new(),
        batch_ids: Vec::new(),
        model_masters: Vec::new(),
        currency: None,
    }
}

/// Each dashboard filter, alone, with values that match and one that does
/// not.
fn slices(fx: &Fixture) -> Vec<(&'static str, UsageSlice)> {
    let base = slice(fx);
    let with = |f: &dyn Fn(&mut UsageSlice)| {
        let mut slice = base.clone();
        f(&mut slice);
        slice
    };
    vec![
        ("all", base.clone()),
        ("team", with(&|s| s.team_ids = vec![fx.team_a])),
        (
            "account",
            with(&|s| s.service_account_ids = vec![fx.account_b, fx.account_a]),
        ),
        ("key", with(&|s| s.api_key_ids = vec![fx.key_a])),
        ("user", with(&|s| s.user_ids = vec![fx.owner_b.0])),
        (
            "end user",
            with(&|s| s.end_users = vec!["bob\\o'neil".into(), "nobody".into()]),
        ),
        ("model", with(&|s| s.models = vec!["gpt-4o".into()])),
        (
            "alias",
            with(&|s| s.aliases = vec!["br-claude-alias".into()]),
        ),
        (
            "deployment",
            with(&|s| s.deployments = vec!["raw-deploy".into(), "local-qwen".into()]),
        ),
        (
            "family",
            with(&|s| s.provider_families = vec!["openai".into()]),
        ),
        ("location", with(&|s| s.locations = vec!["eastus".into()])),
        ("operation", with(&|s| s.operations = vec!["chat".into()])),
        (
            "status",
            with(&|s| {
                s.statuses = vec![
                    RequestStatus::Failed,
                    RequestStatus::Unknown,
                    RequestStatus::FailedOver,
                ]
            }),
        ),
        (
            "pending",
            with(&|s| s.statuses = vec![RequestStatus::Pending]),
        ),
        ("outcome", with(&|s| s.outcomes = vec!["expired".into()])),
        (
            "error",
            with(&|s| s.error_categories = vec!["rate_limited".into()]),
        ),
        ("batch", with(&|s| s.batch_ids = vec![fx.batch])),
        (
            "master",
            with(&|s| s.model_masters = vec!["org/gpt".into()]),
        ),
        (
            "currency",
            with(&|s| s.currency = Some(Currency::new("EUR").unwrap())),
        ),
        (
            "combined",
            with(&|s| {
                s.team_ids = vec![fx.team_a, fx.team_b];
                s.models = vec!["gpt-4o".into(), "raw-model".into()];
                s.currency = Some(Currency::new("USD").unwrap());
            }),
        ),
        (
            "narrow window",
            with(&|s| {
                s.from = fx.from + Duration::hours(5);
                s.to = fx.from + Duration::hours(5) + Duration::seconds(1);
            }),
        ),
    ]
}

/// The same answers from both stores, and ClickHouse answered.
macro_rules! same {
    ($label:expr, $postgres:expr, $clickhouse:expr) => {{
        let postgres = $postgres.await.unwrap();
        let clickhouse = $clickhouse
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{}: ClickHouse did not answer", $label));
        assert_eq!(postgres, clickhouse, "{}", $label);
        clickhouse
    }};
}

/// How much of [`compare`] to run: everything, or a cross-section (each
/// query kind, filtered and not) for the states between.
#[derive(Clone, Copy, PartialEq)]
enum Depth {
    Full,
    Quick,
}

/// Every read the dashboard, the usage report and trace search make,
/// against both stores. Returns how many comparisons ran.
async fn compare(harness: &Harness, fx: &Fixture, depth: Depth) -> usize {
    let full = depth == Depth::Full;
    let mut compared = 0;
    let clamps: Vec<Option<Vec<TeamId>>> = if full {
        vec![None, Some(vec![fx.team_a])]
    } else {
        vec![Some(vec![fx.team_a, fx.team_b])]
    };
    for clamp in &clamps {
        let teams = clamp.as_deref();
        for (name, slice) in slices(fx) {
            if !full && !matches!(name, "all" | "user" | "combined" | "pending") {
                continue;
            }
            let previous = slice.previous();
            // Series: every split, granularity and ranking.
            let mut splits: Vec<Option<UsageDimension>> = vec![None];
            splits.extend(
                [
                    UsageDimension::Team,
                    UsageDimension::ServiceAccount,
                    UsageDimension::ApiKey,
                    UsageDimension::User,
                    UsageDimension::EndUser,
                    UsageDimension::Model,
                    UsageDimension::Alias,
                    UsageDimension::Deployment,
                    UsageDimension::ProviderFamily,
                    UsageDimension::Location,
                    UsageDimension::Operation,
                    UsageDimension::Status,
                    UsageDimension::Outcome,
                    UsageDimension::ErrorCategory,
                    UsageDimension::ModelMaster,
                ]
                .map(Some),
            );
            let grid: Vec<(Option<UsageDimension>, Granularity, RankBy, u32)> = if full
                && name == "all"
            {
                let mut grid = Vec::new();
                for split in &splits {
                    for granularity in [Granularity::Hour, Granularity::Day, Granularity::Week] {
                        grid.push((*split, granularity, RankBy::Requests, 3));
                    }
                    grid.push((*split, Granularity::Day, RankBy::Tokens, 2));
                }
                grid
            } else {
                vec![
                    (None, Granularity::Hour, RankBy::Requests, 3),
                    (
                        Some(UsageDimension::Model),
                        Granularity::Day,
                        RankBy::Tokens,
                        2,
                    ),
                    (
                        Some(UsageDimension::User),
                        Granularity::Day,
                        RankBy::Requests,
                        1,
                    ),
                ]
            };
            for (split, granularity, rank, limit) in grid {
                let rank = if rank == RankBy::Tokens && slice.currency.is_some() {
                    RankBy::Cost
                } else {
                    rank
                };
                let query = UsageSeriesQuery {
                    slice: slice.clone(),
                    granularity,
                    split_by: split,
                    limit,
                    rank_by: rank,
                };
                same!(
                    format!("series {name} {split:?} {granularity:?} {rank:?} {clamp:?}"),
                    harness
                        .analytics
                        .series_report(fx.org, teams, &query, &previous),
                    harness
                        .reader
                        .series_report(fx.org, teams, &query, &previous)
                );
                compared += 1;
            }
            // Events, page by page.
            let mut keyset = None;
            loop {
                let label = format!("events {name} {clamp:?} after {keyset:?}");
                let (items, more) = same!(
                    label,
                    harness.analytics.events(fx.org, teams, &slice, 23, keyset),
                    harness.reader.events(fx.org, teams, &slice, 23, keyset)
                );
                compared += 1;
                match items.last() {
                    Some(last) if more => {
                        keyset = Some(TimeKeyset {
                            at: last.recorded_at,
                            id: last.id,
                        })
                    }
                    _ => break,
                }
            }
        }
        // The usage report: every grouping and filter, page by page.
        let base = UsageQuery {
            from: fx.from,
            to: fx.to,
            group_by: vec![UsageGroupBy::Day],
            team_ids: Vec::new(),
            service_account_ids: Vec::new(),
            models: Vec::new(),
            deployments: Vec::new(),
            provider_families: Vec::new(),
            locations: Vec::new(),
            batch_ids: Vec::new(),
            model_masters: Vec::new(),
            currency: None,
        };
        let dimensions = [
            UsageGroupBy::Day,
            UsageGroupBy::Team,
            UsageGroupBy::ServiceAccount,
            UsageGroupBy::Model,
            UsageGroupBy::Deployment,
            UsageGroupBy::ProviderFamily,
            UsageGroupBy::Location,
            UsageGroupBy::ModelMaster,
        ];
        let mut queries: Vec<(String, UsageQuery)> = Vec::new();
        for first in dimensions {
            queries.push((
                format!("{first:?}"),
                UsageQuery {
                    group_by: vec![first],
                    ..base.clone()
                },
            ));
            for second in dimensions {
                if second != first && (full || second == UsageGroupBy::Day) {
                    queries.push((
                        format!("{first:?}+{second:?}"),
                        UsageQuery {
                            group_by: vec![first, second],
                            ..base.clone()
                        },
                    ));
                }
            }
        }
        let filtered = |f: &dyn Fn(&mut UsageQuery)| {
            let mut query = UsageQuery {
                group_by: vec![UsageGroupBy::Model, UsageGroupBy::Team],
                ..base.clone()
            };
            f(&mut query);
            query
        };
        for (name, query) in [
            ("team", filtered(&|q| q.team_ids = vec![fx.team_b])),
            (
                "account",
                filtered(&|q| q.service_account_ids = vec![fx.account_a]),
            ),
            ("model", filtered(&|q| q.models = vec!["claude-x".into()])),
            (
                "deployment",
                filtered(&|q| q.deployments = vec!["az-gpt".into(), "raw-deploy".into()]),
            ),
            (
                "family",
                filtered(&|q| q.provider_families = vec![ProviderFamily::AzureOpenAi]),
            ),
            (
                "location",
                filtered(&|q| q.locations = vec!["global".into()]),
            ),
            ("batch", filtered(&|q| q.batch_ids = vec![fx.batch])),
            (
                "master",
                filtered(&|q| q.model_masters = vec!["org/gpt".into()]),
            ),
            (
                "currency",
                filtered(&|q| q.currency = Some(Currency::new("GBP").unwrap())),
            ),
            (
                "window",
                filtered(&|q| {
                    q.from = fx.from + Duration::hours(30);
                    q.to = fx.from + Duration::hours(31);
                }),
            ),
        ] {
            queries.push((format!("filter {name}"), query));
        }
        for (name, query) in &queries {
            let mut keyset: Option<UsageKeyset> = None;
            loop {
                let label = format!("report {name} {clamp:?} after {keyset:?}");
                let postgres_rows = harness
                    .reporting
                    .usage_rows(fx.org, teams, query, 11, keyset.clone())
                    .await
                    .unwrap();
                let postgres_totals = harness
                    .reporting
                    .usage_totals(fx.org, teams, query)
                    .await
                    .unwrap();
                let clickhouse = harness
                    .reader
                    .usage_report(fx.org, teams, query, 11, keyset.as_ref())
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("{label}: ClickHouse did not answer"));
                assert_eq!(
                    (postgres_rows.clone(), postgres_totals),
                    clickhouse,
                    "{label}"
                );
                compared += 1;
                let (items, more) = postgres_rows;
                match items.last() {
                    Some(last) if more => keyset = Some(UsageKeyset::of(last, &query.group_by)),
                    _ => break,
                }
            }
        }
        // Trace search: every filter, page by page, and summaries.
        let traces = |f: &dyn Fn(&mut TraceQuery)| {
            let mut query = TraceQuery {
                from: Some(fx.from),
                to: Some(fx.to),
                ..TraceQuery::default()
            };
            f(&mut query);
            query
        };
        let trace_queries = [
            ("all", traces(&|_| {})),
            (
                "unbounded",
                traces(&|q| {
                    q.from = None;
                    q.to = None;
                }),
            ),
            ("team", traces(&|q| q.team_ids = vec![fx.team_a])),
            (
                "account",
                traces(&|q| q.service_account_ids = vec![fx.account_b]),
            ),
            ("key", traces(&|q| q.api_key_ids = vec![fx.key_a])),
            ("end user", traces(&|q| q.end_users = vec!["alice".into()])),
            ("model", traces(&|q| q.models = vec!["Qwen-Ünïcode".into()])),
            (
                "deployment",
                traces(&|q| q.deployments = vec!["br-claude".into()]),
            ),
            (
                "family",
                traces(&|q| q.provider_families = vec!["azure-openai".into(), "bedrock".into()]),
            ),
            ("location", traces(&|q| q.locations = vec!["rack-1".into()])),
            (
                "status",
                traces(&|q| q.statuses = vec![TraceStatus::Pending]),
            ),
            (
                "outcome",
                traces(&|q| q.outcomes = vec!["failed-over".into(), "unknown".into()]),
            ),
            ("batch", traces(&|q| q.batch_ids = vec![fx.batch])),
            ("search", traces(&|q| q.search = Some("GPT".into()))),
            ("search raw", traces(&|q| q.search = Some("aw-dep".into()))),
            ("search wildcard", traces(&|q| q.search = Some("_%".into()))),
            (
                "search trace",
                traces(&|q| q.search = Some(fx.some_trace.to_string())),
            ),
            (
                "search correlation",
                traces(&|q| q.search = Some(fx.some_correlation.to_string())),
            ),
        ];
        for (name, query) in &trace_queries {
            let mut keyset = None;
            loop {
                let label = format!("traces {name} {clamp:?} after {keyset:?}");
                let (items, more) = same!(
                    label,
                    harness
                        .reporting
                        .list_traces(fx.org, teams, query, 29, keyset),
                    harness.reader.list_traces(fx.org, teams, query, 29, keyset)
                );
                compared += 1;
                match items.last() {
                    Some(last) if more => {
                        keyset = Some(TimeKeyset {
                            at: last.recorded_at,
                            id: last.id,
                        })
                    }
                    _ => break,
                }
            }
            let mut groupings: Vec<Vec<TraceDimension>> =
                TraceDimension::ALL.iter().map(|d| vec![*d]).collect();
            groupings.push(vec![TraceDimension::Day, TraceDimension::Model]);
            groupings.push(vec![TraceDimension::Status, TraceDimension::Location]);
            if !full {
                groupings.truncate(2);
            }
            for dimensions in groupings {
                for limit in [3, 50] {
                    same!(
                        format!("summary {name} {dimensions:?} {limit} {clamp:?}"),
                        harness.reporting.summarize_traces(
                            fx.org,
                            teams,
                            query,
                            &dimensions,
                            limit
                        ),
                        harness
                            .reader
                            .summarize_traces(fx.org, teams, query, &dimensions, limit)
                    );
                    compared += 1;
                }
            }
        }
    }
    compared
}

#[tokio::test]
async fn every_read_matches_postgresql_whatever_the_sink_has_caught_up_with() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let mut fx = fixture(&harness).await;
    let runner = harness.runner();
    harness.go_live(&runner).await;
    harness.dispatch().await;
    harness.drain(&runner).await;

    // Caught up: everything is ClickHouse's own rows (pending requests
    // still come from PostgreSQL).
    let compared = compare(&harness, &fx, Depth::Full).await;
    assert!(compared > 500, "{compared}");

    // Not caught up: requests whose events wait for dispatch, requests
    // queued but not written, a late usage row on a written request, and
    // new in-flight ones. Answers stay PostgreSQL's.
    for index in 0..40 {
        let request = fx.request((index % 5 == 0).then_some(fx.batch));
        harness.record(fx.org, &request, true).await;
    }
    // Its late usage row comes after it was admitted, as any usage does
    // (the reads bound admission time by the window's end).
    let (written, written_currency, written_at): (Uuid, String, DateTime<Utc>) = sqlx::query_as(
        "SELECT r.id, btrim(r.currency), r.created_at FROM gateway.accounting_reservation r
         WHERE r.organization_id = $1 AND r.state = 'settled'
           AND EXISTS (SELECT 1 FROM gateway.accounting_usage u WHERE u.reservation_id = r.id)
         ORDER BY r.id LIMIT 1",
    )
    .bind(fx.org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    harness
        .add_usage(
            fx.org,
            written,
            written_at,
            &written_currency,
            &Usage {
                amount: Some(12_345),
                tokens: [1000, 1, 1, 1, 1],
                completeness: "complete",
                confidence: "exact",
                after: Duration::days(1),
                currency: None,
            },
        )
        .await;
    let trace: Uuid =
        sqlx::query_scalar("SELECT trace_id FROM gateway.accounting_reservation WHERE id = $1")
            .bind(written)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    harness.emit(fx.org, written, trace, written_at).await;
    compare(&harness, &fx, Depth::Quick).await;
    // Dispatched (queued), not yet written.
    harness.dispatch().await;
    assert!(harness.sink.queue_stats().await.unwrap().depth > 0);
    compare(&harness, &fx, Depth::Quick).await;
    // Written.
    harness.drain(&runner).await;
    compare(&harness, &fx, Depth::Quick).await;

    // Tenant isolation: the other organization's reads see only its own
    // thirty requests, from either store.
    let other = UsageQuery {
        from: fx.from,
        to: fx.to,
        group_by: vec![UsageGroupBy::Deployment],
        team_ids: Vec::new(),
        service_account_ids: Vec::new(),
        models: Vec::new(),
        deployments: Vec::new(),
        provider_families: Vec::new(),
        locations: Vec::new(),
        batch_ids: Vec::new(),
        model_masters: Vec::new(),
        currency: None,
    };
    let ((rows, _), totals) = harness
        .reader
        .usage_report(fx.other_org, None, &other, 50, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].request_count, 30);
    assert_eq!(totals[0].amount_minor, 30 * 999);
    let (traces, _) = harness
        .reader
        .list_traces(fx.org, None, &TraceQuery::default(), 1000, None)
        .await
        .unwrap()
        .unwrap();
    let foreign: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.inference_trace WHERE id = ANY($1) AND organization_id <> $2",
    )
    .bind(traces.iter().map(|trace| trace.id).collect::<Vec<_>>())
    .bind(fx.org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(foreign, 0);
    harness.finish().await;
}

#[tokio::test]
async fn delivery_is_at_least_once_and_writes_are_idempotent() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let mut fx = fixture(&harness).await;
    // Off: nothing is queued, by events or by triggers.
    harness.dispatch().await;
    let batch_line = fx.request(Some(fx.batch));
    harness.record(fx.org, &batch_line, false).await;
    assert_eq!(harness.sink.queue_stats().await.unwrap().depth, 0);

    let runner = harness.runner();
    harness.go_live(&runner).await;
    let backfilled = harness.stored(fx.org).await;
    assert_eq!(backfilled.len(), 321, "every reservation is copied");

    // A finalized request is queued by its event and written once, however
    // often it is delivered.
    let mut request = fx.request(None);
    request.closing = Closing::Succeeded;
    request.usage = vec![Usage {
        amount: Some(5),
        tokens: [7, 1, 0, 0, 0],
        completeness: "complete",
        confidence: "exact",
        after: Duration::milliseconds(1),
        currency: None,
    }];
    let recorded = harness.record(fx.org, &request, true).await;
    harness
        .emit(
            fx.org,
            recorded.reservation,
            recorded.trace,
            request.recorded_at,
        )
        .await;
    harness.dispatch().await;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.analytics_sink_queue
         WHERE organization_id = $1 AND reservation_id = $2",
    )
    .bind(fx.org.0)
    .bind(recorded.reservation)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(
        queued >= 2,
        "two deliveries (and the triggers'), as many entries: append only"
    );
    let report = runner.write_round(&harness.clickhouse).await.unwrap();
    assert_eq!(report.written, 1, "one write for both entries");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.analytics_sink_queue WHERE reservation_id = $1"
        )
        .bind(recorded.reservation)
        .fetch_one(&harness.pool)
        .await
        .unwrap(),
        0,
        "both entries covered by the one read"
    );
    harness.drain(&runner).await;
    let row = |rows: Vec<StoredRow>| {
        rows.into_iter()
            .find(|row| row.reservation_id == recorded.reservation)
    };
    let first = row(harness.stored(fx.org).await).expect("written");
    assert_eq!(first.input_tokens, Some(7));

    // A batch line is queued as it is admitted, before any event.
    let line = harness
        .record(fx.org, &fx.request(Some(fx.batch)), false)
        .await;
    let line_queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.analytics_sink_queue WHERE reservation_id = $1",
    )
    .bind(line.reservation)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(line_queued >= 1, "{line_queued}");

    // Queued again while being written (a late usage row): worker A claims
    // and reads the request, the change queues a new entry, and A's write
    // and acknowledgement leave that entry queued; the next round writes
    // the newer state, and A's older read never wins, however late it
    // lands.
    harness
        .emit(
            fx.org,
            recorded.reservation,
            recorded.trace,
            request.recorded_at,
        )
        .await;
    harness.dispatch().await;
    let claimed = harness
        .sink
        .claim(Utc::now(), Utc::now() + Duration::minutes(5), 1000)
        .await
        .unwrap();
    assert!(claimed.iter().any(|c| c.reservation_id == line.reservation));
    assert!(
        claimed
            .iter()
            .any(|c| c.reservation_id == recorded.reservation)
    );
    let stale_read = harness.sink.read_queued(&claimed).await.unwrap();
    let stale = stale_read.rows;
    harness
        .add_usage(
            fx.org,
            recorded.reservation,
            request.recorded_at,
            request.currency,
            &Usage {
                amount: Some(9),
                tokens: [70, 1, 0, 0, 0],
                completeness: "complete",
                confidence: "exact",
                after: Duration::seconds(1),
                currency: None,
            },
        )
        .await;
    harness
        .emit(
            fx.org,
            recorded.reservation,
            recorded.trace,
            request.recorded_at,
        )
        .await;
    harness.dispatch().await;
    harness
        .clickhouse
        .insert("requests", stale.iter().cloned().map(|row| row.into_row()))
        .await
        .unwrap();
    let covered: Vec<i64> = stale_read.covered.iter().map(|(id, _)| *id).collect();
    let removed = harness.sink.acknowledge(&covered).await.unwrap();
    assert_eq!(removed as usize, covered.len());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.analytics_sink_queue WHERE reservation_id = $1"
        )
        .bind(recorded.reservation)
        .fetch_one(&harness.pool)
        .await
        .unwrap(),
        2,
        "the changes after the read (the usage row, its event) stay queued"
    );
    assert_eq!(
        row(harness.stored(fx.org).await).unwrap().input_tokens,
        Some(7)
    );
    harness.drain(&runner).await;
    let newer = row(harness.stored(fx.org).await).unwrap();
    assert_eq!(newer.input_tokens, Some(70));
    assert!(newer.version > first.version);
    harness
        .clickhouse
        .insert("requests", stale.into_iter().map(|row| row.into_row()))
        .await
        .unwrap();
    let after = row(harness.stored(fx.org).await).unwrap();
    assert_eq!(after.input_tokens, Some(70), "an older read never wins");
    assert_eq!(after.version, newer.version);
    assert_eq!(
        harness
            .stored(fx.org)
            .await
            .iter()
            .filter(|row| row.reservation_id == recorded.reservation)
            .count(),
        1
    );

    // A claim whose worker died is taken again after its lease.
    let entry = fx.request(Some(fx.batch));
    let orphan = harness.record(fx.org, &entry, false).await;
    let lost = harness
        .sink
        .claim(Utc::now(), Utc::now() - Duration::seconds(1), 1000)
        .await
        .unwrap();
    assert!(lost.iter().any(|c| c.reservation_id == orphan.reservation));
    harness.drain(&runner).await;
    assert!(row_of(&harness, fx.org, orphan.reservation).await.is_some());
    harness.finish().await;
}

async fn row_of(harness: &Harness, org: OrganizationId, reservation: Uuid) -> Option<StoredRow> {
    harness
        .stored(org)
        .await
        .into_iter()
        .find(|row| row.reservation_id == reservation)
}

#[tokio::test]
async fn retention_deletes_reach_clickhouse() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let mut fx = fixture(&harness).await;
    let runner = harness.runner();
    harness.go_live(&runner).await;
    // Every version stays in its own part, so FINAL has old versions to
    // hide (the end of this test reads with unsafe server settings).
    harness
        .clickhouse
        .execute("SYSTEM STOP MERGES requests", &[])
        .await
        .unwrap();
    let mut request = fx.request(None);
    request.closing = Closing::Succeeded;
    // In a team the quick comparison's clamp shows.
    request.team = Some(fx.team_a);
    request.account = fx.account_a;
    request.api_key = Some(fx.key_a);
    let kept = harness.record(fx.org, &request, true).await;
    let gone = harness.record(fx.org, &request, true).await;
    let third = harness.record(fx.org, &request, true).await;
    harness.dispatch().await;
    harness.drain(&runner).await;
    assert_eq!(
        row_of(&harness, fx.org, kept.reservation)
            .await
            .unwrap()
            .has_trace,
        1
    );

    // Trace retention deletes a trace, leaving its reservation (and usage);
    // reservation retention deletes a request. As the purge does, in
    // transactions marked as the purge's.
    let purge = async |statements: &[(&str, Uuid)]| {
        let mut tx = harness.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL gateway.retention_purge = 'on'")
            .execute(&mut *tx)
            .await
            .unwrap();
        for (sql, id) in statements {
            sqlx::query(sql).bind(id).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
    };
    purge(&[(
        "DELETE FROM gateway.inference_trace WHERE id = $1",
        kept.trace,
    )])
    .await;
    purge(&[
        (
            "DELETE FROM gateway.accounting_usage WHERE reservation_id = $1",
            gone.reservation,
        ),
        (
            "DELETE FROM gateway.inference_trace WHERE id = $1",
            gone.trace,
        ),
        (
            "DELETE FROM gateway.accounting_reservation WHERE id = $1",
            gone.reservation,
        ),
    ])
    .await;
    // Before the write, reads already agree with PostgreSQL.
    compare(&harness, &fx, Depth::Quick).await;
    harness.drain(&runner).await;
    assert_eq!(
        row_of(&harness, fx.org, kept.reservation)
            .await
            .unwrap()
            .has_trace,
        0
    );
    assert!(row_of(&harness, fx.org, gone.reservation).await.is_none());
    compare(&harness, &fx, Depth::Quick).await;

    // A server or profile default that filters before FINAL would match
    // the old versions that the newest hides; the gateway pins its own
    // values per query, so answers stay right. A request deleted on its
    // own is written alone, so its tombstone is a block whose
    // `recorded_at` is all NULL: a skip index on it (inexact mode) would
    // drop that block from a window's read and bring the old version back.
    purge(&[
        (
            "DELETE FROM gateway.accounting_usage WHERE reservation_id = $1",
            third.reservation,
        ),
        (
            "DELETE FROM gateway.inference_trace WHERE id = $1",
            third.trace,
        ),
        (
            "DELETE FROM gateway.accounting_reservation WHERE id = $1",
            third.reservation,
        ),
    ])
    .await;
    harness.drain(&runner).await;
    assert!(row_of(&harness, fx.org, third.reservation).await.is_none());
    let unsafe_user = format!("gateway_unsafe_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    harness
        .clickhouse
        .execute(
            &format!(
                "CREATE USER {unsafe_user} IDENTIFIED WITH sha256_password BY '{password}'
                 SETTINGS use_skip_indexes_if_final = 1,
                          use_skip_indexes_if_final_exact_mode = 0,
                          optimize_move_to_prewhere_if_final = 1,
                          optimize_move_to_prewhere = 1"
            ),
            &[],
        )
        .await
        .expect("the test's ClickHouse user may create users (access management)");
    harness
        .clickhouse
        .execute(
            &format!("GRANT SELECT ON {}.* TO {unsafe_user}", harness.database),
            &[],
        )
        .await
        .unwrap();
    let config = ClickHouseConfig::from_lookup(&|name: &str| match name {
        "GATEWAY_CLICKHOUSE_URL" => Some(clickhouse_url.clone()),
        "GATEWAY_CLICKHOUSE_DATABASE" => Some(harness.database.clone()),
        "GATEWAY_CLICKHOUSE_USER" => Some(unsafe_user.clone()),
        "GATEWAY_CLICKHOUSE_PASSWORD" => Some(password.clone()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let unsafe_client = ClickHouse::new(&config).unwrap();
    let defaults: Vec<serde_json::Value> = unsafe_client
        .query(
            "SELECT value FROM system.settings
             WHERE name = 'use_skip_indexes_if_final_exact_mode'",
            &[],
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(defaults[0]["value"], "0", "the user's profile is unsafe");
    let mut unsafe_harness = harness;
    let safe_reader = std::mem::replace(
        &mut unsafe_harness.reader,
        ClickHouseAnalytics::new(unsafe_client, unsafe_harness.pool.clone()),
    );
    compare(&unsafe_harness, &fx, Depth::Quick).await;
    unsafe_harness.reader = safe_reader;
    unsafe_harness
        .clickhouse
        .execute(&format!("DROP USER IF EXISTS {unsafe_user}"), &[])
        .await
        .unwrap();
    unsafe_harness.finish().await;
}

#[tokio::test]
async fn changes_no_event_follows_reach_clickhouse() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let mut fx = fixture(&harness).await;
    let runner = harness.runner();
    harness.go_live(&runner).await;
    harness.dispatch().await;
    harness.drain(&runner).await;

    // A request that ended unknown, with an open reconciliation item.
    let mut request = fx.request(None);
    request.closing = Closing::Unknown;
    request.team = Some(fx.team_a);
    request.account = fx.account_a;
    request.api_key = Some(fx.key_a);
    let unknown = harness.record(fx.org, &request, true).await;
    harness.dispatch().await;
    harness.drain(&runner).await;
    // Trace retention deletes its trace; the sink writes that.
    let mut tx = harness.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL gateway.retention_purge = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gateway.inference_trace WHERE id = $1")
        .bind(unknown.trace)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    harness.drain(&runner).await;
    assert_eq!(
        row_of(&harness, fx.org, unknown.reservation)
            .await
            .unwrap()
            .has_trace,
        0
    );
    // An operator resolves the item: usage is added and the reservation
    // settled, but with no trace there is no `inference.finalized`.
    let item: Uuid = sqlx::query_scalar(
        "SELECT id FROM gateway.accounting_reconciliation WHERE reservation_id = $1",
    )
    .bind(unknown.reservation)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    let last_event: i64 =
        sqlx::query_scalar("SELECT COALESCE(max(sequence), 0) FROM gateway.outbox_event")
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    let mut tx = harness.pool.begin().await.unwrap();
    let resolution =
        gateway::AccountingStore::resolve_by_operator(&mut tx, fx.org, item, 4_321, fx.owner_b)
            .await
            .unwrap();
    assert_eq!(
        resolution,
        gateway::persistence::accounting::OperatorResolution::Resolved
    );
    tx.commit().await.unwrap();
    let finalized: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.outbox_event
         WHERE event_type = 'inference.finalized'
           AND payload->>'reservation_id' = $1::text AND sequence > $2",
    )
    .bind(unknown.reservation)
    .bind(last_event)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(finalized, 0, "no event follows the resolution");
    harness.dispatch().await;
    harness.drain(&runner).await;
    let amount: Option<i64> = harness
        .clickhouse
        .query::<serde_json::Value>(
            "SELECT usage_amount_minor FROM requests FINAL WHERE reservation_id = {r:UUID}",
            &[Param::uuid("r", unknown.reservation)],
            Vec::new(),
        )
        .await
        .unwrap()[0]["usage_amount_minor"]
        .as_i64();
    assert_eq!(
        amount,
        Some(4_321),
        "the resolved charge reached ClickHouse"
    );
    assert_eq!(harness.sink.queue_stats().await.unwrap().depth, 0);
    compare(&harness, &fx, Depth::Quick).await;
    harness.finish().await;
}

#[tokio::test]
async fn reads_use_clickhouse_only_from_the_store_copied_to() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let fx = fixture(&harness).await;
    let runner = harness.runner();
    harness.go_live(&runner).await;
    harness.dispatch().await;
    harness.drain(&runner).await;
    let read = |reader: &ClickHouseAnalytics| {
        let reader = reader.clone();
        let slice = slice(&fx);
        let org = fx.org;
        async move { reader.events(org, None, &slice, 5, None).await.unwrap() }
    };
    assert!(read(&harness.reader).await.is_some());
    // Another store, initialized (schema and an id of its own) but never
    // copied to: an API moved to its URL before the worker must read
    // PostgreSQL, though every query there would succeed.
    let other_database = format!("gateway_test_{}", Uuid::new_v4().simple());
    let other = clickhouse_client(&clickhouse_url, &other_database);
    other.migrate().await.unwrap();
    other
        .execute(
            "INSERT INTO gateway_meta (id, instance_id) VALUES (1, generateUUIDv4())",
            &[],
        )
        .await
        .unwrap();
    assert!(other.store_id().await.unwrap().is_some());
    let moved = ClickHouseAnalytics::new(other.clone(), harness.pool.clone());
    assert!(read(&moved).await.is_none(), "another store: PostgreSQL");
    // An empty one, likewise.
    let empty_database = format!("gateway_test_{}", Uuid::new_v4().simple());
    let empty = clickhouse_client(&clickhouse_url, &empty_database);
    empty.migrate().await.unwrap();
    let blank = ClickHouseAnalytics::new(empty.clone(), harness.pool.clone());
    assert!(
        read(&blank).await.is_none(),
        "a store without an id: PostgreSQL"
    );
    // The store copied to still answers.
    assert!(read(&harness.reader).await.is_some());
    for database in [other_database, empty_database] {
        harness
            .clickhouse
            .execute(&format!("DROP DATABASE IF EXISTS {database}"), &[])
            .await
            .unwrap();
    }
    harness.finish().await;
}

#[tokio::test]
async fn a_row_clickhouse_refuses_holds_up_only_itself() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let mut fx = fixture(&harness).await;
    let runner = harness.runner();
    harness.go_live(&runner).await;
    harness.dispatch().await;
    harness.drain(&runner).await;
    let mut recorded = Vec::new();
    for _ in 0..9 {
        let mut request = fx.request(None);
        request.closing = Closing::Succeeded;
        recorded.push(harness.record(fx.org, &request, true).await);
    }
    let poison = recorded[4].reservation;
    // ClickHouse refuses this one request's row, whatever it holds.
    harness
        .clickhouse
        .execute(
            &format!(
                "ALTER TABLE requests ADD CONSTRAINT poison CHECK reservation_id != '{poison}'"
            ),
            &[],
        )
        .await
        .unwrap();
    harness.dispatch().await;
    let report = runner.write_round(&harness.clickhouse).await.unwrap();
    assert_eq!(report.failed, 1, "{report:?}");
    for entry in &recorded {
        assert_eq!(
            row_of(&harness, fx.org, entry.reservation).await.is_some(),
            entry.reservation != poison
        );
    }
    let left: Vec<(Uuid, i32, bool)> = sqlx::query_as(
        "SELECT reservation_id, attempts, next_attempt_at > now() FROM gateway.analytics_sink_queue",
    )
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert!(!left.is_empty());
    assert!(
        left.iter().all(|entry| *entry == (poison, 1, true)),
        "only its entries stay, all backing off: {left:?}"
    );
    // Once ClickHouse takes it, it is written like any other.
    harness
        .clickhouse
        .execute("ALTER TABLE requests DROP CONSTRAINT poison", &[])
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.analytics_sink_queue SET next_attempt_at = now()")
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.drain(&runner).await;
    assert!(row_of(&harness, fx.org, poison).await.is_some());
    compare(&harness, &fx, Depth::Quick).await;
    harness.finish().await;
}

#[tokio::test]
async fn a_dropped_table_comes_back_and_the_sink_copies_history_again() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let fx = fixture(&harness).await;
    let runner = harness.runner();
    harness.go_live(&runner).await;
    harness.dispatch().await;
    harness.drain(&runner).await;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway.accounting_reservation")
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    let copied = async || -> u64 {
        #[derive(serde::Deserialize)]
        struct Count {
            n: u64,
        }
        let rows: Vec<Count> = harness
            .clickhouse
            .query("SELECT count() AS n FROM requests FINAL", &[], Vec::new())
            .await
            .unwrap();
        rows[0].n
    };
    assert_eq!(copied().await, total as u64);
    let mut previous = harness.sink.state().await.unwrap().unwrap().instance_id;

    for table in ["gateway_meta", "requests"] {
        harness
            .clickhouse
            .execute(&format!("DROP TABLE {table}"), &[])
            .await
            .unwrap();
        // The next store check (a fresh worker's first tick; within a
        // minute on a running one) notices, puts the table back and copies
        // history again; meanwhile reads use PostgreSQL.
        let runner = harness.runner();
        let mut live_again = false;
        for _ in 0..30 {
            let _ = runner.tick().await;
            if harness.phase().await != Some(Phase::Live) {
                assert!(
                    harness
                        .reader
                        .events(fx.org, None, &slice(&fx), 5, None)
                        .await
                        .unwrap()
                        .is_none(),
                    "PostgreSQL answers while the copy is redone"
                );
            } else if harness.sink.state().await.unwrap().unwrap().instance_id != previous
                && copied().await == total as u64
            {
                live_again = true;
                break;
            }
        }
        assert!(live_again, "the sink recovered after {table} was dropped");
        assert_eq!(
            harness.sink.state().await.unwrap().unwrap().backfill_rows,
            total
        );
        compare(&harness, &fx, Depth::Quick).await;
        previous = harness.sink.state().await.unwrap().unwrap().instance_id;
    }
    harness.finish().await;
}

#[tokio::test]
async fn reads_use_postgresql_until_the_backfill_completes_and_whenever_clickhouse_fails() {
    let Some((database_url, clickhouse_url)) = test_urls() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let mut fx = fixture(&harness).await;
    let query = UsageSeriesQuery {
        slice: slice(&fx),
        granularity: Granularity::Day,
        split_by: None,
        limit: 3,
        rank_by: RankBy::Requests,
    };
    let previous = query.slice.previous();
    let org = fx.org;
    let answers = || harness.reader.series_report(org, None, &query, &previous);
    // Off.
    assert!(answers().await.unwrap().is_none());
    // On, still settling and then copying: PostgreSQL answers.
    let runner = harness.runner();
    runner.tick().await.unwrap();
    assert_eq!(harness.phase().await, Some(Phase::Starting));
    assert!(answers().await.unwrap().is_none());
    sqlx::query("UPDATE gateway.analytics_sink SET enabled_at = enabled_at - interval '1 minute'")
        .execute(&harness.pool)
        .await
        .unwrap();
    runner.tick().await.unwrap();
    assert_eq!(harness.phase().await, Some(Phase::Backfilling));
    assert!(answers().await.unwrap().is_none());
    // A replaced worker resumes the copy from its cursor once the lease
    // ends.
    sqlx::query(
        "UPDATE gateway.analytics_sink SET backfill_cursor = (
             SELECT id FROM gateway.accounting_reservation ORDER BY id OFFSET 10 LIMIT 1),
             lease_until = now() - interval '1 second'",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    let other = harness.runner();
    for _ in 0..10 {
        other.tick().await.unwrap();
        if harness.phase().await == Some(Phase::Live) {
            break;
        }
    }
    assert_eq!(harness.phase().await, Some(Phase::Live));
    let state = harness.sink.state().await.unwrap().unwrap();
    let total: i64 = sqlx::query_scalar("SELECT count(*) - 11 FROM gateway.accounting_reservation")
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(
        state.backfill_rows, total,
        "the copy resumed after its cursor"
    );
    // The first eleven were skipped by the resumed copy; a restart copies
    // everything again. Live: ClickHouse answers.
    sqlx::query("UPDATE gateway.analytics_sink SET phase = 'starting', instance_id = NULL, backfill_completed_at = NULL")
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.go_live(&runner).await;
    harness.dispatch().await;
    harness.drain(&runner).await;
    assert!(answers().await.unwrap().is_some());

    // ClickHouse unreachable: reads fall back, writes back off and stay
    // queued, and nothing else waits on it.
    let down = clickhouse_client("http://127.0.0.1:9", &harness.database);
    let down_reader = ClickHouseAnalytics::new(down.clone(), harness.pool.clone());
    assert!(
        down_reader
            .series_report(fx.org, None, &query, &previous)
            .await
            .unwrap()
            .is_none()
    );
    let mut request = fx.request(None);
    request.closing = Closing::Succeeded;
    harness.record(fx.org, &request, true).await;
    harness.dispatch().await;
    let down_runner = AnalyticsSinkRunner::new(harness.sink.clone(), Some(down.clone()));
    let report = down_runner.write_round(&down).await.unwrap();
    assert!(report.failed > 0);
    let (attempts, error, due): (i32, Option<String>, DateTime<Utc>) = sqlx::query_as(
        "SELECT attempts, last_error, next_attempt_at FROM gateway.analytics_sink_queue LIMIT 1",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert!(error.is_some());
    assert!(due > Utc::now(), "backs off");
    // Back up: the retry writes it.
    sqlx::query("UPDATE gateway.analytics_sink_queue SET next_attempt_at = now()")
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.drain(&runner).await;

    // The store emptied (or replaced): the sink notices and copies again.
    harness
        .clickhouse
        .execute("TRUNCATE TABLE gateway_meta", &[])
        .await
        .unwrap();
    let fresh = harness.runner();
    fresh.tick().await.unwrap();
    assert_ne!(harness.phase().await, Some(Phase::Live));
    assert!(answers().await.unwrap().is_none());
    harness.go_live(&fresh).await;
    assert!(answers().await.unwrap().is_some());

    // Workers without the setting turn off a sink abandoned for an hour.
    let unconfigured = AnalyticsSinkRunner::new(harness.sink.clone(), None);
    unconfigured.tick().await.unwrap();
    assert!(harness.phase().await.is_some(), "recently seen");
    sqlx::query("UPDATE gateway.analytics_sink SET heartbeat_at = now() - interval '2 hours'")
        .execute(&harness.pool)
        .await
        .unwrap();
    unconfigured.tick().await.unwrap();
    assert!(harness.phase().await.is_none());
    assert!(answers().await.unwrap().is_none());
    harness.finish().await;
}

/// Times the sink and the reads on a few hundred thousand requests
/// (`docs/architecture-plan.md` §10 records the figures). Ignored: it needs
/// `GATEWAY_MEASURE_DATABASE_URL`, an empty database of its own with the
/// `gateway` schema created (it is filled and left full), and a release
/// build:
///
/// ```text
/// cargo test --release --test clickhouse_analytics_integration \
///   measure_sink_and_reads -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "measurement; needs GATEWAY_MEASURE_DATABASE_URL"]
async fn measure_sink_and_reads() {
    let database_url = std::env::var("GATEWAY_MEASURE_DATABASE_URL")
        .expect("GATEWAY_MEASURE_DATABASE_URL: an empty database for the measurement");
    let clickhouse_url = std::env::var("GATEWAY_TEST_CLICKHOUSE_URL").unwrap();
    let requests: i64 = std::env::var("GATEWAY_MEASURE_REQUESTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(300_000);
    let harness = Harness::new(&database_url, &clickhouse_url).await;
    let org = harness.org("clickhouse-measure").await;
    let owner = harness.user(org, "Owner").await;
    let account = harness
        .identity
        .create_service_account(org, owner, None, "measure")
        .await
        .unwrap()
        .id;
    let from = day() - Duration::days(30);
    let started = std::time::Instant::now();
    let mut tx = harness.pool.begin().await.unwrap();
    for (index, sql) in [
        "CREATE TEMPORARY TABLE seed ON COMMIT DROP AS
         SELECT g, gen_random_uuid() AS r, gen_random_uuid() AS t, $1::uuid AS org,
                $4::uuid AS account,
                $3::timestamptz + (g::double precision / $2) * interval '30 days' AS at
         FROM generate_series(1, $2::bigint) g",
        "INSERT INTO gateway.accounting_reservation
             (id, organization_id, service_account_id, idempotency_key, fingerprint, state,
              reserved_minor, expires_at, output_token_cap, input_price_minor,
              output_price_minor, cached_input_price_minor, reasoning_price_minor,
              other_price_minor, price_version, pricing_confidence, currency, deployment, model,
              trace_id, created_at)
         SELECT r, org, account, r::text, '\\x0707070707070707070707070707070707070707070707070707070707070707',
                'settled', 500, at + interval '5 minutes', 100, 1, 1, 1, 1, 1, 'v1', 'known',
                CASE WHEN g % 7 = 0 THEN 'EUR' ELSE 'USD' END, 'deploy-' || (g % 12),
                'model-' || (g % 9), t, at
         FROM seed",
        "INSERT INTO gateway.inference_trace
             (id, organization_id, service_account_id, reservation_id, correlation_id, operation,
              status, outcome, model, input_bound, output_cap, price_version, pricing_confidence,
              currency, amount_minor, metadata, recorded_at, finalized_at,
              provider_error_category)
         SELECT t, org, account, r, gen_random_uuid(), 'inference', 'finalized', 'settled',
                'model-' || (g % 9), 64, 100, 'v1', 'known',
                CASE WHEN g % 7 = 0 THEN 'EUR' ELSE 'USD' END, g % 300,
                jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency',
                                   CASE WHEN g % 7 = 0 THEN 'EUR' ELSE 'USD' END),
                at, at + (g % 2000) * interval '1 millisecond',
                CASE WHEN g % 50 = 0 THEN 'rate_limited' END
         FROM seed",
        "INSERT INTO gateway.accounting_usage
             (id, reservation_id, organization_id, currency, confidence, input_tokens,
              output_tokens, amount_minor, completeness, finality, recorded_at)
         SELECT gen_random_uuid(), r, org, CASE WHEN g % 7 = 0 THEN 'EUR' ELSE 'USD' END,
                'exact', g % 1000, g % 300, g % 300, 'complete', 'final',
                at + interval '10 milliseconds'
         FROM seed WHERE g % 50 <> 0",
    ]
    .into_iter()
    .enumerate()
    {
        let mut query = sqlx::query(sql);
        if index == 0 {
            query = query.bind(org.0).bind(requests).bind(from).bind(account.0);
        }
        query.execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    sqlx::query("ANALYZE").execute(&harness.pool).await.unwrap();
    println!("seeded {requests} requests in {:?}", started.elapsed());

    // The backfill.
    let runner = harness.runner();
    let started = std::time::Instant::now();
    harness.go_live(&runner).await;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway.accounting_reservation")
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    println!(
        "backfill: {total} rows in {elapsed:?} ({:.0} rows/s, including the 1.1 s wait)",
        total as f64 / elapsed.as_secs_f64()
    );

    // Queue writes: re-queue 100,000 requests and write them.
    let queued = 100_000_i64.min(total);
    sqlx::query(
        "INSERT INTO gateway.analytics_sink_queue
             (organization_id, reservation_id, reservation_created_at)
         SELECT organization_id, id, created_at FROM gateway.accounting_reservation
         ORDER BY created_at DESC LIMIT $1",
    )
    .bind(queued)
    .execute(&harness.pool)
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let mut written = 0;
    while written < queued as u64 {
        let round = runner.write_round(&harness.clickhouse).await.unwrap();
        assert_eq!(round.failed, 0);
        written += round.written + round.deleted;
        if round.written + round.deleted == 0 {
            break;
        }
    }
    let elapsed = started.elapsed();
    println!(
        "queue writes: {written} rows in {elapsed:?} ({:.0} rows/s, batches of 2,000)",
        written as f64 / elapsed.as_secs_f64()
    );

    // Reads, each from both stores: the median of five after a warm-up.
    let window = slice_between(from, from + Duration::days(30));
    let report = UsageQuery {
        from,
        to: from + Duration::days(30),
        group_by: vec![UsageGroupBy::Model, UsageGroupBy::Day],
        team_ids: Vec::new(),
        service_account_ids: Vec::new(),
        models: Vec::new(),
        deployments: Vec::new(),
        provider_families: Vec::new(),
        locations: Vec::new(),
        batch_ids: Vec::new(),
        model_masters: Vec::new(),
        currency: None,
    };
    let series = UsageSeriesQuery {
        slice: window.clone(),
        granularity: Granularity::Day,
        split_by: Some(UsageDimension::Model),
        limit: 5,
        rank_by: RankBy::Requests,
    };
    let previous = window.previous();
    let traces = TraceQuery {
        from: Some(from),
        to: Some(from + Duration::days(30)),
        search: Some("model-3".into()),
        ..TraceQuery::default()
    };
    macro_rules! timed {
        ($label:expr, $postgres:expr, $clickhouse:expr) => {{
            let mut times = [Vec::new(), Vec::new()];
            for round in 0..6 {
                let at = std::time::Instant::now();
                $postgres.await.unwrap();
                let postgres = at.elapsed();
                let at = std::time::Instant::now();
                $clickhouse.await.unwrap().expect("ClickHouse answers");
                let clickhouse = at.elapsed();
                if round > 0 {
                    times[0].push(postgres);
                    times[1].push(clickhouse);
                }
            }
            for list in &mut times {
                list.sort();
            }
            println!(
                "| {} | {:.0} ms | {:.0} ms |",
                $label,
                times[0][2].as_secs_f64() * 1000.0,
                times[1][2].as_secs_f64() * 1000.0
            );
        }};
    }
    println!("| Read | PostgreSQL | ClickHouse |");
    timed!(
        "usage report, by model and day, 30 days",
        async {
            harness
                .reporting
                .usage_rows(org, None, &report, 100, None)
                .await?;
            harness.reporting.usage_totals(org, None, &report).await
        },
        harness.reader.usage_report(org, None, &report, 100, None)
    );
    timed!(
        "usage series, top 5 models by day, 30 days (and previous)",
        harness
            .analytics
            .series_report(org, None, &series, &previous),
        harness.reader.series_report(org, None, &series, &previous)
    );
    timed!(
        "trace search, first page of 50",
        harness.reporting.list_traces(org, None, &traces, 50, None),
        harness.reader.list_traces(org, None, &traces, 50, None)
    );
    timed!(
        "trace summary by model",
        harness
            .reporting
            .summarize_traces(org, None, &traces, &[TraceDimension::Model], 50),
        harness
            .reader
            .summarize_traces(org, None, &traces, &[TraceDimension::Model], 50)
    );
    harness.finish().await;
}
