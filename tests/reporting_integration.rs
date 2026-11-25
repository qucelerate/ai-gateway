#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for the reporting lane: usage aggregates, metadata-only
//! traces, and reconciliation items, driven through the real router, RBAC,
//! and schema.
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
    http::{HeaderMap, StatusCode, header::COOKIE},
};
use chrono::{DateTime, Duration, Utc};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, ServiceAccountId, TeamId, UserAccountId,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
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
const TEST_PEPPER: &[u8] = b"reporting-integration-pepper";
const HMAC_KEY: &[u8] = b"reporting-integration-hmac-key-at-least-32";

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

/// One recorded request: a reservation, its trace, and zero or more usage
/// rows (the last one is the authoritative row).
struct Seed<'a> {
    team: Option<TeamId>,
    service_account: Option<ServiceAccountId>,
    model: &'a str,
    currency: &'a str,
    /// `(confidence, amount_minor, completeness, input, output)` per usage row.
    usage: &'a [(&'a str, Option<i64>, &'a str, Option<i64>, Option<i64>)],
    recorded_at: DateTime<Utc>,
    /// Catalog provenance of the routed deployment; `None` for pre-catalog
    /// usage.
    route: Option<&'a ResolvedCatalogRoute>,
    /// The output token cap reserved for; `None` for embeddings.
    output_cap: Option<i64>,
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
        let crypto = AuthCrypto::new(HMAC_KEY, &[13_u8; 32]).unwrap();
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

    async fn team(&self, org: OrganizationId, name: &str) -> TeamId {
        self.identity.create_team(org, name).await.unwrap().id
    }

    /// A user holding exactly one admin role binding: org-wide when `team` is
    /// `None`, otherwise `team_admin` of that team.
    async fn user(&self, org: OrganizationId, team: Option<TeamId>, role: &str) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some(role))
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

    async fn service_account(
        &self,
        org: OrganizationId,
        owner: UserAccountId,
        team: Option<TeamId>,
    ) -> ServiceAccountId {
        self.identity
            .create_service_account(org, owner, team, "reporting")
            .await
            .unwrap()
            .id
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

    /// Insert a settled reservation, its finalized trace, and usage rows one
    /// second apart ending at `recorded_at`. Returns `(reservation, trace)`.
    async fn record(&self, org: OrganizationId, seed: Seed<'_>) -> (Uuid, Uuid) {
        self.record_in_state(org, seed, "settled").await
    }

    async fn record_in_state(
        &self,
        org: OrganizationId,
        seed: Seed<'_>,
        state: &str,
    ) -> (Uuid, Uuid) {
        let reservation = Uuid::new_v4();
        let trace = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, team_id, service_account_id, idempotency_key, fingerprint,
                  state, reserved_minor, expires_at, output_token_cap, input_price_minor,
                  output_price_minor, cached_input_price_minor, reasoning_price_minor,
                  other_price_minor, price_version, pricing_confidence, currency, deployment,
                  model, trace_id, catalog_release_id, catalog_route_id, catalog_deployment_id,
                  catalog_price_id, catalog_digest, catalog_operation, catalog_family,
                  catalog_price_source, catalog_chat_input_count_mode)
             VALUES ($1, $2, $3, $4, $5, $6, $21, 500, $7, 100, 1, 1, 1, 1, 1, 'v1',
                     'known', $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(seed.team.map(|team| team.0))
        .bind(seed.service_account.map(|account| account.0))
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(seed.recorded_at + Duration::minutes(5))
        .bind(seed.currency)
        .bind(
            seed.route
                .map_or("deploy-a", |route| route.deployment.as_str()),
        )
        .bind(seed.model)
        .bind(trace)
        .bind(seed.route.map(|route| route.release_id))
        .bind(seed.route.map(|route| route.route_id))
        .bind(seed.route.map(|route| route.deployment_id))
        .bind(seed.route.map(|route| route.price_id))
        .bind(seed.route.map(|route| route.digest.0.to_vec()))
        .bind(seed.route.map(|route| route.operation.as_str()))
        .bind(seed.route.map(|route| route.family.as_str()))
        .bind(seed.route.map(|route| route.source.as_str()))
        .bind(seed.route.map(|route| route.chat_input_count_mode.as_str()))
        .bind(state)
        .execute(&self.pool)
        .await
        .unwrap();
        let amount = seed.usage.last().and_then(|usage| usage.1);
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, team_id, service_account_id, reservation_id, correlation_id,
                  operation, status, outcome, provider_request_id, model, input_bound, output_cap,
                  price_version, pricing_confidence, currency, amount_minor, metadata,
                  recorded_at, finalized_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'inference', 'finalized', 'succeeded', 'req_opaque',
                     $7, 64, $11, 'v1', 'known', $8, $9,
                     jsonb_build_object('input_bound', 64, 'output_cap', $11, 'currency', $8::text),
                     $10, $10)",
        )
        .bind(trace)
        .bind(org.0)
        .bind(seed.team.map(|team| team.0))
        .bind(seed.service_account.map(|account| account.0))
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(seed.model)
        .bind(seed.currency)
        .bind(amount)
        .bind(seed.recorded_at)
        .bind(seed.output_cap)
        .execute(&self.pool)
        .await
        .unwrap();
        let rows = seed.usage.len() as i64;
        for (index, (confidence, amount, completeness, input, output)) in
            seed.usage.iter().enumerate()
        {
            let finality = match *completeness {
                "complete" => "final",
                "partial" => "partial",
                _ => "interim",
            };
            sqlx::query(
                "INSERT INTO gateway.accounting_usage
                     (id, reservation_id, organization_id, currency, confidence, input_tokens,
                      output_tokens, amount_minor, completeness, finality, recorded_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(Uuid::new_v4())
            .bind(reservation)
            .bind(org.0)
            .bind(seed.currency)
            .bind(*confidence)
            .bind(*input)
            .bind(*output)
            .bind(*amount)
            .bind(*completeness)
            .bind(finality)
            .bind(seed.recorded_at - Duration::seconds(rows - 1 - index as i64))
            .execute(&self.pool)
            .await
            .unwrap();
        }
        (reservation, trace)
    }

    async fn reconciliation(&self, org: OrganizationId, reservation: Uuid, reason: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reconciliation (id, organization_id, reservation_id, reason)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(org.0)
        .bind(reservation)
        .bind(reason)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    /// Hold `reserved` for `reservation` in an organization bucket and a
    /// bucket of `team`, as a reservation's allocations would. Returns the
    /// bucket ids.
    async fn hold(
        &self,
        org: OrganizationId,
        team: TeamId,
        reservation: Uuid,
        reserved: i64,
    ) -> Vec<Uuid> {
        let mut buckets = Vec::new();
        for team in [None, Some(team)] {
            let bucket = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO gateway.quota_bucket
                     (id, organization_id, team_id, scope_kind, period_start, period_end,
                      currency, limit_minor, reserved_minor)
                 VALUES ($1, $2, $3, $4, now() - interval '1 day', now() + interval '1 day',
                         'USD', 100000, $5)",
            )
            .bind(bucket)
            .bind(org.0)
            .bind(team.map(|team| team.0))
            .bind(if team.is_some() {
                "team"
            } else {
                "organization"
            })
            .bind(reserved)
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO gateway.accounting_allocation
                     (id, reservation_id, quota_bucket_id, organization_id, reserved_minor, state)
                 VALUES ($1, $2, $3, $4, $5, 'held')",
            )
            .bind(Uuid::new_v4())
            .bind(reservation)
            .bind(bucket)
            .bind(org.0)
            .bind(reserved)
            .execute(&self.pool)
            .await
            .unwrap();
            buckets.push(bucket);
        }
        buckets
    }

    async fn resolve(
        &self,
        org: OrganizationId,
        item: Uuid,
        session: &(String, String),
        headers: &[(&str, &str)],
        body: Value,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri(format!(
                "/api/admin/v1/organizations/{}/reconciliation-items/{item}/resolve",
                org.0
            ))
            .header("content-type", "application/json")
            .header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            );
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body, headers)
    }

    /// `(reserved_minor, consumed_minor)` of each bucket, in order.
    async fn buckets(&self, buckets: &[Uuid]) -> Vec<(i64, i64)> {
        let mut balances = Vec::new();
        for bucket in buckets {
            balances.push(
                sqlx::query_as(
                    "SELECT reserved_minor, consumed_minor FROM gateway.quota_bucket WHERE id = $1",
                )
                .bind(bucket)
                .fetch_one(&self.pool)
                .await
                .unwrap(),
            );
        }
        balances
    }

    /// `(state, settled_minor)` of the reservation's allocations.
    async fn allocations(&self, reservation: Uuid) -> Vec<(String, Option<i64>)> {
        sqlx::query_as(
            "SELECT state, settled_minor FROM gateway.accounting_allocation
             WHERE reservation_id = $1 ORDER BY quota_bucket_id",
        )
        .bind(reservation)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn get(
        &self,
        uri: &str,
        session: Option<&(String, String)>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder().method("GET").uri(uri);
        if let Some(session) = session {
            builder = builder.header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            );
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body, headers)
    }

    async fn audit_count(&self, org: OrganizationId, action: &str, entity_type: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM gateway.audit_event
             WHERE organization_id = $1 AND action = $2 AND entity_type = $3",
        )
        .bind(org.0)
        .bind(action)
        .bind(entity_type)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

/// Two teams, an org-level service account, and a second tenant.
struct Fixture {
    org: OrganizationId,
    other_org: OrganizationId,
    team_a: TeamId,
    team_b: TeamId,
    owner: UserAccountId,
    team_a_trace: Uuid,
    team_b_trace: Uuid,
    other_org_trace: Uuid,
    team_b_reservation: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

async fn fixture(harness: &Harness) -> Fixture {
    let org = harness.org("reporting").await;
    let other_org = harness.org("reporting-other").await;
    let team_a = harness.team(org, "a").await;
    let team_b = harness.team(org, "b").await;
    let owner = harness.user(org, None, "organization_owner").await;
    let other_owner = harness.user(other_org, None, "organization_owner").await;
    let owner_a = harness.user(org, Some(team_a), "team_admin").await;
    let owner_b = harness.user(org, Some(team_b), "team_admin").await;
    let account_a = harness.service_account(org, owner_a, Some(team_a)).await;
    let account_b = harness.service_account(org, owner_b, Some(team_b)).await;
    let account_org = harness.service_account(org, owner, None).await;
    let account_other = harness.service_account(other_org, other_owner, None).await;
    let day = DateTime::parse_from_rfc3339("2026-03-10T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // An estimated interim row superseded by the exact final row: counted once.
    let (_, team_a_trace) = harness
        .record(
            org,
            Seed {
                team: Some(team_a),
                service_account: Some(account_a),
                model: "model-1",
                currency: "USD",
                usage: &[
                    ("estimated", Some(80), "partial", Some(8), None),
                    ("exact", Some(100), "complete", Some(10), Some(5)),
                ],
                recorded_at: day,
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    harness
        .record(
            org,
            Seed {
                team: Some(team_a),
                service_account: Some(account_a),
                model: "model-1",
                currency: "USD",
                usage: &[("estimated", Some(40), "complete", Some(4), Some(2))],
                recorded_at: day + Duration::hours(1),
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    let (team_b_reservation, team_b_trace) = harness
        .record(
            org,
            Seed {
                team: Some(team_b),
                service_account: Some(account_b),
                model: "model-2",
                currency: "EUR",
                usage: &[("unknown", None, "unavailable", None, None)],
                recorded_at: day + Duration::days(1),
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    harness
        .record(
            org,
            Seed {
                team: None,
                service_account: Some(account_org),
                model: "model-1",
                currency: "USD",
                usage: &[("exact", Some(7), "complete", Some(1), Some(1))],
                recorded_at: day + Duration::days(2),
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    // Outside the queried window.
    harness
        .record(
            org,
            Seed {
                team: Some(team_a),
                service_account: Some(account_a),
                model: "model-1",
                currency: "USD",
                usage: &[("exact", Some(5000), "complete", Some(1), Some(1))],
                recorded_at: day - Duration::days(30),
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    let (_, other_org_trace) = harness
        .record(
            other_org,
            Seed {
                team: None,
                service_account: Some(account_other),
                model: "model-1",
                currency: "USD",
                usage: &[("exact", Some(999), "complete", Some(1), Some(1))],
                recorded_at: day,
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    harness
        .reconciliation(org, team_b_reservation, "provider-unknown")
        .await;
    Fixture {
        org,
        other_org,
        team_a,
        team_b,
        owner,
        team_a_trace,
        team_b_trace,
        other_org_trace,
        team_b_reservation,
        from: day - Duration::days(1),
        to: day + Duration::days(7),
    }
}

fn usage_uri(org: OrganizationId, from: DateTime<Utc>, to: DateTime<Utc>, extra: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/usage?from={}&to={}{extra}",
        org.0,
        from.format("%Y-%m-%dT%H:%M:%SZ"),
        to.format("%Y-%m-%dT%H:%M:%SZ"),
    )
}

fn totals(body: &Value) -> Vec<(String, String, i64, i64, i64)> {
    body["totals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|total| {
            (
                total["currency"].as_str().unwrap().to_owned(),
                total["confidence"].as_str().unwrap().to_owned(),
                total["request_count"].as_i64().unwrap(),
                total["amount_minor"].as_i64().unwrap(),
                total["unpriced_request_count"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn totals_of(rows: &[(&str, &str, i64, i64, i64)]) -> Vec<(String, String, i64, i64, i64)> {
    rows.iter()
        .map(|(currency, confidence, count, amount, unpriced)| {
            (
                (*currency).to_owned(),
                (*confidence).to_owned(),
                *count,
                *amount,
                *unpriced,
            )
        })
        .collect()
}

#[tokio::test]
async fn usage_totals_separate_confidence_and_currency_and_page_stably() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;

    let (status, body, headers) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&group_by=model"),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    // The superseded interim row is not counted; the out-of-window and
    // other-tenant rows are absent; currencies and confidences never merge.
    assert_eq!(
        totals(&body),
        totals_of(&[
            ("EUR", "unknown", 1, 0, 1),
            ("USD", "estimated", 1, 40, 0),
            ("USD", "exact", 2, 107, 0),
        ])
    );
    let eur = &body["totals"][0];
    assert_eq!(eur["incomplete_request_count"], 1);
    assert_eq!(body["totals"][2]["tokens"]["input"], 11);
    assert_eq!(body["group_by"], "model");

    // Page through the grouped rows one at a time.
    let mut rows = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let extra = match &token {
            Some(token) => format!("&group_by=model&page_size=1&page_token={token}"),
            None => "&group_by=model&page_size=1".to_owned(),
        };
        let (status, body, _) = harness
            .get(&usage_uri(fx.org, fx.from, fx.to, &extra), Some(&session))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for item in body["items"].as_array().unwrap() {
            rows.push(format!(
                "{}:{}:{}",
                item["group"].as_str().unwrap(),
                item["currency"].as_str().unwrap(),
                item["confidence"].as_str().unwrap()
            ));
        }
        // Totals are independent of paging.
        assert_eq!(body["totals"].as_array().unwrap().len(), 3);
        match body["next_page_token"].as_str() {
            Some(next) => token = Some(next.to_owned()),
            None => break,
        }
        assert!(rows.len() < 10, "paging did not terminate");
    }
    assert_eq!(
        rows,
        vec![
            "model-1:USD:estimated",
            "model-1:USD:exact",
            "model-2:EUR:unknown"
        ]
    );

    // A token is bound to its filters.
    let (_, first, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&group_by=model&page_size=1"),
            Some(&session),
        )
        .await;
    let token = first["next_page_token"].as_str().unwrap();
    let (status, body, _) = harness
        .get(
            &usage_uri(
                fx.org,
                fx.from,
                fx.to,
                &format!("&group_by=team&page_size=1&page_token={token}"),
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "page_token_query_mismatch");

    // Grouping by team marks organization-level usage with a null group.
    let (_, body, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&group_by=team"),
            Some(&session),
        )
        .await;
    let groups: Vec<Value> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["group"].clone())
        .collect();
    assert!(groups.contains(&Value::Null));
    assert!(groups.contains(&json!(fx.team_a.0.to_string())));

    // Filters narrow the totals.
    let (_, body, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&currency=EUR&model=model-2"),
            Some(&session),
        )
        .await;
    assert_eq!(totals(&body), totals_of(&[("EUR", "unknown", 1, 0, 1)]));

    assert!(
        harness
            .audit_count(fx.org, "usage.read", "usage_report")
            .await
            >= 1
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn late_usage_rows_keep_a_request_in_its_first_window() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;

    // A final row for the in-window EUR request lands after `to`.
    sqlx::query(
        "INSERT INTO gateway.accounting_usage
             (id, reservation_id, organization_id, currency, confidence, input_tokens,
              output_tokens, amount_minor, completeness, finality, recorded_at)
         VALUES ($1, $2, $3, 'EUR', 'exact', 3, 2, 30, 'complete', 'final', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(fx.team_b_reservation)
    .bind(fx.org.0)
    .bind(fx.to + Duration::days(1))
    .execute(&harness.pool)
    .await
    .unwrap();

    // The request stays in the window of its first usage row, measured by
    // its latest row.
    let (status, body, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&currency=EUR"),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(totals(&body), totals_of(&[("EUR", "exact", 1, 30, 0)]));

    // The following window does not count it a second time.
    let (status, body, _) = harness
        .get(
            &usage_uri(fx.org, fx.to, fx.to + Duration::days(7), ""),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(totals(&body), totals_of(&[]));
    harness.pool.close().await;
}

#[tokio::test]
async fn team_admin_is_clamped_to_its_team() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let team_admin = harness.user(fx.org, Some(fx.team_a), "team_admin").await;
    let session = harness.session(team_admin).await;

    let (status, body, _) = harness
        .get(&usage_uri(fx.org, fx.from, fx.to, ""), Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        totals(&body),
        totals_of(&[("USD", "estimated", 1, 40, 0), ("USD", "exact", 1, 100, 0)])
    );

    // Filtering another team is denied rather than silently emptied.
    let (status, _, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, &format!("&team_id={}", fx.team_b.0)),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body, _) = harness
        .get(
            &format!("/api/admin/v1/organizations/{}/traces", fx.org.0),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&fx.team_a_trace.to_string().as_str()));
    assert!(!ids.contains(&fx.team_b_trace.to_string().as_str()));
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["team_id"] == json!(fx.team_a.0.to_string()))
    );

    // Another team's trace does not exist from this actor's point of view.
    let (status, _, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/traces/{}",
                fx.org.0, fx.team_b_trace
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Reconciliation is organization-wide only.
    let (status, _, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/reconciliation-items",
                fx.org.0
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    harness.pool.close().await;
}

#[tokio::test]
async fn cross_tenant_reads_are_denied_and_roles_are_enforced() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let owner = harness.session(fx.owner).await;

    // No binding in the other tenant.
    let (status, _, _) = harness
        .get(&usage_uri(fx.other_org, fx.from, fx.to, ""), Some(&owner))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Another tenant's trace id under this tenant's path is simply absent.
    let (status, _, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/traces/{}",
                fx.org.0, fx.other_org_trace
            ),
            Some(&owner),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Read-only members see usage aggregates but not traces or reconciliation.
    let member = harness.user(fx.org, None, "read_only_member").await;
    let member = harness.session(member).await;
    let (status, _, _) = harness
        .get(&usage_uri(fx.org, fx.from, fx.to, ""), Some(&member))
        .await;
    assert_eq!(status, StatusCode::OK);
    for path in ["traces", "reconciliation-items"] {
        let (status, _, _) = harness
            .get(
                &format!("/api/admin/v1/organizations/{}/{path}", fx.org.0),
                Some(&member),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }

    // Unauthenticated and malformed requests.
    let (status, body, headers) = harness
        .get(&usage_uri(fx.org, fx.from, fx.to, ""), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "authentication_required");
    // Problems are never cached either.
    assert_eq!(headers["cache-control"], "no-store");
    for uri in [
        format!("/api/admin/v1/organizations/{}/usage", fx.org.0),
        usage_uri(fx.org, fx.to, fx.from, ""),
        usage_uri(fx.org, fx.from, fx.to, "&unknown=1"),
        usage_uri(fx.org, fx.from, fx.from + Duration::days(400), ""),
    ] {
        let (status, body, _) = harness.get(&uri, Some(&owner)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(body["code"], "invalid_query", "{uri}");
    }
    harness.pool.close().await;
}

#[tokio::test]
async fn trace_detail_is_metadata_only_and_audited() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let auditor = harness.user(fx.org, None, "auditor").await;
    let session = harness.session(auditor).await;

    let (status, body, headers) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/traces/{}",
                fx.org.0, fx.team_b_trace
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "alias",
            "amount_minor",
            "api_key_id",
            "attempt",
            "batch_id",
            "content_state",
            "correlation_id",
            "currency",
            "custom_id",
            "deployment",
            "duration_ms",
            "end_user",
            "error_category",
            "failover_of",
            "finalized_at",
            "id",
            "input_bound",
            "location",
            "model",
            "operation",
            "organization_id",
            "outcome",
            "output_cap",
            "price_version",
            "pricing_confidence",
            "provider_error",
            "provider_family",
            "provider_request_id",
            "reconciliation",
            "recorded_at",
            "reservation",
            "routing",
            "service_account_id",
            "status",
            "team_id",
            "usage",
            "usage_confidence",
        ]
    );
    assert_eq!(body["usage"]["confidence"], "unknown");
    assert_eq!(body["usage"]["tokens"]["input"], Value::Null);
    assert_eq!(body["usage"]["amount_minor"], Value::Null);
    assert_eq!(body["reconciliation"]["reason"], "provider-unknown");
    assert_eq!(body["reconciliation"]["status"], "open");
    // The fixture's provider failure carries no detail.
    assert_eq!(body["provider_error"], Value::Null);
    assert_eq!(
        body["reservation"]["id"],
        json!(fx.team_b_reservation.to_string())
    );

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'trace.read' AND entity_type = 'inference_trace'
           AND entity_id = $2 AND actor_user_id = $3 AND team_id = $4",
    )
    .bind(fx.org.0)
    .bind(fx.team_b_trace)
    .bind(auditor.0)
    .bind(fx.team_b.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    assert_eq!(body["output_cap"], 100);

    // An embeddings trace reserves input only: its output cap is null.
    let embedder = harness.service_account(fx.org, fx.owner, None).await;
    let (_, embedding_trace) = harness
        .record(
            fx.org,
            Seed {
                team: None,
                service_account: Some(embedder),
                model: "embedding-model",
                currency: "EUR",
                usage: &[("exact", Some(4), "complete", Some(2), Some(0))],
                recorded_at: Utc::now(),
                route: None,
                output_cap: None,
            },
        )
        .await;
    let (status, body, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/traces/{}",
                fx.org.0, embedding_trace
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["output_cap"], Value::Null);

    // The trace list filters by status and pages newest first.
    let (status, body, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/traces?status=finalized&page_size=2",
                fx.org.0
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0]["recorded_at"].as_str() >= items[1]["recorded_at"].as_str());
    assert!(body["next_page_token"].is_string());
    assert!(
        harness
            .audit_count(fx.org, "trace.list", "inference_trace")
            .await
            >= 1
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn traces_filter_by_lists_and_summarize_on_the_server() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let traces = |query: &str| format!("/api/admin/v1/organizations/{}/traces{query}", fx.org.0);
    let ids = |body: &Value| -> Vec<String> {
        let mut ids: Vec<String> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };

    // Lists match any of their values; the other organization never shows.
    let (status, body, _) = harness
        .get(
            &traces(&format!(
                "?model=model-1,model-2&team_id={},{}",
                fx.team_a.0, fx.team_b.0
            )),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 4, "{body}");
    assert!(!ids(&body).contains(&fx.other_org_trace.to_string()));

    // A trace for an app's end user names them, and filters by them.
    let account = Uuid::new_v4();
    let end_user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.service_account (id, organization_id, owner_user_id, name)
         VALUES ($1, $2, $3, 'app')",
    )
    .bind(account)
    .bind(fx.org.0)
    .bind(fx.owner.0)
    .execute(&harness.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id)
         VALUES ($1, $2, $3, 'customer,7')",
    )
    .bind(end_user)
    .bind(fx.org.0)
    .bind(account)
    .execute(&harness.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE gateway.inference_trace SET end_user_id = $1 WHERE id = $2")
        .bind(end_user)
        .bind(fx.team_b_trace)
        .execute(&harness.pool)
        .await
        .unwrap();
    let (status, body, _) = harness
        .get(&traces("?end_user=customer%5C%2C7"), Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), vec![fx.team_b_trace.to_string()]);
    assert_eq!(body["items"][0]["end_user"], "customer,7");
    let (status, body, _) = harness.get(&traces("?model=model-1"), Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["end_user"].is_null())
    );

    // The search matches part of the model, ignoring case.
    let (status, body, _) = harness.get(&traces("?q=DEL-2"), Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), vec![fx.team_b_trace.to_string()]);

    // Counted by team then model: amounts per currency, unpriced apart.
    let (status, body, _) = harness
        .get(&traces("/summary?group_by=team,model"), Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["group_by"], json!(["team", "model"]));
    assert_eq!(body["total"], 5);
    assert_eq!(body["truncated"], false);
    let groups: Vec<(Value, Value, i64, i64, Value)> = body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["values"]["team"].clone(),
                group["values"]["model"].clone(),
                group["count"].as_i64().unwrap(),
                group["unpriced_count"].as_i64().unwrap(),
                group["amounts"].clone(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (
                json!(fx.team_a.0.to_string()),
                json!("model-1"),
                3,
                0,
                json!([{ "currency": "USD", "amount_minor": 5140 }])
            ),
            (
                json!(fx.team_b.0.to_string()),
                json!("model-2"),
                1,
                1,
                json!([])
            ),
            (
                Value::Null,
                json!("model-1"),
                1,
                0,
                json!([{ "currency": "USD", "amount_minor": 7 }])
            ),
        ]
    );

    // Filters narrow the summary too.
    let (status, body, _) = harness
        .get(
            &traces("/summary?group_by=day&model=model-2"),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1);
    assert_eq!(body["groups"][0]["values"]["day"], "2026-03-11");

    // A team admin counts only its own team, and may not ask for another.
    let admin_a = harness.user(fx.org, Some(fx.team_a), "team_admin").await;
    let admin_session = harness.session(admin_a).await;
    let (status, body, _) = harness
        .get(&traces("/summary?group_by=team"), Some(&admin_session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 3);
    for query in [
        format!(
            "/summary?group_by=team&team_id={},{}",
            fx.team_a.0, fx.team_b.0
        ),
        format!("?team_id={},{}", fx.team_a.0, fx.team_b.0),
    ] {
        let (status, body, _) = harness.get(&traces(&query), Some(&admin_session)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{query}: {body}");
    }

    // Malformed requests are refused.
    for query in [
        "/summary",
        "/summary?group_by=region",
        "/summary?group_by=team,team",
        "/summary?group_by=team&page_size=10",
        "?group_by=team",
        "?status=done",
        "?team_id=not-a-uuid",
        "?provider_family=nope",
    ] {
        let (status, body, _) = harness.get(&traces(query), Some(&session)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["code"], "invalid_query", "{query}");
    }

    // Each summary is an audited trace read.
    assert!(
        harness
            .audit_count(fx.org, "trace.list", "inference_trace")
            .await
            >= 4
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn trace_summaries_keep_currencies_apart_and_stop_at_500_groups() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let summary = |query: &str| {
        format!(
            "/api/admin/v1/organizations/{}/traces/summary?{query}",
            fx.org.0
        )
    };

    let admin_b = harness.user(fx.org, Some(fx.team_b), "team_admin").await;
    let account_b = harness
        .service_account(fx.org, admin_b, Some(fx.team_b))
        .await;
    let account_org = harness.service_account(fx.org, fx.owner, None).await;

    // Priced USD and EUR traces in one group stay separate amounts; the
    // fixture's unpriced EUR trace is counted apart.
    for (currency, amount) in [("USD", 25), ("EUR", 9), ("USD", 5)] {
        harness
            .record(
                fx.org,
                Seed {
                    team: Some(fx.team_b),
                    service_account: Some(account_b),
                    model: "model-2",
                    currency,
                    usage: &[("exact", Some(amount), "complete", Some(1), Some(1))],
                    recorded_at: fx.from + Duration::hours(2),
                    route: None,
                    output_cap: Some(100),
                },
            )
            .await;
    }
    let (status, body, _) = harness
        .get(&summary("group_by=model&model=model-2"), Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["groups"][0]["count"], 4, "{body}");
    assert_eq!(body["groups"][0]["unpriced_count"], 1);
    assert_eq!(
        body["groups"][0]["amounts"],
        json!([
            { "currency": "EUR", "amount_minor": 9 },
            { "currency": "USD", "amount_minor": 30 }
        ])
    );

    // Past 500 groups the largest are returned, and the total still counts
    // every matching trace.
    for index in 0..500 {
        harness
            .record(
                fx.org,
                Seed {
                    team: None,
                    service_account: Some(account_org),
                    model: &format!("many-{index:03}"),
                    currency: "USD",
                    usage: &[("exact", Some(1), "complete", Some(1), Some(1))],
                    recorded_at: fx.from + Duration::hours(3),
                    route: None,
                    output_cap: Some(100),
                },
            )
            .await;
    }
    let (status, body, _) = harness
        .get(&summary("group_by=model"), Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["truncated"], true);
    assert_eq!(body["groups"].as_array().unwrap().len(), 500);
    // The fixture's 5 traces, the 3 above and the 500 one-trace models.
    assert_eq!(body["total"], 508);
    // Largest first: the two fixture models lead.
    assert_eq!(body["groups"][0]["values"]["model"], "model-1");
    assert_eq!(body["groups"][1]["values"]["model"], "model-2");
    harness.pool.close().await;
}

#[tokio::test]
async fn usage_groups_by_two_dimensions_and_filters_by_lists() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let usage = |extra: &str| usage_uri(fx.org, fx.from, fx.to, extra);
    // A second model for team A, at the same currency and confidence as its
    // first, whose label holds a comma.
    let admin_a = harness.user(fx.org, Some(fx.team_a), "team_admin").await;
    let account_a = harness
        .service_account(fx.org, admin_a, Some(fx.team_a))
        .await;
    harness
        .record(
            fx.org,
            Seed {
                team: Some(fx.team_a),
                service_account: Some(account_a),
                model: "other,model",
                currency: "USD",
                usage: &[("exact", Some(11), "complete", Some(1), Some(1))],
                recorded_at: fx.from + Duration::hours(30),
                route: None,
                output_cap: Some(100),
            },
        )
        .await;
    let row = |item: &Value| {
        (
            item["values"]["team"].clone(),
            item["values"]["model"].as_str().unwrap().to_owned(),
            item["currency"].as_str().unwrap().to_owned(),
            item["confidence"].as_str().unwrap().to_owned(),
            item["amount_minor"].as_i64().unwrap(),
        )
    };

    // Team then model: each row names both values; organization-level usage
    // has a null team and sorts first.
    let (status, body, _) = harness
        .get(&usage("&group_by=team,model&page_size=100"), Some(&session))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["group_by"], "team,model");
    let all: Vec<_> = body["items"].as_array().unwrap().iter().map(row).collect();
    let team = |id: TeamId| json!(id.0.to_string());
    let mut expected = vec![
        (
            team(fx.team_a),
            "model-1".to_owned(),
            "USD".to_owned(),
            "estimated".to_owned(),
            40,
        ),
        (
            team(fx.team_a),
            "model-1".to_owned(),
            "USD".to_owned(),
            "exact".to_owned(),
            100,
        ),
        (
            team(fx.team_a),
            "other,model".to_owned(),
            "USD".to_owned(),
            "exact".to_owned(),
            11,
        ),
        (
            team(fx.team_b),
            "model-2".to_owned(),
            "EUR".to_owned(),
            "unknown".to_owned(),
            0,
        ),
    ];
    expected.sort_by(|a, b| a.0.as_str().cmp(&b.0.as_str()));
    expected.insert(
        0,
        (
            Value::Null,
            "model-1".to_owned(),
            "USD".to_owned(),
            "exact".to_owned(),
            7,
        ),
    );
    assert_eq!(all, expected);
    // `group` keeps the first dimension's value.
    assert_eq!(body["items"][0]["group"], Value::Null);

    // Paging one row at a time walks the same rows across both dimensions.
    let mut paged = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let extra = match &token {
            Some(token) => format!("&group_by=team,model&page_size=1&page_token={token}"),
            None => "&group_by=team,model&page_size=1".to_owned(),
        };
        let (status, body, _) = harness.get(&usage(&extra), Some(&session)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        paged.extend(body["items"].as_array().unwrap().iter().map(row));
        match body["next_page_token"].as_str() {
            Some(next) => token = Some(next.to_owned()),
            None => break,
        }
        assert!(paged.len() < 10, "paging did not terminate");
    }
    assert_eq!(paged, all);

    // Lists match any of their values.
    let (status, body, _) = harness
        .get(
            &usage(&format!(
                "&group_by=model&team_id={},{}&model=model-1,model-3",
                fx.team_a.0, fx.team_b.0
            )),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let totals: Vec<(String, i64)> = body["totals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|total| {
            (
                total["confidence"].as_str().unwrap().to_owned(),
                total["amount_minor"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        totals,
        vec![("estimated".to_owned(), 40), ("exact".to_owned(), 100)]
    );

    // A comma within a label is escaped; the label is matched whole.
    let (status, body, _) = harness
        .get(
            &usage("&group_by=model&model=other%5C,model"),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let models: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["group"].as_str().unwrap())
        .collect();
    assert_eq!(models, vec!["other,model"]);

    // A team admin may list only its own teams.
    let admin_session = harness.session(admin_a).await;
    let (status, body, _) = harness
        .get(
            &usage(&format!("&team_id={},{}", fx.team_a.0, fx.team_b.0)),
            Some(&admin_session),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    for extra in [
        "&group_by=team,team",
        "&group_by=team,model,day",
        "&group_by=status",
        "&team_id=not-a-uuid",
        "&provider_family=nope",
    ] {
        let (status, body, _) = harness.get(&usage(extra), Some(&session)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{extra}: {body}");
        assert_eq!(body["code"], "invalid_query", "{extra}");
    }
    harness.pool.close().await;
}

#[tokio::test]
async fn reconciliation_items_list_open_work_for_operators() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let operator = harness.user(fx.org, None, "operator").await;
    let session = harness.session(operator).await;

    let uri = |status: &str| {
        format!(
            "/api/admin/v1/organizations/{}/reconciliation-items{status}",
            fx.org.0
        )
    };
    let (status, body, _) = harness.get(&uri(""), Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0]["reservation_id"],
        json!(fx.team_b_reservation.to_string())
    );
    assert_eq!(items[0]["trace_id"], json!(fx.team_b_trace.to_string()));
    assert_eq!(items[0]["currency"], "EUR");
    assert_eq!(items[0]["reservation_state"], "settled");

    let (_, body, _) = harness.get(&uri("?status=resolved"), Some(&session)).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    let (status, _, _) = harness.get(&uri("?status=bogus"), Some(&session)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        harness
            .audit_count(fx.org, "reconciliation.list", "reconciliation_item")
            .await
            >= 1
    );
    harness.pool.close().await;
}

/// An open item whose reservation is unknown and still holds 500 in an
/// organization bucket and a team bucket. Returns `(item, reservation,
/// buckets)`.
async fn unknown_item(harness: &Harness, fx: &Fixture) -> (Uuid, Uuid, Vec<Uuid>) {
    let team_owner = harness.user(fx.org, Some(fx.team_a), "team_admin").await;
    let account = harness
        .service_account(fx.org, team_owner, Some(fx.team_a))
        .await;
    let (reservation, _) = harness
        .record_in_state(
            fx.org,
            Seed {
                team: Some(fx.team_a),
                service_account: Some(account),
                model: "model-1",
                currency: "USD",
                usage: &[("unknown", None, "partial", Some(12), Some(3))],
                recorded_at: fx.from + Duration::days(3),
                route: None,
                output_cap: Some(100),
            },
            "unknown",
        )
        .await;
    let buckets = harness.hold(fx.org, fx.team_a, reservation, 500).await;
    let item = harness
        .reconciliation(fx.org, reservation, "provider-unknown")
        .await;
    (item, reservation, buckets)
}

#[tokio::test]
async fn resolving_an_item_charges_the_amount_and_releases_held_quota() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let (item, reservation, buckets) = unknown_item(&harness, &fx).await;
    let session = harness.session(fx.owner).await;
    let headers = [("idempotency-key", "resolve-1"), ("if-match", "*")];

    let (status, body, response_headers) = harness
        .resolve(
            fx.org,
            item,
            &session,
            &headers,
            json!({ "amount_minor": 650 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(response_headers["etag"], "\"v1\"");
    assert_eq!(response_headers["cache-control"], "no-store");
    assert_eq!(body["status"], "resolved");
    assert_eq!(body["resolution"], "operator");
    assert_eq!(body["resolved_amount_minor"], 650);
    assert_eq!(body["resolved_by"], json!(fx.owner.0.to_string()));
    assert_eq!(body["reservation_state"], "settled");
    assert!(body["resolved_at"].is_string());

    // The amount may exceed the reservation, as a bound breach would.
    assert_eq!(harness.buckets(&buckets).await, vec![(0, 650), (0, 650)]);
    assert_eq!(
        harness.allocations(reservation).await,
        vec![("settled".to_owned(), Some(650)); 2]
    );
    let (confidence, amount, input, output, finality): (
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
    ) = sqlx::query_as(
        "SELECT confidence, amount_minor, input_tokens, output_tokens, finality
         FROM gateway.accounting_usage WHERE reservation_id = $1
         ORDER BY recorded_at DESC LIMIT 1",
    )
    .bind(reservation)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        (
            confidence.as_str(),
            amount,
            input,
            output,
            finality.as_str()
        ),
        ("estimated", Some(650), Some(12), Some(3), "final")
    );
    let trace: (String, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT status, outcome, amount_minor FROM gateway.inference_trace
         WHERE reservation_id = $1",
    )
    .bind(reservation)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        trace,
        (
            "finalized".to_owned(),
            Some("operator-resolved".to_owned()),
            Some(650)
        )
    );
    let events: Vec<(String, Value)> = sqlx::query_as(
        "SELECT event_type, payload FROM gateway.outbox_event
         WHERE payload->>'reservation_id' = $1 ORDER BY sequence",
    )
    .bind(reservation.to_string())
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "inference.finalized");
    assert_eq!(events[0].1["outcome"], "operator-resolved");
    assert_eq!(events[0].1["amount_minor"], 650);
    assert_eq!(events[1].0, "reconciliation.resolved");
    assert_eq!(events[1].1["resolution"], "operator");
    assert_eq!(events[1].1["resolved_amount_minor"], 650);
    assert_eq!(events[1].1["resolved_by"], json!(fx.owner.0.to_string()));
    assert_eq!(
        harness
            .audit_count(fx.org, "reconciliation_item.resolve", "reconciliation_item")
            .await,
        1
    );

    // A retry replays the recorded response without charging again.
    let (status, replay, response_headers) = harness
        .resolve(
            fx.org,
            item,
            &session,
            &headers,
            json!({ "amount_minor": 650 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, body);
    assert_eq!(response_headers["etag"], "\"v1\"");
    assert_eq!(harness.buckets(&buckets).await, vec![(0, 650), (0, 650)]);
    assert_eq!(
        harness
            .audit_count(fx.org, "reconciliation_item.resolve", "reconciliation_item")
            .await,
        1
    );

    let (status, body, _) = harness
        .resolve(
            fx.org,
            item,
            &session,
            &headers,
            json!({ "amount_minor": 1 }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "idempotency_conflict", "{body}");

    // A second resolution is refused and changes nothing.
    let (status, body, _) = harness
        .resolve(
            fx.org,
            item,
            &session,
            &[("idempotency-key", "resolve-2"), ("if-match", "*")],
            json!({ "amount_minor": 1 }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "reconciliation_not_open", "{body}");
    assert_eq!(harness.buckets(&buckets).await, vec![(0, 650), (0, 650)]);

    // The list shows the resolution.
    let (_, body, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/reconciliation-items?status=resolved",
                fx.org.0
            ),
            Some(&session),
        )
        .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], json!(item.to_string()));
    assert_eq!(items[0]["resolved_amount_minor"], 650);
    harness.pool.close().await;
}

#[tokio::test]
async fn resolving_an_item_is_owner_and_admin_only_and_validated() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let (item, reservation, buckets) = unknown_item(&harness, &fx).await;
    let body = json!({ "amount_minor": 0 });
    let headers = [("idempotency-key", "resolve"), ("if-match", "*")];

    let operator = harness.user(fx.org, None, "operator").await;
    let auditor = harness.user(fx.org, None, "auditor").await;
    let team_admin = harness.user(fx.org, Some(fx.team_a), "team_admin").await;
    for user in [operator, auditor, team_admin] {
        let session = harness.session(user).await;
        let (status, problem, _) = harness
            .resolve(fx.org, item, &session, &headers, body.clone())
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    }

    // Another tenant's owner cannot see the item.
    let other_owner = harness.user(fx.other_org, None, "organization_owner").await;
    let other_session = harness.session(other_owner).await;
    let (status, _, _) = harness
        .resolve(fx.other_org, item, &other_session, &headers, body.clone())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let admin = harness.user(fx.org, None, "organization_admin").await;
    let session = harness.session(admin).await;
    let cases: [(&[(&str, &str)], Value, StatusCode, &str); 6] = [
        (
            &headers,
            json!({ "amount_minor": -1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_amount",
        ),
        (
            &headers,
            json!({ "amount_minor": 0, "note": "x" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "request_body_unprocessable",
        ),
        (
            &[("idempotency-key", "resolve")],
            body.clone(),
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
        ),
        (
            &[("if-match", "*")],
            body.clone(),
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
        ),
        (
            &[("idempotency-key", "resolve"), ("if-match", "\"v2\"")],
            body.clone(),
            StatusCode::CONFLICT,
            "version_conflict",
        ),
        (
            &[("idempotency-key", "settled"), ("if-match", "*")],
            body.clone(),
            StatusCode::CONFLICT,
            "reconciliation_not_open",
        ),
    ];
    let settled_item = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM gateway.accounting_reconciliation WHERE reservation_id = $1",
    )
    .bind(fx.team_b_reservation)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    for (case_headers, case_body, expected, code) in cases {
        // Only the last case targets the item whose reservation already settled.
        let target = if code == "reconciliation_not_open" {
            settled_item
        } else {
            item
        };
        let (status, problem, _) = harness
            .resolve(fx.org, target, &session, case_headers, case_body)
            .await;
        assert_eq!(status, expected, "{problem}");
        assert_eq!(problem["code"], code, "{problem}");
    }
    let (status, _, _) = harness
        .resolve(fx.org, Uuid::new_v4(), &session, &headers, body.clone())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(harness.buckets(&buckets).await, vec![(500, 0), (500, 0)]);

    // In-flight accounting holding the reservation or a bucket makes the
    // resolution fail fast instead of waiting under the organization lock.
    for (lock, target) in [
        (
            "SELECT 1 FROM gateway.accounting_reservation WHERE id = $1 FOR UPDATE",
            reservation,
        ),
        (
            "SELECT 1 FROM gateway.quota_bucket WHERE id = $1 FOR UPDATE",
            buckets[1],
        ),
    ] {
        let mut settling = harness.pool.begin().await.unwrap();
        sqlx::query(lock)
            .bind(target)
            .execute(&mut *settling)
            .await
            .unwrap();
        let (status, problem, _) = harness
            .resolve(fx.org, item, &session, &headers, body.clone())
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{problem}");
        assert_eq!(problem["code"], "reconciliation_busy", "{problem}");
        settling.rollback().await.unwrap();
    }
    assert_eq!(harness.buckets(&buckets).await, vec![(500, 0), (500, 0)]);

    // Zero releases the held quota without a charge.
    let (status, resolved, _) = harness
        .resolve(fx.org, item, &session, &headers, body)
        .await;
    assert_eq!(status, StatusCode::OK, "{resolved}");
    assert_eq!(resolved["resolved_amount_minor"], 0);
    assert_eq!(harness.buckets(&buckets).await, vec![(0, 0), (0, 0)]);
    assert_eq!(
        harness.allocations(reservation).await,
        vec![("settled".to_owned(), Some(0)); 2]
    );
    harness.pool.close().await;
}

/// A single-deployment release serving `gpt-4o` from `family` in `location`,
/// priced like every `Seed` reservation.
fn azure_manifest(family: &str, location: &str) -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": format!("{family}-{location}")},
        "providers": [{"adapter_instance_key": "azure-main", "family": family}],
        "deployments": [{
            "name": "gpt-4o", "model": "gpt-4o", "location": location,
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
        "routes": [{"alias": "chat", "operation": "chat", "deployment": "gpt-4o"}]
    }))
    .unwrap()
}

#[tokio::test]
async fn provider_family_and_location_stay_distinguishable() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let fx = fixture(&harness).await;
    let session = harness.session(fx.owner).await;
    let account = harness.service_account(fx.org, fx.owner, None).await;

    // The same deployment name served by Azure OpenAI and then by Azure AI
    // Foundry in another region, as across a catalog upgrade.
    let catalog = CatalogStore::new(harness.pool.clone());
    let mut routes = Vec::new();
    for (family, location) in [
        ("azure-openai", "eastus"),
        ("azure-ai-foundry", "westeurope"),
    ] {
        let manifest = azure_manifest(family, location);
        catalog
            .apply(fx.org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap();
        routes.push(
            catalog
                .resolve_active(fx.org, "chat", "chat")
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let day = fx.from + Duration::days(1);
    let mut traces = Vec::new();
    for (route, amount, hour) in [(&routes[0], 100, 3), (&routes[1], 30, 4)] {
        let (_, trace) = harness
            .record(
                fx.org,
                Seed {
                    team: None,
                    service_account: Some(account),
                    model: "gpt-4o",
                    currency: "USD",
                    usage: &[("exact", Some(amount), "complete", Some(1), Some(1))],
                    recorded_at: day + Duration::hours(hour),
                    route: Some(route),
                    output_cap: Some(100),
                },
            )
            .await;
        traces.push(trace);
    }

    let groups = |body: &Value| -> Vec<(Value, String, String, i64, i64)> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["group"].clone(),
                    item["currency"].as_str().unwrap().to_owned(),
                    item["confidence"].as_str().unwrap().to_owned(),
                    item["request_count"].as_i64().unwrap(),
                    item["amount_minor"].as_i64().unwrap(),
                )
            })
            .collect()
    };
    let routed = |body: &Value| -> Vec<(Value, String, String, i64, i64)> {
        groups(body)
            .into_iter()
            .filter(|row| !row.0.is_null())
            .collect()
    };

    // By deployment name the two providers are indistinguishable...
    let (status, body, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&group_by=deployment"),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(groups(&body).contains(&(
        json!("gpt-4o"),
        "USD".to_owned(),
        "exact".to_owned(),
        2,
        130
    )));

    // ...but family and location keep them apart, and usage without catalog
    // provenance groups under null.
    for (group_by, first, second) in [
        ("provider_family", "azure-ai-foundry", "azure-openai"),
        ("location", "eastus", "westeurope"),
    ] {
        let (status, body, _) = harness
            .get(
                &usage_uri(fx.org, fx.from, fx.to, &format!("&group_by={group_by}")),
                Some(&session),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["group_by"], group_by);
        let amount = |group: &str| {
            if group == "eastus" || group == "azure-openai" {
                100
            } else {
                30
            }
        };
        assert_eq!(
            routed(&body),
            vec![
                (
                    json!(first),
                    "USD".to_owned(),
                    "exact".to_owned(),
                    1,
                    amount(first)
                ),
                (
                    json!(second),
                    "USD".to_owned(),
                    "exact".to_owned(),
                    1,
                    amount(second)
                ),
            ],
            "{group_by}"
        );
        assert!(
            groups(&body).iter().any(|row| row.0.is_null()),
            "{group_by}"
        );

        // Paging one row at a time, across the null group, yields the same rows.
        let mut paged = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut extra = format!("&group_by={group_by}&page_size=1");
            if let Some(token) = &token {
                extra.push_str(&format!("&page_token={token}"));
            }
            let (status, page, _) = harness
                .get(&usage_uri(fx.org, fx.from, fx.to, &extra), Some(&session))
                .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            paged.extend(groups(&page));
            match page["next_page_token"].as_str() {
                Some(next) => token = Some(next.to_owned()),
                None => break,
            }
            assert!(paged.len() < 20, "paging did not terminate");
        }
        assert_eq!(paged, groups(&body), "{group_by}");
    }

    // Filters narrow to one provider or region.
    for (filter, amount) in [
        ("&provider_family=azure-openai", 100),
        ("&location=westeurope", 30),
        ("&provider_family=azure-ai-foundry&location=westeurope", 30),
    ] {
        let (status, body, _) = harness
            .get(&usage_uri(fx.org, fx.from, fx.to, filter), Some(&session))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            totals(&body),
            totals_of(&[("USD", "exact", 1, amount, 0)]),
            "{filter}"
        );
    }
    let (_, body, _) = harness
        .get(
            &usage_uri(
                fx.org,
                fx.from,
                fx.to,
                "&provider_family=azure-openai&location=westeurope",
            ),
            Some(&session),
        )
        .await;
    assert_eq!(totals(&body), totals_of(&[]));
    for filter in ["&provider_family=azure", "&location="] {
        let (status, body, _) = harness
            .get(&usage_uri(fx.org, fx.from, fx.to, filter), Some(&session))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{filter}");
        assert_eq!(body["code"], "invalid_query", "{filter}");
    }

    // A page token is bound to the provenance filters.
    let (_, first, _) = harness
        .get(
            &usage_uri(
                fx.org,
                fx.from,
                fx.to,
                "&group_by=location&provider_family=azure-openai&page_size=1",
            ),
            Some(&session),
        )
        .await;
    let (_, all, _) = harness
        .get(
            &usage_uri(fx.org, fx.from, fx.to, "&group_by=location&page_size=1"),
            Some(&session),
        )
        .await;
    let token = all["next_page_token"].as_str().unwrap();
    assert!(first["next_page_token"].is_null());
    let (status, body, _) = harness
        .get(
            &usage_uri(
                fx.org,
                fx.from,
                fx.to,
                &format!(
                    "&group_by=location&provider_family=azure-openai&page_size=1&page_token={token}"
                ),
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "page_token_query_mismatch");

    // Trace summaries carry the routed family and location.
    let (status, body, _) = harness
        .get(
            &format!("/api/admin/v1/organizations/{}/traces", fx.org.0),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let provenance = |id: Uuid| {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == json!(id.to_string()))
            .map(|item| (item["provider_family"].clone(), item["location"].clone()))
            .unwrap()
    };
    assert_eq!(
        provenance(traces[0]),
        (json!("azure-openai"), json!("eastus"))
    );
    assert_eq!(
        provenance(traces[1]),
        (json!("azure-ai-foundry"), json!("westeurope"))
    );
    assert_eq!(provenance(fx.team_a_trace), (Value::Null, Value::Null));

    // Trace detail carries them too.
    let (status, body, _) = harness
        .get(
            &format!(
                "/api/admin/v1/organizations/{}/traces/{}",
                fx.org.0, traces[1]
            ),
            Some(&session),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider_family"], "azure-ai-foundry");
    assert_eq!(body["location"], "westeurope");
    assert_eq!(body["deployment"], "gpt-4o");
    harness.pool.close().await;
}
