//! Operational metrics, served in the Prometheus text format on a separate
//! listener (`GATEWAY_METRICS_ADDR`).
//!
//! Every family is declared here with a fixed label set, and every label
//! value comes from a closed set: an operation, a provider family, an outcome
//! or problem code, a route template, a status class. Organization, key and
//! deployment identifiers, model names, prompts, URLs and provider text are
//! never labels, so the series count stays bounded and a scrape reveals
//! nothing about any tenant.
//!
//! Series live for the life of the process. Counters only grow; a restart
//! resets them, as Prometheus expects.

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use sqlx::PgPool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// A metric family: its name, help text, type and label names.
#[derive(Debug)]
pub struct Family {
    name: &'static str,
    help: &'static str,
    kind: Kind,
    labels: &'static [&'static str],
}

const fn counter(
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
) -> Family {
    Family {
        name,
        help,
        kind: Kind::Counter,
        labels,
    }
}

const fn gauge(name: &'static str, help: &'static str, labels: &'static [&'static str]) -> Family {
    Family {
        name,
        help,
        kind: Kind::Gauge,
        labels,
    }
}

const fn histogram(
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
) -> Family {
    Family {
        name,
        help,
        kind: Kind::Histogram,
        labels,
    }
}

pub static BUILD_INFO: Family = gauge(
    "gateway_build_info",
    "Always 1; the version label is the running build.",
    &["version"],
);
pub static PROCESS_START: Family = gauge(
    "gateway_process_start_time_seconds",
    "Unix time the process started.",
    &[],
);
pub static HTTP_REQUESTS: Family = counter(
    "gateway_http_requests_total",
    "HTTP requests by route template, method and status class.",
    &["route", "method", "status"],
);
pub static HTTP_DURATION: Family = histogram(
    "gateway_http_request_duration_seconds",
    "Time to the response head by route template; for streams this excludes the body, \
     see gateway_provider_call_duration_seconds.",
    &["route"],
);
pub static HTTP_VERSIONS: Family = counter(
    "gateway_http_requests_by_version_total",
    "HTTP requests by protocol version: HTTP/1.1, HTTP/2 or HTTP/3.",
    &["version"],
);
pub static INFERENCE_REQUESTS: Family = counter(
    "gateway_inference_requests_total",
    "Inference requests by operation and outcome: ok, or the problem code returned. A \
     started stream is counted when it ends: ok, its error code, incomplete or abandoned.",
    &["operation", "outcome"],
);
pub static PROVIDER_CALLS: Family = counter(
    "gateway_provider_calls_total",
    "Provider calls by provider family, operation and outcome: ok, an error category, \
     incomplete (a stream ended early) or abandoned (the client left mid-stream).",
    &["provider_family", "operation", "outcome"],
);
pub static PROVIDER_DURATION: Family = histogram(
    "gateway_provider_call_duration_seconds",
    "Provider call duration by provider family and operation; streams until their end.",
    &["provider_family", "operation"],
);
pub static PROVIDER_RETRIES: Family = counter(
    "gateway_provider_retries_total",
    "Provider calls tried again by the gateway, by provider family, operation and reason: \
     not_sent (the connection never opened) or rate_limited (the provider refused with 429).",
    &["provider_family", "operation", "reason"],
);
pub static ROUTE_SELECTIONS: Family = counter(
    "gateway_route_selections_total",
    "Deployments considered by routes of more than one deployment (D-023), by provider \
     family and outcome: picked, skipped_circuit_open, skipped_rate_limited, \
     skipped_unavailable (its evidence is refused or requires action), picked_unhealthy \
     (no deployment was healthy, so the preferred one was tried anyway), or \
     skipped_policy (the organization's routing policy does not permit it; counted for \
     routes of any size).",
    &["provider_family", "outcome"],
);
pub static ROUTE_FAILOVERS: Family = counter(
    "gateway_route_failovers_total",
    "Requests failed over to another deployment of their route (D-023), by the \
     provider family of the deployment that failed and why: not_sent (the \
     connection never opened), rate_limited (429), refused (a status the \
     adapter documents as refused before inference) or circuit_open.",
    &["provider_family", "reason"],
);
pub static MODEL_EVIDENCE: Family = counter(
    "gateway_model_evidence_total",
    "Model availability observations from traffic, by outcome: written, failed (the \
     write failed) or dropped (the writer's queue was full).",
    &["outcome"],
);
pub static MODEL_PROBES: Family = counter(
    "gateway_model_probes_total",
    "Discovery's probes of listed models, by outcome: verified, refused, \
     inconclusive (the call showed nothing about the model) or failed (the \
     result could not be recorded).",
    &["outcome"],
);
pub static CIRCUIT_TRANSITIONS: Family = counter(
    "gateway_provider_circuit_transitions_total",
    "Provider circuit changes: opened, closed (recovered), or reopened (a probe failed).",
    &["transition"],
);
pub static SETTLEMENTS: Family = counter(
    "gateway_settlements_total",
    "Finalized reservations by outcome: settled (usage known), unknown (usage not \
     reported), refused (released: the provider refused before any work), \
     bound_breach, input_count_drift.",
    &["outcome"],
);
pub static SETTLEMENT_RETRIES: Family = counter(
    "gateway_settlement_retries_total",
    "Background settlement retries: scheduled, succeeded, abandoned, or not_scheduled \
     (too many pending).",
    &["outcome"],
);
pub static BUDGET_LEASES: Family = counter(
    "gateway_budget_leases_total",
    "Budget leases (GATEWAY_BUDGET_LEASES) taken, and returned by reason: idle, drained, \
     shutdown, expired (by the worker or near expiry), limit, operator.",
    &["event", "reason"],
);
pub static BUDGET_LEASE_MINOR: Family = counter(
    "gateway_budget_lease_minor_total",
    "Minor units leased, and used (held or charged by requests) when leases were returned; \
     used over leased is lease utilisation.",
    &["kind"],
);
pub static BUDGET_LEASE_RESERVATIONS: Family = counter(
    "gateway_budget_lease_reservations_total",
    "Budget holds of new reservations, by path: lease (in memory, under a lease) or bucket \
     (the budget's row locked, as without leases).",
    &["path"],
);
pub static RATE_LIMIT_LEASES: Family = counter(
    "gateway_rate_limit_leases_total",
    "Rate-limit leases (GATEWAY_RATE_LIMIT_LEASES) taken, and returned by reason: ended, \
     shutdown, expired (by the worker).",
    &["event", "reason"],
);
pub static RATE_LIMIT_LEASE_ADMISSIONS: Family = counter(
    "gateway_rate_limit_lease_admissions_total",
    "Admissions leased, and used (reservations made under them) when leases were returned; \
     used over leased is lease utilisation.",
    &["kind"],
);
pub static RATE_LIMIT_ADMISSIONS: Family = counter(
    "gateway_rate_limit_admissions_total",
    "Rate-limit admissions of new reservations with leases on, by path: lease (in memory, \
     under a lease) or policy (the policies' rows locked, as without leases).",
    &["path"],
);
pub static CREDENTIAL_SECRETS_RESEALED: Family = counter(
    "gateway_credential_secrets_resealed_total",
    "Stored secrets re-sealed under the active data key, by kind (provider_credential, \
     webhook_secret).",
    &["kind"],
);
pub static CREDENTIAL_SECRETS_PENDING: Family = gauge(
    "gateway_credential_secrets_pending_reseal",
    "Stored secrets not yet sealed under the active data key (sampled by the worker).",
    &[],
);
pub static SETTLEMENT_RETRIES_PENDING: Family = gauge(
    "gateway_settlement_retries_pending",
    "Settlements this process is still retrying in the background.",
    &[],
);
pub static DB_POOL_CONNECTIONS: Family = gauge(
    "gateway_db_pool_connections",
    "Database pool connections by pool and state (idle, in_use, max).",
    &["pool", "state"],
);
pub static WORKER_RUNS: Family = counter(
    "gateway_worker_task_runs_total",
    "Worker task runs by task and outcome (ok, error).",
    &["task", "outcome"],
);
pub static RESERVATIONS_RECOVERED: Family = counter(
    "gateway_accounting_reservations_recovered_total",
    "Expired reservations released or reconciled by recovery.",
    &[],
);
pub static OUTBOX_EVENTS: Family = counter(
    "gateway_outbox_events_total",
    "Outbox events handed to consumers (dispatched) or left for retry (failed).",
    &["outcome"],
);
pub static WEBHOOK_ATTEMPTS: Family = counter(
    "gateway_webhook_delivery_attempts_total",
    "Webhook delivery attempts by outcome (delivered, failed, parked).",
    &["outcome"],
);
pub static USAGE_EXPORT_OBJECTS: Family = counter(
    "gateway_usage_export_objects_total",
    "Usage export objects written to organizations' storage, by outcome (written, failed).",
    &["outcome"],
);
pub static TRACE_CONTENT: Family = counter(
    "gateway_trace_content_total",
    "Traced requests, by what became of their content (stored, off, unavailable, failed, dropped).",
    &["state"],
);
pub static USAGE_EXPORT_RECORDS: Family = counter(
    "gateway_usage_export_records_total",
    "Usage records written to organizations' storage.",
    &[],
);
pub static DISCOVERY_JOBS: Family = counter(
    "gateway_model_discovery_jobs_total",
    "Model discovery jobs finished, by outcome (succeeded, failed).",
    &["outcome"],
);
pub static PRICE_FETCHES: Family = counter(
    "gateway_price_source_fetches_total",
    "Price source fetches by outcome (changed, unchanged, failed).",
    &["outcome"],
);
pub static PRICES_ADOPTED: Family = counter(
    "gateway_catalog_prices_adopted_total",
    "Published prices the worker adopted into a new release (D-024).",
    &[],
);
pub static SWEPT_ROWS: Family = counter(
    "gateway_retention_swept_rows_total",
    "Rows removed by the retention sweep, by table.",
    &["table"],
);
pub static OUTBOX_PENDING: Family = gauge(
    "gateway_outbox_pending_events",
    "Outbox events not yet dispatched (sampled by the worker).",
    &[],
);
pub static OUTBOX_OLDEST: Family = gauge(
    "gateway_outbox_oldest_pending_age_seconds",
    "Age of the oldest undispatched outbox event; 0 when none (sampled by the worker).",
    &[],
);
pub static WEBHOOKS_PENDING: Family = gauge(
    "gateway_webhook_deliveries_pending",
    "Webhook deliveries awaiting an attempt (sampled by the worker).",
    &[],
);
pub static RECONCILIATION_OPEN: Family = gauge(
    "gateway_reconciliation_items_open",
    "Open reconciliation items across all organizations (sampled by the worker).",
    &[],
);
pub static RESERVATIONS_OVERDUE: Family = gauge(
    "gateway_accounting_reservations_overdue",
    "Reservations past expiry that recovery has not yet processed (sampled by the worker).",
    &[],
);
pub static USAGE_EXPORT_QUEUED: Family = gauge(
    "gateway_usage_export_records_queued",
    "Usage records waiting to be written to organizations' storage (sampled by the worker).",
    &[],
);
pub static USAGE_EXPORTS_FAILING: Family = gauge(
    "gateway_usage_exports_failing",
    "Usage exports whose writes have failed for an hour (sampled by the worker).",
    &[],
);
pub static CONFIG_APPLIED_SEQUENCE: Family = gauge(
    "gateway_config_applied_sequence",
    "The configuration feed position this process has applied.",
    &[],
);
pub static CONFIG_AGE: Family = gauge(
    "gateway_config_age_seconds",
    "Seconds since this process last read the configuration feed.",
    &[],
);
pub static CONFIG_RELOADS: Family = counter(
    "gateway_config_reloads_total",
    "Full configuration loads, by reason (start, entry_lost).",
    &["reason"],
);
pub static MODEL_CARD_MATCH_SYNCS: Family = counter(
    "gateway_model_card_match_syncs_total",
    "Writes of the model cards' exact matches usage is stamped with, by outcome (ok, error).",
    &["outcome"],
);
pub static MODEL_AGREEMENTS: Family = counter(
    "gateway_model_agreements_total",
    "Model agreements administrators asked the gateway to accept (D-035), by outcome (accepted, already_accepted, unconfirmed, failed).",
    &["outcome"],
);
pub static MODEL_AGREEMENT_CHECKS: Family = counter(
    "gateway_model_agreement_checks_total",
    "The worker's checks of accepted model agreements, by the status each left (pending, available, verified, refused, failed, error).",
    &["status"],
);
pub static PRICE_SOURCES_FAILING: Family = gauge(
    "gateway_price_sources_failing",
    "Active price sources whose latest fetch failed (sampled by the worker).",
    &[],
);

pub static ANALYTICS_SINK_ROWS: Family = counter(
    "gateway_analytics_sink_rows_total",
    "Requests written to ClickHouse by the analytics sink, by kind (written, deleted, backfilled).",
    &["kind"],
);
pub static ANALYTICS_SINK_ERRORS: Family = counter(
    "gateway_analytics_sink_errors_total",
    "Failed analytics sink steps, by stage (write, backfill, schema, check).",
    &["stage"],
);
pub static ANALYTICS_SINK_QUEUED: Family = gauge(
    "gateway_analytics_sink_queue_entries",
    "Analytics sink queue entries waiting to be written to ClickHouse (a request queued twice counts twice until written).",
    &[],
);
pub static ANALYTICS_SINK_LAG: Family = gauge(
    "gateway_analytics_sink_lag_seconds",
    "Age of the oldest analytics sink queue entry; 0 when none.",
    &[],
);
pub static ANALYTICS_SINK_PHASE: Family = gauge(
    "gateway_analytics_sink_phase",
    "1 for the analytics sink's phase (off, starting, backfilling, live), 0 for the others.",
    &["phase"],
);
pub static ANALYTICS_READS: Family = counter(
    "gateway_analytics_reads_total",
    "Usage and trace reads with the ClickHouse sink configured, by the store that answered (clickhouse, postgres) and why (ok, not_live, behind, other_store, error).",
    &["store", "reason"],
);

static FAMILIES: [&Family; 57] = [
    &BUILD_INFO,
    &PROCESS_START,
    &HTTP_REQUESTS,
    &HTTP_DURATION,
    &HTTP_VERSIONS,
    &INFERENCE_REQUESTS,
    &PROVIDER_CALLS,
    &PROVIDER_DURATION,
    &PROVIDER_RETRIES,
    &CIRCUIT_TRANSITIONS,
    &ROUTE_SELECTIONS,
    &ROUTE_FAILOVERS,
    &MODEL_EVIDENCE,
    &MODEL_PROBES,
    &SETTLEMENTS,
    &SETTLEMENT_RETRIES,
    &SETTLEMENT_RETRIES_PENDING,
    &BUDGET_LEASES,
    &BUDGET_LEASE_MINOR,
    &BUDGET_LEASE_RESERVATIONS,
    &RATE_LIMIT_LEASES,
    &RATE_LIMIT_LEASE_ADMISSIONS,
    &RATE_LIMIT_ADMISSIONS,
    &CREDENTIAL_SECRETS_RESEALED,
    &CREDENTIAL_SECRETS_PENDING,
    &DB_POOL_CONNECTIONS,
    &WORKER_RUNS,
    &RESERVATIONS_RECOVERED,
    &OUTBOX_EVENTS,
    &WEBHOOK_ATTEMPTS,
    &USAGE_EXPORT_OBJECTS,
    &TRACE_CONTENT,
    &USAGE_EXPORT_RECORDS,
    &DISCOVERY_JOBS,
    &PRICE_FETCHES,
    &PRICES_ADOPTED,
    &SWEPT_ROWS,
    &OUTBOX_PENDING,
    &OUTBOX_OLDEST,
    &WEBHOOKS_PENDING,
    &RECONCILIATION_OPEN,
    &RESERVATIONS_OVERDUE,
    &PRICE_SOURCES_FAILING,
    &USAGE_EXPORT_QUEUED,
    &USAGE_EXPORTS_FAILING,
    &CONFIG_APPLIED_SEQUENCE,
    &CONFIG_AGE,
    &CONFIG_RELOADS,
    &MODEL_CARD_MATCH_SYNCS,
    &MODEL_AGREEMENTS,
    &MODEL_AGREEMENT_CHECKS,
    &ANALYTICS_SINK_ROWS,
    &ANALYTICS_SINK_ERRORS,
    &ANALYTICS_SINK_QUEUED,
    &ANALYTICS_SINK_LAG,
    &ANALYTICS_SINK_PHASE,
    &ANALYTICS_READS,
];

/// Upper bounds of the duration buckets, in seconds. Provider calls run up
/// to minutes, so the range is wide.
const BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

enum Series {
    Counter(AtomicU64),
    Gauge(AtomicI64),
    Histogram {
        buckets: [AtomicU64; BUCKETS.len()],
        count: AtomicU64,
        sum_micros: AtomicU64,
    },
}

type SeriesKey = (&'static str, Box<[Box<str>]>);

/// The process's metrics.
pub struct Registry {
    series: RwLock<HashMap<SeriesKey, Arc<Series>>>,
    pools: Mutex<Vec<(&'static str, PgPool)>>,
    started: f64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            series: RwLock::new(HashMap::new()),
            pools: Mutex::new(Vec::new()),
            started: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |elapsed| elapsed.as_secs_f64()),
        }
    }

    fn series(&self, family: &'static Family, labels: &[&str]) -> Arc<Series> {
        debug_assert_eq!(labels.len(), family.labels.len(), "{}", family.name);
        let key: SeriesKey = (
            family.name,
            labels.iter().map(|&value| value.into()).collect(),
        );
        if let Some(series) = self
            .series
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
        {
            return series.clone();
        }
        self.series
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key)
            .or_insert_with(|| {
                Arc::new(match family.kind {
                    Kind::Counter => Series::Counter(AtomicU64::new(0)),
                    Kind::Gauge => Series::Gauge(AtomicI64::new(0)),
                    Kind::Histogram => Series::Histogram {
                        buckets: std::array::from_fn(|_| AtomicU64::new(0)),
                        count: AtomicU64::new(0),
                        sum_micros: AtomicU64::new(0),
                    },
                })
            })
            .clone()
    }

    /// Adds `by` to a counter.
    pub fn add(&self, family: &'static Family, labels: &[&str], by: u64) {
        if let Series::Counter(value) = &*self.series(family, labels) {
            value.fetch_add(by, Ordering::Relaxed);
        }
    }

    pub fn inc(&self, family: &'static Family, labels: &[&str]) {
        self.add(family, labels, 1);
    }

    pub fn set(&self, family: &'static Family, labels: &[&str], to: i64) {
        if let Series::Gauge(value) = &*self.series(family, labels) {
            value.store(to, Ordering::Relaxed);
        }
    }

    pub fn observe(&self, family: &'static Family, labels: &[&str], elapsed: Duration) {
        if let Series::Histogram {
            buckets,
            count,
            sum_micros,
        } = &*self.series(family, labels)
        {
            let seconds = elapsed.as_secs_f64();
            if let Some(bucket) = BUCKETS.iter().position(|&bound| seconds <= bound) {
                buckets[bucket].fetch_add(1, Ordering::Relaxed);
            }
            count.fetch_add(1, Ordering::Relaxed);
            sum_micros.fetch_add(
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    /// Reports `pool`'s connections under `name`, replacing any pool
    /// registered under that name.
    pub fn register_pool(&self, name: &'static str, pool: PgPool) {
        let mut pools = self
            .pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pools.retain(|(existing, _)| *existing != name);
        pools.push((name, pool));
    }

    /// The current value of a counter or gauge, for tests and diagnostics.
    pub fn value(&self, family: &'static Family, labels: &[&str]) -> Option<i64> {
        let key: SeriesKey = (
            family.name,
            labels.iter().map(|&value| value.into()).collect(),
        );
        let series = self
            .series
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()?;
        match &*series {
            Series::Counter(value) => i64::try_from(value.load(Ordering::Relaxed)).ok(),
            Series::Gauge(value) => Some(value.load(Ordering::Relaxed)),
            Series::Histogram { count, .. } => i64::try_from(count.load(Ordering::Relaxed)).ok(),
        }
    }

    /// Every family in the Prometheus text exposition format, version
    /// 0.0.4.
    pub fn render(&self) -> String {
        self.set(&BUILD_INFO, &[env!("CARGO_PKG_VERSION")], 1);
        if let Some(age) = crate::application::config_cache::age_seconds() {
            self.set(&CONFIG_AGE, &[], age);
        }
        self.set(
            &SETTLEMENT_RETRIES_PENDING,
            &[],
            i64::try_from(crate::application::settlement_retry::pending()).unwrap_or(i64::MAX),
        );
        let pools = self
            .pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for (name, pool) in pools {
            let size = i64::from(pool.size());
            let idle = i64::try_from(pool.num_idle()).unwrap_or(i64::MAX);
            let max = i64::from(pool.options().get_max_connections());
            self.set(&DB_POOL_CONNECTIONS, &[name, "idle"], idle);
            self.set(
                &DB_POOL_CONNECTIONS,
                &[name, "in_use"],
                (size - idle).max(0),
            );
            self.set(&DB_POOL_CONNECTIONS, &[name, "max"], max);
        }

        let series = self
            .series
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = String::new();
        // Every family is described, with or without series.
        for family in FAMILIES {
            let _ = writeln!(out, "# HELP {} {}", family.name, family.help);
            let _ = writeln!(out, "# TYPE {} {}", family.name, family.kind.as_str());
            if family.name == PROCESS_START.name {
                let _ = writeln!(out, "{} {}", family.name, self.started);
                continue;
            }
            let mut members: Vec<_> = series
                .iter()
                .filter(|((name, _), _)| *name == family.name)
                .collect();
            members.sort_by(|((_, a), _), ((_, b), _)| a.cmp(b));
            for ((_, values), series) in members {
                let labels = label_set(family.labels, values);
                match &**series {
                    Series::Counter(value) => {
                        let _ = writeln!(
                            out,
                            "{}{} {}",
                            family.name,
                            braces(&labels),
                            value.load(Ordering::Relaxed)
                        );
                    }
                    Series::Gauge(value) => {
                        let _ = writeln!(
                            out,
                            "{}{} {}",
                            family.name,
                            braces(&labels),
                            value.load(Ordering::Relaxed)
                        );
                    }
                    Series::Histogram {
                        buckets,
                        count,
                        sum_micros,
                    } => {
                        let mut cumulative = 0;
                        for (bound, bucket) in BUCKETS.iter().zip(buckets) {
                            cumulative += bucket.load(Ordering::Relaxed);
                            let _ = writeln!(
                                out,
                                "{}_bucket{} {cumulative}",
                                family.name,
                                braces(&with_le(&labels, &bound.to_string()))
                            );
                        }
                        let count = count.load(Ordering::Relaxed);
                        let _ = writeln!(
                            out,
                            "{}_bucket{} {count}",
                            family.name,
                            braces(&with_le(&labels, "+Inf"))
                        );
                        let _ = writeln!(
                            out,
                            "{}_sum{} {}",
                            family.name,
                            braces(&labels),
                            sum_micros.load(Ordering::Relaxed) as f64 / 1e6
                        );
                        let _ = writeln!(out, "{}_count{} {count}", family.name, braces(&labels));
                    }
                }
            }
        }
        out
    }
}

fn label_set(names: &[&str], values: &[Box<str>]) -> String {
    names
        .iter()
        .zip(values)
        .map(|(name, value)| format!("{name}=\"{}\"", escape(value)))
        .collect::<Vec<_>>()
        .join(",")
}

fn with_le(labels: &str, le: &str) -> String {
    if labels.is_empty() {
        format!("le=\"{le}\"")
    } else {
        format!("{labels},le=\"{le}\"")
    }
}

fn braces(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{labels}}}")
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// The process-wide registry.
pub fn global() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(Registry::new)
}

/// A provider family as a label value: only families the catalog knows,
/// anything else `unknown`.
pub fn provider_family_label(family: Option<&str>) -> &'static str {
    use crate::domain::catalog::ProviderFamily;
    family
        .and_then(|family| {
            ProviderFamily::ALL
                .into_iter()
                .find(|known| known.as_str() == family)
        })
        .map_or("unknown", ProviderFamily::as_str)
}

/// The problem code an inference response carries, for the outcome label.
#[derive(Clone, Copy, Debug)]
pub struct ProblemCode(pub &'static str);

/// `GET /metrics` on the metrics listener.
pub fn router() -> Router {
    Router::new().route("/metrics", get(scrape))
}

async fn scrape() -> Response {
    let mut response = global().render().into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_gauges_and_histograms() {
        let registry = Registry::new();
        registry.inc(&WORKER_RUNS, &["price_sync", "ok"]);
        registry.add(&WORKER_RUNS, &["price_sync", "ok"], 2);
        registry.inc(&WORKER_RUNS, &["accounting_recovery", "error"]);
        registry.set(&OUTBOX_PENDING, &[], 7);
        registry.observe(
            &PROVIDER_DURATION,
            &["openai", "chat"],
            Duration::from_millis(30),
        );
        registry.observe(
            &PROVIDER_DURATION,
            &["openai", "chat"],
            Duration::from_secs(200),
        );
        let text = registry.render();
        assert!(text.contains("# TYPE gateway_worker_task_runs_total counter\n"));
        // Series are sorted by label values.
        assert!(text.contains(
            "gateway_worker_task_runs_total{task=\"accounting_recovery\",outcome=\"error\"} 1\n\
             gateway_worker_task_runs_total{task=\"price_sync\",outcome=\"ok\"} 3\n"
        ));
        assert!(text.contains("gateway_outbox_pending_events 7\n"));
        assert!(text.contains(&format!(
            "gateway_build_info{{version=\"{}\"}} 1\n",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(text.contains("gateway_settlement_retries_pending "));
        let histogram = "gateway_provider_call_duration_seconds";
        let labels = "provider_family=\"openai\",operation=\"chat\"";
        assert!(text.contains(&format!("{histogram}_bucket{{{labels},le=\"0.025\"}} 0\n")));
        assert!(text.contains(&format!("{histogram}_bucket{{{labels},le=\"0.05\"}} 1\n")));
        assert!(text.contains(&format!("{histogram}_bucket{{{labels},le=\"120\"}} 1\n")));
        assert!(text.contains(&format!("{histogram}_bucket{{{labels},le=\"+Inf\"}} 2\n")));
        assert!(text.contains(&format!("{histogram}_sum{{{labels}}} 200.03\n")));
        assert!(text.contains(&format!("{histogram}_count{{{labels}}} 2\n")));
        // Each family is described once, even without series.
        assert_eq!(text.matches("# TYPE gateway_build_info gauge").count(), 1);
        assert!(text.contains("# TYPE gateway_inference_requests_total counter\n"));
        assert_eq!(registry.value(&WORKER_RUNS, &["price_sync", "ok"]), Some(3));
        assert_eq!(registry.value(&WORKER_RUNS, &["missing", "ok"]), None);
    }

    #[test]
    fn label_values_are_escaped() {
        let registry = Registry::new();
        registry.inc(&HTTP_REQUESTS, &["/a\"b\\c\nd", "GET", "2xx"]);
        assert!(registry.render().contains(
            "gateway_http_requests_total{route=\"/a\\\"b\\\\c\\nd\",method=\"GET\",status=\"2xx\"} 1\n"
        ));
    }

    #[test]
    fn every_family_name_is_unique_and_conventional() {
        let mut names: Vec<_> = FAMILIES.iter().map(|family| family.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), FAMILIES.len());
        for family in FAMILIES {
            assert!(family.name.starts_with("gateway_"));
            assert_eq!(
                family.kind == Kind::Counter,
                family.name.ends_with("_total"),
                "{}",
                family.name
            );
        }
    }
}
