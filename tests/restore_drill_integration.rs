#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! The backup/restore drill, end to end: seed the test database through the
//! gateway's own stores, run `scripts/restore-drill.sh` into an empty second
//! database, then check that the restored copy works: migrations are a
//! no-op, an API key issued before the backup authenticates, a stored
//! credential opens with the same key, usage totals match, and new rows
//! take fresh sequence values.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL` and `GATEWAY_RESTORE_TEST_DATABASE_URL`
//! (an empty `gateway` schema owned by the same role), and `pg_dump`,
//! `pg_restore` and `psql` on the PATH. Skipped locally when either URL is
//! absent; required in CI.

use std::{
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};

use chrono::Utc;
use gateway::{
    ApiKeyPepper, IdentityStore,
    domain::{
        catalog::ProviderFamily,
        credentials::{CredentialBinding, CredentialCipher, ProviderSecret, ProviderSecretInput},
        identity::{ApiKeyScope, MembershipRole},
        outbox::{OutboxPayload, ReconciliationOpened},
    },
    persistence::outbox,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;
use zeroize::Zeroizing;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const PEPPER: &[u8] = b"restore-drill-pepper";

fn urls() -> Option<(String, String)> {
    let in_ci = std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some();
    match (
        std::env::var("GATEWAY_TEST_DATABASE_URL"),
        std::env::var("GATEWAY_RESTORE_TEST_DATABASE_URL"),
    ) {
        (Ok(source), Ok(target)) => Some((source, target)),
        _ if in_ci => panic!(
            "GATEWAY_TEST_DATABASE_URL and GATEWAY_RESTORE_TEST_DATABASE_URL must be set in CI"
        ),
        _ => None,
    }
}

async fn pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .unwrap_or_else(|error| panic!("test database is unreachable: {error}"))
}

/// Empties the target's `gateway` schema, which the application role owns
/// but cannot recreate.
async fn empty_schema(pool: &PgPool) {
    sqlx::query(
        "DO $$
         DECLARE
             statements text[];
             statement text;
         BEGIN
             -- Names are taken before anything is dropped.
             SELECT array_agg(sql) INTO statements FROM (
                 SELECT format('DROP VIEW IF EXISTS gateway.%I CASCADE', viewname) AS sql
                 FROM pg_views WHERE schemaname = 'gateway'
                 UNION ALL SELECT format('DROP TABLE IF EXISTS gateway.%I CASCADE', tablename)
                 FROM pg_tables WHERE schemaname = 'gateway'
                 UNION ALL SELECT format('DROP SEQUENCE IF EXISTS gateway.%I CASCADE', sequencename)
                 FROM pg_sequences WHERE schemaname = 'gateway'
                 UNION ALL SELECT format('DROP ROUTINE IF EXISTS %s CASCADE', p.oid::regprocedure::text)
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                 WHERE n.nspname = 'gateway'
                 UNION ALL SELECT format('DROP TYPE IF EXISTS gateway.%I CASCADE', t.typname)
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                 WHERE n.nspname = 'gateway' AND t.typtype IN ('e', 'd', 'c')
                   AND NOT EXISTS (SELECT 1 FROM pg_class c WHERE c.reltype = t.oid)
             ) objects;
             FOREACH statement IN ARRAY coalesce(statements, '{}') LOOP
                 EXECUTE statement;
             END LOOP;
         END $$",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn drill(source: &str, target: &str) -> std::process::Output {
    Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/restore-drill.sh"
        ))
        .arg(source)
        .arg(target)
        .output()
        .expect("sh runs the drill script")
}

async fn usage_total(pool: &PgPool, org: Uuid) -> (i64, i64) {
    sqlx::query_as(
        "SELECT count(*), coalesce(sum(amount_minor), 0)::BIGINT FROM gateway.accounting_usage
         WHERE organization_id = $1",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_restored_backup_serves_the_data_it_was_taken_from() {
    let _guard = database_guard();
    let Some((source_url, target_url)) = urls() else {
        return;
    };
    let source = pool(&source_url).await;
    gateway::persistence::migrations::run(&source)
        .await
        .unwrap();
    let target = pool(&target_url).await;
    empty_schema(&target).await;

    // Seed: an organization with a service account key, a stored provider
    // credential, a settled request with usage, and a pending outbox event.
    let identity = IdentityStore::new(source.clone(), ApiKeyPepper::from_bytes(PEPPER).unwrap());
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("restore-{suffix}"), "Restore drill")
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
        .create_service_account(org, user, None, "drill")
        .await
        .unwrap()
        .id;
    let key = identity
        .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
        .await
        .unwrap();
    let cipher = CredentialCipher::new(&[23_u8; 32]).unwrap();
    let binding = CredentialBinding {
        organization_id: org,
        adapter_instance_key: "openai-main",
        family: ProviderFamily::OpenAi,
    };
    let secret = ProviderSecret::for_family(
        ProviderFamily::OpenAi,
        ProviderSecretInput {
            api_key: Some(Zeroizing::new("sk-restore".to_owned())),
            ..ProviderSecretInput::default()
        },
    )
    .unwrap();
    let sealed = cipher.seal(binding, &secret).unwrap();
    sqlx::query(
        "INSERT INTO gateway.provider_credential
             (id, organization_id, adapter_instance_key, family, key_id, ciphertext)
         VALUES ($1, $2, 'openai-main', 'openai', $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(&sealed.key_id)
    .bind(&sealed.ciphertext)
    .execute(&source)
    .await
    .unwrap();
    let reservation = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.accounting_reservation
             (id, organization_id, idempotency_key, fingerprint, state, reserved_minor,
              expires_at, output_token_cap, input_price_minor, output_price_minor,
              cached_input_price_minor, reasoning_price_minor, other_price_minor,
              price_version, pricing_confidence, currency, deployment, model, trace_id)
         VALUES ($1, $2, 'drill', $3, 'settled', 500, now(), 100, 1, 1, 1, 1, 1, 'v1',
                 'known', 'USD', 'deploy-a', 'model-a', $4)",
    )
    .bind(reservation)
    .bind(org.0)
    .bind(vec![7_u8; 32])
    .bind(Uuid::new_v4())
    .execute(&source)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.accounting_usage
             (id, reservation_id, organization_id, currency, confidence, input_tokens,
              output_tokens, amount_minor, completeness, finality)
         VALUES ($1, $2, $3, 'USD', 'exact', 10, 20, 4321, 'complete', 'final')",
    )
    .bind(Uuid::new_v4())
    .bind(reservation)
    .bind(org.0)
    .execute(&source)
    .await
    .unwrap();
    let event = move || {
        OutboxPayload::ReconciliationOpened(ReconciliationOpened {
            item_id: Uuid::new_v4(),
            reservation_id: reservation,
            reason: "provider-unknown".to_owned(),
            created_at: Utc::now(),
        })
    };
    let mut conn = source.acquire().await.unwrap();
    let seeded_event = outbox::emit(&mut conn, org, &event()).await.unwrap();
    drop(conn);
    let source_sequence: i64 = sqlx::query_scalar("SELECT max(sequence) FROM gateway.outbox_event")
        .fetch_one(&source)
        .await
        .unwrap();

    // The drill itself, while the source keeps taking writes, as a live
    // gateway would. Audit events have no consumer, so nothing else in the
    // database reacts to them.
    let (from, to) = (source_url.clone(), target_url.clone());
    // The ids of the synthetic events, so cleanup removes only those.
    let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Uuid>::new()));
    // Stops the writer between inserts, never during one, so every row it
    // committed is recorded.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writes = {
        let (source, written, stop) = (source.clone(), written.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO gateway.audit_event
                         (id, organization_id, actor_type, action, entity_type, entity_id,
                          metadata)
                     VALUES ($1, $2, 'system', 'api_key.created', 'api_key',
                             $3, jsonb_build_object('api_key_id', $3::text))",
                )
                .bind(id)
                .bind(org.0)
                .bind(Uuid::new_v4())
                .execute(&source)
                .await
                .expect("the source takes writes during the drill");
                written.lock().unwrap().push(id);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
    };
    let output = tokio::task::spawn_blocking(move || drill(&from, &to))
        .await
        .unwrap();
    assert!(!writes.is_finished(), "the writer kept running");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writes.await.expect("the writer stops cleanly");
    let synthetic = written.lock().unwrap().clone();
    assert!(
        !synthetic.is_empty(),
        "the source took writes during the drill"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "drill failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("restore drill passed"), "{stdout}");
    assert!(!stdout.contains("sk-restore") && !stderr.contains("sk-restore"));

    // The restored copy works.
    let migrations = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gateway._sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = migrations(target.clone()).await;
    gateway::persistence::migrations::run(&target)
        .await
        .unwrap();
    assert_eq!(
        migrations(target.clone()).await,
        before,
        "nothing left to migrate"
    );
    let restored = IdentityStore::new(target.clone(), ApiKeyPepper::from_bytes(PEPPER).unwrap());
    let verified = restored
        .authenticate_bearer_key(&key.bearer)
        .await
        .unwrap()
        .expect("a key issued before the backup authenticates");
    assert_eq!(verified.organization_id, org);
    let (key_id, ciphertext): (String, Vec<u8>) = sqlx::query_as(
        "SELECT key_id, ciphertext FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(org.0)
    .fetch_one(&target)
    .await
    .unwrap();
    let opened = cipher.open(binding, &key_id, &ciphertext).unwrap();
    assert!(
        matches!(opened, ProviderSecret::ApiKey { ref api_key } if api_key.as_str() == "sk-restore")
    );
    assert!(
        CredentialCipher::new(&[24_u8; 32])
            .unwrap()
            .open(binding, &key_id, &ciphertext)
            .is_err(),
        "secrets open only with the source's key"
    );
    assert_eq!(usage_total(&target, org.0).await, (1, 4321));
    assert_eq!(
        usage_total(&target, org.0).await,
        usage_total(&source, org.0).await
    );
    let pending: bool =
        sqlx::query_scalar("SELECT dispatched_at IS NULL FROM gateway.outbox_event WHERE id = $1")
            .bind(seeded_event)
            .fetch_one(&target)
            .await
            .unwrap();
    assert!(pending, "undelivered events survive the restore");
    let mut conn = target.acquire().await.unwrap();
    let new_event = outbox::emit(&mut conn, org, &event()).await.unwrap();
    drop(conn);
    let new_sequence: i64 =
        sqlx::query_scalar("SELECT sequence FROM gateway.outbox_event WHERE id = $1")
            .bind(new_event)
            .fetch_one(&target)
            .await
            .unwrap();
    assert!(
        new_sequence > source_sequence,
        "sequences continue after a restore"
    );

    // A second drill into the same target is refused rather than merged.
    let (from, to) = (source_url.clone(), target_url.clone());
    let output = tokio::task::spawn_blocking(move || drill(&from, &to))
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to restore"));

    empty_schema(&target).await;
    // Remove only the synthetic audit records from the shared database,
    // through the one path the audit trigger allows. The events of real
    // actions, such as the seeded key's creation, stay with the rows they
    // describe.
    let mut tx = source.begin().await.unwrap();
    sqlx::query("SET LOCAL gateway.retention_purge = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let removed = sqlx::query("DELETE FROM gateway.audit_event WHERE id = ANY($1)")
        .bind(&synthetic)
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();
    assert_eq!(removed, synthetic.len() as u64);
    let key_created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND action = 'api_key.created' AND entity_id = $2",
    )
    .bind(org.0)
    .bind(key.id.0)
    .fetch_one(&source)
    .await
    .unwrap();
    assert_eq!(key_created, 1, "the seeded key keeps its audit history");
}
