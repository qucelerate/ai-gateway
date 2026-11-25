//! The first owner of an organization, which no admin can grant because
//! there is no admin yet. An operator runs `gateway bootstrap-owner` with the
//! person's OIDC issuer and subject; this creates, in one transaction and
//! idempotently, whatever is missing of the organization, the user, an owner
//! membership and an organization-wide `organization_owner` role binding.
//! The binding is audited by the system actor.

use sqlx::PgPool;
use uuid::Uuid;

use super::admin::{AdminStoreError, insert_audit_on};
use crate::domain::{
    admin::AdminRole,
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditTargetResource,
    },
    identity::{MembershipRole, OrganizationId, UserAccountId},
};

/// Who becomes the organization's first owner.
pub struct OwnerBootstrap<'a> {
    pub organization_slug: &'a str,
    /// The organization's name, if it has to be created; else the slug.
    pub organization_name: Option<&'a str>,
    pub oidc_issuer: &'a str,
    pub oidc_subject: &'a str,
    pub display_name: Option<&'a str>,
}

/// What a bootstrap found and what it created.
#[derive(Debug, Eq, PartialEq)]
pub struct OwnerBootstrapped {
    pub organization_id: OrganizationId,
    pub user_id: UserAccountId,
    pub created_organization: bool,
    pub created_user: bool,
    /// False when the user already held an enabled owner binding.
    pub granted_owner: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("the user account exists but is disabled")]
    UserDisabled,
    #[error("the user holds a role binding the admin API did not create")]
    ForeignBinding,
    #[error(transparent)]
    Store(#[from] AdminStoreError),
}

impl From<sqlx::Error> for BootstrapError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error.into())
    }
}

#[derive(Clone)]
pub struct BootstrapStore {
    pool: PgPool,
}

impl BootstrapStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn bootstrap_owner(
        &self,
        request: &OwnerBootstrap<'_>,
    ) -> Result<OwnerBootstrapped, BootstrapError> {
        let mut tx = self.pool.begin().await?;

        let (organization_id, created_organization) = ensure_organization(
            &mut tx,
            request.organization_slug,
            request.organization_name,
        )
        .await?;
        let (user_id, created_user) = ensure_user(
            &mut tx,
            request.oidc_issuer,
            request.oidc_subject,
            request.display_name,
        )
        .await?;
        let granted = grant_membership(
            &mut tx,
            organization_id,
            user_id,
            MembershipRole::Owner,
            AuditActor::System,
            "role_binding.bootstrapped",
        )
        .await?;
        tx.commit().await?;
        Ok(OwnerBootstrapped {
            organization_id: OrganizationId(organization_id),
            user_id: UserAccountId(user_id),
            created_organization,
            created_user,
            granted_owner: granted,
        })
    }
}

/// The organization with `slug`, created with `name` (else the slug) if
/// missing. Conflict-safe, then a locked read: concurrent callers converge.
pub(crate) async fn ensure_organization(
    tx: &mut sqlx::PgConnection,
    slug: &str,
    name: Option<&str>,
) -> Result<(Uuid, bool), BootstrapError> {
    let created = sqlx::query(
        "INSERT INTO gateway.organization (id, slug, name) VALUES ($1, $2, $3)
         ON CONFLICT (slug) WHERE deleted_at IS NULL DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(slug)
    .bind(name.unwrap_or(slug))
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    let id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM gateway.organization WHERE slug = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(slug)
    .fetch_one(&mut *tx)
    .await?;
    seed_version(&mut *tx, id, "organization", id).await?;
    Ok((id, created))
}

/// The enabled user `(issuer, subject)`, created if missing. A disabled
/// account was disabled on purpose: re-enabling it is a decision for an
/// admin, so it is refused.
pub(crate) async fn ensure_user(
    tx: &mut sqlx::PgConnection,
    issuer: &str,
    subject: &str,
    display_name: Option<&str>,
) -> Result<(Uuid, bool), BootstrapError> {
    let created = sqlx::query(
        "INSERT INTO gateway.user_account (id, oidc_issuer, oidc_subject, display_name)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (oidc_issuer, oidc_subject)
             WHERE oidc_issuer IS NOT NULL AND oidc_subject IS NOT NULL
         DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(issuer)
    .bind(subject)
    .bind(display_name)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    let (id, enabled) = sqlx::query_as::<_, (Uuid, bool)>(
        "SELECT id, enabled FROM gateway.user_account
         WHERE oidc_issuer = $1 AND oidc_subject = $2 FOR UPDATE",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_one(&mut *tx)
    .await?;
    if !enabled {
        return Err(BootstrapError::UserDisabled);
    }
    Ok((id, created))
}

/// An enabled membership in `role` and its derived organization-wide role
/// binding, exactly as the admin API keeps them (`upsert_derived_org_binding`):
/// the binding's id is md5(org:user), owner/admin/auditor memberships enable
/// it in the matching role, and member/read-only ones disable it. A later
/// change to the membership through the API therefore reaches this very
/// binding. Version rows are seeded, or moved on where something changed. A
/// granted binding is audited as `action` by `actor`; returns whether one was.
pub(crate) async fn grant_membership(
    tx: &mut sqlx::PgConnection,
    organization_id: Uuid,
    user_id: Uuid,
    role: MembershipRole,
    actor: AuditActor,
    action: &'static str,
) -> Result<bool, BootstrapError> {
    let membership_changed = sqlx::query(
        "INSERT INTO gateway.organization_membership AS m (organization_id, user_id, role)
         VALUES ($1, $2, $3)
         ON CONFLICT (organization_id, user_id) DO UPDATE
             SET role = EXCLUDED.role, enabled = true
             WHERE m.role <> EXCLUDED.role OR NOT m.enabled",
    )
    .bind(organization_id)
    .bind(user_id)
    .bind(role.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if membership_changed {
        bump_version(&mut *tx, organization_id, "membership", user_id).await?;
    } else {
        seed_version(&mut *tx, organization_id, "membership", user_id).await?;
    }

    let admin_role = AdminRole::from_membership(role);
    // An enabled binding in this role with another id was not made by the
    // admin API: its membership changes would not reach it.
    let foreign = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM gateway.admin_role_binding
             WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL
               AND role = $3 AND id <> md5($1::text || ':' || $2::text)::uuid
         )",
    )
    .bind(organization_id)
    .bind(user_id)
    .bind(admin_role.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if foreign && admin_role != AdminRole::ReadOnlyMember {
        return Err(BootstrapError::ForeignBinding);
    }
    let binding_id = sqlx::query_scalar::<_, Uuid>("SELECT md5($1::text || ':' || $2::text)::uuid")
        .bind(organization_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    let granted = if admin_role == AdminRole::ReadOnlyMember {
        let disabled = sqlx::query(
            "UPDATE gateway.admin_role_binding
             SET enabled = false, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND enabled",
        )
        .bind(binding_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if disabled {
            bump_version(&mut *tx, organization_id, "role_binding", binding_id).await?;
        }
        false
    } else {
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding AS b (id, organization_id, user_id, role)
             VALUES ($3, $1, $2, $4)
             ON CONFLICT (id) DO UPDATE
                 SET role = EXCLUDED.role, enabled = true, updated_at = CURRENT_TIMESTAMP
                 WHERE b.role <> EXCLUDED.role OR NOT b.enabled",
        )
        .bind(organization_id)
        .bind(user_id)
        .bind(binding_id)
        .bind(admin_role.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1
    };
    if granted {
        bump_version(&mut *tx, organization_id, "role_binding", binding_id).await?;
        insert_audit_on(
            &mut *tx,
            &AdminAuditEvent::new(
                actor,
                OrganizationId(organization_id),
                None,
                AuditTargetResource::RoleBinding,
                AuditAction::new(action).expect("audit action constant is valid"),
                binding_id,
                AuditOutcome::Succeeded,
                Uuid::new_v4(),
                None,
                Some(AuditSnapshot {
                    enabled: Some(true),
                    role: Some(admin_role),
                    ..Default::default()
                }),
            ),
        )
        .await?;
    } else if admin_role != AdminRole::ReadOnlyMember {
        seed_version(&mut *tx, organization_id, "role_binding", binding_id).await?;
    }
    Ok(granted)
}

/// The admin API's version row for a resource, created at version 1 if it
/// has none: its first API change then expects ETag 1.
pub(crate) async fn seed_version(
    tx: &mut sqlx::PgConnection,
    organization_id: Uuid,
    resource_type: &str,
    resource_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
         VALUES ($1, $2, $3)
         ON CONFLICT (organization_id, resource_type, resource_id) DO NOTHING",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .execute(tx)
    .await?;
    Ok(())
}

/// A resource the bootstrap changed: its version moves on, so a client
/// holding the old ETag gets a conflict rather than overwriting the change.
pub(crate) async fn bump_version(
    tx: &mut sqlx::PgConnection,
    organization_id: Uuid,
    resource_type: &str,
    resource_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version AS v
             (organization_id, resource_type, resource_id)
         VALUES ($1, $2, $3)
         ON CONFLICT (organization_id, resource_type, resource_id) DO UPDATE
             SET version = v.version + 1, updated_at = CURRENT_TIMESTAMP",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .execute(tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    async fn pool() -> Option<PgPool> {
        let url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return None,
        };
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        Some(pool)
    }

    #[tokio::test]
    async fn the_first_owner_is_bootstrapped_once_and_audited() {
        let Some(pool) = pool().await else {
            return;
        };
        let store = BootstrapStore::new(pool.clone());
        let suffix = Uuid::new_v4().simple().to_string();
        let slug = format!("bootstrap-{}", &suffix[..12]);
        let subject = format!("entra-sub-{suffix}");
        let request = OwnerBootstrap {
            organization_slug: &slug,
            organization_name: Some("Bootstrap Test"),
            oidc_issuer: "https://login.microsoftonline.com/tenant/v2.0",
            oidc_subject: &subject,
            display_name: Some("First Owner"),
        };

        // Two at once converge on the same rows.
        let (a, b) = tokio::join!(
            store.bootstrap_owner(&request),
            store.bootstrap_owner(&request)
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            (a.organization_id, a.user_id),
            (b.organization_id, b.user_id)
        );
        assert!(a.created_organization ^ b.created_organization);
        assert!(a.granted_owner ^ b.granted_owner);
        let first = a;
        // Idempotent: nothing more to create or grant.
        let again = store.bootstrap_owner(&request).await.unwrap();
        assert!(!again.created_organization && !again.created_user && !again.granted_owner);

        // The binding is the one the admin API's membership changes manage,
        // md5(org:user), and every resource has the version row the API's
        // ETags need.
        let (binding_id, role, membership): (Uuid, String, String) = sqlx::query_as(
            "SELECT b.id, b.role, m.role FROM gateway.admin_role_binding b
             JOIN gateway.organization_membership m
               ON m.organization_id = b.organization_id AND m.user_id = b.user_id
             WHERE b.organization_id = $1 AND b.user_id = $2 AND b.enabled AND m.enabled
               AND b.team_id IS NULL",
        )
        .bind(first.organization_id.0)
        .bind(first.user_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (role.as_str(), membership.as_str()),
            ("organization_owner", "owner")
        );
        let derived: Uuid = sqlx::query_scalar("SELECT md5($1::text || ':' || $2::text)::uuid")
            .bind(first.organization_id.0)
            .bind(first.user_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(binding_id, derived);
        let versions: Vec<(String, i64)> = sqlx::query_as(
            "SELECT resource_type, version FROM gateway.admin_resource_version
             WHERE organization_id = $1 ORDER BY resource_type",
        )
        .bind(first.organization_id.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            ["membership", "organization", "role_binding"]
        );

        // A demotion through the admin API disables exactly this binding
        // (as `upsert_derived_org_binding` does), leaving no owner access.
        sqlx::query(
            "UPDATE gateway.admin_role_binding SET enabled = FALSE
             WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL
               AND id = md5($1::text || ':' || $2::text)::uuid",
        )
        .bind(first.organization_id.0)
        .bind(first.user_id.0)
        .execute(&pool)
        .await
        .unwrap();
        let owner_access: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.admin_role_binding
             WHERE organization_id = $1 AND user_id = $2 AND enabled",
        )
        .bind(first.organization_id.0)
        .bind(first.user_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(owner_access, 0);

        // Bootstrapping again re-grants it, moves its version on, and audits.
        let version_before = versions
            .iter()
            .find(|(kind, _)| kind == "role_binding")
            .unwrap()
            .1;
        assert!(store.bootstrap_owner(&request).await.unwrap().granted_owner);
        let version_after: i64 = sqlx::query_scalar(
            "SELECT version FROM gateway.admin_resource_version
             WHERE organization_id = $1 AND resource_type = 'role_binding' AND resource_id = $2",
        )
        .bind(first.organization_id.0)
        .bind(binding_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(version_after, version_before + 1);
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.audit_event
             WHERE organization_id = $1 AND action = 'role_binding.bootstrapped'
               AND actor_type = 'system' AND entity_type = 'role_binding'",
        )
        .bind(first.organization_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audited, 2);

        // An owner binding the admin API did not create is refused.
        let other_subject = format!("{subject}-2");
        let other = OwnerBootstrap {
            organization_slug: &slug,
            organization_name: None,
            oidc_issuer: "https://login.microsoftonline.com/tenant/v2.0",
            oidc_subject: &other_subject,
            display_name: None,
        };
        let second = store.bootstrap_owner(&other).await.unwrap();
        assert!(!second.created_organization && second.created_user);
        sqlx::query(
            "UPDATE gateway.admin_role_binding SET id = $3
             WHERE organization_id = $1 AND user_id = $2",
        )
        .bind(second.organization_id.0)
        .bind(second.user_id.0)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.bootstrap_owner(&other).await,
            Err(BootstrapError::ForeignBinding)
        ));

        // A disabled account is not re-enabled by a bootstrap.
        sqlx::query("UPDATE gateway.user_account SET enabled = false WHERE id = $1")
            .bind(second.user_id.0)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.bootstrap_owner(&other).await,
            Err(BootstrapError::UserDisabled)
        ));
    }
}
