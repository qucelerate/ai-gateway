// Reporting application service.
//
// Resolves the actor's server-side authorization context, clamps each query to
// the scopes the relevant permission grants (org-wide, or an exact team set),
// runs the read-only query, and records a read audit event before returning.
// The audit write fails closed: a report that cannot be audited is not
// returned. Team filters outside the actor's scopes are denied rather than
// silently emptied.
//
// The one write is resolving a reconciliation item, which needs
// `reconciliation_resolve` (owner and admin, organization-wide) and runs
// through the admin mutation coordinator, so it is idempotent, versioned, and
// audited in the same transaction as the accounting change.

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, join_team_ids, partition_scopes};
use crate::domain::{
    admin::{
        AdminAuthorizationContext, AdminIdempotencyKey, AdminOperation, AdminPermission,
        AdminScope, IdempotencyResultMetadata,
    },
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditTargetResource,
    },
    identity::{OrganizationId, TeamId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    reporting::{
        ReconciliationItem, ReconciliationStatusFilter, TraceDetail, TraceDimension, TraceQuery,
        TraceSummary, TraceSummaryReport, UsageQuery, UsageReport,
    },
    usage_analytics::{
        PeriodTotals, RankBy, UsageEvent, UsageSeries, UsageSeriesQuery, UsageSeriesReport,
        UsageSlice,
    },
    version::{IfMatch, VersionConflict},
};
use crate::persistence::{
    accounting::{AccountingStore, AccountingStoreError, OperatorResolution},
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    reporting::{ReportingStore, ReportingStoreError, TimeKeyset, UsageKeyset},
    usage_analytics::{SeriesReport, UsageAnalyticsStore},
};

pub const ROUTE_USAGE: &str = "/api/admin/v1/organizations/{org}/usage";
pub const ROUTE_TRACES: &str = "/api/admin/v1/organizations/{org}/traces";
pub const ROUTE_TRACE: &str = "/api/admin/v1/organizations/{org}/traces/{id}";
pub const ROUTE_TRACE_SUMMARY: &str = "/api/admin/v1/organizations/{org}/traces/summary";
pub const ROUTE_USAGE_SERIES: &str = "/api/admin/v1/organizations/{org}/usage/series";
pub const ROUTE_USAGE_EVENTS: &str = "/api/admin/v1/organizations/{org}/usage/events";
/// Most groups one trace summary returns.
pub const MAX_TRACE_GROUPS: u32 = 500;
pub const ROUTE_RECONCILIATION_ITEMS: &str =
    "/api/admin/v1/organizations/{org}/reconciliation-items";
pub const ROUTE_RECONCILIATION_RESOLVE: &str =
    "/api/admin/v1/organizations/{org}/reconciliation-items/{id}/resolve";

const USAGE_COLLECTION: &str = "usage";
const TRACE_COLLECTION: &str = "traces";
const RECONCILIATION_COLLECTION: &str = "reconciliation_items";
const USAGE_EVENT_COLLECTION: &str = "usage_events";
const ORDER_USAGE: &str = "group,currency,confidence";
const ORDER_RECORDED_DESC: &str = "recorded_at desc,id desc";
const ORDER_CREATED_ASC: &str = "created_at,id";

#[derive(Debug)]
pub enum ReportingError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    Pagination(PaginationError),
    /// The resolution amount is negative.
    InvalidAmount,
    /// The item is already resolved, or its reservation is no longer unknown.
    NotOpen,
    /// In-flight accounting holds the reservation or its buckets; retry.
    Busy,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(ReportingStoreError),
    Accounting(AccountingStoreError),
    Admin(AdminStoreError),
}

/// A resolved reconciliation item and the version its resolution wrote.
#[derive(Debug)]
pub struct ResolvedItem {
    pub item: ReconciliationItem,
    pub version: u64,
}

/// Rows visible to the actor: `None` for org-wide callers, otherwise the exact
/// authorized teams. `filters` is the clamp's canonical cursor binding.
struct Clamp {
    teams: Option<Vec<TeamId>>,
    filters: String,
}

impl Clamp {
    fn teams(&self) -> Option<&[TeamId]> {
        self.teams.as_deref()
    }
}

#[derive(Clone)]
pub struct ReportingService {
    admin: AdminStore,
    analytics: UsageAnalyticsStore,
    store: ReportingStore,
    /// Answers the dashboard, report and trace search when the ClickHouse
    /// sink is live; otherwise (or on any ClickHouse failure) they read
    /// PostgreSQL.
    clickhouse:
        Option<std::sync::Arc<crate::persistence::clickhouse_analytics::ClickHouseAnalytics>>,
    codec: Option<SignedCursorCodec>,
}

impl ReportingService {
    pub fn new(admin: AdminStore, store: ReportingStore, codec: Option<SignedCursorCodec>) -> Self {
        Self {
            admin,
            analytics: store.analytics(),
            clickhouse: store.clickhouse(),
            store,
            codec,
        }
    }

    /// `read`'s answer from ClickHouse, or `None` to read PostgreSQL: the
    /// sink is not configured, not live, or ClickHouse failed.
    async fn via_clickhouse<T>(
        &self,
        read: impl AsyncFnOnce(
            &crate::persistence::clickhouse_analytics::ClickHouseAnalytics,
        ) -> Result<Option<T>, ReportingStoreError>,
    ) -> Result<Option<T>, ReportingError> {
        match self.clickhouse.as_deref() {
            Some(clickhouse) => read(clickhouse).await.map_err(ReportingError::Store),
            None => Ok(None),
        }
    }

    /// A slice of requests charted over time, with its totals and the
    /// previous window's. Needs `usage_read`, clamped like the usage report
    /// and audited as a usage read.
    pub async fn usage_series(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: &UsageSeriesQuery,
        request_id: Uuid,
    ) -> Result<UsageSeriesReport, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let clamp = clamp(&context, AdminPermission::UsageRead, &query.slice.team_ids)?;
        let previous_slice = query.slice.previous();
        let SeriesReport {
            mut series,
            truncated,
            totals,
            previous,
        } = match self
            .via_clickhouse(async |ch| {
                ch.series_report(organization_id, clamp.teams(), query, &previous_slice)
                    .await
            })
            .await?
        {
            Some(report) => report,
            None => self
                .analytics
                .series_report(organization_id, clamp.teams(), query, &previous_slice)
                .await
                .map_err(ReportingError::Store)?,
        };
        if let Some(dimension) = query.split_by.filter(|dimension| dimension.is_identity()) {
            let values: Vec<String> = series.iter().filter_map(|s| s.value.clone()).collect();
            let labels = self
                .analytics
                .labels(organization_id, dimension, &values)
                .await
                .map_err(ReportingError::Store)?;
            for item in &mut series {
                item.label = item
                    .value
                    .as_ref()
                    .and_then(|value| labels.get(value).cloned());
            }
        }
        sort_series(
            &mut series,
            query.rank_by,
            query.slice.currency.as_ref().map(|c| c.as_str()),
        );
        self.audit(
            actor,
            organization_id,
            query.slice.team(),
            AuditTargetResource::UsageReport,
            "usage.read",
            organization_id.0,
            request_id,
        )
        .await?;
        Ok(UsageSeriesReport {
            from: query.slice.from,
            to: query.slice.to,
            granularity: query.granularity,
            split_by: query.split_by,
            rank_by: query.rank_by,
            buckets: query.granularity.buckets(query.slice.from, query.slice.to),
            totals,
            previous: PeriodTotals {
                from: previous_slice.from,
                to: previous_slice.to,
                totals: previous,
            },
            series,
            truncated,
        })
    }

    /// The requests in a slice, newest first. Each is a request's metadata
    /// trace, so listing them needs `trace_read` like the trace list, under
    /// the same team clamp, and is audited as one.
    pub async fn usage_events(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        slice: &UsageSlice,
        page: &PageRequest,
        request_id: Uuid,
    ) -> Result<Page<UsageEvent>, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let clamp = clamp(&context, AdminPermission::TraceRead, &slice.team_ids)?;
        let filters = format!("{};{}", clamp.filters, digest(&slice.normalized_filters()));
        let scope = AdminScope::Organization(organization_id);
        let codec = self.codec()?;
        let keyset = decode(
            codec,
            page,
            actor,
            scope,
            USAGE_EVENT_COLLECTION,
            &filters,
            ORDER_RECORDED_DESC,
        )?
        .map(|claim| TimeKeyset::parse(&claim))
        .transpose()
        .map_err(ReportingError::Store)?;
        let (items, has_more) = match self
            .via_clickhouse(async |ch| {
                ch.events(
                    organization_id,
                    clamp.teams(),
                    slice,
                    page.page_size,
                    keyset,
                )
                .await
            })
            .await?
        {
            Some(page) => page,
            None => self
                .analytics
                .events(
                    organization_id,
                    clamp.teams(),
                    slice,
                    page.page_size,
                    keyset,
                )
                .await
                .map_err(ReportingError::Store)?,
        };
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                USAGE_EVENT_COLLECTION,
                &filters,
                ORDER_RECORDED_DESC,
                TimeKeyset {
                    at: last.recorded_at,
                    id: last.id,
                }
                .to_claim(),
            )?),
            _ => None,
        };
        self.audit(
            actor,
            organization_id,
            slice.team(),
            AuditTargetResource::InferenceTrace,
            "trace.list",
            organization_id.0,
            request_id,
        )
        .await?;
        Ok(Page::new(items, next_page_token))
    }

    pub async fn usage_report(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: &UsageQuery,
        page: &PageRequest,
        request_id: Uuid,
    ) -> Result<UsageReport, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let clamp = clamp(&context, AdminPermission::UsageRead, &query.team_ids)?;
        let filters = format!("{};{}", clamp.filters, digest(&query.normalized_filters()));
        let scope = AdminScope::Organization(organization_id);
        let codec = self.codec()?;
        let keyset = decode(
            codec,
            page,
            actor,
            scope,
            USAGE_COLLECTION,
            &filters,
            ORDER_USAGE,
        )?
        .map(|claim| UsageKeyset::parse(&claim))
        .transpose()
        .map_err(ReportingError::Store)?;
        let ((items, has_more), totals) = match self
            .via_clickhouse(async |ch| {
                ch.usage_report(
                    organization_id,
                    clamp.teams(),
                    query,
                    page.page_size,
                    keyset.as_ref(),
                )
                .await
            })
            .await?
        {
            Some(report) => report,
            None => {
                let rows = self
                    .store
                    .usage_rows(
                        organization_id,
                        clamp.teams(),
                        query,
                        page.page_size,
                        keyset,
                    )
                    .await
                    .map_err(ReportingError::Store)?;
                let totals = self
                    .store
                    .usage_totals(organization_id, clamp.teams(), query)
                    .await
                    .map_err(ReportingError::Store)?;
                (rows, totals)
            }
        };
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                USAGE_COLLECTION,
                &filters,
                ORDER_USAGE,
                UsageKeyset::of(last, &query.group_by).to_claim(),
            )?),
            _ => None,
        };
        self.audit(
            actor,
            organization_id,
            query.team(),
            AuditTargetResource::UsageReport,
            "usage.read",
            organization_id.0,
            request_id,
        )
        .await?;
        Ok(UsageReport {
            from: query.from,
            to: query.to,
            group_by: query.group_by_name(),
            totals,
            items,
            next_page_token,
        })
    }

    pub async fn list_traces(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: &TraceQuery,
        page: &PageRequest,
        request_id: Uuid,
    ) -> Result<Page<TraceSummary>, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let clamp = clamp(&context, AdminPermission::TraceRead, &query.team_ids)?;
        let filters = format!("{};{}", clamp.filters, digest(&query.normalized_filters()));
        let scope = AdminScope::Organization(organization_id);
        let codec = self.codec()?;
        let keyset = decode(
            codec,
            page,
            actor,
            scope,
            TRACE_COLLECTION,
            &filters,
            ORDER_RECORDED_DESC,
        )?
        .map(|claim| TimeKeyset::parse(&claim))
        .transpose()
        .map_err(ReportingError::Store)?;
        let (items, has_more) = match self
            .via_clickhouse(async |ch| {
                ch.list_traces(
                    organization_id,
                    clamp.teams(),
                    query,
                    page.page_size,
                    keyset,
                )
                .await
            })
            .await?
        {
            Some(page) => page,
            None => self
                .store
                .list_traces(
                    organization_id,
                    clamp.teams(),
                    query,
                    page.page_size,
                    keyset,
                )
                .await
                .map_err(ReportingError::Store)?,
        };
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                TRACE_COLLECTION,
                &filters,
                ORDER_RECORDED_DESC,
                TimeKeyset {
                    at: last.recorded_at,
                    id: last.id,
                }
                .to_claim(),
            )?),
            _ => None,
        };
        self.audit(
            actor,
            organization_id,
            query.team(),
            AuditTargetResource::InferenceTrace,
            "trace.list",
            organization_id.0,
            request_id,
        )
        .await?;
        Ok(Page::new(items, next_page_token))
    }

    /// Matching traces counted by one or two dimensions, under the same team
    /// clamp as the list and audited as a trace read.
    pub async fn summarize_traces(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: &TraceQuery,
        dimensions: Vec<TraceDimension>,
        request_id: Uuid,
    ) -> Result<TraceSummaryReport, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let clamp = clamp(&context, AdminPermission::TraceRead, &query.team_ids)?;
        let (groups, total, truncated) = match self
            .via_clickhouse(async |ch| {
                ch.summarize_traces(
                    organization_id,
                    clamp.teams(),
                    query,
                    &dimensions,
                    MAX_TRACE_GROUPS,
                )
                .await
            })
            .await?
        {
            Some(summary) => summary,
            None => self
                .store
                .summarize_traces(
                    organization_id,
                    clamp.teams(),
                    query,
                    &dimensions,
                    MAX_TRACE_GROUPS,
                )
                .await
                .map_err(ReportingError::Store)?,
        };
        self.audit(
            actor,
            organization_id,
            query.team(),
            AuditTargetResource::InferenceTrace,
            "trace.list",
            organization_id.0,
            request_id,
        )
        .await?;
        Ok(TraceSummaryReport {
            group_by: dimensions,
            total,
            truncated,
            groups,
        })
    }

    /// A trace outside the actor's scopes resolves to `NotFound`, so trace
    /// existence never leaks across tenant or team boundaries.
    pub async fn get_trace(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        trace_id: Uuid,
        request_id: Uuid,
    ) -> Result<TraceDetail, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let clamp = clamp(&context, AdminPermission::TraceRead, &[])?;
        let trace = self
            .store
            .get_trace(organization_id, clamp.teams(), trace_id)
            .await
            .map_err(ReportingError::Store)?
            .ok_or(ReportingError::NotFound)?;
        self.audit(
            actor,
            organization_id,
            trace.summary.team_id,
            AuditTargetResource::InferenceTrace,
            "trace.read",
            trace_id,
            request_id,
        )
        .await?;
        Ok(trace)
    }

    pub async fn list_reconciliation_items(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        status: ReconciliationStatusFilter,
        page: &PageRequest,
        request_id: Uuid,
    ) -> Result<Page<ReconciliationItem>, ReportingError> {
        let context = self.context(actor, organization_id).await?;
        let scope = AdminScope::Organization(organization_id);
        if !context.allows(AdminPermission::ReconciliationRead, scope) {
            return Err(ReportingError::Forbidden);
        }
        let filters = format!("status={}", status.as_str());
        let codec = self.codec()?;
        let keyset = decode(
            codec,
            page,
            actor,
            scope,
            RECONCILIATION_COLLECTION,
            &filters,
            ORDER_CREATED_ASC,
        )?
        .map(|claim| TimeKeyset::parse(&claim))
        .transpose()
        .map_err(ReportingError::Store)?;
        let (items, has_more) = self
            .store
            .list_reconciliation_items(organization_id, status, page.page_size, keyset)
            .await
            .map_err(ReportingError::Store)?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                RECONCILIATION_COLLECTION,
                &filters,
                ORDER_CREATED_ASC,
                TimeKeyset {
                    at: last.created_at,
                    id: last.id,
                }
                .to_claim(),
            )?),
            _ => None,
        };
        self.audit(
            actor,
            organization_id,
            None,
            AuditTargetResource::ReconciliationItem,
            "reconciliation.list",
            organization_id.0,
            request_id,
        )
        .await?;
        Ok(Page::new(items, next_page_token))
    }

    /// Resolves an open item by charging `amount_minor` against the quota its
    /// reservation still holds; zero releases the hold without a charge.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_reconciliation_item(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        item_id: Uuid,
        amount_minor: i64,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<ResolvedItem>, ReportingError> {
        if amount_minor < 0 {
            return Err(ReportingError::InvalidAmount);
        }
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::ReconciliationResolve,
            route: ROUTE_RECONCILIATION_RESOLVE.into(),
            method: "POST".into(),
            operation: AdminOperation::new("reconciliation_item.resolve")
                .expect("operation constant is valid"),
            idempotency_key,
            target_id: item_id,
            parent_id: None,
            target: AuditTargetResource::ReconciliationItem,
            audit_action: AuditAction::new("reconciliation_item.resolve")
                .expect("audit action constant is valid"),
            if_match,
            request_id,
            body: serde_json::json!({ "amount_minor": amount_minor }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // The recorded response, even if the item has since changed.
            MutationStart::Replay(record) => {
                let metadata = record.response_metadata.ok_or(ReportingError::NotFound)?;
                let item = metadata
                    .snapshot
                    .and_then(|snapshot| serde_json::from_value(snapshot).ok())
                    .ok_or(ReportingError::Store(ReportingStoreError::InvalidState))?;
                Ok(MutationOutcome {
                    item: ResolvedItem {
                        item,
                        version: metadata.version.unwrap_or(1),
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim and the version bump.
                let tx = mutation.transaction();
                match AccountingStore::resolve_by_operator(
                    tx,
                    organization_id,
                    item_id,
                    amount_minor,
                    actor,
                )
                .await
                .map_err(ReportingError::Accounting)?
                {
                    OperatorResolution::Resolved => {}
                    OperatorResolution::NotFound => return Err(ReportingError::NotFound),
                    OperatorResolution::NotOpen => return Err(ReportingError::NotOpen),
                    OperatorResolution::Busy => return Err(ReportingError::Busy),
                }
                let item = ReportingStore::find_reconciliation_item(tx, organization_id, item_id)
                    .await
                    .map_err(ReportingError::Store)?
                    .ok_or(ReportingError::NotFound)?;
                let next = mutation.next_version().get();
                let snapshot = serde_json::to_value(&item)
                    .map_err(|_| ReportingError::Store(ReportingStoreError::InvalidState))?;
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
                            version: Some(next),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: item_id,
                            version: Some(next),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: ResolvedItem {
                        item,
                        version: next,
                    },
                    replayed: false,
                })
            }
        }
    }

    async fn context(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<AdminAuthorizationContext, ReportingError> {
        self.admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)
    }

    fn codec(&self) -> Result<&SignedCursorCodec, ReportingError> {
        self.codec
            .as_ref()
            .ok_or(ReportingError::AuthenticationUnavailable)
    }

    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        team_id: Option<TeamId>,
        target: AuditTargetResource,
        action: &str,
        target_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), ReportingError> {
        let action = AuditAction::new(action).expect("audit action constant is valid");
        self.admin
            .record_audit(AdminAuditEvent::new(
                AuditActor::User(actor),
                organization_id,
                team_id,
                target,
                action,
                target_id,
                AuditOutcome::Succeeded,
                request_id,
                None,
                None,
            ))
            .await
            .map_err(ReportingError::Admin)
    }
}

/// Largest first by the ranking measure, ties by value; `other` last.
fn sort_series(series: &mut [UsageSeries], rank_by: RankBy, currency: Option<&str>) {
    let rank = |item: &UsageSeries| -> i64 {
        match rank_by {
            RankBy::Requests => item.totals.requests,
            RankBy::Tokens => item.totals.tokens.input + item.totals.tokens.output,
            RankBy::Cost => item
                .totals
                .amounts
                .iter()
                .filter(|amount| Some(amount.currency.as_str()) == currency)
                .map(|amount| amount.amount_minor)
                .sum(),
        }
    };
    series.sort_by(|a, b| {
        a.other
            .cmp(&b.other)
            .then_with(|| rank(b).cmp(&rank(a)))
            .then_with(|| match (&a.value, &b.value) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
}

/// Every team the query filters by must be one the actor may read.
fn clamp(
    context: &AdminAuthorizationContext,
    permission: AdminPermission,
    team_filter: &[TeamId],
) -> Result<Clamp, ReportingError> {
    let (org_wide, teams) = partition_scopes(context.authorized_scopes(permission));
    if org_wide {
        return Ok(Clamp {
            teams: None,
            filters: "scope=organization".to_owned(),
        });
    }
    if teams.is_empty() || team_filter.iter().any(|team| !teams.contains(team)) {
        return Err(ReportingError::Forbidden);
    }
    Ok(Clamp {
        filters: format!("teams={}", join_team_ids(&teams)),
        teams: Some(teams),
    })
}

/// A query's canonical filters as bound into page tokens: a digest, so long
/// filter lists never outgrow the token.
fn digest(filters: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(filters.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn decode(
    codec: &SignedCursorCodec,
    page: &PageRequest,
    actor: UserAccountId,
    scope: AdminScope,
    collection: &str,
    filters: &str,
    order: &str,
) -> Result<Option<String>, ReportingError> {
    let Some(token) = page.page_token.as_deref() else {
        return Ok(None);
    };
    let claims = codec
        .decode_and_validate(token, actor, scope, collection, filters, order)
        .map_err(ReportingError::Pagination)?;
    Ok((!claims.last_keyset.is_empty()).then_some(claims.last_keyset))
}

fn encode(
    codec: &SignedCursorCodec,
    actor: UserAccountId,
    scope: AdminScope,
    collection: &str,
    filters: &str,
    order: &str,
    keyset: String,
) -> Result<String, ReportingError> {
    let claims = CursorClaims::new(actor, scope, collection, filters, order, keyset, None)
        .map_err(ReportingError::Pagination)?;
    codec.encode(&claims).map_err(ReportingError::Pagination)
}

fn map_admin_error(error: AdminStoreError) -> ReportingError {
    match error {
        AdminStoreError::Authorization(_) => ReportingError::Forbidden,
        AdminStoreError::ResourceNotFound => ReportingError::NotFound,
        AdminStoreError::VersionConflict(conflict) => ReportingError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => ReportingError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => ReportingError::IdempotencyInProgress,
        other => ReportingError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminRole, AdminRoleBinding};

    fn context(
        bindings: Vec<AdminRoleBinding>,
        org: OrganizationId,
        user: UserAccountId,
    ) -> AdminAuthorizationContext {
        AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings,
        }
    }

    #[test]
    fn team_admin_is_clamped_and_cannot_filter_other_teams() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        let other = TeamId::new();
        let user = UserAccountId::new();
        let admin = context(
            vec![AdminRoleBinding::team(
                org,
                team,
                user,
                AdminRole::TeamAdmin,
            )],
            org,
            user,
        );
        let clamp_for = |filter: &[TeamId]| clamp(&admin, AdminPermission::UsageRead, filter);
        assert_eq!(clamp_for(&[]).unwrap().teams, Some(vec![team]));
        assert!(clamp_for(&[team]).is_ok());
        assert!(matches!(
            clamp_for(&[other]),
            Err(ReportingError::Forbidden)
        ));
        // A list is allowed only when every team in it is.
        assert!(matches!(
            clamp_for(&[team, other]),
            Err(ReportingError::Forbidden)
        ));
        // Reconciliation stays org-wide: a team admin holds no grant for it.
        assert!(matches!(
            clamp(&admin, AdminPermission::ReconciliationRead, &[]),
            Err(ReportingError::Forbidden)
        ));
    }

    #[test]
    fn org_wide_roles_see_organization_scope() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        for (role, usage, trace) in [
            (AdminRole::OrganizationOwner, true, true),
            (AdminRole::Auditor, true, true),
            (AdminRole::Operator, true, true),
            (AdminRole::ReadOnlyMember, true, false),
        ] {
            let actor = context(
                vec![AdminRoleBinding::organization(org, user, role)],
                org,
                user,
            );
            assert_eq!(
                clamp(&actor, AdminPermission::UsageRead, &[]).is_ok(),
                usage,
                "{role:?}"
            );
            assert_eq!(
                clamp(&actor, AdminPermission::TraceRead, &[]).is_ok(),
                trace,
                "{role:?}"
            );
        }
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-reporting-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_USAGE,
            ROUTE_TRACES,
            ROUTE_TRACE,
            ROUTE_TRACE_SUMMARY,
            ROUTE_USAGE_SERIES,
            ROUTE_USAGE_EVENTS,
            ROUTE_RECONCILIATION_ITEMS,
            ROUTE_RECONCILIATION_RESOLVE,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
