// HTTP transport for the service-identity admin lane.
//
// Routes live under `/api/admin/v1/organizations/{org}`. Authentication is
// human-session-only (the OIDC browser session cookie); API-key bearer tokens
// are never accepted here. Every mutation requires an `Idempotency-Key` and
// conditional updates require `If-Match`. Create/rotate responses carry the
// plaintext bearer exactly once and are marked `no-store`; every other
// response exposes only secret-free metadata. Problems follow the project
// `application/problem+json` shape and always include a request id.
//
// Wire-format note: API-key `scopes` use the shared `ApiKeyScope` serde
// vocabulary (`"Inference"`, `"Embeddings"`, `"CatalogRead"`, `"AdminRead"`),
// because the create/rotate request and response DTOs are the foundation
// `ApiKeyMutationRequest`/`ApiKeyOperationResponse` types. Persisted scope
// strings remain the lowercase `as_str()` vocabulary used by verification.
//
// This module is deliberately not registered in the crate yet; a later lane
// owns registry/router wiring. The public entrypoint is
// [`router`], which accepts the concrete stores so it can be composed by the
// integration lane and exercised by this module's integration tests.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, COOKIE, ETAG},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    application::admin_identities::{
        MutationOutcome, ROUTE_API_KEY, ROUTE_API_KEY_REVOKE, ROUTE_API_KEY_ROTATE, ROUTE_API_KEYS,
        ROUTE_SERVICE_ACCOUNT, ROUTE_SERVICE_ACCOUNT_DEACTIVATE, ROUTE_SERVICE_ACCOUNTS,
        ServiceIdentityError, ServiceIdentityService,
    },
    domain::{
        admin::AdminIdempotencyKey,
        auth::AuthCrypto,
        identity::{ApiKeyId, OrganizationId, ServiceAccountId, UserAccountId},
        keys::ApiKeyMutationRequest,
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        version::IfMatch,
    },
    persistence::{
        admin::AdminStore,
        auth::AuthStore,
        service_identities::{ServiceIdentityStore, ServiceIdentityStoreError},
    },
};

pub(crate) const SESSION_COOKIE: &str = "__Host-gateway_session";
pub(crate) const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";
const IDEMPOTENCY_HEADER: &str = "idempotency-key";
const IF_MATCH_HEADER: &str = "if-match";
const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateServiceAccountRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateServiceAccountRequest {
    pub name: String,
}

/// Handler state for this module. The service holds the application
/// orchestration; the auth store and crypto authenticate human sessions.
#[derive(Clone)]
pub struct AdminIdentitiesState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ServiceIdentityService>,
}

/// Compose the service-identity admin router. The concrete stores are supplied
/// by the integration lane (which owns registry/router wiring) so this module
/// can be exercised independently by integration tests.
pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: ServiceIdentityStore,
) -> Router {
    let service = Arc::new(ServiceIdentityService::new(
        admin,
        store,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    let state = AdminIdentitiesState {
        auth,
        crypto,
        service,
    };
    Router::new()
        .route(
            ROUTE_SERVICE_ACCOUNTS,
            get(list_service_accounts).post(create_service_account),
        )
        .route(
            ROUTE_SERVICE_ACCOUNT,
            get(get_service_account)
                .patch(update_service_account)
                .delete(delete_service_account),
        )
        .route(
            ROUTE_SERVICE_ACCOUNT_DEACTIVATE,
            post(deactivate_service_account),
        )
        .route(ROUTE_API_KEYS, get(list_api_keys).post(create_api_key))
        .route(ROUTE_API_KEY, get(get_api_key))
        .route(ROUTE_API_KEY_ROTATE, post(rotate_api_key))
        .route(ROUTE_API_KEY_REVOKE, post(revoke_api_key))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Service-account handlers
// ---------------------------------------------------------------------------

/// `?enabled=true` lists only enabled service accounts, `false` only
/// disabled ones; without it, all.
#[derive(Debug, Default, serde::Deserialize)]
struct ServiceAccountFilter {
    #[serde(default)]
    enabled: Option<bool>,
}

/// `?usable=true` lists only keys that can be used now (active and not
/// expired); without it, all.
#[derive(Debug, Default, serde::Deserialize)]
struct ApiKeyFilter {
    #[serde(default)]
    usable: bool,
}

async fn list_service_accounts(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, axum::extract::rejection::QueryRejection>,
    filter: Result<Query<ServiceAccountFilter>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SERVICE_ACCOUNTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page {
        Ok(Query(page)) => page,
        Err(_) => return pagination_problem(PaginationError::LimitOutOfBounds, path, &id),
    };
    if let Err(error) = page.validate() {
        return pagination_problem(error, path, &id);
    }
    let Ok(Query(filter)) = filter else {
        return invalid_filter(path, &id, "enabled must be true or false");
    };
    match state
        .service
        .list_service_accounts(actor, organization_id, filter.enabled, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => service_problem(error, path, &id),
    }
}

async fn get_service_account(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, ServiceAccountId)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SERVICE_ACCOUNT;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .get_service_account(actor, organization_id, service_account_id)
        .await
    {
        Ok(item) => {
            let version = item.version;
            let mut response = (StatusCode::OK, Json(item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn create_service_account(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<CreateServiceAccountRequest>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_SERVICE_ACCOUNTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .create_service_account(
            actor,
            organization_id,
            &body.name,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            let version = outcome.item.version;
            let mut response = (status, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn update_service_account(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, ServiceAccountId)>,
    body: Result<Json<UpdateServiceAccountRequest>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_SERVICE_ACCOUNT;
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
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .update_service_account(
            actor,
            organization_id,
            service_account_id,
            &body.name,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.version;
            let mut response = (StatusCode::OK, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn deactivate_service_account(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, ServiceAccountId)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_SERVICE_ACCOUNT_DEACTIVATE;
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
        .deactivate_service_account(
            actor,
            organization_id,
            service_account_id,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.version;
            let mut response = (StatusCode::OK, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn delete_service_account(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, ServiceAccountId)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_SERVICE_ACCOUNT;
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
        .delete_service_account(
            actor,
            organization_id,
            service_account_id,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        // An account that has made requests is disabled, not deleted; the
        // answer says so and carries the account as it now is.
        Ok(MutationOutcome {
            item: Some(account),
            ..
        }) => {
            let version = account.version;
            let mut response = (
                StatusCode::OK,
                Json(serde_json::json!({
                    "deleted": false,
                    "disabled": true,
                    "reason": "usage_history",
                    "service_account": account,
                })),
            )
                .into_response();
            set_etag(&mut response, version);
            response
        }
        Ok(outcome) => {
            if outcome.replayed {
                (StatusCode::OK, Json(serde_json::json!({ "deleted": true }))).into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        Err(error) => service_problem(error, path, &id),
    }
}

// ---------------------------------------------------------------------------
// API-key handlers
// ---------------------------------------------------------------------------

async fn list_api_keys(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, ServiceAccountId)>,
    page: Result<Query<PageRequest>, axum::extract::rejection::QueryRejection>,
    filter: Result<Query<ApiKeyFilter>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_API_KEYS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page {
        Ok(Query(page)) => page,
        Err(_) => return pagination_problem(PaginationError::LimitOutOfBounds, path, &id),
    };
    if let Err(error) = page.validate() {
        return pagination_problem(error, path, &id);
    }
    let Ok(Query(filter)) = filter else {
        return invalid_filter(path, &id, "usable must be true or false");
    };
    match state
        .service
        .list_api_keys(
            actor,
            organization_id,
            service_account_id,
            filter.usable,
            &page,
        )
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => service_problem(error, path, &id),
    }
}

async fn get_api_key(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id, key_id)): Path<(
        OrganizationId,
        ServiceAccountId,
        ApiKeyId,
    )>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_API_KEY;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .get_api_key(actor, organization_id, service_account_id, key_id)
        .await
    {
        Ok(item) => {
            let version = item.version;
            let mut response = (StatusCode::OK, Json(item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn create_api_key(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, ServiceAccountId)>,
    body: Result<Json<ApiKeyMutationRequest>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_API_KEYS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .create_api_key(
            actor,
            organization_id,
            service_account_id,
            &body,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(response) => {
            let status = if response.replay {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            secret_response(status, response)
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn rotate_api_key(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id, key_id)): Path<(
        OrganizationId,
        ServiceAccountId,
        ApiKeyId,
    )>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_API_KEY_ROTATE;
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
        .rotate_api_key(
            actor,
            organization_id,
            service_account_id,
            key_id,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(response) => {
            let status = if response.replay {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            secret_response(status, response)
        }
        Err(error) => service_problem(error, path, &id),
    }
}

async fn revoke_api_key(
    State(state): State<AdminIdentitiesState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id, key_id)): Path<(
        OrganizationId,
        ServiceAccountId,
        ApiKeyId,
    )>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_API_KEY_REVOKE;
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
        .revoke_api_key(
            actor,
            organization_id,
            service_account_id,
            key_id,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.version;
            let mut response = (StatusCode::OK, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => service_problem(error, path, &id),
    }
}

// ---------------------------------------------------------------------------
// Authentication and precondition helpers
// ---------------------------------------------------------------------------

async fn human_actor(
    state: &AdminIdentitiesState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    match authenticated_user(state, headers).await {
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

async fn authenticated_user(
    state: &AdminIdentitiesState,
    headers: &HeaderMap,
) -> Result<Option<UserAccountId>, sqlx::Error> {
    crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers).await
}

pub(crate) fn require_idempotency_key(
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<AdminIdempotencyKey, Response> {
    let Some(value) = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key header is required for mutations",
            path,
            id,
        ));
    };
    AdminIdempotencyKey::new(value).map_err(|_| {
        problem(
            StatusCode::BAD_REQUEST,
            "idempotency_key_invalid",
            "Idempotency-Key header is invalid",
            path,
            id,
        )
    })
}

pub(crate) fn require_if_match(
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<IfMatch, Response> {
    let Some(value) = headers
        .get(IF_MATCH_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(problem(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
            "If-Match is required for conditional updates",
            path,
            id,
        ));
    };
    IfMatch::parse(value).map_err(|_| {
        problem(
            StatusCode::BAD_REQUEST,
            "if_match_invalid",
            "If-Match value is invalid",
            path,
            id,
        )
    })
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}

pub(crate) fn request_context(headers: &HeaderMap) -> (String, Uuid) {
    let candidate = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128 && !value.trim().is_empty());
    match candidate.and_then(|value| Uuid::parse_str(value).ok()) {
        // A nil request id would violate the audit `request_id` check; it is
        // regenerated rather than persisted.
        Some(uuid) if !uuid.is_nil() => (uuid.to_string(), uuid),
        _ => {
            let uuid = Uuid::new_v4();
            (uuid.to_string(), uuid)
        }
    }
}

// ---------------------------------------------------------------------------
// Problem responses
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    kind: String,
    code: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    instance: String,
    request_id: String,
}

pub(crate) fn problem(
    status: StatusCode,
    code: &'static str,
    detail: impl Into<String>,
    path: &str,
    id: &str,
) -> Response {
    let body = ProblemBody {
        kind: format!("https://gateway.invalid/problems/{code}"),
        code,
        title: status.canonical_reason().unwrap_or("Request failed"),
        status: status.as_u16(),
        detail: detail.into(),
        instance: path.to_owned(),
        request_id: id.to_owned(),
    };
    let mut response = (status, Json(body)).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(id).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    response
}

/// Response layer for read lanes that expose tenant data: every response,
/// success or problem, carries `Cache-Control: no-store`.
pub(crate) async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn service_problem(error: ServiceIdentityError, path: &str, id: &str) -> Response {
    match error {
        ServiceIdentityError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        ServiceIdentityError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        ServiceIdentityError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        ServiceIdentityError::AmbiguousTeamScope => problem(
            StatusCode::CONFLICT,
            "ambiguous_team_scope",
            "the authenticated principal is a member of multiple teams; team context is required",
            path,
            id,
        ),
        ServiceIdentityError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        ServiceIdentityError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        ServiceIdentityError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        ServiceIdentityError::InvalidRequest(detail) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            detail,
            path,
            id,
        ),
        ServiceIdentityError::InvalidState => problem(
            StatusCode::CONFLICT,
            "invalid_state",
            "resource state does not permit this operation",
            path,
            id,
        ),
        ServiceIdentityError::Pagination(error) => pagination_problem(error, path, id),
        ServiceIdentityError::Store(ServiceIdentityStoreError::NotFound) => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        ServiceIdentityError::Store(ServiceIdentityStoreError::ActiveApiKeysExist) => problem(
            StatusCode::CONFLICT,
            "active_api_keys_exist",
            "service account cannot be deleted until every API key is revoked",
            path,
            id,
        ),
        ServiceIdentityError::Store(_) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "service identity store unavailable",
            path,
            id,
        ),
        ServiceIdentityError::Admin(error) => {
            tracing::warn!(error = ?error, "unclassified admin store failure in service-identity lane");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "service identity store unavailable",
                path,
                id,
            )
        }
    }
}

fn invalid_filter(path: &str, id: &str, detail: &str) -> Response {
    problem(StatusCode::BAD_REQUEST, "invalid_query", detail, path, id)
}

pub(crate) fn pagination_problem(error: PaginationError, path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        error.problem_code(),
        error.to_string(),
        path,
        id,
    )
}

pub(crate) fn body_rejection(rejection: JsonRejection, path: &str, id: &str) -> Response {
    let status = rejection.status();
    let code = match status {
        StatusCode::UNPROCESSABLE_ENTITY => "request_body_unprocessable",
        _ => "request_body_invalid",
    };
    let detail = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "request body too large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported media type",
        StatusCode::UNPROCESSABLE_ENTITY => "request body is invalid or contains unknown fields",
        _ => "request body is invalid",
    };
    problem(status, code, detail, path, id)
}

pub(crate) fn set_etag(response: &mut Response, version: u64) {
    response.headers_mut().insert(
        ETAG,
        HeaderValue::from_str(&format!("\"v{version}\""))
            .expect("an ETag built from a version number is valid"),
    );
}

/// Responses that carry (or may have carried) a plaintext bearer must never be
/// cached or replayed by intermediaries. Applied to create/rotate responses,
/// including idempotent replays, where the secret is intentionally absent.
fn secret_response(
    status: StatusCode,
    response: crate::domain::keys::ApiKeyOperationResponse,
) -> Response {
    let mut response = (status, Json(response)).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, private"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::identity::ApiKeyMetadata;

    #[test]
    fn request_dtos_reject_unknown_fields_and_secret_smuggling() {
        assert!(serde_json::from_str::<CreateServiceAccountRequest>(r#"{"name":"bot"}"#).is_ok());
        assert!(
            serde_json::from_str::<CreateServiceAccountRequest>(
                r#"{"name":"bot","team_id":"00000000-0000-0000-0000-000000000000"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ApiKeyMutationRequest>(
                r#"{"scopes":["inference"],"digest":"deadbeef"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn secret_responses_are_no_store() {
        let metadata = ApiKeyMetadata {
            id: ApiKeyId::new(),
            organization_id: OrganizationId::new(),
            service_account_id: ServiceAccountId::new(),
            successor_of: None,
            public_prefix: "gw_0123456789abcdef0123".into(),
            scopes: vec![crate::domain::identity::ApiKeyScope::Inference],
            expires_at: None,
            revoked_at: None,
            disabled_at: None,
            status: "active".into(),
        };
        let response = secret_response(
            StatusCode::CREATED,
            crate::domain::keys::ApiKeyOperationResponse::created(metadata, "gw_plaintext".into()),
        );
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store, no-cache, private")
        );
        let replay_metadata = ApiKeyMetadata {
            id: ApiKeyId::new(),
            organization_id: OrganizationId::new(),
            service_account_id: ServiceAccountId::new(),
            successor_of: None,
            public_prefix: "gw_0123456789abcdef0123".into(),
            scopes: vec![crate::domain::identity::ApiKeyScope::Inference],
            expires_at: None,
            revoked_at: None,
            disabled_at: None,
            status: "active".into(),
        };
        let replayed = secret_response(
            StatusCode::OK,
            crate::domain::keys::ApiKeyOperationResponse::replayed(replay_metadata),
        );
        assert_eq!(
            replayed
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store, no-cache, private")
        );
    }

    #[test]
    fn problem_responses_include_request_id_and_problem_content_type() {
        let response = problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            "/api/admin/v1/organizations/{org}/service-accounts",
            "req-123",
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        assert_eq!(
            response
                .headers()
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("req-123")
        );
    }

    #[test]
    fn cookie_parsing_is_scoped_by_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static(
                "__Host-gateway_session=abc; __Host-gateway_oidc_browser=def; other=x",
            ),
        );
        assert_eq!(cookie(&headers, SESSION_COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie(&headers, BROWSER_COOKIE).as_deref(), Some("def"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn request_context_generates_or_echoes_a_uuid() {
        let mut headers = HeaderMap::new();
        let (id, uuid) = request_context(&headers);
        assert_eq!(id, uuid.to_string());
        let uuid_value = Uuid::new_v4();
        headers.insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_str(&uuid_value.to_string()).unwrap(),
        );
        let (id, uuid) = request_context(&headers);
        assert_eq!(id, uuid_value.to_string());
        assert_eq!(uuid, uuid_value);
        // A nil request id would violate the audit `request_id` check; it is
        // regenerated instead of echoed.
        headers.insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_static("00000000-0000-0000-0000-000000000000"),
        );
        let (id, uuid) = request_context(&headers);
        assert_ne!(uuid.to_string(), "00000000-0000-0000-0000-000000000000");
        assert_eq!(id, uuid.to_string());
    }
}
