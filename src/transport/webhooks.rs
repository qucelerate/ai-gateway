// HTTP transport for webhook endpoints under
// `/api/admin/v1/organizations/{org}/webhook-endpoints`: list and create an
// organization's endpoints; read, update and delete one; rotate its signing
// secret; list its deliveries; and redeliver a parked delivery.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Writes require `Idempotency-Key`; writes to an
// existing endpoint also require `If-Match` (`*` for any state). Redelivery
// takes no `If-Match`: the delivery must be parked instead. Problems
// share the admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use uuid::Uuid;

use crate::{
    application::webhooks::{
        ROUTE_WEBHOOK_DELIVERIES, ROUTE_WEBHOOK_DELIVERY_REDELIVER, ROUTE_WEBHOOK_ENDPOINT,
        ROUTE_WEBHOOK_ENDPOINT_ROTATE_SECRET, ROUTE_WEBHOOK_ENDPOINTS, WebhookAdminError,
        WebhookAdminService,
    },
    domain::{
        auth::AuthCrypto,
        credentials::CredentialCipher,
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        webhook::{WebhookEndpointInput, WebhookEndpointUpdate},
    },
    persistence::{admin::AdminStore, auth::AuthStore, webhooks::WebhookStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct WebhookState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<WebhookAdminService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: WebhookStore,
    cipher: Option<CredentialCipher>,
) -> Router {
    let service = Arc::new(WebhookAdminService::new(
        admin,
        store,
        cipher,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(
            ROUTE_WEBHOOK_ENDPOINTS,
            get(list_endpoints).post(create_endpoint),
        )
        .route(
            ROUTE_WEBHOOK_ENDPOINT,
            get(get_endpoint)
                .patch(update_endpoint)
                .delete(delete_endpoint),
        )
        .route(ROUTE_WEBHOOK_ENDPOINT_ROTATE_SECRET, post(rotate_secret))
        .route(ROUTE_WEBHOOK_DELIVERIES, get(list_deliveries))
        .route(ROUTE_WEBHOOK_DELIVERY_REDELIVER, post(redeliver))
        .with_state(WebhookState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list_endpoints(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_WEBHOOK_ENDPOINTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    match state.service.list(actor, organization_id, &page).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn create_endpoint(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<WebhookEndpointInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_WEBHOOK_ENDPOINTS;
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
        .create(actor, organization_id, input, idempotency_key, request_id)
        .await
    {
        Ok(outcome) => {
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            let version = outcome.item.endpoint.version;
            let mut response = (status, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn get_endpoint(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path((organization_id, endpoint_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_WEBHOOK_ENDPOINT;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.get(actor, organization_id, endpoint_id).await {
        Ok(endpoint) => {
            let version = endpoint.version;
            let mut response = (StatusCode::OK, Json(endpoint)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn update_endpoint(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path((organization_id, endpoint_id)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<WebhookEndpointUpdate>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_WEBHOOK_ENDPOINT;
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
        .update(
            actor,
            organization_id,
            endpoint_id,
            input,
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
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn rotate_secret(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path((organization_id, endpoint_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_WEBHOOK_ENDPOINT_ROTATE_SECRET;
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
        .rotate_secret(
            actor,
            organization_id,
            endpoint_id,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.endpoint.version;
            let mut response = (StatusCode::OK, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn delete_endpoint(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path((organization_id, endpoint_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_WEBHOOK_ENDPOINT;
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
        .delete(
            actor,
            organization_id,
            endpoint_id,
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
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn list_deliveries(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path((organization_id, endpoint_id)): Path<(OrganizationId, Uuid)>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_WEBHOOK_DELIVERIES;
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
        .list_deliveries(actor, organization_id, endpoint_id, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => webhook_problem(error, path, &id),
    }
}

async fn redeliver(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path((organization_id, endpoint_id, delivery_id)): Path<(OrganizationId, Uuid, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_WEBHOOK_DELIVERY_REDELIVER;
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
        .redeliver(
            actor,
            organization_id,
            endpoint_id,
            delivery_id,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome.item)).into_response(),
        Err(error) => webhook_problem(error, path, &id),
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

async fn human_actor(
    state: &WebhookState,
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

fn webhook_problem(error: WebhookAdminError, path: &str, id: &str) -> Response {
    match error {
        WebhookAdminError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        WebhookAdminError::EncryptionUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_encryption_unavailable",
            "webhook secret storage is not configured on this gateway",
            path,
            id,
        ),
        WebhookAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        WebhookAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        WebhookAdminError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_webhook_endpoint",
            error.to_string(),
            path,
            id,
        ),
        WebhookAdminError::LimitReached => problem(
            StatusCode::CONFLICT,
            "webhook_endpoint_limit_reached",
            "the organization already has the maximum number of webhook endpoints",
            path,
            id,
        ),
        WebhookAdminError::NotParked => problem(
            StatusCode::CONFLICT,
            "webhook_delivery_not_parked",
            "only a parked delivery can be redelivered",
            path,
            id,
        ),
        WebhookAdminError::Pagination(error) => pagination_problem(error, path, id),
        WebhookAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        WebhookAdminError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        WebhookAdminError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        WebhookAdminError::Store(_) | WebhookAdminError::Admin(_) => {
            tracing::warn!(error = ?error, "webhook lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "webhook store unavailable",
                path,
                id,
            )
        }
    }
}
