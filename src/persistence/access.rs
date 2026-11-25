//! Persistence and service layer for the organization/access administration
//! API (`/api/admin/v1/organizations/{org}`).
//!
//! All mutations run through the shared admin mutation coordinator so RBAC,
//! idempotency, optimistic concurrency, version seeding, and typed audit are
//! applied by the same code used by every other administrative lane. Reads are
//! bounded cursor pages bound to the authenticated actor and the path-derived
//! scope; no client-supplied context is ever trusted.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    admin::{
        AdminAuthorizationContext, AdminIdempotencyKey, AdminOperation, AdminPermission, AdminRole,
        AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    auth::AuthCrypto,
    identity::{MembershipRole, OrganizationId, TeamId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    version::IfMatch,
};
use crate::persistence::admin::{
    AdminIdempotencyRecord, AdminMutationRequest, AdminStore, AdminStoreError, MutationStart,
};

/// Errors surfaced by the access store. HTTP mapping is owned by the transport
/// module; this type stays persistence-shaped.
#[derive(Debug, Error)]
pub enum AccessError {
    #[error("admin coordinator failed: {0}")]
    Admin(#[from] AdminStoreError),
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("resource was not found")]
    NotFound,
    #[error("the last active organization owner cannot be removed")]
    LastOwner,
    #[error("the requested role exceeds the actor's grant ceiling")]
    EscalationDenied,
    #[error("request is invalid: {0}")]
    InvalidRequest(&'static str),
    #[error("pagination failed: {0}")]
    Pagination(#[from] PaginationError),
    #[error("authorization failed: {0}")]
    Authorization(#[from] crate::domain::admin::AuthorizationError),
    #[error("cursor signing is unavailable without session/auth crypto")]
    AuthenticationUnavailable,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamView {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub version: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MembershipView {
    pub organization_id: Uuid,
    pub team_id: Option<Uuid>,
    pub user_id: Uuid,
    /// The person's name and email as their identity provider last gave
    /// them at sign-in (or as entered for a local account); null until then.
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub role: String,
    pub enabled: bool,
    pub version: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoleBindingView {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub user_id: Uuid,
    /// As on [`MembershipView`].
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub team_id: Option<Uuid>,
    pub role: String,
    pub enabled: bool,
    pub version: u64,
}

/// Context shared by bounded-cursor list operations. The actor and scope are
/// always resolved server-side (session user + path resource), never from a
/// client body.
#[derive(Clone, Debug)]
pub struct ListContext {
    pub actor_id: UserAccountId,
    pub scope: AdminScope,
    pub collection: &'static str,
    pub filters: String,
    pub order: &'static str,
}

/// Every field the coordinator needs to begin a mutation. `body` must carry
/// the scope/parent identifiers so idempotency fingerprints are tenant- and
/// parent-scoped.
#[derive(Clone, Debug)]
pub struct MutationRequest {
    pub actor_id: UserAccountId,
    pub organization_id: OrganizationId,
    pub scope: AdminScope,
    pub permission: AdminPermission,
    pub route: String,
    pub method: &'static str,
    pub operation: &'static str,
    pub idempotency_key: AdminIdempotencyKey,
    pub target_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub target: AuditTargetResource,
    pub audit_action: &'static str,
    pub if_match: IfMatch,
    pub request_id: Uuid,
    pub body: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MutationResult {
    pub resource_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    pub replayed: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub secret_unavailable_on_replay: bool,
}

/// Repository + coordinator facade for the organization/access API.
#[derive(Clone)]
pub struct AccessStore {
    pool: PgPool,
    admin: AdminStore,
    codec: Option<SignedCursorCodec>,
}

impl AccessStore {
    pub fn new(pool: PgPool, crypto: Option<AuthCrypto>) -> Self {
        Self {
            pool: pool.clone(),
            admin: AdminStore::new(pool),
            codec: crypto.map(SignedCursorCodec::new),
        }
    }

    pub fn admin(&self) -> &AdminStore {
        &self.admin
    }

    pub async fn authorization_context(
        &self,
        actor_id: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<AdminAuthorizationContext, AccessError> {
        Ok(self
            .admin
            .authorization_context(actor_id, organization_id)
            .await?)
    }

    // ------------------------------------------------------------------
    // Reads (bounded cursor lists + team detail)
    // ------------------------------------------------------------------

    pub async fn list_teams(
        &self,
        ctx: &ListContext,
        page: PageRequest,
    ) -> Result<Page<TeamView>, AccessError> {
        page.validate()?;
        let keyset = self.decoded_keyset(ctx, &page)?;
        let after_name = keyset.as_ref().and_then(|parts| parts.first().cloned());
        let after_id = keyset
            .as_ref()
            .and_then(|parts| parts.get(1))
            .and_then(|value| Uuid::parse_str(value).ok());
        if keyset.is_some() && after_id.is_none() {
            return Err(AccessError::Pagination(PaginationError::InvalidToken));
        }
        let limit = i64::from(page.page_size) + 1;
        let rows = sqlx::query_as::<_, TeamRow>(
            "SELECT t.id, t.organization_id, t.name,
                    COALESCE(v.version, 1)::bigint AS version
             FROM gateway.team t
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = t.organization_id
              AND v.resource_type = 'team'
              AND v.resource_id = t.id
             WHERE t.organization_id = $1
               AND ($2::text IS NULL OR (t.name, t.id) > ($2, $3))
             ORDER BY t.name, t.id
             LIMIT $4",
        )
        .bind(ctx.scope.organization_id().0)
        .bind(after_name)
        .bind(after_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page.page_size as usize;
        let items: Vec<TeamView> = rows
            .into_iter()
            .take(page.page_size as usize)
            .map(Into::into)
            .collect();
        let next_page_token = if has_more {
            let last = items.last().expect("page with rows has a last item");
            self.next_token(ctx, &[&last.name, &last.id.to_string()])?
        } else {
            None
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn get_team(
        &self,
        organization_id: OrganizationId,
        team_id: TeamId,
    ) -> Result<TeamView, AccessError> {
        let row = sqlx::query_as::<_, TeamRow>(
            "SELECT t.id, t.organization_id, t.name,
                    COALESCE(v.version, 1)::bigint AS version
             FROM gateway.team t
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = t.organization_id
              AND v.resource_type = 'team'
              AND v.resource_id = t.id
             WHERE t.organization_id = $1 AND t.id = $2",
        )
        .bind(organization_id.0)
        .bind(team_id.0)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccessError::NotFound)?;
        Ok(row.into())
    }

    pub async fn list_org_members(
        &self,
        ctx: &ListContext,
        page: PageRequest,
    ) -> Result<Page<MembershipView>, AccessError> {
        page.validate()?;
        let keyset = self.decoded_keyset(ctx, &page)?;
        let after_user = keyset
            .as_ref()
            .and_then(|parts| parts.first())
            .and_then(|value| Uuid::parse_str(value).ok());
        if keyset.is_some() && after_user.is_none() {
            return Err(AccessError::Pagination(PaginationError::InvalidToken));
        }
        let limit = i64::from(page.page_size) + 1;
        let rows = sqlx::query_as::<_, OrgMemberRow>(
            "SELECT om.user_id, om.role, om.enabled, u.display_name, u.email,
                    COALESCE(v.version, 1)::bigint AS version
             FROM gateway.organization_membership om
             JOIN gateway.user_account u ON u.id = om.user_id
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = om.organization_id
              AND v.resource_type = 'membership'
              AND v.resource_id = om.user_id
             WHERE om.organization_id = $1
               AND ($2::uuid IS NULL OR om.user_id > $2)
             ORDER BY om.user_id
             LIMIT $3",
        )
        .bind(ctx.scope.organization_id().0)
        .bind(after_user)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page.page_size as usize;
        let items: Vec<MembershipView> = rows
            .into_iter()
            .take(page.page_size as usize)
            .map(|row| MembershipView {
                organization_id: ctx.scope.organization_id().0,
                team_id: None,
                user_id: row.user_id,
                display_name: row.display_name,
                email: row.email,
                role: row.role,
                enabled: row.enabled,
                version: u64::try_from(row.version).unwrap_or(1),
            })
            .collect();
        let next_page_token = if has_more {
            let last = items.last().expect("page with rows has a last item");
            self.next_token(ctx, &[&last.user_id.to_string()])?
        } else {
            None
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn list_team_members(
        &self,
        ctx: &ListContext,
        page: PageRequest,
    ) -> Result<Page<MembershipView>, AccessError> {
        page.validate()?;
        let keyset = self.decoded_keyset(ctx, &page)?;
        let after_user = keyset
            .as_ref()
            .and_then(|parts| parts.first())
            .and_then(|value| Uuid::parse_str(value).ok());
        if keyset.is_some() && after_user.is_none() {
            return Err(AccessError::Pagination(PaginationError::InvalidToken));
        }
        let (organization_id, team_id) = match ctx.scope {
            AdminScope::Team {
                organization_id,
                team_id,
            } => (organization_id, team_id),
            AdminScope::Organization(_) => {
                return Err(AccessError::InvalidRequest("team scope required"));
            }
        };
        let limit = i64::from(page.page_size) + 1;
        let rows = sqlx::query_as::<_, TeamMemberRow>(
            "SELECT tm.user_id, tm.role, tm.enabled, u.display_name, u.email,
                    COALESCE(v.version, 1)::bigint AS version
             FROM gateway.team_membership tm
             JOIN gateway.user_account u ON u.id = tm.user_id
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = tm.organization_id
              AND v.resource_type = 'membership'
              AND v.resource_id = md5(tm.team_id::text || ':' || tm.user_id::text)::uuid
             WHERE tm.organization_id = $1 AND tm.team_id = $2
               AND ($3::uuid IS NULL OR tm.user_id > $3)
             ORDER BY tm.user_id
             LIMIT $4",
        )
        .bind(organization_id.0)
        .bind(team_id.0)
        .bind(after_user)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page.page_size as usize;
        let items: Vec<MembershipView> = rows
            .into_iter()
            .take(page.page_size as usize)
            .map(|row| MembershipView {
                organization_id: organization_id.0,
                team_id: Some(team_id.0),
                user_id: row.user_id,
                display_name: row.display_name,
                email: row.email,
                role: row.role,
                enabled: row.enabled,
                version: u64::try_from(row.version).unwrap_or(1),
            })
            .collect();
        let next_page_token = if has_more {
            let last = items.last().expect("page with rows has a last item");
            self.next_token(ctx, &[&last.user_id.to_string()])?
        } else {
            None
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn list_role_bindings(
        &self,
        ctx: &ListContext,
        page: PageRequest,
    ) -> Result<Page<RoleBindingView>, AccessError> {
        page.validate()?;
        let keyset = self.decoded_keyset(ctx, &page)?;
        let after_user = keyset
            .as_ref()
            .and_then(|parts| parts.first())
            .and_then(|value| Uuid::parse_str(value).ok());
        let after_id = keyset
            .as_ref()
            .and_then(|parts| parts.get(1))
            .and_then(|value| Uuid::parse_str(value).ok());
        if keyset.is_some() && (after_user.is_none() || after_id.is_none()) {
            return Err(AccessError::Pagination(PaginationError::InvalidToken));
        }
        let limit = i64::from(page.page_size) + 1;
        let rows = sqlx::query_as::<_, RoleBindingRow>(
            "SELECT b.id, b.organization_id, b.user_id, b.team_id, b.role, b.enabled,
                    u.display_name, u.email, COALESCE(v.version, 1)::bigint AS version
             FROM gateway.admin_role_binding b
             JOIN gateway.user_account u ON u.id = b.user_id
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = b.organization_id
              AND v.resource_type = 'role_binding'
              AND v.resource_id = b.id
             WHERE b.organization_id = $1
               AND ($2::uuid IS NULL OR (b.user_id, b.id) > ($2, $3))
             ORDER BY b.user_id, b.id
             LIMIT $4",
        )
        .bind(ctx.scope.organization_id().0)
        .bind(after_user)
        .bind(after_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page.page_size as usize;
        let items: Vec<RoleBindingView> = rows
            .into_iter()
            .take(page.page_size as usize)
            .map(Into::into)
            .collect();
        let next_page_token = if has_more {
            let last = items.last().expect("page with rows has a last item");
            self.next_token(ctx, &[&last.user_id.to_string(), &last.id.to_string()])?
        } else {
            None
        };
        Ok(Page::new(items, next_page_token))
    }

    // ------------------------------------------------------------------
    // Mutations
    // ------------------------------------------------------------------

    pub async fn create_team(
        &self,
        request: MutationRequest,
        name: &str,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                sqlx::query(
                    "INSERT INTO gateway.team (id, organization_id, name) VALUES ($1, $2, $3)",
                )
                .bind(target_id)
                .bind(organization_id.0)
                .bind(name)
                .execute(&mut **mutation.transaction())
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn update_team(
        &self,
        request: MutationRequest,
        name: &str,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                let changed = sqlx::query(
                    "UPDATE gateway.team SET name = $3 WHERE organization_id = $1 AND id = $2",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .bind(name)
                .execute(&mut **mutation.transaction())
                .await?
                .rows_affected();
                if changed != 1 {
                    return Err(AccessError::NotFound);
                }
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn delete_team(
        &self,
        request: MutationRequest,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                sqlx::query("DELETE FROM gateway.team WHERE organization_id = $1 AND id = $2")
                    .bind(organization_id.0)
                    .bind(target_id)
                    .execute(&mut **mutation.transaction())
                    .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn create_org_member(
        &self,
        request: MutationRequest,
        role: MembershipRole,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                enforce_org_membership_create_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    role,
                )
                .await?;
                sqlx::query(
                    "INSERT INTO gateway.organization_membership (organization_id, user_id, role)
                     VALUES ($1, $2, $3)",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .bind(role.as_str())
                .execute(&mut **mutation.transaction())
                .await?;
                upsert_derived_org_binding(
                    &mut mutation.transaction(),
                    organization_id,
                    UserAccountId(target_id),
                    role,
                    true,
                )
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn update_org_member(
        &self,
        request: MutationRequest,
        role: Option<MembershipRole>,
        enabled: Option<bool>,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        if role.is_none() && enabled.is_none() {
            return Err(AccessError::InvalidRequest("role or enabled is required"));
        }
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                let current = sqlx::query_as::<_, (String, bool)>(
                    "SELECT role, enabled FROM gateway.organization_membership
                     WHERE organization_id = $1 AND user_id = $2",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .fetch_optional(&mut **mutation.transaction())
                .await?
                .ok_or(AccessError::NotFound)?;
                let old_role = current
                    .0
                    .parse::<MembershipRole>()
                    .map_err(|_| AccessError::InvalidRequest("stored membership role"))?;
                let old_enabled = current.1;
                let new_role = role.unwrap_or(old_role);
                let new_enabled = enabled.unwrap_or(old_enabled);
                enforce_org_membership_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    UserAccountId(target_id),
                    new_role,
                    new_enabled,
                )
                .await?;
                assert_membership_not_last_owner(
                    &mut mutation.transaction(),
                    organization_id,
                    UserAccountId(target_id),
                    new_role,
                    new_enabled,
                )
                .await?;
                sqlx::query(
                    "UPDATE gateway.organization_membership SET role = $3, enabled = $4
                     WHERE organization_id = $1 AND user_id = $2",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .bind(new_role.as_str())
                .bind(new_enabled)
                .execute(&mut **mutation.transaction())
                .await?;
                upsert_derived_org_binding(
                    &mut mutation.transaction(),
                    organization_id,
                    UserAccountId(target_id),
                    new_role,
                    new_enabled,
                )
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn delete_org_member(
        &self,
        request: MutationRequest,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                enforce_org_membership_delete_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    UserAccountId(target_id),
                )
                .await?;
                assert_org_member_delete_safe(
                    &mut mutation.transaction(),
                    organization_id,
                    UserAccountId(target_id),
                )
                .await?;
                sqlx::query(
                    "DELETE FROM gateway.organization_membership
                     WHERE organization_id = $1 AND user_id = $2",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .execute(&mut **mutation.transaction())
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn create_team_member(
        &self,
        request: MutationRequest,
        role: MembershipRole,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let team_id = match request.scope {
            AdminScope::Team { team_id, .. } => team_id,
            AdminScope::Organization(_) => {
                return Err(AccessError::InvalidRequest("team scope required"));
            }
        };
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                enforce_team_membership_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    team_id,
                    UserAccountId(target_id),
                    role,
                    true,
                )
                .await?;
                sqlx::query(
                    "INSERT INTO gateway.team_membership (organization_id, team_id, user_id, role)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(organization_id.0)
                .bind(team_id.0)
                .bind(target_id)
                .bind(role.as_str())
                .execute(&mut **mutation.transaction())
                .await?;
                upsert_derived_team_binding(
                    &mut mutation.transaction(),
                    organization_id,
                    team_id,
                    UserAccountId(target_id),
                    role,
                    true,
                )
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn update_team_member(
        &self,
        request: MutationRequest,
        role: Option<MembershipRole>,
        enabled: Option<bool>,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let team_id = match request.scope {
            AdminScope::Team { team_id, .. } => team_id,
            AdminScope::Organization(_) => {
                return Err(AccessError::InvalidRequest("team scope required"));
            }
        };
        if role.is_none() && enabled.is_none() {
            return Err(AccessError::InvalidRequest("role or enabled is required"));
        }
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                let current = sqlx::query_as::<_, (String, bool)>(
                    "SELECT role, enabled FROM gateway.team_membership
                     WHERE organization_id = $1 AND team_id = $2 AND user_id = $3",
                )
                .bind(organization_id.0)
                .bind(team_id.0)
                .bind(target_id)
                .fetch_optional(&mut **mutation.transaction())
                .await?
                .ok_or(AccessError::NotFound)?;
                let old_role = current
                    .0
                    .parse::<MembershipRole>()
                    .map_err(|_| AccessError::InvalidRequest("stored membership role"))?;
                let old_enabled = current.1;
                let new_role = role.unwrap_or(old_role);
                let new_enabled = enabled.unwrap_or(old_enabled);
                enforce_team_membership_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    team_id,
                    UserAccountId(target_id),
                    new_role,
                    new_enabled,
                )
                .await?;
                sqlx::query(
                    "UPDATE gateway.team_membership SET role = $4, enabled = $5
                     WHERE organization_id = $1 AND team_id = $2 AND user_id = $3",
                )
                .bind(organization_id.0)
                .bind(team_id.0)
                .bind(target_id)
                .bind(new_role.as_str())
                .bind(new_enabled)
                .execute(&mut **mutation.transaction())
                .await?;
                upsert_derived_team_binding(
                    &mut mutation.transaction(),
                    organization_id,
                    team_id,
                    UserAccountId(target_id),
                    new_role,
                    new_enabled,
                )
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn delete_team_member(
        &self,
        request: MutationRequest,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let team_id = match request.scope {
            AdminScope::Team { team_id, .. } => team_id,
            AdminScope::Organization(_) => {
                return Err(AccessError::InvalidRequest("team scope required"));
            }
        };
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                enforce_team_membership_delete_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    team_id,
                    UserAccountId(target_id),
                )
                .await?;
                sqlx::query(
                    "DELETE FROM gateway.team_membership
                     WHERE organization_id = $1 AND team_id = $2 AND user_id = $3",
                )
                .bind(organization_id.0)
                .bind(team_id.0)
                .bind(target_id)
                .execute(&mut **mutation.transaction())
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn create_role_binding(
        &self,
        request: MutationRequest,
        subject_user_id: Uuid,
        role: AdminRole,
    ) -> Result<MutationResult, AccessError> {
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                let target_id = mutation.target_id();
                assert_grant_within_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    role,
                )
                .await?;
                sqlx::query(
                    "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (id) DO UPDATE
                       SET role = EXCLUDED.role, enabled = TRUE, updated_at = CURRENT_TIMESTAMP",
                )
                .bind(target_id)
                .bind(organization_id.0)
                .bind(subject_user_id)
                .bind(role.as_str())
                .execute(&mut **mutation.transaction())
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn update_role_binding(
        &self,
        request: MutationRequest,
        role: AdminRole,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                enforce_binding_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    target_id,
                    Some(role),
                )
                .await?;
                assert_binding_owner_survives(
                    &mut mutation.transaction(),
                    organization_id,
                    target_id,
                    true,
                    Some(role),
                )
                .await?;
                sqlx::query(
                    "UPDATE gateway.admin_role_binding SET role = $3
                     WHERE organization_id = $1 AND id = $2",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .bind(role.as_str())
                .execute(&mut **mutation.transaction())
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    pub async fn revoke_role_binding(
        &self,
        request: MutationRequest,
    ) -> Result<MutationResult, AccessError> {
        let target_id = request.target_id;
        let organization_id = request.organization_id;
        let actor_id = request.actor_id;
        let admin_request = self.admin_request(request)?;
        let coordinator = self.admin.coordinator();
        let start = coordinator
            .begin(admin_request)
            .await
            .map_err(AccessError::from)?;
        match start {
            MutationStart::Replay(record) => Ok(MutationResult::replayed(&record)),
            MutationStart::New(mut mutation) => {
                enforce_binding_ceiling(
                    &mut mutation.transaction(),
                    actor_id,
                    organization_id,
                    target_id,
                    None,
                )
                .await?;
                assert_binding_owner_survives(
                    &mut mutation.transaction(),
                    organization_id,
                    target_id,
                    false,
                    None,
                )
                .await?;
                sqlx::query(
                    "UPDATE gateway.admin_role_binding SET enabled = FALSE
                     WHERE organization_id = $1 AND id = $2",
                )
                .bind(organization_id.0)
                .bind(target_id)
                .execute(&mut **mutation.transaction())
                .await?;
                finish_mutation(mutation, target_id).await
            }
        }
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn admin_request(&self, request: MutationRequest) -> Result<AdminMutationRequest, AccessError> {
        let operation = AdminOperation::new(request.operation)
            .map_err(|_| AccessError::InvalidRequest("operation"))?;
        let audit_action = AuditAction::new(request.audit_action)
            .map_err(|_| AccessError::InvalidRequest("audit action"))?;
        Ok(AdminMutationRequest {
            actor_id: request.actor_id,
            organization_id: request.organization_id,
            scope: request.scope,
            permission: request.permission,
            route: request.route,
            method: request.method.to_owned(),
            operation,
            idempotency_key: request.idempotency_key,
            target_id: request.target_id,
            parent_id: request.parent_id,
            target: request.target,
            audit_action,
            if_match: request.if_match,
            request_id: request.request_id,
            body: request.body,
        })
    }

    fn decoded_keyset(
        &self,
        ctx: &ListContext,
        page: &PageRequest,
    ) -> Result<Option<Vec<String>>, AccessError> {
        let Some(token) = page.page_token.as_deref() else {
            return Ok(None);
        };
        let codec = self
            .codec
            .as_ref()
            .ok_or(AccessError::AuthenticationUnavailable)?;
        let claims = codec
            .decode_and_validate(
                token,
                ctx.actor_id,
                ctx.scope,
                ctx.collection,
                &ctx.filters,
                ctx.order,
            )
            .map_err(AccessError::Pagination)?;
        Ok(decode_keyset(&claims.last_keyset))
    }

    fn next_token(
        &self,
        ctx: &ListContext,
        keyset: &[&str],
    ) -> Result<Option<String>, AccessError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or(AccessError::AuthenticationUnavailable)?;
        let claims = CursorClaims::new(
            ctx.actor_id,
            ctx.scope,
            ctx.collection,
            &ctx.filters,
            ctx.order,
            encode_keyset(keyset),
            None,
        )
        .map_err(AccessError::Pagination)?;
        codec
            .encode(&claims)
            .map(Some)
            .map_err(AccessError::Pagination)
    }
}

/// Grant-ceiling check executed inside the mutation transaction (the org row
/// is already locked by the coordinator), so the actor's ceiling cannot change
/// concurrently. `AdminRole::can_grant` prevents any escalation.
async fn assert_grant_within_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    granted: AdminRole,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling(transaction, actor_id, organization_id).await?;
    let Some(ceiling) = ceiling else {
        return Err(AccessError::EscalationDenied);
    };
    if ceiling.can_grant(granted) {
        Ok(())
    } else {
        Err(AccessError::EscalationDenied)
    }
}

/// Anti-escalation for binding updates/revokes: the actor may neither raise a
/// binding to a role above their ceiling nor modify a binding that already
/// ranks above their ceiling.
async fn enforce_binding_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    target_binding_id: Uuid,
    new_role: Option<AdminRole>,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling(transaction, actor_id, organization_id).await?;
    let Some(ceiling) = ceiling else {
        return Err(AccessError::EscalationDenied);
    };
    let current_role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.0)
    .bind(target_binding_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(current) = current_role {
        let current = current
            .parse::<AdminRole>()
            .map_err(|_| AccessError::InvalidRequest("stored role"))?;
        if current.rank() > ceiling.rank() {
            return Err(AccessError::EscalationDenied);
        }
    }
    if let Some(new_role) = new_role {
        if !ceiling.can_grant(new_role) {
            return Err(AccessError::EscalationDenied);
        }
    }
    Ok(())
}

/// The actor's highest org-wide role inside the mutation transaction (the org
/// row is locked by the coordinator for the whole mutation).
async fn actor_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
) -> Result<Option<AdminRole>, AccessError> {
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL AND enabled",
    )
    .bind(organization_id.0)
    .bind(actor_id.0)
    .fetch_all(&mut **transaction)
    .await?;
    Ok(roles
        .iter()
        .filter_map(|role| role.parse::<AdminRole>().ok())
        .max_by_key(|role| role.rank()))
}

/// The actor's highest role covering a team scope: an org-wide binding
/// inherits into every team, and an exact team binding covers its own team.
async fn actor_ceiling_at_team(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    team_id: TeamId,
) -> Result<Option<AdminRole>, AccessError> {
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND enabled
           AND (team_id IS NULL OR team_id = $3)",
    )
    .bind(organization_id.0)
    .bind(actor_id.0)
    .bind(team_id.0)
    .fetch_all(&mut **transaction)
    .await?;
    Ok(roles
        .iter()
        .filter_map(|role| role.parse::<AdminRole>().ok())
        .max_by_key(|role| role.rank()))
}

/// Grant-ceiling enforcement for the org-membership role projection. The actor
/// may not project a membership role above their ceiling (e.g. an organization
/// admin must not create an owner via the membership path) nor modify a
/// membership whose existing projected binding outranks them.
async fn enforce_org_membership_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    target_user_id: UserAccountId,
    new_role: MembershipRole,
    new_enabled: bool,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling(transaction, actor_id, organization_id)
        .await?
        .ok_or(AccessError::EscalationDenied)?;
    enforce_target_org_binding_within_ceiling(
        transaction,
        organization_id,
        target_user_id,
        ceiling,
    )
    .await?;
    let derived = AdminRole::from_membership(new_role);
    if new_enabled && derived != AdminRole::ReadOnlyMember && !ceiling.can_grant(derived) {
        return Err(AccessError::EscalationDenied);
    }
    Ok(())
}

/// Grant-ceiling enforcement for org-membership create (no existing projection
/// to constrain): the newly projected binding must be grantable.
async fn enforce_org_membership_create_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    new_role: MembershipRole,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling(transaction, actor_id, organization_id)
        .await?
        .ok_or(AccessError::EscalationDenied)?;
    let derived = AdminRole::from_membership(new_role);
    if derived != AdminRole::ReadOnlyMember && !ceiling.can_grant(derived) {
        return Err(AccessError::EscalationDenied);
    }
    Ok(())
}

/// Grant-ceiling enforcement for org-membership delete: the actor may not
/// remove a membership whose projected binding outranks them.
async fn enforce_org_membership_delete_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    target_user_id: UserAccountId,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling(transaction, actor_id, organization_id)
        .await?
        .ok_or(AccessError::EscalationDenied)?;
    enforce_target_org_binding_within_ceiling(transaction, organization_id, target_user_id, ceiling)
        .await
}

/// A membership whose enabled org-wide projection outranks the actor cannot be
/// modified through the membership path (prevents bypassing the role-binding
/// anti-escalation rule by demoting/removing a real owner).
async fn enforce_target_org_binding_within_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    target_user_id: UserAccountId,
    ceiling: AdminRole,
) -> Result<(), AccessError> {
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL AND enabled",
    )
    .bind(organization_id.0)
    .bind(target_user_id.0)
    .fetch_all(&mut **transaction)
    .await?;
    let highest = roles
        .iter()
        .filter_map(|role| role.parse::<AdminRole>().ok())
        .max_by_key(|role| role.rank());
    if highest.is_some_and(|role| role.rank() > ceiling.rank()) {
        return Err(AccessError::EscalationDenied);
    }
    Ok(())
}

/// Grant-ceiling enforcement for the team-membership role projection. Only
/// owner/admin team memberships project a `team_admin` binding, and the actor
/// must be able to grant that role at the team scope; a membership whose
/// existing team projection outranks the actor cannot be modified.
async fn enforce_team_membership_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    team_id: TeamId,
    target_user_id: UserAccountId,
    new_role: MembershipRole,
    new_enabled: bool,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling_at_team(transaction, actor_id, organization_id, team_id)
        .await?
        .ok_or(AccessError::EscalationDenied)?;
    let target_roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id = $3 AND enabled",
    )
    .bind(organization_id.0)
    .bind(target_user_id.0)
    .bind(team_id.0)
    .fetch_all(&mut **transaction)
    .await?;
    let highest = target_roles
        .iter()
        .filter_map(|role| role.parse::<AdminRole>().ok())
        .max_by_key(|role| role.rank());
    if highest.is_some_and(|role| role.rank() > ceiling.rank()) {
        return Err(AccessError::EscalationDenied);
    }
    let projects_team_admin =
        new_enabled && matches!(new_role, MembershipRole::Owner | MembershipRole::Admin);
    if projects_team_admin && !ceiling.can_grant(AdminRole::TeamAdmin) {
        return Err(AccessError::EscalationDenied);
    }
    Ok(())
}

/// Grant-ceiling enforcement for team-membership delete: the actor may not
/// remove a team membership whose projected team binding outranks them.
async fn enforce_team_membership_delete_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
    team_id: TeamId,
    target_user_id: UserAccountId,
) -> Result<(), AccessError> {
    let ceiling = actor_ceiling_at_team(transaction, actor_id, organization_id, team_id)
        .await?
        .ok_or(AccessError::EscalationDenied)?;
    let target_roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id = $3 AND enabled",
    )
    .bind(organization_id.0)
    .bind(target_user_id.0)
    .bind(team_id.0)
    .fetch_all(&mut **transaction)
    .await?;
    let highest = target_roles
        .iter()
        .filter_map(|role| role.parse::<AdminRole>().ok())
        .max_by_key(|role| role.rank());
    if highest.is_some_and(|role| role.rank() > ceiling.rank()) {
        return Err(AccessError::EscalationDenied);
    }
    Ok(())
}

/// Prevent removal of the last active organization owner through a role-binding
/// revoke or downgrade. The org row is FOR UPDATE locked by the coordinator for
/// the whole mutation, making the count race-safe.
async fn assert_binding_owner_survives(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    target_binding_id: Uuid,
    enabled_after: bool,
    role_after: Option<AdminRole>,
) -> Result<(), AccessError> {
    let current: Option<(String, bool)> = sqlx::query_as(
        "SELECT role, enabled FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.0)
    .bind(target_binding_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((role, enabled)) = current else {
        return Ok(());
    };
    if role != "organization_owner" || !enabled {
        return Ok(());
    }
    let still_owner = enabled_after && matches!(role_after, Some(AdminRole::OrganizationOwner));
    if still_owner {
        return Ok(());
    }
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND role = 'organization_owner' AND enabled AND id <> $2",
    )
    .bind(organization_id.0)
    .bind(target_binding_id)
    .fetch_one(&mut **transaction)
    .await?;
    if others == 0 {
        Err(AccessError::LastOwner)
    } else {
        Ok(())
    }
}

/// Membership-side last-owner protection: a role/enabled change that would
/// remove or downgrade the user's enabled org-owner binding must leave another
/// enabled owner in place.
async fn assert_membership_not_last_owner(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    user_id: UserAccountId,
    new_role: MembershipRole,
    new_enabled: bool,
) -> Result<(), AccessError> {
    let binding: Option<(String, bool)> = sqlx::query_as(
        "SELECT role, enabled FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL
           AND role = 'organization_owner'",
    )
    .bind(organization_id.0)
    .bind(user_id.0)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((_, enabled)) = binding else {
        return Ok(());
    };
    if !enabled {
        return Ok(());
    }
    let still_owner = new_enabled
        && matches!(
            AdminRole::from_membership(new_role),
            AdminRole::OrganizationOwner
        );
    if still_owner {
        return Ok(());
    }
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND role = 'organization_owner' AND enabled AND user_id <> $2",
    )
    .bind(organization_id.0)
    .bind(user_id.0)
    .fetch_one(&mut **transaction)
    .await?;
    if others == 0 {
        Err(AccessError::LastOwner)
    } else {
        Ok(())
    }
}

async fn assert_org_member_delete_safe(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    user_id: UserAccountId,
) -> Result<(), AccessError> {
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL
           AND role = 'organization_owner'",
    )
    .bind(organization_id.0)
    .bind(user_id.0)
    .fetch_optional(&mut **transaction)
    .await?;
    if enabled != Some(true) {
        return Ok(());
    }
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND role = 'organization_owner' AND enabled AND user_id <> $2",
    )
    .bind(organization_id.0)
    .bind(user_id.0)
    .fetch_one(&mut **transaction)
    .await?;
    if others == 0 {
        Err(AccessError::LastOwner)
    } else {
        Ok(())
    }
}

/// Keep the org-wide role-binding projection aligned with an org membership:
/// owner/admin/auditor memberships map to the deterministic `md5(org:user)`
/// binding; generic members and disabled memberships disable it.
async fn upsert_derived_org_binding(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    user_id: UserAccountId,
    role: MembershipRole,
    enabled: bool,
) -> Result<(), AccessError> {
    let admin_role = AdminRole::from_membership(role);
    if !enabled || admin_role == AdminRole::ReadOnlyMember {
        sqlx::query(
            "UPDATE gateway.admin_role_binding
             SET enabled = FALSE, updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL
               AND id = md5($1::text || ':' || $2::text)::uuid",
        )
        .bind(organization_id.0)
        .bind(user_id.0)
        .execute(&mut **transaction)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
             VALUES (md5($1::text || ':' || $2::text)::uuid, $1, $2, $3)
             ON CONFLICT (id) DO UPDATE
               SET role = EXCLUDED.role, enabled = TRUE, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(organization_id.0)
        .bind(user_id.0)
        .bind(admin_role.as_str())
        .execute(&mut **transaction)
        .await?;
    }
    seed_derived_org_binding_version(transaction, organization_id, user_id).await?;
    Ok(())
}

async fn seed_derived_org_binding_version(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    user_id: UserAccountId,
) -> Result<(), AccessError> {
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version AS version_row
             (organization_id, resource_type, resource_id)
         SELECT $1, 'role_binding', md5($1::text || ':' || $2::text)::uuid
         WHERE EXISTS (
             SELECT 1 FROM gateway.admin_role_binding
             WHERE organization_id = $1 AND id = md5($1::text || ':' || $2::text)::uuid
         )
         ON CONFLICT (organization_id, resource_type, resource_id) DO UPDATE
             SET version = version_row.version + 1,
                 updated_at = CURRENT_TIMESTAMP",
    )
    .bind(organization_id.0)
    .bind(user_id.0)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Keep the team-scoped role-binding projection aligned with a team membership:
/// owner/admin memberships map to `team_admin` on the deterministic
/// `md5(org:team:user)` binding; other roles and disabled memberships disable it.
async fn upsert_derived_team_binding(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    team_id: TeamId,
    user_id: UserAccountId,
    role: MembershipRole,
    enabled: bool,
) -> Result<(), AccessError> {
    let should_bind = enabled && matches!(role, MembershipRole::Owner | MembershipRole::Admin);
    if should_bind {
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role)
             VALUES (md5($1::text || ':' || $2::text || ':' || $3::text)::uuid, $1, $3, $2, 'team_admin')
             ON CONFLICT (id) DO UPDATE
               SET enabled = TRUE, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(organization_id.0)
        .bind(team_id.0)
        .bind(user_id.0)
        .execute(&mut **transaction)
        .await?;
    } else {
        sqlx::query(
            "UPDATE gateway.admin_role_binding
             SET enabled = FALSE, updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND user_id = $2 AND team_id = $3
               AND id = md5($1::text || ':' || $3::text || ':' || $2::text)::uuid",
        )
        .bind(organization_id.0)
        .bind(user_id.0)
        .bind(team_id.0)
        .execute(&mut **transaction)
        .await?;
    }
    seed_derived_team_binding_version(transaction, organization_id, team_id, user_id).await?;
    Ok(())
}

async fn seed_derived_team_binding_version(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    team_id: TeamId,
    user_id: UserAccountId,
) -> Result<(), AccessError> {
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version AS version_row
             (organization_id, resource_type, resource_id)
         SELECT $1, 'role_binding', md5($1::text || ':' || $2::text || ':' || $3::text)::uuid
         WHERE EXISTS (
             SELECT 1 FROM gateway.admin_role_binding
             WHERE organization_id = $1
               AND id = md5($1::text || ':' || $2::text || ':' || $3::text)::uuid
         )
         ON CONFLICT (organization_id, resource_type, resource_id) DO UPDATE
             SET version = version_row.version + 1,
                 updated_at = CURRENT_TIMESTAMP",
    )
    .bind(organization_id.0)
    .bind(team_id.0)
    .bind(user_id.0)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Complete a mutation: typed audit + idempotency completion happen inside
/// `AdminMutation::commit` before the transaction commits.
async fn finish_mutation(
    mutation: crate::persistence::admin::AdminMutation<'_>,
    target_id: Uuid,
) -> Result<MutationResult, AccessError> {
    let version = mutation.next_version().get();
    mutation
        .commit(
            None,
            Some(AuditSnapshot {
                version: Some(version),
                ..Default::default()
            }),
            Some(IdempotencyResultMetadata {
                resource_id: target_id,
                version: Some(version),
                secret_unavailable_on_replay: false,
                snapshot: None,
            }),
        )
        .await
        .map_err(AccessError::from)?;
    Ok(MutationResult {
        resource_id: target_id,
        version: Some(version),
        replayed: false,
        secret_unavailable_on_replay: false,
    })
}

impl MutationResult {
    fn replayed(record: &AdminIdempotencyRecord) -> Self {
        Self {
            resource_id: record.resource_id.unwrap_or_default(),
            version: record
                .response_metadata
                .as_ref()
                .and_then(|metadata| metadata.version),
            replayed: true,
            secret_unavailable_on_replay: record
                .response_metadata
                .as_ref()
                .map(|metadata| metadata.secret_unavailable_on_replay)
                .unwrap_or(false),
        }
    }
}

fn encode_keyset(parts: &[&str]) -> String {
    URL_SAFE_NO_PAD.encode(parts.join("\u{1f}").as_bytes())
}

fn decode_keyset(encoded: &str) -> Option<Vec<String>> {
    let bytes = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(text.split('\u{1f}').map(str::to_owned).collect())
}

#[derive(FromRow)]
struct TeamRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    version: i64,
}

impl From<TeamRow> for TeamView {
    fn from(row: TeamRow) -> Self {
        Self {
            id: row.id,
            organization_id: row.organization_id,
            name: row.name,
            version: u64::try_from(row.version).unwrap_or(1),
        }
    }
}

#[derive(FromRow)]
struct OrgMemberRow {
    user_id: Uuid,
    display_name: Option<String>,
    email: Option<String>,
    role: String,
    enabled: bool,
    version: i64,
}

#[derive(FromRow)]
struct TeamMemberRow {
    user_id: Uuid,
    display_name: Option<String>,
    email: Option<String>,
    role: String,
    enabled: bool,
    version: i64,
}

#[derive(FromRow)]
struct RoleBindingRow {
    id: Uuid,
    organization_id: Uuid,
    user_id: Uuid,
    display_name: Option<String>,
    email: Option<String>,
    team_id: Option<Uuid>,
    role: String,
    enabled: bool,
    version: i64,
}

impl From<RoleBindingRow> for RoleBindingView {
    fn from(row: RoleBindingRow) -> Self {
        Self {
            id: row.id,
            organization_id: row.organization_id,
            user_id: row.user_id,
            display_name: row.display_name,
            email: row.email,
            team_id: row.team_id,
            role: row.role,
            enabled: row.enabled,
            version: u64::try_from(row.version).unwrap_or(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyset_round_trips_and_survives_punctuation() {
        let encoded = encode_keyset(&["team a,b: c;", "123e4567-e89b-12d3-a456-426614174000"]);
        let decoded = decode_keyset(&encoded).expect("round trip");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], "team a,b: c;");
        assert_eq!(decoded[1], "123e4567-e89b-12d3-a456-426614174000");
        assert!(decode_keyset("!!not-base64!!").is_none());
    }

    #[test]
    fn mutation_result_replay_carries_no_secret() {
        let record = AdminIdempotencyRecord {
            organization_id: OrganizationId::new(),
            actor_id: UserAccountId::new(),
            operation: "team.create".into(),
            key: "key".into(),
            fingerprint: crate::domain::admin::RequestFingerprint([0; 32]),
            status: "succeeded".into(),
            resource_id: Some(Uuid::new_v4()),
            response_metadata: Some(IdempotencyResultMetadata {
                resource_id: Uuid::new_v4(),
                version: Some(2),
                secret_unavailable_on_replay: true,
                snapshot: None,
            }),
        };
        let result = MutationResult::replayed(&record);
        assert!(result.replayed);
        assert_eq!(result.version, Some(2));
        assert!(result.secret_unavailable_on_replay);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["resource_id"], record.resource_id.unwrap().to_string());
        assert_eq!(json["version"], 2);
    }
}
