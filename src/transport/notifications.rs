// HTTP transport for a member's notifications:
// `GET /api/admin/v1/organizations/{org}/notifications` (paged; filters
// `kind`, `severity`, `read`), `GET .../notifications/unread-count` and
// `POST .../notifications/read`.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Marking read changes only the caller's own read
// state and is naturally idempotent, so it takes neither `Idempotency-Key`
// nor `If-Match`. Problems share the admin lanes' `application/problem+json`
// shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

use crate::{
    application::notifications::{
        NotificationService, NotificationServiceError, ROUTE_NOTIFICATIONS,
        ROUTE_NOTIFICATIONS_READ, ROUTE_NOTIFICATIONS_UNREAD_COUNT,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        notifications::{MarkRead, NotificationFilter, NotificationKind, ReadFilter, Severity},
        pagination::{DEFAULT_PAGE_SIZE, PageRequest, SignedCursorCodec},
    },
    persistence::{admin::AdminStore, auth::AuthStore, notifications::NotificationStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
    },
};

#[derive(Clone)]
pub struct NotificationState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<NotificationService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: NotificationStore,
) -> Router {
    let service = Arc::new(NotificationService::new(
        admin,
        store,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(ROUTE_NOTIFICATIONS, get(list))
        .route(ROUTE_NOTIFICATIONS_UNREAD_COUNT, get(unread_count))
        .route(ROUTE_NOTIFICATIONS_READ, post(mark_read))
        .with_state(NotificationState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    page_size: Option<u32>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    read: Option<ReadFilter>,
}

async fn list(
    State(state): State<NotificationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_NOTIFICATIONS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(query)) = query else {
        return invalid_query("the query is not valid", path, &id);
    };
    let page = PageRequest {
        page_size: query.page_size.unwrap_or(DEFAULT_PAGE_SIZE),
        page_token: query.page_token.filter(|token| !token.is_empty()),
    };
    if let Err(error) = page.validate() {
        return pagination_problem(error, path, &id);
    }
    let kind = match query.kind.as_deref().filter(|kind| !kind.is_empty()) {
        Some(kind) => match NotificationKind::parse(kind) {
            Some(kind) => Some(kind),
            None => return invalid_query("unknown notification kind", path, &id),
        },
        None => None,
    };
    let severity = match query.severity.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => match Severity::parse(value) {
            Some(severity) => Some(severity),
            None => {
                return invalid_query("severity is info, warning or action", path, &id);
            }
        },
        None => None,
    };
    let filter = NotificationFilter {
        kind,
        severity,
        read: query.read.unwrap_or_default(),
    };
    match state
        .service
        .list(actor, organization_id, &filter, &page)
        .await
    {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => notification_problem(error, path, &id),
    }
}

async fn unread_count(
    State(state): State<NotificationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_NOTIFICATIONS_UNREAD_COUNT;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.unread_count(actor, organization_id).await {
        Ok(count) => (StatusCode::OK, Json(count)).into_response(),
        Err(error) => notification_problem(error, path, &id),
    }
}

async fn mark_read(
    State(state): State<NotificationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<MarkRead>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_NOTIFICATIONS_READ;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .mark_read(actor, organization_id, input, chrono::Utc::now())
        .await
    {
        Ok(count) => (StatusCode::OK, Json(count)).into_response(),
        Err(error) => notification_problem(error, path, &id),
    }
}

async fn actor(
    state: &NotificationState,
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

fn invalid_query(detail: &str, path: &str, id: &str) -> Response {
    problem(StatusCode::BAD_REQUEST, "invalid_request", detail, path, id)
}

fn notification_problem(error: NotificationServiceError, path: &str, id: &str) -> Response {
    match error {
        NotificationServiceError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        NotificationServiceError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        NotificationServiceError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            error.to_string(),
            path,
            id,
        ),
        NotificationServiceError::Pagination(error) => pagination_problem(error, path, id),
        NotificationServiceError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        NotificationServiceError::Store(_) | NotificationServiceError::Admin(_) => {
            tracing::warn!(error = ?error, "notification lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "notification store unavailable",
                path,
                id,
            )
        }
    }
}
