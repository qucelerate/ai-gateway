//! The deployment admin API: organizations as a whole, and the deployment
//! audit log. See `openapi/admin-deployment-v1.json`.
//!
//! Deployment admins (the account first-run setup made, and those the
//! operator grants with `gateway admin grant`) create, suspend, delete and
//! restore organizations. They see each organization's name, state and
//! member count, never its contents. Owners may ask for their
//! organization's deletion; a deployment admin carries it out.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    application::{AppState, invitations::InvitationLinks, organization_slug_ok},
    domain::{
        identity::{OrganizationId, UserAccountId},
        password::normalize_email,
    },
    persistence::deployment::{LifecycleError, LocalOwner, NewOrganization, OwnerInvitation},
    transport::{
        admin_identities::{problem, request_context},
        http::{authenticated_session, csrf_valid},
        local_accounts::{
            LINK_VALID_HOURS, hash, human_session, invalid, invalid_body, local_enabled, owner,
            reset_link, unauthenticated, unavailable,
        },
    },
};

pub const ROUTE_ORGANIZATIONS: &str = "/api/admin/v1/deployment/organizations";
pub const ROUTE_SUSPEND: &str = "/api/admin/v1/deployment/organizations/{organization_id}/suspend";
pub const ROUTE_UNSUSPEND: &str =
    "/api/admin/v1/deployment/organizations/{organization_id}/unsuspend";
pub const ROUTE_DELETE: &str = "/api/admin/v1/deployment/organizations/{organization_id}/delete";
pub const ROUTE_RESTORE: &str = "/api/admin/v1/deployment/organizations/{organization_id}/restore";
pub const ROUTE_AUDIT: &str = "/api/admin/v1/deployment/audit";
/// Self-serve creation, behind `GATEWAY_SELF_SERVE_ORGANIZATIONS`.
pub const ROUTE_SELF_SERVE: &str = "/api/admin/v1/organizations";
pub const ROUTE_DELETION_REQUEST: &str =
    "/api/admin/v1/organizations/{organization_id}/deletion-request";

/// How long an invitation for a new organization's first owner works.
const OWNER_INVITATION_DAYS: i64 = 7;

/// A deployment admin's session (a person, not an agent), with the CSRF
/// token for writes.
async fn deployment_admin(
    state: &AppState,
    headers: &HeaderMap,
    write: bool,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    if write && !csrf_valid(state, headers) {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "csrf_origin_or_token_required",
            "the request must come from the admin UI with its CSRF token",
            path,
            id,
        ));
    }
    let actor = match human_session(state, headers).await {
        Ok(Some(actor)) => actor,
        Ok(None) => return Err(unauthenticated(path, id)),
        Err(()) => return Err(unavailable(path, id)),
    };
    match state.deployment().is_admin(actor).await {
        Ok(true) => Ok(actor),
        Ok(false) => Err(problem(
            StatusCode::FORBIDDEN,
            "deployment_admin_required",
            "only the deployment's admins may do this",
            path,
            id,
        )),
        Err(_) => Err(unavailable(path, id)),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    deleted: bool,
}

/// `GET /deployment/organizations[?deleted=true]`.
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ORGANIZATIONS;
    if let Err(response) = deployment_admin(&state, &headers, false, path, &id).await {
        return response;
    }
    match state.deployment().organizations(query.deleted).await {
        Ok(items) => Json(serde_json::json!({
            "items": items,
            "self_serve": state.local_accounts_config().self_serve_organizations,
        }))
        .into_response(),
        Err(_) => unavailable(path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    name: String,
    slug: String,
    #[serde(default)]
    owner: Option<OwnerBody>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerBody {
    email: String,
    /// `invitation`: an invitation link (accepted with a password or an
    /// identity provider); `local_account`: a local account now, with a
    /// set-password link when it is new.
    method: OwnerMethod,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OwnerMethod {
    Invitation,
    LocalAccount,
}

/// `POST /deployment/organizations`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ORGANIZATIONS;
    let actor = match deployment_admin(&state, &headers, true, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    let (name, slug) = match name_and_slug(&body.name, &body.slug, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let owner = match body.owner {
        Some(owner) => match normalize_email(&owner.email) {
            Some(email) => Some((email, owner.method)),
            None => return invalid(path, &id, "the owner's email address is not valid"),
        },
        None => None,
    };
    let origin = state.oidc().origin.clone().unwrap_or_default();
    // Links need an origin; a local account needs local accounts.
    let links = match &owner {
        Some((_, OwnerMethod::Invitation)) => {
            match InvitationLinks::from_config_or_local(state.oidc(), local_enabled(&state)) {
                Ok(links) => Some(links),
                Err(_) => {
                    return problem(
                        StatusCode::CONFLICT,
                        "invitations_unavailable",
                        "invitations need GATEWAY_ORIGIN and a way to sign in (OIDC or local accounts)",
                        path,
                        &id,
                    );
                }
            }
        }
        Some((_, OwnerMethod::LocalAccount)) if !local_enabled(&state) => {
            return problem(
                StatusCode::CONFLICT,
                "local_accounts_disabled",
                "local accounts are turned off on this gateway; invite the owner instead",
                path,
                &id,
            );
        }
        _ => None,
    };
    let unusable = match &owner {
        Some((_, OwnerMethod::LocalAccount)) => {
            let random = Zeroizing::new(format!("{}{}", Uuid::new_v4(), Uuid::new_v4()));
            match hash(random).await {
                Ok(hash) => Some(hash),
                Err(()) => return unavailable(path, &id),
            }
        }
        _ => None,
    };
    let local_owner = match (&owner, &unusable) {
        (Some((email, OwnerMethod::LocalAccount)), Some(unusable_hash)) => {
            Some(LocalOwner::Email {
                email,
                unusable_hash,
            })
        }
        _ => None,
    };
    let set_password_valid_for = chrono::Duration::hours(LINK_VALID_HOURS);
    let invitation_valid_for = chrono::Duration::days(OWNER_INVITATION_DAYS);
    let owner_invitation = match (&owner, &links) {
        (Some((_, OwnerMethod::Invitation)), Some(links)) => Some(OwnerInvitation {
            issuer: links.issuer(),
            valid_for: invitation_valid_for,
        }),
        _ => None,
    };
    let created = match state
        .deployment()
        .create(
            Some(actor),
            NewOrganization {
                name: &name,
                slug: &slug,
                local_owner,
                set_password_valid_for,
                owner_user: None,
                owner_invitation,
                action: "organization.created",
            },
        )
        .await
    {
        Ok(created) => created,
        Err(error) => return lifecycle_problem(error, path, &id),
    };
    let email = owner.as_ref().map(|(email, _)| email.as_str());
    let owner_link = match (&created.invitation, &created.set_password, &links) {
        (Some(token), _, Some(links)) => serde_json::json!({
            "kind": "invitation",
            "email": email,
            "link": links.link(token),
            "expires_at": chrono::Utc::now() + invitation_valid_for,
        }),
        (None, Some(token), _) => serde_json::json!({
            "kind": "set_password",
            "email": email,
            "link": reset_link(&origin, token.as_str()),
            "expires_at": chrono::Utc::now() + set_password_valid_for,
        }),
        _ => serde_json::Value::Null,
    };
    let organization = match state
        .deployment()
        .organization(created.organization_id.0)
        .await
    {
        Ok(Some(organization)) => organization,
        _ => return unavailable(path, &id),
    };
    let mut response = (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "organization": organization,
            // Shown once: the person who made it sends it on.
            "owner_link": owner_link,
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn name_and_slug(
    name: &str,
    slug: &str,
    path: &str,
    id: &str,
) -> Result<(String, String), Response> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        return Err(invalid(
            path,
            id,
            "the organization's name must have 1 to 200 characters",
        ));
    }
    let slug = slug.trim();
    if !organization_slug_ok(slug) {
        return Err(invalid(
            path,
            id,
            "the short name must be 1 to 64 lowercase letters, digits or hyphens, not starting with a hyphen",
        ));
    }
    Ok((name.to_owned(), slug.to_owned()))
}

/// `POST /deployment/organizations/{id}/suspend`.
pub async fn suspend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization): Path<Uuid>,
) -> Response {
    transition(state, headers, organization, ROUTE_SUSPEND, true).await
}

/// `POST /deployment/organizations/{id}/unsuspend`.
pub async fn unsuspend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization): Path<Uuid>,
) -> Response {
    transition(state, headers, organization, ROUTE_UNSUSPEND, false).await
}

async fn transition(
    state: AppState,
    headers: HeaderMap,
    organization: Uuid,
    path: &str,
    suspend: bool,
) -> Response {
    let (id, _) = request_context(&headers);
    let actor = match deployment_admin(&state, &headers, true, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let result = if suspend {
        state.deployment().suspend(actor, organization).await
    } else {
        state.deployment().unsuspend(actor, organization).await
    };
    match result {
        Ok(()) => summary(&state, organization, path, &id).await,
        Err(error) => lifecycle_problem(error, path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteBody {
    /// The organization's slug, typed to confirm.
    confirm_slug: String,
}

/// `POST /deployment/organizations/{id}/delete`.
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization): Path<Uuid>,
    body: Result<Json<DeleteBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DELETE;
    let actor = match deployment_admin(&state, &headers, true, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    match state
        .deployment()
        .delete(actor, organization, body.confirm_slug.trim())
        .await
    {
        Ok(()) => summary(&state, organization, path, &id).await,
        Err(error) => lifecycle_problem(error, path, &id),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RestoreBody {
    /// A new slug, when a live organization took the old one.
    #[serde(default)]
    slug: Option<String>,
}

/// `POST /deployment/organizations/{id}/restore`: back to suspended.
pub async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization): Path<Uuid>,
    body: Result<Json<RestoreBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_RESTORE;
    let actor = match deployment_admin(&state, &headers, true, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(JsonRejection::MissingJsonContentType(_)) => RestoreBody::default(),
        Err(_) => return invalid_body(path, &id),
    };
    let slug = body.slug.as_deref().map(str::trim);
    if slug.is_some_and(|slug| !organization_slug_ok(slug)) {
        return invalid(
            path,
            &id,
            "the short name must be 1 to 64 lowercase letters, digits or hyphens, not starting with a hyphen",
        );
    }
    match state.deployment().restore(actor, organization, slug).await {
        Ok(_) => summary(&state, organization, path, &id).await,
        Err(error) => lifecycle_problem(error, path, &id),
    }
}

async fn summary(state: &AppState, organization: Uuid, path: &str, id: &str) -> Response {
    match state.deployment().organization(organization).await {
        Ok(Some(summary)) => Json(summary).into_response(),
        Ok(None) => lifecycle_problem(LifecycleError::NotFound, path, id),
        Err(_) => unavailable(path, id),
    }
}

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /deployment/audit`: newest first.
pub async fn audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_AUDIT;
    if let Err(response) = deployment_admin(&state, &headers, false, path, &id).await {
        return response;
    }
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    match state.deployment().events(limit, None).await {
        Ok(items) => Json(serde_json::json!({ "items": items })).into_response(),
        Err(_) => unavailable(path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfServeBody {
    name: String,
    slug: String,
}

/// `POST /organizations`: with `GATEWAY_SELF_SERVE_ORGANIZATIONS` on, any
/// signed-in person creates an organization and owns it.
pub async fn self_serve(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SelfServeBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SELF_SERVE;
    if !state.local_accounts_config().self_serve_organizations {
        return problem(
            StatusCode::FORBIDDEN,
            "self_serve_organizations_disabled",
            "this gateway's admins create organizations; ask one",
            path,
            &id,
        );
    }
    if !csrf_valid(&state, &headers) {
        return problem(
            StatusCode::FORBIDDEN,
            "csrf_origin_or_token_required",
            "the request must come from the admin UI with its CSRF token",
            path,
            &id,
        );
    }
    if crate::transport::admin_auth::agent_token(&headers).is_some() {
        return unauthenticated(path, &id);
    }
    let actor = match authenticated_session(&state, &headers).await {
        Ok(Some((actor, None))) => actor,
        // A session from one organization's own provider acts only there:
        // it could neither list nor select the organization it made.
        Ok(Some((_, Some(_)))) => {
            return problem(
                StatusCode::FORBIDDEN,
                "deployment_sign_in_required",
                "you signed in through one organization's own identity provider; to create an organization, sign in with the gateway's own sign-in (a local account or its identity provider)",
                path,
                &id,
            );
        }
        Ok(None) => return unauthenticated(path, &id),
        Err(_) => return unavailable(path, &id),
    };
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    let (name, slug) = match name_and_slug(&body.name, &body.slug, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state
        .deployment()
        .create(
            Some(actor),
            NewOrganization {
                name: &name,
                slug: &slug,
                local_owner: None,
                set_password_valid_for: chrono::Duration::hours(LINK_VALID_HOURS),
                owner_user: Some(actor),
                owner_invitation: None,
                action: "organization.self_served",
            },
        )
        .await
    {
        Ok(created) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": created.organization_id.0,
                "slug": slug,
                "name": name,
            })),
        )
            .into_response(),
        Err(error) => lifecycle_problem(error, path, &id),
    }
}

/// `GET /organizations/{org}/deletion-request`: any member.
pub async fn get_deletion_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DELETION_REQUEST;
    let user = match human_session(&state, &headers).await {
        Ok(Some(user)) => user,
        Ok(None) => return unauthenticated(path, &id),
        Err(()) => return unavailable(path, &id),
    };
    match state
        .identity()
        .human_principal(user, organization_id, None)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return lifecycle_problem(LifecycleError::NotFound, path, &id),
        Err(_) => return unavailable(path, &id),
    }
    deletion_response(&state, organization_id, path, &id).await
}

/// `PUT` (ask) or `DELETE` (withdraw) `/organizations/{org}/deletion-request`:
/// owners only.
pub async fn put_deletion_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    deletion_request(state, headers, organization_id, true).await
}

pub async fn delete_deletion_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    deletion_request(state, headers, organization_id, false).await
}

async fn deletion_request(
    state: AppState,
    headers: HeaderMap,
    organization_id: OrganizationId,
    request: bool,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DELETION_REQUEST;
    let actor = match owner(&state, &headers, organization_id, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .deployment()
        .request_deletion(actor, organization_id, request)
        .await
    {
        Ok(()) => deletion_response(&state, organization_id, path, &id).await,
        Err(error) => lifecycle_problem(error, path, &id),
    }
}

async fn deletion_response(
    state: &AppState,
    organization_id: OrganizationId,
    path: &str,
    id: &str,
) -> Response {
    match state.deployment().deletion_requested(organization_id).await {
        Ok(requested_at) => {
            Json(serde_json::json!({ "requested_at": requested_at })).into_response()
        }
        Err(_) => unavailable(path, id),
    }
}

fn lifecycle_problem(error: LifecycleError, path: &str, id: &str) -> Response {
    match error {
        LifecycleError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "organization_not_found",
            "no such organization",
            path,
            id,
        ),
        LifecycleError::SlugTaken => problem(
            StatusCode::CONFLICT,
            "slug_taken",
            "another organization has this short name; choose another",
            path,
            id,
        ),
        LifecycleError::WrongState("active") => problem(
            StatusCode::CONFLICT,
            "organization_active",
            "suspend the organization first",
            path,
            id,
        ),
        LifecycleError::WrongState("suspended") => problem(
            StatusCode::CONFLICT,
            "organization_suspended",
            "the organization is suspended",
            path,
            id,
        ),
        LifecycleError::WrongState(_) => problem(
            StatusCode::CONFLICT,
            "organization_deleted",
            "the organization is deleted; restore it first",
            path,
            id,
        ),
        LifecycleError::ConfirmationMismatch => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirmation_mismatch",
            "type the organization's short name exactly to confirm",
            path,
            id,
        ),
        LifecycleError::LastOrganization => problem(
            StatusCode::CONFLICT,
            "last_organization",
            "the deployment's last organization cannot be deleted",
            path,
            id,
        ),
        LifecycleError::OwnerUnavailable => problem(
            StatusCode::CONFLICT,
            "owner_unavailable",
            "the account with this email address is disabled",
            path,
            id,
        ),
        LifecycleError::Store(_) | LifecycleError::Grant => unavailable(path, id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-deployment-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_ORGANIZATIONS,
            ROUTE_SUSPEND,
            ROUTE_UNSUSPEND,
            ROUTE_DELETE,
            ROUTE_RESTORE,
            ROUTE_AUDIT,
            ROUTE_SELF_SERVE,
            ROUTE_DELETION_REQUEST,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
