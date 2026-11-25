// HTTP transport for invitations under
// `/api/admin/v1/organizations/{org}/invitations`: list an organization's
// invitations, invite someone by link, and revoke a pending invitation.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Writes require `Idempotency-Key`; they take no
// `If-Match`: an invitation is only ever created or revoked, and a revoke
// requires it to be pending instead. Problems share the admin lanes'
// `application/problem+json` shape. The link in a create response is never
// logged.

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
    application::invitations::{
        InvitationAdminError, InvitationAdminService, InvitationInput, InvitationLinks,
        ROUTE_INVITATION_REVOKE, ROUTE_INVITATIONS,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
    },
    persistence::{admin::AdminStore, auth::AuthStore, invitations::InvitationStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key,
    },
};

#[derive(Clone)]
pub struct InvitationState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<InvitationAdminService>,
}

/// `links` is `None` when `GATEWAY_ORIGIN` or `OIDC_ISSUER` is not set: the
/// list still works, and create answers `invitations_unavailable`.
pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: InvitationStore,
    links: Option<InvitationLinks>,
) -> Router {
    let service = Arc::new(InvitationAdminService::new(
        admin,
        store,
        links,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(
            ROUTE_INVITATIONS,
            get(list_invitations).post(create_invitation),
        )
        .route(ROUTE_INVITATION_REVOKE, post(revoke_invitation))
        .with_state(InvitationState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list_invitations(
    State(state): State<InvitationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_INVITATIONS;
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
        Err(error) => invitation_problem(error, path, &id),
    }
}

async fn create_invitation(
    State(state): State<InvitationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<InvitationInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_INVITATIONS;
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
            (status, Json(outcome.item)).into_response()
        }
        Err(error) => invitation_problem(error, path, &id),
    }
}

async fn revoke_invitation(
    State(state): State<InvitationState>,
    headers: HeaderMap,
    Path((organization_id, invitation_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_INVITATION_REVOKE;
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
        .revoke(
            actor,
            organization_id,
            invitation_id,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome.item)).into_response(),
        Err(error) => invitation_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &InvitationState,
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

fn invitation_problem(error: InvitationAdminError, path: &str, id: &str) -> Response {
    match error {
        InvitationAdminError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        InvitationAdminError::LinksUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "invitations_unavailable",
            "invitation links need GATEWAY_ORIGIN and OIDC_ISSUER to be set on this gateway",
            path,
            id,
        ),
        InvitationAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        InvitationAdminError::EscalationDenied => problem(
            StatusCode::FORBIDDEN,
            "escalation_denied",
            "you can only invite people in a role at or below your own",
            path,
            id,
        ),
        InvitationAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        InvitationAdminError::NotPending => problem(
            StatusCode::CONFLICT,
            "invitation_not_pending",
            "the invitation was already accepted, revoked or has expired",
            path,
            id,
        ),
        InvitationAdminError::Invalid(detail) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_invitation",
            detail,
            path,
            id,
        ),
        InvitationAdminError::Pagination(error) => pagination_problem(error, path, id),
        InvitationAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        InvitationAdminError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        InvitationAdminError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        InvitationAdminError::Store(_) | InvitationAdminError::Admin(_) => {
            // Store errors carry no token: it never reaches a query that
            // could fail with it, only its digest does.
            tracing::warn!(error = ?error, "invitation lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "invitation store unavailable",
                path,
                id,
            )
        }
    }
}
