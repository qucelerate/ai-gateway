#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Configuration versions and their feed (migration 0062): publication by
//! the catalog triggers, what an organization serves with and without a pin,
//! and a process reloading when the entry it applied is lost.
//!
//! Deleting feed entries, as a restore would, disturbs every process loaded
//! from the shared database, so this lives in its own test binary and its
//! tests run one at a time.

use std::sync::{Mutex, MutexGuard, OnceLock};

use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, application::config_cache::ConfigCache,
    config::InstanceConfig, persistence::config_versions::ConfigStore,
};
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

async fn pool() -> Option<PgPool> {
    let url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => return None,
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    Some(pool)
}

async fn organization(pool: &PgPool) -> OrganizationId {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id, slug, name) VALUES ($1, $2, 'Versions')")
        .bind(id)
        .bind(format!("versions-{}", id.simple()))
        .execute(pool)
        .await
        .unwrap();
    OrganizationId(id)
}

fn manifest(version: &str, alias: &str) -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": version},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "m", "location": null,
                         "adapter_instance_key": "openai-main",
                         "capabilities": [{
                             "operation": "chat", "max_input_bytes": 4096,
                             "max_output_tokens": 64, "max_tokens_per_byte": 1,
                             "tokenizer": "configured", "reservation_ttl_seconds": 60,
                             "chat_input_count_mode": "unsupported"}]}],
        "prices": [{"deployment": "chat-a", "operation": "chat", "input_minor_per_token": 3,
                    "output_minor_per_token": 11, "cached_input_minor_per_token": 0,
                    "reasoning_minor_per_token": 0, "other_minor_per_token": 0,
                    "currency": "USD", "source": "test", "source_version": "v1",
                    "confidence": "known"}],
        "routes": [{"alias": alias, "operation": "chat", "deployment": "chat-a"}]
    }))
    .unwrap()
}

async fn apply(pool: &PgPool, org: OrganizationId, version: &str, alias: &str) {
    let manifest = manifest(version, alias);
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
}

/// The organization's versions, oldest first.
async fn versions(pool: &PgPool, org: OrganizationId) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT version_id FROM gateway.config_feed
         WHERE organization_id = $1 AND kind = 'version' ORDER BY sequence",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn feed(pool: &PgPool, org: OrganizationId, kind: &str, version: Option<Uuid>) {
    // As a publisher does: the feed lock, held until commit.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('config:feed', 0))")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.config_feed (entry_id, organization_id, kind, version_id)
         VALUES (gen_random_uuid(), $1, $2, $3)",
    )
    .bind(org.0)
    .bind(kind)
    .bind(version)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn cache(pool: &PgPool) -> ConfigCache {
    ConfigCache::load(
        ConfigStore::new(pool.clone()),
        InstanceConfig {
            id: format!("versions-test-{}", Uuid::new_v4().simple()),
            ..InstanceConfig::default()
        },
        "api",
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn publishing_and_pins_decide_what_is_served() {
    let Some(pool) = pool().await else {
        return;
    };
    let _guard = database_guard();
    let org = organization(&pool).await;
    apply(&pool, org, "v1", "one").await;
    // Re-applying the active release publishes nothing new.
    apply(&pool, org, "v1", "one").await;
    apply(&pool, org, "v2", "two").await;
    let published = versions(&pool, org).await;
    assert_eq!(published.len(), 2);

    // Re-activating the first release publishes a third version with the
    // first one's document.
    apply(&pool, org, "v1", "one").await;
    let published = versions(&pool, org).await;
    assert_eq!(published.len(), 3);
    let digests: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT digest FROM gateway.config_version WHERE id = ANY($1) ORDER BY created_at",
    )
    .bind(&published)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(digests[0], digests[2]);
    assert_ne!(digests[0], digests[1]);

    // Versions and feed entries never change.
    let error = sqlx::query("UPDATE gateway.config_version SET release_id = NULL WHERE id = $1")
        .bind(published[0])
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("immutable"), "{error}");

    let cache = cache(&pool).await;
    assert_eq!(cache.served_version(org), Some(published[2]));

    // A pin holds the organization on its version; versions published while
    // pinned are not served; unpinning serves the newest.
    feed(&pool, org, "pin", Some(published[1])).await;
    apply(&pool, org, "v3", "three").await;
    cache.refresh().await.unwrap();
    assert_eq!(cache.served_version(org), Some(published[1]));
    feed(&pool, org, "unpin", None).await;
    cache.refresh().await.unwrap();
    let newest = *versions(&pool, org).await.last().unwrap();
    assert_eq!(cache.served_version(org), Some(newest));
    assert_eq!(cache.applied().unwrap().sequence, {
        let head: i64 = sqlx::query_scalar("SELECT max(sequence) FROM gateway.config_feed")
            .fetch_one(&pool)
            .await
            .unwrap();
        head
    });
}

#[tokio::test]
async fn deactivating_a_release_stops_its_routes() {
    let Some(pool) = pool().await else {
        return;
    };
    let _guard = database_guard();
    let org = organization(&pool).await;
    apply(&pool, org, "v1", "one").await;
    let store = ConfigStore::new(pool.clone());
    let served = |version: Option<gateway::persistence::config_versions::LoadedVersion>| {
        version.map(|version| version.document.routes.len())
    };
    assert_eq!(served(store.served_now(org).await.unwrap()), Some(1));
    sqlx::query("DELETE FROM gateway.catalog_active_release WHERE organization_id = $1")
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(served(store.served_now(org).await.unwrap()), Some(0));

    // Deleting the organization takes its versions with it, publishing
    // nothing for an organization that is gone.
    apply(&pool, org, "v2", "two").await;
    sqlx::query("DELETE FROM gateway.organization WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.config_feed WHERE organization_id = $1")
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn a_lost_entry_makes_a_process_reload() {
    let Some(pool) = pool().await else {
        return;
    };
    let _guard = database_guard();
    let org = organization(&pool).await;
    apply(&pool, org, "v1", "one").await;
    let cache = cache(&pool).await;
    let before = gateway::metrics::global()
        .value(&gateway::metrics::CONFIG_RELOADS, &["entry_lost"])
        .unwrap_or(0);

    // A restore loses the newest publication: v2's entry is gone, and a
    // later publication (v3) passes its position.
    apply(&pool, org, "v2", "two").await;
    cache.refresh().await.unwrap();
    let lost = cache.applied().unwrap();
    sqlx::query("DELETE FROM gateway.config_feed WHERE sequence = $1")
        .bind(lost.sequence)
        .execute(&pool)
        .await
        .unwrap();
    apply(&pool, org, "v3", "three").await;
    cache.refresh().await.unwrap();
    let after = gateway::metrics::global()
        .value(&gateway::metrics::CONFIG_RELOADS, &["entry_lost"])
        .unwrap_or(0);
    assert_eq!(after, before + 1);
    assert_eq!(
        cache.served_version(org),
        versions(&pool, org).await.last().copied()
    );
    assert!(cache.applied().unwrap().sequence > lost.sequence);
}

/// A capability's reasoning allowance reaches the served document (and so
/// data planes), and is left out of it when zero, so documents for catalogs
/// without one are unchanged.
#[tokio::test]
async fn a_reasoning_allowance_is_served_only_when_set() {
    let Some(pool) = pool().await else {
        return;
    };
    let _guard = database_guard();
    let org = organization(&pool).await;
    apply(&pool, org, "v1", "one").await;
    let raw = || async {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT v.document FROM gateway.config_feed f
             JOIN gateway.config_version v
               ON v.organization_id = f.organization_id AND v.id = f.version_id
             WHERE f.organization_id = $1 AND f.kind = 'version'
             ORDER BY f.sequence DESC LIMIT 1",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let document = raw().await;
    assert!(
        document["routes"][0]["capability"]
            .get("reasoning_allowance_tokens")
            .is_none()
    );

    let mut manifest = manifest("v2", "one");
    manifest.deployments[0].capabilities[0].reasoning_allowance_tokens = 4096;
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    let document = raw().await;
    assert_eq!(
        document["routes"][0]["capability"]["reasoning_allowance_tokens"],
        4096
    );
    let served = ConfigStore::new(pool.clone())
        .served_now(org)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        served.document.routes[0]
            .capability
            .reasoning_allowance_tokens,
        4096
    );
}
