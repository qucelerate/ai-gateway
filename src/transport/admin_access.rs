//! Organization/access administration transport.
//!
//! Human-session-only handlers for teams, memberships, and role bindings under
//! `/api/admin/v1/organizations/{org}`. The org/team scope is always derived
//! from the resolved path resource, never from a client body. Every mutation
//! requires an `Idempotency-Key`; conditional mutations require `If-Match`.
//!
//! Errors follow the shared admin contract: `application/problem+json` with a
//! safe detail, an `instance` path, and a `request_id` that is echoed from
//! (or generated for) the `x-request-id` header and fed into every audit and
//! idempotency reference. Deletes return 204 No Content (200 `{deleted:true}`
//! on an idempotent replay) and ETags use the `"v<version>"` format, matching
//! the service-identity lane.
//!
//! The router is a standalone `Router` so the integration lane can merge it
//! under the application's admin registry.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_TYPE, ETAG},
    },
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    application::audit::{AuditApplicationService, AuditFilterInput, AuditServiceError},
    domain::{
        admin::{AdminIdempotencyKey, AdminPermission, AdminRole, AdminScope, authorize},
        audit::AuditTargetResource,
        auth::AuthCrypto,
        identity::{MembershipRole, OrganizationId, TeamId, UserAccountId},
        pagination::PageRequest,
        version::IfMatch,
    },
    persistence::{
        AuthStore,
        access::{AccessError, AccessStore, ListContext, MutationRequest, MutationResult},
    },
};

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const IF_MATCH_HEADER: &str = "if-match";
const REQUEST_ID_HEADER: &str = "x-request-id";

const ROUTE_TEAMS: &str = "/api/admin/v1/organizations/{org}/teams";
const ROUTE_TEAM: &str = "/api/admin/v1/organizations/{org}/teams/{team}";
const ROUTE_TEAM_MEMBERS: &str = "/api/admin/v1/organizations/{org}/teams/{team}/members";
const ROUTE_TEAM_MEMBER: &str = "/api/admin/v1/organizations/{org}/teams/{team}/members/{user}";
const ROUTE_MEMBERS: &str = "/api/admin/v1/organizations/{org}/members";
const ROUTE_MEMBER: &str = "/api/admin/v1/organizations/{org}/members/{user}";
const ROUTE_ROLE_BINDINGS: &str = "/api/admin/v1/organizations/{org}/role-bindings";
const ROUTE_ROLE_BINDING: &str = "/api/admin/v1/organizations/{org}/role-bindings/{binding}";
const ROUTE_AUDIT_EVENTS: &str = "/api/admin/v1/organizations/{org}/audit-events";
const ROUTE_AUDIT_SUMMARY: &str = "/api/admin/v1/organizations/{org}/audit-events/summary";

/// Handler state: the access store (persistence + coordinator) plus the
/// session store and crypto required for human-session authentication.
#[derive(Clone)]
pub struct AdminAccessState {
    access: AccessStore,
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    audit: Option<AuditApplicationService>,
}

impl AdminAccessState {
    pub fn new(access: AccessStore, auth: AuthStore, crypto: Option<AuthCrypto>) -> Self {
        let audit = AuditApplicationService::new(
            access.admin().clone(),
            access.admin().audit_store(),
            crypto.clone(),
        );
        Self {
            access,
            auth,
            crypto,
            audit: Some(audit),
        }
    }

    pub fn new_with_audit(
        access: AccessStore,
        auth: AuthStore,
        crypto: Option<AuthCrypto>,
        audit: AuditApplicationService,
    ) -> Self {
        Self {
            access,
            auth,
            crypto,
            audit: Some(audit),
        }
    }
}

// ---------------------------------------------------------------------------
// Request/response DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTeamRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTeamRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMembershipRequest {
    pub user_id: Uuid,
    pub role: MembershipRole,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMembershipRequest {
    pub role: Option<MembershipRole>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRoleBindingRequest {
    pub user_id: Uuid,
    pub role: AdminRole,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRoleBindingRequest {
    pub role: AdminRole,
}

/// Public entrypoint: build the merged organization/access router. The
/// integration lane registers this under `/api/admin/v1`.
pub fn admin_access_router(state: AdminAccessState) -> Router {
    Router::new()
        .route(ROUTE_TEAMS, get(list_teams).post(create_team))
        .route(
            ROUTE_TEAM,
            get(get_team).patch(patch_team).delete(delete_team),
        )
        .route(
            ROUTE_TEAM_MEMBERS,
            get(list_team_members).post(create_team_member),
        )
        .route(
            ROUTE_TEAM_MEMBER,
            patch(patch_team_member).delete(delete_team_member),
        )
        .route(ROUTE_MEMBERS, get(list_org_members).post(create_org_member))
        .route(
            ROUTE_MEMBER,
            patch(patch_org_member).delete(delete_org_member),
        )
        .route(
            ROUTE_ROLE_BINDINGS,
            get(list_role_bindings).post(create_role_binding),
        )
        .route(
            ROUTE_ROLE_BINDING,
            patch(patch_role_binding).delete(delete_role_binding),
        )
        .route(ROUTE_AUDIT_EVENTS, get(list_audit_events))
        .route(ROUTE_AUDIT_SUMMARY, get(summarize_audit_events))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors (shared admin contract: application/problem+json + request id)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum HandlerError {
    Access {
        error: AccessError,
        path: &'static str,
        id: String,
    },
    Http {
        status: StatusCode,
        code: &'static str,
        detail: &'static str,
        path: &'static str,
        id: String,
    },
}

impl HandlerError {
    fn access(error: AccessError, path: &'static str, id: &str) -> Self {
        Self::Access {
            error,
            path,
            id: id.to_owned(),
        }
    }

    fn http(
        status: StatusCode,
        code: &'static str,
        detail: &'static str,
        path: &'static str,
        id: &str,
    ) -> Self {
        Self::Http {
            status,
            code,
            detail,
            path,
            id: id.to_owned(),
        }
    }
}

impl From<AccessError> for HandlerError {
    fn from(error: AccessError) -> Self {
        // Fallback without request context; handlers normally use
        // `HandlerError::access` with the request id.
        Self::Access {
            error,
            path: "unknown",
            id: String::new(),
        }
    }
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        match self {
            Self::Http {
                status,
                code,
                detail,
                path,
                id,
            } => problem(status, code, detail, path, &id),
            Self::Access { error, path, id } => access_error_response(error, path, &id),
        }
    }
}

fn access_error_response(error: AccessError, path: &'static str, id: &str) -> Response {
    use crate::persistence::AdminStoreError;
    match error {
        AccessError::Admin(AdminStoreError::Authorization(_)) | AccessError::Authorization(_) => {
            problem(
                StatusCode::FORBIDDEN,
                "forbidden",
                "administrative permission denied",
                path,
                id,
            )
        }
        AccessError::Admin(AdminStoreError::ResourceNotFound) | AccessError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        AccessError::Admin(AdminStoreError::VersionConflict(conflict)) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        AccessError::Admin(AdminStoreError::IdempotencyConflict) => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        AccessError::Admin(AdminStoreError::IdempotencyInProgress) => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        AccessError::Admin(AdminStoreError::Database(error)) => database_response(error, path, id),
        AccessError::Admin(AdminStoreError::InvalidRole) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_stored_role",
            "stored administrative role is invalid",
            path,
            id,
        ),
        AccessError::Admin(AdminStoreError::SafeSerialization) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "safe_serialization",
            "audit or replay metadata could not be serialized",
            path,
            id,
        ),
        AccessError::Admin(AdminStoreError::InvalidAuditActor) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_audit_actor",
            "audit actor is invalid",
            path,
            id,
        ),
        AccessError::Database(error) => database_response(error, path, id),
        AccessError::LastOwner => problem(
            StatusCode::CONFLICT,
            "last_owner_required",
            "the last active organization owner cannot be removed",
            path,
            id,
        ),
        AccessError::EscalationDenied => problem(
            StatusCode::FORBIDDEN,
            "escalation_denied",
            "the requested role exceeds the actor's grant ceiling",
            path,
            id,
        ),
        AccessError::InvalidRequest(detail) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            detail,
            path,
            id,
        ),
        AccessError::Pagination(error) => problem(
            StatusCode::BAD_REQUEST,
            error.problem_code(),
            error.to_string(),
            path,
            id,
        ),
        AccessError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "cursor signing is unavailable without session/auth crypto",
            path,
            id,
        ),
    }
}

fn database_response(error: sqlx::Error, path: &'static str, id: &str) -> Response {
    if let Some(database_error) = error.as_database_error() {
        match database_error.code().as_deref() {
            // unique / foreign key / check violations are client conflicts.
            Some("23505") | Some("23503") | Some("23514") => problem(
                StatusCode::CONFLICT,
                "conflict",
                "the request conflicts with existing resource state",
                path,
                id,
            ),
            _ => problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "database is unavailable",
                path,
                id,
            ),
        }
    } else {
        problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "database is unavailable",
            path,
            id,
        )
    }
}

#[derive(Debug, Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    kind: String,
    code: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    instance: String,
    request_id: String,
}

fn problem(
    status: StatusCode,
    code: &'static str,
    detail: impl Into<String>,
    path: &str,
    id: &str,
) -> Response {
    let body = ProblemBody {
        kind: format!("https://gateway.invalid/problems/{code}"),
        code,
        title: status.canonical_reason().unwrap_or("Request failed"),
        status: status.as_u16(),
        detail: detail.into(),
        instance: path.to_owned(),
        request_id: id.to_owned(),
    };
    let mut response = (status, Json(body)).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(id).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    response
}

// ---------------------------------------------------------------------------
// Shared handler helpers
// ---------------------------------------------------------------------------

/// Echo a client `x-request-id` when it is a valid non-nil UUID, otherwise
/// generate one. The same id is returned in problem bodies/headers and fed into
/// every audit/idempotency reference.
fn request_context(headers: &HeaderMap) -> (String, Uuid) {
    let candidate = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128 && !value.trim().is_empty());
    match candidate.and_then(|value| Uuid::parse_str(value).ok()) {
        // A nil request id would violate the audit `request_id` check; it is
        // regenerated rather than persisted.
        Some(uuid) if !uuid.is_nil() => (uuid.to_string(), uuid),
        _ => {
            let uuid = Uuid::new_v4();
            (uuid.to_string(), uuid)
        }
    }
}

async fn actor(
    state: &AdminAccessState,
    headers: &HeaderMap,
    path: &'static str,
    id: &str,
) -> Result<UserAccountId, HandlerError> {
    match crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers)
        .await
    {
        Ok(Some(user_id)) => Ok(user_id),
        Ok(None) => Err(HandlerError::http(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "a session or an agent token is required",
            path,
            id,
        )),
        Err(_) => Err(HandlerError::http(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_store_unavailable",
            "authentication service unavailable",
            path,
            id,
        )),
    }
}

async fn authorize_read(
    state: &AdminAccessState,
    actor: UserAccountId,
    organization_id: OrganizationId,
    permission: AdminPermission,
    scope: AdminScope,
    path: &'static str,
    id: &str,
) -> Result<(), HandlerError> {
    let context = state
        .access
        .authorization_context(actor, organization_id)
        .await
        .map_err(|error| HandlerError::access(error, path, id))?;
    authorize(&context, permission, scope).map_err(|_| {
        HandlerError::http(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        )
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    page_size: Option<u32>,
    page_token: Option<String>,
    actor: Option<String>,
    action: Option<String>,
    target_type: Option<String>,
    target_id: Option<String>,
    outcome: Option<String>,
    actor_type: Option<String>,
    q: Option<String>,
    occurred_after: Option<String>,
    occurred_before: Option<String>,
    exclude_views: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditSummaryQuery {
    group_by: Option<String>,
    actor: Option<String>,
    action: Option<String>,
    target_type: Option<String>,
    target_id: Option<String>,
    outcome: Option<String>,
    actor_type: Option<String>,
    q: Option<String>,
    occurred_after: Option<String>,
    occurred_before: Option<String>,
    exclude_views: Option<bool>,
}

async fn summarize_audit_events(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    query: Result<Query<AuditSummaryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, HandlerError> {
    let (request_id_string, request_id) = request_context(&headers);
    let path = ROUTE_AUDIT_SUMMARY;
    let Query(query) = query.map_err(|_| {
        HandlerError::http(
            StatusCode::BAD_REQUEST,
            "invalid_audit_query",
            "audit query invalid",
            path,
            &request_id_string,
        )
    })?;
    let actor = actor(&state, &headers, path, &request_id_string).await?;
    let service = state.audit.as_ref().ok_or_else(|| {
        HandlerError::http(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_unavailable",
            "audit service unavailable",
            path,
            &request_id_string,
        )
    })?;
    let summary = service
        .summarize(
            actor,
            OrganizationId(org),
            AuditFilterInput {
                actor: query.actor,
                action: query.action,
                target_type: query.target_type,
                target_id: query.target_id,
                outcome: query.outcome,
                actor_type: query.actor_type,
                q: query.q,
                occurred_after: query.occurred_after,
                occurred_before: query.occurred_before,
                exclude_views: query.exclude_views.unwrap_or(false),
            },
            query.group_by,
            request_id,
        )
        .await
        .map_err(|error| audit_handler_error(error, path, &request_id_string))?;
    Ok(Json(summary).into_response())
}

async fn list_audit_events(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    query: Result<Query<AuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, HandlerError> {
    let (request_id_string, request_id) = request_context(&headers);
    let path = ROUTE_AUDIT_EVENTS;
    let Query(query) = query.map_err(|_| {
        HandlerError::http(
            StatusCode::BAD_REQUEST,
            "invalid_audit_query",
            "audit query invalid",
            path,
            &request_id_string,
        )
    })?;
    let actor = actor(&state, &headers, path, &request_id_string).await?;
    let service = state.audit.as_ref().ok_or_else(|| {
        HandlerError::http(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_unavailable",
            "audit service unavailable",
            path,
            &request_id_string,
        )
    })?;
    let page = PageRequest {
        page_size: query
            .page_size
            .unwrap_or_else(|| PageRequest::default().page_size),
        page_token: query.page_token,
    };
    let page = service
        .list(
            actor,
            OrganizationId(org),
            AuditFilterInput {
                actor: query.actor,
                action: query.action,
                target_type: query.target_type,
                target_id: query.target_id,
                outcome: query.outcome,
                actor_type: query.actor_type,
                q: query.q,
                occurred_after: query.occurred_after,
                occurred_before: query.occurred_before,
                exclude_views: query.exclude_views.unwrap_or(false),
            },
            page,
            request_id,
        )
        .await
        .map_err(|error| audit_handler_error(error, path, &request_id_string))?;
    Ok(Json(page).into_response())
}

fn audit_handler_error(
    error: AuditServiceError,
    path: &'static str,
    request_id: &str,
) -> HandlerError {
    match error {
        AuditServiceError::Forbidden => HandlerError::http(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            request_id,
        ),
        AuditServiceError::InvalidRequest(_) | AuditServiceError::Pagination(_) => {
            HandlerError::http(
                StatusCode::BAD_REQUEST,
                "invalid_audit_query",
                "audit query is invalid",
                path,
                request_id,
            )
        }
        AuditServiceError::AuthenticationUnavailable => HandlerError::http(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "cursor signing unavailable",
            path,
            request_id,
        ),
        AuditServiceError::Store(_) | AuditServiceError::Admin(_) => HandlerError::http(
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "database unavailable",
            path,
            request_id,
        ),
    }
}

fn idempotency_key(
    headers: &HeaderMap,
    path: &'static str,
    id: &str,
) -> Result<AdminIdempotencyKey, HandlerError> {
    let Some(value) = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(HandlerError::http(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key header is required for mutations",
            path,
            id,
        ));
    };
    AdminIdempotencyKey::new(value).map_err(|_| {
        HandlerError::http(
            StatusCode::BAD_REQUEST,
            "idempotency_key_invalid",
            "Idempotency-Key header is invalid",
            path,
            id,
        )
    })
}

fn if_match(headers: &HeaderMap, path: &'static str, id: &str) -> Result<IfMatch, HandlerError> {
    let Some(value) = headers
        .get(IF_MATCH_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(HandlerError::http(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
            "If-Match is required for conditional updates",
            path,
            id,
        ));
    };
    IfMatch::parse(value).map_err(|_| {
        HandlerError::http(
            StatusCode::BAD_REQUEST,
            "if_match_invalid",
            "If-Match value is invalid",
            path,
            id,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn mutation_request(
    actor: UserAccountId,
    organization_id: OrganizationId,
    scope: AdminScope,
    permission: AdminPermission,
    route: String,
    method: &'static str,
    operation: &'static str,
    idempotency_key: AdminIdempotencyKey,
    target_id: Uuid,
    parent_id: Option<Uuid>,
    target: AuditTargetResource,
    audit_action: &'static str,
    if_match: IfMatch,
    request_id: Uuid,
    body: serde_json::Value,
) -> MutationRequest {
    MutationRequest {
        actor_id: actor,
        organization_id,
        scope,
        permission,
        route,
        method,
        operation,
        idempotency_key,
        target_id,
        parent_id,
        target,
        audit_action,
        if_match,
        request_id,
        body,
    }
}

fn mutation_response(
    success_status: StatusCode,
    result: MutationResult,
    status_word: &'static str,
) -> Response {
    let status = if result.replayed {
        StatusCode::OK
    } else {
        success_status
    };
    let status_word = if result.replayed {
        "replayed"
    } else {
        status_word
    };
    let mut response = (
        status,
        Json(serde_json::json!({
            "status": status_word,
            "resource_id": result.resource_id,
            "version": result.version,
            "replayed": result.replayed,
            "secret_unavailable_on_replay": result.secret_unavailable_on_replay,
        })),
    )
        .into_response();
    if let Some(version) = result.version {
        response.headers_mut().insert(ETAG, etag_header(version));
    }
    response
}

/// Delete convention shared with the service-identity lane: 204 No Content, or
/// 200 `{deleted:true}` for an idempotent replay.
fn deletion_response(result: MutationResult) -> Response {
    if result.replayed {
        (StatusCode::OK, Json(serde_json::json!({ "deleted": true }))).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

/// Deterministic create target id derived from the scope, operation, and
/// idempotency key so retries reproduce the same resource and the coordinator
/// can safely replay the mutation.
fn derived_create_id(org: Uuid, operation: &str, key: &AdminIdempotencyKey) -> Uuid {
    use md5::{Digest, Md5};
    use std::fmt::Write as _;
    let mut hasher = Md5::new();
    hasher.update(format!("{org}|{operation}|{}", key.as_str()).as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter() {
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Uuid::parse_str(&hex).expect("an md5 digest hex is a valid uuid")
}

fn etag_header(version: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"v{version}\"")).expect("etag is a valid header value")
}

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

async fn list_teams(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(page): Query<PageRequest>,
) -> Result<Response, HandlerError> {
    let (id, _) = request_context(&headers);
    let path = ROUTE_TEAMS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    authorize_read(
        &state,
        actor,
        org,
        AdminPermission::TeamsList,
        AdminScope::Organization(org),
        path,
        &id,
    )
    .await?;
    let ctx = ListContext {
        actor_id: actor,
        scope: AdminScope::Organization(org),
        collection: "teams",
        filters: format!("org={}", org.0),
        order: "name,id",
    };
    let page = state
        .access
        .list_teams(&ctx, page)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(Json(page).into_response())
}

async fn create_team(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(body): Json<CreateTeamRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TEAMS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    // Create targets are derived from (org, operation, idempotency key) so a
    // retry reproduces the same resource id and the coordinator can replay.
    let team_id = derived_create_id(org.0, "team.create", &idempotency_key);
    let name = body.name.clone();
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::TeamMutate,
        format!("/api/admin/v1/organizations/{}/teams", org.0),
        "POST",
        "team.create",
        idempotency_key,
        team_id,
        None,
        AuditTargetResource::Team,
        "team.created",
        IfMatch::Any,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "target_id": team_id,
            "name": name,
        }),
    );
    let result = state
        .access
        .create_team(request, &name)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::CREATED, result, "created"))
}

async fn get_team(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team)): Path<(Uuid, Uuid)>,
) -> Result<Response, HandlerError> {
    let (id, _) = request_context(&headers);
    let path = ROUTE_TEAM;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    authorize_read(
        &state,
        actor,
        org,
        AdminPermission::TeamRead,
        AdminScope::Team {
            organization_id: org,
            team_id: team,
        },
        path,
        &id,
    )
    .await?;
    let view = state
        .access
        .get_team(org, team)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    let version = view.version;
    let mut response = Json(view).into_response();
    response.headers_mut().insert(ETAG, etag_header(version));
    Ok(response)
}

async fn patch_team(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateTeamRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TEAM;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let name = body.name.clone();
    // Team lifecycle (rename) is org-level so no audit row pins the team and a
    // subsequent delete is not blocked by the audit team FK.
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::TeamMutate,
        format!("/api/admin/v1/organizations/{}/teams/{}", org.0, team.0),
        "PATCH",
        "team.update",
        idempotency_key,
        team.0,
        None,
        AuditTargetResource::Team,
        "team.updated",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "team_id": team,
            "target_id": team,
            "name": name,
        }),
    );
    let result = state
        .access
        .update_team(request, &name)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::OK, result, "updated"))
}

async fn delete_team(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team)): Path<(Uuid, Uuid)>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TEAM;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    // Team deletion is an org-level destructive action: scope at the org so
    // the audit row does not reference the team it is about to delete (the
    // audit team FK would otherwise block the removal).
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::TeamMutate,
        format!("/api/admin/v1/organizations/{}/teams/{}", org.0, team.0),
        "DELETE",
        "team.delete",
        idempotency_key,
        team.0,
        None,
        AuditTargetResource::Team,
        "team.deleted",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "team_id": team,
            "target_id": team,
        }),
    );
    let result = state
        .access
        .delete_team(request)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(deletion_response(result))
}

// ---------------------------------------------------------------------------
// Memberships (org scope)
// ---------------------------------------------------------------------------

async fn list_org_members(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(page): Query<PageRequest>,
) -> Result<Response, HandlerError> {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MEMBERS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    authorize_read(
        &state,
        actor,
        org,
        AdminPermission::MembersList,
        AdminScope::Organization(org),
        path,
        &id,
    )
    .await?;
    let ctx = ListContext {
        actor_id: actor,
        scope: AdminScope::Organization(org),
        collection: "members",
        filters: format!("org={}", org.0),
        order: "user_id",
    };
    let page = state
        .access
        .list_org_members(&ctx, page)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(Json(page).into_response())
}

async fn create_org_member(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(body): Json<CreateMembershipRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MEMBERS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let user_id = body.user_id;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::MemberMutate,
        format!("/api/admin/v1/organizations/{}/members", org.0),
        "POST",
        "membership.create",
        idempotency_key,
        user_id,
        None,
        AuditTargetResource::Membership,
        "membership.created",
        IfMatch::Any,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "target_id": user_id,
            "user_id": user_id,
            "role": body.role,
        }),
    );
    let result = state
        .access
        .create_org_member(request, body.role)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::CREATED, result, "created"))
}

async fn patch_org_member(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, user)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateMembershipRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MEMBER;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::MemberMutate,
        format!("/api/admin/v1/organizations/{}/members/{}", org.0, user),
        "PATCH",
        "membership.update",
        idempotency_key,
        user,
        None,
        AuditTargetResource::Membership,
        "membership.updated",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "user_id": user,
            "target_id": user,
            "role": body.role,
            "enabled": body.enabled,
        }),
    );
    let result = state
        .access
        .update_org_member(request, body.role, body.enabled)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::OK, result, "updated"))
}

async fn delete_org_member(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, user)): Path<(Uuid, Uuid)>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MEMBER;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::MemberMutate,
        format!("/api/admin/v1/organizations/{}/members/{}", org.0, user),
        "DELETE",
        "membership.delete",
        idempotency_key,
        user,
        None,
        AuditTargetResource::Membership,
        "membership.deleted",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "user_id": user,
            "target_id": user,
        }),
    );
    let result = state
        .access
        .delete_org_member(request)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(deletion_response(result))
}

// ---------------------------------------------------------------------------
// Memberships (team scope)
// ---------------------------------------------------------------------------

async fn list_team_members(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team)): Path<(Uuid, Uuid)>,
    Query(page): Query<PageRequest>,
) -> Result<Response, HandlerError> {
    let (id, _) = request_context(&headers);
    let path = ROUTE_TEAM_MEMBERS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    let scope = AdminScope::Team {
        organization_id: org,
        team_id: team,
    };
    authorize_read(
        &state,
        actor,
        org,
        AdminPermission::MembersList,
        scope,
        path,
        &id,
    )
    .await?;
    state
        .access
        .get_team(org, team)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?; // 404 for a missing team
    let ctx = ListContext {
        actor_id: actor,
        scope,
        collection: "members",
        filters: format!("org={},team={}", org.0, team.0),
        order: "user_id",
    };
    let page = state
        .access
        .list_team_members(&ctx, page)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(Json(page).into_response())
}

async fn create_team_member(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team)): Path<(Uuid, Uuid)>,
    Json(body): Json<CreateMembershipRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TEAM_MEMBERS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let user_id = body.user_id;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Team {
            organization_id: org,
            team_id: team,
        },
        AdminPermission::MemberMutate,
        format!(
            "/api/admin/v1/organizations/{}/teams/{}/members",
            org.0, team.0
        ),
        "POST",
        "team_membership.create",
        idempotency_key,
        user_id,
        None,
        AuditTargetResource::Membership,
        "membership.created",
        IfMatch::Any,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "team_id": team,
            "target_id": user_id,
            "user_id": user_id,
            "role": body.role,
        }),
    );
    let result = state
        .access
        .create_team_member(request, body.role)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::CREATED, result, "created"))
}

async fn patch_team_member(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team, user)): Path<(Uuid, Uuid, Uuid)>,
    Json(body): Json<UpdateMembershipRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TEAM_MEMBER;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Team {
            organization_id: org,
            team_id: team,
        },
        AdminPermission::MemberMutate,
        format!(
            "/api/admin/v1/organizations/{}/teams/{}/members/{}",
            org.0, team.0, user
        ),
        "PATCH",
        "team_membership.update",
        idempotency_key,
        user,
        None,
        AuditTargetResource::Membership,
        "membership.updated",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "team_id": team,
            "user_id": user,
            "target_id": user,
            "role": body.role,
            "enabled": body.enabled,
        }),
    );
    let result = state
        .access
        .update_team_member(request, body.role, body.enabled)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::OK, result, "updated"))
}

async fn delete_team_member(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, team, user)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TEAM_MEMBER;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let team = TeamId(team);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Team {
            organization_id: org,
            team_id: team,
        },
        AdminPermission::MemberMutate,
        format!(
            "/api/admin/v1/organizations/{}/teams/{}/members/{}",
            org.0, team.0, user
        ),
        "DELETE",
        "team_membership.delete",
        idempotency_key,
        user,
        None,
        AuditTargetResource::Membership,
        "membership.deleted",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "team_id": team,
            "user_id": user,
            "target_id": user,
        }),
    );
    let result = state
        .access
        .delete_team_member(request)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(deletion_response(result))
}

// ---------------------------------------------------------------------------
// Role bindings
// ---------------------------------------------------------------------------

async fn list_role_bindings(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(page): Query<PageRequest>,
) -> Result<Response, HandlerError> {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ROLE_BINDINGS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    authorize_read(
        &state,
        actor,
        org,
        AdminPermission::RoleBindingsList,
        AdminScope::Organization(org),
        path,
        &id,
    )
    .await?;
    let ctx = ListContext {
        actor_id: actor,
        scope: AdminScope::Organization(org),
        collection: "role_bindings",
        filters: format!("org={}", org.0),
        order: "user_id,id",
    };
    let page = state
        .access
        .list_role_bindings(&ctx, page)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(Json(page).into_response())
}

async fn create_role_binding(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(body): Json<CreateRoleBindingRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_ROLE_BINDINGS;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let binding_id = derived_create_id(org.0, "role_binding.create", &idempotency_key);
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::RoleBindingMutate,
        format!("/api/admin/v1/organizations/{}/role-bindings", org.0),
        "POST",
        "role_binding.create",
        idempotency_key,
        binding_id,
        Some(body.user_id),
        AuditTargetResource::RoleBinding,
        "role_binding.created",
        IfMatch::Any,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "target_id": binding_id,
            "user_id": body.user_id,
            "role": body.role,
        }),
    );
    let result = state
        .access
        .create_role_binding(request, body.user_id, body.role)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::CREATED, result, "created"))
}

async fn patch_role_binding(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, binding)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateRoleBindingRequest>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_ROLE_BINDING;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::RoleBindingMutate,
        format!(
            "/api/admin/v1/organizations/{}/role-bindings/{}",
            org.0, binding
        ),
        "PATCH",
        "role_binding.update",
        idempotency_key,
        binding,
        None,
        AuditTargetResource::RoleBinding,
        "role_binding.updated",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "target_id": binding,
            "role": body.role,
        }),
    );
    let result = state
        .access
        .update_role_binding(request, body.role)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(mutation_response(StatusCode::OK, result, "updated"))
}

async fn delete_role_binding(
    State(state): State<AdminAccessState>,
    headers: HeaderMap,
    Path((org, binding)): Path<(Uuid, Uuid)>,
) -> Result<Response, HandlerError> {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_ROLE_BINDING;
    let actor = actor(&state, &headers, path, &id).await?;
    let org = OrganizationId(org);
    let idempotency_key = idempotency_key(&headers, path, &id)?;
    let if_match = if_match(&headers, path, &id)?;
    let request = mutation_request(
        actor,
        org,
        AdminScope::Organization(org),
        AdminPermission::RoleBindingMutate,
        format!(
            "/api/admin/v1/organizations/{}/role-bindings/{}",
            org.0, binding
        ),
        "DELETE",
        "role_binding.revoke",
        idempotency_key,
        binding,
        None,
        AuditTargetResource::RoleBinding,
        "role_binding.revoked",
        if_match,
        request_id,
        serde_json::json!({
            "organization_id": org,
            "target_id": binding,
        }),
    );
    let result = state
        .access
        .revoke_role_binding(request)
        .await
        .map_err(|error| HandlerError::access(error, path, &id))?;
    Ok(deletion_response(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::auth::AuthCrypto;

    #[test]
    fn problem_bodies_use_the_shared_admin_contract() {
        let response = problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            ROUTE_TEAMS,
            "req-1",
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        assert_eq!(
            response
                .headers()
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("req-1")
        );
        let response = access_error_response(AccessError::LastOwner, ROUTE_TEAMS, "req-1");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let response = access_error_response(AccessError::EscalationDenied, ROUTE_TEAMS, "req-1");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn idempotency_and_if_match_headers_are_required_and_validated() {
        let empty = HeaderMap::new();
        let path = ROUTE_TEAMS;
        assert!(idempotency_key(&empty, path, "id").is_err());
        assert!(if_match(&empty, path, "id").is_err());
        let mut headers = HeaderMap::new();
        headers.insert(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_static("key-1"));
        headers.insert(IF_MATCH_HEADER, HeaderValue::from_static("\"3\""));
        assert!(idempotency_key(&headers, path, "id").is_ok());
        assert_eq!(
            if_match(&headers, path, "id").unwrap(),
            IfMatch::parse("\"3\"").unwrap()
        );
        headers.insert(IF_MATCH_HEADER, HeaderValue::from_static("not-a-version"));
        assert!(if_match(&headers, path, "id").is_err());
    }

    #[test]
    fn etag_header_uses_identities_version_format() {
        assert_eq!(etag_header(4).to_str().unwrap(), "\"v4\"");
    }

    #[test]
    fn request_context_echoes_valid_ids_and_regenerates_nil() {
        let mut headers = HeaderMap::new();
        let (id, _) = request_context(&headers);
        assert!(!id.is_empty());
        let uuid = Uuid::new_v4();
        headers.insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_str(&uuid.to_string()).unwrap(),
        );
        let (id, echoed) = request_context(&headers);
        assert_eq!(id, uuid.to_string());
        assert_eq!(echoed, uuid);
        headers.insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_static("00000000-0000-0000-0000-000000000000"),
        );
        let (id, regenerated) = request_context(&headers);
        assert_ne!(
            regenerated.to_string(),
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(id, regenerated.to_string());
    }

    fn crypto() -> AuthCrypto {
        AuthCrypto::new(b"access-route-session-hmac-key-32-bytes", &[11_u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn access_state_is_cloneable_for_router_merging() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://gateway_app:opaque@localhost/gateway")
            .unwrap();
        let state = AdminAccessState::new(
            AccessStore::new(pool.clone(), Some(crypto())),
            AuthStore::new(pool),
            Some(crypto()),
        );
        let _clone = state.clone();
        let _router = admin_access_router(state);
    }
}
