// HTTP transport for the routing policy (D-023, Phase 9):
// `GET`/`PUT /api/admin/v1/organizations/{org}/catalog/routing-policy`.
//
// Both require a session or an agent token and respond with
// `Cache-Control: no-store`. Setting the policy requires `Idempotency-Key`
// and `If-Match` (`*` for any state). Problems share the admin lanes'
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
    application::routing_policy::{ROUTE_ROUTING_POLICY, RoutingPolicyError, RoutingPolicyService},
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        routing_policy::RoutingPolicyInput,
    },
    persistence::{admin::AdminStore, auth::AuthStore, routing_policy::RoutingPolicyStore},
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct RoutingPolicyState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<RoutingPolicyService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: RoutingPolicyStore,
) -> Router {
    Router::new()
        .route(ROUTE_ROUTING_POLICY, get(get_policy).put(set_policy))
        .with_state(RoutingPolicyState {
            service: Arc::new(RoutingPolicyService::new(admin, store)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn get_policy(
    State(state): State<RoutingPolicyState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ROUTING_POLICY;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.policy(actor, organization_id).await {
        Ok(policy) => {
            let version = policy.version;
            let mut response = (StatusCode::OK, Json(policy)).into_response();
            if version > 0 {
                set_etag(&mut response, version);
            }
            response
        }
        Err(error) => policy_problem(error, path, &id),
    }
}

async fn set_policy(
    State(state): State<RoutingPolicyState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<RoutingPolicyInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_ROUTING_POLICY;
    let actor = match actor(&state, &headers, path, &id).await {
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
        .set_policy(
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
            let version = outcome.item.policy.version;
            let mut response = (status, Json(outcome.item.policy)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => policy_problem(error, path, &id),
    }
}

async fn actor(
    state: &RoutingPolicyState,
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

fn policy_problem(error: RoutingPolicyError, path: &str, id: &str) -> Response {
    match error {
        RoutingPolicyError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        RoutingPolicyError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        RoutingPolicyError::Invalid(invalid) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_routing_policy",
            invalid.to_string(),
            path,
            id,
        ),
        RoutingPolicyError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        RoutingPolicyError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        RoutingPolicyError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        RoutingPolicyError::Store(_) | RoutingPolicyError::Admin(_) => {
            tracing::warn!(error = ?error, "routing policy lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "routing policy store unavailable",
                path,
                id,
            )
        }
    }
}
