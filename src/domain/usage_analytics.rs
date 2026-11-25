//! Usage dashboard read models: requests sliced by time and by who, what and
//! how, as time series and as a list of the requests themselves.
//!
//! The unit is one request: its trace, with the latest usage recorded for its
//! reservation. Money stays per currency and is never summed across
//! currencies; a request with usage but no amount counts as unpriced, and one
//! with no usage at all (it failed before the provider reported any, or is
//! still running) counts as `no_usage`. Times are UTC.

use std::{collections::BTreeMap, fmt};

use chrono::{DateTime, Duration, DurationRound, SecondsFormat, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::{
    accounting::Currency,
    identity::{ServiceAccountId, TeamId},
    reporting::{
        CurrencyAmount, MAX_USAGE_WINDOW_DAYS, ReportConfidence, ReportQueryError, TokenTotals,
        TraceTokens, labels_filter, parse_filter, provider_families_filter,
    },
};

/// Most series one query returns besides `other`.
pub const MAX_SERIES: u32 = 20;
pub const DEFAULT_SERIES: u32 = 8;
/// Most buckets one series query covers: 31 days by the hour.
pub const MAX_BUCKETS: i64 = 744;

/// What a request can be sliced by.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDimension {
    Team,
    ServiceAccount,
    ApiKey,
    /// The service account's current owner: inference authenticates with
    /// service account keys, so a request has no user of its own.
    User,
    /// The app's end user the request named (D-028).
    EndUser,
    Model,
    /// The public model alias the request named.
    Alias,
    Deployment,
    ProviderFamily,
    Location,
    /// `chat`, `chat_stream`, `embeddings`, `chat_batch` or `embeddings_batch`.
    Operation,
    /// `succeeded`, `failed`, `pending`, `expired` (never reached the provider), `unknown` (closed by expiry after a claim) or `failed-over`.
    Status,
    /// How accounting closed the request: `settled`, `unknown`, ...
    Outcome,
    /// The gateway's category for a failed provider request.
    ErrorCategory,
    /// The master record (one model across providers, D-033) the
    /// deployment was linked to when the request was served.
    ModelMaster,
}

impl UsageDimension {
    pub const ALL: [Self; 15] = [
        Self::Team,
        Self::ServiceAccount,
        Self::ApiKey,
        Self::User,
        Self::EndUser,
        Self::Model,
        Self::Alias,
        Self::Deployment,
        Self::ProviderFamily,
        Self::Location,
        Self::Operation,
        Self::Status,
        Self::Outcome,
        Self::ErrorCategory,
        Self::ModelMaster,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::ServiceAccount => "service_account",
            Self::ApiKey => "api_key",
            Self::User => "user",
            Self::EndUser => "end_user",
            Self::Model => "model",
            Self::Alias => "alias",
            Self::Deployment => "deployment",
            Self::ProviderFamily => "provider_family",
            Self::Location => "location",
            Self::Operation => "operation",
            Self::Status => "status",
            Self::Outcome => "outcome",
            Self::ErrorCategory => "error_category",
            Self::ModelMaster => "model_master",
        }
    }

    pub fn parse(value: &str) -> Result<Self, UsageQueryError> {
        Self::ALL
            .into_iter()
            .find(|dimension| dimension.as_str() == value.trim())
            .ok_or(UsageQueryError::InvalidSplitBy)
    }

    /// Whether the value is an id the dashboard shows by name.
    pub const fn is_identity(self) -> bool {
        matches!(
            self,
            Self::Team | Self::ServiceAccount | Self::ApiKey | Self::User
        )
    }
}

/// Whether a request succeeded, failed at the provider, or is still running.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Succeeded,
    Failed,
    Pending,
    /// Expired before the provider was called.
    Expired,
    /// Closed by expiry after a claim; the provider's result is unknown.
    Unknown,
    /// Refused by the provider before any work and tried again on another
    /// deployment of its route, whose own trace tells how it went (D-023).
    #[serde(rename = "failed-over")]
    FailedOver,
}

impl RequestStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Pending => "pending",
            Self::Expired => "expired",
            Self::Unknown => "unknown",
            Self::FailedOver => "failed-over",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "pending" => Some(Self::Pending),
            "expired" => Some(Self::Expired),
            "unknown" => Some(Self::Unknown),
            "failed-over" => Some(Self::FailedOver),
            _ => None,
        }
    }
}

/// Bucket width of a series. Days start at 00:00 UTC, weeks on Monday.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Hour,
    #[default]
    Day,
    Week,
}

impl Granularity {
    pub fn parse(value: &str) -> Result<Self, UsageQueryError> {
        match value.trim() {
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            _ => Err(UsageQueryError::InvalidGranularity),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
        }
    }

    fn width(self) -> Duration {
        match self {
            Self::Hour => Duration::hours(1),
            Self::Day => Duration::days(1),
            Self::Week => Duration::weeks(1),
        }
    }

    /// The start of the bucket holding `at`.
    pub fn floor(self, at: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Hour | Self::Day => at
                .duration_trunc(self.width())
                .expect("a UTC instant truncates to an hour or a day"),
            Self::Week => {
                let day = at
                    .duration_trunc(Duration::days(1))
                    .expect("a UTC instant truncates to a day");
                let offset = i64::from(chrono::Datelike::weekday(&day).num_days_from_monday());
                day - Duration::days(offset)
            }
        }
    }

    /// Every bucket start overlapping `[from, to)`.
    pub fn buckets(self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let mut buckets = Vec::new();
        let mut at = self.floor(from);
        while at < to {
            buckets.push(at);
            at += self.width();
        }
        buckets
    }
}

/// What a series query ranks its top series by.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RankBy {
    #[default]
    Requests,
    /// Input plus output tokens.
    Tokens,
    /// The amount in the query's one currency; needs a `currency` filter.
    Cost,
}

impl RankBy {
    pub fn parse(value: &str) -> Result<Self, UsageQueryError> {
        match value.trim() {
            "requests" => Ok(Self::Requests),
            "tokens" => Ok(Self::Tokens),
            "cost" => Ok(Self::Cost),
            _ => Err(UsageQueryError::InvalidRankBy),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requests => "requests",
            Self::Tokens => "tokens",
            Self::Cost => "cost",
        }
    }
}

/// Filters as given: each list matches any of its values, and an empty list
/// does not filter.
#[derive(Clone, Debug, Default)]
pub struct UsageSliceInput {
    pub team_ids: Vec<String>,
    pub service_account_ids: Vec<String>,
    pub api_key_ids: Vec<String>,
    pub user_ids: Vec<String>,
    pub end_users: Vec<String>,
    pub models: Vec<String>,
    pub aliases: Vec<String>,
    pub deployments: Vec<String>,
    pub provider_families: Vec<String>,
    pub locations: Vec<String>,
    pub operations: Vec<String>,
    pub statuses: Vec<String>,
    pub outcomes: Vec<String>,
    pub error_categories: Vec<String>,
    /// Batches, as `batch_<uuid>` or the bare UUID: their lines' requests.
    pub batch_ids: Vec<String>,
    /// Master records (`anthropic/claude-sonnet-4-6`, `org/<slug>`).
    pub model_masters: Vec<String>,
}

/// A validated slice of requests: those recorded in `[from, to)` that match
/// every filter. Lists are sorted and deduplicated, so one slice has one
/// canonical form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSlice {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub team_ids: Vec<TeamId>,
    pub service_account_ids: Vec<ServiceAccountId>,
    pub api_key_ids: Vec<Uuid>,
    pub user_ids: Vec<Uuid>,
    /// The app's IDs for its end users, as sent (D-028).
    pub end_users: Vec<String>,
    pub models: Vec<String>,
    pub aliases: Vec<String>,
    pub deployments: Vec<String>,
    pub provider_families: Vec<String>,
    pub locations: Vec<String>,
    pub operations: Vec<String>,
    pub statuses: Vec<RequestStatus>,
    pub outcomes: Vec<String>,
    pub error_categories: Vec<String>,
    /// The lines of these batches (D-021).
    pub batch_ids: Vec<Uuid>,
    /// Requests served by deployments linked to these masters then.
    pub model_masters: Vec<String>,
    pub currency: Option<Currency>,
}

impl UsageSlice {
    pub fn new(
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        input: UsageSliceInput,
        currency: Option<Currency>,
    ) -> Result<Self, UsageQueryError> {
        if from >= to {
            return Err(ReportQueryError::InvalidWindow.into());
        }
        if to - from > Duration::days(MAX_USAGE_WINDOW_DAYS) {
            return Err(ReportQueryError::WindowTooLarge.into());
        }
        let uuids = |values: Vec<String>| parse_filter(values, |id| id.trim().parse::<Uuid>().ok());
        Ok(Self {
            from,
            to,
            team_ids: uuids(input.team_ids)?.into_iter().map(TeamId).collect(),
            service_account_ids: uuids(input.service_account_ids)?
                .into_iter()
                .map(ServiceAccountId)
                .collect(),
            api_key_ids: uuids(input.api_key_ids)?,
            user_ids: uuids(input.user_ids)?,
            end_users: parse_filter(input.end_users, |id| {
                crate::domain::end_user::parse_end_user_id(id).ok()
            })?,
            models: labels_filter(input.models)?,
            aliases: labels_filter(input.aliases)?,
            deployments: labels_filter(input.deployments)?,
            provider_families: provider_families_filter(input.provider_families)?,
            locations: labels_filter(input.locations)?,
            operations: labels_filter(input.operations)?,
            statuses: parse_filter(input.statuses, RequestStatus::parse)?,
            outcomes: labels_filter(input.outcomes)?,
            error_categories: labels_filter(input.error_categories)?,
            batch_ids: parse_filter(input.batch_ids, crate::domain::batch::parse_batch_ref)?,
            model_masters: crate::domain::reporting::model_masters(input.model_masters)?,
            currency,
        })
    }

    /// The same filters over the window just before this one, as long.
    pub fn previous(&self) -> Self {
        Self {
            from: self.from - (self.to - self.from),
            to: self.from,
            ..self.clone()
        }
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
            "from": timestamp(self.from),
            "to": timestamp(self.to),
            "team": self.team_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            "service_account": self.service_account_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
            "api_key": self.api_key_ids,
            "user": self.user_ids,
            "end_user": self.end_users,
            "model": self.models,
            "alias": self.aliases,
            "deployment": self.deployments,
            "provider_family": self.provider_families,
            "location": self.locations,
            "operation": self.operations,
            "status": self.statuses.iter().map(|status| status.as_str()).collect::<Vec<_>>(),
            "outcome": self.outcomes,
            "error_category": self.error_categories,
            "currency": self.currency.as_ref().map(Currency::as_str),
        });
        // Only when set, so a page token made before the filter existed
        // still binds to the same slice.
        if !self.batch_ids.is_empty() {
            filters["batch"] = serde_json::json!(self.batch_ids);
        }
        if !self.model_masters.is_empty() {
            filters["model_master"] = serde_json::json!(self.model_masters);
        }
        filters.to_string()
    }
}

/// A slice charted over time: bucketed by `granularity`, optionally split by
/// one dimension into its top `limit` values and `other`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSeriesQuery {
    pub slice: UsageSlice,
    pub granularity: Granularity,
    pub split_by: Option<UsageDimension>,
    pub limit: u32,
    pub rank_by: RankBy,
}

impl UsageSeriesQuery {
    pub fn new(
        slice: UsageSlice,
        granularity: Granularity,
        split_by: Option<UsageDimension>,
        limit: Option<u32>,
        rank_by: RankBy,
    ) -> Result<Self, UsageQueryError> {
        let limit = limit.unwrap_or(DEFAULT_SERIES);
        if !(1..=MAX_SERIES).contains(&limit) {
            return Err(UsageQueryError::InvalidLimit);
        }
        if rank_by == RankBy::Cost && slice.currency.is_none() {
            return Err(UsageQueryError::CostNeedsCurrency);
        }
        if granularity.buckets(slice.from, slice.to).len() as i64 > MAX_BUCKETS {
            return Err(UsageQueryError::TooManyBuckets);
        }
        Ok(Self {
            slice,
            granularity,
            split_by,
            limit,
            rank_by,
        })
    }
}

/// How long requests took, in milliseconds, from admission to settlement:
/// provider time plus the gateway's own. Pending requests are left out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DurationStats {
    pub average_ms: Option<i64>,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
}

/// What a set of requests amounts to. `amounts` holds one entry per currency.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UsageMeasures {
    pub requests: i64,
    pub failed_requests: i64,
    pub pending_requests: i64,
    /// Reservations that expired before the provider was called.
    pub expired_requests: i64,
    /// Requests the expiry worker closed after a claim: the provider's
    /// result is unknown until settled or resolved.
    pub unknown_requests: i64,
    pub tokens: TokenTotals,
    pub amounts: Vec<CurrencyAmount>,
    /// Requests with usage but no amount: not counted in `amounts`.
    pub unpriced_requests: i64,
    /// Requests with no usage recorded: no tokens, no amount.
    pub no_usage_requests: i64,
    /// Requests whose usage was partial or unavailable.
    pub incomplete_requests: i64,
    pub duration: DurationStats,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsagePoint {
    pub bucket: DateTime<Utc>,
    #[serde(flatten)]
    pub measures: UsageMeasures,
}

/// One series: the requests with one value of the split dimension (or all of
/// them, unsplit), or `other` for the values past the top `limit`. Points are
/// sparse: a bucket without requests has none.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageSeries {
    /// `null` for `other`, for an unsplit query, and for requests without
    /// the value (organization-level usage split by team, for example).
    pub value: Option<String>,
    /// The value's name, for teams, service accounts, API keys (their
    /// prefix) and users.
    pub label: Option<String>,
    pub other: bool,
    pub totals: UsageMeasures,
    pub points: Vec<UsagePoint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PeriodTotals {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub totals: UsageMeasures,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageSeriesReport {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub granularity: Granularity,
    pub split_by: Option<UsageDimension>,
    pub rank_by: RankBy,
    /// Every bucket's start, so a chart can draw empty ones.
    pub buckets: Vec<DateTime<Utc>>,
    pub totals: UsageMeasures,
    /// The same filters over the window just before this one.
    pub previous: PeriodTotals,
    /// Largest first by `rank_by`; `other` last.
    pub series: Vec<UsageSeries>,
    /// Whether values past the top `limit` were folded into `other`.
    pub truncated: bool,
}

/// Names for the identities a request names.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct EventLabels {
    pub team: Option<String>,
    pub service_account: Option<String>,
    /// The key's public prefix.
    pub api_key: Option<String>,
    pub user: Option<String>,
}

/// One request in a slice. `id` is its trace id: the trace view and its
/// content read take it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageEvent {
    pub id: Uuid,
    /// The request id the gateway returned to the caller.
    pub correlation_id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub team_id: Option<TeamId>,
    pub service_account_id: Option<ServiceAccountId>,
    pub api_key_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub labels: EventLabels,
    pub model: String,
    pub alias: Option<String>,
    pub deployment: String,
    pub provider_family: Option<String>,
    pub location: Option<String>,
    pub operation: Option<String>,
    pub status: RequestStatus,
    pub outcome: Option<String>,
    pub error_category: Option<String>,
    pub tokens: TraceTokens,
    pub currency: String,
    pub amount_minor: Option<i64>,
    pub usage_confidence: Option<ReportConfidence>,
    /// What the trace holds of the request's content: `stored`, `writing`,
    /// `off`, `unavailable`, `failed` or `expired`; `null` when tracing was
    /// not on for it. The metadata trace exists either way.
    pub content_state: Option<String>,
    /// The batch the request was a line of (`batch_<uuid>`); `null` for a
    /// request made on its own.
    pub batch_id: Option<String>,
    /// The line's `custom_id` in its batch.
    pub custom_id: Option<String>,
    /// The master record its deployment was linked to when it was served
    /// (D-033); `null` for none.
    pub model_master: Option<String>,
}

/// Sums measures that were counted apart (the points of one series).
/// Durations cannot be summed and are left to the caller.
pub fn add_counts(total: &mut UsageMeasures, other: &UsageMeasures) {
    total.requests += other.requests;
    total.failed_requests += other.failed_requests;
    total.pending_requests += other.pending_requests;
    total.expired_requests += other.expired_requests;
    total.unknown_requests += other.unknown_requests;
    total.tokens.input += other.tokens.input;
    total.tokens.output += other.tokens.output;
    total.tokens.cached_input += other.tokens.cached_input;
    total.tokens.reasoning += other.tokens.reasoning;
    total.tokens.other += other.tokens.other;
    total.unpriced_requests += other.unpriced_requests;
    total.no_usage_requests += other.no_usage_requests;
    total.incomplete_requests += other.incomplete_requests;
    let mut amounts: BTreeMap<String, i64> = total
        .amounts
        .drain(..)
        .map(|amount| (amount.currency, amount.amount_minor))
        .collect();
    for amount in &other.amounts {
        *amounts.entry(amount.currency.clone()).or_default() += amount.amount_minor;
    }
    total.amounts = amounts
        .into_iter()
        .map(|(currency, amount_minor)| CurrencyAmount {
            currency,
            amount_minor,
        })
        .collect();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageQueryError {
    Report(ReportQueryError),
    InvalidGranularity,
    InvalidSplitBy,
    InvalidRankBy,
    InvalidLimit,
    CostNeedsCurrency,
    TooManyBuckets,
}

impl From<ReportQueryError> for UsageQueryError {
    fn from(error: ReportQueryError) -> Self {
        Self::Report(error)
    }
}

impl fmt::Display for UsageQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Report(error) => error.fmt(f),
            Self::InvalidGranularity => f.write_str("granularity must be hour, day or week"),
            Self::InvalidSplitBy => f.write_str(
                "split_by must be one of team, service_account, api_key, user, model, alias, deployment, provider_family, location, operation, status, outcome or error_category",
            ),
            Self::InvalidRankBy => f.write_str("rank_by must be requests, tokens or cost"),
            Self::InvalidLimit => f.write_str("limit must be 1 to 20"),
            Self::CostNeedsCurrency => {
                f.write_str("rank_by=cost needs a currency: amounts are never summed across currencies")
            }
            Self::TooManyBuckets => f.write_str(
                "the window holds more than 744 buckets at this granularity; choose a coarser one",
            ),
        }
    }
}
impl std::error::Error for UsageQueryError {}

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

    fn slice(from: &str, to: &str) -> UsageSlice {
        UsageSlice::new(at(from), at(to), UsageSliceInput::default(), None).unwrap()
    }

    #[test]
    fn every_status_reads_back_as_it_is_written() {
        for status in [
            RequestStatus::Succeeded,
            RequestStatus::Failed,
            RequestStatus::Pending,
            RequestStatus::Expired,
            RequestStatus::Unknown,
            RequestStatus::FailedOver,
        ] {
            let written = serde_json::to_value(status).unwrap();
            assert_eq!(written, status.as_str());
            assert_eq!(RequestStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn buckets_cover_the_window_from_its_floor() {
        let hours =
            Granularity::Hour.buckets(at("2026-11-01T10:30:00Z"), at("2026-11-01T12:00:00Z"));
        assert_eq!(
            hours,
            vec![at("2026-11-01T10:00:00Z"), at("2026-11-01T11:00:00Z")]
        );
        // 2026-11-04 is a Wednesday; its week starts Monday 2026-11-02.
        let weeks =
            Granularity::Week.buckets(at("2026-11-04T08:00:00Z"), at("2026-11-10T00:00:00Z"));
        assert_eq!(
            weeks,
            vec![at("2026-11-02T00:00:00Z"), at("2026-11-09T00:00:00Z")]
        );
        let days = Granularity::Day.buckets(at("2026-11-01T00:00:00Z"), at("2026-11-03T00:00:00Z"));
        assert_eq!(days.len(), 2);
    }

    #[test]
    fn series_queries_are_bounded() {
        let month = slice("2026-10-01T00:00:00Z", "2026-11-01T00:00:00Z");
        assert!(
            UsageSeriesQuery::new(
                month.clone(),
                Granularity::Hour,
                None,
                None,
                RankBy::Requests
            )
            .is_ok()
        );
        let year = slice("2026-01-01T00:00:00Z", "2026-12-01T00:00:00Z");
        assert_eq!(
            UsageSeriesQuery::new(
                year.clone(),
                Granularity::Hour,
                None,
                None,
                RankBy::Requests
            ),
            Err(UsageQueryError::TooManyBuckets)
        );
        assert!(
            UsageSeriesQuery::new(year, Granularity::Day, None, None, RankBy::Requests).is_ok()
        );
        assert_eq!(
            UsageSeriesQuery::new(
                month.clone(),
                Granularity::Day,
                None,
                Some(21),
                RankBy::Requests
            ),
            Err(UsageQueryError::InvalidLimit)
        );
        assert_eq!(
            UsageSeriesQuery::new(month, Granularity::Day, None, None, RankBy::Cost),
            Err(UsageQueryError::CostNeedsCurrency)
        );
    }

    #[test]
    fn slices_validate_and_normalize_filters() {
        // A batch filter binds page tokens only when set, either way it is named.
        let id = Uuid::new_v4();
        let by = |batch_ids: Vec<String>| {
            UsageSlice::new(
                at("2026-11-01T00:00:00Z"),
                at("2026-11-02T00:00:00Z"),
                UsageSliceInput {
                    batch_ids,
                    ..Default::default()
                },
                None,
            )
        };
        let named = by(vec![format!("batch_{id}")]).unwrap();
        assert_eq!(named.batch_ids, vec![id]);
        assert_eq!(
            named.normalized_filters(),
            by(vec![id.to_string()]).unwrap().normalized_filters()
        );
        assert!(
            !by(Vec::new())
                .unwrap()
                .normalized_filters()
                .contains("batch")
        );
        assert!(by(vec!["batch_nope".into()]).is_err());
        let input = UsageSliceInput {
            statuses: vec!["failed".into(), "succeeded".into(), "failed".into()],
            api_key_ids: vec!["not-a-uuid".into()],
            ..Default::default()
        };
        assert_eq!(
            UsageSlice::new(
                at("2026-11-01T00:00:00Z"),
                at("2026-11-02T00:00:00Z"),
                input,
                None
            ),
            Err(UsageQueryError::Report(
                ReportQueryError::InvalidFilterValue
            ))
        );
        let input = UsageSliceInput {
            statuses: vec!["failed".into(), "succeeded".into(), "failed".into()],
            ..Default::default()
        };
        let slice = UsageSlice::new(
            at("2026-11-01T00:00:00Z"),
            at("2026-11-02T00:00:00Z"),
            input,
            None,
        )
        .unwrap();
        assert_eq!(
            slice.statuses,
            vec![RequestStatus::Succeeded, RequestStatus::Failed]
        );
        let previous = slice.previous();
        assert_eq!(previous.from, at("2026-10-31T00:00:00Z"));
        assert_eq!(previous.to, slice.from);
        assert_ne!(previous.normalized_filters(), slice.normalized_filters());
        assert_eq!(
            UsageSlice::new(slice.to, slice.from, UsageSliceInput::default(), None),
            Err(UsageQueryError::Report(ReportQueryError::InvalidWindow))
        );
    }

    #[test]
    fn counts_add_and_amounts_stay_per_currency() {
        let mut total = UsageMeasures {
            requests: 1,
            amounts: vec![CurrencyAmount {
                currency: "USD".into(),
                amount_minor: 5,
            }],
            ..Default::default()
        };
        add_counts(
            &mut total,
            &UsageMeasures {
                requests: 2,
                amounts: vec![
                    CurrencyAmount {
                        currency: "EUR".into(),
                        amount_minor: 7,
                    },
                    CurrencyAmount {
                        currency: "USD".into(),
                        amount_minor: 1,
                    },
                ],
                ..Default::default()
            },
        );
        assert_eq!(total.requests, 3);
        assert_eq!(
            total.amounts,
            vec![
                CurrencyAmount {
                    currency: "EUR".into(),
                    amount_minor: 7
                },
                CurrencyAmount {
                    currency: "USD".into(),
                    amount_minor: 6
                },
            ]
        );
    }
}
