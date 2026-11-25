// HTTP transport for the reporting lane: usage aggregates, metadata-only
// traces, and reconciliation items under `/api/admin/v1/organizations/{org}`.
//
// Every route requires a session or an agent token. All are reads except resolving a
// reconciliation item, which requires `Idempotency-Key` and `If-Match`.
// Responses carry `Cache-Control: no-store` because they expose tenant
// financial data. Problems share the admin lanes' `application/problem+json`
// shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    application::reporting::{
        ROUTE_RECONCILIATION_ITEMS, ROUTE_RECONCILIATION_RESOLVE, ROUTE_TRACE, ROUTE_TRACE_SUMMARY,
        ROUTE_TRACES, ROUTE_USAGE, ROUTE_USAGE_EVENTS, ROUTE_USAGE_SERIES, ReportingError,
        ReportingService,
    },
    domain::{
        accounting::Currency,
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        reporting::{
            ReconciliationStatusFilter, ReportQueryError, TraceDimension, TraceFilterInput,
            TraceQuery, UsageFilterInput, UsageGroupBy, UsageQuery, split_list,
        },
        usage_analytics::{
            Granularity, RankBy, UsageDimension, UsageQueryError, UsageSeriesQuery, UsageSlice,
            UsageSliceInput,
        },
    },
    persistence::{
        admin::AdminStore,
        auth::AuthStore,
        reporting::{ReportingStore, ReportingStoreError},
    },
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Usage filters. `group_by` names one or two dimensions (default `day`);
/// each list filter is comma-separated and matches any of its values.
struct UsageParams {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    group_by: Option<String>,
    team_id: Option<String>,
    service_account_id: Option<String>,
    model: Option<String>,
    deployment: Option<String>,
    provider_family: Option<String>,
    location: Option<String>,
    batch_id: Option<String>,
    model_master: Option<String>,
    currency: Option<Currency>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

/// Usage dashboard filters: a window and comma-separated lists, each matching
/// any of its values. `granularity`, `split_by`, `limit` and `rank_by` are
/// for the series only; `page_size` and `page_token` for the events only.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceParams {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    team_id: Option<String>,
    service_account_id: Option<String>,
    api_key_id: Option<String>,
    user_id: Option<String>,
    end_user: Option<String>,
    model: Option<String>,
    alias: Option<String>,
    deployment: Option<String>,
    provider_family: Option<String>,
    location: Option<String>,
    operation: Option<String>,
    status: Option<String>,
    outcome: Option<String>,
    error_category: Option<String>,
    batch_id: Option<String>,
    model_master: Option<String>,
    currency: Option<Currency>,
    granularity: Option<String>,
    split_by: Option<String>,
    limit: Option<u32>,
    rank_by: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

impl SliceParams {
    fn slice(self) -> Result<UsageSlice, UsageQueryError> {
        UsageSlice::new(
            self.from,
            self.to,
            UsageSliceInput {
                team_ids: list(self.team_id),
                service_account_ids: list(self.service_account_id),
                api_key_ids: list(self.api_key_id),
                user_ids: list(self.user_id),
                end_users: list(self.end_user),
                models: list(self.model),
                aliases: list(self.alias),
                deployments: list(self.deployment),
                provider_families: list(self.provider_family),
                locations: list(self.location),
                operations: list(self.operation),
                statuses: list(self.status),
                outcomes: list(self.outcome),
                error_categories: list(self.error_category),
                batch_ids: list(self.batch_id),
                model_masters: list(self.model_master),
            },
            self.currency,
        )
    }

    fn series(mut self) -> Result<UsageSeriesQuery, UsageQueryError> {
        let granularity = self
            .granularity
            .take()
            .map_or(Ok(Granularity::Day), |value| Granularity::parse(&value))?;
        let split_by = self
            .split_by
            .take()
            .filter(|value| !value.trim().is_empty())
            .map(|value| UsageDimension::parse(&value))
            .transpose()?;
        let rank_by = self
            .rank_by
            .take()
            .map_or(Ok(RankBy::Requests), |value| RankBy::parse(&value))?;
        let limit = self.limit.take();
        UsageSeriesQuery::new(self.slice()?, granularity, split_by, limit, rank_by)
    }
}

/// Trace filters. Each list filter is comma-separated and matches any of its
/// values; `q` matches part of the model or deployment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceParams {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    team_id: Option<String>,
    service_account_id: Option<String>,
    api_key_id: Option<String>,
    end_user: Option<String>,
    model: Option<String>,
    deployment: Option<String>,
    provider_family: Option<String>,
    location: Option<String>,
    status: Option<String>,
    outcome: Option<String>,
    batch_id: Option<String>,
    q: Option<String>,
    /// Summary only: one or two dimensions.
    group_by: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

/// A comma-separated list filter's values (`\\,` for a comma in a value).
fn list(value: Option<String>) -> Vec<String> {
    value.as_deref().map(split_list).unwrap_or_default()
}

impl TraceParams {
    fn query(self) -> Result<TraceQuery, ReportQueryError> {
        TraceQuery::new(
            self.from,
            self.to,
            TraceFilterInput {
                team_ids: list(self.team_id),
                service_account_ids: list(self.service_account_id),
                api_key_ids: list(self.api_key_id),
                end_users: list(self.end_user),
                models: list(self.model),
                deployments: list(self.deployment),
                provider_families: list(self.provider_family),
                locations: list(self.location),
                statuses: list(self.status),
                outcomes: list(self.outcome),
                batch_ids: list(self.batch_id),
                search: self.q,
            },
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconciliationParams {
    #[serde(default)]
    status: ReconciliationStatusFilter,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveBody {
    amount_minor: i64,
}

#[derive(Clone)]
pub struct ReportingState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ReportingService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: ReportingStore,
) -> Router {
    let service = Arc::new(ReportingService::new(
        admin,
        store,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(ROUTE_USAGE, get(usage_report))
        .route(ROUTE_USAGE_SERIES, get(usage_series))
        .route(ROUTE_USAGE_EVENTS, get(usage_events))
        .route(ROUTE_TRACES, get(list_traces))
        .route(ROUTE_TRACE_SUMMARY, get(summarize_traces))
        .route(ROUTE_TRACE, get(get_trace))
        .route(ROUTE_RECONCILIATION_ITEMS, get(list_reconciliation_items))
        .route(
            ROUTE_RECONCILIATION_RESOLVE,
            post(resolve_reconciliation_item),
        )
        .with_state(ReportingState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn usage_report(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<UsageParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_USAGE;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    let group_by = match params.group_by.as_deref() {
        None => Ok(vec![UsageGroupBy::Day]),
        Some(value) => UsageGroupBy::parse_list(value),
    };
    let query = match group_by.and_then(|group_by| {
        UsageQuery::new(
            params.from,
            params.to,
            group_by,
            UsageFilterInput {
                team_ids: list(params.team_id),
                service_account_ids: list(params.service_account_id),
                models: list(params.model),
                deployments: list(params.deployment),
                provider_families: list(params.provider_family),
                locations: list(params.location),
                batch_ids: list(params.batch_id),
                model_masters: list(params.model_master),
            },
            params.currency,
        )
    }) {
        Ok(query) => query,
        Err(error) => return query_problem(error, path, &id),
    };
    match state
        .service
        .usage_report(actor, organization_id, &query, &page, request_id)
        .await
    {
        Ok(report) => ok(report),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn usage_series(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<SliceParams>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_USAGE_SERIES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    // A series is one response: no paging.
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    if params._page_size.is_some() || params._page_token.is_some() {
        return invalid_query(path, &id);
    }
    let query = match params.series() {
        Ok(query) => query,
        Err(error) => return usage_query_problem(error, path, &id),
    };
    match state
        .service
        .usage_series(actor, organization_id, &query, request_id)
        .await
    {
        Ok(report) => ok(report),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn usage_events(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<SliceParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_USAGE_EVENTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    if params.granularity.is_some()
        || params.split_by.is_some()
        || params.limit.is_some()
        || params.rank_by.is_some()
    {
        return invalid_query(path, &id);
    }
    let slice = match params.slice() {
        Ok(slice) => slice,
        Err(error) => return usage_query_problem(error, path, &id),
    };
    match state
        .service
        .usage_events(actor, organization_id, &slice, &page, request_id)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn list_traces(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<TraceParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TRACES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    if params.group_by.is_some() {
        return invalid_query(path, &id);
    }
    let query = match params.query() {
        Ok(query) => query,
        Err(error) => return query_problem(error, path, &id),
    };
    match state
        .service
        .list_traces(actor, organization_id, &query, &page, request_id)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn summarize_traces(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<TraceParams>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TRACE_SUMMARY;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    // A summary is one response: no paging.
    let Ok(Query(mut params)) = params else {
        return invalid_query(path, &id);
    };
    if params._page_size.is_some() || params._page_token.is_some() {
        return invalid_query(path, &id);
    }
    let dimensions = match params
        .group_by
        .take()
        .ok_or(ReportQueryError::InvalidGroupBy)
        .and_then(|value| TraceDimension::parse_list(&value))
    {
        Ok(dimensions) => dimensions,
        Err(error) => return query_problem(error, path, &id),
    };
    let query = match params.query() {
        Ok(query) => query,
        Err(error) => return query_problem(error, path, &id),
    };
    match state
        .service
        .summarize_traces(actor, organization_id, &query, dimensions, request_id)
        .await
    {
        Ok(summary) => ok(summary),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn get_trace(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    path_params: Result<Path<(OrganizationId, Uuid)>, PathRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TRACE;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    // A malformed trace id cannot name an existing trace.
    let Ok(Path((organization_id, trace_id))) = path_params else {
        return reporting_problem(ReportingError::NotFound, path, &id);
    };
    match state
        .service
        .get_trace(actor, organization_id, trace_id, request_id)
        .await
    {
        Ok(trace) => ok(trace),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn list_reconciliation_items(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<ReconciliationParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_RECONCILIATION_ITEMS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    match state
        .service
        .list_reconciliation_items(actor, organization_id, params.status, &page, request_id)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn resolve_reconciliation_item(
    State(state): State<ReportingState>,
    headers: HeaderMap,
    path_params: Result<Path<(OrganizationId, Uuid)>, PathRejection>,
    body: Result<Json<ResolveBody>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_RECONCILIATION_RESOLVE;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    // A malformed item id cannot name an existing item.
    let Ok(Path((organization_id, item_id))) = path_params else {
        return reporting_problem(ReportingError::NotFound, path, &id);
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .resolve_reconciliation_item(
            actor,
            organization_id,
            item_id,
            body.amount_minor,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.version;
            let mut response = ok(outcome.item.item);
            set_etag(&mut response, version);
            response
        }
        Err(error) => reporting_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &ReportingState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    let session =
        crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers)
            .await;
    match session {
        Ok(Some(actor)) => Ok(actor),
        Ok(None) => Err(problem(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "a session or an agent token is required",
            path,
            id,
        )),
        Err(_) => Err(problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        )),
    }
}

fn page_request(
    page: Result<Query<PageRequest>, QueryRejection>,
    path: &str,
    id: &str,
) -> Result<PageRequest, Response> {
    let Ok(Query(page)) = page else {
        return Err(pagination_problem(
            PaginationError::LimitOutOfBounds,
            path,
            id,
        ));
    };
    page.validate()
        .map_err(|error| pagination_problem(error, path, id))?;
    Ok(page)
}

fn ok(body: impl Serialize) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

fn invalid_query(path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "query parameters are missing, unknown, or malformed",
        path,
        id,
    )
}

fn query_problem(error: ReportQueryError, path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        error.to_string(),
        path,
        id,
    )
}

fn usage_query_problem(error: UsageQueryError, path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        error.to_string(),
        path,
        id,
    )
}

fn reporting_problem(error: ReportingError, path: &str, id: &str) -> Response {
    match error {
        ReportingError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        ReportingError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        ReportingError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        ReportingError::Pagination(error) => pagination_problem(error, path, id),
        ReportingError::InvalidAmount => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_amount",
            "amount_minor must not be negative",
            path,
            id,
        ),
        ReportingError::NotOpen => problem(
            StatusCode::CONFLICT,
            "reconciliation_not_open",
            "reconciliation item is not open",
            path,
            id,
        ),
        ReportingError::Busy => problem(
            StatusCode::CONFLICT,
            "reconciliation_busy",
            "the reservation is being settled; retry the request",
            path,
            id,
        ),
        ReportingError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        ReportingError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        ReportingError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        // Only a keyset from a validly signed token reaches the store, so a
        // parse failure there is a server-side inconsistency, not client input.
        ReportingError::Store(ReportingStoreError::InvalidState)
        | ReportingError::Store(ReportingStoreError::Database(_))
        | ReportingError::Accounting(_)
        | ReportingError::Admin(_) => {
            tracing::warn!(error = ?error, "reporting lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "reporting store unavailable",
                path,
                id,
            )
        }
    }
}
