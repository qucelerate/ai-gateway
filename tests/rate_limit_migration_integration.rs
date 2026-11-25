//! The rate-limit levels migrations (0150 to 0156) against traffic that is
//! running while they apply. A settlement updates its reservation and its
//! counting trigger then reads the rate-limit policies; a first version of
//! the change altered both tables in one transaction and deadlocked against
//! it. Each migration now strongly locks one busy table, and gives up after
//! `lock_timeout` to be tried again.

use std::{borrow::Cow, env, str::FromStr, time::Duration};

use sqlx::{
    Connection, Executor, PgConnection, PgPool,
    migrate::Migrator,
    postgres::{PgConnectOptions, PgPoolOptions},
};

static ALL: Migrator = sqlx::migrate!("./migrations");
/// The first of the rate-limit levels migrations.
const FIRST: i64 = 150;

/// A database of its own (CI provisions `gateway_migration`), since these
/// tests rebuild the `gateway` schema from scratch; the test role may not
/// create databases, but needs `CREATE` on this one to recreate the schema.
fn database_url() -> Option<String> {
    env::var("GATEWAY_MIGRATION_TEST_DATABASE_URL").ok()
}

/// The two tests share that database, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The database with an empty `gateway` schema and every migration before
/// 0150 applied, as a gateway from before the change left it.
async fn database_before(url: &str) -> PgConnectOptions {
    let options = PgConnectOptions::from_str(url).unwrap();
    let mut connection = PgConnection::connect_with(&options).await.unwrap();
    connection
        .execute("DROP SCHEMA IF EXISTS gateway CASCADE")
        .await
        .unwrap();
    connection
        .execute("CREATE SCHEMA gateway AUTHORIZATION CURRENT_USER")
        .await
        .unwrap();
    connection
        .execute("SET search_path TO gateway, public")
        .await
        .unwrap();
    let before = Migrator {
        migrations: Cow::Owned(
            ALL.migrations
                .iter()
                .filter(|migration| migration.version < FIRST)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    options
}

async fn pool(options: &PgConnectOptions) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap()
}

/// Waits until a session waits for a lock on `table`.
async fn someone_waits_on(connection: &mut PgConnection, table: &str) {
    for _ in 0..200 {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_locks
                            WHERE NOT granted AND relation = $1::regclass)",
        )
        .bind(table)
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("nobody came to wait on {table}");
}

/// What a settlement takes, in its order: the reservation's table for its
/// update, then (in the counting trigger) the policies' for reading them.
async fn settlement_starts(options: &PgConnectOptions) -> PgConnection {
    let mut settlement = PgConnection::connect_with(options).await.unwrap();
    settlement.execute("BEGIN").await.unwrap();
    settlement
        .execute("LOCK TABLE gateway.accounting_reservation IN ROW EXCLUSIVE MODE")
        .await
        .unwrap();
    settlement
}

async fn settlement_reads_policies(settlement: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT count(*) FROM gateway.rate_limit_policy")
        .execute(&mut *settlement)
        .await
        .map(|_| ())
}

async fn drop_schema(options: &PgConnectOptions) {
    let mut connection = PgConnection::connect_with(options).await.unwrap();
    connection
        .execute("DROP SCHEMA IF EXISTS gateway CASCADE")
        .await
        .unwrap();
}

/// The interleaving that deadlocked: a settlement holds the reservations'
/// table, the migration starts, then the settlement's trigger reads the
/// policies. Taking both tables in one transaction, as the first version
/// did, deadlocks (shown first, as a control); the migrations as split do
/// not: the settlement goes on, commits, and the migrations complete.
#[tokio::test]
async fn the_migrations_do_not_deadlock_with_a_settlement_under_way() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let options = database_before(&url).await;

    // Control: both tables in one transaction, policies first.
    let mut settlement = settlement_starts(&options).await;
    let combined = tokio::spawn({
        let options = options.clone();
        async move {
            let mut migration = PgConnection::connect_with(&options).await.unwrap();
            let mut tx = migration.begin().await.unwrap();
            tx.execute("LOCK TABLE gateway.rate_limit_policy IN ACCESS EXCLUSIVE MODE")
                .await?;
            tx.execute("LOCK TABLE gateway.accounting_reservation IN ACCESS EXCLUSIVE MODE")
                .await?;
            tx.commit().await
        }
    });
    let mut watcher = PgConnection::connect_with(&options).await.unwrap();
    someone_waits_on(&mut watcher, "gateway.accounting_reservation").await;
    let read = settlement_reads_policies(&mut settlement).await;
    // Ends the settlement either way, so the other side can finish.
    let _ = settlement.execute("ROLLBACK").await;
    let migrated = combined.await.unwrap();
    let deadlocked = |result: &Result<(), sqlx::Error>| {
        matches!(result, Err(sqlx::Error::Database(error))
            if error.code().as_deref() == Some("40P01"))
    };
    assert!(
        deadlocked(&read) || deadlocked(&migrated),
        "the control did not deadlock: {read:?}, {migrated:?}"
    );

    // The migrations as they are.
    let mut settlement = settlement_starts(&options).await;
    let pool = pool(&options).await;
    let migrations = tokio::spawn({
        let pool = pool.clone();
        async move { gateway::persistence::migrations::run(&pool).await }
    });
    someone_waits_on(&mut watcher, "gateway.accounting_reservation").await;
    tokio::time::timeout(
        Duration::from_secs(2),
        settlement_reads_policies(&mut settlement),
    )
    .await
    .expect("the settlement waited on the migration")
    .expect("the settlement failed");
    settlement.execute("COMMIT").await.unwrap();
    migrations.await.unwrap().unwrap();
    let applied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway._sqlx_migrations WHERE version >= $1 AND success",
    )
    .bind(FIRST)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        applied,
        ALL.migrations
            .iter()
            .filter(|migration| migration.version >= FIRST)
            .count() as i64
    );
    pool.close().await;
    drop(watcher);
    drop(settlement);
    drop_schema(&options).await;
}

/// A migration that cannot take its lock within `lock_timeout` rolls back
/// and is tried again, so a long transaction on the table delays the
/// migrations instead of queueing every request behind them for its whole
/// length, and the run still completes once the table is free.
#[tokio::test]
async fn a_migration_that_times_out_on_a_lock_is_tried_again() {
    let Some(url) = database_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let options = database_before(&url).await;
    let mut settlement = settlement_starts(&options).await;
    let pool = pool(&options).await;
    let started = std::time::Instant::now();
    let migrations = tokio::spawn({
        let pool = pool.clone();
        async move { gateway::persistence::migrations::run(&pool).await }
    });
    let mut watcher = PgConnection::connect_with(&options).await.unwrap();
    someone_waits_on(&mut watcher, "gateway.accounting_reservation").await;
    // Past the first attempt's timeout: it gave up, and nothing waits now
    // until the retry.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let waiting: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gateway._sqlx_migrations WHERE version = $1)",
    )
    .bind(FIRST)
    .fetch_one(&mut watcher)
    .await
    .unwrap();
    assert!(!waiting, "0150 applied while its table was held");
    // Another request's reservation is not held up for long meanwhile: at
    // most until the waiting attempt times out.
    let mut other = PgConnection::connect_with(&options).await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        other.execute("BEGIN").await?;
        other
            .execute("LOCK TABLE gateway.accounting_reservation IN ROW EXCLUSIVE MODE")
            .await?;
        other.execute("COMMIT").await
    })
    .await
    .expect("a request waited behind the migration past its lock_timeout")
    .unwrap();
    drop(other);
    settlement.execute("COMMIT").await.unwrap();
    migrations.await.unwrap().unwrap();
    assert!(started.elapsed() >= Duration::from_secs(3));
    // The migrator's own lock is not left behind by the failed attempt.
    let held: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(&mut watcher)
    .await
    .unwrap();
    assert_eq!(held, 0);
    pool.close().await;
    drop(watcher);
    drop(settlement);
    drop_schema(&options).await;
}
