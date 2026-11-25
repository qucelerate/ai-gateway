//! `gateway-load`: load and resilience testing of the gateway (D-030).
//!
//! - `fake-provider`: an OpenAI-compatible provider that answers as each
//!   request's directive asks: latency, stream length, output size, and
//!   trouble (429 with `Retry-After`, 5xx, cut streams, timeouts). It must
//!   run on the gateway's host: the gateway sends plain HTTP only to
//!   loopback.
//! - `setup`: seeds a run's organizations, keys, budgets and catalog in the
//!   gateway's database and writes the tenants file.
//! - `run`: the open-loop generator, run from any host that reaches the
//!   gateways; with `--database-url` it also samples the database and,
//!   after the load, checks the accounting. Exits 1 when a gate fails.
//!
//! See `docs/operations.md`, *Load and resilience testing*, and
//! `tests/load/run.sh`, which runs a whole stack on one machine.

mod args;
mod check;
mod directive;
mod fake;
mod generator;
mod hist;
mod report;
mod sampler;
mod setup;

use std::{io::Write, process::ExitCode, time::Duration};

use sqlx::postgres::PgPoolOptions;

use crate::args::Flags;

const USAGE: &str = "usage: gateway-load <fake-provider|setup|run> [--flag value ...]

  fake-provider --listen 127.0.0.1:24599 [--default-latency 300ms]
  setup  --database-url URL --pepper PEPPER [--tenants FILE] [--orgs 1] [--tight-orgs 1]
         [--keys 10] [--key-budgets false] [--end-users true] [--trace-content false]
         [--rate-limits true] [--rate-limit-rpm N] [--rate-limit-concurrent N]
         [--rate-limit-tpm N] [--key-rate-limits false]
         [--reservation-ttl 120] [--run LABEL]
  run    --targets URL[,URL...] --tenants FILE [--profile smoke|nightly|gate-single|gate-spread|soak]
         [--rate N/s] [--duration 10m] [--database-url URL] [--admin-database-url URL]
         [--event '<at>:<label>:<command>' ...] [--pid-file label=PATH ...]
         [--fake-stats URL ...] [--report FILE] [--history FILE] [--gate-<name> LIMIT ...]

Every flag can also be set as LOAD_<NAME> in the environment.";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let flags = match Flags::parse(args) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command.as_str() {
        "fake-provider" => fake::serve(&flags).await.map(|()| true),
        "setup" => setup::run(&flags).await.map(|()| true),
        "run" => run(flags).await,
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(true)
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("gateway-load: {error}");
            ExitCode::from(2)
        }
    }
}

/// `label=path` pairs.
fn pairs(values: Vec<String>) -> Result<Vec<(String, String)>, String> {
    values
        .into_iter()
        .map(|value| {
            value
                .split_once('=')
                .map(|(label, path)| (label.trim().to_owned(), path.trim().to_owned()))
                .ok_or_else(|| format!("expected label=path, got {value:?}"))
        })
        .collect()
}

async fn run(mut flags: Flags) -> Result<bool, String> {
    report::apply_profile(&mut flags)?;
    let tenants = setup::Tenants::read(&flags.string("tenants", "load-tenants.json"))?;
    let plan = generator::Plan::from_flags(&flags)?;
    let orgs = tenants.org_ids();
    let tight: Vec<_> = tenants
        .tenants
        .iter()
        .filter(|tenant| tenant.tight)
        .map(|tenant| tenant.org)
        .collect();

    let database_url = flags.get("database-url");
    let pool = match &database_url {
        Some(url) => Some(
            PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(Duration::from_secs(10))
                .connect(url)
                .await
                .map_err(|error| format!("--database-url: {error}"))?,
        ),
        None => None,
    };
    let admin_url = flags.get("admin-database-url").or(database_url.clone());
    let admin = match &admin_url {
        Some(url) => Some(
            PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_secs(5))
                .connect(url)
                .await
                .map_err(|error| format!("--admin-database-url: {error}"))?,
        ),
        None => None,
    };
    let gateway_url = flags.get("gateway-database-url").or(database_url.clone());
    let parsed = gateway_url
        .as_deref()
        .and_then(|url| url::Url::parse(url).ok());
    let start = std::time::Instant::now();
    let sampler = sampler::Sampler::start(
        sampler::Config {
            admin,
            role: flags
                .get("gateway-role")
                .or_else(|| parsed.as_ref().map(|url| url.username().to_owned())),
            database: parsed
                .as_ref()
                .map(|url| url.path().trim_start_matches('/').to_owned()),
            orgs: orgs.clone(),
            interval: flags.duration("sample-interval", Duration::from_secs(5))?,
            pid_files: pairs(flags.all("pid-file"))?,
        },
        start,
    )
    .await;
    let ttl = tenants.reservation_ttl_seconds;
    let stats = generator::run(plan.clone(), tenants).await?;
    let samples = sampler.finish().await;

    let mut fake_stats = Vec::new();
    for url in flags.all("fake-stats") {
        let value = match reqwest::get(&url).await {
            Ok(response) => response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_default(),
            Err(error) => serde_json::json!({"error": error.to_string()}),
        };
        fake_stats.push(serde_json::json!({"url": url, "stats": value}));
    }

    let accounting = match &pool {
        Some(pool) if flags.parsed("check", true)? => {
            eprintln!("checking accounting (waiting for open reservations to settle or expire)");
            let settle = flags.duration("settle-timeout", Duration::from_secs(ttl + 90))?;
            Some(
                check::check(
                    pool,
                    &orgs,
                    &tight,
                    &stats.audit,
                    stats.due - stats.not_sent,
                    flags.parsed("sole-generator", true)?,
                    settle,
                )
                .await
                .map_err(|error| format!("accounting check: {error}"))?,
            )
        }
        _ => None,
    };

    let (report, gates) = report::build(
        &flags,
        &plan,
        &stats,
        &samples,
        accounting.as_ref(),
        fake_stats,
    );
    report::print_summary(&report, &gates);
    if let Some(path) = flags.get("report") {
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap())
            .map_err(|error| format!("{path}: {error}"))?;
        eprintln!("report written to {path}");
    }
    if let Some(path) = flags.get("history") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("{path}: {error}"))?;
        writeln!(file, "{}", report::history_line(&report))
            .map_err(|error| format!("{path}: {error}"))?;
    }
    Ok(report["pass"] == true)
}
