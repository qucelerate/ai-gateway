// HTTP transport for a provider account's model agreements (D-035):
// `GET /api/admin/v1/organizations/{org}/catalog/agreements/{adapter_instance_key}`
// lists the models that need one, with their offers;
// `POST .../accept` accepts them.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. POST requires `Idempotency-Key`. Problems share
// the admin lanes' `application/problem+json` shape; an offer that changed
// adds `changes`. Offer tokens never appear in a response.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::{
    application::model_agreements::{
        AgreementError, ModelAgreementService, OfferChange, ROUTE_AGREEMENTS,
        ROUTE_AGREEMENTS_ACCEPT,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        model_agreement::{AcceptInput, AgreementReason},
    },
    persistence::auth::AuthStore,
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
    },
};

#[derive(Clone)]
pub struct ModelAgreementState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ModelAgreementService>,
}

pub fn router(
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: ModelAgreementService,
) -> Router {
    Router::new()
        .route(ROUTE_AGREEMENTS, get(list))
        .route(ROUTE_AGREEMENTS_ACCEPT, post(accept))
        .with_state(ModelAgreementState {
            service: Arc::new(service),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list(
    State(state): State<ModelAgreementState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_AGREEMENTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .get(actor, organization_id, &adapter_instance_key)
        .await
    {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(error) => agreement_problem(error, path, &id),
    }
}

async fn accept(
    State(state): State<ModelAgreementState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
    body: Result<Json<AcceptInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AGREEMENTS_ACCEPT;
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
        .accept(
            actor,
            organization_id,
            adapter_instance_key,
            input,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome.item)).into_response(),
        Err(error) => agreement_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &ModelAgreementState,
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

/// `agreement_offer_changed`, naming each changed model's current offers.
fn offer_changed(changes: Vec<OfferChange>, path: &str, id: &str) -> Response {
    let status = StatusCode::CONFLICT;
    let body = serde_json::json!({
        "type": "https://gateway.invalid/problems/agreement_offer_changed",
        "code": "agreement_offer_changed",
        "title": status.canonical_reason().unwrap_or("Request failed"),
        "status": status.as_u16(),
        "detail": "a model's current offer is not the one named; nothing was accepted. Read the agreements again and accept the current offer.",
        "instance": path,
        "request_id": id,
        "changes": changes,
    });
    let mut response = (status, Json(body)).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

fn agreement_problem(error: AgreementError, path: &str, id: &str) -> Response {
    match error {
        AgreementError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        AgreementError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        AgreementError::InvalidKey => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_agreement_request",
            "the adapter instance key is invalid",
            path,
            id,
        ),
        AgreementError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_agreement_request",
            error.to_string(),
            path,
            id,
        ),
        AgreementError::UnknownAccount => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_provider_account",
            "the active catalog release declares no provider account with this key",
            path,
            id,
        ),
        AgreementError::Unsupported => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agreements_unsupported",
            "the provider account's family has no model agreements (only Bedrock's)",
            path,
            id,
        ),
        AgreementError::Unavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "agreements_unavailable",
            "the gateway cannot reach this provider account: it has no usable credential",
            path,
            id,
        ),
        AgreementError::ProcessCredential => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agreements_need_tenant_credential",
            "this provider account is called with the gateway operator's own credential, so \
             accepting an agreement would subscribe the operator's AWS account; agreements are \
             only for an account the organization connected with its own role",
            path,
            id,
        ),
        AgreementError::PermissionMissing => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agreement_permission_missing",
            format!(
                "{} Nothing was accepted.",
                AgreementReason::AgreementPermissionMissing.detail()
            ),
            path,
            id,
        ),
        AgreementError::OfferChanged(changes) => offer_changed(changes, path, id),
        AgreementError::Provider(error) => {
            tracing::warn!(error = ?error, "reading a provider account's models for agreements failed");
            problem(
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
                "the provider failed to list the account's models",
                path,
                id,
            )
        }
        AgreementError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        AgreementError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        AgreementError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        AgreementError::Store(_) | AgreementError::Admin(_) => {
            tracing::warn!(error = ?error, "model agreement lane failure");
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
