// HTTP transport for an organization's own identity provider under
// `/api/admin/v1/organizations/{org}/identity-provider`: read it, save a
// pending configuration, and disable it. The test sign-in that activates a
// pending configuration (`POST …/identity-provider/test`) starts a login, so
// it lives with the sign-in routes in `transport::http`.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. `PUT` and `DELETE` require `Idempotency-Key`
// and `If-Match` (`*` only to create). The client secret is accepted on
// `PUT` and never returned. Problems share the admin lanes'
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
    application::identity_provider::{
        IdentityProviderAdminError, IdentityProviderService, ROUTE_IDENTITY_PROVIDER,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        identity_provider::IdentityProviderInput,
    },
    persistence::auth::AuthStore,
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct IdentityProviderState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<IdentityProviderService>,
}

pub fn router(
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: IdentityProviderService,
) -> Router {
    Router::new()
        .route(
            ROUTE_IDENTITY_PROVIDER,
            get(get_provider).put(put_provider).delete(disable_provider),
        )
        .with_state(IdentityProviderState {
            auth,
            crypto,
            service: Arc::new(service),
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn get_provider(
    State(state): State<IdentityProviderState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_IDENTITY_PROVIDER;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.get(actor, organization_id).await {
        Ok(provider) => {
            let version = provider.version;
            let mut response = (StatusCode::OK, Json(provider)).into_response();
            if version > 0 {
                set_etag(&mut response, version);
            }
            response
        }
        Err(error) => provider_problem(error, path, &id),
    }
}

async fn put_provider(
    State(state): State<IdentityProviderState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<IdentityProviderInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_IDENTITY_PROVIDER;
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
        .put(
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
            let version = outcome.item.provider.version;
            let mut response = (status, Json(outcome.item.provider)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => provider_problem(error, path, &id),
    }
}

async fn disable_provider(
    State(state): State<IdentityProviderState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_IDENTITY_PROVIDER;
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
        .disable(
            actor,
            organization_id,
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
        Err(error) => provider_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &IdentityProviderState,
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

pub(crate) fn provider_problem(
    error: IdentityProviderAdminError,
    path: &str,
    id: &str,
) -> Response {
    match error {
        IdentityProviderAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        IdentityProviderAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        IdentityProviderAdminError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_identity_provider",
            error.detail(),
            path,
            id,
        ),
        IdentityProviderAdminError::EncryptionUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_encryption_unavailable",
            "client secrets need GATEWAY_CREDENTIAL_ENCRYPTION_KEY (or KMS) to be set on this gateway",
            path,
            id,
        ),
        IdentityProviderAdminError::NothingToTest => problem(
            StatusCode::CONFLICT,
            "nothing_to_test",
            "there is no saved change to test; save the configuration first",
            path,
            id,
        ),
        IdentityProviderAdminError::TestedByAnotherOwner => problem(
            StatusCode::CONFLICT,
            "tested_by_another_owner",
            "another owner saved this change; only they can test it, or save it again yourself",
            path,
            id,
        ),
        IdentityProviderAdminError::Changed => problem(
            StatusCode::CONFLICT,
            "identity_provider_changed",
            "the configuration changed since the test started; test it again",
            path,
            id,
        ),
        IdentityProviderAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        IdentityProviderAdminError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        IdentityProviderAdminError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        IdentityProviderAdminError::Store(_) | IdentityProviderAdminError::Admin(_) => {
            // Store errors carry no secret: only sealed bytes reach a query.
            tracing::warn!(error = ?error, "identity provider lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "identity provider store unavailable",
                path,
                id,
            )
        }
    }
}
