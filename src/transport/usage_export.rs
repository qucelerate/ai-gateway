// HTTP transport for an organization's usage export:
// `/api/admin/v1/organizations/{org}/usage-export` reads (GET), sets (PUT)
// and deletes (DELETE) it.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. PUT and DELETE require `Idempotency-Key` and
// `If-Match` (`*` for any state). A destination the gateway cannot write is
// refused with 422 `destination_unverified`, whose detail is a fixed phrase
// naming what failed. Problems share the admin lanes'
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
    application::usage_export::{ROUTE_USAGE_EXPORT, UsageExportError, UsageExportService},
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        usage_export::{DestinationInput, ObjectWriter},
    },
    persistence::{admin::AdminStore, auth::AuthStore, usage_export::UsageExportStore},
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct UsageExportState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<UsageExportService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: UsageExportStore,
    writer: Arc<dyn ObjectWriter>,
) -> Router {
    Router::new()
        .route(
            ROUTE_USAGE_EXPORT,
            get(get_export).put(set_export).delete(delete_export),
        )
        .with_state(UsageExportState {
            service: Arc::new(UsageExportService::new(admin, store, writer)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn get_export(
    State(state): State<UsageExportState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_USAGE_EXPORT;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.get(actor, organization_id).await {
        Ok(export) => {
            let version = export.version;
            let mut response = (StatusCode::OK, Json(export)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => export_problem(error, path, &id),
    }
}

async fn set_export(
    State(state): State<UsageExportState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<DestinationInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_USAGE_EXPORT;
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
            let version = outcome.item.export.version;
            let mut response = (status, Json(outcome.item.export)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => export_problem(error, path, &id),
    }
}

async fn delete_export(
    State(state): State<UsageExportState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_USAGE_EXPORT;
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
        Err(error) => export_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &UsageExportState,
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

fn export_problem(error: UsageExportError, path: &str, id: &str) -> Response {
    match error {
        UsageExportError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        UsageExportError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        UsageExportError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_usage_export",
            error.to_string(),
            path,
            id,
        ),
        UsageExportError::Unverified(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_unverified",
            format!("the verification object could not be written: {error}"),
            path,
            id,
        ),
        UsageExportError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        UsageExportError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        UsageExportError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        UsageExportError::Store(_) | UsageExportError::Admin(_) => {
            tracing::warn!(error = ?error, "usage export lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "usage export store unavailable",
                path,
                id,
            )
        }
    }
}
