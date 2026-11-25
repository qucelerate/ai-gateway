#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for usage export: the admin lane (driven through the
//! real router, RBAC, idempotency, versioning, verification and audit), and
//! the queue from outbox dispatch through sealing to written objects, with
//! retries, failing reports, destination changes and retention.
//!
//! Objects go to an in-memory writer; the cloud writers are unit-tested
//! against local fakes. Run with `GATEWAY_TEST_DATABASE_URL` set; tests are
//! skipped locally when it is absent.

use std::{
    io::Read,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
};
use chrono::{DateTime, Duration, DurationRound, Utc};
use flate2::read::GzDecoder;
use gateway::{
    OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        identity::MembershipRole,
        keys::ApiKeyPepper,
        outbox::{InferenceFinalized, OutboxPayload},
        usage_export::{DestinationInput, ObjectWriter, UsageRecord, WriteError},
    },
    jobs::usage_export::UsageExportRunner,
    persistence::{
        AdminStore, AuthStore, IdentityStore, OutboxStore,
        outbox::{self, Sinks},
        usage_export::{UsageExportSink, UsageExportStore},
        webhooks::WebhookFanout,
    },
    transport::usage_export::router,
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
const TEST_PEPPER: &[u8] = b"usage-export-integration-pepper";
const HMAC_KEY: &[u8] = b"usage-export-integration-hmac-key-0032";

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

#[derive(Clone, Debug)]
struct Written {
    organization: OrganizationId,
    destination: DestinationInput,
    name: String,
    content_type: &'static str,
    body: Vec<u8>,
}

/// Remembers every write; fails them while `fail` holds an error.
#[derive(Default)]
struct MemoryWriter {
    written: Mutex<Vec<Written>>,
    fail: Mutex<Option<WriteError>>,
}

#[async_trait]
impl ObjectWriter for MemoryWriter {
    async fn write(
        &self,
        organization: OrganizationId,
        destination: &DestinationInput,
        name: &str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Result<(), WriteError> {
        if let Some(error) = *self.fail.lock().unwrap() {
            return Err(error);
        }
        self.written.lock().unwrap().push(Written {
            organization,
            destination: destination.clone(),
            name: name.to_owned(),
            content_type,
            body,
        });
        Ok(())
    }
}

impl MemoryWriter {
    fn take(&self) -> Vec<Written> {
        std::mem::take(&mut *self.written.lock().unwrap())
    }

    fn failing(&self, error: Option<WriteError>) {
        *self.fail.lock().unwrap() = error;
    }
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
    writer: Arc<MemoryWriter>,
    store: UsageExportStore,
    outbox: OutboxStore,
    runner: UsageExportRunner,
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[31_u8; 32]).unwrap();
        let writer = Arc::new(MemoryWriter::default());
        let store = UsageExportStore::new(pool.clone());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            store.clone(),
            writer.clone(),
        );
        let runner = UsageExportRunner::new(store.clone(), writer.clone());
        Self {
            outbox: OutboxStore::new(pool.clone()),
            pool,
            identity,
            crypto,
            router,
            writer,
            store,
            runner,
        }
    }

    async fn org(&self) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("usage-export-{suffix}"), "Usage export")
            .await
            .unwrap()
            .id
    }

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
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
             VALUES ($1, $2, $3, $4, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
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

    async fn send(
        &self,
        method: Method,
        org: OrganizationId,
        session: &(String, String),
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(format!(
                "/api/admin/v1/organizations/{}/usage-export",
                org.0
            ))
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
        let body = match body {
            Some(body) => {
                builder = builder.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body, headers)
    }

    async fn put(
        &self,
        org: OrganizationId,
        session: &(String, String),
        if_match: &str,
        body: Value,
    ) -> (StatusCode, Value, HeaderMap) {
        let key = Uuid::new_v4().to_string();
        self.send(
            Method::PUT,
            org,
            session,
            &[("idempotency-key", &key), ("if-match", if_match)],
            Some(body),
        )
        .await
    }

    /// An organization with an owner and an export to GCS under
    /// `warehouse`, enabled `enabled_ago` before now.
    async fn exporting_org(&self, enabled_ago: Duration) -> (OrganizationId, (String, String)) {
        let org = self.org().await;
        let owner = self.user(org, "organization_owner").await;
        let session = self.session(owner).await;
        let (status, _, _) = self.put(org, &session, "*", gcs("warehouse")).await;
        assert_eq!(status, StatusCode::CREATED);
        self.writer.take();
        sqlx::query("UPDATE gateway.usage_export SET enabled_at = $2 WHERE organization_id = $1")
            .bind(org.0)
            .bind(Utc::now() - enabled_ago)
            .execute(&self.pool)
            .await
            .unwrap();
        (org, session)
    }

    /// A settled reservation and its trace finalized at `at`, with exact
    /// usage, and its `inference.finalized` event. Returns the trace.
    async fn finalize(&self, org: OrganizationId, at: DateTime<Utc>) -> Uuid {
        self.finalize_for(org, at, None).await
    }

    /// A finalized request, made by `for_end_user`'s service account for
    /// its end user when given.
    async fn finalize_for(
        &self,
        org: OrganizationId,
        at: DateTime<Utc>,
        for_end_user: Option<(Uuid, Uuid)>,
    ) -> Uuid {
        let reservation = Uuid::new_v4();
        let trace = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, idempotency_key, fingerprint, state, reserved_minor,
                  expires_at, output_token_cap, input_price_minor, output_price_minor,
                  cached_input_price_minor, reasoning_price_minor, other_price_minor,
                  price_version, pricing_confidence, currency, deployment, model, trace_id,
                  service_account_id, end_user_id)
             VALUES ($1, $2, $3, $4, 'settled', 500, $5, 100, 1, 1, 1, 1, 1, 'v1', 'known',
                     'USD', 'deploy-a', 'model-1', $6, $7, $8)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(at + Duration::minutes(5))
        .bind(trace)
        .bind(for_end_user.map(|(account, _)| account))
        .bind(for_end_user.map(|(_, end_user)| end_user))
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.inference_trace
                 (id, organization_id, reservation_id, correlation_id, operation, status,
                  outcome, provider_request_id, model, input_bound, output_cap, price_version,
                  pricing_confidence, currency, amount_minor, metadata, recorded_at,
                  finalized_at)
             VALUES ($1, $2, $3, $4, 'inference', 'finalized', 'succeeded', 'req_opaque',
                     'model-1', 64, 100, 'v1', 'known', 'USD', 42,
                     jsonb_build_object('input_bound', 64, 'output_cap', 100, 'currency', 'USD'),
                     $5, $5)",
        )
        .bind(trace)
        .bind(org.0)
        .bind(reservation)
        .bind(Uuid::new_v4())
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.accounting_usage
                 (id, reservation_id, organization_id, currency, confidence, input_tokens,
                  output_tokens, amount_minor, completeness, finality, recorded_at)
             VALUES ($1, $2, $3, 'USD', 'exact', 120, 30, 42, 'complete', 'final', $4)",
        )
        .bind(Uuid::new_v4())
        .bind(reservation)
        .bind(org.0)
        .bind(at)
        .execute(&self.pool)
        .await
        .unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        outbox::emit(
            &mut conn,
            org,
            &OutboxPayload::InferenceFinalized(InferenceFinalized {
                trace_id: trace,
                reservation_id: reservation,
                team_id: None,
                service_account_id: None,
                model: "model-1".into(),
                currency: "USD".into(),
                outcome: "settled".into(),
                amount_minor: Some(42),
                finalized_at: at,
            }),
        )
        .await
        .unwrap();
        trace
    }

    /// Starts a pipeline test from an empty export queue: the worker's
    /// rounds take every organization's batches.
    async fn reset(&self) {
        self.dispatch().await;
        sqlx::query("DELETE FROM gateway.usage_export")
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// Dispatches every due event as the worker does.
    async fn dispatch(&self) {
        let sinks = [
            &WebhookFanout as &dyn gateway::persistence::OutboxSink,
            &UsageExportSink,
        ];
        while self
            .outbox
            .dispatch_batch(&Sinks(&sinks), Utc::now(), 500)
            .await
            .unwrap()
            .dispatched
            > 0
        {}
    }

    async fn queued(&self, org: OrganizationId) -> (i64, i64) {
        sqlx::query_as(
            "SELECT count(*), count(batch_id) FROM gateway.usage_export_record
             WHERE organization_id = $1",
        )
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Makes every batch of `org` due now.
    async fn due_now(&self, org: OrganizationId) {
        sqlx::query(
            "UPDATE gateway.usage_export_batch SET next_attempt_at = now()
             WHERE organization_id = $1",
        )
        .bind(org.0)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn audit_count(&self, org: OrganizationId, action: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM gateway.audit_event
             WHERE organization_id = $1 AND action = $2 AND entity_type = 'usage_export'
               AND outcome = 'succeeded'",
        )
        .bind(org.0)
        .bind(action)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

fn gcs(prefix: &str) -> Value {
    json!({
        "kind": "gcs",
        "connection": "gemini-main",
        "bucket": "acme-usage",
        "prefix": prefix,
    })
}

fn lines(body: &[u8]) -> Vec<UsageRecord> {
    let mut text = String::new();
    GzDecoder::new(body).read_to_string(&mut text).unwrap();
    assert!(text.ends_with('\n'));
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn harness() -> Option<Harness> {
    let url = test_database_url()?;
    Some(Harness::new(&url).await)
}

#[tokio::test]
async fn an_export_is_verified_versioned_and_audited() {
    let _guard = database_guard();
    let Some(h) = harness().await else {
        return;
    };
    let org = h.org().await;
    let owner = h.session(h.user(org, "organization_owner").await).await;
    let operator = h.session(h.user(org, "operator").await).await;
    let auditor = h.session(h.user(org, "auditor").await).await;

    let (status, _, _) = h.send(Method::GET, org, &auditor, &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Refused before anything is written on the operator's behalf.
    let (status, _, _) = h.put(org, &operator, "*", gcs("warehouse")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(h.writer.take().is_empty());
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'usage_export.set'
           AND entity_type = 'usage_export' AND outcome = 'denied'",
    )
    .bind(org.0)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(denied, 1);

    let (status, body, _) = h
        .put(
            org,
            &owner,
            "*",
            json!({"kind": "s3", "connection": "bedrock-main", "bucket": "acme-usage"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_usage_export", "{body}");

    // A destination the gateway cannot write is not saved.
    h.writer.failing(Some(WriteError::Denied));
    let (status, body, _) = h.put(org, &owner, "*", gcs("warehouse")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "destination_unverified");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .ends_with("storage denied the write"),
        "{body}"
    );
    h.writer.failing(None);
    let (status, _, _) = h.send(Method::GET, org, &auditor, &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body, headers) = h.put(org, &owner, "*", gcs("warehouse")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers.get("etag").unwrap(), "\"v1\"");
    assert_eq!(body["kind"], "gcs");
    assert_eq!(body["failing"], false);
    assert_eq!(body["queued_records"], 0);
    let written = h.writer.take();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].name, "warehouse/_gateway-verify.json");
    assert_eq!(written[0].content_type, "application/json");
    assert_eq!(written[0].organization, org);
    let marker: Value = serde_json::from_slice(&written[0].body).unwrap();
    assert_eq!(marker["organization_id"], org.0.to_string());

    let (status, body, headers) = h.send(Method::GET, org, &auditor, &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("etag").unwrap(), "\"v1\"");
    assert_eq!(body["connection"], "gemini-main");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");

    // A stale version conflicts; the current one updates.
    let (status, _, _) = h.put(org, &owner, "\"v7\"", gcs("raw")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body, headers) = h.put(org, &owner, "\"v1\"", gcs("raw")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("etag").unwrap(), "\"v2\"");
    assert_eq!(body["prefix"], "raw");
    assert_eq!(h.writer.take()[0].name, "raw/_gateway-verify.json");

    // The same request again replays the recorded response.
    let key = Uuid::new_v4().to_string();
    let headers = [("idempotency-key", key.as_str()), ("if-match", "*")];
    let (first, first_body, _) = h
        .send(Method::PUT, org, &owner, &headers, Some(gcs("raw")))
        .await;
    let (again, again_body, _) = h
        .send(Method::PUT, org, &owner, &headers, Some(gcs("raw")))
        .await;
    assert_eq!((first, again), (StatusCode::OK, StatusCode::OK));
    assert_eq!(first_body["version"], again_body["version"]);
    assert_eq!(h.audit_count(org, "usage_export.set").await, 3);

    let key = Uuid::new_v4().to_string();
    let (status, _, _) = h
        .send(
            Method::DELETE,
            org,
            &operator,
            &[("idempotency-key", &key), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = h
        .send(
            Method::DELETE,
            org,
            &owner,
            &[("idempotency-key", &key), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.audit_count(org, "usage_export.deleted").await, 1);
    let (status, _, _) = h.send(Method::GET, org, &auditor, &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Set again: a new export that keeps the version history.
    let (status, body, _) = h.put(org, &owner, "*", gcs("warehouse")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["version"], 5);
}

#[tokio::test]
async fn finalized_usage_is_written_once_per_record_by_hour() {
    let _guard = database_guard();
    let Some(h) = harness().await else {
        return;
    };
    h.reset().await;
    let (org, _) = h.exporting_org(Duration::hours(3)).await;
    let quiet = h.org().await;
    let now = Utc::now();
    let hour = now.duration_trunc(Duration::hours(1)).unwrap();
    let earlier = h.finalize(org, hour - Duration::minutes(30)).await;
    // The first was for an app's end user: the record names them.
    let account = Uuid::new_v4();
    let end_user = Uuid::new_v4();
    let owner = h.user(org, "organization_owner").await;
    sqlx::query(
        "INSERT INTO gateway.service_account (id, organization_id, owner_user_id, name)
         VALUES ($1, $2, $3, 'app')",
    )
    .bind(account)
    .bind(org.0)
    .bind(owner.0)
    .execute(&h.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id)
         VALUES ($1, $2, $3, 'customer-7')",
    )
    .bind(end_user)
    .bind(org.0)
    .bind(account)
    .execute(&h.pool)
    .await
    .unwrap();
    let first = h.finalize_for(org, now, Some((account, end_user))).await;
    let second = h.finalize(org, now).await;
    // Before the export was enabled, and in an organization without one.
    h.finalize(org, now - Duration::hours(4)).await;
    h.finalize(quiet, now).await;
    h.dispatch().await;
    assert_eq!(h.queued(org).await, (3, 0));
    assert_eq!(h.queued(quiet).await, (0, 0));

    // Due only once the oldest has waited five minutes.
    let report = h.runner.run_once().await.unwrap();
    assert_eq!((report.sealed_batches, report.claimed), (0, 0));
    sqlx::query(
        "UPDATE gateway.usage_export_record SET queued_at = now() - interval '6 minutes'
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .execute(&h.pool)
    .await
    .unwrap();
    let report = h.runner.run_once().await.unwrap();
    assert_eq!(report.sealed_batches, 2);
    assert_eq!(report.written, 2);
    assert_eq!(report.records_written, 3);
    assert_eq!(h.queued(org).await, (0, 0));

    let mut written = h.writer.take();
    written.sort_by(|a, b| a.name.cmp(&b.name));
    let prefix = format!("warehouse/organization={}/date=", org.0);
    for object in &written {
        assert!(object.name.starts_with(&prefix), "{}", object.name);
        assert!(object.name.ends_with(".ndjson.gz"));
        assert_eq!(object.content_type, "application/gzip");
        assert_eq!(object.destination.bucket, "acme-usage");
    }
    let hour_of = |at: DateTime<Utc>| at.format("/hour=%H/").to_string();
    let earlier_object = written
        .iter()
        .find(|object| object.name.contains(&hour_of(hour - Duration::minutes(30))))
        .unwrap();
    let records = lines(&earlier_object.body);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].trace_id, earlier);
    let current = written
        .iter()
        .find(|object| object.name.contains(&hour_of(now)))
        .unwrap();
    let records = lines(&current.body);
    assert_eq!(
        records
            .iter()
            .map(|record| record.trace_id)
            .collect::<Vec<_>>(),
        [first, second]
    );
    let record = &records[0];
    assert_eq!(record.schema, "usage-export/v1");
    assert_eq!(record.end_user.as_deref(), Some("customer-7"));
    assert_eq!(record.service_account_id, Some(account));
    assert_eq!(records[1].end_user, None);
    assert_eq!(record.organization_id, org.0);
    assert_eq!(record.deployment.as_deref(), Some("deploy-a"));
    assert_eq!(record.model, "model-1");
    assert_eq!(record.operation, "inference");
    assert_eq!(record.outcome, "settled");
    assert_eq!(
        (record.input_tokens, record.output_tokens),
        (Some(120), Some(30))
    );
    assert_eq!(record.usage_confidence.as_deref(), Some("exact"));
    assert_eq!(record.amount_minor, Some(42));
    assert_eq!(record.currency, "USD");
    // `<first>-<last>` are the records' outbox sequences.
    let range = current.name.rsplit('/').next().unwrap();
    let (low, high) = range
        .trim_end_matches(".ndjson.gz")
        .split_once('-')
        .unwrap();
    assert!(low.parse::<i64>().unwrap() < high.parse::<i64>().unwrap());

    let export = h.store.get(org).await.unwrap().unwrap();
    assert!(export.last_success_at.is_some());
    assert_eq!(export.queued_records, 0);
}

#[tokio::test]
async fn failed_writes_retry_the_same_object_and_are_reported_once() {
    let _guard = database_guard();
    let Some(h) = harness().await else {
        return;
    };
    h.reset().await;
    let (org, _) = h.exporting_org(Duration::hours(1)).await;
    h.finalize(org, Utc::now()).await;
    h.dispatch().await;
    h.store.seal(org).await.unwrap();
    let name: String = sqlx::query_scalar(
        "SELECT object_name FROM gateway.usage_export_batch WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&h.pool)
    .await
    .unwrap();

    h.writer.failing(Some(WriteError::Denied));
    let report = h.runner.write_due().await.unwrap();
    assert_eq!((report.claimed, report.failed), (1, 1));
    // Backed off: nothing is due again yet.
    assert_eq!(h.runner.write_due().await.unwrap().claimed, 0);
    let export = h.store.get(org).await.unwrap().unwrap();
    assert_eq!(
        export.last_error.as_deref(),
        Some("storage denied the write")
    );
    assert!(export.failing_since.is_some());
    assert!(!export.failing);
    assert_eq!(export.queued_records, 1);

    // Reported once it has failed for an hour, and only once.
    sqlx::query(
        "UPDATE gateway.usage_export SET failing_since = now() - interval '2 hours'
         WHERE organization_id = $1",
    )
    .bind(org.0)
    .execute(&h.pool)
    .await
    .unwrap();
    assert!(h.store.get(org).await.unwrap().unwrap().failing);
    assert_eq!(h.store.report_failing(Utc::now()).await.unwrap(), 1);
    assert_eq!(h.store.report_failing(Utc::now()).await.unwrap(), 0);
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM gateway.outbox_event
         WHERE organization_id = $1 AND event_type = 'usage_export.failing'",
    )
    .bind(org.0)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(payload["kind"], "gcs");
    assert_eq!(payload["queued_records"], 1);
    assert_eq!(payload["last_error"], "storage denied the write");
    // The failing event itself is not usage.
    h.dispatch().await;
    assert_eq!(h.queued(org).await, (1, 1));

    // The retry writes the object sealed earlier, under the same name; not
    // while the organization is suspended, when its connection is unused.
    h.writer.failing(None);
    h.due_now(org).await;
    let suspend = |suspended: bool| {
        sqlx::query(
            "UPDATE gateway.organization
             SET suspended_at = CASE WHEN $2 THEN now() END WHERE id = $1",
        )
        .bind(org.0)
        .bind(suspended)
        .execute(&h.pool)
    };
    suspend(true).await.unwrap();
    assert_eq!(h.runner.write_due().await.unwrap().claimed, 0);
    assert!(h.writer.take().is_empty());
    suspend(false).await.unwrap();
    assert_eq!(h.runner.write_due().await.unwrap().written, 1);
    let written = h.writer.take();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].name, name);
    let export = h.store.get(org).await.unwrap().unwrap();
    assert_eq!(export.failing_since, None);
    assert_eq!(export.last_error, None);
    assert!(!export.failing);
}

#[tokio::test]
async fn a_new_destination_takes_the_queue_and_deleting_discards_it() {
    let _guard = database_guard();
    let Some(h) = harness().await else {
        return;
    };
    h.reset().await;
    let (org, owner) = h.exporting_org(Duration::hours(1)).await;
    h.finalize(org, Utc::now()).await;
    h.finalize(org, Utc::now()).await;
    h.dispatch().await;
    h.store.seal(org).await.unwrap();
    assert_eq!(h.queued(org).await, (2, 2));

    // Sealed under `warehouse`; unsealed by the change, sealed again under
    // `raw`.
    let (status, _, _) = h.put(org, &owner, "*", gcs("raw")).await;
    assert_eq!(status, StatusCode::OK);
    h.writer.take();
    assert_eq!(h.queued(org).await, (2, 0));
    h.store.seal(org).await.unwrap();
    h.runner.write_due().await.unwrap();
    let written = h.writer.take();
    assert_eq!(written.len(), 1);
    assert!(written[0].name.starts_with("raw/organization="));
    assert_eq!(lines(&written[0].body).len(), 2);

    h.finalize(org, Utc::now()).await;
    h.dispatch().await;
    assert_eq!(h.queued(org).await, (1, 0));
    let key = Uuid::new_v4().to_string();
    let (status, _, _) = h
        .send(
            Method::DELETE,
            org,
            &owner,
            &[("idempotency-key", &key), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.queued(org).await, (0, 0));
    // Usage after the export is gone is not queued.
    h.finalize(org, Utc::now()).await;
    h.dispatch().await;
    assert_eq!(h.queued(org).await, (0, 0));
}

#[tokio::test]
async fn records_past_usage_retention_are_dropped_by_the_hour() {
    let _guard = database_guard();
    let Some(h) = harness().await else {
        return;
    };
    h.reset().await;
    let (org, _) = h.exporting_org(Duration::days(60)).await;
    let now = Utc::now();
    h.finalize(org, now - Duration::days(45)).await;
    h.finalize(org, now - Duration::days(40)).await;
    h.finalize(org, now - Duration::days(10)).await;
    h.dispatch().await;
    h.store.seal(org).await.unwrap();
    assert_eq!(h.queued(org).await, (3, 3));
    // Without a usage retention period nothing is dropped.
    assert_eq!(h.store.sweep_expired(now, 1_000).await.unwrap(), 0);

    sqlx::query(
        "INSERT INTO gateway.organization_retention_policy
             (organization_id, id, trace_days, usage_days)
         VALUES ($1, $2, 30, 30)",
    )
    .bind(org.0)
    .bind(Uuid::new_v4())
    .execute(&h.pool)
    .await
    .unwrap();
    assert_eq!(h.store.sweep_expired(now, 1_000).await.unwrap(), 2);
    assert_eq!(h.queued(org).await, (1, 1));
    let batches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.usage_export_batch WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(batches, 1);
}

#[tokio::test]
async fn usage_is_exported_after_its_trace_is_purged() {
    let _guard = database_guard();
    let Some(h) = harness().await else {
        return;
    };
    h.reset().await;
    let (org, _) = h.exporting_org(Duration::hours(1)).await;
    let trace = h.finalize(org, Utc::now()).await;
    // Trace retention can be shorter than usage retention, so a trace may
    // be gone while its event still waits.
    let mut tx = h.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL gateway.retention_purge = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gateway.inference_trace WHERE id = $1")
        .bind(trace)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h.dispatch().await;
    assert_eq!(h.queued(org).await, (1, 0));
    h.store.seal(org).await.unwrap();
    h.runner.write_due().await.unwrap();
    let written = h.writer.take();
    let records = lines(&written[0].body);
    assert_eq!(records[0].trace_id, trace);
    assert_eq!(records[0].input_tokens, Some(120));
    assert_eq!(records[0].deployment.as_deref(), Some("deploy-a"));
}
