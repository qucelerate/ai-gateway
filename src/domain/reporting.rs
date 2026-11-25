//! Reporting read models: usage aggregates, metadata-only inference traces
//! and reconciliation items.
//!
//! Reports never blend confidence levels or currencies: every aggregate is
//! keyed by `(currency, confidence)`, and an amount is only summed from rows
//! that carry one. Rows without an amount are counted as `unpriced` instead of
//! being treated as zero. Traces expose accounting metadata only; there is no
//! field that could carry prompt, completion, or credential content.

use std::{fmt, str::FromStr};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    accounting::Currency,
    catalog::ProviderFamily,
    identity::{OrganizationId, ServiceAccountId, TeamId, UserAccountId},
};

/// Longest window one usage query may cover.
pub const MAX_USAGE_WINDOW_DAYS: i64 = 366;
const MAX_LABEL_LENGTH: usize = 255;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportConfidence {
    Exact,
    Estimated,
    Unknown,
}

impl ReportConfidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Estimated => "estimated",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact" => Some(Self::Exact),
            "estimated" => Some(Self::Estimated),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Dimension a usage report is grouped by. Every group is further split by
/// currency and confidence.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroupBy {
    #[default]
    Day,
    Team,
    ServiceAccount,
    Model,
    Deployment,
    /// Catalog provider family, so Azure OpenAI and Azure AI Foundry (and any
    /// other families serving the same model) stay apart.
    ProviderFamily,
    /// Catalog deployment location, so regional prices stay apart.
    Location,
    /// The master record (one model across providers, D-033) the
    /// deployment was linked to when the request was served.
    ModelMaster,
}

impl UsageGroupBy {
    pub const ALL: [Self; 8] = [
        Self::Day,
        Self::Team,
        Self::ServiceAccount,
        Self::Model,
        Self::Deployment,
        Self::ProviderFamily,
        Self::Location,
        Self::ModelMaster,
    ];

    /// One or two distinct dimensions, comma-separated: usage by team then
    /// model is `team,model`.
    pub fn parse_list(value: &str) -> Result<Vec<Self>, ReportQueryError> {
        let mut dimensions = Vec::new();
        for part in value.split(',').map(str::trim) {
            let dimension = Self::ALL
                .into_iter()
                .find(|dimension| dimension.as_str() == part)
                .ok_or(ReportQueryError::InvalidUsageGroupBy)?;
            if dimensions.contains(&dimension) {
                return Err(ReportQueryError::InvalidUsageGroupBy);
            }
            dimensions.push(dimension);
        }
        if dimensions.is_empty() || dimensions.len() > 2 {
            return Err(ReportQueryError::InvalidUsageGroupBy);
        }
        Ok(dimensions)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Team => "team",
            Self::ServiceAccount => "service_account",
            Self::Model => "model",
            Self::Deployment => "deployment",
            Self::ProviderFamily => "provider_family",
            Self::Location => "location",
            Self::ModelMaster => "model_master",
        }
    }
}

/// Usage filters as given: each list matches any of its values, and an empty
/// list does not filter.
#[derive(Clone, Debug, Default)]
pub struct UsageFilterInput {
    pub team_ids: Vec<String>,
    pub service_account_ids: Vec<String>,
    pub models: Vec<String>,
    pub deployments: Vec<String>,
    pub provider_families: Vec<String>,
    pub locations: Vec<String>,
    /// Batches, as `batch_<uuid>` or the bare UUID: their lines' usage.
    pub batch_ids: Vec<String>,
    /// Master records (`anthropic/claude-sonnet-4-6`, `org/<slug>`).
    pub model_masters: Vec<String>,
}

/// Validated usage query. `from` is inclusive and `to` exclusive; both are
/// required so a page token always refers to one fixed window. Rows group by
/// one or two dimensions; lists are sorted and deduplicated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageQuery {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub group_by: Vec<UsageGroupBy>,
    pub team_ids: Vec<TeamId>,
    pub service_account_ids: Vec<ServiceAccountId>,
    pub models: Vec<String>,
    pub deployments: Vec<String>,
    pub provider_families: Vec<ProviderFamily>,
    pub locations: Vec<String>,
    /// The lines of these batches (D-021).
    pub batch_ids: Vec<Uuid>,
    /// Usage served by deployments linked to these masters then (D-033).
    pub model_masters: Vec<String>,
    pub currency: Option<Currency>,
}

impl UsageQuery {
    pub fn new(
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: Vec<UsageGroupBy>,
        input: UsageFilterInput,
        currency: Option<Currency>,
    ) -> Result<Self, ReportQueryError> {
        validate_window(from, to)?;
        if to - from > Duration::days(MAX_USAGE_WINDOW_DAYS) {
            return Err(ReportQueryError::WindowTooLarge);
        }
        if group_by.is_empty() || group_by.len() > 2 || group_by.first() == group_by.get(1) {
            return Err(ReportQueryError::InvalidUsageGroupBy);
        }
        Ok(Self {
            from,
            to,
            group_by,
            team_ids: uuids(input.team_ids)?.into_iter().map(TeamId).collect(),
            service_account_ids: uuids(input.service_account_ids)?
                .into_iter()
                .map(ServiceAccountId)
                .collect(),
            models: labels(input.models)?,
            deployments: labels(input.deployments)?,
            provider_families: families(input.provider_families)?,
            locations: labels(input.locations)?,
            batch_ids: parsed(input.batch_ids, crate::domain::batch::parse_batch_ref)?,
            model_masters: model_masters(input.model_masters)?,
            currency,
        })
    }

    /// The single team a query is about, for its audit record.
    pub fn team(&self) -> Option<TeamId> {
        match self.team_ids.as_slice() {
            [team] => Some(*team),
            _ => None,
        }
    }

    /// The dimensions as the report names them: `team,model`.
    pub fn group_by_name(&self) -> String {
        self.group_by
            .iter()
            .map(|dimension| dimension.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Canonical filter string; the service binds its digest into page
    /// tokens. JSON escapes every value, so free-form labels cannot forge
    /// another query's binding.
    pub fn normalized_filters(&self) -> String {
        let mut filters = serde_json::json!({
            "from": timestamp(self.from),
            "to": timestamp(self.to),
            "group_by": self.group_by_name(),
            "team": self.team_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            "service_account": self.service_account_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            "model": self.models,
            "deployment": self.deployments,
            "provider_family": self.provider_families.iter().map(|family| family.as_str()).collect::<Vec<_>>(),
            "location": self.locations,
            "currency": self.currency.as_ref().map(Currency::as_str),
        });
        // Only when set, so a page token made before the filter existed
        // still binds to the same query.
        if !self.batch_ids.is_empty() {
            filters["batch"] = serde_json::json!(self.batch_ids);
        }
        if !self.model_masters.is_empty() {
            filters["model_master"] = serde_json::json!(self.model_masters);
        }
        filters.to_string()
    }
}

/// A master record filter: card ids and `org/<slug>` records, checked for
/// form only (a record since deleted still names past usage).
pub fn model_masters(values: Vec<String>) -> Result<Vec<String>, ReportQueryError> {
    parsed(values, |value| {
        let value = value.trim();
        crate::domain::model_master::valid_master_id(value).then(|| value.to_owned())
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenTotals {
    pub input: i64,
    pub output: i64,
    pub cached_input: i64,
    pub reasoning: i64,
    pub other: i64,
}

/// One `(currency, confidence)` aggregate. `amount_minor` sums only rows that
/// carry an amount; `unpriced_request_count` counts the rest. Token totals sum
/// reported counters; `incomplete_request_count` counts rows whose usage was
/// partial or unavailable, so a total is never mistaken for a complete one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageAggregate {
    pub currency: String,
    pub confidence: ReportConfidence,
    pub request_count: i64,
    pub amount_minor: i64,
    pub unpriced_request_count: i64,
    pub incomplete_request_count: i64,
    pub tokens: TokenTotals,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageReportRow {
    /// Group value: a UTC date (`YYYY-MM-DD`) for `day`, an id for `team` and
    /// `service_account`, or the label for `model`, `deployment`,
    /// `provider_family` and `location`. `null` marks organization-level usage
    /// when grouping by team or service account, usage without catalog
    /// provenance when grouping by provider family, and a deployment without a
    /// location (or without catalog provenance) when grouping by location.
    /// For two dimensions, the first one's value; see `values`.
    pub group: Option<String>,
    /// Each grouped dimension's value, by dimension name.
    pub values: std::collections::BTreeMap<&'static str, Option<String>>,
    #[serde(flatten)]
    pub aggregate: UsageAggregate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageReport {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// The dimensions, comma-separated: `day`, or `team,model`.
    pub group_by: String,
    /// Totals over the whole filtered window, independent of paging.
    pub totals: Vec<UsageAggregate>,
    pub items: Vec<UsageReportRow>,
    pub next_page_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceStatus {
    Pending,
    Finalized,
}

impl TraceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Finalized => "finalized",
        }
    }
}

/// Most values one trace filter may list.
pub const MAX_FILTER_VALUES: usize = 50;
/// Longest trace search term, in characters.
pub const MAX_SEARCH_LENGTH: usize = 200;

/// Trace filters as given: each list matches any of its values (an empty list
/// does not filter), and `search` matches part of the model or deployment,
/// or is a trace id or request (correlation) id.
#[derive(Clone, Debug, Default)]
pub struct TraceFilterInput {
    pub team_ids: Vec<String>,
    pub service_account_ids: Vec<String>,
    pub api_key_ids: Vec<String>,
    /// The apps' own ids of their end users (D-028).
    pub end_users: Vec<String>,
    pub models: Vec<String>,
    pub deployments: Vec<String>,
    pub provider_families: Vec<String>,
    pub locations: Vec<String>,
    pub statuses: Vec<String>,
    pub outcomes: Vec<String>,
    /// Batches, as `batch_<uuid>` or the bare UUID: their lines' traces.
    pub batch_ids: Vec<String>,
    pub search: Option<String>,
}

/// Validated trace query; lists are sorted and deduplicated, so one view of
/// the traces has one canonical form.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub team_ids: Vec<TeamId>,
    pub service_account_ids: Vec<ServiceAccountId>,
    pub api_key_ids: Vec<Uuid>,
    pub end_users: Vec<String>,
    pub models: Vec<String>,
    pub deployments: Vec<String>,
    pub provider_families: Vec<String>,
    pub locations: Vec<String>,
    pub statuses: Vec<TraceStatus>,
    pub outcomes: Vec<String>,
    /// The lines of these batches (D-021).
    pub batch_ids: Vec<Uuid>,
    pub search: Option<String>,
}

impl TraceQuery {
    pub fn new(
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        input: TraceFilterInput,
    ) -> Result<Self, ReportQueryError> {
        if let (Some(from), Some(to)) = (from, to) {
            validate_window(from, to)?;
        }
        let search = match input.search.map(|term| term.trim().to_owned()) {
            Some(term) if term.chars().count() > MAX_SEARCH_LENGTH => {
                return Err(ReportQueryError::InvalidSearch);
            }
            Some(term) if !term.is_empty() => Some(term),
            _ => None,
        };
        Ok(Self {
            from,
            to,
            team_ids: uuids(input.team_ids)?.into_iter().map(TeamId).collect(),
            service_account_ids: uuids(input.service_account_ids)?
                .into_iter()
                .map(ServiceAccountId)
                .collect(),
            api_key_ids: uuids(input.api_key_ids)?,
            end_users: parsed(input.end_users, |id| {
                crate::domain::end_user::parse_end_user_id(id).ok()
            })?,
            models: labels(input.models)?,
            deployments: labels(input.deployments)?,
            provider_families: families(input.provider_families)?
                .into_iter()
                .map(|family| family.as_str().to_owned())
                .collect(),
            locations: labels(input.locations)?,
            statuses: parsed(input.statuses, |status| match status.trim() {
                "pending" => Some(TraceStatus::Pending),
                "finalized" => Some(TraceStatus::Finalized),
                _ => None,
            })?,
            outcomes: labels(input.outcomes)?,
            batch_ids: parsed(input.batch_ids, crate::domain::batch::parse_batch_ref)?,
            search,
        })
    }

    /// The single team a query is about, for its audit record.
    pub fn team(&self) -> Option<TeamId> {
        match self.team_ids.as_slice() {
            [team] => Some(*team),
            _ => None,
        }
    }

    /// Canonical filter string; the service binds its digest into page
    /// tokens. JSON escapes every value, so free-form labels cannot forge
    /// another query's binding.
    pub fn normalized_filters(&self) -> String {
        let mut filters = serde_json::json!({
            "from": self.from.map(timestamp),
            "to": self.to.map(timestamp),
            "team": self.team_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            "service_account": self.service_account_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            "api_key": self.api_key_ids,
            "end_user": self.end_users,
            "model": self.models,
            "deployment": self.deployments,
            "provider_family": self.provider_families,
            "location": self.locations,
            "status": self.statuses.iter().map(|status| status.as_str()).collect::<Vec<_>>(),
            "outcome": self.outcomes,
            "q": self.search,
        });
        // Only when set, so a page token made before the filter existed
        // still binds to the same view.
        if !self.batch_ids.is_empty() {
            filters["batch"] = serde_json::json!(self.batch_ids);
        }
        filters.to_string()
    }
}

/// What trace summaries count by. `day` is the UTC date the trace was
/// recorded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceDimension {
    Day,
    Team,
    ServiceAccount,
    Model,
    Deployment,
    ProviderFamily,
    Location,
    Status,
    Outcome,
}

impl TraceDimension {
    pub const ALL: [Self; 9] = [
        Self::Day,
        Self::Team,
        Self::ServiceAccount,
        Self::Model,
        Self::Deployment,
        Self::ProviderFamily,
        Self::Location,
        Self::Status,
        Self::Outcome,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Team => "team",
            Self::ServiceAccount => "service_account",
            Self::Model => "model",
            Self::Deployment => "deployment",
            Self::ProviderFamily => "provider_family",
            Self::Location => "location",
            Self::Status => "status",
            Self::Outcome => "outcome",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|dimension| dimension.as_str() == value)
    }

    /// One or two distinct dimensions, comma-separated.
    pub fn parse_list(value: &str) -> Result<Vec<Self>, ReportQueryError> {
        let mut dimensions = Vec::new();
        for part in value.split(',').map(str::trim) {
            let dimension = Self::parse(part).ok_or(ReportQueryError::InvalidGroupBy)?;
            if dimensions.contains(&dimension) {
                return Err(ReportQueryError::InvalidGroupBy);
            }
            dimensions.push(dimension);
        }
        if dimensions.is_empty() || dimensions.len() > 2 {
            return Err(ReportQueryError::InvalidGroupBy);
        }
        Ok(dimensions)
    }
}

/// A priced amount in one currency.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CurrencyAmount {
    pub currency: String,
    pub amount_minor: i64,
}

/// One summary group. `values` holds each dimension's value (`null` when the
/// trace has none, such as organization-level usage for `team`). Amounts stay
/// per currency, summed only from priced traces; `unpriced_count` counts the
/// rest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceGroup {
    pub values: std::collections::BTreeMap<&'static str, Option<String>>,
    pub count: i64,
    pub unpriced_count: i64,
    pub amounts: Vec<CurrencyAmount>,
    pub first_recorded_at: DateTime<Utc>,
    pub last_recorded_at: DateTime<Utc>,
}

/// Traces counted by one or two dimensions, largest groups first. `total`
/// counts every matching trace, including groups past the limit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceSummaryReport {
    pub group_by: Vec<TraceDimension>,
    pub total: i64,
    pub truncated: bool,
    pub groups: Vec<TraceGroup>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceSummary {
    pub id: Uuid,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub service_account_id: Option<ServiceAccountId>,
    pub status: TraceStatus,
    pub outcome: Option<String>,
    pub model: String,
    pub deployment: String,
    /// `null` when the request had no catalog provenance.
    pub provider_family: Option<String>,
    /// `null` when the deployment has no location or no catalog provenance.
    pub location: Option<String>,
    pub currency: String,
    pub amount_minor: Option<i64>,
    pub usage_confidence: Option<ReportConfidence>,
    pub recorded_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    /// The request id the gateway returned to the caller.
    pub correlation_id: Uuid,
    /// The API key that made the request; `null` for requests admitted
    /// before keys were recorded.
    pub api_key_id: Option<Uuid>,
    /// The app's own id of the end user the request was for (D-028);
    /// `null` when it named none.
    pub end_user: Option<String>,
    /// The public model alias the request named.
    pub alias: Option<String>,
    /// `chat`, `chat_stream`, `embeddings`, `chat_batch` or `embeddings_batch`; `null` without catalog
    /// provenance.
    pub operation: Option<String>,
    /// Admission to settlement, in milliseconds; `null` while pending or when
    /// the request was closed later by expiry or an operator.
    pub duration_ms: Option<i64>,
    /// The gateway's category for a failed provider request.
    pub error_category: Option<String>,
    /// What the trace holds of the request's content (`stored`, `writing`,
    /// `off`, `unavailable`, `failed`, `expired`); `null` when tracing was not
    /// on for it.
    pub content_state: Option<String>,
    /// Which attempt of its request this is, from 1: a request that failed
    /// over to another deployment of its route has one trace per attempt
    /// (D-023).
    pub attempt: i16,
    /// The trace of the attempt this one replaced, whose outcome is
    /// `failed-over`; `null` for a request's first attempt.
    pub failover_of: Option<Uuid>,
    /// The batch the request was a line of (`batch_<uuid>`); `null` for a
    /// request made on its own.
    pub batch_id: Option<String>,
    /// The line's `custom_id` in its batch.
    pub custom_id: Option<String>,
}

/// Which catalog release, route, deployment and price served a request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceRouting {
    pub release_id: Uuid,
    pub route_id: Uuid,
    pub deployment_id: Uuid,
    pub price_id: Uuid,
    pub price_source: Option<String>,
    pub price_override_id: Option<Uuid>,
    pub chat_input_count_mode: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceReservation {
    pub id: Uuid,
    pub state: String,
    pub reserved_minor: i64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TraceTokens {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cached_input: Option<i64>,
    pub reasoning: Option<i64>,
    pub other: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceUsage {
    pub confidence: ReportConfidence,
    pub completeness: String,
    pub finality: String,
    pub tokens: TraceTokens,
    pub amount_minor: Option<i64>,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceReconciliation {
    pub id: Uuid,
    pub reason: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Why the provider request failed: the gateway's category, the provider's
/// HTTP status and its error type. Never a message, which may echo the request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceProviderError {
    pub category: String,
    pub http_status: Option<u16>,
    pub error_type: Option<String>,
}

/// Metadata-only trace detail. Provider request ids are opaque correlation
/// handles; no request or response content is stored or returned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TraceDetail {
    #[serde(flatten)]
    pub summary: TraceSummary,
    pub provider_request_id: Option<String>,
    pub input_bound: i64,
    /// `None` for embeddings, which reserve input only.
    pub output_cap: Option<i64>,
    pub price_version: String,
    pub pricing_confidence: String,
    pub reservation: TraceReservation,
    /// Latest usage recorded for the reservation, if any.
    pub usage: Option<TraceUsage>,
    pub reconciliation: Option<TraceReconciliation>,
    /// Set when the provider request failed.
    pub provider_error: Option<TraceProviderError>,
    /// `null` when the request had no catalog provenance.
    pub routing: Option<TraceRouting>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationStatusFilter {
    #[default]
    Open,
    Resolved,
    All,
}

impl ReconciliationStatusFilter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::All => "all",
        }
    }
}

/// Reconciliation list items; oldest first so the list works as a queue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconciliationItem {
    pub id: Uuid,
    pub reservation_id: Uuid,
    pub trace_id: Uuid,
    pub team_id: Option<TeamId>,
    pub service_account_id: Option<ServiceAccountId>,
    pub reason: String,
    pub status: String,
    pub reservation_state: String,
    pub model: String,
    pub deployment: String,
    pub currency: String,
    pub reserved_minor: i64,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// `settlement` when a late settlement closed the item, `operator` when an
    /// administrator resolved it; absent while open.
    pub resolution: Option<String>,
    /// The amount an operator charged; absent otherwise.
    pub resolved_amount_minor: Option<i64>,
    pub resolved_by: Option<UserAccountId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportQueryError {
    InvalidWindow,
    WindowTooLarge,
    InvalidLabel,
    UnknownProviderFamily,
    /// A list filter names an unknown value (an ID that is not a UUID, a
    /// status that does not exist) or lists too many.
    InvalidFilterValue,
    InvalidSearch,
    InvalidGroupBy,
    InvalidUsageGroupBy,
}

impl fmt::Display for ReportQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidWindow => "from must be earlier than to",
            Self::WindowTooLarge => "the reporting window may not exceed 366 days",
            Self::InvalidLabel => {
                "model, deployment and location filters must be 1 to 255 characters"
            }
            Self::UnknownProviderFamily => "provider_family is not a known provider family",
            Self::InvalidFilterValue => {
                "a filter lists an unknown value, or more than 50 values"
            }
            Self::InvalidSearch => "q may not exceed 200 characters",
            Self::InvalidUsageGroupBy => {
                "group_by must name one or two distinct dimensions: day, team, service_account, model, deployment, provider_family or location"
            }
            Self::InvalidGroupBy => {
                "group_by must name one or two distinct dimensions: day, team, service_account, model, deployment, provider_family, location, status or outcome"
            }
        })
    }
}
impl std::error::Error for ReportQueryError {}

fn validate_window(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<(), ReportQueryError> {
    if from < to {
        Ok(())
    } else {
        Err(ReportQueryError::InvalidWindow)
    }
}

/// A list filter's values: split at commas, where `\\,` is a comma within a
/// value and `\\\\` a backslash, so any label can be listed.
pub fn split_list(value: &str) -> Vec<String> {
    let mut values = vec![String::new()];
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some(escaped @ (',' | '\\')) => values.last_mut().unwrap().push(escaped),
                Some(other) => {
                    let value = values.last_mut().unwrap();
                    value.push('\\');
                    value.push(other);
                }
                None => values.last_mut().unwrap().push('\\'),
            },
            ',' => values.push(String::new()),
            other => values.last_mut().unwrap().push(other),
        }
    }
    values
}

/// Each value parsed, then sorted and deduplicated.
fn parsed<T: Ord>(
    values: Vec<String>,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, ReportQueryError> {
    if values.len() > MAX_FILTER_VALUES {
        return Err(ReportQueryError::InvalidFilterValue);
    }
    let mut parsed = values
        .iter()
        .map(|value| parse(value).ok_or(ReportQueryError::InvalidFilterValue))
        .collect::<Result<Vec<_>, _>>()?;
    parsed.sort();
    parsed.dedup();
    Ok(parsed)
}

fn uuids(values: Vec<String>) -> Result<Vec<Uuid>, ReportQueryError> {
    parsed(values, |id| id.trim().parse().ok())
}

/// Known provider families, sorted by name.
fn families(values: Vec<String>) -> Result<Vec<ProviderFamily>, ReportQueryError> {
    let names = parsed(values, |family| {
        ProviderFamily::from_str(family.trim())
            .ok()
            .map(ProviderFamily::as_str)
    })
    .map_err(|error| match error {
        ReportQueryError::InvalidFilterValue => ReportQueryError::UnknownProviderFamily,
        other => other,
    })?;
    Ok(names
        .into_iter()
        .filter_map(|name| ProviderFamily::from_str(name).ok())
        .collect())
}

/// Labels match exactly as stored, surrounding spaces included.
fn labels(values: Vec<String>) -> Result<Vec<String>, ReportQueryError> {
    if values
        .iter()
        .any(|value| value.is_empty() || value.len() > MAX_LABEL_LENGTH)
    {
        return Err(ReportQueryError::InvalidLabel);
    }
    parsed(values, |value| Some(value.to_owned()))
}

/// A list filter's values parsed, sorted and deduplicated; at most
/// [`MAX_FILTER_VALUES`].
pub(crate) fn parse_filter<T: Ord>(
    values: Vec<String>,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, ReportQueryError> {
    parsed(values, parse)
}

/// A label list filter: values match exactly as stored.
pub(crate) fn labels_filter(values: Vec<String>) -> Result<Vec<String>, ReportQueryError> {
    labels(values)
}

/// Known provider families' names, sorted.
pub(crate) fn provider_families_filter(
    values: Vec<String>,
) -> Result<Vec<String>, ReportQueryError> {
    Ok(families(values)?
        .into_iter()
        .map(|family| family.as_str().to_owned())
        .collect())
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn usage_query_rejects_inverted_and_oversized_windows() {
        let from = at("2026-01-01T00:00:00Z");
        let query = |to| {
            UsageQuery::new(
                from,
                to,
                vec![UsageGroupBy::Day],
                UsageFilterInput::default(),
                None,
            )
        };
        assert_eq!(query(from), Err(ReportQueryError::InvalidWindow));
        assert_eq!(
            query(at("2027-01-03T00:00:00Z")),
            Err(ReportQueryError::WindowTooLarge)
        );
        assert!(query(at("2026-02-01T00:00:00Z")).is_ok());
    }

    #[test]
    fn filters_normalize_every_dimension() {
        let query = UsageQuery::new(
            at("2026-01-01T00:00:00Z"),
            at("2026-02-01T00:00:00Z"),
            vec![UsageGroupBy::Team, UsageGroupBy::Model],
            UsageFilterInput {
                models: vec!["m".into(), "a".into(), "m".into()],
                provider_families: vec!["azure-openai".into(), "azure-ai-foundry".into()],
                locations: vec!["eastus".into()],
                ..UsageFilterInput::default()
            },
            Some(Currency::new("USD").unwrap()),
        )
        .unwrap();
        let filters: serde_json::Value = serde_json::from_str(&query.normalized_filters()).unwrap();
        assert_eq!(
            filters,
            serde_json::json!({
                "from": "2026-01-01T00:00:00.000000Z",
                "to": "2026-02-01T00:00:00.000000Z",
                "group_by": "team,model",
                "team": [],
                "service_account": [],
                "model": ["a", "m"],
                "deployment": [],
                "provider_family": ["azure-ai-foundry", "azure-openai"],
                "location": ["eastus"],
                "currency": "USD",
            })
        );
        // Labels containing separators cannot collide with another query.
        let usage = |model: &str, deployment: &str| {
            UsageQuery::new(
                at("2026-01-01T00:00:00Z"),
                at("2026-02-01T00:00:00Z"),
                vec![UsageGroupBy::Day],
                UsageFilterInput {
                    models: vec![model.into()],
                    deployments: vec![deployment.into()],
                    ..UsageFilterInput::default()
                },
                None,
            )
            .unwrap()
            .normalized_filters()
        };
        assert_ne!(
            usage("gpt", "d;deployment=e"),
            usage("gpt;deployment=d", "e")
        );
        assert_eq!(
            TraceQuery::new(
                None,
                None,
                TraceFilterInput {
                    models: vec![String::new()],
                    ..TraceFilterInput::default()
                }
            ),
            Err(ReportQueryError::InvalidLabel)
        );
    }

    #[test]
    fn trace_filters_are_canonical_lists() {
        let team = TeamId::new();
        let query = TraceQuery::new(
            None,
            None,
            TraceFilterInput {
                team_ids: vec![team.0.to_string(), team.0.to_string()],
                models: vec!["b".into(), "a".into(), "b".into()],
                provider_families: vec!["azure-ai-foundry".into()],
                statuses: vec!["pending".into()],
                search: Some("  gpt ".into()),
                ..TraceFilterInput::default()
            },
        )
        .unwrap();
        assert_eq!(query.team_ids, vec![team]);
        assert_eq!(query.team(), Some(team));
        assert_eq!(query.models, vec!["a", "b"]);
        assert_eq!(query.statuses, vec![TraceStatus::Pending]);
        assert_eq!(query.search.as_deref(), Some("gpt"));
        // Labels are kept as given, spaces included.
        let spaced = TraceQuery::new(
            None,
            None,
            TraceFilterInput {
                models: vec![" eastus ".into()],
                ..TraceFilterInput::default()
            },
        )
        .unwrap();
        assert_eq!(spaced.models, vec![" eastus "]);
        let invalid = |input: TraceFilterInput| TraceQuery::new(None, None, input).unwrap_err();
        assert_eq!(
            invalid(TraceFilterInput {
                team_ids: vec!["not-a-uuid".into()],
                ..TraceFilterInput::default()
            }),
            ReportQueryError::InvalidFilterValue
        );
        assert_eq!(
            invalid(TraceFilterInput {
                statuses: vec!["done".into()],
                ..TraceFilterInput::default()
            }),
            ReportQueryError::InvalidFilterValue
        );
        assert_eq!(
            invalid(TraceFilterInput {
                provider_families: vec!["nope".into()],
                ..TraceFilterInput::default()
            }),
            ReportQueryError::UnknownProviderFamily
        );
        assert_eq!(
            invalid(TraceFilterInput {
                models: vec!["m".into(); MAX_FILTER_VALUES + 1],
                ..TraceFilterInput::default()
            }),
            ReportQueryError::InvalidFilterValue
        );
        assert_eq!(
            invalid(TraceFilterInput {
                search: Some("x".repeat(MAX_SEARCH_LENGTH + 1)),
                ..TraceFilterInput::default()
            }),
            ReportQueryError::InvalidSearch
        );
    }

    #[test]
    fn list_filters_split_at_unescaped_commas() {
        assert_eq!(split_list("a,b"), vec!["a", "b"]);
        assert_eq!(split_list("east\\,us,west"), vec!["east,us", "west"]);
        assert_eq!(split_list("a\\\\,b"), vec!["a\\", "b"]);
        assert_eq!(split_list("a\\b"), vec!["a\\b"]);
        assert_eq!(split_list("a\\"), vec!["a\\"]);
        assert_eq!(split_list(""), vec![""]);
    }

    #[test]
    fn trace_summaries_group_by_one_or_two_dimensions() {
        assert_eq!(
            TraceDimension::parse_list("team, model").unwrap(),
            vec![TraceDimension::Team, TraceDimension::Model]
        );
        for bad in ["", "team,team", "team,model,day", "region"] {
            assert_eq!(
                TraceDimension::parse_list(bad),
                Err(ReportQueryError::InvalidGroupBy),
                "{bad}"
            );
        }
    }

    #[test]
    fn usage_groups_and_filters_by_model_master() {
        assert_eq!(
            UsageGroupBy::parse_list("model_master,provider_family").unwrap(),
            vec![UsageGroupBy::ModelMaster, UsageGroupBy::ProviderFamily]
        );
        let query = |masters: Vec<&str>| {
            UsageQuery::new(
                at("2026-01-01T00:00:00Z"),
                at("2026-02-01T00:00:00Z"),
                vec![UsageGroupBy::Day],
                UsageFilterInput {
                    model_masters: masters.into_iter().map(str::to_owned).collect(),
                    ..Default::default()
                },
                None,
            )
        };
        let filtered = query(vec![" org/qwen ", "anthropic/claude-sonnet-4-6"]).unwrap();
        assert_eq!(
            filtered.model_masters,
            ["anthropic/claude-sonnet-4-6", "org/qwen"]
        );
        assert!(filtered.normalized_filters().contains("\"model_master\""));
        // Unset, a page token made before the filter existed still binds.
        assert!(
            !query(vec![])
                .unwrap()
                .normalized_filters()
                .contains("model_master")
        );
        for bad in ["claude", "a/b/c", "Org/X"] {
            assert_eq!(
                query(vec![bad]),
                Err(ReportQueryError::InvalidFilterValue),
                "{bad}"
            );
        }
    }

    #[test]
    fn usage_groups_by_one_or_two_dimensions() {
        assert_eq!(
            UsageGroupBy::parse_list("team,model").unwrap(),
            vec![UsageGroupBy::Team, UsageGroupBy::Model]
        );
        for bad in ["", "team,team", "team,model,day", "status"] {
            assert_eq!(
                UsageGroupBy::parse_list(bad),
                Err(ReportQueryError::InvalidUsageGroupBy),
                "{bad}"
            );
        }
        let query = |group_by: Vec<UsageGroupBy>| {
            UsageQuery::new(
                at("2026-01-01T00:00:00Z"),
                at("2026-02-01T00:00:00Z"),
                group_by,
                UsageFilterInput::default(),
                None,
            )
        };
        assert_eq!(query(vec![]), Err(ReportQueryError::InvalidUsageGroupBy));
        assert_eq!(
            query(vec![UsageGroupBy::Day, UsageGroupBy::Day]),
            Err(ReportQueryError::InvalidUsageGroupBy)
        );
        assert_eq!(
            query(vec![UsageGroupBy::Team, UsageGroupBy::Model])
                .unwrap()
                .group_by_name(),
            "team,model"
        );
    }

    #[test]
    fn usage_filters_accept_only_known_families_and_bounded_locations() {
        let query = |family: Option<&str>, location: Option<&str>| {
            UsageQuery::new(
                at("2026-01-01T00:00:00Z"),
                at("2026-02-01T00:00:00Z"),
                vec![UsageGroupBy::ProviderFamily],
                UsageFilterInput {
                    provider_families: family.into_iter().map(str::to_owned).collect(),
                    locations: location.into_iter().map(str::to_owned).collect(),
                    ..UsageFilterInput::default()
                },
                None,
            )
        };
        for family in ProviderFamily::ALL {
            assert_eq!(
                query(Some(family.as_str()), None)
                    .unwrap()
                    .provider_families,
                vec![family]
            );
        }
        assert_eq!(
            query(Some("azure"), None),
            Err(ReportQueryError::UnknownProviderFamily)
        );
        assert_eq!(query(None, Some("")), Err(ReportQueryError::InvalidLabel));
    }

    #[test]
    fn wire_names_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&ReportConfidence::Estimated).unwrap(),
            "\"estimated\""
        );
        assert_eq!(
            serde_json::to_string(&UsageGroupBy::ServiceAccount).unwrap(),
            "\"service_account\""
        );
        for group_by in [UsageGroupBy::ProviderFamily, UsageGroupBy::Location] {
            assert_eq!(serde_json::to_value(group_by).unwrap(), group_by.as_str());
        }
        let row = UsageReportRow {
            group: None,
            values: [("team", None), ("model", Some("m".to_owned()))]
                .into_iter()
                .collect(),
            aggregate: UsageAggregate {
                currency: "USD".into(),
                confidence: ReportConfidence::Unknown,
                request_count: 1,
                amount_minor: 0,
                unpriced_request_count: 1,
                incomplete_request_count: 1,
                tokens: TokenTotals::default(),
            },
        };
        let value = serde_json::to_value(row).unwrap();
        assert_eq!(value["group"], serde_json::Value::Null);
        assert_eq!(
            value["values"],
            serde_json::json!({ "team": null, "model": "m" })
        );
        assert_eq!(value["confidence"], "unknown");
        assert_eq!(value["unpriced_request_count"], 1);
    }
}
