// HTTP transport for spending budgets under
// `/api/admin/v1/organizations/{org}/budgets`: list the organization's
// budgets with what their current windows have used, and set or delete the
// budget of the organization, a team, or a service account.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Writes require `Idempotency-Key` and `If-Match`
// (`*` for any state). Problems share the admin lanes'
// `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use uuid::Uuid;

use crate::{
    application::budgets::{
        BudgetAdminError, BudgetAdminService, ROUTE_API_KEY_BUDGET, ROUTE_BUDGETS,
        ROUTE_ORGANIZATION_BUDGET, ROUTE_SERVICE_ACCOUNT_BUDGET, ROUTE_TEAM_BUDGET,
    },
    domain::{
        auth::AuthCrypto,
        budget::{BudgetInput, BudgetScope},
        identity::{ApiKeyId, OrganizationId, ServiceAccountId, TeamId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
    },
    persistence::{admin::AdminStore, auth::AuthStore, budgets::BudgetStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct BudgetState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<BudgetAdminService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: BudgetStore,
) -> Router {
    let service = Arc::new(BudgetAdminService::new(
        admin,
        store,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(ROUTE_BUDGETS, get(list_policies))
        .route(
            ROUTE_ORGANIZATION_BUDGET,
            put(set_organization_policy).delete(delete_organization_policy),
        )
        .route(
            ROUTE_TEAM_BUDGET,
            put(set_team_policy).delete(delete_team_policy),
        )
        .route(
            ROUTE_SERVICE_ACCOUNT_BUDGET,
            put(set_service_account_policy).delete(delete_service_account_policy),
        )
        .route(
            ROUTE_API_KEY_BUDGET,
            put(set_api_key_policy).delete(delete_api_key_policy),
        )
        .with_state(BudgetState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list_policies(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_BUDGETS;
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
    match state.service.list(actor, organization_id, &page).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => budget_problem(error, path, &id),
    }
}

async fn set_organization_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<BudgetInput>, JsonRejection>,
) -> Response {
    set_policy(
        state,
        headers,
        organization_id,
        BudgetScope::Organization,
        ROUTE_ORGANIZATION_BUDGET,
        body,
    )
    .await
}

async fn set_team_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path((organization_id, team_id)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<BudgetInput>, JsonRejection>,
) -> Response {
    set_policy(
        state,
        headers,
        organization_id,
        BudgetScope::Team(TeamId(team_id)),
        ROUTE_TEAM_BUDGET,
        body,
    )
    .await
}

async fn set_service_account_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<BudgetInput>, JsonRejection>,
) -> Response {
    set_policy(
        state,
        headers,
        organization_id,
        BudgetScope::ServiceAccount(ServiceAccountId(service_account_id)),
        ROUTE_SERVICE_ACCOUNT_BUDGET,
        body,
    )
    .await
}

async fn set_api_key_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path((organization_id, api_key_id)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<BudgetInput>, JsonRejection>,
) -> Response {
    set_policy(
        state,
        headers,
        organization_id,
        BudgetScope::ApiKey(ApiKeyId(api_key_id)),
        ROUTE_API_KEY_BUDGET,
        body,
    )
    .await
}

async fn delete_organization_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    delete_policy(
        state,
        headers,
        organization_id,
        BudgetScope::Organization,
        ROUTE_ORGANIZATION_BUDGET,
    )
    .await
}

async fn delete_team_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path((organization_id, team_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    delete_policy(
        state,
        headers,
        organization_id,
        BudgetScope::Team(TeamId(team_id)),
        ROUTE_TEAM_BUDGET,
    )
    .await
}

async fn delete_service_account_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path((organization_id, service_account_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    delete_policy(
        state,
        headers,
        organization_id,
        BudgetScope::ServiceAccount(ServiceAccountId(service_account_id)),
        ROUTE_SERVICE_ACCOUNT_BUDGET,
    )
    .await
}

async fn delete_api_key_policy(
    State(state): State<BudgetState>,
    headers: HeaderMap,
    Path((organization_id, api_key_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    delete_policy(
        state,
        headers,
        organization_id,
        BudgetScope::ApiKey(ApiKeyId(api_key_id)),
        ROUTE_API_KEY_BUDGET,
    )
    .await
}

async fn set_policy(
    state: BudgetState,
    headers: HeaderMap,
    organization_id: OrganizationId,
    scope: BudgetScope,
    path: &str,
    body: Result<Json<BudgetInput>, JsonRejection>,
) -> Response {
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
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .set(
            actor,
            organization_id,
            scope,
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
        Err(error) => budget_problem(error, path, &id),
    }
}

async fn delete_policy(
    state: BudgetState,
    headers: HeaderMap,
    organization_id: OrganizationId,
    scope: BudgetScope,
    path: &str,
) -> Response {
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
        .delete(
            actor,
            organization_id,
            scope,
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
        Err(error) => budget_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &BudgetState,
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

fn budget_problem(error: BudgetAdminError, path: &str, id: &str) -> Response {
    match error {
        BudgetAdminError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        BudgetAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        BudgetAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        BudgetAdminError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_budget",
            error.to_string(),
            path,
            id,
        ),
        BudgetAdminError::Pagination(error) => pagination_problem(error, path, id),
        BudgetAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        BudgetAdminError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        BudgetAdminError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        BudgetAdminError::Store(_) | BudgetAdminError::Admin(_) => {
            tracing::warn!(error = ?error, "budget lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "budget store unavailable",
                path,
                id,
            )
        }
    }
}
