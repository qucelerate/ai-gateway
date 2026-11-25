//! The checks `gateway doctor` runs. Each reads; none writes.
//!
//! The database session is read-only (`default_transaction_read_only`) and
//! bounded (`statement_timeout`, `lock_timeout`), and every check is
//! bounded as a whole too, so a stalled database or network cannot hang
//! the command.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    str::FromStr,
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use url::Url;

use super::{
    Check, Options,
    net::{Probe, STEP_TIMEOUT, probe},
};
use crate::{
    AppConfig, ConfigError, GatewayCommand, GatewayProfile,
    listeners::ListenerConfigError,
    persistence::migrations::{
        self, DATA_PLANE_FUNCTIONS, DATA_PLANE_READS, DATA_PLANE_ROLE, DATA_PLANE_WRITES,
        RENUMBERED_MIGRATIONS,
    },
};

/// The most any one check may take, whatever it waits for.
pub(super) const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// The doctor session's statement and lock timeouts, in milliseconds.
const STATEMENT_TIMEOUT_MS: u32 = 5_000;
const LOCK_TIMEOUT_MS: u32 = 2_000;

/// Skew below this (plus half the round trip) is ok; up to
/// [`SKEW_FAIL_MS`] it warns. Past that, request signing (AWS SigV4) and
/// token validation start to reject.
const SKEW_WARN_MS: i64 = 2_000;
const SKEW_FAIL_MS: i64 = 5 * 60 * 1_000;

pub(super) async fn run(lookup: &dyn Fn(&str) -> Option<String>, options: Options) -> Vec<Check> {
    let mut checks = vec![required_settings(lookup)];
    let config = match AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| lookup(name)) {
        Ok(config) => {
            checks.push(Check::ok(
                "config.load",
                format!("configuration loads (profile {})", config.profile),
            ));
            config
        }
        Err(error) => {
            checks.push(Check::fail(
                "config.load",
                config_error_summary(&error),
                "fix the setting named above (see .env.example); the remaining checks need a configuration that loads",
            ));
            for id in [
                "credentials.encryption_key",
                "database.connect",
                "database.migrations",
                "database.grants",
                "database.clock_skew",
                "providers.configured",
                "providers.network",
                "trace_store.reachable",
                "clickhouse.reachable",
            ] {
                checks.push(Check::skip(id, "not run: the configuration does not load"));
            }
            return checks;
        }
    };

    checks.push(encryption_key(&config));
    let stored = database(&config, &mut checks).await;
    let endpoints = provider_endpoints(&config);
    checks.push(providers_configured(&endpoints, stored.as_ref()));
    checks.push(
        bounded(
            "providers.network",
            providers_network(&endpoints, options.network, lookup),
        )
        .await,
    );
    checks.push(bounded("trace_store.reachable", trace_store(&config, lookup)).await);
    checks.push(bounded("clickhouse.reachable", clickhouse(&config, lookup)).await);
    checks
}

/// `check`, or a failed check `id` if it takes longer than `limit`.
pub(super) async fn bounded_by(
    id: &'static str,
    limit: Duration,
    check: impl Future<Output = Check>,
) -> Check {
    match tokio::time::timeout(limit, check).await {
        Ok(check) => check,
        Err(_) => Check::fail(
            id,
            format!("did not finish within {} s", limit.as_secs()),
            "something on the way is stalling: check the network path, and for the database, sessions holding locks",
        ),
    }
}

async fn bounded(id: &'static str, check: impl Future<Output = Check>) -> Check {
    bounded_by(id, CHECK_TIMEOUT, check).await
}

/// The settings every process needs, by name: present or missing.
fn required_settings(lookup: &dyn Fn(&str) -> Option<String>) -> Check {
    let set = |name: &str| lookup(name).is_some_and(|value| !value.trim().is_empty());
    let mut required = vec!["DATABASE_URL", "GATEWAY_PROFILE"];
    // A worker never reads the pepper; every API-serving profile does.
    if lookup("GATEWAY_PROFILE").as_deref().map(str::trim) != Some("worker") {
        required.push("API_KEY_PEPPER");
    }
    let missing: Vec<_> = required.iter().copied().filter(|name| !set(name)).collect();
    if missing.is_empty() {
        Check::ok("config.required", format!("set: {}", required.join(", ")))
    } else {
        Check::fail(
            "config.required",
            format!("missing: {}", missing.join(", ")),
            "set them in the environment or the .env file (see .env.example)",
        )
    }
}

/// A configuration error's message, without the value that caused it.
pub(super) fn config_error_summary(error: &ConfigError) -> String {
    match error {
        ConfigError::InvalidBindAddr { .. } => "BIND_ADDR is not a valid socket address".into(),
        ConfigError::InvalidMetricsAddr { .. } => {
            "GATEWAY_METRICS_ADDR must be a socket address other than BIND_ADDR".into()
        }
        ConfigError::InvalidProfile { .. } => {
            "GATEWAY_PROFILE must be all-in-one, api, worker or data-plane".into()
        }
        ConfigError::InvalidArgument { .. } => "invalid command-line argument".into(),
        ConfigError::InvalidPepper(_) => "API_KEY_PEPPER is invalid".into(),
        ConfigError::InvalidListener(ListenerConfigError::InvalidAddr { name, .. }) => {
            format!("{name} is not a valid socket address")
        }
        other => other.to_string(),
    }
}

fn encryption_key(config: &AppConfig) -> Check {
    if config.credential_cipher.is_some() {
        Check::ok(
            "credentials.encryption_key",
            "GATEWAY_CREDENTIAL_ENCRYPTION_KEY is set: provider credentials can be stored",
        )
    } else if config.credential_kms.is_some() {
        // The key's settings only; AWS KMS itself is not called.
        Check::ok(
            "credentials.encryption_key",
            "GATEWAY_CREDENTIAL_KMS_KEY_ID is set: provider credentials are sealed under AWS KMS data keys",
        )
    } else {
        Check::warn(
            "credentials.encryption_key",
            "GATEWAY_CREDENTIAL_ENCRYPTION_KEY is not set: provider credentials cannot be stored through the admin API",
            "generate one with `openssl rand -base64 32 | tr '+/' '-_' | tr -d '='` and set it on every gateway and worker process",
        )
    }
}

/// Opens the doctor's database session: one connection, read-only, with
/// short statement and lock timeouts, within [`STEP_TIMEOUT`]. The error
/// is the real one (refused, unknown host, authentication failed...),
/// never the URL or its password.
pub async fn connect(database_url: &str) -> Result<PgConnection, String> {
    let options = PgConnectOptions::from_str(database_url)
        .map_err(|_| "DATABASE_URL is not a valid PostgreSQL URL".to_owned())?
        .application_name("gateway-doctor")
        .options([
            ("statement_timeout", STATEMENT_TIMEOUT_MS.to_string()),
            ("lock_timeout", LOCK_TIMEOUT_MS.to_string()),
            ("default_transaction_read_only", "on".to_owned()),
        ]);
    match tokio::time::timeout(STEP_TIMEOUT, PgConnection::connect_with(&options)).await {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(error)) => Err(sql_error(&error)),
        Err(_) => Err(format!(
            "no answer within {} s (a firewall dropping packets, or the wrong host)",
            STEP_TIMEOUT.as_secs()
        )),
    }
}

/// The database checks; returns the stored provider credentials per family
/// when they could be read.
async fn database(config: &AppConfig, checks: &mut Vec<Check>) -> Option<BTreeMap<String, i64>> {
    let mut connection = match connect(config.database_url()).await {
        Ok(connection) => connection,
        Err(reason) => {
            checks.push(Check::fail(
                "database.connect",
                format!("cannot connect to DATABASE_URL: {reason}"),
                "check that PostgreSQL is running and reachable from here, and the URL's host, port, database, user and password",
            ));
            for id in [
                "database.migrations",
                "database.grants",
                "database.clock_skew",
            ] {
                checks.push(Check::skip(id, "not run: the database is unreachable"));
            }
            return None;
        }
    };
    let conn = &mut connection;
    checks.push(
        bounded("database.connect", async {
            let identity: Result<(String, String), _> =
                sqlx::query_as("SELECT current_user::text, current_setting('server_version')")
                    .fetch_one(&mut *conn)
                    .await;
            match identity {
                Ok((role, version)) => Check::ok(
                    "database.connect",
                    format!("connected as role {role} (PostgreSQL {version})"),
                ),
                Err(error) => Check::fail(
                    "database.connect",
                    format!("connected, but a query failed: {}", sql_error(&error)),
                    "check the role can log in to the database and run queries",
                ),
            }
        })
        .await,
    );
    let schema: bool = tokio::time::timeout(
        CHECK_TIMEOUT,
        sqlx::query_scalar("SELECT to_regnamespace('gateway') IS NOT NULL").fetch_one(&mut *conn),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or(false);
    if !schema {
        checks.push(Check::fail(
            "database.migrations",
            "the database has no `gateway` schema",
            "create the `gateway` schema owned by the application role, then run `gateway migrate`",
        ));
        checks.push(Check::skip(
            "database.grants",
            "not run: no `gateway` schema",
        ));
    } else {
        checks.push(
            bounded(
                "database.migrations",
                migration_history(&mut *conn, config.profile),
            )
            .await,
        );
        checks.push(bounded("database.grants", grants(&mut *conn, config.profile)).await);
    }
    checks.push(bounded("database.clock_skew", clock_skew(&mut *conn)).await);
    let stored = if schema {
        tokio::time::timeout(
            CHECK_TIMEOUT,
            sqlx::query_as::<_, (String, i64)>(
                "SELECT family, count(*) FROM gateway.provider_credential GROUP BY family",
            )
            .fetch_all(&mut *conn),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|rows| rows.into_iter().collect())
    } else {
        None
    };
    let _ = tokio::time::timeout(STEP_TIMEOUT, connection.close()).await;
    stored
}

/// A database error as it is safe to print: never the URL.
fn sql_error(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Configuration(_) => "DATABASE_URL is not a valid PostgreSQL URL".into(),
        sqlx::Error::Database(database) => match database.code() {
            Some(code) => format!("{} (SQLSTATE {code})", database.message()),
            None => database.message().to_owned(),
        },
        sqlx::Error::Io(io) => format!("{io}"),
        sqlx::Error::Tls(_) => "TLS negotiation failed".into(),
        sqlx::Error::PoolTimedOut => "timed out".into(),
        sqlx::Error::Protocol(_) => "protocol error".into(),
        _ => "database error".into(),
    }
}

async fn migration_history(conn: &mut PgConnection, profile: GatewayProfile) -> Check {
    const ID: &str = "database.migrations";
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('gateway._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .unwrap_or(false);
    if !exists {
        return Check::fail(
            ID,
            "no migrations have been applied",
            "run `gateway migrate` with the application role",
        );
    }
    let rows = sqlx::query_as::<_, (i64, bool, Vec<u8>)>(
        "SELECT version, success, checksum FROM gateway._sqlx_migrations",
    )
    .fetch_all(&mut *conn)
    .await;
    match rows {
        Ok(applied) => {
            let embedded: Vec<_> = migrations::embedded().collect();
            evaluate_migrations(&embedded, &applied, RENUMBERED_MIGRATIONS)
        }
        Err(error) if profile == GatewayProfile::DataPlane => Check::skip(
            ID,
            format!(
                "this role cannot read the migration history ({})",
                sql_error(&error)
            ),
        )
        .with_hint("expected for the data-plane role; run `gateway doctor` with the application role to check migrations"),
        Err(error) => Check::fail(
            ID,
            format!("cannot read the migration history: {}", sql_error(&error)),
            "connect as the application role that runs `gateway migrate`",
        ),
    }
}

/// Compares the binary's migrations with the database's history: failed,
/// changed, pending, and unknown (applied by a newer release), in that
/// order of severity.
///
/// `renumbered` lists `(previous, current)` versions of migrations that
/// moved: a history row under `previous` whose checksum is the embedded
/// `current` migration's is that migration, applied before it moved, and
/// `gateway migrate` renumbers it. It is pending, not changed.
pub fn evaluate_migrations(
    embedded: &[(i64, &[u8])],
    applied: &[(i64, bool, Vec<u8>)],
    renumbered: &[(i64, i64)],
) -> Check {
    const ID: &str = "database.migrations";
    let list = |versions: &[i64]| {
        let shown: Vec<_> = versions.iter().take(5).map(i64::to_string).collect();
        let more = versions.len().saturating_sub(5);
        if more > 0 {
            format!("{} and {more} more", shown.join(", "))
        } else {
            shown.join(", ")
        }
    };
    let known: BTreeMap<i64, &[u8]> = embedded.iter().copied().collect();
    let mut history: BTreeMap<i64, (bool, &[u8])> = applied
        .iter()
        .map(|(version, success, checksum)| (*version, (*success, checksum.as_slice())))
        .collect();
    let binary_at = known.keys().next_back().copied().unwrap_or(0);
    let database_at = history.keys().next_back().copied().unwrap_or(0);

    // Rows `gateway migrate` would renumber are read under their current
    // versions, and leave their old ones pending.
    let mut moved = Vec::new();
    for (previous, current) in renumbered {
        let is_moved = !history.contains_key(current)
            && history
                .get(previous)
                .zip(known.get(current))
                .is_some_and(|((_, checksum), ours)| checksum == ours);
        if is_moved && let Some(row) = history.remove(previous) {
            history.insert(*current, row);
            moved.push(*previous);
        }
    }

    let failed: Vec<i64> = history
        .iter()
        .filter(|(_, (success, _))| !success)
        .map(|(version, _)| *version)
        .collect();
    if !failed.is_empty() {
        return Check::fail(
            ID,
            format!("migration {} did not complete", list(&failed)),
            "read the `gateway migrate` output for the cause, fix it, and run `gateway migrate` again",
        );
    }
    let changed: Vec<i64> = history
        .iter()
        .filter(|(version, (_, checksum))| known.get(version).is_some_and(|ours| ours != checksum))
        .map(|(version, _)| *version)
        .collect();
    if !changed.is_empty() {
        return Check::fail(
            ID,
            format!(
                "applied migration {} differs from this binary's copy",
                list(&changed)
            ),
            "the database was migrated by a different build: run the release that migrated it, or restore a backup made with this one",
        );
    }
    let pending: Vec<i64> = known
        .keys()
        .filter(|version| !history.contains_key(version))
        .copied()
        .collect();
    if !pending.is_empty() {
        let renumbering = if moved.is_empty() {
            String::new()
        } else {
            format!(
                "; the history also records migration {} under an earlier number, which `gateway migrate` corrects",
                list(&moved)
            )
        };
        return Check::fail(
            ID,
            format!(
                "{} migration(s) pending: the database is at {database_at}, this binary at {binary_at}{renumbering}",
                pending.len()
            ),
            "run `gateway migrate` with the application role before starting this release",
        );
    }
    let unknown: Vec<i64> = history
        .keys()
        .filter(|version| !known.contains_key(version))
        .copied()
        .collect();
    if !unknown.is_empty() {
        return Check::warn(
            ID,
            format!(
                "the database has migration {} that this binary does not know: it is at {database_at}, this binary at {binary_at}",
                list(&unknown)
            ),
            "a newer release migrated this database; run that release (migrations are forward-only)",
        );
    }
    Check::ok(
        ID,
        format!(
            "up to date: {} migrations applied, latest {binary_at}",
            history.len()
        ),
    )
}

/// The privileges the profile's processes use: the data plane's exact
/// grants, or every privilege on every gateway table for the others.
async fn grants(conn: &mut PgConnection, profile: GatewayProfile) -> Check {
    const ID: &str = "database.grants";
    let result = if profile == GatewayProfile::DataPlane {
        data_plane_grants(conn)
            .await
            .map(|grants| evaluate_data_plane_grants(&grants))
    } else {
        application_grants(conn)
            .await
            .map(|grants| evaluate_application_grants(&grants))
    };
    result.unwrap_or_else(|error| {
        Check::warn(
            ID,
            format!("cannot inspect privileges: {}", sql_error(&error)),
            "run `gateway doctor` with the role the gateway connects as",
        )
    })
}

/// What the data-plane role lacks of the grants `gateway migrate` gives it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DataPlaneGrants {
    /// Tables in the grant list that do not exist.
    pub missing_tables: Vec<String>,
    /// Tables and the privileges the role lacks on them.
    pub lacking: Vec<(String, Vec<String>)>,
    /// Functions in the grant list that do not exist.
    pub missing_functions: Vec<String>,
    /// Functions the role may not execute.
    pub no_execute: Vec<String>,
    /// Whether it may update `api_key.last_used_at`.
    pub last_used_at: bool,
}

async fn data_plane_grants(conn: &mut PgConnection) -> Result<DataPlaneGrants, sqlx::Error> {
    let reads: Vec<String> = DATA_PLANE_READS.iter().map(|t| (*t).to_owned()).collect();
    let writes: Vec<String> = DATA_PLANE_WRITES.iter().map(|t| (*t).to_owned()).collect();
    let tables: Vec<(String, bool, Vec<String>)> = sqlx::query_as(
        r#"
SELECT t, r IS NOT NULL,
       CASE WHEN r IS NULL THEN ARRAY[]::text[] ELSE array_remove(ARRAY[
         CASE WHEN has_table_privilege(r, 'SELECT') THEN NULL ELSE 'SELECT' END,
         CASE WHEN NOT w OR has_table_privilege(r, 'INSERT') THEN NULL ELSE 'INSERT' END,
         CASE WHEN NOT w OR has_table_privilege(r, 'UPDATE') THEN NULL ELSE 'UPDATE' END
       ], NULL) END
FROM (
  SELECT t, w, to_regclass('gateway.' || t) AS r FROM (
    SELECT unnest($1::text[]) AS t, false AS w
    UNION ALL
    SELECT unnest($2::text[]), true
  ) listed
) resolved
ORDER BY t
"#,
    )
    .bind(&reads)
    .bind(&writes)
    .fetch_all(&mut *conn)
    .await?;
    let functions: Vec<String> = DATA_PLANE_FUNCTIONS
        .iter()
        .map(|f| (*f).to_owned())
        .collect();
    let callable: Vec<(String, bool, bool)> = sqlx::query_as(
        r#"
SELECT f, p IS NOT NULL, COALESCE(p IS NOT NULL AND has_function_privilege(p, 'EXECUTE'), false)
FROM (SELECT f, to_regprocedure('gateway.' || f) AS p FROM unnest($1::text[]) AS f) resolved
ORDER BY f
"#,
    )
    .bind(&functions)
    .fetch_all(&mut *conn)
    .await?;
    let last_used_at: bool = sqlx::query_scalar(
        r#"
SELECT CASE WHEN to_regclass('gateway.api_key') IS NULL THEN false
            ELSE has_column_privilege('gateway.api_key', 'last_used_at', 'UPDATE') END
"#,
    )
    .fetch_one(&mut *conn)
    .await?;
    let mut grants = DataPlaneGrants {
        last_used_at,
        ..Default::default()
    };
    for (table, present, lacking) in tables {
        if !present {
            grants.missing_tables.push(table);
        } else if !lacking.is_empty() {
            grants.lacking.push((table, lacking));
        }
    }
    for (function, present, executable) in callable {
        if !present {
            grants.missing_functions.push(function);
        } else if !executable {
            grants.no_execute.push(function);
        }
    }
    Ok(grants)
}

/// Fails on anything the data plane would be refused, naming each table,
/// function or column.
pub fn evaluate_data_plane_grants(grants: &DataPlaneGrants) -> Check {
    const ID: &str = "database.grants";
    let mut problems = Vec::new();
    if !grants.missing_tables.is_empty() {
        problems.push(format!(
            "missing tables: {}",
            grants.missing_tables.join(", ")
        ));
    }
    if !grants.lacking.is_empty() {
        problems.push(format!(
            "lacks {}",
            grants
                .lacking
                .iter()
                .map(|(table, privileges)| format!("{} on {table}", privileges.join("/")))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !grants.missing_functions.is_empty() {
        problems.push(format!(
            "missing functions: {}",
            grants.missing_functions.join(", ")
        ));
    }
    if !grants.no_execute.is_empty() {
        problems.push(format!("lacks EXECUTE on {}", grants.no_execute.join(", ")));
    }
    if !grants.last_used_at {
        problems.push("lacks UPDATE (last_used_at) on api_key".to_owned());
    }
    if problems.is_empty() {
        Check::ok(
            ID,
            format!(
                "the role has the data plane's privileges: {} tables, {} functions and api_key.last_used_at",
                DATA_PLANE_READS.len() + DATA_PLANE_WRITES.len(),
                DATA_PLANE_FUNCTIONS.len()
            ),
        )
    } else {
        let hint = if grants.missing_tables.is_empty() && grants.missing_functions.is_empty() {
            format!(
                "run `gateway migrate` once with the application role: it grants `{DATA_PLANE_ROLE}` exactly these privileges"
            )
        } else {
            "the schema is behind this binary: run `gateway migrate` with the application role, which also grants the data plane's privileges".to_owned()
        };
        Check::fail(ID, format!("the role {}", problems.join("; ")), hint)
    }
}

/// What the application role lacks: it owns the `gateway` schema, so it
/// holds every privilege on every table, and the gateway reads, inserts,
/// updates and deletes (outbox, webhooks, configuration versions, catalog
/// sync, retention sweeps...).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApplicationGrants {
    pub usage: bool,
    pub create: bool,
    /// Tables and the privileges the role lacks on them.
    pub lacking: Vec<(String, Vec<String>)>,
    pub data_plane_role: bool,
}

async fn application_grants(conn: &mut PgConnection) -> Result<ApplicationGrants, sqlx::Error> {
    let (usage, create): (bool, bool) = sqlx::query_as(
        "SELECT has_schema_privilege('gateway', 'USAGE'), has_schema_privilege('gateway', 'CREATE')",
    )
    .fetch_one(&mut *conn)
    .await?;
    let lacking: Vec<(String, Vec<String>)> = if usage {
        sqlx::query_as(
            r#"
SELECT c.relname::text, array_remove(ARRAY[
         CASE WHEN has_table_privilege(c.oid, 'SELECT') THEN NULL ELSE 'SELECT' END,
         CASE WHEN has_table_privilege(c.oid, 'INSERT') THEN NULL ELSE 'INSERT' END,
         CASE WHEN has_table_privilege(c.oid, 'UPDATE') THEN NULL ELSE 'UPDATE' END,
         CASE WHEN has_table_privilege(c.oid, 'DELETE') THEN NULL ELSE 'DELETE' END
       ], NULL) AS lacking
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'gateway' AND c.relkind IN ('r', 'p') AND NOT c.relispartition
ORDER BY 1
"#,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .filter(|(_, lacking): &(String, Vec<String>)| !lacking.is_empty())
        .collect()
    } else {
        Vec::new()
    };
    let data_plane_role: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(DATA_PLANE_ROLE)
            .fetch_one(&mut *conn)
            .await?;
    Ok(ApplicationGrants {
        usage,
        create,
        lacking,
        data_plane_role,
    })
}

pub fn evaluate_application_grants(grants: &ApplicationGrants) -> Check {
    const ID: &str = "database.grants";
    if !grants.usage {
        return Check::fail(
            ID,
            "the role has no USAGE on schema `gateway`",
            "connect as the application role that owns the `gateway` schema",
        );
    }
    if !grants.lacking.is_empty() {
        let shown: Vec<_> = grants
            .lacking
            .iter()
            .take(10)
            .map(|(table, privileges)| format!("{} on {table}", privileges.join("/")))
            .collect();
        let more = grants.lacking.len().saturating_sub(10);
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        return Check::fail(
            ID,
            format!(
                "the role lacks privileges on {} table(s): {}{more}",
                grants.lacking.len(),
                shown.join(", ")
            ),
            "connect as the application role that owns the `gateway` schema (the data-plane role is for GATEWAY_PROFILE=data-plane only)",
        );
    }
    let role_note = if grants.data_plane_role {
        format!("; role `{DATA_PLANE_ROLE}` exists for data planes")
    } else {
        String::new()
    };
    if grants.create {
        Check::ok(
            ID,
            format!(
                "the role reads, writes and deletes in every gateway table and can run migrations{role_note}"
            ),
        )
    } else {
        Check::warn(
            ID,
            format!(
                "the role reads, writes and deletes in every gateway table but cannot create objects in schema `gateway`{role_note}"
            ),
            "`gateway migrate` must run as the schema's owner",
        )
    }
}

async fn clock_skew(conn: &mut PgConnection) -> Check {
    let before = SystemTime::now();
    let started = Instant::now();
    let database: Result<DateTime<Utc>, _> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await;
    let round_trip = started.elapsed();
    match database {
        Ok(database) => {
            let local: DateTime<Utc> = (before + round_trip / 2).into();
            let skew = (database - local).num_milliseconds();
            evaluate_clock_skew(skew, round_trip.as_millis() as i64)
        }
        Err(error) => Check::warn(
            "database.clock_skew",
            format!("cannot read the database clock: {}", sql_error(&error)),
            "check the database connection",
        ),
    }
}

/// `skew_ms` is the database clock minus this host's; half the round trip
/// is allowed on top of the thresholds.
pub fn evaluate_clock_skew(skew_ms: i64, round_trip_ms: i64) -> Check {
    const ID: &str = "database.clock_skew";
    let slack = round_trip_ms.max(0) / 2;
    let magnitude = skew_ms.unsigned_abs() as i64;
    let direction = if skew_ms > 0 { "behind" } else { "ahead of" };
    let describe = || format!("this host's clock is {magnitude} ms {direction} the database's");
    let hint = "keep every host's clock synchronized with NTP (chrony or systemd-timesyncd)";
    if magnitude <= SKEW_WARN_MS + slack {
        Check::ok(ID, format!("within {SKEW_WARN_MS} ms ({magnitude} ms)"))
    } else if magnitude <= SKEW_FAIL_MS + slack {
        Check::warn(ID, describe(), hint)
    } else {
        Check::fail(
            ID,
            format!(
                "{}: signed provider requests and token checks will fail",
                describe()
            ),
            hint,
        )
    }
}

/// Each provider adapter the environment configures: its family and base
/// URL.
fn provider_endpoints(config: &AppConfig) -> Vec<(&'static str, String)> {
    let mut endpoints = Vec::new();
    let mut add = |family: &'static str, url: Option<&String>| {
        if let Some(url) = url {
            endpoints.push((family, url.clone()));
        }
    };
    add("openai", config.openai.as_ref().map(|c| &c.base_url));
    add("anthropic", config.anthropic.as_ref().map(|c| &c.base_url));
    add("gemini", config.gemini.as_ref().map(|c| &c.base_url));
    add("bedrock", config.bedrock.as_ref().map(|c| &c.base_url));
    add(
        "azure-openai",
        config.azure_openai.as_ref().map(|c| &c.base_url),
    );
    add(
        "azure-ai-foundry",
        config.azure_ai_foundry.as_ref().map(|c| &c.base_url),
    );
    add(
        "openai-compatible",
        config.openai_compatible.as_ref().map(|c| &c.base_url),
    );
    endpoints
}

fn providers_configured(
    endpoints: &[(&'static str, String)],
    stored: Option<&BTreeMap<String, i64>>,
) -> Check {
    const ID: &str = "providers.configured";
    let families: BTreeSet<&str> = endpoints.iter().map(|(family, _)| *family).collect();
    let from_env = if families.is_empty() {
        "none in the environment".to_owned()
    } else {
        format!(
            "environment: {}",
            families.into_iter().collect::<Vec<_>>().join(", ")
        )
    };
    let from_db = match stored {
        None => "stored credentials unknown".to_owned(),
        Some(stored) if stored.is_empty() => "no stored credentials".to_owned(),
        Some(stored) => format!(
            "stored credentials: {}",
            stored
                .iter()
                .map(|(family, count)| format!("{family} ({count})"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let summary = format!("{from_env}; {from_db}");
    if endpoints.is_empty() && stored.is_none_or(BTreeMap::is_empty) {
        Check::warn(
            ID,
            summary,
            "no provider is configured, so inference requests fail: set a provider's settings (see .env.example) or add a provider connection in the admin console",
        )
    } else {
        Check::ok(ID, summary)
    }
}

async fn providers_network(
    endpoints: &[(&'static str, String)],
    network: bool,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Check {
    const ID: &str = "providers.network";
    if !network {
        return Check::skip(ID, "not run").with_hint(
            "pass --network to resolve and TLS-connect to each provider host (no model calls)",
        );
    }
    if endpoints.is_empty() {
        return Check::skip(
            ID,
            "no provider adapters in the environment (credentials stored in the database are not probed)",
        );
    }
    let probes = endpoints.iter().map(|(family, url)| async move {
        let host = Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "?".to_owned());
        (*family, host, probe(url, None, lookup).await)
    });
    let results = futures::future::join_all(probes).await;
    let describe = |want: fn(&Probe) -> Option<&String>| {
        results
            .iter()
            .filter_map(|(family, host, result)| {
                want(result).map(|detail| format!("{family} {host} ({detail})"))
            })
            .collect::<Vec<_>>()
    };
    let failed = describe(|p| match p {
        Probe::Failed(reason) => Some(reason),
        _ => None,
    });
    let unprobed = describe(|p| match p {
        Probe::NotProbed(reason) => Some(reason),
        _ => None,
    });
    let reached = describe(|p| match p {
        Probe::Reached(how) => Some(how),
        _ => None,
    });
    let mut parts = Vec::new();
    for (label, items) in [
        ("unreachable", &failed),
        ("not probed", &unprobed),
        ("reachable", &reached),
    ] {
        if !items.is_empty() {
            parts.push(format!("{label}: {}", items.join("; ")));
        }
    }
    let summary = parts.join("; ");
    if !failed.is_empty() {
        Check::fail(
            ID,
            summary,
            "check DNS, egress firewall rules and any HTTPS proxy between this host and the provider",
        )
    } else if !unprobed.is_empty() {
        Check::warn(
            ID,
            summary,
            "the gateway reaches these hosts through the proxy in HTTPS_PROXY/ALL_PROXY; check the proxy allows them",
        )
    } else {
        Check::ok(ID, summary)
    }
}

/// One store's probe as a check.
fn store_check(id: &'static str, result: Probe, what: &str, hint: &str) -> Check {
    match result {
        Probe::Reached(how) => Check::ok(id, format!("{what} reachable ({how})")),
        Probe::Failed(reason) => Check::fail(id, format!("{what} unreachable: {reason}"), hint),
        Probe::NotProbed(reason) => Check::warn(
            id,
            format!("{what} {reason}"),
            "the gateway reaches it through the proxy in the environment; check the proxy allows it",
        ),
    }
}

async fn trace_store(config: &AppConfig, lookup: &dyn Fn(&str) -> Option<String>) -> Check {
    const ID: &str = "trace_store.reachable";
    let Some(store) = &config.trace_store else {
        return Check::skip(
            ID,
            "no trace store configured (GATEWAY_TRACE_STORE_BUCKET unset): traces keep timing and usage, not content",
        );
    };
    let url = match &store.endpoint {
        Some(endpoint) => endpoint.as_str().to_owned(),
        None => format!("https://s3.{}.amazonaws.com", store.region),
    };
    store_check(
        ID,
        probe(&url, None, lookup).await,
        "bucket endpoint",
        "check GATEWAY_TRACE_STORE_ENDPOINT and GATEWAY_TRACE_STORE_REGION, and that the object store is running",
    )
}

async fn clickhouse(config: &AppConfig, lookup: &dyn Fn(&str) -> Option<String>) -> Check {
    const ID: &str = "clickhouse.reachable";
    let Some(clickhouse) = &config.clickhouse else {
        return Check::skip(
            ID,
            "ClickHouse not configured (GATEWAY_CLICKHOUSE_URL unset): analytics read PostgreSQL",
        );
    };
    store_check(
        ID,
        probe(
            clickhouse.url.as_str(),
            clickhouse.ca_pem.as_deref(),
            lookup,
        )
        .await,
        "ClickHouse",
        "check GATEWAY_CLICKHOUSE_URL and that ClickHouse is running",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn required_settings_name_what_is_missing_and_never_values() {
        let vars = [
            ("DATABASE_URL", "postgres://u:s3cret@db/gw"),
            ("GATEWAY_PROFILE", "api"),
        ];
        let check = required_settings(&lookup(&vars));
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.summary, "missing: API_KEY_PEPPER");
        let check = required_settings(&lookup(&[
            ("DATABASE_URL", "x"),
            ("GATEWAY_PROFILE", "worker"),
        ]));
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.summary, "set: DATABASE_URL, GATEWAY_PROFILE");
        let vars = [
            ("DATABASE_URL", "postgres://u:s3cret@db/gw"),
            ("GATEWAY_PROFILE", "api"),
            ("API_KEY_PEPPER", "pepper-value-that-is-secret"),
        ];
        let check = required_settings(&lookup(&vars));
        assert_eq!(check.status, Status::Ok);
        assert!(!check.summary.contains("s3cret") && !check.summary.contains("pepper-value"));
    }

    #[test]
    fn config_errors_drop_values() {
        let summary = config_error_summary(&ConfigError::InvalidBindAddr {
            value: "secret-ish:99999".into(),
        });
        assert!(!summary.contains("secret-ish"));
        assert_eq!(summary, "BIND_ADDR is not a valid socket address");
        let summary = config_error_summary(&ConfigError::InvalidProfile {
            value: "oops".into(),
        });
        assert!(!summary.contains("oops"));
        assert_eq!(
            config_error_summary(&ConfigError::Missing {
                name: "DATABASE_URL"
            }),
            "missing required environment variable DATABASE_URL"
        );
    }

    #[tokio::test]
    async fn an_unloadable_configuration_fails_and_skips_the_rest() {
        let checks = run(&lookup(&[("GATEWAY_PROFILE", "api")]), Options::default()).await;
        let status = |id: &str| checks.iter().find(|c| c.id == id).unwrap().status;
        assert_eq!(status("config.required"), Status::Fail);
        assert_eq!(status("config.load"), Status::Fail);
        assert_eq!(status("database.connect"), Status::Skip);
        assert_eq!(status("clickhouse.reachable"), Status::Skip);
        assert_eq!(checks.len(), 11);
    }

    fn evaluate(embedded: &[(i64, &[u8])], applied: &[(i64, bool, Vec<u8>)]) -> Check {
        evaluate_migrations(embedded, applied, &[])
    }

    const A: &[u8] = b"a";
    const B: &[u8] = b"b";

    #[test]
    fn migrations_up_to_date() {
        let check = evaluate(
            &[(1, A), (2, B)],
            &[(1, true, A.to_vec()), (2, true, B.to_vec())],
        );
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.summary, "up to date: 2 migrations applied, latest 2");
    }

    #[test]
    fn migrations_pending_fail() {
        let check = evaluate(&[(1, A), (2, B)], &[(1, true, A.to_vec())]);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.summary.contains("1 migration(s) pending"),
            "{}",
            check.summary
        );
        assert!(check.summary.contains("database is at 1, this binary at 2"));
        assert!(check.hint.unwrap().contains("gateway migrate"));
        let check = evaluate(&[(1, A)], &[]);
        assert_eq!(check.status, Status::Fail);
        assert!(check.summary.contains("database is at 0"));
    }

    #[test]
    fn migrations_failed_or_changed_fail_first() {
        let check = evaluate(
            &[(1, A), (2, B)],
            &[(1, true, A.to_vec()), (2, false, B.to_vec())],
        );
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.summary, "migration 2 did not complete");
        let check = evaluate(&[(1, A), (2, B)], &[(1, true, B.to_vec())]);
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.summary,
            "applied migration 1 differs from this binary's copy"
        );
    }

    #[test]
    fn migrations_from_a_newer_release_warn() {
        let check = evaluate(&[(1, A)], &[(1, true, A.to_vec()), (2, true, B.to_vec())]);
        assert_eq!(check.status, Status::Warn);
        assert!(
            check
                .summary
                .contains("migration 2 that this binary does not know")
        );
    }

    #[test]
    fn long_version_lists_are_shortened() {
        let embedded: Vec<(i64, &[u8])> = (1..=9).map(|v| (v, A)).collect();
        let applied: Vec<_> = (1..=9).map(|v| (v, false, A.to_vec())).collect();
        let check = evaluate(&embedded, &applied);
        assert_eq!(
            check.summary,
            "migration 1, 2, 3, 4, 5 and 4 more did not complete"
        );
    }

    #[test]
    fn embedded_migrations_are_listed() {
        let embedded: Vec<_> = migrations::embedded().collect();
        assert!(embedded.len() > 100);
        assert!(embedded.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn clock_skew_thresholds() {
        assert_eq!(evaluate_clock_skew(150, 4).status, Status::Ok);
        assert_eq!(evaluate_clock_skew(-2_000, 0).status, Status::Ok);
        // Half the round trip is slack.
        assert_eq!(evaluate_clock_skew(2_400, 1_000).status, Status::Ok);
        let check = evaluate_clock_skew(-3_000, 10);
        assert_eq!(check.status, Status::Warn);
        assert_eq!(
            check.summary,
            "this host's clock is 3000 ms ahead of the database's"
        );
        let check = evaluate_clock_skew(6 * 60 * 1_000, 10);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check
                .summary
                .starts_with("this host's clock is 360000 ms behind")
        );
    }

    #[test]
    fn providers_warn_only_when_none_anywhere() {
        let none = providers_configured(&[], Some(&BTreeMap::new()));
        assert_eq!(none.status, Status::Warn);
        assert_eq!(
            none.summary,
            "none in the environment; no stored credentials"
        );
        let stored = BTreeMap::from([("anthropic".to_owned(), 2)]);
        let check = providers_configured(&[], Some(&stored));
        assert_eq!(check.status, Status::Ok);
        assert_eq!(
            check.summary,
            "none in the environment; stored credentials: anthropic (2)"
        );
        let check = providers_configured(&[("openai", "https://api.openai.com/v1".into())], None);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(
            check.summary,
            "environment: openai; stored credentials unknown"
        );
    }

    #[tokio::test]
    async fn network_checks_are_opt_in() {
        let endpoints = [("openai", "https://api.openai.com/v1".to_owned())];
        let check = providers_network(&endpoints, false, &|_| None).await;
        assert_eq!(check.status, Status::Skip);
        assert!(check.hint.unwrap().contains("--network"));
    }

    #[test]
    fn renumbered_migrations_are_pending_not_changed() {
        // History from before 23 and 24 moved to 26 and 27: rows 23 and 24
        // carry the checksums of today's 26 and 27.
        const C: &[u8] = b"c";
        const D: &[u8] = b"d";
        let embedded = [(22, A), (23, B), (24, C), (26, D), (27, A)];
        let applied = vec![
            (22, true, A.to_vec()),
            (23, true, D.to_vec()),
            (24, true, A.to_vec()),
        ];
        let check = evaluate_migrations(&embedded, &applied, &[(23, 26), (24, 27)]);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.summary.starts_with("2 migration(s) pending"),
            "{}",
            check.summary
        );
        assert!(
            check
                .summary
                .contains("records migration 23, 24 under an earlier number")
        );
        assert!(!check.summary.contains("differs"));
        assert!(check.hint.unwrap().contains("gateway migrate"));
        // Without the renumbering they would read as changed.
        let check = evaluate_migrations(&embedded, &applied, &[]);
        assert!(check.summary.contains("differs"), "{}", check.summary);
        // Once repaired and applied, the history is up to date.
        let repaired = vec![
            (22, true, A.to_vec()),
            (23, true, B.to_vec()),
            (24, true, C.to_vec()),
            (26, true, D.to_vec()),
            (27, true, A.to_vec()),
        ];
        let check = evaluate_migrations(&embedded, &repaired, RENUMBERED_MIGRATIONS);
        assert_eq!(check.status, Status::Ok, "{}", check.summary);
    }

    #[test]
    fn data_plane_grants_name_every_gap() {
        let complete = DataPlaneGrants {
            last_used_at: true,
            ..Default::default()
        };
        let check = evaluate_data_plane_grants(&complete);
        assert_eq!(check.status, Status::Ok);
        assert!(
            check
                .summary
                .contains(&format!("{} functions", DATA_PLANE_FUNCTIONS.len()))
        );
        let gaps = DataPlaneGrants {
            missing_tables: vec!["batch_staging".into()],
            lacking: vec![(
                "quota_bucket".into(),
                vec!["INSERT".into(), "UPDATE".into()],
            )],
            missing_functions: vec![],
            no_execute: vec!["return_rate_limit_lease(UUID, TEXT)".into()],
            last_used_at: false,
        };
        let check = evaluate_data_plane_grants(&gaps);
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.summary,
            "the role missing tables: batch_staging; lacks INSERT/UPDATE on quota_bucket; lacks EXECUTE on return_rate_limit_lease(UUID, TEXT); lacks UPDATE (last_used_at) on api_key"
        );
        assert!(check.hint.unwrap().contains("schema is behind"));
        let only_privileges = DataPlaneGrants {
            no_execute: vec!["f()".into()],
            last_used_at: true,
            ..Default::default()
        };
        let hint = evaluate_data_plane_grants(&only_privileges).hint.unwrap();
        assert!(hint.contains(DATA_PLANE_ROLE), "{hint}");
    }

    #[test]
    fn application_grants_include_delete() {
        let mut grants = ApplicationGrants {
            usage: true,
            create: true,
            lacking: vec![],
            data_plane_role: false,
        };
        let check = evaluate_application_grants(&grants);
        assert_eq!(check.status, Status::Ok);
        assert!(check.summary.contains("deletes"));
        grants.lacking = vec![("outbox_event".into(), vec!["DELETE".into()])];
        let check = evaluate_application_grants(&grants);
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.summary,
            "the role lacks privileges on 1 table(s): DELETE on outbox_event"
        );
        grants.lacking.clear();
        grants.create = false;
        assert_eq!(evaluate_application_grants(&grants).status, Status::Warn);
        grants.usage = false;
        assert_eq!(evaluate_application_grants(&grants).status, Status::Fail);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_check_fails_at_its_deadline() {
        let check = bounded_by(
            "database.grants",
            Duration::from_secs(3),
            std::future::pending::<Check>(),
        )
        .await;
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.summary, "did not finish within 3 s");
        let check = bounded_by("x.y", Duration::from_secs(3), async {
            Check::ok("x.y", "fine")
        })
        .await;
        assert_eq!(check.status, Status::Ok);
    }

    #[tokio::test]
    async fn connect_errors_are_specific_and_never_show_the_url() {
        let error = connect("not a url").await.unwrap_err();
        assert_eq!(error, "DATABASE_URL is not a valid PostgreSQL URL");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let started = Instant::now();
        let error = connect(&format!(
            "postgres://doctor:s3cret@127.0.0.1:{port}/gateway"
        ))
        .await
        .unwrap_err();
        assert!(started.elapsed() < STEP_TIMEOUT, "{:?}", started.elapsed());
        assert!(error.to_lowercase().contains("refused"), "{error}");
        assert!(
            !error.contains("s3cret") && !error.contains("doctor@"),
            "{error}"
        );
    }
}
