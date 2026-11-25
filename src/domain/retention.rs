//! Data retention and export.
//!
//! An organization's retention policy says how many days its traces, usage
//! records and audit events are kept; the worker deletes what is older. A
//! period left unset keeps that data indefinitely. Usage is kept per
//! reservation: a reservation goes with its usage, allocations, price
//! snapshot, settlement record, resolved reconciliation item and trace, and
//! only once it is final and no reconciliation item is open.
//!
//! Exports stream the same data as NDJSON or CSV for a bounded time range.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const MIN_TRACE_DAYS: u32 = 7;
pub const MIN_USAGE_DAYS: u32 = 30;
pub const MIN_AUDIT_DAYS: u32 = 90;
pub const MAX_RETENTION_DAYS: u32 = 3650;
/// Longest time range one export covers.
pub const MAX_EXPORT_DAYS: i64 = 366;
/// Range of an export that names neither end.
pub const DEFAULT_EXPORT_DAYS: i64 = 30;

/// The policy as set: a write replaces the whole policy. A period that is
/// null or left out keeps that data indefinitely, so an omission never
/// deletes anything.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetentionPolicyInput {
    pub trace_days: Option<u32>,
    pub usage_days: Option<u32>,
    pub audit_days: Option<u32>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RetentionPolicyError {
    #[error("trace_days must be null or between 7 and 3650")]
    TraceDays,
    #[error("usage_days must be null or between 30 and 3650")]
    UsageDays,
    #[error("audit_days must be null or between 90 and 3650")]
    AuditDays,
    #[error(
        "with usage_days set, trace_days must be set and not exceed it: a trace is removed with its usage"
    )]
    TraceOutlivesUsage,
}

impl RetentionPolicyInput {
    pub fn validate(self) -> Result<Self, RetentionPolicyError> {
        let within = |days: Option<u32>, min: u32| {
            days.is_none_or(|days| (min..=MAX_RETENTION_DAYS).contains(&days))
        };
        if !within(self.trace_days, MIN_TRACE_DAYS) {
            return Err(RetentionPolicyError::TraceDays);
        }
        if !within(self.usage_days, MIN_USAGE_DAYS) {
            return Err(RetentionPolicyError::UsageDays);
        }
        if !within(self.audit_days, MIN_AUDIT_DAYS) {
            return Err(RetentionPolicyError::AuditDays);
        }
        // A trace goes with its reservation, so keeping traces longer than
        // usage, or indefinitely while usage expires, cannot be honored.
        if let Some(usage) = self.usage_days {
            if self.trace_days.is_none_or(|trace| trace > usage) {
                return Err(RetentionPolicyError::TraceOutlivesUsage);
            }
        }
        Ok(self)
    }
}

/// Rows the purge has removed under the policy, since it was first set.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PurgedCounts {
    pub traces: u64,
    /// Reservations, each with its usage and accounting records.
    pub reservations: u64,
    pub audit_events: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetentionPolicyView {
    pub trace_days: Option<u32>,
    pub usage_days: Option<u32>,
    pub audit_days: Option<u32>,
    /// 0 until the policy is first set; then the `ETag` version.
    pub version: u64,
    pub updated_at: Option<DateTime<Utc>>,
    pub last_purged_at: Option<DateTime<Utc>>,
    pub purged: PurgedCounts,
}

impl RetentionPolicyView {
    /// An organization that never set a policy keeps everything.
    pub fn unset() -> Self {
        Self {
            trace_days: None,
            usage_days: None,
            audit_days: None,
            version: 0,
            updated_at: None,
            last_purged_at: None,
            purged: PurgedCounts::default(),
        }
    }
}

/// A kind of data the purge removes, by the period that governs it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionKind {
    Traces,
    Usage,
    AuditEvents,
}

impl RetentionKind {
    pub const ALL: [Self; 3] = [Self::Traces, Self::Usage, Self::AuditEvents];

    /// The policy column holding the period.
    pub const fn column(self) -> &'static str {
        match self {
            Self::Traces => "trace_days",
            Self::Usage => "usage_days",
            Self::AuditEvents => "audit_days",
        }
    }

    /// The table counted in the retention metrics.
    pub const fn table(self) -> &'static str {
        match self {
            Self::Traces => "inference_trace",
            Self::Usage => "accounting_reservation",
            Self::AuditEvents => "audit_event",
        }
    }
}

/// Where an export ended up. An export is `streaming` while rows are sent
/// (or, if the gateway stopped mid-export, until the record ages out);
/// `abandoned` means the client left before the end.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportStatus {
    Streaming,
    Completed,
    Failed,
    Abandoned,
}

impl ExportStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

/// One export, as recorded when it was requested: who asked for which slice
/// of which data, and how it ended.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExportRecord {
    pub id: Uuid,
    pub dataset: String,
    pub format: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub actor_user_id: Option<Uuid>,
    pub request_id: Uuid,
    pub status: ExportStatus,
    pub rows: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// The batch the export was limited to (`batch_<uuid>`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// The master record the export was limited to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_master: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportDataset {
    Usage,
    Traces,
    AuditEvents,
}

impl ExportDataset {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "usage" => Some(Self::Usage),
            "traces" => Some(Self::Traces),
            "audit-events" => Some(Self::AuditEvents),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Traces => "traces",
            Self::AuditEvents => "audit-events",
        }
    }

    /// The audit action recorded when the dataset is exported.
    pub const fn audit_action(self) -> &'static str {
        match self {
            Self::Usage => "usage.exported",
            Self::Traces => "traces.exported",
            Self::AuditEvents => "audit_events.exported",
        }
    }

    /// Column names, in CSV order; each NDJSON object has the same keys.
    pub const fn columns(self) -> &'static [&'static str] {
        match self {
            Self::Usage => &[
                "usage_id",
                "recorded_at",
                "reservation_id",
                "team_id",
                "service_account_id",
                "deployment",
                "model",
                "currency",
                "confidence",
                "completeness",
                "finality",
                "input_tokens",
                "output_tokens",
                "cached_input_tokens",
                "reasoning_tokens",
                "other_tokens",
                "amount_minor",
                "batch_id",
                "custom_id",
                "model_master",
            ],
            Self::Traces => &[
                "trace_id",
                "recorded_at",
                "finalized_at",
                "reservation_id",
                "correlation_id",
                "team_id",
                "service_account_id",
                "status",
                "outcome",
                "provider_request_id",
                "deployment",
                "model",
                "provider_family",
                "input_bound",
                "output_cap",
                "price_version",
                "pricing_confidence",
                "currency",
                "amount_minor",
                "batch_id",
                "custom_id",
                "model_master",
            ],
            Self::AuditEvents => &[
                "event_id",
                "occurred_at",
                "actor_type",
                "actor_user_id",
                "actor_service_account_id",
                "team_id",
                "action",
                "entity_type",
                "entity_id",
                "outcome",
                "request_id",
                "metadata",
                "before_metadata",
                "after_metadata",
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExportFormat {
    #[default]
    Ndjson,
    Csv,
}

impl ExportFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ndjson" | "jsonl" => Some(Self::Ndjson),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::Csv => "csv",
        }
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Ndjson => "application/x-ndjson",
            Self::Csv => "text/csv; charset=utf-8",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::Csv => "csv",
        }
    }

    /// Text before the first row: the CSV header.
    pub fn preamble(self, dataset: ExportDataset) -> String {
        match self {
            Self::Ndjson => String::new(),
            Self::Csv => format!("{}\r\n", dataset.columns().join(",")),
        }
    }

    /// One row, whose values are in `dataset.columns()` order.
    pub fn row(self, dataset: ExportDataset, values: &[Value]) -> String {
        match self {
            Self::Ndjson => {
                let object: serde_json::Map<String, Value> = dataset
                    .columns()
                    .iter()
                    .zip(values)
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect();
                format!("{}\n", Value::Object(object))
            }
            Self::Csv => {
                let cells: Vec<String> = values.iter().map(csv_cell).collect();
                format!("{}\r\n", cells.join(","))
            }
        }
    }
}

/// One RFC 4180 cell. Null is empty; objects are their JSON text. A text
/// cell a spreadsheet would read as a formula is prefixed with `'`.
fn csv_cell(value: &Value) -> String {
    let text = match value {
        Value::Null => return String::new(),
        Value::Bool(value) => return value.to_string(),
        Value::Number(value) => return value.to_string(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    };
    let text = if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}")
    } else {
        text
    };
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ExportQueryError {
    #[error("dataset must be usage, traces or audit-events")]
    Dataset,
    #[error("format must be ndjson or csv")]
    Format,
    #[error("from and to must be RFC 3339 timestamps")]
    Timestamp,
    #[error("from must be before to, at most 366 days apart")]
    Range,
    #[error("batch_id must name a batch (batch_<uuid>), and only usage and traces take it")]
    Batch,
    #[error(
        "model_master must name a master record (vendor/model or org/<slug>), and only usage and traces take it"
    )]
    ModelMaster,
}

/// Which rows an export covers: those recorded in `[from, to)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportQuery {
    pub dataset: ExportDataset,
    pub format: ExportFormat,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Only the lines of this batch (usage and traces).
    pub batch: Option<Uuid>,
    /// Only requests served by deployments linked to this master then
    /// (usage and traces, D-033).
    pub model_master: Option<String>,
}

impl ExportQuery {
    /// `to` defaults to `now` and `from` to 30 days before `to`.
    pub fn new(
        dataset: &str,
        format: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Self, ExportQueryError> {
        let dataset = ExportDataset::parse(dataset).ok_or(ExportQueryError::Dataset)?;
        let format = match format {
            Some(format) => ExportFormat::parse(format).ok_or(ExportQueryError::Format)?,
            None => ExportFormat::default(),
        };
        let parse = |value: &str| {
            DateTime::parse_from_rfc3339(value)
                .map(|time| time.with_timezone(&Utc))
                .map_err(|_| ExportQueryError::Timestamp)
        };
        let to = to.map(parse).transpose()?.unwrap_or(now);
        let from = from
            .map(parse)
            .transpose()?
            .unwrap_or(to - Duration::days(DEFAULT_EXPORT_DAYS));
        if from >= to || to - from > Duration::days(MAX_EXPORT_DAYS) {
            return Err(ExportQueryError::Range);
        }
        Ok(Self {
            dataset,
            format,
            from,
            to,
            batch: None,
            model_master: None,
        })
    }

    /// Limits a usage or traces export to one batch's lines, named as
    /// `batch_<uuid>` or the bare UUID.
    pub fn with_batch(self, batch: Option<&str>) -> Result<Self, ExportQueryError> {
        let Some(batch) = batch else {
            return Ok(self);
        };
        if self.dataset == ExportDataset::AuditEvents {
            return Err(ExportQueryError::Batch);
        }
        let batch = crate::domain::batch::parse_batch_ref(batch).ok_or(ExportQueryError::Batch)?;
        Ok(Self {
            batch: Some(batch),
            ..self
        })
    }

    /// Limits a usage or traces export to requests served by deployments
    /// linked to master record `master` when they were served.
    pub fn with_model_master(self, master: Option<&str>) -> Result<Self, ExportQueryError> {
        let Some(master) = master.map(str::trim) else {
            return Ok(self);
        };
        if self.dataset == ExportDataset::AuditEvents
            || !crate::domain::model_master::valid_master_id(master)
        {
            return Err(ExportQueryError::ModelMaster);
        }
        Ok(Self {
            model_master: Some(master.to_owned()),
            ..self
        })
    }

    /// The download's file name, e.g. `usage-20260901-20261001.csv`.
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}.{}",
            self.dataset.as_str(),
            self.from.format("%Y%m%dT%H%M%SZ"),
            self.to.format("%Y%m%dT%H%M%SZ"),
            self.format.extension()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(trace: Option<u32>, usage: Option<u32>, audit: Option<u32>) -> RetentionPolicyInput {
        RetentionPolicyInput {
            trace_days: trace,
            usage_days: usage,
            audit_days: audit,
        }
    }

    #[test]
    fn exports_limit_usage_and_traces_to_a_model_master() {
        let now = Utc::now();
        let query = |dataset: &str| ExportQuery::new(dataset, None, None, None, now).unwrap();
        let usage = query("usage")
            .with_model_master(Some("anthropic/claude-sonnet-4-6"))
            .unwrap();
        assert_eq!(
            usage.model_master.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(
            query("usage").with_model_master(None).unwrap().model_master,
            None
        );
        assert_eq!(
            query("traces").with_model_master(Some("bad")),
            Err(ExportQueryError::ModelMaster)
        );
        assert_eq!(
            query("audit-events").with_model_master(Some("org/qwen")),
            Err(ExportQueryError::ModelMaster)
        );
        for dataset in [ExportDataset::Usage, ExportDataset::Traces] {
            assert_eq!(dataset.columns().last(), Some(&"model_master"));
        }
    }

    #[test]
    fn policies_keep_each_kind_for_at_least_its_floor() {
        assert!(policy(None, None, None).validate().is_ok());
        assert!(policy(Some(7), Some(30), Some(90)).validate().is_ok());
        assert!(policy(Some(3650), None, Some(3650)).validate().is_ok());
        assert_eq!(
            policy(None, Some(30), None).validate(),
            Err(RetentionPolicyError::TraceOutlivesUsage)
        );
        assert_eq!(
            policy(Some(6), None, None).validate(),
            Err(RetentionPolicyError::TraceDays)
        );
        assert_eq!(
            policy(Some(7), Some(29), None).validate(),
            Err(RetentionPolicyError::UsageDays)
        );
        assert_eq!(
            policy(None, None, Some(89)).validate(),
            Err(RetentionPolicyError::AuditDays)
        );
        assert_eq!(
            policy(Some(7), Some(3651), None).validate(),
            Err(RetentionPolicyError::UsageDays)
        );
        assert_eq!(
            policy(Some(60), Some(30), None).validate(),
            Err(RetentionPolicyError::TraceOutlivesUsage)
        );
        // Left out means kept indefinitely.
        assert_eq!(
            serde_json::from_value::<RetentionPolicyInput>(json!({"trace_days": 7})).unwrap(),
            policy(Some(7), None, None)
        );
        assert!(
            serde_json::from_value::<RetentionPolicyInput>(json!({
                "trace_days": null, "usage_days": null, "audit_days": null, "extra": 1
            }))
            .is_err()
        );
    }

    #[test]
    fn export_ranges_default_to_thirty_days_and_are_bounded() {
        let now = DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let query = ExportQuery::new("usage", None, None, None, now).unwrap();
        assert_eq!(query.format, ExportFormat::Ndjson);
        assert_eq!(query.to, now);
        assert_eq!(query.from, now - Duration::days(30));
        assert_eq!(
            query.file_name(),
            "usage-20260901T000000Z-20261001T000000Z.ndjson"
        );
        let query = ExportQuery::new(
            "audit-events",
            Some("csv"),
            Some("2026-01-01T00:00:00+02:00"),
            Some("2026-02-01T00:00:00Z"),
            now,
        )
        .unwrap();
        assert_eq!(query.dataset, ExportDataset::AuditEvents);
        assert_eq!(query.from.to_rfc3339(), "2025-12-31T22:00:00+00:00");
        for (dataset, format, from, to, error) in [
            ("users", None, None, None, ExportQueryError::Dataset),
            ("usage", Some("xml"), None, None, ExportQueryError::Format),
            (
                "usage",
                None,
                Some("yesterday"),
                None,
                ExportQueryError::Timestamp,
            ),
            (
                "usage",
                None,
                Some("2026-10-01T00:00:00Z"),
                Some("2026-10-01T00:00:00Z"),
                ExportQueryError::Range,
            ),
            (
                "usage",
                None,
                Some("2025-01-01T00:00:00Z"),
                Some("2026-10-01T00:00:00Z"),
                ExportQueryError::Range,
            ),
        ] {
            assert_eq!(ExportQuery::new(dataset, format, from, to, now), Err(error));
        }
    }

    #[test]
    fn rows_encode_as_ndjson_objects_and_safe_csv() {
        let dataset = ExportDataset::AuditEvents;
        let mut values: Vec<Value> = vec![Value::Null; dataset.columns().len()];
        values[0] = json!("e1");
        values[6] = json!("=HYPERLINK(\"x\")");
        values[8] = json!(42);
        values[11] = json!({"role": "auditor", "note": "a,b"});
        let line = ExportFormat::Ndjson.row(dataset, &values);
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["event_id"], "e1");
        assert_eq!(parsed["entity_id"], 42);
        assert_eq!(parsed["actor_user_id"], Value::Null);
        assert_eq!(parsed["metadata"]["role"], "auditor");
        assert_eq!(
            ExportFormat::Csv.preamble(dataset),
            format!("{}\r\n", dataset.columns().join(","))
        );
        let row = ExportFormat::Csv.row(dataset, &values);
        let cells: Vec<&str> = row.trim_end_matches("\r\n").splitn(8, ',').collect();
        assert_eq!(cells[0], "e1");
        assert_eq!(cells[1], "");
        // Formula-looking text is neutralized, then quoted for its quotes.
        assert_eq!(cells[6], "\"'=HYPERLINK(\"\"x\"\")\"");
        assert!(row.contains(",42,"));
        assert!(row.contains("\"{\"\"note\"\":\"\"a,b\"\",\"\"role\"\":\"\"auditor\"\"}\""));
        assert_eq!(csv_cell(&json!("-5")), "'-5");
        assert_eq!(csv_cell(&json!(-5)), "-5");
        assert_eq!(csv_cell(&json!("line\nbreak")), "\"line\nbreak\"");
    }
}
