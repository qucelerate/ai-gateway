#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for data retention and export: the policy API (RBAC,
//! versioning, idempotency, audit), the worker purge against seeded
//! accounting and audit rows, and NDJSON/CSV exports through the real router.
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
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
};
use chrono::{DateTime, Duration, Utc};
use gateway::{
    OrganizationId, UserAccountId,
    domain::retention::{PurgedCounts, RetentionKind},
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    jobs::retention::purge_expired,
    persistence::{AdminStore, AuthStore, IdentityStore, retention::RetentionStore},
    transport::retention::router,
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
const TEST_PEPPER: &[u8] = b"retention-integration-pepper";
const HMAC_KEY: &[u8] = b"retention-integration-hmac-key-0032";

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

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    store: RetentionStore,
    router: Router,
}

struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(5))
            .connect(database_url)
            .await
            .unwrap_or_else(|error| panic!("GATEWAY_TEST_DATABASE_URL is unreachable: {error}"));
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[41_u8; 32]).unwrap();
        let store = RetentionStore::new(pool.clone());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            store.clone(),
        );
        Self {
            pool,
            identity,
            crypto,
            store,
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

    /// A user with one admin role binding: org-wide, or `team_admin` of a
    /// new team when `role` is `team_admin`.
    async fn user(&self, org: OrganizationId, role: &str) -> UserAccountId {
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
        let team = if role == "team_admin" {
            let team = self.identity.create_team(org, "team").await.unwrap().id;
            self.identity
                .add_team_membership(org, team, user, MembershipRole::Admin)
                .await
                .unwrap();
            Some(team.0)
        } else {
            None
        };
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(team)
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> String {
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
        format!(
            "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={browser}",
            session.token
        )
    }

    async fn as_role(&self, org: OrganizationId, role: &str) -> String {
        let user = self.user(org, role).await;
        self.session(user).await
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        cookie: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(COOKIE, cookie);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let response = self
            .router
            .clone()
            .oneshot(
                builder
                    .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Response {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn put_policy(
        &self,
        org: OrganizationId,
        cookie: &str,
        key: &str,
        if_match: &str,
        body: Value,
    ) -> Response {
        self.send(
            Method::PUT,
            &policy_uri(org),
            cookie,
            &[("idempotency-key", key), ("if-match", if_match)],
            Some(body),
        )
        .await
    }

    /// A reservation in `state` created at `at`, with a finalized trace and
    /// one final usage row at the same time. Returns its id.
    async fn request(
        &self,
        org: OrganizationId,
        state: &str,
        model: &str,
        at: DateTime<Utc>,
    ) -> Uuid {
        let reservation = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, idempotency_key, fingerprint, state, reserved_minor,
                  expires_at, output_token_cap, input_price_minor, output_price_minor,
                  cached_input_price_minor, reasoning_price_minor, other_price_minor,
                  price_version, pricing_confidence, currency, deployment, model, trace_id,
                  created_at)
             VALUES ($1, $2, $3, $4, $5, 500, $6, 100, 1, 1, 1, 1, 1, 'v1', 'known', 'USD',
                     'deploy-a', $7, $8, $9)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(state)
        .bind(at + Duration::minutes(5))
        .bind(model)
        .bind(Uuid::new_v4())
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, reservation_id, correlation_id, operation, status,
                  outcome, model, input_bound, output_cap, price_version, pricing_confidence,
                  currency, amount_minor, metadata, recorded_at, finalized_at)
             VALUES ($1, $2, $3, $4, 'inference', 'finalized', 'succeeded', $5, 64, 100, 'v1',
                     'known', 'USD', 120,
                     jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency', 'USD'),
                     $6, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(model)
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        self.usage(org, reservation, 1, at).await;
        reservation
    }

    /// `rows` final usage rows for `reservation`, one microsecond apart.
    async fn usage(&self, org: OrganizationId, reservation: Uuid, rows: i32, at: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO gateway.accounting_usage
                 (id, reservation_id, organization_id, currency, confidence, input_tokens,
                  output_tokens, amount_minor, completeness, finality, recorded_at)
             SELECT gen_random_uuid(), $1, $2, 'USD', 'exact', 10, 20, 120, 'complete', 'final',
                    $3 + n * interval '1 microsecond'
             FROM generate_series(0, $4 - 1) AS n",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(at)
        .bind(rows)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Holds an allocation of `reservation` in a new organization bucket.
    async fn allocation(&self, org: OrganizationId, reservation: Uuid) {
        let bucket = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, scope_kind, period_start, period_end, currency,
                  limit_minor, reserved_minor)
             VALUES ($1, $2, 'organization', now() - interval '400 days',
                     now() + interval '1 day', 'USD', 100000, 0)",
        )
        .bind(bucket)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.accounting_allocation
                 (id, reservation_id, quota_bucket_id, organization_id, reserved_minor, state,
                  released_at)
             VALUES ($1, $2, $3, $4, 500, 'released', now())",
        )
        .bind(Uuid::new_v4())
        .bind(reservation)
        .bind(bucket)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn reconciliation(&self, org: OrganizationId, reservation: Uuid, resolved: bool) {
        sqlx::query(
            "INSERT INTO gateway.accounting_reconciliation
                 (id, organization_id, reservation_id, reason, status, resolved_at)
             VALUES ($1, $2, $3, 'provider-unknown', $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(reservation)
        .bind(if resolved { "resolved" } else { "open" })
        .bind(resolved.then(Utc::now))
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn audit_event(&self, org: OrganizationId, at: DateTime<Utc>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.audit_event
                 (id, organization_id, actor_type, action, entity_type, entity_id, metadata,
                  occurred_at)
             VALUES ($1, $2, 'system', 'api_key.created', 'api_key', $3,
                     jsonb_build_object('api_key_id', $3::text), $4)",
        )
        .bind(id)
        .bind(org.0)
        .bind(Uuid::new_v4())
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    async fn exists(&self, table: &str, column: &str, id: Uuid) -> bool {
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM gateway.{table} WHERE {column} = $1)"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn audit_actions(&self, org: OrganizationId, entity_type: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, outcome FROM gateway.audit_event
             WHERE organization_id = $1 AND entity_type = $2 ORDER BY occurred_at, id",
        )
        .bind(org.0)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn policy_uri(org: OrganizationId) -> String {
    format!("/api/admin/v1/organizations/{}/retention-policy", org.0)
}

fn export_uri(org: OrganizationId, dataset: &str, query: &str) -> String {
    format!(
        "/api/admin/v1/organizations/{}/exports/{dataset}?{query}",
        org.0
    )
}

#[tokio::test]
async fn tenant_admins_set_a_versioned_audited_policy() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("retention-policy").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let reader = harness.as_role(org, "read_only_member").await;

    // Unset: everything is kept, and there is no version yet.
    let response = harness
        .send(Method::GET, &policy_uri(org), &reader, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let policy = response.json();
    assert_eq!(
        (
            &policy["trace_days"],
            &policy["usage_days"],
            &policy["audit_days"]
        ),
        (&Value::Null, &Value::Null, &Value::Null)
    );
    assert_eq!(policy["version"], 0);
    assert!(response.headers.get("etag").is_none());
    assert_eq!(response.headers["cache-control"], "no-store");

    // Readers may not set it.
    let body = json!({"trace_days": 30, "usage_days": 400, "audit_days": 365});
    let response = harness
        .put_policy(org, &reader, "reader", "*", body.clone())
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    let created = harness
        .put_policy(org, &admin, "set-1", "*", body.clone())
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.headers["etag"], "\"v1\"");
    let policy = created.json();
    assert_eq!(
        (
            &policy["trace_days"],
            &policy["usage_days"],
            &policy["audit_days"]
        ),
        (&json!(30), &json!(400), &json!(365))
    );
    assert_eq!(
        policy["purged"],
        json!({"traces": 0, "reservations": 0, "audit_events": 0})
    );
    let replayed = harness
        .put_policy(org, &admin, "set-1", "*", body.clone())
        .await;
    assert_eq!(replayed.status, StatusCode::OK);
    assert_eq!(replayed.json(), created.json());

    for (body, code) in [
        (
            json!({"trace_days": 60, "usage_days": 30}),
            "invalid_retention_policy",
        ),
        (json!({"audit_days": 30}), "invalid_retention_policy"),
        // Expiring usage would take traces kept indefinitely with it.
        (json!({"usage_days": 30}), "invalid_retention_policy"),
        (json!({"usage_days": -1}), "request_body_unprocessable"),
        (
            json!({"usage_days": 30, "keep": true}),
            "request_body_unprocessable",
        ),
    ] {
        let response = harness.put_policy(org, &admin, "bad", "*", body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            response.body
        );
        assert_eq!(response.json()["code"], code, "{}", response.body);
    }
    let stale = harness
        .put_policy(org, &admin, "set-2", "\"v7\"", json!({}))
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);

    // Left out means kept indefinitely.
    let updated = harness
        .put_policy(org, &admin, "set-3", "\"v1\"", json!({"audit_days": 90}))
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.headers["etag"], "\"v2\"");
    let response = harness
        .send(Method::GET, &policy_uri(org), &reader, &[], None)
        .await;
    assert_eq!(response.headers["etag"], "\"v2\"");
    assert_eq!(response.json()["trace_days"], Value::Null);
    assert_eq!(response.json()["audit_days"], 90);

    let team_admin = harness.as_role(org, "team_admin").await;
    let response = harness
        .send(Method::GET, &policy_uri(org), &team_admin, &[], None)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    assert_eq!(
        harness.audit_actions(org, "retention_policy").await,
        [
            ("retention_policy.set".to_owned(), "denied".to_owned()),
            ("retention_policy.set".to_owned(), "succeeded".to_owned()),
            ("retention_policy.set".to_owned(), "succeeded".to_owned()),
        ]
    );
}

#[tokio::test]
async fn the_purge_removes_only_expired_final_data() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("retention-purge").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let untouched = harness.org("retention-none").await;
    let now = Utc::now();
    let days = Duration::days;

    let expired = harness.request(org, "settled", "m", now - days(40)).await;
    harness.allocation(org, expired).await;
    harness.reconciliation(org, expired, true).await;
    // Old but not final, or with an open reconciliation item.
    let claimed = harness.request(org, "claimed", "m", now - days(40)).await;
    let disputed = harness.request(org, "unknown", "m", now - days(40)).await;
    harness.reconciliation(org, disputed, false).await;
    // Traces expire first.
    let recent = harness.request(org, "settled", "m", now - days(10)).await;
    let fresh = harness.request(org, "settled", "m", now - days(1)).await;
    let old_audit = harness.audit_event(org, now - days(100)).await;
    let new_audit = harness.audit_event(org, now - days(10)).await;
    // An organization without a policy keeps everything.
    let elsewhere = harness
        .request(untouched, "settled", "m", now - days(400))
        .await;
    let elsewhere_audit = harness.audit_event(untouched, now - days(400)).await;

    // Deletes stay blocked outside the purge.
    for (table, column, id) in [
        ("accounting_usage", "reservation_id", expired),
        ("audit_event", "id", old_audit),
    ] {
        let error = sqlx::query(&format!("DELETE FROM gateway.{table} WHERE {column} = $1"))
            .bind(id)
            .execute(&harness.pool)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("append-only") || error.to_string().contains("immutable"),
            "{error}"
        );
    }

    let response = harness
        .put_policy(
            org,
            &admin,
            "purge",
            "*",
            json!({"trace_days": 7, "usage_days": 30, "audit_days": 90}),
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);

    let report = purge_expired(&harness.store, now).await.unwrap();
    assert_eq!(report.failed, 0);
    let purged = report.purged;
    assert!(purged.traces >= 3 && purged.reservations >= 1 && purged.audit_events >= 1);

    for table in [
        "accounting_reservation",
        "accounting_usage",
        "accounting_allocation",
        "accounting_reconciliation",
        "inference_trace",
    ] {
        let column = if table == "accounting_reservation" {
            "id"
        } else {
            "reservation_id"
        };
        assert!(!harness.exists(table, column, expired).await, "{table}");
    }
    for kept in [claimed, disputed, recent, fresh, elsewhere] {
        assert!(harness.exists("accounting_reservation", "id", kept).await);
        assert!(
            harness
                .exists("accounting_usage", "reservation_id", kept)
                .await
        );
    }
    assert!(
        !harness
            .exists("inference_trace", "reservation_id", recent)
            .await
    );
    assert!(
        harness
            .exists("inference_trace", "reservation_id", fresh)
            .await
    );
    // A final reservation kept for its open item still loses its old trace.
    assert!(
        !harness
            .exists("inference_trace", "reservation_id", disputed)
            .await
    );
    assert!(
        harness
            .exists("inference_trace", "reservation_id", claimed)
            .await
    );
    assert!(!harness.exists("audit_event", "id", old_audit).await);
    assert!(harness.exists("audit_event", "id", new_audit).await);
    assert!(harness.exists("audit_event", "id", elsewhere_audit).await);

    let response = harness
        .send(Method::GET, &policy_uri(org), &admin, &[], None)
        .await;
    let policy = response.json();
    assert_eq!(
        policy["purged"],
        json!({"traces": 3, "reservations": 1, "audit_events": 1}),
        "{policy}"
    );
    assert!(policy["last_purged_at"].is_string());
    // The purge is in the audit log, by the system: one event per batch
    // that deleted something (traces, usage, audit events), one run.
    let purges = || async {
        sqlx::query_as::<_, (String, String, Option<Uuid>, Uuid, String)>(
            "SELECT outcome, actor_type, actor_user_id, request_id, action FROM gateway.audit_event
             WHERE organization_id = $1 AND action LIKE 'retention.%' ORDER BY action",
        )
        .bind(org.0)
        .fetch_all(&harness.pool)
        .await
        .unwrap()
    };
    let first = purges().await;
    assert_eq!(first.len(), 3, "{first:?}");
    let actions: Vec<&str> = first.iter().map(|event| event.4.as_str()).collect();
    assert_eq!(
        actions,
        [
            "retention.audit_events_purged",
            "retention.traces_purged",
            "retention.usage_purged"
        ]
    );
    assert!(first.iter().all(|(outcome, actor, user, _, _)| {
        outcome == "succeeded" && actor == "system" && user.is_none()
    }));
    assert!(
        first.iter().all(|event| event.3 == first[0].3),
        "one run id"
    );

    // A second run finds nothing more; the totals stay.
    purge_expired(&harness.store, now).await.unwrap();
    let response = harness
        .send(Method::GET, &policy_uri(org), &admin, &[], None)
        .await;
    assert_eq!(response.json()["purged"], policy["purged"]);
    // A run that deletes nothing writes no event.
    assert_eq!(purges().await, first);

    // Each batch reads the policy as it stands: once a period is removed,
    // nothing more of that kind is deleted.
    let older = harness.request(org, "settled", "m", now - days(50)).await;
    let response = harness
        .put_policy(org, &admin, "keep", "\"v1\"", json!({"audit_days": 90}))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    for kind in [RetentionKind::Traces, RetentionKind::Usage] {
        assert_eq!(
            harness
                .store
                .purge(kind, org.0, now, 1_000, Uuid::new_v4())
                .await
                .unwrap(),
            PurgedCounts::default()
        );
    }
    assert!(harness.exists("accounting_reservation", "id", older).await);
    assert!(
        harness
            .exists("inference_trace", "reservation_id", older)
            .await
    );

    // Traces removed with their usage are counted as traces, in the same
    // transaction as the batch.
    let other = harness.org("retention-counts").await;
    let other_admin = harness.as_role(other, "organization_admin").await;
    let response = harness
        .put_policy(
            other,
            &other_admin,
            "counts",
            "*",
            json!({"trace_days": 30, "usage_days": 30, "audit_days": 90}),
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
    harness.request(other, "settled", "m", now - days(40)).await;
    assert_eq!(
        harness
            .store
            .purge(RetentionKind::Usage, other.0, now, 1_000, Uuid::new_v4())
            .await
            .unwrap(),
        PurgedCounts {
            traces: 1,
            reservations: 1,
            audit_events: 0
        }
    );
    let response = harness
        .send(Method::GET, &policy_uri(other), &other_admin, &[], None)
        .await;
    assert_eq!(
        response.json()["purged"],
        json!({"traces": 1, "reservations": 1, "audit_events": 0})
    );

    // An old export record stays while an audit event still names it.
    let export = |id: Uuid| {
        sqlx::query(
            "INSERT INTO gateway.data_export
                 (id, organization_id, dataset, format, range_from, range_to, request_id,
                  status, started_at, finished_at)
             VALUES ($1, $2, 'usage', 'csv', now() - interval '200 days',
                     now() - interval '150 days', gen_random_uuid(), 'completed',
                     now() - interval '100 days', now() - interval '100 days')",
        )
        .bind(id)
        .bind(other.0)
        .execute(&harness.pool)
    };
    let (named, orphan) = (Uuid::new_v4(), Uuid::new_v4());
    export(named).await.unwrap();
    export(orphan).await.unwrap();
    sqlx::query(
        "INSERT INTO gateway.audit_event
             (id, organization_id, actor_type, action, entity_type, entity_id, metadata)
         VALUES (gen_random_uuid(), $1, 'system', 'usage.exported', 'data_export', $2,
                 jsonb_build_object('target_id', $2::text))",
    )
    .bind(other.0)
    .bind(named)
    .execute(&harness.pool)
    .await
    .unwrap();
    harness
        .store
        .purge(
            RetentionKind::AuditEvents,
            other.0,
            now,
            1_000,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
    assert!(harness.exists("data_export", "id", named).await);
    assert!(!harness.exists("data_export", "id", orphan).await);
}

#[tokio::test]
async fn exports_stream_ndjson_and_safe_csv() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("retention-export").await;
    let reader = harness.as_role(org, "read_only_member").await;
    let auditor = harness.as_role(org, "auditor").await;
    let team_admin = harness.as_role(org, "team_admin").await;
    let now = Utc::now();
    let at = now - Duration::days(2);
    let reservation = harness.request(org, "settled", "=cmd|calc", at).await;
    // More rows than one export batch.
    harness.usage(org, reservation, 1_004, at).await;
    harness.audit_event(org, at).await;
    // Outside the range.
    harness
        .request(org, "settled", "old", now - Duration::days(40))
        .await;

    let response = harness
        .send(
            Method::GET,
            &export_uri(org, "usage", ""),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.headers["content-type"], "application/x-ndjson");
    let disposition = response.headers["content-disposition"].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"usage-")
            && disposition.ends_with(".ndjson\""),
        "{disposition}"
    );
    let lines: Vec<Value> = response
        .body
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1_005);
    assert_eq!(lines[0]["reservation_id"], reservation.to_string());
    assert_eq!(lines[0]["model"], "=cmd|calc");
    assert_eq!(lines[0]["input_tokens"], 10);
    assert_eq!(lines[0]["amount_minor"], 120);
    assert!(lines[0]["recorded_at"].as_str().unwrap().ends_with('Z'));
    let mut seen: Vec<&str> = lines
        .iter()
        .map(|line| line["usage_id"].as_str().unwrap())
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 1_005, "no row twice across batches");

    let from = (at - Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let response = harness
        .send(
            Method::GET,
            &export_uri(org, "usage", &format!("format=csv&from={from}")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers["content-type"], "text/csv; charset=utf-8");
    let rows: Vec<&str> = response.body.split("\r\n").collect();
    assert!(rows[0].starts_with("usage_id,recorded_at,reservation_id,team_id"));
    assert_eq!(
        rows.len(),
        1 + 1_005 + 1,
        "header, rows, and the final line end"
    );
    assert!(rows[1].contains(",'=cmd|calc,"), "{}", rows[1]);

    let response = harness
        .send(
            Method::GET,
            &export_uri(org, "audit-events", "format=csv"),
            &auditor,
            &[],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert!(response.body.contains("api_key.created"));
    let response = harness
        .send(
            Method::GET,
            &export_uri(org, "traces", ""),
            &auditor,
            &[],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.lines().count(), 1);

    // Each dataset needs its own read permission, held org-wide.
    for (cookie, dataset) in [
        (&reader, "traces"),
        (&reader, "audit-events"),
        (&team_admin, "usage"),
    ] {
        let response = harness
            .send(
                Method::GET,
                &export_uri(org, dataset, ""),
                cookie,
                &[],
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{dataset}");
    }
    for query in [
        "format=xml",
        "from=yesterday",
        "from=2020-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
        "limit=5",
    ] {
        let response = harness
            .send(
                Method::GET,
                &export_uri(org, "usage", query),
                &reader,
                &[],
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(response.json()["code"], "invalid_query");
    }
    let response = harness
        .send(
            Method::GET,
            &export_uri(org, "users", ""),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // A download dropped before its body is read is recorded as abandoned,
    // and audited as failed.
    let response = harness
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(export_uri(org, "usage", "format=csv"))
                .header(COOKIE, reader.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    tokio::time::sleep(StdDuration::from_millis(300)).await;

    assert_eq!(
        harness.audit_actions(org, "data_export").await,
        [
            ("usage.exported".to_owned(), "succeeded".to_owned()),
            ("usage.exported".to_owned(), "succeeded".to_owned()),
            ("audit_events.exported".to_owned(), "succeeded".to_owned()),
            ("traces.exported".to_owned(), "succeeded".to_owned()),
            ("usage.exported".to_owned(), "succeeded".to_owned()),
            ("usage.exported".to_owned(), "failed".to_owned()),
        ]
    );

    // Auditors see what each export covered and how it ended; readers may
    // not list them.
    let list = format!("/api/admin/v1/organizations/{}/exports", org.0);
    let response = harness.send(Method::GET, &list, &reader, &[], None).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let mut items = Vec::new();
    let mut uri = format!("{list}?page_size=2");
    loop {
        let response = harness.send(Method::GET, &uri, &auditor, &[], None).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let page = response.json();
        items.extend(page["items"].as_array().unwrap().clone());
        let Some(token) = page["next_page_token"].as_str() else {
            break;
        };
        uri = format!("{list}?page_size=2&page_token={token}");
    }
    let summary: Vec<(&str, &str, &str, u64)> = items
        .iter()
        .map(|item| {
            (
                item["dataset"].as_str().unwrap(),
                item["format"].as_str().unwrap(),
                item["status"].as_str().unwrap(),
                item["rows"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(summary.len(), 5, "{items:?}");
    // Newest first.
    assert_eq!(summary[0], ("usage", "csv", "abandoned", 0));
    assert_eq!(summary[1], ("traces", "ndjson", "completed", 1));
    assert_eq!(summary[2].0, "audit-events");
    assert_eq!(summary[3], ("usage", "csv", "completed", 1_005));
    assert_eq!(summary[4], ("usage", "ndjson", "completed", 1_005));
    assert_eq!(items[3]["from"].as_str().unwrap()[..19], from[..19]);
    // The audit events name the export record.
    let named: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event e
         JOIN gateway.data_export x ON x.id = e.entity_id
         WHERE e.organization_id = $1 AND e.entity_type = 'data_export'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(named, 6);
}
