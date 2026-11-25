// HTTP transport for a provider account's automatic model offers:
// `/api/admin/v1/organizations/{org}/catalog/auto-offers/{adapter_instance_key}`
// reads (GET), sets (PUT) and deletes (DELETE) the opt-in.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. PUT and DELETE require `Idempotency-Key` and
// `If-Match` (`*` for any state). Problems share the admin lanes'
// `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    application::auto_offer::{AutoOfferError, AutoOfferService, ROUTE_AUTO_OFFER},
    domain::{
        auth::AuthCrypto,
        auto_offer::AutoOfferInput,
        identity::{OrganizationId, UserAccountId},
    },
    persistence::{admin::AdminStore, auth::AuthStore, auto_offer::AutoOfferStore},
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct AutoOfferState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<AutoOfferService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: AutoOfferStore,
) -> Router {
    Router::new()
        .route(
            ROUTE_AUTO_OFFER,
            get(get_offer).put(set_offer).delete(delete_offer),
        )
        .with_state(AutoOfferState {
            service: Arc::new(AutoOfferService::new(admin, store)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn get_offer(
    State(state): State<AutoOfferState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_AUTO_OFFER;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .get(actor, organization_id, &adapter_instance_key)
        .await
    {
        Ok(offer) => {
            let version = offer.version;
            let mut response = (StatusCode::OK, Json(offer)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => offer_problem(error, path, &id),
    }
}

async fn set_offer(
    State(state): State<AutoOfferState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
    body: Result<Json<AutoOfferInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AUTO_OFFER;
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
        .set(
            actor,
            organization_id,
            adapter_instance_key,
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
            let version = outcome.item.offer.version;
            let mut response = (status, Json(outcome.item.offer)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => offer_problem(error, path, &id),
    }
}

async fn delete_offer(
    State(state): State<AutoOfferState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AUTO_OFFER;
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
            adapter_instance_key,
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
        Err(error) => offer_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &AutoOfferState,
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

fn offer_problem(error: AutoOfferError, path: &str, id: &str) -> Response {
    match error {
        AutoOfferError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        AutoOfferError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        AutoOfferError::InvalidKey => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_auto_offer",
            "the adapter instance key is invalid",
            path,
            id,
        ),
        AutoOfferError::InvalidLocation => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_auto_offer",
            "location must be 1 to 255 bytes without surrounding spaces or control characters",
            path,
            id,
        ),
        AutoOfferError::UnknownAccount => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_provider_account",
            "the active catalog release declares no provider account with this key",
            path,
            id,
        ),
        AutoOfferError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        AutoOfferError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        AutoOfferError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        AutoOfferError::Store(_) | AutoOfferError::Admin(_) => {
            tracing::warn!(error = ?error, "auto offer lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "catalog store unavailable",
                path,
                id,
            )
        }
    }
}
