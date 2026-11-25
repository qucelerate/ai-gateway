//! Integration tests for operational metrics: requests through the real
//! router are counted by route template and inference outcome without
//! identifiers, the worker's backlog sample reads the database, and the
//! metrics router serves the Prometheus text format.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header::CONTENT_TYPE},
};
use chrono::Utc;
use gateway::{
    ApiKeyPepper, AppConfig, IdentityStore,
    domain::outbox::{OutboxPayload, ReconciliationOpened},
    metrics::{
        HTTP_REQUESTS, INFERENCE_REQUESTS, OUTBOX_PENDING, RECONCILIATION_OPEN, WORKER_RUNS,
        global as metrics,
    },
    persistence::{backlog::BacklogStore, outbox},
};
use http_body_util::BodyExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

const PEPPER: &str = "metrics-integration-pepper-0000000000000000";

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

async fn pool(url: &str) -> PgPool {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .unwrap_or_else(|error| panic!("GATEWAY_TEST_DATABASE_URL is unreachable: {error}"));
    gateway::persistence::migrations::run(&pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
    pool
}

async fn send(router: &Router, method: Method, uri: &str, auth: Option<&str>) -> StatusCode {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        request = request.header("authorization", auth);
    }
    request = request
        .header(CONTENT_TYPE, "application/json")
        .header("idempotency-key", "metrics-test");
    router
        .clone()
        .oneshot(request.body(Body::from("{}")).unwrap())
        .await
        .unwrap()
        .status()
}

fn count(family: &'static gateway::metrics::Family, labels: &[&str]) -> i64 {
    metrics().value(family, labels).unwrap_or(0)
}

async fn scrape() -> (StatusCode, String, String) {
    let response = gateway::metrics::router()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response.headers()[CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

#[tokio::test]
async fn requests_are_counted_by_route_template_and_outcome() {
    let Some(url) = test_database_url() else {
        return;
    };
    // The worker profile reads no provider settings from the ambient
    // environment; the API state built from it is the same.
    let config = AppConfig::from_values(
        Some(&url),
        Some("127.0.0.1:0"),
        Some("worker"),
        Some(PEPPER),
    )
    .unwrap();
    let state = gateway::application::connect(&config).await.unwrap();
    let router = gateway::transport::router(state);
    let chat = "/api/inference/v1/chat/completions";

    let missing = count(&INFERENCE_REQUESTS, &["chat", "authentication_required"]);
    let invalid = count(&INFERENCE_REQUESTS, &["chat", "invalid_api_key"]);
    let unauthorized = count(&HTTP_REQUESTS, &[chat, "POST", "4xx"]);
    assert_eq!(
        send(&router, Method::POST, chat, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&router, Method::POST, chat, Some("Bearer not-a-key")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        count(&INFERENCE_REQUESTS, &["chat", "authentication_required"]),
        missing + 1
    );
    assert_eq!(
        count(&INFERENCE_REQUESTS, &["chat", "invalid_api_key"]),
        invalid + 1
    );
    assert_eq!(
        count(&HTTP_REQUESTS, &[chat, "POST", "4xx"]),
        unauthorized + 2
    );

    // Paths with identifiers are counted under their template.
    let org = Uuid::new_v4();
    let template = "/api/admin/v1/organizations/{org}/catalog/price-sources";
    let before = count(&HTTP_REQUESTS, &[template, "GET", "4xx"]);
    let status = send(
        &router,
        Method::GET,
        &format!("/api/admin/v1/organizations/{org}/catalog/price-sources"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(count(&HTTP_REQUESTS, &[template, "GET", "4xx"]), before + 1);
    let before = count(&HTTP_REQUESTS, &["unmatched", "GET", "4xx"]);
    let status = send(&router, Method::GET, &format!("/probe/{org}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        count(&HTTP_REQUESTS, &["unmatched", "GET", "4xx"]),
        before + 1
    );

    let (status, content_type, text) = scrape().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");
    assert!(text.contains(
        "gateway_inference_requests_total{operation=\"chat\",outcome=\"invalid_api_key\"}"
    ));
    assert!(text.contains("gateway_db_pool_connections{pool=\"api\",state=\"max\"} 10\n"));
    assert!(!text.contains(&org.to_string()), "identifiers never appear");
    assert!(!text.contains("not-a-key"));
}

#[tokio::test]
async fn the_worker_samples_backlogs_from_the_database() {
    let Some(url) = test_database_url() else {
        return;
    };
    let pool = pool(&url).await;
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"metrics-test-pepper").unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("metrics-{suffix}"), "Metrics")
        .await
        .unwrap()
        .id;
    let mut conn = pool.acquire().await.unwrap();
    let payload = OutboxPayload::ReconciliationOpened(ReconciliationOpened {
        item_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
        reason: "provider-unknown".to_owned(),
        created_at: Utc::now(),
    });
    let event = outbox::emit(&mut conn, org, &payload).await.unwrap();
    drop(conn);

    let backlog = BacklogStore::new(pool.clone());
    let sample = backlog.sample().await.unwrap();
    assert!(sample.outbox_pending >= 1, "{sample:?}");
    assert!(sample.outbox_oldest_seconds >= 0);
    assert!(sample.reconciliation_open >= 0);

    let runs = count(&WORKER_RUNS, &["backlog_sample", "ok"]);
    gateway::jobs::sample_backlog(&backlog).await;
    assert_eq!(count(&WORKER_RUNS, &["backlog_sample", "ok"]), runs + 1);
    assert!(count(&OUTBOX_PENDING, &[]) >= 1);
    assert!(metrics().value(&RECONCILIATION_OPEN, &[]).is_some());

    // A failed sample keeps the last values and counts an error.
    pool.close().await;
    let errors = count(&WORKER_RUNS, &["backlog_sample", "error"]);
    let pending = count(&OUTBOX_PENDING, &[]);
    gateway::jobs::sample_backlog(&backlog).await;
    assert_eq!(
        count(&WORKER_RUNS, &["backlog_sample", "error"]),
        errors + 1
    );
    assert_eq!(count(&OUTBOX_PENDING, &[]), pending);

    // Leave nothing pending for other suites.
    let pool = self::pool(&url).await;
    sqlx::query("DELETE FROM gateway.outbox_event WHERE id = $1")
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
}
