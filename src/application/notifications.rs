// Notifications application service: a member's list, unread count and
// read state (Phase 10, D-024).
//
// Any member of the organization may read their notifications, but only
// those whose required permission their role holds organization-wide; a
// team-scoped member sees none of the catalog's. Marking read changes only
// the member's own state, so it is neither versioned nor audited.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::domain::{
    admin::{AdminAuthorizationContext, AdminScope},
    identity::{OrganizationId, UserAccountId},
    notifications::{
        MarkRead, MarkReadError, NotificationFilter, NotificationKind, NotificationView,
        UnreadCount, permission_name,
    },
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
};
use crate::persistence::{
    admin::{AdminStore, AdminStoreError},
    notifications::NotificationStore,
};

pub const ROUTE_NOTIFICATIONS: &str = "/api/admin/v1/organizations/{org}/notifications";
pub const ROUTE_NOTIFICATIONS_UNREAD_COUNT: &str =
    "/api/admin/v1/organizations/{org}/notifications/unread-count";
pub const ROUTE_NOTIFICATIONS_READ: &str = "/api/admin/v1/organizations/{org}/notifications/read";

const COLLECTION: &str = "notifications";
const ORDER_CREATED_DESC: &str = "created_at_desc";

#[derive(Debug)]
pub enum NotificationServiceError {
    Forbidden,
    NotFound,
    Invalid(MarkReadError),
    Pagination(PaginationError),
    AuthenticationUnavailable,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for NotificationServiceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone)]
pub struct NotificationService {
    admin: AdminStore,
    store: NotificationStore,
    codec: Option<SignedCursorCodec>,
}

impl NotificationService {
    pub fn new(
        admin: AdminStore,
        store: NotificationStore,
        codec: Option<SignedCursorCodec>,
    ) -> Self {
        Self {
            admin,
            store,
            codec,
        }
    }

    /// The permissions (by name) whose notifications the member may see.
    async fn visible(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<(AdminScope, Vec<String>), NotificationServiceError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.bindings.iter().any(|binding| binding.enabled) {
            return Err(NotificationServiceError::Forbidden);
        }
        let scope = AdminScope::Organization(organization_id);
        Ok((scope, visible_permissions(&context, scope)))
    }

    /// The member's notifications, newest first.
    pub async fn list(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        filter: &NotificationFilter,
        page: &PageRequest,
    ) -> Result<Page<NotificationView>, NotificationServiceError> {
        let (scope, permissions) = self.visible(actor, organization_id).await?;
        let codec = self
            .codec
            .as_ref()
            .ok_or(NotificationServiceError::AuthenticationUnavailable)?;
        let filters = filter.normalized();
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(
                        token,
                        actor,
                        scope,
                        COLLECTION,
                        &filters,
                        ORDER_CREATED_DESC,
                    )
                    .map_err(NotificationServiceError::Pagination)?;
                Some(decode_keyset(&claims.last_keyset).ok_or_else(|| {
                    NotificationServiceError::Store("notification page keyset is invalid".into())
                })?)
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list(
                organization_id,
                actor,
                &permissions,
                filter,
                page.page_size,
                after,
            )
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(
                CursorClaims::new(
                    actor,
                    scope,
                    COLLECTION,
                    filters,
                    ORDER_CREATED_DESC,
                    encode_keyset(last.created_at, last.id),
                    None,
                )
                .and_then(|claims| codec.encode(&claims))
                .map_err(NotificationServiceError::Pagination)?,
            ),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn unread_count(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<UnreadCount, NotificationServiceError> {
        let (_, permissions) = self.visible(actor, organization_id).await?;
        Ok(UnreadCount {
            unread: self
                .store
                .unread_count(organization_id, actor, &permissions)
                .await?,
        })
    }

    /// Marks notifications read for the member; returns the unread count
    /// after. `through` must name one they may see.
    pub async fn mark_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: MarkRead,
        now: DateTime<Utc>,
    ) -> Result<UnreadCount, NotificationServiceError> {
        let target = input.target().map_err(NotificationServiceError::Invalid)?;
        let (_, permissions) = self.visible(actor, organization_id).await?;
        if !self
            .store
            .mark_read(organization_id, actor, &permissions, &target, now)
            .await?
        {
            return Err(NotificationServiceError::NotFound);
        }
        Ok(UnreadCount {
            unread: self
                .store
                .unread_count(organization_id, actor, &permissions)
                .await?,
        })
    }
}

/// Every notification kind's permission the context holds organization-wide.
fn visible_permissions(context: &AdminAuthorizationContext, scope: AdminScope) -> Vec<String> {
    let mut names: Vec<String> = NotificationKind::ALL
        .into_iter()
        .map(NotificationKind::permission)
        .filter(|permission| context.allows(*permission, scope))
        .map(permission_name)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

fn encode_keyset(created_at: DateTime<Utc>, id: Uuid) -> String {
    format!("{}|{id}", created_at.to_rfc3339())
}

fn decode_keyset(value: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let (created_at, id) = value.split_once('|')?;
    Some((
        DateTime::parse_from_rfc3339(created_at)
            .ok()?
            .with_timezone(&Utc),
        Uuid::parse_str(id).ok()?,
    ))
}

fn map_admin_error(error: AdminStoreError) -> NotificationServiceError {
    match error {
        AdminStoreError::Authorization(_) => NotificationServiceError::Forbidden,
        AdminStoreError::ResourceNotFound => NotificationServiceError::NotFound,
        other => NotificationServiceError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminRole, AdminRoleBinding};

    #[test]
    fn org_wide_roles_see_what_their_role_reads_and_team_members_see_nothing() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for role in [
            AdminRole::OrganizationOwner,
            AdminRole::OrganizationAdmin,
            AdminRole::Operator,
            AdminRole::Auditor,
            AdminRole::ReadOnlyMember,
        ] {
            let context = AdminAuthorizationContext {
                actor_id: user,
                organization_id: org,
                bindings: vec![AdminRoleBinding::organization(org, user, role)],
            };
            let visible = visible_permissions(&context, scope);
            assert!(visible.iter().any(|p| p == "catalog_read"), "{role:?}");
            if role == AdminRole::OrganizationOwner {
                assert_eq!(
                    visible,
                    [
                        "api_keys_list",
                        "budget_read",
                        "catalog_read",
                        "members_list",
                        "reconciliation_read",
                        "usage_read",
                        "webhook_read"
                    ]
                );
            }
        }
        let team = crate::domain::identity::TeamId::new();
        let context = AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings: vec![AdminRoleBinding::team(
                org,
                team,
                user,
                AdminRole::ReadOnlyMember,
            )],
        };
        assert!(visible_permissions(&context, scope).is_empty());
    }

    #[test]
    fn keysets_round_trip() {
        let at = Utc::now();
        let id = Uuid::new_v4();
        assert_eq!(decode_keyset(&encode_keyset(at, id)), Some((at, id)));
        assert_eq!(decode_keyset("nonsense"), None);
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-notifications-v1.json"))
                .unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = [
            ROUTE_NOTIFICATIONS,
            ROUTE_NOTIFICATIONS_UNREAD_COUNT,
            ROUTE_NOTIFICATIONS_READ,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
        // The request takes exactly one target, as the server enforces.
        assert_eq!(
            contract["components"]["schemas"]["MarkRead"]["oneOf"],
            serde_json::json!([{ "required": ["through"] }, { "required": ["ids"] }])
        );
    }
}
