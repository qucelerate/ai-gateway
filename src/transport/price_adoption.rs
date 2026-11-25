// HTTP transport for automatic price adoption (D-024):
// `GET`/`PUT /api/admin/v1/organizations/{org}/catalog/price-policy`,
// `GET .../catalog/price-holds` (`?all=true` for resolved ones too) and
// `POST .../catalog/price-holds/{hold_id}/dismiss`.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Setting the policy requires `Idempotency-Key`
// and `If-Match` (`*` for any state). Problems share the admin lanes'
// `application/problem+json` shape. Setting the policy and dismissing a
// hold require `Idempotency-Key` and `If-Match` (`*` for any state).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::{
    application::price_adoption::{
        PriceAdoptionError, PriceAdoptionService, ROUTE_PRICE_HOLD_DISMISS, ROUTE_PRICE_HOLDS,
        ROUTE_PRICE_POLICY,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        price_adoption::PricePolicyInput,
    },
    persistence::{admin::AdminStore, auth::AuthStore, price_adoption::PriceAdoptionStore},
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct PriceAdoptionState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<PriceAdoptionService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: PriceAdoptionStore,
) -> Router {
    Router::new()
        .route(ROUTE_PRICE_POLICY, get(get_policy).put(set_policy))
        .route(ROUTE_PRICE_HOLDS, get(list_holds))
        .route(ROUTE_PRICE_HOLD_DISMISS, post(dismiss_hold))
        .with_state(PriceAdoptionState {
            service: Arc::new(PriceAdoptionService::new(admin, store)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn get_policy(
    State(state): State<PriceAdoptionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_POLICY;
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
    State(state): State<PriceAdoptionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<PricePolicyInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_POLICY;
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

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldsQuery {
    #[serde(default)]
    all: bool,
}

async fn list_holds(
    State(state): State<PriceAdoptionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    query: Result<Query<HoldsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_HOLDS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(query)) = query else {
        return problem(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "`all` must be true or false",
            path,
            &id,
        );
    };
    match state.service.holds(actor, organization_id, query.all).await {
        Ok(items) => (StatusCode::OK, Json(serde_json::json!({ "items": items }))).into_response(),
        Err(error) => policy_problem(error, path, &id),
    }
}

async fn dismiss_hold(
    State(state): State<PriceAdoptionState>,
    headers: HeaderMap,
    Path((organization_id, hold_id)): Path<(OrganizationId, uuid::Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_HOLD_DISMISS;
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
    match state
        .service
        .dismiss(
            actor,
            organization_id,
            hold_id,
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
        Err(error) => policy_problem(error, path, &id),
    }
}

async fn actor(
    state: &PriceAdoptionState,
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

fn policy_problem(error: PriceAdoptionError, path: &str, id: &str) -> Response {
    match error {
        PriceAdoptionError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        PriceAdoptionError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        PriceAdoptionError::NotHeld => problem(
            StatusCode::CONFLICT,
            "not_held",
            "the price is no longer held",
            path,
            id,
        ),
        PriceAdoptionError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        PriceAdoptionError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        PriceAdoptionError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        PriceAdoptionError::Store(_) | PriceAdoptionError::Admin(_) => {
            tracing::warn!(error = ?error, "price adoption lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "price adoption store unavailable",
                path,
                id,
            )
        }
    }
}
