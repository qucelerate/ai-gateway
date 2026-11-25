//! Read-only reporting queries over the append-only accounting tables.
//!
//! Every query is bound to one organization and, for team-scoped actors, to
//! an explicit team allowlist (`authorized_teams`); organization-level rows
//! (no team) are only visible to org-wide callers. Usage reports use the
//! latest usage row per reservation so interim and final rows for one request
//! are never double counted.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, ServiceAccountId, TeamId, UserAccountId},
    reporting::{
        CurrencyAmount, ReconciliationItem, ReconciliationStatusFilter, ReportConfidence,
        TokenTotals, TraceDetail, TraceDimension, TraceGroup, TraceProviderError, TraceQuery,
        TraceReconciliation, TraceReservation, TraceRouting, TraceStatus, TraceSummary,
        TraceTokens, TraceUsage, UsageAggregate, UsageGroupBy, UsageQuery, UsageReportRow,
    },
};

#[derive(Debug)]
pub enum ReportingStoreError {
    Database(sqlx::Error),
    /// A persisted value is outside the vocabulary the schema allows, or a
    /// page keyset could not be parsed.
    InvalidState,
}

impl From<sqlx::Error> for ReportingStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// `(time, id)` keyset shared by the trace and reconciliation lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeKeyset {
    pub at: DateTime<Utc>,
    pub id: Uuid,
}

impl TimeKeyset {
    pub fn to_claim(&self) -> String {
        format!("{}|{}", self.at.to_rfc3339(), self.id)
    }

    pub fn parse(claim: &str) -> Result<Self, ReportingStoreError> {
        let (at, id) = claim
            .split_once('|')
            .ok_or(ReportingStoreError::InvalidState)?;
        let at = DateTime::parse_from_rfc3339(at)
            .map_err(|_| ReportingStoreError::InvalidState)?
            .with_timezone(&Utc);
        let id = Uuid::parse_str(id).map_err(|_| ReportingStoreError::InvalidState)?;
        Ok(Self { at, id })
    }
}

/// `(group, second group, currency, confidence)` keyset for usage report
/// rows. A missing group (organization-level usage, or no second dimension)
/// sorts as the empty string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageKeyset {
    pub group: String,
    pub second: String,
    pub currency: String,
    pub confidence: String,
}

impl UsageKeyset {
    pub fn of(row: &UsageReportRow, group_by: &[UsageGroupBy]) -> Self {
        let value = |index: usize| {
            group_by
                .get(index)
                .and_then(|dimension| row.values.get(dimension.as_str()).cloned().flatten())
                .unwrap_or_default()
        };
        Self {
            group: value(0),
            second: value(1),
            currency: row.aggregate.currency.clone(),
            confidence: row.aggregate.confidence.as_str().to_owned(),
        }
    }

    pub fn to_claim(&self) -> String {
        serde_json::json!([self.group, self.second, self.currency, self.confidence]).to_string()
    }

    pub fn parse(claim: &str) -> Result<Self, ReportingStoreError> {
        let [group, second, currency, confidence]: [String; 4] =
            serde_json::from_str(claim).map_err(|_| ReportingStoreError::InvalidState)?;
        Ok(Self {
            group,
            second,
            currency,
            confidence,
        })
    }
}

/// Latest usage row per reservation, attributed to the window that holds the
/// reservation's first usage row. A request therefore lands in exactly one
/// window: later interim/final rows update its measures instead of moving it
/// out of (or double-counting it across) windows. Binds: $1 org, $2 from,
/// $3 to, $4 team clamp, $5 team, $6 service account, $7 model,
/// $8 deployment, $9 currency, $10 provider family, $11 location, $12 batch,
/// $13 model master (the record the deployment was linked to when served).
const USAGE_SCOPE: &str = "
WITH latest AS (
  SELECT DISTINCT ON (u.reservation_id)
         u.reservation_id, btrim(u.currency) AS currency, u.confidence, u.input_tokens,
         u.output_tokens, u.cached_input_tokens, u.reasoning_tokens, u.other_tokens,
         u.amount_minor, u.completeness,
         min(u.recorded_at) OVER (PARTITION BY u.reservation_id) AS attributed_at
  FROM gateway.accounting_usage u
  WHERE u.organization_id = $1
    AND u.reservation_id IN (
      SELECT w.reservation_id FROM gateway.accounting_usage w
      WHERE w.organization_id = $1 AND w.recorded_at >= $2 AND w.recorded_at < $3)
  ORDER BY u.reservation_id, u.recorded_at DESC, u.id DESC
), scoped AS (
  SELECT l.*, r.team_id, r.service_account_id, r.model, r.deployment, r.model_master,
         r.catalog_family AS provider_family, d.location
  FROM latest l
  JOIN gateway.accounting_reservation r
    ON r.organization_id = $1 AND r.id = l.reservation_id
  LEFT JOIN gateway.catalog_deployment d
    ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
   AND d.id = r.catalog_deployment_id
  WHERE l.attributed_at >= $2 AND l.attributed_at < $3
    AND ($4::uuid[] IS NULL OR r.team_id = ANY($4))
    AND ($5::uuid[] IS NULL OR r.team_id = ANY($5))
    AND ($6::uuid[] IS NULL OR r.service_account_id = ANY($6))
    AND ($7::text[] IS NULL OR r.model = ANY($7))
    AND ($8::text[] IS NULL OR r.deployment = ANY($8))
    AND ($9::text IS NULL OR l.currency = $9)
    AND ($10::text[] IS NULL OR r.catalog_family = ANY($10))
    AND ($11::text[] IS NULL OR d.location = ANY($11))
    AND ($12::uuid[] IS NULL OR r.batch_id = ANY($12))
    AND ($13::text[] IS NULL OR r.model_master = ANY($13))
)";

const USAGE_MEASURES: &str = "
  count(*)::bigint AS request_count,
  COALESCE(sum(amount_minor), 0)::bigint AS amount_minor,
  count(*) FILTER (WHERE amount_minor IS NULL)::bigint AS unpriced_request_count,
  count(*) FILTER (WHERE completeness <> 'complete')::bigint AS incomplete_request_count,
  COALESCE(sum(input_tokens), 0)::bigint AS input_tokens,
  COALESCE(sum(output_tokens), 0)::bigint AS output_tokens,
  COALESCE(sum(cached_input_tokens), 0)::bigint AS cached_input_tokens,
  COALESCE(sum(reasoning_tokens), 0)::bigint AS reasoning_tokens,
  COALESCE(sum(other_tokens), 0)::bigint AS other_tokens";

const TRACE_COLUMNS: &str = "
  t.id, t.organization_id, t.team_id, t.service_account_id, t.status, t.outcome, t.model,
  r.deployment, r.catalog_family AS provider_family, d.location,
  btrim(t.currency) AS currency, t.amount_minor, lu.confidence AS usage_confidence,
  t.recorded_at, t.finalized_at, t.correlation_id, t.api_key_id,
  (SELECT eu.external_id FROM gateway.end_user eu WHERE eu.id = t.end_user_id) AS end_user,
  cr.alias,
  t.catalog_operation AS operation, t.provider_error_category AS error_category,
  CASE WHEN t.finalized_at IS NOT NULL
        AND t.outcome IS DISTINCT FROM 'expired'
        AND t.outcome IS DISTINCT FROM 'operator-resolved'
        AND NOT EXISTS (
          SELECT 1 FROM gateway.accounting_reconciliation rc
          WHERE rc.organization_id = t.organization_id AND rc.reservation_id = t.reservation_id
            AND rc.reason = 'expired-after-claim')
   THEN GREATEST(0, round(extract(epoch FROM t.finalized_at - t.recorded_at) * 1000))::bigint
  END AS duration_ms,
  tc.state AS content_state, t.attempt, t.failover_of,
  CASE WHEN r.batch_id IS NOT NULL THEN 'batch_' || r.batch_id::text END AS batch_id,
  bl.custom_id";

const TRACE_FROM: &str = "
FROM gateway.inference_trace t
JOIN gateway.accounting_reservation r
  ON r.organization_id = t.organization_id AND r.id = t.reservation_id
LEFT JOIN gateway.catalog_deployment d
  ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
 AND d.id = r.catalog_deployment_id
LEFT JOIN LATERAL (
  SELECT u.id AS usage_id, u.confidence, u.completeness, u.finality, u.input_tokens,
         u.output_tokens, u.cached_input_tokens, u.reasoning_tokens, u.other_tokens,
         u.amount_minor AS usage_amount_minor, u.recorded_at AS usage_recorded_at
  FROM gateway.accounting_usage u
  WHERE u.organization_id = t.organization_id AND u.reservation_id = t.reservation_id
  ORDER BY u.recorded_at DESC, u.id DESC
  LIMIT 1
) lu ON true
LEFT JOIN gateway.catalog_route cr
  ON cr.organization_id = t.organization_id AND cr.id = t.catalog_route_id
LEFT JOIN gateway.inference_trace_content tc
  ON tc.organization_id = t.organization_id AND tc.trace_id = t.id
LEFT JOIN gateway.batch_line bl
  ON bl.organization_id = r.organization_id AND bl.reservation_id = r.id";

/// The trace joins a summary needs: no usage lookup.
const TRACE_SUMMARY_FROM: &str = "
FROM gateway.inference_trace t
JOIN gateway.accounting_reservation r
  ON r.organization_id = t.organization_id AND r.id = t.reservation_id
LEFT JOIN gateway.catalog_deployment d
  ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
 AND d.id = r.catalog_deployment_id";

/// One `(group, second group, currency, confidence)` row of the usage
/// report; ClickHouse's rows are read into the same shape.
#[derive(FromRow)]
pub(super) struct AggregateRow {
    pub(super) group_key: Option<String>,
    pub(super) second_key: Option<String>,
    pub(super) currency: String,
    pub(super) confidence: String,
    pub(super) request_count: i64,
    pub(super) amount_minor: i64,
    pub(super) unpriced_request_count: i64,
    pub(super) incomplete_request_count: i64,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cached_input_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) other_tokens: i64,
}

impl AggregateRow {
    #[allow(clippy::type_complexity)]
    pub(super) fn into_aggregate(
        self,
    ) -> Result<((Option<String>, Option<String>), UsageAggregate), ReportingStoreError> {
        let confidence = confidence(&self.confidence)?;
        Ok((
            (self.group_key, self.second_key),
            UsageAggregate {
                currency: self.currency,
                confidence,
                request_count: self.request_count,
                amount_minor: self.amount_minor,
                unpriced_request_count: self.unpriced_request_count,
                incomplete_request_count: self.incomplete_request_count,
                tokens: TokenTotals {
                    input: self.input_tokens,
                    output: self.output_tokens,
                    cached_input: self.cached_input_tokens,
                    reasoning: self.reasoning_tokens,
                    other: self.other_tokens,
                },
            },
        ))
    }
}

#[derive(FromRow)]
struct TraceSummaryRow {
    id: Uuid,
    organization_id: Uuid,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    status: String,
    outcome: Option<String>,
    model: String,
    deployment: String,
    provider_family: Option<String>,
    location: Option<String>,
    currency: String,
    amount_minor: Option<i64>,
    usage_confidence: Option<String>,
    recorded_at: DateTime<Utc>,
    finalized_at: Option<DateTime<Utc>>,
    correlation_id: Uuid,
    api_key_id: Option<Uuid>,
    end_user: Option<String>,
    alias: Option<String>,
    operation: Option<String>,
    error_category: Option<String>,
    duration_ms: Option<i64>,
    content_state: Option<String>,
    attempt: i16,
    failover_of: Option<Uuid>,
    batch_id: Option<String>,
    custom_id: Option<String>,
}

impl TryFrom<TraceSummaryRow> for TraceSummary {
    type Error = ReportingStoreError;

    fn try_from(row: TraceSummaryRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            organization_id: OrganizationId(row.organization_id),
            team_id: row.team_id.map(TeamId),
            service_account_id: row.service_account_id.map(ServiceAccountId),
            status: match row.status.as_str() {
                "pending" => TraceStatus::Pending,
                "finalized" => TraceStatus::Finalized,
                _ => return Err(ReportingStoreError::InvalidState),
            },
            outcome: row.outcome,
            model: row.model,
            deployment: row.deployment,
            provider_family: row.provider_family,
            location: row.location,
            currency: row.currency,
            amount_minor: row.amount_minor,
            usage_confidence: row
                .usage_confidence
                .as_deref()
                .map(confidence)
                .transpose()?,
            recorded_at: row.recorded_at,
            finalized_at: row.finalized_at,
            correlation_id: row.correlation_id,
            api_key_id: row.api_key_id,
            end_user: row.end_user,
            alias: row.alias,
            operation: row.operation,
            duration_ms: row.duration_ms,
            error_category: row.error_category,
            content_state: row.content_state,
            attempt: row.attempt,
            failover_of: row.failover_of,
            batch_id: row.batch_id,
            custom_id: row.custom_id,
        })
    }
}

#[derive(FromRow)]
struct TraceDetailRow {
    #[sqlx(flatten)]
    summary: TraceSummaryRow,
    provider_request_id: Option<String>,
    input_bound: i64,
    output_cap: Option<i64>,
    price_version: String,
    pricing_confidence: String,
    reservation_id: Uuid,
    reservation_state: String,
    reserved_minor: i64,
    expires_at: DateTime<Utc>,
    usage_id: Option<Uuid>,
    completeness: Option<String>,
    finality: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    other_tokens: Option<i64>,
    usage_amount_minor: Option<i64>,
    usage_recorded_at: Option<DateTime<Utc>>,
    reconciliation_id: Option<Uuid>,
    reconciliation_reason: Option<String>,
    reconciliation_status: Option<String>,
    reconciliation_created_at: Option<DateTime<Utc>>,
    reconciliation_resolved_at: Option<DateTime<Utc>>,
    provider_error_category: Option<String>,
    provider_error_status: Option<i16>,
    provider_error_code: Option<String>,
    catalog_release_id: Option<Uuid>,
    catalog_route_id: Option<Uuid>,
    catalog_deployment_id: Option<Uuid>,
    catalog_price_id: Option<Uuid>,
    catalog_price_source: Option<String>,
    catalog_price_override_id: Option<Uuid>,
    catalog_chat_input_count_mode: Option<String>,
}

impl TryFrom<TraceDetailRow> for TraceDetail {
    type Error = ReportingStoreError;

    fn try_from(row: TraceDetailRow) -> Result<Self, Self::Error> {
        let usage = match row.usage_id {
            None => None,
            Some(_) => Some(TraceUsage {
                confidence: row
                    .summary
                    .usage_confidence
                    .as_deref()
                    .map(confidence)
                    .transpose()?
                    .ok_or(ReportingStoreError::InvalidState)?,
                completeness: row.completeness.ok_or(ReportingStoreError::InvalidState)?,
                finality: row.finality.ok_or(ReportingStoreError::InvalidState)?,
                tokens: TraceTokens {
                    input: row.input_tokens,
                    output: row.output_tokens,
                    cached_input: row.cached_input_tokens,
                    reasoning: row.reasoning_tokens,
                    other: row.other_tokens,
                },
                amount_minor: row.usage_amount_minor,
                recorded_at: row
                    .usage_recorded_at
                    .ok_or(ReportingStoreError::InvalidState)?,
            }),
        };
        let reconciliation = match row.reconciliation_id {
            None => None,
            Some(id) => Some(TraceReconciliation {
                id,
                reason: row
                    .reconciliation_reason
                    .ok_or(ReportingStoreError::InvalidState)?,
                status: row
                    .reconciliation_status
                    .ok_or(ReportingStoreError::InvalidState)?,
                created_at: row
                    .reconciliation_created_at
                    .ok_or(ReportingStoreError::InvalidState)?,
                resolved_at: row.reconciliation_resolved_at,
            }),
        };
        let routing = match (
            row.catalog_release_id,
            row.catalog_route_id,
            row.catalog_deployment_id,
            row.catalog_price_id,
        ) {
            (Some(release_id), Some(route_id), Some(deployment_id), Some(price_id)) => {
                Some(TraceRouting {
                    release_id,
                    route_id,
                    deployment_id,
                    price_id,
                    price_source: row.catalog_price_source,
                    price_override_id: row.catalog_price_override_id,
                    chat_input_count_mode: row.catalog_chat_input_count_mode,
                })
            }
            _ => None,
        };
        Ok(Self {
            summary: row.summary.try_into()?,
            provider_request_id: row.provider_request_id,
            input_bound: row.input_bound,
            output_cap: row.output_cap,
            price_version: row.price_version,
            pricing_confidence: row.pricing_confidence,
            reservation: TraceReservation {
                id: row.reservation_id,
                state: row.reservation_state,
                reserved_minor: row.reserved_minor,
                expires_at: row.expires_at,
            },
            usage,
            reconciliation,
            provider_error: row
                .provider_error_category
                .map(|category| TraceProviderError {
                    category,
                    http_status: row
                        .provider_error_status
                        .and_then(|status| u16::try_from(status).ok()),
                    error_type: row.provider_error_code,
                }),
            routing,
        })
    }
}

#[derive(FromRow)]
struct ReconciliationRow {
    id: Uuid,
    reservation_id: Uuid,
    trace_id: Uuid,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    reason: String,
    status: String,
    reservation_state: String,
    model: String,
    deployment: String,
    currency: String,
    reserved_minor: i64,
    created_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    resolution: Option<String>,
    resolved_amount_minor: Option<i64>,
    resolved_by: Option<Uuid>,
}

impl From<ReconciliationRow> for ReconciliationItem {
    fn from(row: ReconciliationRow) -> Self {
        Self {
            id: row.id,
            reservation_id: row.reservation_id,
            trace_id: row.trace_id,
            team_id: row.team_id.map(TeamId),
            service_account_id: row.service_account_id.map(ServiceAccountId),
            reason: row.reason,
            status: row.status,
            reservation_state: row.reservation_state,
            model: row.model,
            deployment: row.deployment,
            currency: row.currency,
            reserved_minor: row.reserved_minor,
            created_at: row.created_at,
            resolved_at: row.resolved_at,
            resolution: row.resolution,
            resolved_amount_minor: row.resolved_amount_minor,
            resolved_by: row.resolved_by.map(UserAccountId),
        }
    }
}

const RECONCILIATION_SELECT: &str = "
    SELECT c.id, c.reservation_id, r.trace_id, r.team_id, r.service_account_id,
           c.reason, c.status, r.state AS reservation_state, r.model, r.deployment,
           btrim(r.currency) AS currency, r.reserved_minor, c.created_at, c.resolved_at,
           c.resolution, c.resolved_amount_minor, c.resolved_by
    FROM gateway.accounting_reconciliation c
    JOIN gateway.accounting_reservation r
      ON r.organization_id = c.organization_id AND r.id = c.reservation_id";

#[derive(Clone)]
pub struct ReportingStore {
    pool: PgPool,
    /// The ClickHouse analytics sink's reader, when configured.
    clickhouse: Option<std::sync::Arc<super::clickhouse_analytics::ClickHouseAnalytics>>,
}

impl ReportingStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            clickhouse: None,
        }
    }

    /// Reads the usage dashboard, the usage report and trace search from
    /// ClickHouse while its sink is live (D-030).
    #[must_use]
    pub fn with_clickhouse(mut self, clickhouse: crate::clickhouse::ClickHouse) -> Self {
        self.clickhouse = Some(std::sync::Arc::new(
            super::clickhouse_analytics::ClickHouseAnalytics::new(clickhouse, self.pool.clone()),
        ));
        self
    }

    pub fn clickhouse(
        &self,
    ) -> Option<std::sync::Arc<super::clickhouse_analytics::ClickHouseAnalytics>> {
        self.clickhouse.clone()
    }

    /// The usage dashboard's queries, on the same pool.
    pub fn analytics(&self) -> super::usage_analytics::UsageAnalyticsStore {
        super::usage_analytics::UsageAnalyticsStore::new(self.pool.clone())
    }

    /// Totals per `(currency, confidence)` over the whole filtered window.
    pub async fn usage_totals(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &UsageQuery,
    ) -> Result<Vec<UsageAggregate>, ReportingStoreError> {
        let sql = format!(
            "{USAGE_SCOPE}
             SELECT NULL::text AS group_key, NULL::text AS second_key, currency, confidence,
                    {USAGE_MEASURES}
             FROM scoped
             GROUP BY currency, confidence
             ORDER BY currency, confidence"
        );
        let rows = bind_usage_scope(
            sqlx::query_as::<_, AggregateRow>(&sql),
            organization_id,
            authorized_teams,
            query,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| row.into_aggregate().map(|(_, aggregate)| aggregate))
            .collect()
    }

    /// One page of `(group, currency, confidence)` rows. Returns whether a
    /// further page exists.
    pub async fn usage_rows(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &UsageQuery,
        page_size: u32,
        keyset: Option<UsageKeyset>,
    ) -> Result<(Vec<UsageReportRow>, bool), ReportingStoreError> {
        let expression = |dimension: Option<&UsageGroupBy>| match dimension {
            None => "NULL::text",
            Some(UsageGroupBy::Day) => "to_char(attributed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')",
            Some(UsageGroupBy::Team) => "team_id::text",
            Some(UsageGroupBy::ServiceAccount) => "service_account_id::text",
            Some(UsageGroupBy::Model) => "model",
            Some(UsageGroupBy::Deployment) => "deployment",
            Some(UsageGroupBy::ProviderFamily) => "provider_family",
            Some(UsageGroupBy::Location) => "location",
            Some(UsageGroupBy::ModelMaster) => "model_master",
        };
        let group = expression(query.group_by.first());
        let second = expression(query.group_by.get(1));
        let sql = format!(
            "{USAGE_SCOPE}, grouped AS (
               SELECT {group} AS group_key, {second} AS second_key, currency, confidence,
                      {USAGE_MEASURES}
               FROM scoped
               GROUP BY 1, 2, 3, 4
             )
             SELECT * FROM grouped
             WHERE $14::text IS NULL
                OR (COALESCE(group_key, '') COLLATE \"C\", COALESCE(second_key, '') COLLATE \"C\",
                    currency COLLATE \"C\", confidence COLLATE \"C\")
                   > ($14, $15, $16, $17)
             ORDER BY COALESCE(group_key, '') COLLATE \"C\", COALESCE(second_key, '') COLLATE \"C\",
                      currency COLLATE \"C\", confidence COLLATE \"C\"
             LIMIT $18"
        );
        let (group_key, second_key, currency, confidence) = match keyset {
            Some(keyset) => (
                Some(keyset.group),
                Some(keyset.second),
                Some(keyset.currency),
                Some(keyset.confidence),
            ),
            None => (None, None, None, None),
        };
        let rows = bind_usage_scope(
            sqlx::query_as::<_, AggregateRow>(&sql),
            organization_id,
            authorized_teams,
            query,
        )
        .bind(group_key)
        .bind(second_key)
        .bind(currency)
        .bind(confidence)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        report_rows(rows, query, page_size)
    }

    pub async fn list_traces(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &TraceQuery,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<(Vec<TraceSummary>, bool), ReportingStoreError> {
        let mut sql = QueryBuilder::<Postgres>::new(format!("SELECT {TRACE_COLUMNS} {TRACE_FROM}"));
        push_trace_filters(&mut sql, organization_id, authorized_teams, query);
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
            .build_query_as::<TraceSummaryRow>()
            .fetch_all(&self.pool)
            .await?;
        let mut items = rows
            .into_iter()
            .map(TraceSummary::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > page_size as usize;
        items.truncate(page_size as usize);
        Ok((items, has_more))
    }

    /// The traces with these ids, in the order given; ids that are gone,
    /// or outside the organization or the team clamp, are left out.
    /// ClickHouse found and ordered the page (`persistence::clickhouse_analytics`).
    pub async fn traces_by_ids(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        ids: &[Uuid],
    ) -> Result<Vec<TraceSummary>, ReportingStoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, TraceSummaryRow>(&format!(
            "SELECT {TRACE_COLUMNS} {TRACE_FROM}
             WHERE t.organization_id = $1
               AND ($2::uuid[] IS NULL OR t.team_id = ANY($2))
               AND t.id = ANY($3)"
        ))
        .bind(organization_id.0)
        .bind(team_clamp(authorized_teams))
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        let mut found = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let trace = TraceSummary::try_from(row)?;
            found.insert(trace.id, trace);
        }
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// Matching traces counted by one or two dimensions, largest first, at
    /// most `limit` groups. Returns the groups, the count of every matching
    /// trace and whether groups were left out.
    pub async fn summarize_traces(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        query: &TraceQuery,
        dimensions: &[TraceDimension],
        limit: u32,
    ) -> Result<(Vec<TraceGroup>, i64, bool), ReportingStoreError> {
        let keys: Vec<String> = (0..dimensions.len())
            .map(|index| format!("d{index}"))
            .collect();
        let keys = keys.join(", ");
        let mut sql = QueryBuilder::<Postgres>::new("WITH filtered AS (SELECT ");
        for (index, dimension) in dimensions.iter().enumerate() {
            sql.push(format!("{} AS d{index}, ", trace_dimension(*dimension)));
        }
        sql.push("btrim(t.currency) AS currency, t.amount_minor, t.recorded_at ");
        sql.push(TRACE_SUMMARY_FROM);
        push_trace_filters(&mut sql, organization_id, authorized_teams, query);
        // Amounts are summed per currency first, and never across currencies.
        sql.push(format!(
            "), by_currency AS (
               SELECT {keys}, currency, count(*) AS traces,
                      sum(amount_minor)::bigint AS amount_minor,
                      count(*) FILTER (WHERE amount_minor IS NULL) AS unpriced,
                      min(recorded_at) AS first_at, max(recorded_at) AS last_at
               FROM filtered GROUP BY {keys}, currency
             )
             SELECT {keys}, sum(traces)::bigint AS traces, sum(unpriced)::bigint AS unpriced,
                    array_agg(currency ORDER BY currency)
                      FILTER (WHERE amount_minor IS NOT NULL) AS currencies,
                    array_agg(amount_minor ORDER BY currency)
                      FILTER (WHERE amount_minor IS NOT NULL) AS amounts,
                    min(first_at) AS first_at, max(last_at) AS last_at,
                    (sum(sum(traces)) OVER ())::bigint AS total
             FROM by_currency GROUP BY {keys}
             ORDER BY traces DESC"
        ));
        // Ties break on the values in byte order (as ClickHouse orders
        // them), so a truncated summary is deterministic.
        for index in 0..dimensions.len() {
            sql.push(format!(", d{index} COLLATE \"C\" NULLS LAST"));
        }
        sql.push(" LIMIT ");
        sql.push_bind(i64::from(limit) + 1);
        let rows = sql.build().fetch_all(&self.pool).await?;
        let total = match rows.first() {
            Some(row) => row.try_get::<i64, _>("total")?,
            None => 0,
        };
        let mut groups = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut values = std::collections::BTreeMap::new();
            for (index, dimension) in dimensions.iter().enumerate() {
                values.insert(
                    dimension.as_str(),
                    row.try_get::<Option<String>, _>(format!("d{index}").as_str())?,
                );
            }
            let currencies: Option<Vec<String>> = row.try_get("currencies")?;
            let amounts: Option<Vec<i64>> = row.try_get("amounts")?;
            groups.push(TraceGroup {
                values,
                count: row.try_get("traces")?,
                unpriced_count: row.try_get("unpriced")?,
                amounts: currencies
                    .unwrap_or_default()
                    .into_iter()
                    .zip(amounts.unwrap_or_default())
                    .map(|(currency, amount_minor)| CurrencyAmount {
                        currency,
                        amount_minor,
                    })
                    .collect(),
                first_recorded_at: row.try_get("first_at")?,
                last_recorded_at: row.try_get("last_at")?,
            });
        }
        let truncated = groups.len() > limit as usize;
        groups.truncate(limit as usize);
        Ok((groups, total, truncated))
    }

    pub async fn get_trace(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        trace_id: Uuid,
    ) -> Result<Option<TraceDetail>, ReportingStoreError> {
        let sql = format!(
            "SELECT {TRACE_COLUMNS},
                    t.provider_request_id, t.input_bound, t.output_cap,
                    t.price_version, t.pricing_confidence,
                    r.id AS reservation_id, r.state AS reservation_state, r.reserved_minor,
                    r.expires_at,
                    lu.usage_id, lu.completeness, lu.finality, lu.input_tokens, lu.output_tokens,
                    lu.cached_input_tokens, lu.reasoning_tokens, lu.other_tokens,
                    lu.usage_amount_minor, lu.usage_recorded_at,
                    c.id AS reconciliation_id, c.reason AS reconciliation_reason,
                    c.status AS reconciliation_status,
                    c.created_at AS reconciliation_created_at,
                    c.resolved_at AS reconciliation_resolved_at,
                    t.provider_error_category, t.provider_error_status, t.provider_error_code,
                    t.catalog_release_id, t.catalog_route_id, t.catalog_deployment_id,
                    t.catalog_price_id, t.catalog_price_source, t.catalog_price_override_id,
                    t.catalog_chat_input_count_mode
             {TRACE_FROM}
             LEFT JOIN gateway.accounting_reconciliation c
               ON c.organization_id = t.organization_id AND c.reservation_id = t.reservation_id
             WHERE t.organization_id = $1
               AND t.id = $2
               AND ($3::uuid[] IS NULL OR t.team_id = ANY($3))"
        );
        let row = sqlx::query_as::<_, TraceDetailRow>(&sql)
            .bind(organization_id.0)
            .bind(trace_id)
            .bind(team_clamp(authorized_teams))
            .fetch_optional(&self.pool)
            .await?;
        row.map(TraceDetail::try_from).transpose()
    }

    pub async fn list_reconciliation_items(
        &self,
        organization_id: OrganizationId,
        status: ReconciliationStatusFilter,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<(Vec<ReconciliationItem>, bool), ReportingStoreError> {
        let status = match status {
            ReconciliationStatusFilter::All => None,
            other => Some(other.as_str()),
        };
        let rows = sqlx::query_as::<_, ReconciliationRow>(&format!(
            "{RECONCILIATION_SELECT}
             WHERE c.organization_id = $1
               AND ($2::text IS NULL OR c.status = $2)
               AND ($3::timestamptz IS NULL OR (c.created_at, c.id) > ($3, $4::uuid))
             ORDER BY c.created_at ASC, c.id ASC
             LIMIT $5"
        ))
        .bind(organization_id.0)
        .bind(status)
        .bind(keyset.map(|keyset| keyset.at))
        .bind(keyset.map(|keyset| keyset.id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let mut items: Vec<ReconciliationItem> = rows.into_iter().map(Into::into).collect();
        let has_more = items.len() > page_size as usize;
        items.truncate(page_size as usize);
        Ok((items, has_more))
    }

    /// One reconciliation item, read on the caller's connection so a
    /// mutation sees its own uncommitted resolution.
    pub async fn find_reconciliation_item(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        item_id: Uuid,
    ) -> Result<Option<ReconciliationItem>, ReportingStoreError> {
        let row = sqlx::query_as::<_, ReconciliationRow>(&format!(
            "{RECONCILIATION_SELECT}
             WHERE c.organization_id = $1 AND c.id = $2"
        ))
        .bind(organization_id.0)
        .bind(item_id)
        .fetch_optional(conn)
        .await?;
        Ok(row.map(Into::into))
    }
}

/// A page of report rows from up to `page_size + 1` aggregate rows, and
/// whether a further page exists.
pub(super) fn report_rows(
    rows: Vec<AggregateRow>,
    query: &UsageQuery,
    page_size: u32,
) -> Result<(Vec<UsageReportRow>, bool), ReportingStoreError> {
    let mut items = rows
        .into_iter()
        .map(|row| {
            row.into_aggregate().map(|((group, second), aggregate)| {
                let mut values = std::collections::BTreeMap::new();
                for (dimension, value) in query.group_by.iter().zip([group.clone(), second]) {
                    values.insert(dimension.as_str(), value);
                }
                UsageReportRow {
                    group,
                    values,
                    aggregate,
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = items.len() > page_size as usize;
    items.truncate(page_size as usize);
    Ok((items, has_more))
}

fn bind_usage_scope<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, AggregateRow, sqlx::postgres::PgArguments>,
    organization_id: OrganizationId,
    authorized_teams: Option<&[TeamId]>,
    filters: &'q UsageQuery,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, AggregateRow, sqlx::postgres::PgArguments> {
    query
        .bind(organization_id.0)
        .bind(filters.from)
        .bind(filters.to)
        .bind(team_clamp(authorized_teams))
        .bind(any(filters.team_ids.iter().map(|id| id.0).collect()))
        .bind(any(filters
            .service_account_ids
            .iter()
            .map(|id| id.0)
            .collect()))
        .bind(any(filters.models.clone()))
        .bind(any(filters.deployments.clone()))
        .bind(filters.currency.as_ref().map(|currency| currency.as_str()))
        .bind(any(filters
            .provider_families
            .iter()
            .map(|family| family.as_str().to_owned())
            .collect()))
        .bind(any(filters.locations.clone()))
        .bind(any(filters.batch_ids.clone()))
        .bind(any(filters.model_masters.clone()))
}

/// A list filter's bind: `NULL` (no filter) when empty.
fn any<T>(values: Vec<T>) -> Option<Vec<T>> {
    (!values.is_empty()).then_some(values)
}

fn trace_dimension(dimension: TraceDimension) -> &'static str {
    match dimension {
        TraceDimension::Day => "to_char(t.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')",
        TraceDimension::Team => "t.team_id::text",
        TraceDimension::ServiceAccount => "t.service_account_id::text",
        TraceDimension::Model => "t.model",
        TraceDimension::Deployment => "r.deployment",
        TraceDimension::ProviderFamily => "r.catalog_family",
        TraceDimension::Location => "d.location",
        TraceDimension::Status => "t.status",
        TraceDimension::Outcome => "t.outcome",
    }
}

/// `WHERE` for the trace list and summary: the organization, the actor's
/// team clamp and the query's filters.
fn push_trace_filters(
    sql: &mut QueryBuilder<'_, Postgres>,
    organization_id: OrganizationId,
    authorized_teams: Option<&[TeamId]>,
    query: &TraceQuery,
) {
    sql.push(" WHERE t.organization_id = ");
    sql.push_bind(organization_id.0);
    if let Some(teams) = team_clamp(authorized_teams) {
        sql.push(" AND t.team_id = ANY(");
        sql.push_bind(teams);
        sql.push(")");
    }
    let ids = |ids: Vec<Uuid>| (!ids.is_empty()).then_some(ids);
    for (column, values) in [
        (
            "t.team_id",
            ids(query.team_ids.iter().map(|id| id.0).collect()),
        ),
        (
            "t.service_account_id",
            ids(query.service_account_ids.iter().map(|id| id.0).collect()),
        ),
        ("t.api_key_id", ids(query.api_key_ids.clone())),
        ("r.batch_id", ids(query.batch_ids.clone())),
    ] {
        if let Some(values) = values {
            sql.push(format!(" AND {column} = ANY("));
            sql.push_bind(values);
            sql.push(")");
        }
    }
    if !query.end_users.is_empty() {
        sql.push(
            " AND t.end_user_id IN (SELECT eu.id FROM gateway.end_user eu \
             WHERE eu.organization_id = t.organization_id AND eu.external_id = ANY(",
        );
        sql.push_bind(query.end_users.clone());
        sql.push("))");
    }
    let statuses: Vec<String> = query
        .statuses
        .iter()
        .map(|status| status.as_str().to_owned())
        .collect();
    for (column, values) in [
        ("t.model", &query.models),
        ("r.deployment", &query.deployments),
        ("r.catalog_family", &query.provider_families),
        ("d.location", &query.locations),
        ("t.status", &statuses),
        ("t.outcome", &query.outcomes),
    ] {
        if !values.is_empty() {
            sql.push(format!(" AND {column} = ANY("));
            sql.push_bind(values.clone());
            sql.push(")");
        }
    }
    if let Some(search) = query.search.as_deref() {
        let pattern = format!("%{}%", escape_like(search));
        sql.push(" AND (t.model ILIKE ");
        sql.push_bind(pattern.clone());
        sql.push(" ESCAPE '\\' OR r.deployment ILIKE ");
        sql.push_bind(pattern);
        sql.push(" ESCAPE '\\'");
        // A trace or request id finds that request.
        if let Ok(id) = search.parse::<Uuid>() {
            sql.push(" OR t.id = ");
            sql.push_bind(id);
            sql.push(" OR t.correlation_id = ");
            sql.push_bind(id);
        }
        sql.push(")");
    }
    if let Some(from) = query.from {
        sql.push(" AND t.recorded_at >= ");
        sql.push_bind(from);
    }
    if let Some(to) = query.to {
        sql.push(" AND t.recorded_at < ");
        sql.push_bind(to);
    }
}

/// A search term with `LIKE` wildcards taken literally.
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn team_clamp(authorized_teams: Option<&[TeamId]>) -> Option<Vec<Uuid>> {
    authorized_teams.map(|teams| teams.iter().map(|team| team.0).collect())
}

fn confidence(value: &str) -> Result<ReportConfidence, ReportingStoreError> {
    ReportConfidence::parse(value).ok_or(ReportingStoreError::InvalidState)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysets_round_trip_through_claims() {
        let keyset = TimeKeyset {
            at: Utc::now(),
            id: Uuid::new_v4(),
        };
        assert_eq!(TimeKeyset::parse(&keyset.to_claim()).unwrap(), keyset);
        let usage = UsageKeyset {
            group: "gpt|model".into(),
            second: String::new(),
            currency: "USD".into(),
            confidence: "exact".into(),
        };
        assert_eq!(UsageKeyset::parse(&usage.to_claim()).unwrap(), usage);
        assert!(UsageKeyset::parse("[\"a\"]").is_err());
        // A three-value claim from before the second dimension is refused.
        assert!(UsageKeyset::parse("[\"a\",\"USD\",\"exact\"]").is_err());
    }
}
