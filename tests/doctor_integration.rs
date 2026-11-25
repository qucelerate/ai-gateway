//! `gateway doctor` against a real database: a migrated database passes the
//! database checks, and an unreachable one fails without printing the URL's
//! password.
//!
//! The database tests need `GATEWAY_TEST_DATABASE_URL` (the application
//! role); the data-plane test also `GATEWAY_TEST_DATA_PLANE_DATABASE_URL`
//! (the `gateway_data_plane` role on the same database), and is skipped
//! without it.

use std::time::{Duration, Instant};

use gateway::doctor::{Options, Status, connect, diagnose};
use sqlx::postgres::PgPoolOptions;

const PEPPER: &str = "doctor-test-pepper-0000000000000000";

fn database_url() -> Option<String> {
    match std::env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_)
            if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() =>
        {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => None,
    }
}

fn lookup(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    }
}

#[tokio::test]
async fn a_migrated_database_passes_the_database_checks() {
    let Some(url) = database_url() else {
        eprintln!("skipping: GATEWAY_TEST_DATABASE_URL is not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    pool.close().await;

    let vars = lookup(vec![
        ("DATABASE_URL", url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
    ]);
    let report = diagnose(&vars, Options::default()).await;
    let check = |id: &str| {
        report
            .checks
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("no {id} check"))
    };
    for id in [
        "config.required",
        "config.load",
        "database.connect",
        "database.migrations",
    ] {
        assert_eq!(check(id).status, Status::Ok, "{id}: {:?}", check(id));
    }
    // The application role owns the schema: every privilege, DELETE too.
    assert_eq!(
        check("database.grants").status,
        Status::Ok,
        "{:?}",
        check("database.grants")
    );
    assert!(check("database.grants").summary.contains("deletes"));
    assert_ne!(check("database.clock_skew").status, Status::Fail);
    assert_eq!(check("credentials.encryption_key").status, Status::Warn);
    assert_eq!(check("providers.network").status, Status::Skip);
    assert_eq!(check("trace_store.reachable").status, Status::Skip);
    assert_eq!(check("clickhouse.reachable").status, Status::Skip);
    assert!(
        check("database.migrations")
            .summary
            .starts_with("up to date")
    );
    assert_eq!(report.exit_code(), 0, "{}", report.to_text());

    let text = report.to_text() + &report.to_json();
    assert!(!text.contains(PEPPER));
    if let Some(password) = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.password().map(str::to_owned))
    {
        assert!(
            !text.contains(&password),
            "the report shows the database password"
        );
    }
}

#[tokio::test]
async fn an_unreachable_database_fails_without_showing_the_url() {
    let vars = lookup(vec![
        (
            "DATABASE_URL",
            "postgres://gateway:doctor-s3cret@127.0.0.1:1/gateway".to_owned(),
        ),
        ("GATEWAY_PROFILE", "worker".to_owned()),
    ]);
    let started = Instant::now();
    let report = diagnose(&vars, Options::default()).await;
    // The real error, at once: not a pool timeout seconds later.
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    let status = |id: &str| report.checks.iter().find(|c| c.id == id).unwrap().status;
    let connect_check = report
        .checks
        .iter()
        .find(|c| c.id == "database.connect")
        .unwrap();
    assert!(
        connect_check.summary.to_lowercase().contains("refused"),
        "{}",
        connect_check.summary
    );
    assert_eq!(status("config.load"), Status::Ok);
    assert_eq!(status("database.connect"), Status::Fail);
    assert_eq!(status("database.migrations"), Status::Skip);
    assert_eq!(status("database.clock_skew"), Status::Skip);
    assert_eq!(report.status, Status::Fail);
    assert_eq!(report.exit_code(), 1);
    let text = report.to_text() + &report.to_json();
    assert!(!text.contains("doctor-s3cret"), "{text}");
}

#[tokio::test]
async fn the_doctor_session_is_read_only_and_bounded() {
    let Some(url) = database_url() else {
        eprintln!("skipping: GATEWAY_TEST_DATABASE_URL is not set");
        return;
    };
    let mut connection = connect(&url).await.unwrap();
    let setting = |name: &'static str| format!("SELECT current_setting('{name}')");
    let mut show = async |name: &'static str| -> String {
        sqlx::query_scalar(&setting(name))
            .fetch_one(&mut connection)
            .await
            .unwrap()
    };
    assert_eq!(show("default_transaction_read_only").await, "on");
    assert_eq!(show("statement_timeout").await, "5s");
    assert_eq!(show("lock_timeout").await, "2s");
    let refused = sqlx::query("CREATE TEMPORARY TABLE doctor_write_probe (x int)")
        .execute(&mut connection)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("read-only transaction"),
        "{refused}"
    );
    let slow = sqlx::query("SELECT pg_sleep(10)")
        .execute(&mut connection)
        .await
        .unwrap_err();
    assert!(slow.to_string().contains("statement timeout"), "{slow}");
}

#[tokio::test]
async fn the_data_plane_role_passes_its_grant_check() {
    let (Some(url), Ok(data_plane)) = (
        database_url(),
        std::env::var("GATEWAY_TEST_DATA_PLANE_DATABASE_URL"),
    ) else {
        eprintln!("skipping: GATEWAY_TEST_DATA_PLANE_DATABASE_URL is not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    // Migrating grants the data plane its privileges.
    gateway::persistence::migrations::run(&pool).await.unwrap();
    pool.close().await;
    let vars = lookup(vec![
        ("DATABASE_URL", data_plane),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
    ]);
    let report = diagnose(&vars, Options::default()).await;
    let check = |id: &str| report.checks.iter().find(|c| c.id == id).unwrap();
    assert_eq!(check("database.connect").status, Status::Ok);
    let grants = check("database.grants");
    assert_eq!(grants.status, Status::Ok, "{grants:?}");
    assert!(
        grants
            .summary
            .contains("functions and api_key.last_used_at")
    );
    // The data plane cannot read the migration history: skipped, not failed.
    assert_ne!(check("database.migrations").status, Status::Fail);
}
