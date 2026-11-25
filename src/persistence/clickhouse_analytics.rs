//! The usage dashboard, the usage report and trace search, read from the
//! ClickHouse analytics sink when it is live (D-030; `docs/operations.md`,
//! *Analytics in ClickHouse*). Budgets, settlement and reconciliation never
//! read from here.
//!
//! Each method mirrors a PostgreSQL query in `usage_analytics` or
//! `reporting` and returns what that query would: the same filters, groups,
//! measures, order and pages (`tests/clickhouse_analytics_integration.rs`
//! runs both on the same data and compares). A method returns `Ok(None)`
//! when ClickHouse cannot answer (the sink is not live, it is too far
//! behind, or ClickHouse failed), and the caller then reads PostgreSQL.
//!
//! Every query is scoped to one organization: the `src` subquery every
//! statement reads from is the organization's rows of `requests` (less the
//! overlay's) and the organization's overlay rows, and the team clamp and
//! filters apply on top.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::clickhouse::{ClickHouse, ClickHouseError, ExternalTable, Param};
use crate::domain::{
    identity::{OrganizationId, TeamId},
    reporting::{
        CurrencyAmount, TraceDimension, TraceGroup, TraceQuery, TraceSummary, UsageAggregate,
        UsageGroupBy, UsageQuery, UsageReportRow,
    },
    usage_analytics::{
        Granularity, RankBy, UsageDimension, UsageEvent, UsageSeriesQuery, UsageSlice,
    },
};
use crate::metrics::{ANALYTICS_READS, global as metrics};

use super::analytics_sink::{AnalyticsSinkStore, NotReady, Overlay, ROW_COLUMNS, ROW_STRUCTURE};
use super::reporting::{
    AggregateRow, ReportingStore, ReportingStoreError, TimeKeyset, UsageKeyset, report_rows,
};
use super::usage_analytics::{
    MeasureRow, SeriesReport, UsageAnalyticsStore, assemble_series, assemble_totals,
};

/// How long a process trusts the store id it last read from its
/// ClickHouse before reading it again. A read uses ClickHouse only when
/// that id is the one PostgreSQL records the copy was made to, so for at
/// most this long after its store is emptied or replaced behind the same
/// URL, a process may still read it (the worker's own check then turns
/// the sink back to copying within a minute). A different URL is a
/// different client with no cached id, so a move is seen at once.
pub const STORE_ID_CACHE: Duration = Duration::from_secs(5);

/// Reads the analytics queries from ClickHouse.
#[derive(Clone)]
pub struct ClickHouseAnalytics {
    clickhouse: ClickHouse,
    /// The store id last read from `clickhouse`, and when.
    store: Arc<Mutex<Option<(Instant, Option<Uuid>)>>>,
    sink: AnalyticsSinkStore,
    analytics: UsageAnalyticsStore,
    reporting: ReportingStore,
}

impl std::fmt::Debug for ClickHouseAnalytics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseAnalytics")
            .field("clickhouse", &self.clickhouse)
            .finish_non_exhaustive()
    }
}

/// A query's text, its parameters and the external tables it reads.
struct Statement {
    sql: String,
    params: Vec<Param>,
}

/// `WHERE` conditions and the parameters they name.
#[derive(Default)]
struct Conditions {
    parts: Vec<String>,
    params: Vec<Param>,
    /// Whether the `until` bound on the sort key is set ([`Self::before`]).
    until: bool,
}

/// How much later than its trace's `recorded_at` a request's `created_at`
/// can be. The reservation takes `clock_timestamp()` while the trace,
/// inserted in the same transaction, takes the transaction's start, so
/// `created_at` trails by as long as admission's transaction ran
/// (milliseconds); usage rows come later still. An hour is ample and still
/// lets a window's end prune every later partition and granule.
const CREATED_SLACK: chrono::Duration = chrono::Duration::hours(1);

impl Conditions {
    /// The window ends at `to`: bounds `created_at` (the sort key, so
    /// ClickHouse reads no part or granule past it) at `to` plus
    /// [`CREATED_SLACK`]. Without it FINAL reads the organization's whole
    /// history.
    fn before(&mut self, to: DateTime<Utc>) {
        self.until = true;
        self.param(Param::time("until", to + CREATED_SLACK));
    }

    fn push(&mut self, condition: impl Into<String>) {
        self.parts.push(condition.into());
    }

    fn param(&mut self, param: Param) {
        self.params.push(param);
    }

    /// `column` is one of `values`; no condition when empty.
    fn uuids(&mut self, column: &str, name: &'static str, values: &[Uuid]) {
        if !values.is_empty() {
            self.push(format!("has({{{name}:Array(UUID)}}, {column})"));
            self.param(Param::uuids(name, values));
        }
    }

    fn strings(&mut self, column: &str, name: &'static str, values: &[String]) {
        if !values.is_empty() {
            self.push(format!("has({{{name}:Array(String)}}, {column})"));
            self.param(Param::strings(name, values));
        }
    }

    fn clamp(&mut self, column: &str, authorized_teams: Option<&[TeamId]>) {
        if let Some(teams) = authorized_teams {
            let teams: Vec<Uuid> = teams.iter().map(|team| team.0).collect();
            self.push(format!("has({{clamp:Array(UUID)}}, {column})"));
            self.param(Param::uuids("clamp", &teams));
        }
    }

    fn sql(&self) -> String {
        if self.parts.is_empty() {
            "1".to_owned()
        } else {
            self.parts.join("\n  AND ")
        }
    }
}

/// The organization's requests: ClickHouse's newest versions, less the
/// requests the overlay reads from PostgreSQL, plus the overlay's rows.
/// With `users`, each carries its service account's current owner.
fn source(users: bool, conditions: &Conditions) -> String {
    let until = if conditions.until {
        "\n           AND created_at < {until:DateTime64(6, 'UTC')}"
    } else {
        ""
    };
    let rows = format!(
        "SELECT {ROW_COLUMNS} FROM requests FINAL
         WHERE organization_id = {{org:UUID}}{until}
           AND reservation_id NOT IN (SELECT reservation_id FROM overlay_ids)
         UNION ALL
         SELECT {ROW_COLUMNS} FROM overlay WHERE organization_id = {{org:UUID}}"
    );
    if users {
        format!(
            "(SELECT s.*, o.owner_user_id AS user_id FROM ({rows}) AS s
              LEFT JOIN owners AS o ON o.service_account_id = s.service_account_id)"
        )
    } else {
        format!("(SELECT *, CAST(NULL AS Nullable(UUID)) AS user_id FROM ({rows}))")
    }
}

/// `prefer_column_name_to_alias`: a measure named like the column it sums
/// (`sum(input_tokens) AS input_tokens`) reads the column, as in SQL.
const SETTINGS: &str = "SETTINGS group_by_use_nulls = 1, transform_null_in = 1, join_use_nulls = 1, \
    prefer_column_name_to_alias = 1";

/// The dashboard's dimensions over a request row, as text (as PostgreSQL's
/// `::text` gives them).
fn dimension(dimension: UsageDimension) -> &'static str {
    match dimension {
        UsageDimension::Team => "toString(team_id)",
        UsageDimension::ServiceAccount => "toString(service_account_id)",
        UsageDimension::ApiKey => "toString(api_key_id)",
        UsageDimension::User => "toString(user_id)",
        UsageDimension::EndUser => "end_user",
        UsageDimension::Model => "model",
        UsageDimension::Alias => "alias",
        UsageDimension::Deployment => "deployment",
        UsageDimension::ProviderFamily => "provider_family",
        UsageDimension::Location => "location",
        UsageDimension::Operation => "operation",
        UsageDimension::Status => "request_status",
        UsageDimension::Outcome => "outcome",
        UsageDimension::ErrorCategory => "error_category",
        UsageDimension::ModelMaster => "model_master",
    }
}

/// Bucket start in seconds since the epoch, in UTC; weeks start on Monday.
fn bucket(granularity: Granularity) -> &'static str {
    match granularity {
        Granularity::Hour => "toInt64(toUnixTimestamp(toStartOfHour(assumeNotNull(recorded_at))))",
        Granularity::Day => "toInt64(toUnixTimestamp(toStartOfDay(assumeNotNull(recorded_at))))",
        Granularity::Week => {
            "toInt64(toUnixTimestamp(toDateTime(toMonday(assumeNotNull(recorded_at)), 'UTC')))"
        }
    }
}

/// The dashboard's measures. Durations come back as a sum and a count (the
/// average is rounded here, exactly as PostgreSQL rounds a numeric), and
/// percentiles as PostgreSQL's `percentile_disc`: the value at position
/// `ceil(p * n)` of the sorted durations.
const MEASURES: &str = "
  toInt64(count()) AS requests,
  toInt64(countIf(request_status = 'failed')) AS failed_requests,
  toInt64(countIf(request_status = 'pending')) AS pending_requests,
  toInt64(countIf(request_status = 'expired')) AS expired_requests,
  toInt64(countIf(request_status = 'unknown')) AS unknown_requests,
  toInt64(sum(ifNull(input_tokens, 0))) AS input_tokens,
  toInt64(sum(ifNull(output_tokens, 0))) AS output_tokens,
  toInt64(sum(ifNull(cached_input_tokens, 0))) AS cached_input_tokens,
  toInt64(sum(ifNull(reasoning_tokens, 0))) AS reasoning_tokens,
  toInt64(sum(ifNull(other_tokens, 0))) AS other_tokens,
  sum(amount_minor) AS amount_minor,
  toInt64(countIf(has_usage = 1 AND amount_minor IS NULL)) AS unpriced_requests,
  toInt64(countIf(has_usage = 0)) AS no_usage_requests,
  toInt64(countIf(has_usage = 1 AND usage_completeness != 'complete')) AS incomplete_requests,
  sum(duration_ms) AS duration_sum,
  toInt64(count(duration_ms)) AS duration_count,
  if(count(duration_ms) = 0, NULL, arrayElement(arraySort(groupArray(duration_ms)),
     toUInt64(greatest(1, ceil(0.5 * count(duration_ms)))))) AS duration_p50,
  if(count(duration_ms) = 0, NULL, arrayElement(arraySort(groupArray(duration_ms)),
     toUInt64(greatest(1, ceil(0.95 * count(duration_ms)))))) AS duration_p95";

#[derive(Deserialize)]
struct ChMeasureRow {
    #[serde(default)]
    series_key: Option<String>,
    #[serde(default)]
    other: Option<u8>,
    #[serde(default)]
    bucket: Option<i64>,
    currency: Option<String>,
    requests: i64,
    failed_requests: i64,
    pending_requests: i64,
    expired_requests: i64,
    unknown_requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    reasoning_tokens: i64,
    other_tokens: i64,
    amount_minor: Option<i64>,
    unpriced_requests: i64,
    no_usage_requests: i64,
    incomplete_requests: i64,
    duration_sum: Option<i64>,
    duration_count: i64,
    duration_p50: Option<i64>,
    duration_p95: Option<i64>,
}

/// `round(avg)` as PostgreSQL rounds the numeric average of non-negative
/// integers: half away from zero.
fn rounded_average(sum: Option<i64>, count: i64) -> Option<i64> {
    let sum = i128::from(sum?);
    let count = i128::from(count);
    if count == 0 {
        return None;
    }
    let doubled = 2 * sum;
    let rounded = if doubled >= 0 {
        (doubled + count) / (2 * count)
    } else {
        -((-doubled + count) / (2 * count))
    };
    i64::try_from(rounded).ok()
}

impl ChMeasureRow {
    fn into_measure_row(self) -> Result<MeasureRow, ReportingStoreError> {
        let bucket = match self.bucket {
            Some(seconds) => Some(
                DateTime::<Utc>::from_timestamp(seconds, 0)
                    .ok_or(ReportingStoreError::InvalidState)?,
            ),
            None => None,
        };
        Ok(MeasureRow {
            series_key: self.series_key,
            other: self.other.unwrap_or(0) == 1,
            all_buckets: i32::from(bucket.is_none()),
            bucket,
            all_currencies: i32::from(self.currency.is_none()),
            currency: self.currency,
            requests: self.requests,
            failed_requests: self.failed_requests,
            pending_requests: self.pending_requests,
            expired_requests: self.expired_requests,
            unknown_requests: self.unknown_requests,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            reasoning_tokens: self.reasoning_tokens,
            other_tokens: self.other_tokens,
            amount_minor: self.amount_minor,
            unpriced_requests: self.unpriced_requests,
            no_usage_requests: self.no_usage_requests,
            incomplete_requests: self.incomplete_requests,
            duration_average: rounded_average(self.duration_sum, self.duration_count),
            duration_p50: self.duration_p50,
            duration_p95: self.duration_p95,
        })
    }
}

/// The dashboard slice's conditions (`push_slice` in `usage_analytics`).
fn slice_conditions(authorized_teams: Option<&[TeamId]>, slice: &UsageSlice) -> Conditions {
    let mut c = Conditions::default();
    c.push("has_trace = 1");
    c.clamp("team_id", authorized_teams);
    c.push(
        "recorded_at >= {from:DateTime64(6, 'UTC')} AND recorded_at < {to:DateTime64(6, 'UTC')}",
    );
    c.param(Param::time("from", slice.from));
    c.param(Param::time("to", slice.to));
    c.before(slice.to);
    let teams: Vec<Uuid> = slice.team_ids.iter().map(|id| id.0).collect();
    let accounts: Vec<Uuid> = slice.service_account_ids.iter().map(|id| id.0).collect();
    c.uuids("team_id", "team", &teams);
    c.uuids("service_account_id", "account", &accounts);
    c.uuids("api_key_id", "key", &slice.api_key_ids);
    c.uuids("user_id", "user", &slice.user_ids);
    c.uuids("batch_id", "batch", &slice.batch_ids);
    let statuses: Vec<String> = slice
        .statuses
        .iter()
        .map(|status| status.as_str().to_owned())
        .collect();
    c.strings("model", "model", &slice.models);
    c.strings("alias", "alias", &slice.aliases);
    c.strings("model_master", "master", &slice.model_masters);
    c.strings("deployment", "deployment", &slice.deployments);
    c.strings("provider_family", "family", &slice.provider_families);
    c.strings("location", "location", &slice.locations);
    c.strings("operation", "operation", &slice.operations);
    c.strings("request_status", "status", &statuses);
    c.strings("outcome", "outcome", &slice.outcomes);
    c.strings("error_category", "error", &slice.error_categories);
    c.strings("end_user", "end_user", &slice.end_users);
    if let Some(currency) = slice.currency.as_ref() {
        c.push("coalesce(usage_currency, trace_currency) = {currency:String}");
        c.param(Param::string("currency", currency.as_str()));
    }
    c
}

fn slice_needs_users(slice: &UsageSlice, split_by: Option<UsageDimension>) -> bool {
    !slice.user_ids.is_empty() || split_by == Some(UsageDimension::User)
}

/// The per-request columns the measures read (`push_measured`).
fn measured(key: &str, bucket: &str, users: bool, conditions: &Conditions) -> String {
    format!(
        "SELECT CAST({key} AS Nullable(String)) AS k, {bucket} AS bucket,
                coalesce(usage_currency, trace_currency) AS currency, request_status, has_usage,
                usage_completeness, input_tokens, output_tokens, cached_input_tokens,
                reasoning_tokens, other_tokens, usage_amount_minor AS amount_minor, duration_ms
         FROM {source}
         WHERE {conditions}",
        source = source(users, conditions),
        conditions = conditions.sql(),
    )
}

fn totals_statement(
    authorized_teams: Option<&[TeamId]>,
    slice: &UsageSlice,
    users: bool,
) -> Statement {
    let conditions = slice_conditions(authorized_teams, slice);
    let sql = format!(
        "WITH ev AS ({measured})
         SELECT currency, {MEASURES}
         FROM ev GROUP BY GROUPING SETS ((), (currency))
         {SETTINGS}",
        measured = measured("NULL", "CAST(NULL AS Nullable(Int64))", users, &conditions),
    );
    Statement {
        sql,
        params: conditions.params,
    }
}

fn series_statement(authorized_teams: Option<&[TeamId]>, query: &UsageSeriesQuery) -> Statement {
    let users = slice_needs_users(&query.slice, query.split_by);
    let conditions = slice_conditions(authorized_teams, &query.slice);
    let key = query.split_by.map_or("NULL", dimension);
    let rank = match query.rank_by {
        RankBy::Requests => "count()",
        RankBy::Tokens => "sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))",
        RankBy::Cost => "ifNull(sum(amount_minor), 0)",
    };
    let sql = format!(
        "WITH ev AS ({measured}),
         top AS (SELECT k FROM ev GROUP BY k ORDER BY {rank} DESC, k ASC NULLS LAST
                 LIMIT {limit})
         SELECT if(ranked, k, NULL) AS series_key, toUInt8(NOT ranked) AS other, bucket,
                currency, {MEASURES}
         FROM (SELECT *, k IN (SELECT k FROM top) AS ranked FROM ev)
         GROUP BY GROUPING SETS (
           (series_key, other, bucket), (series_key, other, bucket, currency),
           (series_key, other), (series_key, other, currency))
         {SETTINGS}",
        measured = measured(key, bucket(query.granularity), users, &conditions),
        limit = query.limit,
    );
    Statement {
        sql,
        params: conditions.params,
    }
}

/// The usage report's scope (`USAGE_SCOPE` in `reporting`): requests with
/// usage, attributed to the time of their first usage row.
fn usage_conditions(authorized_teams: Option<&[TeamId]>, query: &UsageQuery) -> Conditions {
    let mut c = Conditions::default();
    c.push("has_usage = 1");
    c.push(
        "usage_attributed_at >= {from:DateTime64(6, 'UTC')}
         AND usage_attributed_at < {to:DateTime64(6, 'UTC')}",
    );
    c.param(Param::time("from", query.from));
    c.param(Param::time("to", query.to));
    c.before(query.to);
    c.clamp("reservation_team_id", authorized_teams);
    let teams: Vec<Uuid> = query.team_ids.iter().map(|id| id.0).collect();
    let accounts: Vec<Uuid> = query.service_account_ids.iter().map(|id| id.0).collect();
    let families: Vec<String> = query
        .provider_families
        .iter()
        .map(|family| family.as_str().to_owned())
        .collect();
    c.uuids("reservation_team_id", "team", &teams);
    c.uuids("reservation_service_account_id", "account", &accounts);
    c.strings("reservation_model", "model", &query.models);
    c.strings("deployment", "deployment", &query.deployments);
    if let Some(currency) = query.currency.as_ref() {
        c.push("usage_currency = {currency:String}");
        c.param(Param::string("currency", currency.as_str()));
    }
    c.strings("provider_family", "family", &families);
    c.strings("location", "location", &query.locations);
    c.uuids("batch_id", "batch", &query.batch_ids);
    c.strings("model_master", "master", &query.model_masters);
    c
}

const USAGE_MEASURES: &str = "
  toInt64(count()) AS request_count,
  toInt64(ifNull(sum(usage_amount_minor), 0)) AS amount_minor,
  toInt64(countIf(usage_amount_minor IS NULL)) AS unpriced_request_count,
  toInt64(countIf(usage_completeness != 'complete')) AS incomplete_request_count,
  toInt64(sum(ifNull(input_tokens, 0))) AS input_tokens,
  toInt64(sum(ifNull(output_tokens, 0))) AS output_tokens,
  toInt64(sum(ifNull(cached_input_tokens, 0))) AS cached_input_tokens,
  toInt64(sum(ifNull(reasoning_tokens, 0))) AS reasoning_tokens,
  toInt64(sum(ifNull(other_tokens, 0))) AS other_tokens";

fn group_expression(dimension: Option<&UsageGroupBy>) -> &'static str {
    match dimension {
        None => "CAST(NULL AS Nullable(String))",
        Some(UsageGroupBy::Day) => {
            "formatDateTime(assumeNotNull(usage_attributed_at), '%Y-%m-%d', 'UTC')"
        }
        Some(UsageGroupBy::Team) => "toString(reservation_team_id)",
        Some(UsageGroupBy::ServiceAccount) => "toString(reservation_service_account_id)",
        Some(UsageGroupBy::Model) => "reservation_model",
        Some(UsageGroupBy::Deployment) => "deployment",
        Some(UsageGroupBy::ProviderFamily) => "provider_family",
        Some(UsageGroupBy::Location) => "location",
        Some(UsageGroupBy::ModelMaster) => "model_master",
    }
}

#[derive(Deserialize)]
struct ChAggregateRow {
    group_key: Option<String>,
    second_key: Option<String>,
    currency: String,
    confidence: String,
    request_count: i64,
    amount_minor: i64,
    unpriced_request_count: i64,
    incomplete_request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    reasoning_tokens: i64,
    other_tokens: i64,
}

impl From<ChAggregateRow> for AggregateRow {
    fn from(row: ChAggregateRow) -> Self {
        Self {
            group_key: row.group_key,
            second_key: row.second_key,
            currency: row.currency,
            confidence: row.confidence,
            request_count: row.request_count,
            amount_minor: row.amount_minor,
            unpriced_request_count: row.unpriced_request_count,
            incomplete_request_count: row.incomplete_request_count,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cached_input_tokens: row.cached_input_tokens,
            reasoning_tokens: row.reasoning_tokens,
            other_tokens: row.other_tokens,
        }
    }
}

fn usage_totals_statement(authorized_teams: Option<&[TeamId]>, query: &UsageQuery) -> Statement {
    let conditions = usage_conditions(authorized_teams, query);
    let sql = format!(
        "SELECT CAST(NULL AS Nullable(String)) AS group_key,
                CAST(NULL AS Nullable(String)) AS second_key,
                assumeNotNull(usage_currency) AS currency,
                assumeNotNull(usage_confidence) AS confidence, {USAGE_MEASURES}
         FROM {source}
         WHERE {conditions}
         GROUP BY currency, confidence
         ORDER BY currency, confidence
         {SETTINGS}",
        source = source(false, &conditions),
        conditions = conditions.sql(),
    );
    Statement {
        sql,
        params: conditions.params,
    }
}

fn usage_rows_statement(
    authorized_teams: Option<&[TeamId]>,
    query: &UsageQuery,
    page_size: u32,
    keyset: Option<&UsageKeyset>,
) -> Statement {
    let mut conditions = usage_conditions(authorized_teams, query);
    let group = group_expression(query.group_by.first());
    let second = group_expression(query.group_by.get(1));
    let after = match keyset {
        Some(keyset) => {
            conditions.param(Param::string("k_group", &keyset.group));
            conditions.param(Param::string("k_second", &keyset.second));
            conditions.param(Param::string("k_currency", &keyset.currency));
            conditions.param(Param::string("k_confidence", &keyset.confidence));
            "WHERE (ifNull(group_key, ''), ifNull(second_key, ''), currency, confidence)
               > ({k_group:String}, {k_second:String}, {k_currency:String}, {k_confidence:String})"
        }
        None => "",
    };
    let sql = format!(
        "SELECT * FROM (
           SELECT CAST({group} AS Nullable(String)) AS group_key,
                  CAST({second} AS Nullable(String)) AS second_key,
                  assumeNotNull(usage_currency) AS currency,
                  assumeNotNull(usage_confidence) AS confidence, {USAGE_MEASURES}
           FROM {source}
           WHERE {where_}
           GROUP BY group_key, second_key, currency, confidence
         )
         {after}
         ORDER BY ifNull(group_key, ''), ifNull(second_key, ''), currency, confidence
         LIMIT {limit}
         {SETTINGS}",
        source = source(false, &conditions),
        where_ = conditions.sql(),
        limit = u64::from(page_size) + 1,
    );
    Statement {
        sql,
        params: conditions.params,
    }
}

/// The trace list's conditions (`push_trace_filters` in `reporting`).
fn trace_conditions(authorized_teams: Option<&[TeamId]>, query: &TraceQuery) -> Conditions {
    let mut c = Conditions::default();
    c.push("has_trace = 1");
    c.clamp("team_id", authorized_teams);
    let teams: Vec<Uuid> = query.team_ids.iter().map(|id| id.0).collect();
    let accounts: Vec<Uuid> = query.service_account_ids.iter().map(|id| id.0).collect();
    c.uuids("team_id", "team", &teams);
    c.uuids("service_account_id", "account", &accounts);
    c.uuids("api_key_id", "key", &query.api_key_ids);
    c.uuids("batch_id", "batch", &query.batch_ids);
    c.strings("end_user", "end_user", &query.end_users);
    let statuses: Vec<String> = query
        .statuses
        .iter()
        .map(|status| status.as_str().to_owned())
        .collect();
    c.strings("model", "model", &query.models);
    c.strings("deployment", "deployment", &query.deployments);
    c.strings("provider_family", "family", &query.provider_families);
    c.strings("location", "location", &query.locations);
    c.strings("trace_status", "status", &statuses);
    c.strings("outcome", "outcome", &query.outcomes);
    if let Some(search) = query.search.as_deref() {
        // `ILIKE '%term%'` with the term's wildcards escaped: a
        // case-insensitive substring.
        let mut condition = "(positionCaseInsensitiveUTF8(ifNull(model, ''), {q:String}) > 0
           OR positionCaseInsensitiveUTF8(deployment, {q:String}) > 0"
            .to_owned();
        c.param(Param::string("q", search));
        if let Ok(id) = search.parse::<Uuid>() {
            condition.push_str(" OR trace_id = {q_id:UUID} OR correlation_id = {q_id:UUID}");
            c.param(Param::uuid("q_id", id));
        }
        condition.push(')');
        c.push(condition);
    }
    if let Some(from) = query.from {
        c.push("recorded_at >= {from:DateTime64(6, 'UTC')}");
        c.param(Param::time("from", from));
    }
    if let Some(to) = query.to {
        c.push("recorded_at < {to:DateTime64(6, 'UTC')}");
        c.param(Param::time("to", to));
        c.before(to);
    }
    c
}

/// A page of trace ids, newest first: `(recorded_at, id)` descending, ids
/// compared as their text (which orders as PostgreSQL's `uuid` does).
fn page_statement(
    mut conditions: Conditions,
    page_size: u32,
    keyset: Option<TimeKeyset>,
    users: bool,
) -> Statement {
    if let Some(keyset) = keyset {
        conditions.push(
            "(assumeNotNull(recorded_at), toString(assumeNotNull(trace_id)))
               < ({k_at:DateTime64(6, 'UTC')}, {k_id:String})",
        );
        conditions.param(Param::time("k_at", keyset.at));
        conditions.param(Param::string("k_id", &keyset.id.to_string()));
    }
    let sql = format!(
        "SELECT toString(assumeNotNull(trace_id)) AS id
         FROM {source}
         WHERE {where_}
         ORDER BY assumeNotNull(recorded_at) DESC, toString(assumeNotNull(trace_id)) DESC
         LIMIT {limit}
         {SETTINGS}",
        source = source(users, &conditions),
        where_ = conditions.sql(),
        limit = u64::from(page_size) + 1,
    );
    Statement {
        sql,
        params: conditions.params,
    }
}

fn trace_dimension(dimension: TraceDimension) -> &'static str {
    match dimension {
        TraceDimension::Day => "formatDateTime(assumeNotNull(recorded_at), '%Y-%m-%d', 'UTC')",
        TraceDimension::Team => "toString(team_id)",
        TraceDimension::ServiceAccount => "toString(service_account_id)",
        TraceDimension::Model => "model",
        TraceDimension::Deployment => "deployment",
        TraceDimension::ProviderFamily => "provider_family",
        TraceDimension::Location => "location",
        TraceDimension::Status => "trace_status",
        TraceDimension::Outcome => "outcome",
    }
}

fn summary_statement(
    authorized_teams: Option<&[TeamId]>,
    query: &TraceQuery,
    dimensions: &[TraceDimension],
    limit: u32,
) -> Statement {
    let conditions = trace_conditions(authorized_teams, query);
    let keys: Vec<String> = (0..dimensions.len()).map(|i| format!("d{i}")).collect();
    let keys = keys.join(", ");
    let selected: Vec<String> = dimensions
        .iter()
        .enumerate()
        .map(|(i, d)| format!("CAST({} AS Nullable(String)) AS d{i}", trace_dimension(*d)))
        .collect();
    let order: Vec<String> = (0..dimensions.len())
        .map(|i| format!("d{i} ASC NULLS LAST"))
        .collect();
    let sql = format!(
        "WITH filtered AS (
           SELECT {selected}, assumeNotNull(trace_currency) AS currency,
                  trace_amount_minor AS amount, assumeNotNull(recorded_at) AS at
           FROM {source}
           WHERE {where_}
         ), by_currency AS (
           SELECT {keys}, currency, count() AS n, sum(amount) AS amount_sum,
                  countIf(amount IS NULL) AS unpriced_n, min(at) AS first, max(at) AS last
           FROM filtered GROUP BY {keys}, currency
         )
         SELECT {keys}, toInt64(sum(n)) AS traces, toInt64(sum(unpriced_n)) AS unpriced,
                arraySort(x -> x.1, groupArrayIf((currency, assumeNotNull(amount_sum)),
                                                 amount_sum IS NOT NULL)) AS amounts,
                toInt64(toUnixTimestamp64Micro(min(first))) AS first_at,
                toInt64(toUnixTimestamp64Micro(max(last))) AS last_at,
                toInt64(sum(sum(n)) OVER ()) AS total
         FROM by_currency GROUP BY {keys}
         ORDER BY traces DESC, {order}
         LIMIT {limit}
         {SETTINGS}",
        selected = selected.join(", "),
        source = source(false, &conditions),
        where_ = conditions.sql(),
        order = order.join(", "),
        limit = u64::from(limit) + 1,
    );
    Statement {
        sql,
        params: conditions.params,
    }
}

#[derive(Deserialize)]
struct ChGroupRow {
    #[serde(default)]
    d0: Option<String>,
    #[serde(default)]
    d1: Option<String>,
    traces: i64,
    unpriced: i64,
    amounts: Vec<(String, i64)>,
    first_at: i64,
    last_at: i64,
    total: i64,
}

#[derive(Deserialize)]
struct IdRow {
    id: Uuid,
}

fn micros(value: i64) -> Result<DateTime<Utc>, ReportingStoreError> {
    DateTime::<Utc>::from_timestamp_micros(value).ok_or(ReportingStoreError::InvalidState)
}

#[derive(serde::Serialize)]
struct IdLine {
    reservation_id: Uuid,
}

#[derive(serde::Serialize)]
struct OwnerLine {
    service_account_id: Uuid,
    owner_user_id: Option<Uuid>,
}

fn tables(overlay: &Overlay, users: bool) -> Result<Vec<ExternalTable>, ClickHouseError> {
    let mut tables = vec![
        ExternalTable::new(
            "overlay_ids",
            "reservation_id UUID",
            overlay.ids.iter().map(|id| IdLine {
                reservation_id: *id,
            }),
        )?,
        ExternalTable::new(
            "overlay",
            ROW_STRUCTURE,
            overlay
                .rows
                .iter()
                .cloned()
                .map(|row| row.into_overlay_row()),
        )?,
    ];
    if users {
        tables.push(ExternalTable::new(
            "owners",
            "service_account_id UUID, owner_user_id Nullable(UUID)",
            overlay.owners.iter().map(|(account, owner)| OwnerLine {
                service_account_id: *account,
                owner_user_id: *owner,
            }),
        )?);
    }
    Ok(tables)
}

/// Whether a ClickHouse read answered or the caller falls back.
fn record(store: &'static str, reason: &'static str) {
    metrics().inc(&ANALYTICS_READS, &[store, reason]);
}

impl ClickHouseAnalytics {
    pub fn new(clickhouse: ClickHouse, pool: PgPool) -> Self {
        Self {
            clickhouse,
            store: Arc::new(Mutex::new(None)),
            sink: AnalyticsSinkStore::new(pool.clone()),
            analytics: UsageAnalyticsStore::new(pool.clone()),
            reporting: ReportingStore::new(pool),
        }
    }

    /// The overlay for a read, or `None` (counted) when ClickHouse cannot
    /// serve it.
    async fn overlay(
        &self,
        organization_id: OrganizationId,
        users: bool,
    ) -> Result<Option<Overlay>, ReportingStoreError> {
        match self.sink.overlay(organization_id, users).await? {
            Ok(overlay) => match self.store_id(overlay.instance_id).await {
                Ok(Some(id)) if id == overlay.instance_id => Ok(Some(overlay)),
                Ok(_) => {
                    record("postgres", NotReady::OtherStore.as_str());
                    tracing::debug!(
                        "the ClickHouse read is not the store copied to; reading PostgreSQL"
                    );
                    Ok(None)
                }
                Err(error) => Self::failed(error),
            },
            Err(reason) => {
                record("postgres", reason.as_str());
                if reason == NotReady::Behind {
                    tracing::debug!("analytics sink behind; reading PostgreSQL");
                }
                Ok(None)
            }
        }
    }

    /// The store id of this process's ClickHouse. A match with `expected`
    /// is trusted for [`STORE_ID_CACHE`]; anything else is read again, so a
    /// store the copy has just been redone to is used as soon as it is live.
    async fn store_id(&self, expected: Uuid) -> Result<Option<Uuid>, ClickHouseError> {
        {
            let cached = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((at, Some(id))) = *cached
                && id == expected
                && at.elapsed() < STORE_ID_CACHE
            {
                return Ok(Some(id));
            }
        }
        let id = self.clickhouse.store_id().await?;
        *self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((Instant::now(), id));
        Ok(id)
    }

    async fn run<T: serde::de::DeserializeOwned>(
        &self,
        organization_id: OrganizationId,
        statement: Statement,
        overlay: &Overlay,
        users: bool,
    ) -> Result<Vec<T>, ClickHouseError> {
        let mut params = statement.params;
        params.push(Param::uuid("org", organization_id.0));
        self.clickhouse
            .query(&statement.sql, &params, tables(overlay, users)?)
            .await
    }

    /// A ClickHouse failure is logged and counted, and the read falls back.
    fn failed<T>(error: ClickHouseError) -> Result<Option<T>, ReportingStoreError> {
        tracing::warn!(%error, "analytics read from ClickHouse failed; reading PostgreSQL");
        record("postgres", "error");
        Ok(None)
    }

    /// `UsageAnalyticsStore::series_report`.
    pub async fn series_report(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &UsageSeriesQuery,
        previous: &UsageSlice,
    ) -> Result<Option<SeriesReport>, ReportingStoreError> {
        let users = slice_needs_users(&query.slice, query.split_by);
        let Some(overlay) = self.overlay(organization_id, users).await? else {
            return Ok(None);
        };
        let (series, current, before) = futures::join!(
            self.run::<ChMeasureRow>(
                organization_id,
                series_statement(authorized_teams, query),
                &overlay,
                users
            ),
            self.run::<ChMeasureRow>(
                organization_id,
                totals_statement(authorized_teams, &query.slice, users),
                &overlay,
                users
            ),
            self.run::<ChMeasureRow>(
                organization_id,
                totals_statement(authorized_teams, previous, users),
                &overlay,
                users
            ),
        );
        let (series, current, before) = match (series, current, before) {
            (Ok(series), Ok(current), Ok(before)) => (series, current, before),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                return Self::failed(error);
            }
        };
        let rows = |rows: Vec<ChMeasureRow>| {
            rows.into_iter()
                .map(ChMeasureRow::into_measure_row)
                .collect::<Result<Vec<_>, _>>()
        };
        let (series, truncated) = assemble_series(&rows(series)?)?;
        let totals = assemble_totals(&rows(current)?)?;
        let previous = assemble_totals(&rows(before)?)?;
        record("clickhouse", "ok");
        Ok(Some(SeriesReport {
            series,
            truncated,
            totals,
            previous,
        }))
    }

    /// `UsageAnalyticsStore::events`: ClickHouse finds and orders the page,
    /// PostgreSQL reads its requests by id.
    pub async fn events(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        slice: &UsageSlice,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<Option<(Vec<UsageEvent>, bool)>, ReportingStoreError> {
        let users = slice_needs_users(slice, None);
        let Some(overlay) = self.overlay(organization_id, users).await? else {
            return Ok(None);
        };
        let statement = page_statement(
            slice_conditions(authorized_teams, slice),
            page_size,
            keyset,
            users,
        );
        let ids = match self
            .run::<IdRow>(organization_id, statement, &overlay, users)
            .await
        {
            Ok(rows) => rows,
            Err(error) => return Self::failed(error),
        };
        let has_more = ids.len() > page_size as usize;
        let ids: Vec<Uuid> = ids
            .into_iter()
            .take(page_size as usize)
            .map(|row| row.id)
            .collect();
        let items = self
            .analytics
            .events_by_ids(organization_id, authorized_teams, &ids)
            .await?;
        record("clickhouse", "ok");
        Ok(Some((items, has_more)))
    }

    /// `ReportingStore::usage_rows` and `usage_totals`, from one overlay.
    #[allow(clippy::type_complexity)]
    pub async fn usage_report(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &UsageQuery,
        page_size: u32,
        keyset: Option<&UsageKeyset>,
    ) -> Result<Option<((Vec<UsageReportRow>, bool), Vec<UsageAggregate>)>, ReportingStoreError>
    {
        let Some(overlay) = self.overlay(organization_id, false).await? else {
            return Ok(None);
        };
        let (rows, totals) = futures::join!(
            self.run::<ChAggregateRow>(
                organization_id,
                usage_rows_statement(authorized_teams, query, page_size, keyset),
                &overlay,
                false
            ),
            self.run::<ChAggregateRow>(
                organization_id,
                usage_totals_statement(authorized_teams, query),
                &overlay,
                false
            ),
        );
        let (rows, totals) = match (rows, totals) {
            (Ok(rows), Ok(totals)) => (rows, totals),
            (Err(error), _) | (_, Err(error)) => return Self::failed(error),
        };
        let page = report_rows(rows.into_iter().map(Into::into).collect(), query, page_size)?;
        let totals = totals
            .into_iter()
            .map(|row| {
                AggregateRow::from(row)
                    .into_aggregate()
                    .map(|(_, aggregate)| aggregate)
            })
            .collect::<Result<Vec<_>, _>>()?;
        record("clickhouse", "ok");
        Ok(Some((page, totals)))
    }

    /// `ReportingStore::list_traces`: ClickHouse finds and orders the page,
    /// PostgreSQL reads its traces by id.
    pub async fn list_traces(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &TraceQuery,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<Option<(Vec<TraceSummary>, bool)>, ReportingStoreError> {
        let Some(overlay) = self.overlay(organization_id, false).await? else {
            return Ok(None);
        };
        let statement = page_statement(
            trace_conditions(authorized_teams, query),
            page_size,
            keyset,
            false,
        );
        let ids = match self
            .run::<IdRow>(organization_id, statement, &overlay, false)
            .await
        {
            Ok(rows) => rows,
            Err(error) => return Self::failed(error),
        };
        let has_more = ids.len() > page_size as usize;
        let ids: Vec<Uuid> = ids
            .into_iter()
            .take(page_size as usize)
            .map(|row| row.id)
            .collect();
        let items = self
            .reporting
            .traces_by_ids(organization_id, authorized_teams, &ids)
            .await?;
        record("clickhouse", "ok");
        Ok(Some((items, has_more)))
    }

    /// `ReportingStore::summarize_traces`.
    #[allow(clippy::type_complexity)]
    pub async fn summarize_traces(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &TraceQuery,
        dimensions: &[TraceDimension],
        limit: u32,
    ) -> Result<Option<(Vec<TraceGroup>, i64, bool)>, ReportingStoreError> {
        let Some(overlay) = self.overlay(organization_id, false).await? else {
            return Ok(None);
        };
        let statement = summary_statement(authorized_teams, query, dimensions, limit);
        let rows = match self
            .run::<ChGroupRow>(organization_id, statement, &overlay, false)
            .await
        {
            Ok(rows) => rows,
            Err(error) => return Self::failed(error),
        };
        let total = rows.first().map_or(0, |row| row.total);
        let mut groups = Vec::with_capacity(rows.len());
        for row in rows {
            let mut values = BTreeMap::new();
            for (index, dimension) in dimensions.iter().enumerate() {
                let value = match index {
                    0 => row.d0.clone(),
                    _ => row.d1.clone(),
                };
                values.insert(dimension.as_str(), value);
            }
            groups.push(TraceGroup {
                values,
                count: row.traces,
                unpriced_count: row.unpriced,
                amounts: row
                    .amounts
                    .into_iter()
                    .map(|(currency, amount_minor)| CurrencyAmount {
                        currency,
                        amount_minor,
                    })
                    .collect(),
                first_recorded_at: micros(row.first_at)?,
                last_recorded_at: micros(row.last_at)?,
            });
        }
        let truncated = groups.len() > limit as usize;
        groups.truncate(limit as usize);
        record("clickhouse", "ok");
        Ok(Some((groups, total, truncated)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_round_half_away_from_zero_like_numeric() {
        assert_eq!(rounded_average(Some(5), 2), Some(3));
        assert_eq!(rounded_average(Some(4), 2), Some(2));
        assert_eq!(rounded_average(Some(7), 3), Some(2));
        assert_eq!(rounded_average(Some(8), 3), Some(3));
        assert_eq!(rounded_average(None, 0), None);
        assert_eq!(rounded_average(Some(0), 0), None);
        assert_eq!(rounded_average(Some(-5), 2), Some(-3));
    }

    #[test]
    fn every_statement_is_scoped_to_the_organization() {
        let mut bounded = Conditions::default();
        bounded.before(Utc::now());
        for conditions in [&Conditions::default(), &bounded] {
            let with_users = source(true, conditions);
            assert_eq!(
                with_users.matches("organization_id = {org:UUID}").count(),
                2
            );
            assert!(source(false, conditions).contains("FROM requests FINAL"));
        }
        assert!(source(false, &bounded).contains("AND created_at < {until:"));
        assert!(!source(false, &Conditions::default()).contains("created_at <"));
    }
}
