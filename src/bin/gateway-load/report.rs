//! The run's report and its gates: each threshold is a flag
//! (`--gate-<name>`), set by the profile and overridable; a gate whose flag
//! is unset is not checked. The report is JSON (`--report`), a summary on
//! standard output, and optionally a line appended to a history file
//! (`--history`) so nightly runs can be compared.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    args::Flags,
    check::Accounting,
    generator::{Class, Plan, Stats},
    sampler::{Samples, TableStat},
};

#[derive(Debug, Serialize)]
pub struct Gate {
    pub name: String,
    pub limit: f64,
    pub value: Option<f64>,
    pub pass: bool,
    pub note: String,
}

fn ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1e4).round() / 10.0
}

/// A profile's defaults, applied beneath the command line and environment.
pub fn apply_profile(flags: &mut Flags) -> Result<(), String> {
    let profile = flags.string("profile", "smoke");
    let faults: &[(&str, &str)] = &[
        ("fault-429", "0.02"),
        ("fault-429-all", "0.005"),
        ("fault-500", "0.003"),
        ("fault-503", "0.003"),
        ("fault-drop", "0.005"),
        ("fault-timeout", "0.001"),
    ];
    let common: &[(&str, &str)] = &[
        ("gate-error-rate", "0.001"),
        ("gate-send-lag-p99-ms", "50"),
        ("gate-throughput-ratio", "0.999"),
        ("gate-recovery-s", "60"),
    ];
    let specific: &[(&str, &str)] = match profile.as_str() {
        // A minute at a low rate: the harness itself works.
        "smoke" => &[
            ("rate", "20"),
            ("duration", "60s"),
            ("warmup", "5s"),
            ("ramp", "5s"),
            ("stream-seconds", "2..10"),
            ("gate-error-rate", "0.01"),
            ("gate-overhead-p95-ms", "250"),
        ],
        // The nightly CI run: ten minutes on the shared runner (database,
        // gateways and generator on one machine), so its gates catch
        // regressions rather than prove capacity.
        "nightly" => &[
            ("rate", "100"),
            ("duration", "10m"),
            ("warmup", "30s"),
            ("ramp", "30s"),
            ("stream-seconds", "2..30"),
            ("gate-overhead-p95-ms", "150"),
            ("gate-send-lag-p99-ms", "100"),
            ("gate-outbox-lag-s", "120"),
        ],
        // The plan's gates (D-030), on separate hosts: 50,000 a minute on
        // one organization's budget; 180,000 a minute over 100.
        "gate-single" => &[
            ("tenancy", "single"),
            ("rate", "833.34"),
            ("duration", "15m"),
            ("warmup", "60s"),
            ("ramp", "60s"),
            ("stream-seconds", "2..60"),
            ("prompt-bytes", "400..400000"),
            ("gate-overhead-p95-ms", "50"),
            ("gate-outbox-lag-s", "60"),
        ],
        "gate-spread" => &[
            ("tenancy", "spread"),
            ("rate", "3000"),
            ("duration", "15m"),
            ("warmup", "60s"),
            ("ramp", "60s"),
            ("stream-seconds", "2..60"),
            ("prompt-bytes", "400..400000"),
            ("gate-overhead-p95-ms", "50"),
            ("gate-outbox-lag-s", "60"),
        ],
        // Hours at the target rate: memory, connections and backlogs flat.
        "soak" => &[
            ("tenancy", "spread"),
            ("rate", "833.34"),
            ("duration", "4h"),
            ("warmup", "60s"),
            ("ramp", "60s"),
            ("window", "5m"),
            ("stream-seconds", "2..60"),
            ("prompt-bytes", "400..400000"),
            ("gate-overhead-p95-ms", "50"),
            ("gate-outbox-lag-s", "60"),
            ("gate-rss-growth-pct", "20"),
        ],
        other => return Err(format!("unknown --profile {other:?}")),
    };
    for (name, value) in faults.iter().chain(common).chain(specific) {
        flags.set_default(name, value);
    }
    // A threshold that is given must be a number (or `off`): one that does
    // not parse must not quietly drop its check.
    for name in GATES {
        if let Some(value) = flags.get(&format!("gate-{name}")) {
            gate_limit(&value).map_err(|()| {
                format!("--gate-{name}: {value:?} is not a number (or `off` to skip the check)")
            })?;
        }
    }
    Ok(())
}

/// Every pass/fail threshold, as `--gate-<name>`.
const GATES: &[&str] = &[
    "error-rate",
    "overhead-p95-ms",
    "send-lag-p99-ms",
    "throughput-ratio",
    "db-sessions-max",
    "outbox-lag-s",
    "rss-growth-pct",
    "recovery-s",
];

/// A threshold's limit: a finite number, or `off` (no check).
fn gate_limit(value: &str) -> Result<Option<f64>, ()> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    match value.parse::<f64>() {
        Ok(limit) if limit.is_finite() => Ok(Some(limit)),
        _ => Err(()),
    }
}

fn gate(
    flags: &Flags,
    name: &str,
    value: Option<f64>,
    lower_is_better: bool,
    note: &str,
) -> Option<Gate> {
    // Validated by `apply_profile`: absent or `off` means no check.
    let limit = gate_limit(&flags.get(&format!("gate-{name}"))?).ok()??;
    let pass = match value {
        Some(value) if lower_is_better => value <= limit,
        Some(value) => value >= limit,
        None => false,
    };
    Some(Gate {
        name: name.to_owned(),
        limit,
        value,
        pass,
        note: note.to_owned(),
    })
}

/// Growth of a process's memory: the mean of the last tenth of its
/// samples over the first tenth, for the pid it ended with (a restart
/// starts it afresh), once warm.
fn rss_growth(samples: &Samples, warmup: f64) -> Option<(String, f64)> {
    let mut worst: Option<(String, f64)> = None;
    let mut labels: Vec<&str> = samples.processes.iter().map(|s| s.label.as_str()).collect();
    labels.sort_unstable();
    labels.dedup();
    for label in labels {
        let series: Vec<_> = samples
            .processes
            .iter()
            .filter(|s| s.label == label && s.t_s >= warmup)
            .collect();
        let Some(last) = series.last() else { continue };
        let series: Vec<_> = series.iter().filter(|s| s.pid == last.pid).collect();
        if series.len() < 20 {
            continue;
        }
        let tenth = series.len() / 10;
        let mean = |part: &[&&crate::sampler::ProcessSample]| {
            part.iter().map(|s| s.rss_mb).sum::<f64>() / part.len() as f64
        };
        let first = mean(&series[..tenth]);
        let end = mean(&series[series.len() - tenth..]);
        let growth = (end / first.max(1.0) - 1.0) * 100.0;
        if worst.as_ref().is_none_or(|(_, value)| growth > *value) {
            worst = Some((label.to_owned(), growth));
        }
    }
    worst
}

/// The counters' change over the load. PostgreSQL resets them when it
/// recovers from a crash; a counter that went down counts from zero.
fn delta(start: i64, end: i64, reset: &mut bool) -> i64 {
    if end >= start {
        end - start
    } else {
        *reset = true;
        end
    }
}

const FIRST_HOUR_S: f64 = 3_600.0;

/// Growth and autovacuum of each table over the load, with what it comes
/// to a request and, at the offered rate, a month: the figures the
/// partitioning decision waits on (`docs/architecture-plan.md`, D-030).
fn tables(samples: &Samples, warmup: f64, sent: u64, rate_per_s: f64) -> Value {
    if samples.tables_start.is_empty() || samples.tables_end.is_empty() {
        return Value::Null;
    }
    let start: std::collections::HashMap<&str, &TableStat> = samples
        .tables_start
        .iter()
        .map(|t| (t.table.as_str(), t))
        .collect();
    let empty = TableStat::default();
    let mut reset = false;
    let mut rows: Vec<(i64, Value)> = Vec::new();
    let (mut growth_total, mut autovacuums_total) = (0_i64, 0_i64);
    for end in &samples.tables_end {
        let begin = start.get(end.table.as_str()).copied().unwrap_or(&empty);
        let growth = end.total_bytes - begin.total_bytes;
        let inserted = delta(begin.inserted, end.inserted, &mut reset);
        let updated = delta(begin.updated, end.updated, &mut reset);
        let hot = delta(begin.hot_updated, end.hot_updated, &mut reset);
        let deleted = delta(begin.deleted, end.deleted, &mut reset);
        let autovacuums = delta(begin.autovacuums, end.autovacuums, &mut reset);
        let autoanalyzes = delta(begin.autoanalyzes, end.autoanalyzes, &mut reset);
        growth_total += growth;
        autovacuums_total += autovacuums;
        // Measured once warm. A table only vacuumed or analyzed during the
        // load (dead rows from an earlier run) still shows.
        let shares: Vec<(f64, f64)> = samples
            .dead_share
            .get(&end.table)
            .map(|series| {
                series
                    .iter()
                    .copied()
                    .filter(|(t_s, _)| *t_s >= warmup)
                    .collect()
            })
            .unwrap_or_default();
        if growth == 0
            && inserted + updated + deleted + autovacuums + autoanalyzes == 0
            && shares.is_empty()
        {
            continue;
        }
        let max = |part: &mut dyn Iterator<Item = f64>| {
            part.fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
        };
        let dead_share_max = max(&mut shares.iter().map(|(_, share)| *share));
        // The partitioning rule reads dead rows after the soak's first
        // hour, when an early spike on a young table is past.
        let dead_share_after_first_hour_max = max(&mut shares
            .iter()
            .filter(|(t_s, _)| *t_s >= FIRST_HOUR_S)
            .map(|(_, share)| *share));
        // The peak of each minute, to tell a spike from a rising trend.
        let mut by_minute: Vec<(u64, f64)> = Vec::new();
        for (t_s, share) in &shares {
            let minute = (*t_s / 60.0) as u64;
            match by_minute.last_mut() {
                Some((last, peak)) if *last == minute => *peak = peak.max(*share),
                _ => by_minute.push((minute, *share)),
            }
        }
        rows.push((
            growth,
            json!({
                "table": end.table,
                "bytes_start": begin.total_bytes,
                "bytes_end": end.total_bytes,
                "growth_bytes": growth,
                "heap_growth_bytes": end.heap_bytes - begin.heap_bytes,
                "index_growth_bytes": end.index_bytes - begin.index_bytes,
                "bytes_per_request": growth as f64 / sent.max(1) as f64,
                "rows_end": end.live,
                "inserted": inserted,
                "updated": updated,
                "hot_update_share": (updated > 0).then(|| hot as f64 / updated as f64),
                "deleted": deleted,
                "dead_end": end.dead,
                "dead_share_max": dead_share_max,
                "dead_share_after_first_hour_max": dead_share_after_first_hour_max,
                "dead_share_by_minute": by_minute,
                "autovacuums": autovacuums,
                "autoanalyzes": autoanalyzes,
            }),
        ));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    let bytes_per_request = growth_total as f64 / sent.max(1) as f64;
    json!({
        "growth_bytes": growth_total,
        "bytes_per_request": bytes_per_request,
        "projected_gib_per_month": bytes_per_request * rate_per_s * 86_400.0 * 30.0
            / f64::from(1_u32 << 30),
        "autovacuums": autovacuums_total,
        "counters_reset": reset,
        "by_table": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    })
}

pub fn build(
    flags: &Flags,
    plan: &Plan,
    stats: &Stats,
    samples: &Samples,
    accounting: Option<&Accounting>,
    fake_stats: Vec<Value>,
) -> (Value, Vec<Gate>) {
    let measured = stats.measured.max(1) as f64;
    let error_rate = stats.unexpected as f64 / measured;
    let warmup = plan.warmup.as_secs_f64();
    let after_warmup: Vec<_> = samples.db.iter().filter(|s| s.t_s >= warmup).collect();
    let max_of = |f: &dyn Fn(&crate::sampler::DbSample) -> f64| {
        after_warmup
            .iter()
            .map(|s| f(s))
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
    };
    let mean_of = |f: &dyn Fn(&crate::sampler::DbSample) -> f64| {
        (!after_warmup.is_empty())
            .then(|| after_warmup.iter().map(|s| f(s)).sum::<f64>() / after_warmup.len() as f64)
    };
    let sessions_max = max_of(&|s| s.sessions as f64);
    let outbox_lag_max = max_of(&|s| s.outbox_oldest_s as f64);
    let measured_seconds = (plan.duration.saturating_sub(plan.warmup))
        .as_secs_f64()
        .max(1.0);
    let throughput = stats.ok as f64 / measured_seconds;
    let completed_ratio = 1.0 - stats.not_sent as f64 / stats.due.max(1) as f64;

    let mut gates = Vec::new();
    gates.extend(gate(flags, "error-rate", Some(error_rate), true,
        "unexpected outcomes (not the injected fault's, not during an injected failure) per measured request"));
    for class in Class::ALL {
        let histogram = stats.overhead(class);
        let value = (histogram.count() > 0).then(|| ms(histogram.percentile(95.0)));
        if let Some(mut g) = gate(flags, "overhead-p95-ms", value, true, "") {
            g.name = format!("overhead-p95-ms {}", class.as_str());
            g.note = if class == Class::Stream {
                "to the first content event, less the provider's time to first byte".into()
            } else {
                "latency from due, less the provider's".into()
            };
            if histogram.count() == 0 {
                g.pass = true;
                g.note = "no requests of this kind".into();
            }
            gates.push(g);
        }
    }
    gates.extend(gate(
        flags,
        "send-lag-p99-ms",
        Some(ms(stats.send_lag.percentile(99.0))),
        true,
        "the generator's own lateness; if it fails, the run measured the generator",
    ));
    gates.extend(gate(
        flags,
        "throughput-ratio",
        Some(completed_ratio),
        false,
        "share of due requests actually sent",
    ));
    gates.extend(gate(
        flags,
        "db-sessions-max",
        sessions_max,
        true,
        "database sessions of the gateway role",
    ));
    gates.extend(gate(
        flags,
        "outbox-lag-s",
        outbox_lag_max,
        true,
        "oldest undispatched outbox event",
    ));
    // A configured memory check that could not measure fails, rather than
    // passing unmeasured (no local pid files, or too few samples after a
    // late restart); `--gate-rss-growth-pct off` skips it.
    let growth = rss_growth(samples, warmup);
    gates.extend(gate(
        flags,
        "rss-growth-pct",
        growth.as_ref().map(|(_, growth)| *growth),
        true,
        &growth.as_ref().map_or_else(
            || {
                "not measured: no process had 20 samples after warm-up (pass --pid-file, or `off`)"
                    .to_owned()
            },
            |(label, _)| format!("worst: {label}"),
        ),
    ));
    gates.push(Gate {
        name: "crashed-tasks".into(),
        limit: 0.0,
        value: Some(stats.crashed as f64),
        pass: stats.crashed == 0,
        note: "request or event tasks that panicked instead of recording an outcome".into(),
    });
    for event in &stats.events {
        if let Some(mut g) = gate(flags, "recovery-s", event.recovery_s, true, "") {
            g.name = format!("recovery-s {}", event.label);
            g.note = format!(
                "command {}",
                if event.exit_ok == Some(true) {
                    "ok"
                } else {
                    "FAILED"
                }
            );
            g.pass &= event.exit_ok == Some(true);
            gates.push(g);
        }
    }
    if let Some(accounting) = accounting {
        gates.push(Gate {
            name: "accounting-exact".into(),
            limit: 0.0,
            value: Some(accounting.violation_count as f64),
            pass: accounting.exact(),
            note: "money checks: violations found".into(),
        });
    }

    let mut outcomes: Vec<Value> = stats
        .outcomes
        .iter()
        .map(|((class, fault, label), count)| {
            json!({"class": class.as_str(), "fault": fault.as_str(), "outcome": label, "count": count})
        })
        .collect();
    outcomes.sort_by_key(|value| {
        (
            value["class"].as_str().unwrap_or("").to_owned(),
            value["fault"].as_str().unwrap_or("").to_owned(),
            std::cmp::Reverse(value["count"].as_u64().unwrap_or(0)),
        )
    });
    let mut waits: Vec<(&String, &u64)> = samples.waits.iter().collect();
    waits.sort_by(|a, b| b.1.cmp(a.1));
    let samples_count = samples.db.len().max(1) as f64;
    let windows: Vec<Value> = stats
        .windows
        .iter()
        .map(|w| {
            json!({"start_s": w.start.as_secs(), "count": w.completed,
                   "overhead_p50_ms": ms(w.overhead.percentile(50.0)),
                   "overhead_p95_ms": ms(w.overhead.percentile(95.0)),
                   "overhead_p99_ms": ms(w.overhead.percentile(99.0))})
        })
        .collect();
    let report = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "git_sha": std::env::var("LOAD_GIT_SHA").ok(),
        "host": std::env::var("LOAD_HOST_LABEL").ok(),
        "plan": plan,
        "result": {
            "elapsed_s": stats.elapsed.as_secs_f64(),
            "due": stats.due,
            "measured": stats.measured,
            "ok": stats.ok,
            "expected_failures": stats.expected_failures,
            "unexpected": stats.unexpected,
            "disrupted": stats.disrupted,
            "not_sent": stats.not_sent,
            "error_rate": error_rate,
            "ok_per_s": throughput,
            "offered_per_min": plan.rate * 60.0,
            "in_flight_max": stats.in_flight_max,
            "send_lag": stats.send_lag.summary(),
            "latency_during_failures": stats.during_failures.summary(),
            "classes": stats.class_summary(),
            "first_unexpected": stats.first_unexpected,
            "audit_dropped": stats.audit_dropped,
        },
        "outcomes": outcomes,
        "events": stats.events,
        "windows": windows,
        "database": {
            "samples": samples.db.len(),
            "sample_errors": samples.db_errors,
            "sessions_max": sessions_max,
            "sessions_mean": mean_of(&|s| s.sessions as f64),
            "active_mean": mean_of(&|s| s.active as f64),
            "active_max": max_of(&|s| s.active as f64),
            "lock_waits_mean": mean_of(&|s| s.lock_waits as f64),
            "commits_per_s_mean": mean_of(&|s| s.commits_per_s),
            "commits_per_ok_request": mean_of(&|s| s.commits_per_s).map(|c| c / throughput.max(1e-9)),
            "outbox_pending_max": max_of(&|s| s.outbox_pending as f64),
            "outbox_lag_max_s": outbox_lag_max,
            "reservations_open_max": max_of(&|s| s.reservations_open as f64),
            "top_waits": waits.iter().take(6).map(|(name, count)| {
                json!({"wait": name, "sessions_avg": **count as f64 / samples_count})
            }).collect::<Vec<_>>(),
            "series": samples.db,
            "tables": tables(samples, warmup, stats.due - stats.not_sent, plan.rate),
        },
        "processes": samples.processes,
        "fake_providers": fake_stats,
        "accounting": accounting,
        "gates": gates,
        "pass": gates.iter().all(|g| g.pass),
    });
    (report, gates)
}

pub fn print_summary(report: &Value, gates: &[Gate]) {
    let result = &report["result"];
    let database = &report["database"];
    println!(
        "== load run ({}) ==",
        report["plan"]["profile"].as_str().unwrap_or("")
    );
    println!(
        "offered {:.0}/min for {:.0}s: due {} ok {} expected-failures {} unexpected {} disrupted {} not-sent {}",
        result["offered_per_min"].as_f64().unwrap_or(0.0),
        result["elapsed_s"].as_f64().unwrap_or(0.0),
        result["due"],
        result["ok"],
        result["expected_failures"],
        result["unexpected"],
        result["disrupted"],
        result["not_sent"]
    );
    println!(
        "answered {:.1}/s, in flight max {}, send lag p99 {} ms",
        result["ok_per_s"].as_f64().unwrap_or(0.0),
        result["in_flight_max"],
        result["send_lag"]["p99_ms"]
    );
    for class in Class::ALL {
        let c = &result["classes"][class.as_str()];
        println!(
            "  {:<12} overhead p50 {:>7} p95 {:>7} p99 {:>7} ms | latency p50 {:>8} p99 {:>8} ms (n={})",
            class.as_str(),
            c["overhead"]["p50_ms"],
            c["overhead"]["p95_ms"],
            c["overhead"]["p99_ms"],
            c["latency"]["p50_ms"],
            c["latency"]["p99_ms"],
            c["overhead"]["count"]
        );
    }
    if database["samples"].as_u64().unwrap_or(0) > 0 {
        println!(
            "database: sessions max {} mean {:.1}, active mean {:.1}, commits/s {:.0} ({:.1} per answered request), outbox pending max {} lag max {} s",
            database["sessions_max"],
            database["sessions_mean"].as_f64().unwrap_or(0.0),
            database["active_mean"].as_f64().unwrap_or(0.0),
            database["commits_per_s_mean"].as_f64().unwrap_or(0.0),
            database["commits_per_ok_request"].as_f64().unwrap_or(0.0),
            database["outbox_pending_max"],
            database["outbox_lag_max_s"]
        );
        let waits: Vec<String> = database["top_waits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| {
                format!(
                    "{} {:.1}",
                    w["wait"].as_str().unwrap_or(""),
                    w["sessions_avg"].as_f64().unwrap_or(0.0)
                )
            })
            .collect();
        println!("  top waits: {}", waits.join(", "));
    }
    let tables = &database["tables"];
    if !tables.is_null() {
        let mib = |v: &Value| v.as_f64().unwrap_or(0.0) / f64::from(1_u32 << 20);
        println!(
            "tables: grew {:.1} MiB, {:.0} bytes per request sent, {:.1} GiB a month at the offered rate; {} autovacuums{}",
            mib(&tables["growth_bytes"]),
            tables["bytes_per_request"].as_f64().unwrap_or(0.0),
            tables["projected_gib_per_month"].as_f64().unwrap_or(0.0),
            tables["autovacuums"],
            if tables["counters_reset"] == true {
                " (counters reset during the run)"
            } else {
                ""
            }
        );
        for table in tables["by_table"].as_array().into_iter().flatten().take(6) {
            println!(
                "  {:<34} +{:>8.1} MiB {:>7.0} B/request, dead max {}, {} autovacuums",
                table["table"].as_str().unwrap_or(""),
                mib(&table["growth_bytes"]),
                table["bytes_per_request"].as_f64().unwrap_or(0.0),
                table["dead_share_max"]
                    .as_f64()
                    .map_or("n/a".to_owned(), |v| format!("{:.0}%", v * 100.0)),
                table["autovacuums"]
            );
        }
    }
    if let Some(events) = report["events"].as_array() {
        for event in events {
            println!(
                "event {} at {:.1}s: {} recovery {} s",
                event["label"].as_str().unwrap_or(""),
                event["started_s"].as_f64().unwrap_or(0.0),
                if event["exit_ok"] == true {
                    "ok"
                } else {
                    "FAILED"
                },
                event["recovery_s"]
            );
        }
    }
    let accounting = &report["accounting"];
    if !accounting.is_null() {
        println!(
            "accounting: {} audited, reservations {}, reconciliation open {}, violations {}",
            accounting["audited"],
            accounting["reservations"],
            accounting["reconciliation_open"],
            accounting["violation_count"]
        );
        if accounting["leases_taken"].as_i64().unwrap_or(0) > 0 {
            println!(
                "  budget leases: {} taken, {} still active",
                accounting["leases_taken"], accounting["leases_active"]
            );
        }
        if accounting["rate_leases_taken"].as_i64().unwrap_or(0) > 0 {
            println!(
                "  rate-limit leases: {} taken, {} still active",
                accounting["rate_leases_taken"], accounting["rate_leases_active"]
            );
        }
        for violation in accounting["violations"]
            .as_array()
            .into_iter()
            .flatten()
            .take(10)
        {
            println!("  VIOLATION {}", violation.as_str().unwrap_or(""));
        }
    }
    for line in result["first_unexpected"]
        .as_array()
        .into_iter()
        .flatten()
        .take(5)
    {
        println!("  unexpected: {}", line.as_str().unwrap_or(""));
    }
    for gate in gates {
        println!(
            "gate {:<28} {:>4} value {:>10} limit {:>10}  {}",
            gate.name,
            if gate.pass { "PASS" } else { "FAIL" },
            gate.value.map_or("n/a".to_owned(), |v| format!("{v:.4}")),
            gate.limit,
            gate.note
        );
    }
    println!(
        "RESULT profile={} pass={} ok_per_s={:.1} error_rate={:.5} chat_p95_ms={} stream_first_p95_ms={} embed_p95_ms={}",
        report["plan"]["profile"].as_str().unwrap_or(""),
        report["pass"],
        result["ok_per_s"].as_f64().unwrap_or(0.0),
        result["error_rate"].as_f64().unwrap_or(0.0),
        result["classes"]["chat"]["overhead"]["p95_ms"],
        result["classes"]["chat_stream"]["overhead"]["p95_ms"],
        result["classes"]["embeddings"]["overhead"]["p95_ms"],
    );
}

/// One line of the nightly history: the figures to compare run by run.
pub fn history_line(report: &Value) -> Value {
    let result = &report["result"];
    json!({
        "at": report["generated_at"],
        "git_sha": report["git_sha"],
        "host": report["host"],
        "profile": report["plan"]["profile"],
        "rate": report["plan"]["rate"],
        "pass": report["pass"],
        "ok_per_s": result["ok_per_s"],
        "error_rate": result["error_rate"],
        "chat_overhead_p95_ms": result["classes"]["chat"]["overhead"]["p95_ms"],
        "stream_first_overhead_p95_ms": result["classes"]["chat_stream"]["overhead"]["p95_ms"],
        "embed_overhead_p95_ms": result["classes"]["embeddings"]["overhead"]["p95_ms"],
        "db_sessions_max": report["database"]["sessions_max"],
        "commits_per_ok_request": report["database"]["commits_per_ok_request"],
        "outbox_lag_max_s": report["database"]["outbox_lag_max_s"],
        "table_bytes_per_request": report["database"]["tables"]["bytes_per_request"],
        "table_gib_per_month": report["database"]["tables"]["projected_gib_per_month"],
        "violations": report["accounting"]["violation_count"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(table: &str, total_bytes: i64, inserted: i64, autovacuums: i64) -> TableStat {
        TableStat {
            table: table.to_owned(),
            total_bytes,
            inserted,
            autovacuums,
            ..TableStat::default()
        }
    }

    #[test]
    fn table_growth_is_measured_per_request_and_month() {
        let mut samples = Samples {
            tables_start: vec![stat("a", 1_000, 10, 1), stat("idle", 50, 5, 0)],
            tables_end: vec![
                stat("a", 11_000, 110, 3),
                stat("idle", 50, 5, 0),
                stat("new", 2_000, 4, 0),
            ],
            ..Samples::default()
        };
        samples
            .dead_share
            .insert("a".into(), vec![(1.0, 0.9), (20.0, 0.25), (30.0, 0.1)]);
        let report = tables(&samples, 10.0, 100, 10.0);
        assert_eq!(report["growth_bytes"], 12_000);
        assert_eq!(report["bytes_per_request"], 120.0);
        assert_eq!(report["autovacuums"], 2);
        assert_eq!(report["counters_reset"], false);
        let projected = 120.0 * 10.0 * 86_400.0 * 30.0 / f64::from(1_u32 << 30);
        assert_eq!(report["projected_gib_per_month"], projected);
        let rows = report["by_table"].as_array().unwrap();
        // Largest growth first; a table nothing touched is left out.
        let names: Vec<_> = rows.iter().map(|r| r["table"].as_str().unwrap()).collect();
        assert_eq!(names, ["a", "new"]);
        assert_eq!(rows[0]["inserted"], 100);
        // Dead share during warm-up is not counted.
        assert_eq!(rows[0]["dead_share_max"], 0.25);
        assert_eq!(rows[1]["bytes_start"], 0);
        assert!(rows[1]["dead_share_max"].is_null());
        // Shorter than an hour: nothing after the first hour.
        assert!(rows[0]["dead_share_after_first_hour_max"].is_null());
        assert_eq!(rows[0]["dead_share_by_minute"], json!([[0, 0.25]]));
    }

    #[test]
    fn dead_rows_are_kept_by_minute_and_after_the_first_hour() {
        let mut samples = Samples {
            tables_start: vec![stat("a", 0, 0, 0), stat("vacuumed", 100, 0, 0)],
            tables_end: vec![stat("a", 10, 10, 0), stat("vacuumed", 100, 0, 1)],
            ..Samples::default()
        };
        samples.dead_share.insert(
            "a".into(),
            vec![
                (100.0, 0.6),
                (110.0, 0.7),
                (200.0, 0.1),
                (3_600.0, 0.3),
                (3_700.0, 0.2),
            ],
        );
        let report = tables(&samples, 60.0, 10, 1.0);
        let rows = report["by_table"].as_array().unwrap();
        assert_eq!(rows[0]["dead_share_max"], 0.7);
        assert_eq!(rows[0]["dead_share_after_first_hour_max"], 0.3);
        assert_eq!(
            rows[0]["dead_share_by_minute"],
            json!([[1, 0.7], [3, 0.1], [60, 0.3], [61, 0.2]])
        );
        // Vacuumed during the load without growing or being written.
        assert_eq!(rows[1]["table"], "vacuumed");
        assert_eq!(rows[1]["autovacuums"], 1);
    }

    #[test]
    fn counters_reset_by_a_crash_count_from_zero() {
        let samples = Samples {
            tables_start: vec![stat("a", 1_000, 500, 4)],
            tables_end: vec![stat("a", 1_500, 30, 1)],
            ..Samples::default()
        };
        let report = tables(&samples, 0.0, 10, 1.0);
        assert_eq!(report["counters_reset"], true);
        assert_eq!(report["by_table"][0]["inserted"], 30);
        assert_eq!(report["autovacuums"], 1);
    }

    #[test]
    fn no_table_figures_without_both_reads() {
        let samples = Samples {
            tables_start: vec![stat("a", 1, 1, 0)],
            ..Samples::default()
        };
        assert!(tables(&samples, 0.0, 1, 1.0).is_null());
    }
}
