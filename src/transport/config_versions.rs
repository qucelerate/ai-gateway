// HTTP transport for configuration versions under
// `/api/admin/v1/organizations/{org}/config-versions`: list the
// organization's versions, read its pin, pin it to a version or unpin it, and
// list the API-serving instances with the version each serves it.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Pin writes require `Idempotency-Key` and
// `If-Match` (`*` for any state, `"v0"` before the first pin). Problems share
// the admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    application::config_versions::{
        ConfigAdminError, ConfigAdminService, PinInput, PinView, ROUTE_CONFIG_INSTANCES,
        ROUTE_CONFIG_PIN, ROUTE_CONFIG_VERSIONS,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
    },
    persistence::{admin::AdminStore, auth::AuthStore, config_versions::ConfigStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct ConfigVersionState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ConfigAdminService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: ConfigStore,
) -> Router {
    let service = Arc::new(ConfigAdminService::new(
        admin,
        store,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(ROUTE_CONFIG_VERSIONS, get(list_versions))
        .route(
            ROUTE_CONFIG_PIN,
            get(get_pin).put(set_pin).delete(delete_pin),
        )
        .route(ROUTE_CONFIG_INSTANCES, get(list_instances))
        .with_state(ConfigVersionState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list_versions(
    State(state): State<ConfigVersionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_CONFIG_VERSIONS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(page)) = page else {
        return pagination_problem(PaginationError::LimitOutOfBounds, path, &id);
    };
    if let Err(error) = page.validate() {
        return pagination_problem(error, path, &id);
    }
    match state.service.versions(actor, organization_id, &page).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => config_problem(error, path, &id),
    }
}

async fn list_instances(
    State(state): State<ConfigVersionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_CONFIG_INSTANCES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.instances(actor, organization_id).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => config_problem(error, path, &id),
    }
}

async fn get_pin(
    State(state): State<ConfigVersionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_CONFIG_PIN;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.pin(actor, organization_id).await {
        Ok(pin) => pin_response(pin),
        Err(error) => config_problem(error, path, &id),
    }
}

async fn set_pin(
    State(state): State<ConfigVersionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<PinInput>, JsonRejection>,
) -> Response {
    let path = ROUTE_CONFIG_PIN;
    let (id, _) = request_context(&headers);
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    write_pin(state, headers, organization_id, Some(input)).await
}

async fn delete_pin(
    State(state): State<ConfigVersionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    write_pin(state, headers, organization_id, None).await
}

async fn write_pin(
    state: ConfigVersionState,
    headers: HeaderMap,
    organization_id: OrganizationId,
    input: Option<PinInput>,
) -> Response {
    let path = ROUTE_CONFIG_PIN;
    let (id, request_id) = request_context(&headers);
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
        .set_pin(
            actor,
            organization_id,
            input.map(|input| input.version_id),
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => pin_response(outcome.item),
        Err(error) => config_problem(error, path, &id),
    }
}

fn pin_response(pin: PinView) -> Response {
    let version = pin.version;
    let mut response = (StatusCode::OK, Json(pin)).into_response();
    set_etag(&mut response, version);
    response
}

async fn human_actor(
    state: &ConfigVersionState,
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

fn config_problem(error: ConfigAdminError, path: &str, id: &str) -> Response {
    match error {
        ConfigAdminError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        ConfigAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        ConfigAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        ConfigAdminError::NotPinned => problem(
            StatusCode::CONFLICT,
            "config_not_pinned",
            "the organization is not pinned to a configuration version",
            path,
            id,
        ),
        ConfigAdminError::Pagination(error) => pagination_problem(error, path, id),
        ConfigAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        ConfigAdminError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        ConfigAdminError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        ConfigAdminError::Store(_) | ConfigAdminError::Admin(_) => {
            tracing::warn!(error = ?error, "config version lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "configuration store unavailable",
                path,
                id,
            )
        }
    }
}
