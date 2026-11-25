// HTTP transport for model discovery and price sync under
// `/api/admin/v1/organizations/{org}/catalog`: request a sync of a provider
// account, list and read sync jobs, list discovered models, read drift
// between the active release and what its accounts list, list price sources
// and the prices they publish, ask for a source to be fetched now, set and
// delete price mappings, and read drift between the release's prices and
// published prices.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. A sync or fetch request requires `Idempotency-Key`;
// mapping writes also require `If-Match` (`*` for any state). Problems share
// the admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    application::catalog_sync::{
        CatalogSyncError, CatalogSyncService, ROUTE_DEPLOYABLE_MODELS, ROUTE_DISCOVERED_MODELS,
        ROUTE_DISCOVERY_DRIFT, ROUTE_PRICE_DRIFT, ROUTE_PRICE_MAPPING, ROUTE_PRICE_MAPPINGS,
        ROUTE_PRICE_OBSERVATIONS, ROUTE_PRICE_SOURCE_FETCH, ROUTE_PRICE_SOURCES, ROUTE_SYNC_JOB,
        ROUTE_SYNC_JOBS,
    },
    domain::{
        auth::AuthCrypto,
        discovery::{DeployableModelQuery, DiscoveredModelQuery, SyncJobRequest},
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        pricing::{PriceMappingInput, PriceObservationQuery},
    },
    persistence::{
        admin::AdminStore, auth::AuthStore, catalog_sync::CatalogSyncStore,
        price_sync::PriceSyncStore,
    },
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct CatalogSyncState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<CatalogSyncService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobParams {
    adapter_instance_key: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelParams {
    adapter_instance_key: Option<String>,
    state: Option<String>,
    deployed: Option<bool>,
    evidence: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeployableParams {
    adapter_instance_key: Option<String>,
    deployed: Option<bool>,
    lifecycle_status: Option<String>,
    operation: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationParams {
    source: Option<String>,
    q: Option<String>,
    region: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: CatalogSyncStore,
    prices: PriceSyncStore,
) -> Router {
    let service = Arc::new(CatalogSyncService::new(
        admin,
        store,
        prices,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(ROUTE_SYNC_JOBS, get(list_jobs).post(request_sync))
        .route(ROUTE_SYNC_JOB, get(get_job))
        .route(ROUTE_DISCOVERED_MODELS, get(list_models))
        .route(ROUTE_DEPLOYABLE_MODELS, get(list_deployable))
        .route(ROUTE_DISCOVERY_DRIFT, get(drift))
        .route(ROUTE_PRICE_SOURCES, get(price_sources))
        .route(ROUTE_PRICE_SOURCE_FETCH, post(request_price_fetch))
        .route(ROUTE_PRICE_DRIFT, get(price_drift))
        .route(ROUTE_PRICE_OBSERVATIONS, get(price_observations))
        .route(ROUTE_PRICE_MAPPINGS, get(list_mappings))
        .route(ROUTE_PRICE_MAPPING, put(set_mapping).delete(delete_mapping))
        .with_state(CatalogSyncState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn request_sync(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<SyncJobRequest>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_SYNC_JOBS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .request(actor, organization_id, input, idempotency_key, request_id)
        .await
    {
        Ok(outcome) => {
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (status, Json(outcome.item)).into_response()
        }
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn list_jobs(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<JobParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SYNC_JOBS;
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
        .list_jobs(actor, organization_id, params.adapter_instance_key, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn get_job(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path((organization_id, job_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SYNC_JOB;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.get_job(actor, organization_id, job_id).await {
        Ok(job) => (StatusCode::OK, Json(job)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn list_models(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<ModelParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DISCOVERED_MODELS;
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
    let query = match DiscoveredModelQuery::filtered(
        params.adapter_instance_key,
        params.state.as_deref(),
        params.deployed,
        params.evidence.as_deref(),
    ) {
        Ok(query) => query,
        Err(error) => return sync_problem(CatalogSyncError::Invalid(error), path, &id),
    };
    match state
        .service
        .list_models(actor, organization_id, query, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn list_deployable(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<DeployableParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DEPLOYABLE_MODELS;
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
    let query = match DeployableModelQuery::new(
        params.adapter_instance_key,
        params.deployed,
        params.lifecycle_status,
        params.operation.as_deref(),
    ) {
        Ok(query) => query,
        Err(error) => return sync_problem(CatalogSyncError::Invalid(error), path, &id),
    };
    match state
        .service
        .list_deployable(actor, organization_id, query, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn drift(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DISCOVERY_DRIFT;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.drift(actor, organization_id).await {
        Ok(drift) => (StatusCode::OK, Json(drift)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn price_sources(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_SOURCES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.price_sources(actor, organization_id).await {
        Ok(sources) => (StatusCode::OK, Json(sources)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn request_price_fetch(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path((organization_id, source)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_SOURCE_FETCH;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    match state
        .service
        .request_price_fetch(actor, organization_id, source, idempotency_key, request_id)
        .await
    {
        Ok(outcome) => {
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (status, Json(outcome.item)).into_response()
        }
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn price_drift(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_DRIFT;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.price_drift(actor, organization_id).await {
        Ok(drift) => (StatusCode::OK, Json(drift)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn price_observations(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<ObservationParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_OBSERVATIONS;
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
    let query = match PriceObservationQuery::new(params.source, params.q, params.region) {
        Ok(query) => query,
        Err(error) => {
            return sync_problem(CatalogSyncError::InvalidPriceQuery(error), path, &id);
        }
    };
    match state
        .service
        .price_observations(actor, organization_id, query, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn list_mappings(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_MAPPINGS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    match state
        .service
        .list_mappings(actor, organization_id, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn set_mapping(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path((organization_id, deployment, source)): Path<(OrganizationId, String, String)>,
    body: Result<Json<PriceMappingInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_MAPPING;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .set_mapping(
            actor,
            organization_id,
            deployment,
            source,
            input,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let status = if outcome.item.created && !outcome.replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = outcome.item.mapping.version;
            let mut response = (status, Json(outcome.item.mapping)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => sync_problem(error, path, &id),
    }
}

async fn delete_mapping(
    State(state): State<CatalogSyncState>,
    headers: HeaderMap,
    Path((organization_id, deployment, source)): Path<(OrganizationId, String, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_MAPPING;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state
        .service
        .delete_mapping(
            actor,
            organization_id,
            deployment,
            source,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) if outcome.replayed => {
            (StatusCode::OK, Json(serde_json::json!({ "deleted": true }))).into_response()
        }
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => sync_problem(error, path, &id),
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

fn invalid_query(path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "query parameters are missing, unknown, or malformed",
        path,
        id,
    )
}

async fn human_actor(
    state: &CatalogSyncState,
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

fn sync_problem(error: CatalogSyncError, path: &str, id: &str) -> Response {
    match error {
        CatalogSyncError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        CatalogSyncError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        CatalogSyncError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        CatalogSyncError::Invalid(error) => problem(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            error.to_string(),
            path,
            id,
        ),
        CatalogSyncError::InvalidPriceQuery(error) => problem(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            error.to_string(),
            path,
            id,
        ),
        CatalogSyncError::InvalidMapping(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_price_mapping",
            error.to_string(),
            path,
            id,
        ),
        CatalogSyncError::UnknownPriceSource => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_price_source",
            "no price source with this id is configured",
            path,
            id,
        ),
        CatalogSyncError::UnknownDeployment => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_deployment",
            "neither the active catalog release nor an automatic offer has a deployment with \
             this name in the price source's provider family",
            path,
            id,
        ),
        CatalogSyncError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        CatalogSyncError::UnknownAccount => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_provider_account",
            "the active catalog release declares no provider account with this key",
            path,
            id,
        ),
        CatalogSyncError::InProgress => problem(
            StatusCode::CONFLICT,
            "catalog_sync_in_progress",
            "a sync of this provider account is already queued or running",
            path,
            id,
        ),
        CatalogSyncError::FetchTooSoon(seconds) => {
            let mut response = problem(
                StatusCode::TOO_MANY_REQUESTS,
                "price_fetch_too_soon",
                "the price source was fetched or is being fetched too recently; retry later",
                path,
                id,
            );
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(seconds));
            response
        }
        CatalogSyncError::StalePage => problem(
            StatusCode::CONFLICT,
            "page_token_stale",
            "prices changed since the page token was issued; start from the first page",
            path,
            id,
        ),
        CatalogSyncError::Pagination(error) => pagination_problem(error, path, id),
        CatalogSyncError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        CatalogSyncError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        CatalogSyncError::Store(_) | CatalogSyncError::Admin(_) => {
            tracing::warn!(error = ?error, "catalog sync lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "catalog sync store unavailable",
                path,
                id,
            )
        }
    }
}
