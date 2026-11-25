//! `gateway doctor`: read-only checks of a deployment, for operators and the
//! agents that install it. It loads the configuration, connects to the
//! database, compares its migration history with the binary's, checks the
//! role's grants and the clock, lists the provider adapters, and connects to
//! the trace store and ClickHouse when they are configured. With
//! `--network` it also resolves and TLS-connects to each provider host the
//! environment configures; it never calls a model.
//!
//! It changes nothing and never prints a setting's value: checks name
//! settings, roles, tables and hosts only.
//!
//! Output is text, or JSON with `--json` (schema below, versioned by
//! `schema_version`). The exit code is 1 when any check fails, else 0.
//!
//! ```text
//! {
//!   "schema_version": 1,
//!   "gateway_version": "0.1.0",
//!   "status": "ok" | "warn" | "fail",
//!   "checks": [
//!     { "id": "database.connect", "status": "ok" | "warn" | "fail" | "skip",
//!       "summary": "…", "hint": "…" | null }
//!   ]
//! }
//! ```

mod checks;
mod net;

use std::fmt::Write as _;

use serde::Serialize;

pub use checks::{
    ApplicationGrants, DataPlaneGrants, connect, evaluate_application_grants, evaluate_clock_skew,
    evaluate_data_plane_grants, evaluate_migrations,
};

/// The JSON report's schema version: bumped when a field changes meaning or
/// goes away; new fields and new check ids keep it.
pub const SCHEMA_VERSION: u32 = 1;

/// What the command line asked for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// Print the report as JSON.
    pub json: bool,
    /// Also resolve and TLS-connect to provider hosts.
    pub network: bool,
}

/// One check's outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    /// Not run: not configured, not asked for, or blocked by a failed check.
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// One check: a stable id, its status, what it found, and what to do.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub status: Status,
    pub summary: String,
    pub hint: Option<String>,
}

impl Check {
    pub fn ok(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, Status::Ok, summary, None::<String>)
    }

    pub fn warn(id: &'static str, summary: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(id, Status::Warn, summary, Some(hint))
    }

    pub fn fail(id: &'static str, summary: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(id, Status::Fail, summary, Some(hint))
    }

    pub fn skip(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, Status::Skip, summary, None::<String>)
    }

    fn new(
        id: &'static str,
        status: Status,
        summary: impl Into<String>,
        hint: Option<impl Into<String>>,
    ) -> Self {
        Self {
            id,
            status,
            summary: summary.into(),
            hint: hint.map(Into::into),
        }
    }

    /// The same check with a hint, whatever its status.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Every check, and the status they add up to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub gateway_version: String,
    /// `fail` if any check failed, else `warn` if any warned, else `ok`.
    /// Skipped checks count for nothing.
    pub status: Status,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new(checks: Vec<Check>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            gateway_version: env!("CARGO_PKG_VERSION").to_owned(),
            status: aggregate(&checks),
            checks,
        }
    }

    /// 1 when any check failed, else 0.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.status == Status::Fail)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report always serializes")
    }

    /// The human-readable report: one line per check, its hint indented
    /// below, and a closing count.
    pub fn to_text(&self) -> String {
        let mut text = format!("gateway doctor (gateway {})\n\n", self.gateway_version);
        let width = self.checks.iter().map(|check| check.id.len()).max();
        for check in &self.checks {
            let _ = writeln!(
                text,
                "{:<6} {:<width$}  {}",
                format!("[{}]", check.status.as_str()),
                check.id,
                check.summary,
                width = width.unwrap_or(0),
            );
            if let Some(hint) = &check.hint {
                let _ = writeln!(
                    text,
                    "{:<6} {:<width$}  hint: {hint}",
                    "",
                    "",
                    width = width.unwrap_or(0)
                );
            }
        }
        let count = |status| self.checks.iter().filter(|c| c.status == status).count();
        let _ = writeln!(
            text,
            "\nresult: {} ({} ok, {} warn, {} fail, {} skip)",
            self.status.as_str(),
            count(Status::Ok),
            count(Status::Warn),
            count(Status::Fail),
            count(Status::Skip),
        );
        text
    }
}

/// `fail` beats `warn` beats `ok`; `skip` counts for nothing.
pub fn aggregate(checks: &[Check]) -> Status {
    let has = |status| checks.iter().any(|check| check.status == status);
    if has(Status::Fail) {
        Status::Fail
    } else if has(Status::Warn) {
        Status::Warn
    } else {
        Status::Ok
    }
}

/// Runs every check against `lookup`'s settings (the process environment,
/// for the command).
pub async fn diagnose(lookup: &dyn Fn(&str) -> Option<String>, options: Options) -> Report {
    Report::new(checks::run(lookup, options).await)
}

/// The `gateway doctor` command: prints the report and returns the exit
/// code.
pub async fn run(options: Options) -> i32 {
    let report = diagnose(&|name: &str| std::env::var(name).ok(), options).await;
    if options.json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.to_text());
    }
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GatewayCommand;
    use std::ffi::OsString;

    fn report(statuses: &[Status]) -> Report {
        Report::new(
            statuses
                .iter()
                .map(|status| Check {
                    id: "test.check",
                    status: *status,
                    summary: "summary".into(),
                    hint: None,
                })
                .collect(),
        )
    }

    #[test]
    fn status_aggregates_worst_and_ignores_skips() {
        assert_eq!(report(&[]).status, Status::Ok);
        assert_eq!(report(&[Status::Ok, Status::Skip]).status, Status::Ok);
        assert_eq!(report(&[Status::Skip]).status, Status::Ok);
        assert_eq!(
            report(&[Status::Ok, Status::Warn, Status::Skip]).status,
            Status::Warn
        );
        assert_eq!(
            report(&[Status::Warn, Status::Fail, Status::Ok]).status,
            Status::Fail
        );
    }

    #[test]
    fn exit_code_is_nonzero_only_on_failure() {
        assert_eq!(report(&[Status::Ok, Status::Skip]).exit_code(), 0);
        assert_eq!(report(&[Status::Warn]).exit_code(), 0);
        assert_eq!(report(&[Status::Warn, Status::Fail]).exit_code(), 1);
    }

    #[test]
    fn json_has_the_stable_schema() {
        let report = Report::new(vec![
            Check::ok("config.load", "loads"),
            Check::fail("database.connect", "refused", "start PostgreSQL"),
            Check::skip("trace_store.reachable", "not configured"),
        ]);
        let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["checks", "gateway_version", "schema_version", "status"]
        );
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["status"], "fail");
        assert_eq!(json["gateway_version"], env!("CARGO_PKG_VERSION"));
        let check = &json["checks"][1];
        let mut keys: Vec<_> = check.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["hint", "id", "status", "summary"]);
        assert_eq!(check["id"], "database.connect");
        assert_eq!(check["status"], "fail");
        assert_eq!(check["hint"], "start PostgreSQL");
        assert!(json["checks"][0]["hint"].is_null());
        assert_eq!(json["checks"][2]["status"], "skip");
    }

    #[test]
    fn text_lists_each_check_with_its_hint_and_a_count() {
        let text = Report::new(vec![
            Check::ok("config.load", "configuration loads"),
            Check::warn("database.clock_skew", "clock is 3 s ahead", "sync NTP"),
            Check::skip("providers.network", "not run"),
        ])
        .to_text();
        let lines: Vec<_> = text.lines().collect();
        assert!(lines[0].starts_with("gateway doctor (gateway "));
        assert_eq!(lines[2], "[ok]   config.load          configuration loads");
        assert_eq!(lines[3], "[warn] database.clock_skew  clock is 3 s ahead");
        assert_eq!(lines[4], "                            hint: sync NTP");
        assert_eq!(lines[5], "[skip] providers.network    not run");
        assert_eq!(
            lines.last().unwrap(),
            &"result: warn (1 ok, 1 warn, 0 fail, 1 skip)"
        );
    }

    #[test]
    fn command_line_parses() {
        let parse = |args: &[&str]| GatewayCommand::parse(args.iter().map(OsString::from));
        assert_eq!(
            parse(&["doctor"]).unwrap(),
            GatewayCommand::Doctor {
                json: false,
                network: false
            }
        );
        assert_eq!(
            parse(&["doctor", "--network", "--json"]).unwrap(),
            GatewayCommand::Doctor {
                json: true,
                network: true
            }
        );
        for bad in [&["doctor", "--json", "--json"][..], &["doctor", "--fix"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
