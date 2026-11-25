// HTTP transport for an organization's agents:
// `GET/POST /api/admin/v1/organizations/{org}/agents`,
// `GET/DELETE …/agents/{agent_id}` (DELETE disables the agent),
// `POST …/agents/{agent_id}/tokens` and
// `DELETE …/agents/{agent_id}/tokens/{token_id}` (revokes the token).
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Writes require `Idempotency-Key`. Problems share
// the admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use uuid::Uuid;

use crate::{
    application::agents::{
        AgentError, AgentService, ROUTE_AGENT, ROUTE_AGENT_TOKEN, ROUTE_AGENT_TOKENS, ROUTE_AGENTS,
    },
    domain::{
        agents::{AgentInput, AgentTokenInput},
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
    },
    persistence::{admin::AdminStore, agents::AgentStore, auth::AuthStore},
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
    },
};

#[derive(Clone)]
pub struct AgentState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<AgentService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: AgentStore,
) -> Router {
    Router::new()
        .route(ROUTE_AGENTS, get(list_agents).post(create_agent))
        .route(ROUTE_AGENT, get(get_agent).delete(disable_agent))
        .route(ROUTE_AGENT_TOKENS, post(create_token))
        .route(ROUTE_AGENT_TOKEN, delete(revoke_token))
        .with_state(AgentState {
            service: Arc::new(AgentService::new(admin, store)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list_agents(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_AGENTS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.list(actor, organization_id).await {
        Ok(list) => (StatusCode::OK, Json(list)).into_response(),
        Err(error) => agent_problem(error, path, &id),
    }
}

async fn get_agent(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Path((organization_id, agent_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_AGENT;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.get(actor, organization_id, agent_id).await {
        Ok(agent) => (StatusCode::OK, Json(agent)).into_response(),
        Err(error) => agent_problem(error, path, &id),
    }
}

async fn create_agent(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<AgentInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AGENTS;
    let actor = match actor(&state, &headers, path, &id).await {
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
            (status, Json(outcome.item)).into_response()
        }
        Err(error) => agent_problem(error, path, &id),
    }
}

async fn disable_agent(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Path((organization_id, agent_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AGENT;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    match state
        .service
        .disable(
            actor,
            organization_id,
            agent_id,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome.item)).into_response(),
        Err(error) => agent_problem(error, path, &id),
    }
}

async fn create_token(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Path((organization_id, agent_id)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<AgentTokenInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AGENT_TOKENS;
    let actor = match actor(&state, &headers, path, &id).await {
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
        .create_token(
            actor,
            organization_id,
            agent_id,
            input,
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
            (status, Json(outcome.item)).into_response()
        }
        Err(error) => agent_problem(error, path, &id),
    }
}

async fn revoke_token(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Path((organization_id, agent_id, token_id)): Path<(OrganizationId, Uuid, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_AGENT_TOKEN;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    match state
        .service
        .revoke_token(
            actor,
            organization_id,
            agent_id,
            token_id,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome.item)).into_response(),
        Err(error) => agent_problem(error, path, &id),
    }
}

async fn actor(
    state: &AgentState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    match crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers)
        .await
    {
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

fn agent_problem(error: AgentError, path: &str, id: &str) -> Response {
    match error {
        AgentError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        AgentError::EscalationDenied => problem(
            StatusCode::FORBIDDEN,
            "role_escalation_denied",
            "the agent's role is above your own",
            path,
            id,
        ),
        AgentError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        AgentError::Invalid(detail) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_agent",
            detail,
            path,
            id,
        ),
        AgentError::NameTaken => problem(
            StatusCode::CONFLICT,
            "agent_name_taken",
            "an agent with this name already exists in the organization",
            path,
            id,
        ),
        AgentError::TooManyAgents => problem(
            StatusCode::CONFLICT,
            "too_many_agents",
            "the organization has 200 agents, the most it may have",
            path,
            id,
        ),
        AgentError::TooManyTokens => problem(
            StatusCode::CONFLICT,
            "too_many_agent_tokens",
            "the agent has 20 live tokens, the most it may have; revoke one first",
            path,
            id,
        ),
        AgentError::AgentDisabled => problem(
            StatusCode::CONFLICT,
            "agent_disabled",
            "the agent is disabled",
            path,
            id,
        ),
        AgentError::LastOwner => problem(
            StatusCode::CONFLICT,
            "last_owner_required",
            "the organization must keep at least one enabled owner",
            path,
            id,
        ),
        AgentError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        AgentError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        AgentError::Store(_) | AgentError::Admin(_) => {
            tracing::warn!(error = ?error, "agent lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "agent store unavailable",
                path,
                id,
            )
        }
    }
}
