#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the usage dashboard: usage series and usage events
//! (`/usage/series`, `/usage/events`) and the trace fields the dashboard
//! links to, driven through the real router, RBAC and schema.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::Body,
    http::{StatusCode, header::COOKIE},
};
use chrono::{DateTime, Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, ServiceAccountId, TeamId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        identity::{ApiKeyScope, MembershipRole},
        keys::ApiKeyPepper,
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore, catalog::ResolvedCatalogRoute,
        reporting::ReportingStore,
    },
    transport::reporting::router,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SESSION_COOKIE: &str = "__Host-gateway_session";
const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";
const TEST_PEPPER: &[u8] = b"usage-analytics-integration-pepper";
const HMAC_KEY: &[u8] = b"usage-analytics-integration-hmac-key-32+";

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

/// One request: its reservation, its trace, and its usage (if any).
struct Request<'a> {
    team: Option<TeamId>,
    service_account: ServiceAccountId,
    api_key: Option<Uuid>,
    currency: &'a str,
    /// `(amount_minor, input, output)`; `None` records no usage.
    usage: Option<(Option<i64>, i64, i64)>,
    recorded_at: DateTime<Utc>,
    /// Admission to settlement; `None` leaves the request pending.
    duration_ms: Option<i64>,
    error: Option<&'a str>,
    content: Option<&'a str>,
    route: Option<&'a ResolvedCatalogRoute>,
}

impl Request<'_> {
    fn new(
        team: Option<TeamId>,
        service_account: ServiceAccountId,
        recorded_at: DateTime<Utc>,
    ) -> Self {
        Self {
            team,
            service_account,
            api_key: None,
            currency: "USD",
            usage: None,
            recorded_at,
            duration_ms: Some(1000),
            error: None,
            content: None,
            route: None,
        }
    }
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[17_u8; 32]).unwrap();
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            ReportingStore::new(pool.clone()),
        );
        Self {
            pool,
            identity,
            crypto,
            router,
        }
    }

    async fn org(&self, slug: &str) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id
    }

    async fn user(
        &self,
        org: OrganizationId,
        team: Option<TeamId>,
        role: &str,
        name: &str,
    ) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some(name))
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, MembershipRole::Member)
            .await
            .unwrap();
        if let Some(team) = team {
            self.identity
                .add_team_membership(org, team, user, MembershipRole::Admin)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(team.map(|team| team.0))
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> (String, String) {
        let browser = self.crypto.opaque_token().as_str().to_owned();
        let session = self
            .crypto
            .new_session(user, &browser, StdDuration::from_secs(3600));
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
        .execute(&self.pool)
        .await
        .unwrap();
        (session.token.clone(), browser)
    }

    /// Records a request. Returns its trace id.
    async fn record(&self, org: OrganizationId, request: Request<'_>) -> Uuid {
        let reservation = Uuid::new_v4();
        let trace = Uuid::new_v4();
        let route = request.route;
        let deployment = route.map_or("deploy-a", |route| route.deployment.as_str());
        let model = route.map_or("gpt-4o", |route| route.model.as_str());
        let pending = request.duration_ms.is_none();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, team_id, service_account_id, idempotency_key, fingerprint,
                  state, reserved_minor, expires_at, output_token_cap, input_price_minor,
                  output_price_minor, cached_input_price_minor, reasoning_price_minor,
                  other_price_minor, price_version, pricing_confidence, currency, deployment,
                  model, trace_id, catalog_release_id, catalog_route_id, catalog_deployment_id,
                  catalog_price_id, catalog_digest, catalog_operation, catalog_family,
                  catalog_price_source, catalog_chat_input_count_mode)
             VALUES ($1, $2, $3, $4, $5, $6, $20, 500, $7, 100, 1, 1, $22, $22, $22, 'v1',
                     'known', $8, $9, $21, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(request.team.map(|team| team.0))
        .bind(request.service_account.0)
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(request.recorded_at + Duration::minutes(5))
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
        .bind(if pending { "claimed" } else { "settled" })
        .bind(model)
        // The fixture's routes price every kind of token at 1; the
        // masters test's families report no cached, reasoning or other.
        .bind(route.map_or(1, |route| route.price.cached_input_minor_per_token))
        .execute(&self.pool)
        .await
        .unwrap();
        let amount = request.usage.and_then(|usage| usage.0);
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, team_id, service_account_id, reservation_id, correlation_id,
                  operation, status, outcome, model, input_bound, output_cap, price_version,
                  pricing_confidence, currency, amount_minor, metadata, recorded_at, finalized_at,
                  api_key_id, provider_error_category, catalog_release_id, catalog_route_id,
                  catalog_deployment_id, catalog_price_id, catalog_digest, catalog_operation,
                  catalog_family, catalog_price_source, catalog_chat_input_count_mode)
             VALUES ($1, $2, $3, $4, $5, $6, 'inference', $7, $8, $24, 64, 100, 'v1',
                     'known', $9, $10,
                     jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency', $9::text),
                     $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)",
        )
        .bind(trace)
        .bind(org.0)
        .bind(request.team.map(|team| team.0))
        .bind(request.service_account.0)
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(if pending { "pending" } else { "finalized" })
        .bind((!pending).then_some("settled"))
        .bind(request.currency)
        .bind(amount)
        .bind(request.recorded_at)
        .bind(
            request
                .duration_ms
                .map(|ms| request.recorded_at + Duration::milliseconds(ms)),
        )
        .bind(request.api_key)
        .bind(request.error)
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
        .execute(&self.pool)
        .await
        .unwrap();
        if let Some((amount, input, output)) = request.usage {
            sqlx::query(
                "INSERT INTO gateway.accounting_usage
                     (id, reservation_id, organization_id, currency, confidence, input_tokens,
                      output_tokens, amount_minor, completeness, finality, recorded_at)
                 VALUES ($1, $2, $3, $4, 'exact', $5, $6, $7, 'complete', 'final', $8)",
            )
            .bind(Uuid::new_v4())
            .bind(reservation)
            .bind(org.0)
            .bind(request.currency)
            .bind(input)
            .bind(output)
            .bind(amount)
            .bind(request.recorded_at + Duration::milliseconds(10))
            .execute(&self.pool)
            .await
            .unwrap();
        }
        if let Some(state) = request.content {
            sqlx::query(
                "INSERT INTO gateway.inference_trace_content
                     (trace_id, organization_id, state, object_name, key_id, sealed_key,
                      started_at, finished_at, expired_at)
                 VALUES ($1, $2, $3,
                         CASE WHEN $3 IN ('writing', 'stored') THEN 'traces/' || $1::text END,
                         CASE WHEN $3 IN ('writing', 'stored') THEN 'key-1' END,
                         CASE WHEN $3 IN ('writing', 'stored') THEN '\\x00'::bytea END,
                         $4, $4, CASE WHEN $3 = 'expired' THEN $4 END)",
            )
            .bind(trace)
            .bind(org.0)
            .bind(state)
            .bind(request.recorded_at)
            .execute(&self.pool)
            .await
            .unwrap();
        }
        trace
    }

    async fn get(&self, uri: &str, session: &(String, String)) -> (StatusCode, Value) {
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            )
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn audit_count(&self, org: OrganizationId, action: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM gateway.audit_event WHERE organization_id = $1 AND action = $2",
        )
        .bind(org.0)
        .bind(action)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

fn manifest() -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": "usage-dashboard"},
        "providers": [{"adapter_instance_key": "azure-main", "family": "azure-openai"}],
        "deployments": [{
            "name": "gpt-4o", "model": "gpt-4o", "location": "eastus",
            "adapter_instance_key": "azure-main",
            "capabilities": [{"operation": "chat", "max_input_bytes": 4096, "max_output_tokens": 32,
                "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 30,
                "chat_input_count_mode": "unsupported"}]
        }],
        "prices": [{
            "deployment": "gpt-4o", "operation": "chat",
            "input_minor_per_token": 1, "output_minor_per_token": 1,
            "cached_input_minor_per_token": 1, "reasoning_minor_per_token": 1,
            "other_minor_per_token": 1, "currency": "USD", "source": "test",
            "source_version": "v1", "confidence": "known"
        }],
        "routes": [{"alias": "chat-main", "operation": "chat", "deployment": "gpt-4o"}]
    }))
    .unwrap()
}

struct Fixture {
    org: OrganizationId,
    team_a: TeamId,
    team_b: TeamId,
    owner: UserAccountId,
    team_a_admin: UserAccountId,
    read_only: UserAccountId,
    account_a_owner: UserAccountId,
    key_a1: Uuid,
    key_a1_prefix: String,
    key_a2: Uuid,
    routed_trace: Uuid,
    failed_trace: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

/// Two days of requests in one organization:
///
/// | # | team | key | currency | usage (amount, in, out) | at   | other            |
/// |---|------|-----|----------|-------------------------|------|------------------|
/// | 1 | a    | a1  | USD      | 100, 10, 5              | d+1h | routed, 1500 ms, content stored |
/// | 2 | a    | a2  | USD      | 40, 3, 2                | d+2h | 500 ms, content off |
/// | 3 | a    | a1  | USD      | none                    | d+3h | pending          |
/// | 4 | b    | b   | EUR      | 70, 4, 4                | d+1h |                  |
/// | 5 | b    | b   | EUR      | none                    | d+1h30 | failed (rate_limited) |
/// | 6 | -    | org | USD      | 7, 1, 1                 | d+26h |                 |
///
/// plus one request the day before (USD 11) and one in another organization.
async fn fixture(harness: &Harness) -> Fixture {
    let org = harness.org("usage-dashboard").await;
    let other_org = harness.org("usage-dashboard-other").await;
    let team_a = harness.identity.create_team(org, "a").await.unwrap().id;
    let team_b = harness.identity.create_team(org, "b").await.unwrap().id;
    let owner = harness.user(org, None, "organization_owner", "Owner").await;
    let team_a_admin = harness
        .user(org, Some(team_a), "team_admin", "Admin A")
        .await;
    let team_b_admin = harness
        .user(org, Some(team_b), "team_admin", "Admin B")
        .await;
    let read_only = harness.user(org, None, "read_only_member", "Reader").await;
    let other_owner = harness
        .user(other_org, None, "organization_owner", "Other")
        .await;
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
    let account_a = account(team_a_admin, Some(team_a), "service-a").await;
    let account_b = account(team_b_admin, Some(team_b), "service-b").await;
    let account_org = account(owner, None, "service-org").await;
    let account_other = harness
        .identity
        .create_service_account(other_org, other_owner, None, "other")
        .await
        .unwrap()
        .id;
    // Key lifecycle is audited as an organization owner's.
    let issuer = harness
        .identity
        .create_user(org, None, None, None, Some("Issuer"))
        .await
        .unwrap()
        .id;
    harness
        .identity
        .add_organization_membership(org, issuer, MembershipRole::Owner)
        .await
        .unwrap();
    let key = |account| {
        let identity = harness.identity.clone();
        async move {
            identity
                .create_api_key(org, account, &[ApiKeyScope::Inference], None, issuer)
                .await
                .unwrap()
        }
    };
    let key_a1 = key(account_a).await;
    let key_a2 = key(account_a).await;
    let key_b = key(account_b).await;

    let catalog = CatalogStore::new(harness.pool.clone());
    let manifest = manifest();
    catalog
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    let route = catalog
        .resolve_active(org, "chat-main", "chat")
        .await
        .unwrap()
        .unwrap();

    let day = DateTime::parse_from_rfc3339("2026-06-10T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let hours = |hours: i64| day + Duration::hours(hours);
    let routed_trace = harness
        .record(
            org,
            Request {
                api_key: Some(key_a1.id.0),
                usage: Some((Some(100), 10, 5)),
                duration_ms: Some(1500),
                content: Some("stored"),
                route: Some(&route),
                ..Request::new(Some(team_a), account_a, hours(1))
            },
        )
        .await;
    harness
        .record(
            org,
            Request {
                api_key: Some(key_a2.id.0),
                usage: Some((Some(40), 3, 2)),
                duration_ms: Some(500),
                content: Some("off"),
                ..Request::new(Some(team_a), account_a, hours(2))
            },
        )
        .await;
    harness
        .record(
            org,
            Request {
                api_key: Some(key_a1.id.0),
                duration_ms: None,
                ..Request::new(Some(team_a), account_a, hours(3))
            },
        )
        .await;
    harness
        .record(
            org,
            Request {
                api_key: Some(key_b.id.0),
                currency: "EUR",
                usage: Some((Some(70), 4, 4)),
                ..Request::new(Some(team_b), account_b, hours(1))
            },
        )
        .await;
    let failed_trace = harness
        .record(
            org,
            Request {
                api_key: Some(key_b.id.0),
                currency: "EUR",
                error: Some("rate_limited"),
                duration_ms: Some(200),
                ..Request::new(Some(team_b), account_b, hours(1) + Duration::minutes(30))
            },
        )
        .await;
    harness
        .record(
            org,
            Request {
                usage: Some((Some(7), 1, 1)),
                ..Request::new(None, account_org, hours(26))
            },
        )
        .await;
    // The day before, and another tenant.
    harness
        .record(
            org,
            Request {
                api_key: Some(key_a1.id.0),
                usage: Some((Some(11), 1, 1)),
                ..Request::new(Some(team_a), account_a, hours(-3))
            },
        )
        .await;
    harness
        .record(
            other_org,
            Request {
                usage: Some((Some(999), 9, 9)),
                ..Request::new(None, account_other, hours(1))
            },
        )
        .await;
    Fixture {
        org,
        team_a,
        team_b,
        owner,
        team_a_admin,
        read_only,
        account_a_owner: team_a_admin,
        key_a1: key_a1.id.0,
        key_a1_prefix: key_a1.public_prefix,
        key_a2: key_a2.id.0,
        routed_trace,
        failed_trace,
        from: day,
        to: day + Duration::days(2),
    }
}

fn at(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn series_uri(fx: &Fixture, extra: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/usage/series?from={}&to={}{extra}",
        fx.org.0,
        at(fx.from),
        at(fx.to)
    )
}

fn events_uri(fx: &Fixture, extra: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/usage/events?from={}&to={}{extra}",
        fx.org.0,
        at(fx.from),
        at(fx.to)
    )
}

fn amounts(measures: &Value) -> Vec<(String, i64)> {
    measures["amounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|amount| {
            (
                amount["currency"].as_str().unwrap().to_owned(),
                amount["amount_minor"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn money(values: &[(&str, i64)]) -> Vec<(String, i64)> {
    values
        .iter()
        .map(|(currency, amount)| ((*currency).to_owned(), *amount))
        .collect()
}

#[tokio::test]
async fn series_totals_keep_money_per_currency_and_compare_the_previous_window() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let audits = harness.audit_count(fx.org, "usage.read").await;

    let (status, body) = harness.get(&series_uri(&fx, ""), &session).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let totals = &body["totals"];
    assert_eq!(totals["requests"], 6);
    assert_eq!(totals["failed_requests"], 1);
    assert_eq!(totals["pending_requests"], 1);
    assert_eq!(totals["no_usage_requests"], 2);
    assert_eq!(totals["unpriced_requests"], 0);
    assert_eq!(totals["tokens"]["input"], 18);
    assert_eq!(totals["tokens"]["output"], 12);
    // Exact, and never summed across currencies.
    assert_eq!(amounts(totals), money(&[("EUR", 70), ("USD", 147)]));
    // Durations of settled requests only: 1500, 500, 1000, 200, 1000.
    assert_eq!(totals["duration"]["average_ms"], 840);
    assert_eq!(totals["duration"]["p50_ms"], 1000);
    assert_eq!(totals["duration"]["p95_ms"], 1500);
    assert_eq!(body["previous"]["totals"]["requests"], 1);
    assert_eq!(amounts(&body["previous"]["totals"]), money(&[("USD", 11)]));
    assert_eq!(body["previous"]["to"], body["from"]);
    // Daily buckets over two days, one unsplit series.
    assert_eq!(body["granularity"], "day");
    assert_eq!(body["buckets"].as_array().unwrap().len(), 2);
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["value"], Value::Null);
    assert_eq!(series[0]["other"], false);
    let points = series[0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!(points[0]["requests"], 5);
    assert_eq!(amounts(&points[0]), money(&[("EUR", 70), ("USD", 140)]));
    assert_eq!(points[1]["requests"], 1);
    assert_eq!(amounts(&points[1]), money(&[("USD", 7)]));
    assert_eq!(series[0]["totals"]["requests"], 6);
    assert_eq!(harness.audit_count(fx.org, "usage.read").await, audits + 1);

    // By the hour, the first day's requests land in their hours.
    let (status, body) = harness
        .get(&series_uri(&fx, "&granularity=hour"), &session)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["buckets"].as_array().unwrap().len(), 48);
    let points = body["series"][0]["points"].as_array().unwrap();
    let hourly: Vec<(String, i64)> = points
        .iter()
        .map(|point| {
            (
                point["bucket"].as_str().unwrap().to_owned(),
                point["requests"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        hourly,
        vec![
            ("2026-06-10T01:00:00Z".to_owned(), 3),
            ("2026-06-10T02:00:00Z".to_owned(), 1),
            ("2026-06-10T03:00:00Z".to_owned(), 1),
            ("2026-06-11T02:00:00Z".to_owned(), 1),
        ]
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn series_split_into_top_values_and_other_with_labels() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;

    // By API key: a1 (2 requests), b (2), a2 (1), and the org request's none.
    let (status, body) = harness
        .get(&series_uri(&fx, "&split_by=api_key"), &session)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 4);
    assert_eq!(body["truncated"], false);
    let a1 = series
        .iter()
        .find(|item| item["value"] == fx.key_a1.to_string())
        .unwrap();
    assert_eq!(a1["label"], fx.key_a1_prefix.as_str());
    assert_eq!(a1["totals"]["requests"], 2);
    assert_eq!(amounts(&a1["totals"]), money(&[("USD", 100)]));
    let keyless = series.iter().find(|item| item["value"].is_null()).unwrap();
    assert_eq!(keyless["totals"]["requests"], 1);
    assert_eq!(keyless["label"], Value::Null);

    // The top team by requests (a: 3), then team b (2) and the
    // organization-level request (1) together as `other`.
    let (status, body) = harness
        .get(&series_uri(&fx, "&split_by=team&limit=1"), &session)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["truncated"], true);
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series[0]["value"], fx.team_a.0.to_string());
    assert_eq!(series[0]["label"], "a");
    assert_eq!(series[0]["totals"]["requests"], 3);
    assert_eq!(series[1]["other"], true);
    assert_eq!(series[1]["value"], Value::Null);
    assert_eq!(series[1]["totals"]["requests"], 3);
    assert_eq!(
        amounts(&series[1]["totals"]),
        money(&[("EUR", 70), ("USD", 7)])
    );
    let other_points = series[1]["points"].as_array().unwrap();
    assert_eq!(other_points.len(), 2);

    // Ranked by cost in one currency: team a's USD 140 beats the org's 7.
    let (status, body) = harness
        .get(
            &series_uri(&fx, "&split_by=team&rank_by=cost&currency=USD&limit=1"),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let series = body["series"].as_array().unwrap();
    assert_eq!(series[0]["value"], fx.team_a.0.to_string());
    assert_eq!(amounts(&series[0]["totals"]), money(&[("USD", 140)]));
    assert_eq!(amounts(&series[1]["totals"]), money(&[("USD", 7)]));
    assert_eq!(body["totals"]["requests"], 4);

    // By status and by user (the service account's owner).
    let (_, body) = harness
        .get(&series_uri(&fx, "&split_by=status"), &session)
        .await;
    let by_status: Vec<(String, i64)> = body["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["value"].as_str().unwrap().to_owned(),
                item["totals"]["requests"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_status,
        vec![
            ("succeeded".to_owned(), 4),
            ("failed".to_owned(), 1),
            ("pending".to_owned(), 1)
        ]
    );
    let (_, body) = harness
        .get(&series_uri(&fx, "&split_by=user"), &session)
        .await;
    let owner_a = body["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["value"] == fx.account_a_owner.0.to_string())
        .unwrap();
    assert_eq!(owner_a["label"], "Admin A");
    assert_eq!(owner_a["totals"]["requests"], 3);
    harness.pool.close().await;
}

#[tokio::test]
async fn filters_slice_series_and_events_alike() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;

    for (filter, requests, money_by_currency) in [
        (
            format!("&api_key_id={}", fx.key_a1),
            2,
            money(&[("USD", 100)]),
        ),
        (
            format!("&api_key_id={},{}", fx.key_a1, fx.key_a2),
            3,
            money(&[("USD", 140)]),
        ),
        ("&alias=chat-main".to_owned(), 1, money(&[("USD", 100)])),
        ("&operation=chat".to_owned(), 1, money(&[("USD", 100)])),
        (
            "&provider_family=azure-openai".to_owned(),
            1,
            money(&[("USD", 100)]),
        ),
        ("&location=eastus".to_owned(), 1, money(&[("USD", 100)])),
        ("&status=failed".to_owned(), 1, money(&[])),
        (
            "&status=succeeded,pending".to_owned(),
            5,
            money(&[("EUR", 70), ("USD", 147)]),
        ),
        ("&error_category=rate_limited".to_owned(), 1, money(&[])),
        (
            "&outcome=settled".to_owned(),
            5,
            money(&[("EUR", 70), ("USD", 147)]),
        ),
        (
            format!("&user_id={}", fx.account_a_owner.0),
            3,
            money(&[("USD", 140)]),
        ),
        (
            format!("&team_id={}", fx.team_b.0),
            2,
            money(&[("EUR", 70)]),
        ),
        ("&currency=EUR".to_owned(), 2, money(&[("EUR", 70)])),
        (
            "&model=gpt-4o&deployment=deploy-a".to_owned(),
            5,
            money(&[("EUR", 70), ("USD", 47)]),
        ),
    ] {
        let (status, body) = harness.get(&series_uri(&fx, &filter), &session).await;
        assert_eq!(status, StatusCode::OK, "{filter}: {body}");
        assert_eq!(body["totals"]["requests"], requests, "{filter}");
        assert_eq!(amounts(&body["totals"]), money_by_currency, "{filter}");
        let (status, events) = harness
            .get(
                &events_uri(&fx, &format!("{filter}&page_size=100")),
                &session,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{filter}: {events}");
        assert_eq!(
            events["items"].as_array().unwrap().len() as i64,
            requests,
            "{filter}"
        );
    }
    harness.pool.close().await;
}

#[tokio::test]
async fn events_page_newest_first_and_say_where_the_trace_is() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let audits = harness.audit_count(fx.org, "trace.list").await;

    let mut ids = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        let extra = match &token {
            Some(token) => format!("&page_size=4&page_token={token}"),
            None => "&page_size=4".to_owned(),
        };
        let (status, body) = harness.get(&events_uri(&fx, &extra), &session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        pages += 1;
        for item in body["items"].as_array().unwrap() {
            ids.push((
                item["id"].as_str().unwrap().to_owned(),
                item["recorded_at"].as_str().unwrap().to_owned(),
            ));
        }
        match body["next_page_token"].as_str() {
            Some(next) => token = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, 2);
    assert_eq!(ids.len(), 6);
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    assert_eq!(ids, sorted);
    assert_eq!(harness.audit_count(fx.org, "trace.list").await, audits + 2);

    let (_, body) = harness
        .get(&events_uri(&fx, "&page_size=100"), &session)
        .await;
    let items = body["items"].as_array().unwrap();
    let routed = items
        .iter()
        .find(|item| item["id"] == fx.routed_trace.to_string())
        .unwrap();
    assert_eq!(routed["content_state"], "stored");
    assert_eq!(routed["duration_ms"], 1500);
    assert_eq!(routed["alias"], "chat-main");
    assert_eq!(routed["operation"], "chat");
    assert_eq!(routed["location"], "eastus");
    assert_eq!(routed["status"], "succeeded");
    assert_eq!(routed["tokens"]["input"], 10);
    assert_eq!(routed["amount_minor"], 100);
    assert_eq!(routed["currency"], "USD");
    assert_eq!(routed["api_key_id"], fx.key_a1.to_string());
    assert_eq!(routed["labels"]["api_key"], fx.key_a1_prefix.as_str());
    assert_eq!(routed["labels"]["team"], "a");
    assert_eq!(routed["labels"]["service_account"], "service-a");
    assert_eq!(routed["labels"]["user"], "Admin A");
    let failed = items
        .iter()
        .find(|item| item["id"] == fx.failed_trace.to_string())
        .unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error_category"], "rate_limited");
    assert_eq!(failed["amount_minor"], Value::Null);
    assert_eq!(failed["content_state"], Value::Null);
    let pending = items
        .iter()
        .find(|item| item["status"] == "pending")
        .unwrap();
    assert_eq!(pending["duration_ms"], Value::Null);
    assert!(items.iter().any(|item| item["content_state"] == "off"));

    // A token is bound to its filters.
    let (_, first) = harness
        .get(&events_uri(&fx, "&page_size=1"), &session)
        .await;
    let token = first["next_page_token"].as_str().unwrap();
    let (status, body) = harness
        .get(
            &events_uri(
                &fx,
                &format!("&page_size=1&status=failed&page_token={token}"),
            ),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "page_token_query_mismatch");
    harness.pool.close().await;
}

#[tokio::test]
async fn dashboard_reads_are_clamped_like_the_reports() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;

    // A team admin sees its team's requests only, and no organization-level
    // ones; it may not name another team.
    let admin = harness.session(fx.team_a_admin).await;
    let (status, body) = harness.get(&series_uri(&fx, ""), &admin).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"]["requests"], 3);
    assert_eq!(amounts(&body["totals"]), money(&[("USD", 140)]));
    let (status, body) = harness
        .get(
            &series_uri(&fx, &format!("&team_id={}", fx.team_b.0)),
            &admin,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = harness
        .get(&events_uri(&fx, "&page_size=100"), &admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 3);

    // A read-only member reads usage but not the per-request list, which is
    // trace data.
    let reader = harness.session(fx.read_only).await;
    let (status, body) = harness.get(&series_uri(&fx, ""), &reader).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"]["requests"], 6);
    let (status, _) = harness.get(&events_uri(&fx, ""), &reader).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    harness.pool.close().await;
}

#[tokio::test]
async fn bad_dashboard_queries_are_refused() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let org = fx.org.0;
    for uri in [
        // 60 days by the hour is more than 744 buckets.
        format!(
            "/api/admin/v1/organizations/{org}/usage/series?from=2026-01-01T00:00:00Z&to=2026-03-02T00:00:00Z&granularity=hour"
        ),
        format!(
            "/api/admin/v1/organizations/{org}/usage/series?from=2026-01-01T00:00:00Z&to=2027-03-01T00:00:00Z"
        ),
        format!("/api/admin/v1/organizations/{org}/usage/series?from=2026-01-01T00:00:00Z"),
        series_uri(&fx, "&split_by=colour"),
        series_uri(&fx, "&granularity=minute"),
        series_uri(&fx, "&limit=21"),
        series_uri(&fx, "&rank_by=cost"),
        series_uri(&fx, "&page_size=10"),
        series_uri(&fx, "&status=lost"),
        series_uri(&fx, "&api_key_id=not-a-uuid"),
        series_uri(&fx, "&provider_family=nope"),
        series_uri(&fx, "&unknown=1"),
        events_uri(&fx, "&split_by=team"),
        events_uri(&fx, "&page_size=101"),
    ] {
        let (status, body) = harness.get(&uri, &session).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
    harness.pool.close().await;
}

#[tokio::test]
async fn traces_show_key_alias_timing_content_and_routing() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let traces = format!("/api/admin/v1/organizations/{}/traces", fx.org.0);

    let (status, body) = harness
        .get(&format!("{traces}?api_key_id={}", fx.key_a1), &session)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Two in the window and one the day before.
    assert_eq!(body["items"].as_array().unwrap().len(), 3);

    let (status, detail) = harness
        .get(&format!("{traces}/{}", fx.routed_trace), &session)
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["api_key_id"], fx.key_a1.to_string());
    assert_eq!(detail["alias"], "chat-main");
    assert_eq!(detail["operation"], "chat");
    assert_eq!(detail["duration_ms"], 1500);
    assert_eq!(detail["content_state"], "stored");
    assert!(detail["routing"]["route_id"].is_string(), "{detail}");
    assert!(detail["routing"]["price_source"].is_string(), "{detail}");

    // The request id the caller saw finds the trace.
    let correlation = detail["correlation_id"].as_str().unwrap();
    let (status, body) = harness
        .get(&format!("{traces}?q={correlation}"), &session)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], fx.routed_trace.to_string());
    let (_, body) = harness
        .get(&format!("{traces}?q={}", fx.failed_trace), &session)
        .await;
    assert_eq!(body["items"][0]["error_category"], "rate_limited");
    harness.pool.close().await;
}

#[tokio::test]
async fn expiry_is_neither_success_nor_a_duration() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("usage-expiry").await;
    let owner = harness.user(org, None, "organization_owner", "Owner").await;
    let account = harness
        .identity
        .create_service_account(org, owner, None, "svc")
        .await
        .unwrap()
        .id;
    let at = DateTime::parse_from_rfc3339("2026-08-01T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    // Settled normally, 1 s.
    harness.record(org, Request::new(None, account, at)).await;
    // Expired before a claim, closed 5 minutes later.
    let expired = harness
        .record(
            org,
            Request {
                duration_ms: Some(300_000),
                ..Request::new(None, account, at)
            },
        )
        .await;
    // Claimed, then closed by the expiry worker: the result is unknown.
    let lost = harness
        .record(
            org,
            Request {
                duration_ms: Some(300_000),
                ..Request::new(None, account, at)
            },
        )
        .await;
    // Completed without usable usage: `unknown` outcome, but no expiry.
    let no_usage = harness
        .record(
            org,
            Request {
                duration_ms: Some(2000),
                ..Request::new(None, account, at)
            },
        )
        .await;
    for (trace, outcome) in [
        (expired, "expired"),
        (lost, "unknown"),
        (no_usage, "unknown"),
    ] {
        sqlx::query("UPDATE gateway.inference_trace SET outcome = $2 WHERE id = $1")
            .bind(trace)
            .bind(outcome)
            .execute(&harness.pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO gateway.accounting_reconciliation (id, organization_id, reservation_id, reason)
         SELECT $1, organization_id, reservation_id, 'expired-after-claim'
         FROM gateway.inference_trace WHERE id = $2",
    )
    .bind(Uuid::new_v4())
    .bind(lost)
    .execute(&harness.pool)
    .await
    .unwrap();

    let session = harness.session(owner).await;
    let window = "from=2026-08-01T00:00:00Z&to=2026-08-02T00:00:00Z";
    let (status, body) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/usage/series?{window}&split_by=status",
                org.0
            ),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let totals = &body["totals"];
    assert_eq!(totals["requests"], 4);
    assert_eq!(totals["expired_requests"], 1);
    assert_eq!(totals["unknown_requests"], 1);
    assert_eq!(totals["failed_requests"], 0);
    // Only the two requests that ran to completion have a duration.
    assert_eq!(totals["duration"]["average_ms"], 1500);
    assert_eq!(totals["duration"]["p95_ms"], 2000);
    let mut split: Vec<(String, i64)> = body["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|series| {
            (
                series["value"].as_str().unwrap().to_owned(),
                series["totals"]["requests"].as_i64().unwrap(),
            )
        })
        .collect();
    split.sort();
    assert_eq!(
        split,
        vec![
            ("expired".to_owned(), 1),
            ("succeeded".to_owned(), 2),
            ("unknown".to_owned(), 1)
        ]
    );

    let (status, body) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/usage/events?{window}&status=unknown,expired",
                org.0
            ),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|item| item["duration_ms"].is_null()));
    // The trace list agrees on the duration.
    let (status, body) = harness
        .get(
            &format!("/api/admin/v1/organizations/{}/traces/{lost}", org.0),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["duration_ms"], Value::Null);
    harness.pool.close().await;
}

/// Each deployment gets its own alias, so a route names it.
fn masters_manifest() -> CatalogManifest {
    let capability = json!({"operation": "chat", "max_input_bytes": 4096, "max_output_tokens": 32,
        "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 30,
        "chat_input_count_mode": "unsupported"});
    let deployments = [
        ("anthropic-sonnet", "claude-sonnet-4-6", "anthropic-main"),
        (
            "bedrock-sonnet",
            "us.anthropic.claude-sonnet-4-6",
            "bedrock-main",
        ),
        (
            "bedrock-sonnet-eu",
            "eu.anthropic.claude-sonnet-4-6",
            "bedrock-main",
        ),
        ("foundry-sonnet", "team-sonnet", "foundry-main"),
        ("local-qwen", "qwen-local", "local"),
        ("local-gone", "gone-local", "local"),
        ("local-plain", "plain-local", "local"),
        ("local-own", "own-local", "local"),
    ];
    serde_json::from_value(json!({
        "release": {"version": "usage-masters"},
        "providers": [
            {"adapter_instance_key": "anthropic-main", "family": "anthropic"},
            {"adapter_instance_key": "bedrock-main", "family": "bedrock"},
            {"adapter_instance_key": "foundry-main", "family": "azure-ai-foundry"},
            {"adapter_instance_key": "local", "family": "openai-compatible"}
        ],
        "deployments": deployments.iter().map(|(name, model, account)| json!({
            "name": name, "model": model, "location": "us", "adapter_instance_key": account,
            "capabilities": [capability.clone()]
        })).collect::<Vec<_>>(),
        "prices": deployments.iter().map(|(name, _, _)| json!({
            "deployment": name, "operation": "chat",
            "input_minor_per_token": 1, "output_minor_per_token": 1,
            "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
            "other_minor_per_token": 0, "currency": "USD", "source": "test",
            "source_version": "v1", "confidence": "known"
        })).collect::<Vec<_>>(),
        "routes": deployments.iter().map(|(name, _, _)| json!({
            "alias": name, "operation": "chat", "deployment": name
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

#[tokio::test]
async fn usage_is_stamped_with_its_master_when_served_and_reports_by_it() {
    const SONNET: &str = "anthropic/claude-sonnet-4-6";
    const LUNA: &str = "openai/gpt-6-luna";
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let library = gateway::domain::model_card_files::CardLibrary::builtin().unwrap();
    let stored = gateway::persistence::model_masters::ModelMasterStore::new(harness.pool.clone())
        .sync_card_matches(&library)
        .await
        .unwrap();
    assert!(stored > 10, "{stored}");
    let org = harness.org("usage-masters").await;
    let owner = harness.user(org, None, "organization_owner", "Owner").await;
    let session = harness.session(owner).await;
    let account = harness
        .identity
        .create_service_account(org, owner, None, "masters")
        .await
        .unwrap()
        .id;
    let catalog = CatalogStore::new(harness.pool.clone());
    let manifest = masters_manifest();
    catalog
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    // Links: qwen to the GPT card, the EU profile to none, one to a card no
    // longer on file, one to the organization's own record, and a cleared
    // one (the card decides again).
    for (family, model, master, cleared) in [
        ("openai-compatible", "qwen-local", Some(LUNA), false),
        ("bedrock", "eu.anthropic.claude-sonnet-4-6", None, false),
        ("openai-compatible", "gone-local", Some("acme/gone"), false),
        (
            "openai-compatible",
            "own-local",
            Some("org/own-model"),
            false,
        ),
        (
            "bedrock",
            "us.anthropic.claude-sonnet-4-6",
            Some(LUNA),
            true,
        ),
    ] {
        sqlx::query(
            "INSERT INTO gateway.model_master_link
                 (organization_id, id, family, model, master, deleted_at)
             VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END)",
        )
        .bind(org.0)
        .bind(Uuid::new_v4())
        .bind(family)
        .bind(model)
        .bind(master)
        .bind(cleared)
        .execute(&harness.pool)
        .await
        .unwrap();
    }
    // Foundry's deployment is named apart from its model; its listing
    // names the model.
    sqlx::query(
        "INSERT INTO gateway.catalog_discovered_model
             (organization_id, adapter_instance_key, family, model, first_seen_at, last_seen_at,
              deployment_model)
         VALUES ($1, 'foundry-main', 'azure-ai-foundry', 'team-sonnet', now(), now(),
                 'claude-sonnet-4-6')",
    )
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap();

    let day = DateTime::parse_from_rfc3339("2026-06-10T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut traces = std::collections::BTreeMap::new();
    for (hour, alias) in [
        "anthropic-sonnet",
        "bedrock-sonnet",
        "bedrock-sonnet-eu",
        "foundry-sonnet",
        "local-qwen",
        "local-gone",
        "local-plain",
        "local-own",
    ]
    .into_iter()
    .enumerate()
    {
        let route = catalog
            .resolve_active(org, alias, "chat")
            .await
            .unwrap()
            .unwrap();
        let trace = harness
            .record(
                org,
                Request {
                    usage: Some((Some(10), 1, 1)),
                    route: Some(&route),
                    ..Request::new(None, account, day + Duration::hours(hour as i64))
                },
            )
            .await;
        traces.insert(alias, trace);
    }
    // And one without catalog provenance.
    harness
        .record(
            org,
            Request {
                usage: Some((Some(10), 1, 1)),
                ..Request::new(None, account, day + Duration::hours(9))
            },
        )
        .await;
    let stamped: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT deployment, model_master FROM gateway.accounting_reservation
         WHERE organization_id = $1 ORDER BY created_at, deployment",
    )
    .bind(org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let master_of = |deployment: &str| {
        stamped
            .iter()
            .find(|(name, _)| name == deployment)
            .unwrap_or_else(|| panic!("{deployment}: {stamped:?}"))
            .1
            .clone()
    };
    assert_eq!(master_of("anthropic-sonnet").as_deref(), Some(SONNET));
    assert_eq!(master_of("bedrock-sonnet").as_deref(), Some(SONNET));
    assert_eq!(master_of("foundry-sonnet").as_deref(), Some(SONNET));
    assert_eq!(master_of("local-qwen").as_deref(), Some(LUNA));
    assert_eq!(master_of("bedrock-sonnet-eu"), None);
    assert_eq!(master_of("local-gone"), None);
    assert_eq!(master_of("local-plain"), None);
    assert_eq!(master_of("local-own").as_deref(), Some("org/own-model"));
    assert_eq!(master_of("deploy-a"), None);

    // Recorded when served: a later link, or a write naming the column,
    // leaves it as it was.
    sqlx::query(
        "UPDATE gateway.model_master_link SET master = NULL
         WHERE organization_id = $1 AND model = 'qwen-local'",
    )
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    let rewritten = sqlx::query(
        "UPDATE gateway.accounting_reservation SET model_master = 'org/forged'
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    assert_eq!(rewritten.rows_affected(), 9);
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND model_master IS NOT DISTINCT FROM 'org/forged'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(kept, 0);
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT model_master FROM gateway.accounting_reservation
             WHERE organization_id = $1 AND deployment = 'local-qwen'",
        )
        .bind(org.0)
        .fetch_one(&harness.pool)
        .await
        .unwrap()
        .as_deref(),
        Some(LUNA)
    );

    let window = format!("from={}&to={}", at(day), at(day + Duration::days(1)));
    let base = format!("/api/admin/v1/organizations/{}", org.0);
    // The usage report, grouped by master.
    let (status, body) = harness
        .get(
            &format!("{base}/usage?{window}&group_by=model_master"),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut groups: Vec<(Option<String>, i64)> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["values"]["model_master"], row["group"]);
            (
                row["group"].as_str().map(str::to_owned),
                row["request_count"].as_i64().unwrap(),
            )
        })
        .collect();
    groups.sort();
    assert_eq!(
        groups,
        [
            (None, 4),
            (Some(SONNET.to_owned()), 3),
            (Some(LUNA.to_owned()), 1),
            (Some("org/own-model".to_owned()), 1)
        ]
    );
    // Filtered to two masters, any of them.
    let (status, body) = harness
        .get(
            &format!("{base}/usage?{window}&group_by=day&model_master={SONNET},{LUNA}"),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"][0]["request_count"], 4);
    let (status, body) = harness
        .get(
            &format!("{base}/usage?{window}&model_master=sonnet"),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The dashboard series, split by master.
    let (status, body) = harness
        .get(
            &format!("{base}/usage/series?{window}&split_by=model_master"),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sonnet = body["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|series| series["value"] == SONNET)
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(sonnet["totals"]["requests"], 3);
    assert_eq!(amounts(&sonnet["totals"]), money(&[("USD", 30)]));
    // Events, filtered by master, carry it.
    let (status, body) = harness
        .get(
            &format!("{base}/usage/events?{window}&model_master={LUNA}"),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = body["items"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{body}");
    assert_eq!(events[0]["model_master"], LUNA);
    assert_eq!(events[0]["id"], traces["local-qwen"].to_string());

    // Exports, limited to a master, carry it as their last column.
    let query = gateway::domain::retention::ExportQuery::new(
        "usage",
        None,
        Some(&at(day)),
        Some(&at(day + Duration::days(1))),
        Utc::now(),
    )
    .unwrap()
    .with_model_master(Some(SONNET))
    .unwrap();
    let rows = gateway::persistence::retention::RetentionStore::new(harness.pool.clone())
        .export_page(org, &query, None, 100)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    for (_, _, values) in &rows {
        assert_eq!(values.last(), Some(&json!(SONNET)));
    }
    harness.pool.close().await;
}
