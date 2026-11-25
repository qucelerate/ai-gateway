#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
use std::{
    env,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use gateway::{
    ApiKeyPepper, IdentityStore, OrganizationId,
    domain::outbox::{OutboxPayload, ReconciliationOpened},
    persistence::outbox::{self, OutboxEvent, OutboxSink, OutboxStore, SinkError},
};
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn pool() -> Option<PgPool> {
    let url = env::var("GATEWAY_TEST_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap_or_else(|error| {
            panic!("GATEWAY_TEST_DATABASE_URL is configured but unreachable: {error}")
        });
    gateway::persistence::migrations::run(&pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
    Some(pool)
}

/// Accepts everything, remembering what it saw.
#[derive(Default)]
struct RecordingSink {
    seen: Mutex<Vec<Uuid>>,
    delay: Option<StdDuration>,
}

#[async_trait]
impl OutboxSink for RecordingSink {
    async fn deliver(&self, _: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.seen.lock().unwrap().push(event.id);
        Ok(())
    }
}

/// Writes a marker row, then fails; the write must not survive.
struct FailingSink;

#[async_trait]
impl OutboxSink for FailingSink {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        sqlx::query("INSERT INTO outbox_sink_marker (event_id) VALUES ($1)")
            .bind(event.id)
            .execute(conn)
            .await
            .map_err(|_| SinkError::new("marker insert failed"))?;
        Err(SinkError::new("endpoint returned 503"))
    }
}

/// Writes a marker row and succeeds; the write commits with the dispatch.
struct WritingSink;

#[async_trait]
impl OutboxSink for WritingSink {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        sqlx::query("INSERT INTO outbox_sink_marker (event_id) VALUES ($1)")
            .bind(event.id)
            .execute(conn)
            .await
            .map_err(|_| SinkError::new("marker insert failed"))?;
        Ok(())
    }
}

async fn organization(pool: &PgPool) -> OrganizationId {
    let store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"outbox-test-pepper").unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    store
        .create_organization(&format!("outbox-{suffix}"), "Outbox test")
        .await
        .unwrap()
        .id
}

fn opened() -> OutboxPayload {
    OutboxPayload::ReconciliationOpened(ReconciliationOpened {
        item_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
        reason: "provider-unknown".to_owned(),
        created_at: Utc::now(),
    })
}

async fn emit(pool: &PgPool, organization: OrganizationId) -> Uuid {
    let mut conn = pool.acquire().await.unwrap();
    outbox::emit(&mut conn, organization, &opened())
        .await
        .unwrap()
}

/// Dispatches everything already due so each test starts from its own events.
async fn drain(store: &OutboxStore) {
    let sink = RecordingSink::default();
    while store
        .dispatch_batch(&sink, Utc::now(), 500)
        .await
        .unwrap()
        .dispatched
        > 0
    {}
}

async fn state(pool: &PgPool, id: Uuid) -> (i32, bool, Option<String>) {
    sqlx::query_as(
        "SELECT attempts, dispatched_at IS NOT NULL, last_error
         FROM gateway.outbox_event WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn state_exists(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM gateway.outbox_event WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn marker_table(pool: &PgPool) {
    sqlx::query(
        "CREATE TEMP TABLE IF NOT EXISTS outbox_sink_marker (event_id UUID NOT NULL)
         ON COMMIT PRESERVE ROWS",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn dispatch_delivers_due_events_in_sequence_order_once() {
    let _guard = database_guard();
    let Some(pool) = pool().await else {
        return;
    };
    let store = OutboxStore::new(pool.clone());
    drain(&store).await;
    let org = organization(&pool).await;
    let first = emit(&pool, org).await;
    let second = emit(&pool, org).await;

    let sink = RecordingSink::default();
    let report = store.dispatch_batch(&sink, Utc::now(), 500).await.unwrap();
    assert_eq!(report.dispatched, 2);
    assert_eq!(report.failed, 0);
    assert_eq!(*sink.seen.lock().unwrap(), [first, second]);
    assert_eq!(state(&pool, first).await, (1, true, None));

    let again = RecordingSink::default();
    store.dispatch_batch(&again, Utc::now(), 500).await.unwrap();
    assert!(again.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_delivery_rolls_back_sink_writes_and_backs_off() {
    let _guard = database_guard();
    let Some(pool) = pool().await else {
        return;
    };
    // A single connection so the temporary marker table is visible to the
    // dispatcher and to the assertions.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    marker_table(&pool).await;
    let store = OutboxStore::new(pool.clone());
    drain(&store).await;
    let org = organization(&pool).await;
    let id = emit(&pool, org).await;

    let now = Utc::now();
    let report = store.dispatch_batch(&FailingSink, now, 500).await.unwrap();
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.failed, 1);
    assert_eq!(
        state(&pool, id).await,
        (1, false, Some("endpoint returned 503".to_owned()))
    );
    let markers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_sink_marker WHERE event_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(markers, 0);

    // Not due again until the backoff has passed.
    let early = RecordingSink::default();
    store
        .dispatch_batch(&early, now + Duration::seconds(4), 500)
        .await
        .unwrap();
    assert!(!early.seen.lock().unwrap().contains(&id));

    store
        .dispatch_batch(&WritingSink, now + Duration::seconds(6), 500)
        .await
        .unwrap();
    assert_eq!(state(&pool, id).await, (2, true, None));
    let markers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_sink_marker WHERE event_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(markers, 1);
}

#[tokio::test]
async fn concurrent_dispatchers_never_deliver_an_event_twice() {
    let _guard = database_guard();
    let Some(pool) = pool().await else {
        return;
    };
    let store = OutboxStore::new(pool.clone());
    drain(&store).await;
    let org = organization(&pool).await;
    let mut ids = Vec::new();
    for _ in 0..6 {
        ids.push(emit(&pool, org).await);
    }

    let sink = Arc::new(RecordingSink {
        seen: Mutex::default(),
        delay: Some(StdDuration::from_millis(20)),
    });
    let tasks: Vec<_> = (0..3)
        .map(|_| {
            let store = store.clone();
            let sink = sink.clone();
            tokio::spawn(async move { store.dispatch_batch(&*sink, Utc::now(), 2).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    // Three dispatchers claiming two each cover all six events between them.
    let seen = sink.seen.lock().unwrap().clone();
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(seen.len(), unique.len(), "an event was delivered twice");
    for id in &ids {
        assert!(seen.contains(id), "an event was never delivered");
        assert!(state(&pool, *id).await.1);
    }
}

#[tokio::test]
async fn sweep_removes_only_old_dispatched_events() {
    let _guard = database_guard();
    let Some(pool) = pool().await else {
        return;
    };
    let store = OutboxStore::new(pool.clone());
    drain(&store).await;
    let org = organization(&pool).await;
    let dispatched = emit(&pool, org).await;
    store
        .dispatch_batch(&RecordingSink::default(), Utc::now(), 500)
        .await
        .unwrap();
    let pending = emit(&pool, org).await;

    store
        .sweep_dispatched(Utc::now() - Duration::days(1), 500)
        .await
        .unwrap();
    assert!(state_exists(&pool, dispatched).await);
    // Other suites (the load test above all) leave dispatched events in the
    // shared database; sweep in batches until none is old enough.
    while store
        .sweep_dispatched(Utc::now() + Duration::seconds(1), 500)
        .await
        .unwrap()
        == 500
    {}
    let remaining: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gateway.outbox_event WHERE organization_id = $1 ORDER BY sequence",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, [pending]);
}

#[tokio::test]
async fn event_content_is_immutable_and_dispatch_is_final() {
    let _guard = database_guard();
    let Some(pool) = pool().await else {
        return;
    };
    let store = OutboxStore::new(pool.clone());
    drain(&store).await;
    let org = organization(&pool).await;
    let id = emit(&pool, org).await;

    let rewrite = sqlx::query("UPDATE gateway.outbox_event SET payload = '{}' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await;
    assert!(rewrite.is_err());

    store
        .dispatch_batch(&RecordingSink::default(), Utc::now(), 500)
        .await
        .unwrap();
    let redispatch =
        sqlx::query("UPDATE gateway.outbox_event SET dispatched_at = NULL WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await;
    assert!(redispatch.is_err());
}
