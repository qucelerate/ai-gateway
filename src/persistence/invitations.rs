//! Invitations to an organization: a single-use link that admits whoever
//! signs in with it in a given role. The person is keyed on the `(issuer,
//! subject)` of that sign-in, so nobody has to learn their subject in
//! advance; the link only proves the invitation. Only a SHA-256 digest of
//! the token is stored. Creating, revoking and accepting are audited.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    admin::insert_audit_on,
    bootstrap::{BootstrapError, ensure_organization, ensure_user},
};
use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    identity::{MembershipRole, OrganizationId, UserAccountId},
};

/// A new invitation's token: 32 random bytes, base64url. Shown once.
pub struct InvitationToken(Zeroizing<String>);

impl InvitationToken {
    pub(crate) fn generate() -> Self {
        let mut bytes = Zeroizing::new([0_u8; 32]);
        crate::os_random::fill(bytes.as_mut());
        Self(Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_ref()),
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_tests(value: &str) -> Self {
        Self(Zeroizing::new(value.to_owned()))
    }
}

/// The stored digest of a token as presented, or `None` for one that is
/// not shaped like a token (so it can never match).
pub fn token_digest(token: &str) -> Option<Vec<u8>> {
    (token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then(|| Sha256::digest(token.as_bytes()).to_vec())
}

/// An invitation as it is listed: never the token or its digest.
#[derive(Clone, Debug, FromRow)]
pub struct Invitation {
    pub id: Uuid,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub accepted_by: Option<Uuid>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Invitation {
    pub fn status(&self) -> &'static str {
        self.status_at(Utc::now())
    }

    pub fn status_at(&self, now: DateTime<Utc>) -> &'static str {
        match (self.accepted_at, self.revoked_at) {
            (Some(_), _) => "accepted",
            (_, Some(_)) => "revoked",
            _ if self.expires_at <= now => "expired",
            _ => "pending",
        }
    }
}

const INVITATION_COLUMNS: &str = "id, role, created_at, expires_at, created_by, accepted_at, \
                                  accepted_by, revoked_at";

#[derive(Debug, thiserror::Error)]
pub enum InvitationError {
    /// Unknown, expired, revoked or already accepted; which is not said.
    #[error("the invitation is not valid")]
    Invalid,
    /// The sign-in came from another issuer than the invitation names.
    #[error("the invitation is for another identity provider")]
    WrongIssuer,
    /// The organization's provider configuration the sign-in went through
    /// is no longer its active one; nothing was accepted.
    #[error("the organization's identity provider changed")]
    ProviderChanged,
    #[error(transparent)]
    Provision(#[from] BootstrapError),
}

impl From<sqlx::Error> for InvitationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Provision(error.into())
    }
}

#[derive(Clone)]
pub struct InvitationStore {
    pool: PgPool,
}

impl InvitationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts an invitation `id` to the organization in `role`, to sign in
    /// from `issuer` within `valid_for`, on the caller's transaction, and
    /// returns its token. `created_by` is the inviting user; `None` for the
    /// operator. Auditing is the caller's.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        role: MembershipRole,
        issuer: &str,
        valid_for: chrono::Duration,
        created_by: Option<UserAccountId>,
    ) -> Result<InvitationToken, sqlx::Error> {
        let token = InvitationToken::generate();
        sqlx::query(
            "INSERT INTO gateway.organization_invitation
                 (id, organization_id, token_digest, role, issuer, created_by, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP + $7 * INTERVAL '1 second')",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(token_digest(token.as_str()).expect("a generated token is well-formed"))
        .bind(role.as_str())
        .bind(issuer)
        .bind(created_by.map(|user| user.0))
        .bind(valid_for.num_seconds() as f64)
        .execute(&mut *conn)
        .await?;
        Ok(token)
    }

    /// One invitation of the organization, on the caller's connection.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<Invitation>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM gateway.organization_invitation
             WHERE organization_id = $1 AND id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Revokes the invitation if it is still pending: not accepted, revoked
    /// or expired. Returns false when it is not. Auditing is the caller's.
    pub async fn revoke_pending(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.organization_invitation SET revoked_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND id = $2
               AND accepted_at IS NULL AND revoked_at IS NULL
               AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(organization_id.0)
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }

    /// One page of the organization's invitations, newest first, after the
    /// `(created_at, id)` keyset. The flag reports whether more rows follow.
    pub async fn page(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<(Vec<Invitation>, bool), sqlx::Error> {
        let rows: Vec<Invitation> = sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM gateway.organization_invitation
             WHERE organization_id = $1
               AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3))
             ORDER BY created_at DESC, id DESC
             LIMIT $4"
        ))
        .bind(organization_id.0)
        .bind(after.map(|(created_at, _)| created_at))
        .bind(after.map(|(_, id)| id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok((
            rows.into_iter().take(page_size as usize).collect(),
            has_more,
        ))
    }

    /// The highest organization-wide admin role the actor holds, on the
    /// caller's transaction: the ceiling of the roles they may invite.
    pub async fn actor_ceiling(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        actor: UserAccountId,
    ) -> Result<Option<crate::domain::admin::AdminRole>, sqlx::Error> {
        use crate::domain::admin::AdminRole;
        let roles: Vec<String> = sqlx::query_scalar(
            "SELECT role FROM gateway.admin_role_binding
             WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL AND enabled",
        )
        .bind(organization_id.0)
        .bind(actor.0)
        .fetch_all(&mut *conn)
        .await?;
        Ok(roles
            .iter()
            .filter_map(|role| role.parse::<AdminRole>().ok())
            .max_by_key(|role| role.rank()))
    }

    /// The operator's create (`gateway invitation create`): invites someone
    /// to the organization `slug` (created as `name`, else the slug, if
    /// missing). Returns the invitation's id and its token.
    pub async fn create(
        &self,
        slug: &str,
        name: Option<&str>,
        role: MembershipRole,
        issuer: &str,
        valid_for: chrono::Duration,
        created_by: Option<UserAccountId>,
    ) -> Result<(Uuid, OrganizationId, InvitationToken), InvitationError> {
        let mut tx = self.pool.begin().await?;
        let (organization_id, _) = ensure_organization(&mut tx, slug, name).await?;
        let organization_id = OrganizationId(organization_id);
        let id = Uuid::new_v4();
        let token = Self::insert(
            &mut tx,
            organization_id,
            id,
            role,
            issuer,
            valid_for,
            created_by,
        )
        .await?;
        audit(
            &mut tx,
            created_by.map_or(AuditActor::System, AuditActor::User),
            organization_id.0,
            id,
            "invitation.created",
        )
        .await?;
        tx.commit().await?;
        Ok((id, organization_id, token))
    }

    /// The organization's invitations, newest first.
    pub async fn list(&self, slug: &str) -> Result<Vec<Invitation>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM gateway.organization_invitation
             WHERE organization_id = (SELECT id FROM gateway.organization
                                      WHERE slug = $1 AND deleted_at IS NULL)
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(slug)
        .fetch_all(&self.pool)
        .await
    }

    /// Revokes a pending invitation of the organization `slug`. Returns
    /// false when there is none to revoke.
    pub async fn revoke(
        &self,
        slug: &str,
        id: Uuid,
        revoked_by: Option<UserAccountId>,
    ) -> Result<bool, InvitationError> {
        let mut tx = self.pool.begin().await?;
        let Some(organization_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM gateway.organization WHERE slug = $1 AND deleted_at IS NULL",
        )
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(false);
        };
        if !Self::revoke_pending(&mut tx, OrganizationId(organization_id), id).await? {
            return Ok(false);
        }
        audit(
            &mut tx,
            revoked_by.map_or(AuditActor::System, AuditActor::User),
            organization_id,
            id,
            "invitation.revoked",
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The pending invitation a token names, to start a sign-in with.
    pub async fn pending(&self, token: &str) -> Result<Option<Uuid>, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(None);
        };
        sqlx::query_scalar(
            "SELECT id FROM gateway.organization_invitation
             WHERE token_digest = $1 AND accepted_at IS NULL AND revoked_at IS NULL
               AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
    }

    /// The pending invitation a token names, with its organization and the
    /// issuer the accepting sign-in must come from, to choose the provider
    /// the sign-in goes through.
    pub async fn pending_target(
        &self,
        token: &str,
    ) -> Result<Option<(Uuid, OrganizationId, String)>, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(None);
        };
        let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
            "SELECT i.id, i.organization_id, i.issuer FROM gateway.organization_invitation i
             JOIN gateway.organization o ON o.id = i.organization_id AND o.suspended_at IS NULL
             WHERE i.token_digest = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL
               AND i.expires_at > CURRENT_TIMESTAMP",
        )
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, organization_id, issuer)| (id, OrganizationId(organization_id), issuer)))
    }

    /// Accepts the invitation for the person who signed in as `(issuer,
    /// subject)`: their user (created if missing), an enabled membership in
    /// the invited role and its derived role binding. Single-use: a second
    /// acceptance finds nothing pending.
    pub async fn accept(
        &self,
        id: Uuid,
        issuer: &str,
        subject: &str,
    ) -> Result<UserAccountId, InvitationError> {
        self.accept_through(id, issuer, subject, None).await
    }

    /// [`Self::accept`] for a sign-in that went through the organization's
    /// own provider `configuration`: it is accepted only if that is still
    /// the organization's active, enabled configuration, checked under the
    /// organization's lock (which disabling or activating also takes), so a
    /// provider changed mid-sign-in leaves the invitation unused.
    pub async fn accept_through(
        &self,
        id: Uuid,
        issuer: &str,
        subject: &str,
        configuration: Option<Uuid>,
    ) -> Result<UserAccountId, InvitationError> {
        let mut tx = self.pool.begin().await?;
        // The organization first, then the invitation: the order an admin
        // revoke (through the mutation coordinator) locks them in, so the
        // two cannot deadlock on the membership insert below.
        let organization_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT o.id FROM gateway.organization o
             JOIN gateway.organization_invitation i ON i.organization_id = o.id
             WHERE i.id = $1 AND o.suspended_at IS NULL
             FOR UPDATE OF o",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(InvitationError::Invalid)?;
        if let Some(configuration) = configuration {
            let current = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM gateway.organization_identity_provider
                                WHERE organization_id = $1 AND enabled
                                  AND active_configuration_id = $2)",
            )
            .bind(organization_id)
            .bind(configuration)
            .fetch_one(&mut *tx)
            .await?;
            if !current {
                return Err(InvitationError::ProviderChanged);
            }
        }
        let (role, invited_issuer) = sqlx::query_as::<_, (String, String)>(
            "SELECT role, issuer FROM gateway.organization_invitation
                 WHERE id = $1 AND organization_id = $2
                   AND accepted_at IS NULL AND revoked_at IS NULL
                   AND expires_at > CURRENT_TIMESTAMP
                 FOR UPDATE",
        )
        .bind(id)
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(InvitationError::Invalid)?;
        if invited_issuer != issuer {
            return Err(InvitationError::WrongIssuer);
        }
        let invited = role
            .parse::<MembershipRole>()
            .map_err(|_| InvitationError::Invalid)?;
        let (user_id, _) = ensure_user(&mut tx, issuer, subject, None).await?;
        raise_to(&mut tx, organization_id, user_id, invited).await?;
        sqlx::query(
            "UPDATE gateway.organization_invitation
             SET accepted_at = CURRENT_TIMESTAMP, accepted_by = $2 WHERE id = $1",
        )
        .bind(id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        audit(
            &mut tx,
            AuditActor::User(UserAccountId(user_id)),
            organization_id,
            id,
            "invitation.accepted",
        )
        .await?;
        tx.commit().await?;
        Ok(UserAccountId(user_id))
    }
}

/// Raises the person's access to at least the invited role, never lowering
/// anything and never granting more than was invited.
///
/// - Membership: set to the invited role if they have no enabled membership
///   or a lower one; a higher one is kept.
/// - Permissions, which authorization reads from enabled organization-wide
///   role bindings: if none already ranks at least the invited admin role,
///   the derived binding (`md5(org:user)`, the one the admin API manages)
///   is raised to exactly the invited admin role, never to the membership's
///   (which may be higher than an admin has since allowed). Member and
///   read-only invitations carry no admin role, so grant no binding.
pub(crate) async fn raise_to(
    tx: &mut sqlx::PgConnection,
    organization_id: Uuid,
    user_id: Uuid,
    invited: MembershipRole,
) -> Result<(), InvitationError> {
    use super::bootstrap::{bump_version, seed_version};
    use crate::domain::admin::AdminRole;

    let membership = sqlx::query_scalar::<_, String>(
        "SELECT role FROM gateway.organization_membership
         WHERE organization_id = $1 AND user_id = $2 AND enabled
         FOR UPDATE",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    .and_then(|role| role.parse::<MembershipRole>().ok());
    if membership.is_none_or(|current| rank(current) < rank(invited)) {
        sqlx::query(
            "INSERT INTO gateway.organization_membership AS m (organization_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (organization_id, user_id) DO UPDATE
                 SET role = EXCLUDED.role, enabled = true",
        )
        .bind(organization_id)
        .bind(user_id)
        .bind(invited.as_str())
        .execute(&mut *tx)
        .await?;
        bump_version(&mut *tx, organization_id, "membership", user_id).await?;
    } else {
        seed_version(&mut *tx, organization_id, "membership", user_id).await?;
    }

    let granted = AdminRole::from_membership(invited);
    if granted == AdminRole::ReadOnlyMember {
        return Ok(());
    }
    let held = sqlx::query_scalar::<_, String>(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL AND enabled
         FOR UPDATE",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .filter_map(|role| role.parse::<AdminRole>().ok())
    .map(AdminRole::rank)
    .max();
    if held.is_some_and(|held| held >= granted.rank()) {
        return Ok(());
    }
    let binding_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO gateway.admin_role_binding AS b (id, organization_id, user_id, role)
         VALUES (md5($1::text || ':' || $2::text)::uuid, $1, $2, $3)
         ON CONFLICT (id) DO UPDATE
             SET role = EXCLUDED.role, enabled = true, updated_at = CURRENT_TIMESTAMP
         RETURNING id",
    )
    .bind(organization_id)
    .bind(user_id)
    .bind(granted.as_str())
    .fetch_one(&mut *tx)
    .await?;
    bump_version(&mut *tx, organization_id, "role_binding", binding_id).await?;
    insert_audit_on(
        &mut *tx,
        &AdminAuditEvent::new(
            AuditActor::User(UserAccountId(user_id)),
            OrganizationId(organization_id),
            None,
            AuditTargetResource::RoleBinding,
            AuditAction::new("role_binding.invitation_accepted")
                .expect("audit action constant is valid"),
            binding_id,
            AuditOutcome::Succeeded,
            Uuid::new_v4(),
            None,
            Some(crate::domain::audit::AuditSnapshot {
                enabled: Some(true),
                role: Some(granted),
                ..Default::default()
            }),
        ),
    )
    .await
    .map_err(|error| InvitationError::Provision(error.into()))?;
    Ok(())
}

/// Membership roles by the access they carry, as their admin roles rank.
fn rank(role: MembershipRole) -> (u8, u8) {
    // Member and read-only map to the same admin role; member is the more.
    let tie = u8::from(role == MembershipRole::Member);
    (
        crate::domain::admin::AdminRole::from_membership(role).rank(),
        tie,
    )
}

pub(crate) async fn audit(
    tx: &mut sqlx::PgConnection,
    actor: AuditActor,
    organization_id: Uuid,
    invitation_id: Uuid,
    action: &'static str,
) -> Result<(), InvitationError> {
    insert_audit_on(
        tx,
        &AdminAuditEvent::new(
            actor,
            OrganizationId(organization_id),
            None,
            AuditTargetResource::Invitation,
            AuditAction::new(action).expect("audit action constant is valid"),
            invitation_id,
            AuditOutcome::Succeeded,
            Uuid::new_v4(),
            None,
            None,
        ),
    )
    .await
    .map_err(|error| InvitationError::Provision(error.into()))
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    const ISSUER: &str = "https://login.microsoftonline.com/tenant/v2.0";

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

    #[test]
    fn tokens_are_random_and_digested() {
        let (a, b) = (InvitationToken::generate(), InvitationToken::generate());
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str().len(), 43);
        assert_eq!(token_digest(a.as_str()).unwrap().len(), 32);
        for malformed in [
            "",
            "short",
            &"a".repeat(44),
            &format!("{}!", "a".repeat(42)),
        ] {
            assert!(token_digest(malformed).is_none(), "{malformed}");
        }
    }

    /// A revoke through the admin API locks the organization, then the
    /// invitation; acceptance must take them in the same order, or the two
    /// deadlock (acceptance's membership insert needs the organization).
    #[tokio::test]
    async fn acceptance_locks_the_organization_before_the_invitation() {
        if pool().await.is_none() {
            return;
        }
        // The revoke, the acceptance and the observer each hold a connection.
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&std::env::var("GATEWAY_TEST_DATABASE_URL").unwrap())
            .await
            .unwrap();
        let store = InvitationStore::new(pool.clone());
        let suffix = Uuid::new_v4().simple().to_string();
        let (id, organization, _) = store
            .create(
                &format!("invite-lock-{}", &suffix[..12]),
                None,
                MembershipRole::Member,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();

        // What the coordinator holds first.
        let mut revoke = pool.begin().await.unwrap();
        let revoke_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoke)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM gateway.organization WHERE id = $1 FOR UPDATE")
            .bind(organization.0)
            .execute(&mut *revoke)
            .await
            .unwrap();
        let accepting = tokio::spawn({
            let store = store.clone();
            let subject = format!("sub-lock-{suffix}");
            async move { store.accept(id, ISSUER, &subject).await }
        });
        // Until the acceptance is blocked by this transaction; only it
        // touches this new organization, so no other test's wait counts.
        let mut waiting = false;
        for _ in 0..100 {
            waiting = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                                WHERE $1 = ANY (pg_blocking_pids(pid)))",
            )
            .bind(revoke_pid)
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(waiting, "the acceptance never waited for the organization");
        // The invitation is still free for the revoke to take.
        let free = sqlx::query(
            "SELECT id FROM gateway.organization_invitation WHERE id = $1 FOR UPDATE NOWAIT",
        )
        .bind(id)
        .execute(&mut *revoke)
        .await;
        assert!(
            free.is_ok(),
            "acceptance locked the invitation first: {free:?}"
        );
        revoke.rollback().await.unwrap();
        accepting.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_invitation_admits_whoever_signs_in_with_it_once() {
        let Some(pool) = pool().await else {
            return;
        };
        let store = InvitationStore::new(pool.clone());
        let suffix = Uuid::new_v4().simple().to_string();
        let slug = format!("invite-{}", &suffix[..12]);
        let (id, organization, token) = store
            .create(
                &slug,
                Some("Invite Test"),
                MembershipRole::Owner,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        assert_eq!(store.pending(token.as_str()).await.unwrap(), Some(id));
        assert_eq!(store.pending("x".repeat(43).as_str()).await.unwrap(), None);
        // The token is never stored, only its digest.
        let stored: Vec<u8> = sqlx::query_scalar(
            "SELECT token_digest FROM gateway.organization_invitation WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_ne!(stored, token.as_str().as_bytes());

        // From another identity provider: refused, and still pending.
        assert!(matches!(
            store.accept(id, "https://evil.example/v2.0", "sub").await,
            Err(InvitationError::WrongIssuer)
        ));
        assert_eq!(store.pending(token.as_str()).await.unwrap(), Some(id));

        let subject = format!("sub-{suffix}");
        let user = store.accept(id, ISSUER, &subject).await.unwrap();
        // The person is an owner exactly as the admin API keeps one.
        let (role, membership): (String, String) = sqlx::query_as(
            "SELECT b.role, m.role FROM gateway.admin_role_binding b
             JOIN gateway.organization_membership m
               ON m.organization_id = b.organization_id AND m.user_id = b.user_id
             WHERE b.organization_id = $1 AND b.user_id = $2 AND b.enabled AND m.enabled
               AND b.id = md5($1::text || ':' || $2::text)::uuid",
        )
        .bind(organization.0)
        .bind(user.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (role.as_str(), membership.as_str()),
            ("organization_owner", "owner")
        );

        // Single use.
        assert_eq!(store.pending(token.as_str()).await.unwrap(), None);
        assert!(matches!(
            store.accept(id, ISSUER, "someone-else").await,
            Err(InvitationError::Invalid)
        ));

        // An owner accepting a lesser invitation keeps owner access.
        let (lesser, _, _) = store
            .create(
                &slug,
                None,
                MembershipRole::Member,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        assert_eq!(store.accept(lesser, ISSUER, &subject).await.unwrap(), user);
        let still_owner: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.admin_role_binding
                            WHERE user_id = $1 AND role = 'organization_owner' AND enabled)",
        )
        .bind(user.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still_owner);

        // Owner access held through a role binding of its own (the membership
        // being lower) survives a lesser invitation.
        let (other_owner, _, _) = store
            .create(
                &slug,
                None,
                MembershipRole::Member,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        let bound = store
            .accept(other_owner, ISSUER, &format!("{subject}-b"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
             VALUES ($1, $2, $3, 'organization_owner')",
        )
        .bind(Uuid::new_v4())
        .bind(organization.0)
        .bind(bound.0)
        .execute(&pool)
        .await
        .unwrap();
        let (admin_invite, _, _) = store
            .create(
                &slug,
                None,
                MembershipRole::Admin,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        store
            .accept(admin_invite, ISSUER, &format!("{subject}-b"))
            .await
            .unwrap();
        let (membership, owner): (String, bool) = sqlx::query_as(
            "SELECT m.role, EXISTS (SELECT 1 FROM gateway.admin_role_binding b
                                    WHERE b.user_id = m.user_id AND b.enabled
                                      AND b.role = 'organization_owner')
             FROM gateway.organization_membership m
             WHERE m.organization_id = $1 AND m.user_id = $2",
        )
        .bind(organization.0)
        .bind(bound.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        // The membership rises to the invited role; the owner access is kept.
        assert_eq!(membership, "admin");
        assert!(owner);

        // Permissions an admin revoked through the role-binding API, the
        // membership staying, are restored by an invitation, to exactly the
        // invited role: an owner membership with its binding revoked, invited
        // as admin, becomes an admin, not an owner again.
        let (to_admin, _, _) = store
            .create(
                &slug,
                None,
                MembershipRole::Owner,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        let revoked_user = store
            .accept(to_admin, ISSUER, &format!("{subject}-r"))
            .await
            .unwrap();
        sqlx::query("UPDATE gateway.admin_role_binding SET enabled = false WHERE user_id = $1")
            .bind(revoked_user.0)
            .execute(&pool)
            .await
            .unwrap();
        let (again, _, _) = store
            .create(
                &slug,
                None,
                MembershipRole::Admin,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        store
            .accept(again, ISSUER, &format!("{subject}-r"))
            .await
            .unwrap();
        let roles: Vec<(String, bool)> = sqlx::query_as(
            "SELECT role, enabled FROM gateway.admin_role_binding WHERE user_id = $1",
        )
        .bind(revoked_user.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(roles, [("organization_admin".to_owned(), true)]);

        // A member invitation grants no admin role; revoked and expired
        // ones admit nobody.
        let (member, _, _) = store
            .create(
                &slug,
                None,
                MembershipRole::Member,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        let member_user = store
            .accept(member, ISSUER, &format!("{subject}-m"))
            .await
            .unwrap();
        let bindings: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.admin_role_binding WHERE user_id = $1 AND enabled",
        )
        .bind(member_user.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(bindings, 0);
        let (revoked, _, revoked_token) = store
            .create(
                &slug,
                None,
                MembershipRole::Admin,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        assert!(store.revoke(&slug, revoked, None).await.unwrap());
        assert!(!store.revoke(&slug, revoked, None).await.unwrap());
        assert_eq!(store.pending(revoked_token.as_str()).await.unwrap(), None);
        let (expired, _, expired_token) = store
            .create(
                &slug,
                None,
                MembershipRole::Admin,
                ISSUER,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        sqlx::query(
            "UPDATE gateway.organization_invitation
             SET created_at = CURRENT_TIMESTAMP - INTERVAL '2 hours',
                 expires_at = CURRENT_TIMESTAMP - INTERVAL '1 hour'
             WHERE id = $1",
        )
        .bind(expired)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(store.pending(expired_token.as_str()).await.unwrap(), None);
        assert!(matches!(
            store.accept(expired, ISSUER, "late").await,
            Err(InvitationError::Invalid)
        ));

        let listed = store.list(&slug).await.unwrap();
        // Newest first (the expired one was backdated, so it sorts last).
        assert!(
            listed
                .windows(2)
                .all(|pair| pair[0].created_at >= pair[1].created_at)
        );
        let mut statuses: Vec<&str> = listed.iter().map(Invitation::status).collect();
        statuses.sort_unstable();
        assert_eq!(
            statuses,
            [
                "accepted", "accepted", "accepted", "accepted", "accepted", "accepted", "accepted",
                "expired", "revoked"
            ]
        );
        let audited: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM gateway.audit_event
             WHERE organization_id = $1 AND entity_type = 'invitation' ORDER BY occurred_at",
        )
        .bind(organization.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            audited
                .iter()
                .filter(|a| *a == "invitation.created")
                .count(),
            9
        );
        assert_eq!(
            audited
                .iter()
                .filter(|a| *a == "invitation.accepted")
                .count(),
            7
        );
        assert_eq!(
            audited
                .iter()
                .filter(|a| *a == "invitation.revoked")
                .count(),
            1
        );
    }
}
