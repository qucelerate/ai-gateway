//! Read-only queries behind the usage dashboard: a slice of requests as
//! time series, as totals, and as a paged list.
//!
//! A request is its trace with the latest usage recorded for its
//! reservation, so a request lands once, in the window it was admitted in.
//! Every query is bound to one organization and, for team-scoped actors, to
//! an explicit team allowlist; organization-level requests (no team) are only
//! visible to org-wide callers. Amounts are summed per currency, never across
//! currencies.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, ServiceAccountId, TeamId},
    reporting::{CurrencyAmount, ReportConfidence, TokenTotals, TraceTokens},
    usage_analytics::{
        DurationStats, EventLabels, Granularity, RankBy, RequestStatus, UsageDimension, UsageEvent,
        UsageMeasures, UsagePoint, UsageSeries, UsageSeriesQuery, UsageSlice,
    },
};

use super::reporting::{ReportingStoreError, TimeKeyset};

const EVENT_FROM: &str = "
FROM gateway.inference_trace t
JOIN gateway.accounting_reservation r
  ON r.organization_id = t.organization_id AND r.id = t.reservation_id
LEFT JOIN gateway.catalog_deployment d
  ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
 AND d.id = r.catalog_deployment_id
LEFT JOIN gateway.catalog_route cr
  ON cr.organization_id = t.organization_id AND cr.id = t.catalog_route_id
LEFT JOIN gateway.service_account sa
  ON sa.organization_id = t.organization_id AND sa.id = t.service_account_id
LEFT JOIN LATERAL (
  SELECT u.id AS usage_id, btrim(u.currency) AS usage_currency, u.confidence, u.completeness,
         u.input_tokens, u.output_tokens, u.cached_input_tokens, u.reasoning_tokens,
         u.other_tokens, u.amount_minor AS usage_amount_minor
  FROM gateway.accounting_usage u
  WHERE u.organization_id = t.organization_id AND u.reservation_id = t.reservation_id
  ORDER BY u.recorded_at DESC, u.id DESC
  LIMIT 1
) lu ON true";

/// A failed provider request is failed whatever accounting made of it. A
/// reservation that expired before it was claimed never reached the
/// provider (`expired`); one the expiry worker closed after a claim has an
/// unknown result until it is settled (`unknown`). An attempt the request
/// failed over from is `failed-over`, though its provider request failed.
pub(super) const STATUS: &str = "CASE WHEN t.outcome = 'failed-over' THEN 'failed-over'
  WHEN t.provider_error_category IS NOT NULL THEN 'failed'
  WHEN t.status = 'pending' THEN 'pending'
  WHEN t.outcome = 'expired' THEN 'expired'
  WHEN t.outcome IN ('unknown', 'operator-resolved') AND EXISTS (
    SELECT 1 FROM gateway.accounting_reconciliation rc
    WHERE rc.organization_id = t.organization_id AND rc.reservation_id = t.reservation_id
      AND rc.reason = 'expired-after-claim') THEN 'unknown'
  ELSE 'succeeded' END";

const CURRENCY: &str = "COALESCE(lu.usage_currency, btrim(t.currency))";

/// Admission to settlement. A request closed by the expiry worker (before
/// or after a claim) or by an operator has no meaningful duration: its end
/// is when it was cleaned up. Keep in step with `TRACE_COLUMNS`.
pub(super) const DURATION: &str = "CASE WHEN t.finalized_at IS NOT NULL
   AND t.outcome IS DISTINCT FROM 'expired' AND t.outcome IS DISTINCT FROM 'operator-resolved'
   AND NOT EXISTS (
     SELECT 1 FROM gateway.accounting_reconciliation rc
     WHERE rc.organization_id = t.organization_id AND rc.reservation_id = t.reservation_id
       AND rc.reason = 'expired-after-claim')
  THEN GREATEST(0, round(extract(epoch FROM t.finalized_at - t.recorded_at) * 1000))::bigint END";

const MEASURES: &str = "
  count(*)::bigint AS requests,
  count(*) FILTER (WHERE request_status = 'failed')::bigint AS failed_requests,
  count(*) FILTER (WHERE request_status = 'pending')::bigint AS pending_requests,
  count(*) FILTER (WHERE request_status = 'expired')::bigint AS expired_requests,
  count(*) FILTER (WHERE request_status = 'unknown')::bigint AS unknown_requests,
  COALESCE(sum(input_tokens), 0)::bigint AS input_tokens,
  COALESCE(sum(output_tokens), 0)::bigint AS output_tokens,
  COALESCE(sum(cached_input_tokens), 0)::bigint AS cached_input_tokens,
  COALESCE(sum(reasoning_tokens), 0)::bigint AS reasoning_tokens,
  COALESCE(sum(other_tokens), 0)::bigint AS other_tokens,
  sum(amount_minor)::bigint AS amount_minor,
  count(*) FILTER (WHERE usage_id IS NOT NULL AND amount_minor IS NULL)::bigint
    AS unpriced_requests,
  count(*) FILTER (WHERE usage_id IS NULL)::bigint AS no_usage_requests,
  count(*) FILTER (WHERE usage_id IS NOT NULL AND completeness <> 'complete')::bigint
    AS incomplete_requests,
  round(avg(duration_ms))::bigint AS duration_average,
  percentile_disc(0.5) WITHIN GROUP (ORDER BY duration_ms) AS duration_p50,
  percentile_disc(0.95) WITHIN GROUP (ORDER BY duration_ms) AS duration_p95";

/// The app's ID for the request's end user (D-028), as the app sent it.
const END_USER: &str =
    "(SELECT eu.external_id FROM gateway.end_user eu WHERE eu.id = t.end_user_id)";

fn dimension(dimension: UsageDimension) -> &'static str {
    match dimension {
        UsageDimension::Team => "t.team_id::text",
        UsageDimension::ServiceAccount => "t.service_account_id::text",
        UsageDimension::ApiKey => "t.api_key_id::text",
        UsageDimension::User => "sa.owner_user_id::text",
        UsageDimension::EndUser => END_USER,
        UsageDimension::Model => "t.model",
        UsageDimension::Alias => "cr.alias",
        UsageDimension::Deployment => "r.deployment",
        UsageDimension::ProviderFamily => "r.catalog_family",
        UsageDimension::Location => "d.location",
        UsageDimension::Operation => "t.catalog_operation",
        UsageDimension::Status => STATUS,
        UsageDimension::Outcome => "t.outcome",
        UsageDimension::ErrorCategory => "t.provider_error_category",
        UsageDimension::ModelMaster => "r.model_master",
    }
}

fn bucket(granularity: Granularity) -> &'static str {
    match granularity {
        Granularity::Hour => {
            "date_trunc('hour', t.recorded_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'"
        }
        Granularity::Day => {
            "date_trunc('day', t.recorded_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'"
        }
        Granularity::Week => {
            "date_trunc('week', t.recorded_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'"
        }
    }
}

/// One grouping of a series or totals query: by series value (or none),
/// bucket (`all_buckets`: none) and currency (`all_currencies`: none).
/// ClickHouse's rows are read into the same shape.
#[derive(FromRow)]
pub(super) struct MeasureRow {
    pub(super) series_key: Option<String>,
    pub(super) other: bool,
    pub(super) bucket: Option<DateTime<Utc>>,
    pub(super) currency: Option<String>,
    pub(super) all_buckets: i32,
    pub(super) all_currencies: i32,
    pub(super) requests: i64,
    pub(super) failed_requests: i64,
    pub(super) pending_requests: i64,
    pub(super) expired_requests: i64,
    pub(super) unknown_requests: i64,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cached_input_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) other_tokens: i64,
    pub(super) amount_minor: Option<i64>,
    pub(super) unpriced_requests: i64,
    pub(super) no_usage_requests: i64,
    pub(super) incomplete_requests: i64,
    pub(super) duration_average: Option<i64>,
    pub(super) duration_p50: Option<i64>,
    pub(super) duration_p95: Option<i64>,
}

impl MeasureRow {
    /// Counts from a row over every currency.
    fn counts(&self) -> UsageMeasures {
        UsageMeasures {
            requests: self.requests,
            failed_requests: self.failed_requests,
            pending_requests: self.pending_requests,
            expired_requests: self.expired_requests,
            unknown_requests: self.unknown_requests,
            tokens: TokenTotals {
                input: self.input_tokens,
                output: self.output_tokens,
                cached_input: self.cached_input_tokens,
                reasoning: self.reasoning_tokens,
                other: self.other_tokens,
            },
            amounts: Vec::new(),
            unpriced_requests: self.unpriced_requests,
            no_usage_requests: self.no_usage_requests,
            incomplete_requests: self.incomplete_requests,
            duration: DurationStats {
                average_ms: self.duration_average,
                p50_ms: self.duration_p50,
                p95_ms: self.duration_p95,
            },
        }
    }
}

/// Merges rows of one grouping: the row over every currency sets the counts,
/// and each currency's row adds its amount.
#[derive(Default)]
struct Accumulator {
    measures: UsageMeasures,
    amounts: BTreeMap<String, i64>,
}

impl Accumulator {
    fn add(&mut self, row: &MeasureRow) -> Result<(), ReportingStoreError> {
        if row.all_currencies == 1 {
            let amounts = std::mem::take(&mut self.measures.amounts);
            self.measures = row.counts();
            self.measures.amounts = amounts;
        } else if let Some(amount) = row.amount_minor {
            let currency = row
                .currency
                .clone()
                .ok_or(ReportingStoreError::InvalidState)?;
            self.amounts.insert(currency, amount);
        }
        Ok(())
    }

    fn finish(mut self) -> UsageMeasures {
        self.measures.amounts = self
            .amounts
            .into_iter()
            .map(|(currency, amount_minor)| CurrencyAmount {
                currency,
                amount_minor,
            })
            .collect();
        self.measures
    }
}

#[derive(Default)]
struct SeriesAccumulator {
    totals: Accumulator,
    points: BTreeMap<DateTime<Utc>, Accumulator>,
}

#[derive(FromRow)]
struct EventRow {
    id: Uuid,
    correlation_id: Uuid,
    recorded_at: DateTime<Utc>,
    finalized_at: Option<DateTime<Utc>>,
    duration_ms: Option<i64>,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    user_id: Option<Uuid>,
    team_name: Option<String>,
    service_account_name: Option<String>,
    api_key_prefix: Option<String>,
    user_name: Option<String>,
    model: String,
    alias: Option<String>,
    deployment: String,
    provider_family: Option<String>,
    location: Option<String>,
    operation: Option<String>,
    request_status: String,
    outcome: Option<String>,
    error_category: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    other_tokens: Option<i64>,
    currency: String,
    amount_minor: Option<i64>,
    usage_confidence: Option<String>,
    content_state: Option<String>,
    batch_id: Option<String>,
    custom_id: Option<String>,
    model_master: Option<String>,
}

impl TryFrom<EventRow> for UsageEvent {
    type Error = ReportingStoreError;

    fn try_from(row: EventRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            correlation_id: row.correlation_id,
            recorded_at: row.recorded_at,
            finalized_at: row.finalized_at,
            duration_ms: row.duration_ms,
            team_id: row.team_id.map(TeamId),
            service_account_id: row.service_account_id.map(ServiceAccountId),
            api_key_id: row.api_key_id,
            user_id: row.user_id,
            labels: EventLabels {
                team: row.team_name,
                service_account: row.service_account_name,
                api_key: row.api_key_prefix,
                user: row.user_name,
            },
            model: row.model,
            alias: row.alias,
            deployment: row.deployment,
            provider_family: row.provider_family,
            location: row.location,
            operation: row.operation,
            status: RequestStatus::parse(&row.request_status)
                .ok_or(ReportingStoreError::InvalidState)?,
            outcome: row.outcome,
            error_category: row.error_category,
            tokens: TraceTokens {
                input: row.input_tokens,
                output: row.output_tokens,
                cached_input: row.cached_input_tokens,
                reasoning: row.reasoning_tokens,
                other: row.other_tokens,
            },
            currency: row.currency,
            amount_minor: row.amount_minor,
            usage_confidence: row
                .usage_confidence
                .as_deref()
                .map(|value| {
                    ReportConfidence::parse(value).ok_or(ReportingStoreError::InvalidState)
                })
                .transpose()?,
            content_state: row.content_state,
            batch_id: row.batch_id,
            custom_id: row.custom_id,
            model_master: row.model_master,
        })
    }
}

#[derive(Clone)]
pub struct UsageAnalyticsStore {
    pool: PgPool,
}

impl UsageAnalyticsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The slice's series with its totals and the previous window's
    /// totals, read from one snapshot so they agree with one another.
    pub async fn series_report(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &UsageSeriesQuery,
        previous: &UsageSlice,
    ) -> Result<SeriesReport, ReportingStoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let (series, truncated) = series(&mut tx, organization_id, authorized_teams, query).await?;
        let current = totals(&mut tx, organization_id, authorized_teams, &query.slice).await?;
        let previous = totals(&mut tx, organization_id, authorized_teams, previous).await?;
        tx.commit().await?;
        Ok(SeriesReport {
            series,
            truncated,
            totals: current,
            previous,
        })
    }

    /// One page of the slice's requests, newest first. Returns whether a
    /// further page exists.
    pub async fn events(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        slice: &UsageSlice,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<(Vec<UsageEvent>, bool), ReportingStoreError> {
        let mut sql = QueryBuilder::<Postgres>::new(event_select());
        push_slice(&mut sql, organization_id, authorized_teams, slice);
        if let Some(keyset) = keyset {
            sql.push(" AND (t.recorded_at, t.id) < (");
            sql.push_bind(keyset.at);
            sql.push(", ");
            sql.push_bind(keyset.id);
            sql.push(")");
        }
        sql.push(" ORDER BY t.recorded_at DESC, t.id DESC LIMIT ");
        sql.push_bind(i64::from(page_size) + 1);
        let rows = sql
            .build_query_as::<EventRow>()
            .fetch_all(&self.pool)
            .await?;
        let mut items = rows
            .into_iter()
            .map(UsageEvent::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > page_size as usize;
        items.truncate(page_size as usize);
        Ok((items, has_more))
    }

    /// The requests with these trace ids, in the order given; ids that are
    /// gone, or outside the organization or the team clamp, are left out.
    /// ClickHouse found and ordered the page (`persistence::clickhouse_analytics`).
    pub async fn events_by_ids(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        ids: &[Uuid],
    ) -> Result<Vec<UsageEvent>, ReportingStoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = QueryBuilder::<Postgres>::new(event_select());
        sql.push(" WHERE t.organization_id = ");
        sql.push_bind(organization_id.0);
        if let Some(teams) = authorized_teams {
            sql.push(" AND t.team_id = ANY(");
            sql.push_bind(teams.iter().map(|team| team.0).collect::<Vec<_>>());
            sql.push(")");
        }
        sql.push(" AND t.id = ANY(");
        sql.push_bind(ids.to_vec());
        sql.push(")");
        let rows = sql
            .build_query_as::<EventRow>()
            .fetch_all(&self.pool)
            .await?;
        let mut found: HashMap<Uuid, UsageEvent> = HashMap::with_capacity(rows.len());
        for row in rows {
            let event = UsageEvent::try_from(row)?;
            found.insert(event.id, event);
        }
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// Names for identity values: team and service account names, API key
    /// prefixes, and users' display names (or emails). Values that name
    /// nothing in the organization are left out.
    pub async fn labels(
        &self,
        organization_id: OrganizationId,
        dimension: UsageDimension,
        values: &[String],
    ) -> Result<HashMap<String, String>, ReportingStoreError> {
        let ids: Vec<Uuid> = values
            .iter()
            .filter_map(|value| value.parse().ok())
            .collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = match dimension {
            UsageDimension::Team => {
                "SELECT id::text, name FROM gateway.team WHERE organization_id = $1 AND id = ANY($2)"
            }
            UsageDimension::ServiceAccount => {
                "SELECT id::text, name FROM gateway.service_account
                 WHERE organization_id = $1 AND id = ANY($2)"
            }
            UsageDimension::ApiKey => {
                "SELECT id::text, public_prefix FROM gateway.api_key
                 WHERE organization_id = $1 AND id = ANY($2)"
            }
            UsageDimension::User => {
                "SELECT ua.id::text, COALESCE(NULLIF(ua.display_name, ''), ua.email)
                 FROM gateway.user_account ua
                 JOIN gateway.organization_membership om
                   ON om.organization_id = $1 AND om.user_id = ua.id
                 WHERE ua.id = ANY($2)"
            }
            _ => return Ok(HashMap::new()),
        };
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(sql)
            .bind(organization_id.0)
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, name)| name.map(|name| (id, name)))
            .collect())
    }
}

/// The columns of a request in the dashboard's list, from its joins.
fn event_select() -> String {
    format!(
        "SELECT t.id, t.correlation_id, t.recorded_at, t.finalized_at,
                {DURATION} AS duration_ms,
                t.team_id, t.service_account_id, t.api_key_id, sa.owner_user_id AS user_id,
                tm.name AS team_name, sa.name AS service_account_name,
                k.public_prefix AS api_key_prefix,
                COALESCE(NULLIF(ua.display_name, ''), ua.email) AS user_name,
                t.model, cr.alias, r.deployment, r.catalog_family AS provider_family,
                d.location, t.catalog_operation AS operation, {STATUS} AS request_status,
                t.outcome, t.provider_error_category AS error_category,
                lu.input_tokens, lu.output_tokens, lu.cached_input_tokens,
                lu.reasoning_tokens, lu.other_tokens, {CURRENCY} AS currency,
                lu.usage_amount_minor AS amount_minor, lu.confidence AS usage_confidence,
                tc.state AS content_state,
                CASE WHEN r.batch_id IS NOT NULL THEN 'batch_' || r.batch_id::text END
                  AS batch_id,
                bl.custom_id, r.model_master
         {EVENT_FROM}
         LEFT JOIN gateway.team tm
           ON tm.organization_id = t.organization_id AND tm.id = t.team_id
         LEFT JOIN gateway.api_key k
           ON k.organization_id = t.organization_id AND k.id = t.api_key_id
         LEFT JOIN gateway.user_account ua ON ua.id = sa.owner_user_id
         LEFT JOIN gateway.inference_trace_content tc
           ON tc.organization_id = t.organization_id AND tc.trace_id = t.id
         LEFT JOIN gateway.batch_line bl
           ON bl.organization_id = r.organization_id AND bl.reservation_id = r.id"
    )
}

/// The slice's totals over its whole window.
async fn totals(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    authorized_teams: Option<&[TeamId]>,
    slice: &UsageSlice,
) -> Result<UsageMeasures, ReportingStoreError> {
    let mut sql = QueryBuilder::<Postgres>::new("WITH ev AS (");
    push_measured(&mut sql, "NULL::text", "NULL::timestamptz");
    push_slice(&mut sql, organization_id, authorized_teams, slice);
    sql.push(format!(
        ")
         SELECT NULL::text AS series_key, false AS other, NULL::timestamptz AS bucket,
                currency, 1 AS all_buckets, GROUPING(currency) AS all_currencies, {MEASURES}
         FROM ev GROUP BY GROUPING SETS ((), (currency))"
    ));
    let rows = sql
        .build_query_as::<MeasureRow>()
        .fetch_all(&mut *conn)
        .await?;
    assemble_totals(&rows)
}

/// Totals from rows grouped by `()` and `(currency)`.
pub(super) fn assemble_totals(rows: &[MeasureRow]) -> Result<UsageMeasures, ReportingStoreError> {
    let mut totals = Accumulator::default();
    for row in rows {
        totals.add(row)?;
    }
    Ok(totals.finish())
}

/// The slice bucketed, split into its top values and `other`. Returns the
/// series in no particular order and whether any value was folded into
/// `other`.
async fn series(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    authorized_teams: Option<&[TeamId]>,
    query: &UsageSeriesQuery,
) -> Result<(Vec<UsageSeries>, bool), ReportingStoreError> {
    let key = query.split_by.map_or("NULL::text", dimension);
    let rank = match query.rank_by {
        RankBy::Requests => "count(*)",
        RankBy::Tokens => "COALESCE(sum(input_tokens), 0) + COALESCE(sum(output_tokens), 0)",
        RankBy::Cost => "COALESCE(sum(amount_minor), 0)",
    };
    let mut sql = QueryBuilder::<Postgres>::new("WITH ev AS (");
    push_measured(&mut sql, key, bucket(query.granularity));
    push_slice(&mut sql, organization_id, authorized_teams, &query.slice);
    // Ties break on the value in byte order, as ClickHouse orders them, so
    // the top values are deterministic and the same from either store.
    sql.push(format!(
        "), top AS (
           SELECT k FROM ev GROUP BY k ORDER BY {rank} DESC, k COLLATE \"C\" NULLS LAST LIMIT "
    ));
    sql.push_bind(i64::from(query.limit));
    sql.push(format!(
        "), keyed AS (
           SELECT CASE WHEN ranked THEN ev.k END AS series_key, NOT ranked AS other, ev.*
           FROM ev
           CROSS JOIN LATERAL (
             SELECT EXISTS (SELECT 1 FROM top WHERE top.k IS NOT DISTINCT FROM ev.k) AS ranked
           ) membership
         )
         SELECT series_key, other, bucket, currency,
                GROUPING(bucket) AS all_buckets, GROUPING(currency) AS all_currencies,
                {MEASURES}
         FROM keyed
         GROUP BY GROUPING SETS (
           (series_key, other, bucket), (series_key, other, bucket, currency),
           (series_key, other), (series_key, other, currency))"
    ));
    let rows = sql
        .build_query_as::<MeasureRow>()
        .fetch_all(&mut *conn)
        .await?;
    assemble_series(&rows)
}

/// Series from rows grouped by `(series, other)` with and without bucket
/// and currency, ordered by `(other, value)`, and whether any value was
/// folded into `other`.
pub(super) fn assemble_series(
    rows: &[MeasureRow],
) -> Result<(Vec<UsageSeries>, bool), ReportingStoreError> {
    let mut grouped: BTreeMap<(bool, Option<String>), SeriesAccumulator> = BTreeMap::new();
    for row in rows {
        let series = grouped
            .entry((row.other, row.series_key.clone()))
            .or_default();
        if row.all_buckets == 1 {
            series.totals.add(row)?;
        } else {
            let bucket = row.bucket.ok_or(ReportingStoreError::InvalidState)?;
            series.points.entry(bucket).or_default().add(row)?;
        }
    }
    let truncated = grouped.keys().any(|(other, _)| *other);
    let series = grouped
        .into_iter()
        .map(|((other, value), series)| UsageSeries {
            value,
            label: None,
            other,
            totals: series.totals.finish(),
            points: series
                .points
                .into_iter()
                .map(|(bucket, measures)| UsagePoint {
                    bucket,
                    measures: measures.finish(),
                })
                .collect(),
        })
        .collect();
    Ok((series, truncated))
}

/// A series report's figures, read together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesReport {
    pub series: Vec<UsageSeries>,
    /// Whether any value was folded into `other`.
    pub truncated: bool,
    pub totals: UsageMeasures,
    pub previous: UsageMeasures,
}

/// `SELECT` of the per-request columns the measures read, keyed by `key`
/// and bucketed by `bucket`, from the request joins.
fn push_measured(sql: &mut QueryBuilder<'_, Postgres>, key: &str, bucket: &str) {
    sql.push(format!(
        "SELECT {key} AS k, {bucket} AS bucket, {CURRENCY} AS currency,
                {STATUS} AS request_status, lu.usage_id, lu.completeness, lu.input_tokens,
                lu.output_tokens, lu.cached_input_tokens, lu.reasoning_tokens, lu.other_tokens,
                lu.usage_amount_minor AS amount_minor, {DURATION} AS duration_ms
         {EVENT_FROM}"
    ));
}

/// `WHERE` for every dashboard query: the organization, the actor's team
/// clamp, the window and the slice's filters.
fn push_slice(
    sql: &mut QueryBuilder<'_, Postgres>,
    organization_id: OrganizationId,
    authorized_teams: Option<&[TeamId]>,
    slice: &UsageSlice,
) {
    sql.push(" WHERE t.organization_id = ");
    sql.push_bind(organization_id.0);
    if let Some(teams) = authorized_teams {
        sql.push(" AND t.team_id = ANY(");
        sql.push_bind(teams.iter().map(|team| team.0).collect::<Vec<_>>());
        sql.push(")");
    }
    sql.push(" AND t.recorded_at >= ");
    sql.push_bind(slice.from);
    sql.push(" AND t.recorded_at < ");
    sql.push_bind(slice.to);
    for (column, values) in [
        (
            "t.team_id",
            slice.team_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
        ),
        (
            "t.service_account_id",
            slice.service_account_ids.iter().map(|id| id.0).collect(),
        ),
        ("t.api_key_id", slice.api_key_ids.clone()),
        ("sa.owner_user_id", slice.user_ids.clone()),
        ("r.batch_id", slice.batch_ids.clone()),
    ] {
        if !values.is_empty() {
            sql.push(format!(" AND {column} = ANY("));
            sql.push_bind(values);
            sql.push(")");
        }
    }
    let statuses: Vec<String> = slice
        .statuses
        .iter()
        .map(|status| status.as_str().to_owned())
        .collect();
    for (column, values) in [
        ("t.model", &slice.models),
        ("cr.alias", &slice.aliases),
        ("r.model_master", &slice.model_masters),
        ("r.deployment", &slice.deployments),
        ("r.catalog_family", &slice.provider_families),
        ("d.location", &slice.locations),
        ("t.catalog_operation", &slice.operations),
        (STATUS, &statuses),
        ("t.outcome", &slice.outcomes),
        ("t.provider_error_category", &slice.error_categories),
        (END_USER, &slice.end_users),
    ] {
        if !values.is_empty() {
            sql.push(format!(" AND ({column}) = ANY("));
            sql.push_bind(values.clone());
            sql.push(")");
        }
    }
    if let Some(currency) = slice.currency.as_ref() {
        sql.push(format!(" AND {CURRENCY} = "));
        sql.push_bind(currency.as_str().to_owned());
    }
}
