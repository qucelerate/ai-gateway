//! What the database and the gateway processes do during a run, sampled
//! every few seconds: database sessions (by state and wait), commits, the
//! outbox backlog and its oldest event, open reservations and
//! reconciliation items; each table's dead rows; and each watched
//! process's memory, threads and open files (local processes only, by pid
//! file, so a restarted process is followed). Each table's size and row
//! counters are also read at the start and end of the load, for its growth
//! and autovacuum work.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize)]
pub struct DbSample {
    pub t_s: f64,
    pub sessions: i64,
    pub active: i64,
    pub idle_in_transaction: i64,
    pub lock_waits: i64,
    pub commits_per_s: f64,
    pub outbox_pending: i64,
    pub outbox_oldest_s: i64,
    pub reservations_open: i64,
    pub reconciliation_open: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcessSample {
    pub t_s: f64,
    pub label: String,
    pub pid: u32,
    pub rss_mb: f64,
    pub threads: u64,
    pub open_files: u64,
}

/// A table's size and counters (`pg_stat_user_tables`) at one moment.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TableStat {
    pub table: String,
    pub total_bytes: i64,
    pub heap_bytes: i64,
    pub index_bytes: i64,
    pub live: i64,
    pub dead: i64,
    pub inserted: i64,
    pub updated: i64,
    pub hot_updated: i64,
    pub deleted: i64,
    pub autovacuums: i64,
    pub autoanalyzes: i64,
}

#[derive(Default)]
pub struct Samples {
    pub db: Vec<DbSample>,
    pub processes: Vec<ProcessSample>,
    pub waits: HashMap<String, u64>,
    pub db_errors: u64,
    /// Every table in the `gateway` schema, before and after the load.
    pub tables_start: Vec<TableStat>,
    pub tables_end: Vec<TableStat>,
    /// Each sample's dead share (dead / (live + dead)) of each table:
    /// `table -> [(t_s, share)]`, for tables with at least 1,000 rows.
    pub dead_share: HashMap<String, Vec<(f64, f64)>>,
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Samples>>,
    task: tokio::task::JoinHandle<()>,
    admin: Option<PgPool>,
}

pub struct Config {
    pub admin: Option<PgPool>,
    /// The role and database the gateways connect as.
    pub role: Option<String>,
    pub database: Option<String>,
    pub orgs: Vec<Uuid>,
    pub interval: Duration,
    /// `(label, pid file)`.
    pub pid_files: Vec<(String, String)>,
}

/// Tables of the `gateway` schema, partitions included.
pub async fn table_stats(pool: &PgPool) -> Result<Vec<TableStat>, sqlx::Error> {
    let rows: Vec<(
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    )> = sqlx::query_as(
        "SELECT relname::text,
                    pg_total_relation_size(relid), pg_relation_size(relid),
                    pg_indexes_size(relid),
                    n_live_tup, n_dead_tup, n_tup_ins, n_tup_upd, n_tup_hot_upd,
                    n_tup_del, autovacuum_count, autoanalyze_count
             FROM pg_stat_user_tables WHERE schemaname = 'gateway'
             ORDER BY relname",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                table,
                total_bytes,
                heap_bytes,
                index_bytes,
                live,
                dead,
                inserted,
                updated,
                hot_updated,
                deleted,
                autovacuums,
                autoanalyzes,
            )| TableStat {
                table,
                total_bytes,
                heap_bytes,
                index_bytes,
                live,
                dead,
                inserted,
                updated,
                hot_updated,
                deleted,
                autovacuums,
                autoanalyzes,
            },
        )
        .collect())
}

async fn sample_db(
    pool: &PgPool,
    config: &Config,
    last_commits: &mut Option<(Instant, i64)>,
    t_s: f64,
    samples: &Mutex<Samples>,
) -> Result<(), sqlx::Error> {
    let sessions: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT COALESCE(state, ''), wait_event_type, wait_event FROM pg_stat_activity
         WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()
           AND ($1::text IS NULL OR usename = $1) AND ($2::text IS NULL OR datname = $2)",
    )
    .bind(config.role.as_deref())
    .bind(config.database.as_deref())
    .fetch_all(pool)
    .await?;
    let commits: i64 = sqlx::query_scalar(
        "SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_one(pool)
    .await?;
    let (outbox_pending, outbox_oldest_s): (i64, i64) = sqlx::query_as(
        "SELECT count(*),
                COALESCE(floor(EXTRACT(EPOCH FROM now() - min(occurred_at)))::BIGINT, 0)
         FROM gateway.outbox_event WHERE dispatched_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    let (reservations_open, reconciliation_open): (i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM gateway.accounting_reservation
              WHERE state IN ('reserved', 'claimed') AND organization_id = ANY($1)),
             (SELECT count(*) FROM gateway.accounting_reconciliation
              WHERE status = 'open' AND organization_id = ANY($1))",
    )
    .bind(&config.orgs)
    .fetch_one(pool)
    .await?;
    let dead: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT relname::text, n_live_tup, n_dead_tup FROM pg_stat_user_tables
         WHERE schemaname = 'gateway' AND n_live_tup + n_dead_tup >= 1000",
    )
    .fetch_all(pool)
    .await?;
    let now = Instant::now();
    let commits_per_s = match last_commits.replace((now, commits)) {
        Some((then, before)) => (commits - before) as f64 / now.duration_since(then).as_secs_f64(),
        None => 0.0,
    };
    let mut sample = DbSample {
        t_s,
        sessions: sessions.len() as i64,
        commits_per_s,
        outbox_pending,
        outbox_oldest_s,
        reservations_open,
        reconciliation_open,
        ..DbSample::default()
    };
    let mut samples = samples.lock().unwrap();
    for (table, live, dead) in dead {
        samples
            .dead_share
            .entry(table)
            .or_default()
            .push((t_s, dead as f64 / (live + dead).max(1) as f64));
    }
    for (state, kind, event) in sessions {
        match state.as_str() {
            "active" => sample.active += 1,
            "idle in transaction" => sample.idle_in_transaction += 1,
            _ => {}
        }
        if state == "idle" {
            continue;
        }
        if let (Some(kind), Some(event)) = (kind, event) {
            if kind == "Lock" {
                sample.lock_waits += 1;
            }
            if kind != "Client" && kind != "Activity" {
                *samples.waits.entry(format!("{kind}:{event}")).or_default() += 1;
            }
        }
    }
    samples.db.push(sample);
    Ok(())
}

fn status_field(status: &str, name: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

fn sample_process(label: &str, pid_file: &str, t_s: f64) -> Option<ProcessSample> {
    let pid: u32 = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let open_files = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|entries| entries.count() as u64)
        .unwrap_or(0);
    Some(ProcessSample {
        t_s,
        label: label.to_owned(),
        pid,
        rss_mb: status_field(&status, "VmRSS:").unwrap_or(0) as f64 / 1024.0,
        threads: status_field(&status, "Threads:").unwrap_or(0),
        open_files,
    })
}

impl Sampler {
    /// Starts sampling. With a database, the tables are read first, so the
    /// load is measured from here.
    pub async fn start(config: Config, start: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut first = Samples::default();
        let admin = config.admin.clone();
        if let Some(pool) = &admin {
            match table_stats(pool).await {
                Ok(tables) => first.tables_start = tables,
                Err(_) => first.db_errors += 1,
            }
        }
        let samples = Arc::new(Mutex::new(first));
        let task = {
            let (stop, samples) = (stop.clone(), samples.clone());
            tokio::spawn(async move {
                let mut last_commits = None;
                while !stop.load(Ordering::Relaxed) {
                    let t_s = start.elapsed().as_secs_f64();
                    if let Some(pool) = &config.admin {
                        if sample_db(pool, &config, &mut last_commits, t_s, &samples)
                            .await
                            .is_err()
                        {
                            // The database may be down on purpose (a
                            // restart under load): note it and go on.
                            samples.lock().unwrap().db_errors += 1;
                            last_commits = None;
                        }
                    }
                    for (label, pid_file) in &config.pid_files {
                        if let Some(sample) = sample_process(label, pid_file, t_s) {
                            samples.lock().unwrap().processes.push(sample);
                        }
                    }
                    tokio::time::sleep(config.interval).await;
                }
            })
        };
        Self {
            stop,
            samples,
            task,
            admin,
        }
    }

    /// Stops sampling and reads the tables again, before the accounting
    /// check waits for the worker, so the growth is the load's.
    pub async fn finish(self) -> Samples {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.await;
        let mut samples = Arc::into_inner(self.samples)
            .map(|samples| samples.into_inner().unwrap())
            .unwrap_or_default();
        if let Some(pool) = &self.admin {
            match table_stats(pool).await {
                Ok(tables) => samples.tables_end = tables,
                Err(_) => samples.db_errors += 1,
            }
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_fields_parse() {
        let status = "Name:\tgateway\nVmRSS:\t  123456 kB\nThreads:\t17\n";
        assert_eq!(super::status_field(status, "VmRSS:"), Some(123_456));
        assert_eq!(super::status_field(status, "Threads:"), Some(17));
        assert_eq!(super::status_field(status, "VmSwap:"), None);
    }
}
