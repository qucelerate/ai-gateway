use sqlx::{PgConnection, PgPool, migrate::MigrateError};
use thiserror::Error;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
const FOUNDATION_MIGRATION_VERSION: i64 = 1;
const IDENTITY_FOUNDATION_MIGRATION_VERSION: i64 = 2;
const IDENTITY_FOUNDATION_RELATIONS: &[&str] = &[
    "gateway.organization",
    "gateway.user_account",
    "gateway.team",
    "gateway.organization_membership",
    "gateway.team_membership",
    "gateway.service_account",
    "gateway.api_key",
    "gateway.audit_event",
];
/// Migrations whose file version changed after they may already have been
/// applied, as `(previous_version, current_version)`. The admin-access
/// migrations were merged as 0023/0024 alongside the Azure provider-family
/// migrations with the same versions; they now live at 0026/0027 unchanged.
pub(crate) const RENUMBERED_MIGRATIONS: &[(i64, i64)] = &[(23, 26), (24, 27)];
/// Session advisory lock serializing whole migration runs. sqlx takes its own
/// lock only inside `Migrator::run`, after the baseline and repair steps here
/// have already touched `_sqlx_migrations`.
const MIGRATION_LOCK_KEY: i64 = 0x6761_7465_7761_7900; // "gateway\0"
/// How often a process waiting for another's migration run tries the lock.
const MIGRATION_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// A migration on a busy table sets a short `lock_timeout` (0150 to 0156),
/// so it gives up rather than queue traffic behind the lock it waits for;
/// it then rolls back whole and is tried again this many times, this far
/// apart, before the run fails (and the next start tries again).
const MIGRATION_LOCK_ATTEMPTS: u32 = 5;
const MIGRATION_LOCK_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// The role `data-plane` processes connect as, when the operator created it.
pub const DATA_PLANE_ROLE: &str = "gateway_data_plane";
/// What a data plane may read: identities and keys, the policies that limit
/// them, provider credentials and their keys, and configuration versions.
/// Nothing from the admin API, sessions, audit or the catalog's own tables:
/// routing comes from configuration versions.
pub(crate) const DATA_PLANE_READS: &[&str] = &[
    "organization",
    "team",
    "service_account",
    "api_key",
    "user_account",
    "organization_membership",
    "budget_policy",
    "provider_credential",
    "credential_data_key",
    "cloud_principal_binding",
    "config_feed",
    "config_version",
    // An app's end-user settings, plans and credit grants (D-028).
    "end_user_setting",
    "end_user_plan",
    "end_user_credit_grant",
    // Creating a batch reads whether emulated lines are allowed and where
    // each connection's batch jobs stage (D-021).
    "organization_batch_settings",
    "batch_staging",
];
/// Functions that take locks on its behalf: rate-limit admission locks the
/// organization and its policies, which needs the right to update them.
pub(crate) const DATA_PLANE_FUNCTIONS: &[&str] = &[
    "lock_rate_limit_policies(UUID, UUID, UUID)",
    "lock_rate_limit_policies(UUID, UUID, UUID, UUID)",
    "lock_rate_limit_counters(UUID, UUID, UUID)",
    // Rate-limit leases its gateways take, use and return (D-030).
    "lock_rate_limit_lease_counters(UUID, UUID, UUID, UUID)",
    "grant_rate_limit_lease(UUID, UUID, UUID, UUID, UUID, INTEGER, BIGINT, DOUBLE PRECISION)",
    "use_rate_limit_lease(UUID, INTEGER, UUID, UUID, UUID)",
    "return_rate_limit_lease(UUID, TEXT)",
    // Admission with API-key policies and token limits, and the failover
    // check for the latter (0155).
    "lock_rate_limit_counters(UUID, UUID, UUID, UUID)",
    "use_rate_limit_lease(UUID, INTEGER, UUID, UUID, UUID, UUID)",
    "lock_rate_limit_tokens_covered(UUID, UUID, UUID, UUID)",
];
/// What it writes (and reads): accounting, traces, what its traffic shows
/// about model availability, the outbox, and its own instance row.
pub(crate) const DATA_PLANE_WRITES: &[&str] = &[
    "quota_bucket",
    "accounting_reservation",
    "accounting_allocation",
    "accounting_usage",
    "accounting_finalization",
    "accounting_price_snapshot",
    "accounting_reconciliation",
    // Budget leases its gateways take and return (D-030).
    "budget_lease",
    "inference_trace",
    "catalog_model_evidence",
    "outbox_event",
    "gateway_instance",
    // Files and batches (D-021): a data plane takes uploads and creates
    // batches; the control plane's worker runs them.
    "batch_file",
    "batch",
    "batch_job",
    "batch_line",
    // An end user is created by their first request (D-028).
    "end_user",
];

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("could not acquire a database connection: {0}")]
    Acquire(#[source] sqlx::Error),
    #[error("could not inspect or baseline migration state: {0}")]
    Inspect(#[source] sqlx::Error),
    #[error("migration execution failed: {0}")]
    Execute(#[source] MigrateError),
    #[error("database state is incompatible with the legacy migration baseline")]
    IncompatibleLegacyBaseline,
    #[error("granting the data plane role failed: {0}")]
    Grant(#[source] sqlx::Error),
    #[error("embedded migration {0} is unavailable for baselining")]
    MissingEmbeddedMigration(i64),
}

/// The embedded forward migrations' versions and checksums, which `gateway
/// doctor` compares with the database's history.
pub(crate) fn embedded() -> impl Iterator<Item = (i64, &'static [u8])> {
    MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| (migration.version, &*migration.checksum))
}

/// Apply embedded forward-only migrations on a connection whose search path is
/// temporarily scoped to the pre-provisioned `gateway` schema. The schema and
/// its owner are intentionally managed outside this application.
pub async fn run(pool: &PgPool) -> Result<(), MigrationError> {
    let mut connection = pool.acquire().await.map_err(MigrationError::Acquire)?;
    sqlx::query("SET search_path TO gateway, public")
        .execute(&mut *connection)
        .await
        .map_err(MigrationError::Acquire)?;
    // Processes starting together (API and worker, or parallel tests) would
    // otherwise race to create `_sqlx_migrations` on an empty database.
    // Polled rather than waited for in one statement: a waiting statement is
    // an open transaction, and a migration building an index concurrently
    // (0146) waits for every open transaction, so the run holding the lock
    // and the processes waiting for it would deadlock.
    loop {
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .fetch_one(&mut *connection)
            .await
            .map_err(MigrationError::Acquire)?;
        if locked {
            break;
        }
        tokio::time::sleep(MIGRATION_LOCK_POLL).await;
    }

    let result = migrate_locked(&mut connection).await;
    // Do not leak a migration-specific search path or the lock to the next
    // pool user.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *connection)
        .await;
    let _ = sqlx::query("RESET search_path")
        .execute(&mut *connection)
        .await;
    result
}

async fn migrate_locked(connection: &mut PgConnection) -> Result<(), MigrationError> {
    baseline_legacy_state(&mut *connection).await?;
    repair_renumbered_history(&mut *connection).await?;
    use sqlx::migrate::Migrate;
    let mut attempt = 1;
    loop {
        let Err(error) = MIGRATOR.run_direct(&mut *connection).await else {
            break;
        };
        // sqlx leaves its own advisory lock held when a run fails; release
        // it, or the next run on another connection would wait for it while
        // this one sits in the pool.
        let _ = connection.unlock().await;
        if attempt < MIGRATION_LOCK_ATTEMPTS && lock_timed_out(&error) {
            tracing::warn!(attempt, %error, "a migration timed out waiting for a lock; retrying");
            attempt += 1;
            tokio::time::sleep(MIGRATION_LOCK_RETRY).await;
        } else {
            return Err(MigrationError::Execute(error));
        }
    }
    grant_data_plane(&mut *connection)
        .await
        .map_err(MigrationError::Grant)
}

/// Whether a migration gave up waiting for a lock (`lock_timeout`, SQLSTATE
/// 55P03). Its transaction rolled back, recording nothing, so it can run
/// again.
fn lock_timed_out(error: &MigrateError) -> bool {
    let database = match error {
        MigrateError::ExecuteMigration(sqlx::Error::Database(database), _)
        | MigrateError::Execute(sqlx::Error::Database(database)) => database,
        _ => return false,
    };
    database.code().as_deref() == Some("55P03")
}

/// Grants [`DATA_PLANE_ROLE`] exactly the data plane's access, if the role
/// exists, revoking anything else first. Runs with every migration, so tables
/// added later are covered and a narrowed list takes effect. One transaction,
/// so serving data planes never see their access missing.
async fn grant_data_plane(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    use sqlx::Connection;
    let mut tx = connection.begin().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(DATA_PLANE_ROLE)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Ok(());
    }
    let qualified = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("gateway.{name}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for statement in [
        format!("REVOKE ALL ON ALL TABLES IN SCHEMA gateway FROM {DATA_PLANE_ROLE}"),
        format!("REVOKE ALL ON ALL SEQUENCES IN SCHEMA gateway FROM {DATA_PLANE_ROLE}"),
        format!("GRANT USAGE ON SCHEMA gateway TO {DATA_PLANE_ROLE}"),
        format!(
            "GRANT SELECT ON {} TO {DATA_PLANE_ROLE}",
            qualified(DATA_PLANE_READS)
        ),
        format!(
            "GRANT SELECT, INSERT, UPDATE ON {} TO {DATA_PLANE_ROLE}",
            qualified(DATA_PLANE_WRITES)
        ),
        // When a key was last used; nothing else about it.
        format!("GRANT UPDATE (last_used_at) ON gateway.api_key TO {DATA_PLANE_ROLE}"),
        format!(
            "GRANT EXECUTE ON FUNCTION {} TO {DATA_PLANE_ROLE}",
            qualified(DATA_PLANE_FUNCTIONS)
        ),
    ] {
        sqlx::query(&statement).execute(&mut *tx).await?;
    }
    tx.commit().await
}

async fn baseline_legacy_state(connection: &mut PgConnection) -> Result<(), MigrationError> {
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version BIGINT PRIMARY KEY,
    description TEXT NOT NULL,
    installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
    success BOOLEAN NOT NULL,
    checksum BYTEA NOT NULL,
    execution_time BIGINT NOT NULL
);
        "#,
    )
    .execute(&mut *connection)
    .await
    .map_err(MigrationError::Inspect)?;

    let has_schema_version_table: bool =
        sqlx::query_scalar("SELECT to_regclass('gateway.schema_version') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await
            .map_err(MigrationError::Inspect)?;
    if !has_schema_version_table {
        return Ok(());
    }

    let has_schema_marker: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gateway.schema_version WHERE version = $1)",
    )
    .bind(FOUNDATION_MIGRATION_VERSION)
    .fetch_one(&mut *connection)
    .await
    .map_err(MigrationError::Inspect)?;
    let sqlx_history_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&mut *connection)
        .await
        .map_err(MigrationError::Inspect)?;
    let identity_foundation_relations_present =
        count_present_relations(connection, IDENTITY_FOUNDATION_RELATIONS).await?;
    let baseline_versions = determine_legacy_baseline(
        has_schema_marker,
        sqlx_history_count > 0,
        identity_foundation_relations_present,
    )?;

    for version in baseline_versions {
        let migration = MIGRATOR
            .iter()
            .find(|migration| migration.version == *version)
            .ok_or(MigrationError::MissingEmbeddedMigration(*version))?;
        sqlx::query(
            r#"
INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
VALUES ($1, $2, TRUE, $3, 0)
ON CONFLICT (version) DO NOTHING
            "#,
        )
        .bind(migration.version)
        .bind(&*migration.description)
        .bind(&*migration.checksum)
        .execute(&mut *connection)
        .await
        .map_err(MigrationError::Inspect)?;
    }

    Ok(())
}

/// Move history rows for renumbered migrations to their current version. A row
/// is moved only when its checksum matches the renumbered file's contents, so
/// a different migration applied under the previous version is left alone.
async fn repair_renumbered_history(connection: &mut PgConnection) -> Result<(), MigrationError> {
    for (previous_version, current_version) in RENUMBERED_MIGRATIONS {
        let migration = MIGRATOR
            .iter()
            .find(|migration| migration.version == *current_version)
            .ok_or(MigrationError::MissingEmbeddedMigration(*current_version))?;
        sqlx::query(
            r#"
UPDATE _sqlx_migrations
SET version = $2
WHERE version = $1
  AND checksum = $3
  AND NOT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = $2)
            "#,
        )
        .bind(*previous_version)
        .bind(*current_version)
        .bind(&*migration.checksum)
        .execute(&mut *connection)
        .await
        .map_err(MigrationError::Inspect)?;
    }
    Ok(())
}

async fn count_present_relations(
    connection: &mut PgConnection,
    relations: &[&str],
) -> Result<usize, MigrationError> {
    let mut present = 0;
    for relation in relations {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(*relation)
            .fetch_one(&mut *connection)
            .await
            .map_err(MigrationError::Inspect)?;
        if exists {
            present += 1;
        }
    }
    Ok(present)
}

fn determine_legacy_baseline(
    has_schema_marker: bool,
    has_sqlx_history: bool,
    identity_foundation_relations_present: usize,
) -> Result<&'static [i64], MigrationError> {
    if !has_schema_marker || has_sqlx_history {
        return Ok(&[]);
    }

    if identity_foundation_relations_present == 0 {
        return Ok(&[FOUNDATION_MIGRATION_VERSION]);
    }

    if identity_foundation_relations_present == IDENTITY_FOUNDATION_RELATIONS.len() {
        return Ok(&[
            FOUNDATION_MIGRATION_VERSION,
            IDENTITY_FOUNDATION_MIGRATION_VERSION,
        ]);
    }

    Err(MigrationError::IncompatibleLegacyBaseline)
}

#[cfg(test)]
mod tests {
    use super::{
        FOUNDATION_MIGRATION_VERSION, IDENTITY_FOUNDATION_MIGRATION_VERSION,
        IDENTITY_FOUNDATION_RELATIONS, MIGRATOR, MigrationError, RENUMBERED_MIGRATIONS,
        determine_legacy_baseline, repair_renumbered_history,
    };
    use sqlx::{Connection, PgConnection};
    use std::collections::HashSet;

    #[test]
    fn embedded_migration_versions_are_unique() {
        let mut seen = HashSet::new();
        for migration in MIGRATOR.iter() {
            assert!(
                seen.insert(migration.version),
                "duplicate migration version {} ({})",
                migration.version,
                migration.description
            );
        }
    }

    #[test]
    fn renumbered_migrations_target_embedded_versions() {
        for (previous_version, current_version) in RENUMBERED_MIGRATIONS {
            assert!(previous_version < current_version);
            assert!(
                MIGRATOR
                    .iter()
                    .any(|migration| migration.version == *current_version)
            );
        }
    }

    #[test]
    fn skips_baseline_without_schema_marker() {
        assert!(
            determine_legacy_baseline(false, false, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn skips_baseline_when_sqlx_history_exists() {
        assert!(
            determine_legacy_baseline(true, true, IDENTITY_FOUNDATION_RELATIONS.len())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn baselines_foundation_only_when_identity_tables_are_absent() {
        assert_eq!(
            determine_legacy_baseline(true, false, 0).unwrap(),
            &[FOUNDATION_MIGRATION_VERSION]
        );
    }

    #[test]
    fn baselines_foundation_and_identity_when_legacy_state_is_complete() {
        assert_eq!(
            determine_legacy_baseline(true, false, IDENTITY_FOUNDATION_RELATIONS.len()).unwrap(),
            &[
                FOUNDATION_MIGRATION_VERSION,
                IDENTITY_FOUNDATION_MIGRATION_VERSION
            ]
        );
    }

    #[test]
    fn rejects_partially_provisioned_identity_foundation() {
        assert!(matches!(
            determine_legacy_baseline(true, false, 1),
            Err(MigrationError::IncompatibleLegacyBaseline)
        ));
    }

    #[tokio::test]
    async fn repairs_admin_access_history_recorded_under_previous_versions() {
        let database_url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return,
        };
        let mut connection = PgConnection::connect(&database_url).await.unwrap();
        // Unqualified names resolve in pg_temp first, so this history table
        // shadows the shared one for this connection only.
        sqlx::query(
            "CREATE TEMP TABLE _sqlx_migrations (
                 version BIGINT PRIMARY KEY,
                 description TEXT NOT NULL,
                 installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
                 success BOOLEAN NOT NULL,
                 checksum BYTEA NOT NULL,
                 execution_time BIGINT NOT NULL
             )",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        let embedded = |version: i64| {
            MIGRATOR
                .iter()
                .find(|migration| migration.version == version)
                .unwrap()
        };
        // 23 carries the admin-access checksum and must move to 26; 24 carries
        // the Azure AI Foundry checksum and must stay where it is.
        for (recorded_version, migration) in [(23, embedded(26)), (24, embedded(24))] {
            sqlx::query(
                "INSERT INTO _sqlx_migrations
                     (version, description, success, checksum, execution_time)
                 VALUES ($1, $2, TRUE, $3, 0)",
            )
            .bind(recorded_version)
            .bind(&*migration.description)
            .bind(&*migration.checksum)
            .execute(&mut connection)
            .await
            .unwrap();
        }

        repair_renumbered_history(&mut connection).await.unwrap();
        // Repair is idempotent.
        repair_renumbered_history(&mut connection).await.unwrap();

        let rows: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&mut connection)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                (24, embedded(24).checksum.to_vec()),
                (26, embedded(26).checksum.to_vec()),
            ]
        );
        connection.close().await.unwrap();
    }
}
