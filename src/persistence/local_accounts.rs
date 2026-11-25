//! Local accounts' storage: credentials, single-use password links (the
//! first-run setup link and reset links), password sessions, and the
//! deployment's administrators. See [`crate::domain::password`].
//!
//! Hashing is the caller's (it is CPU-heavy and runs off the executor);
//! this module only stores and compares what it is given, inside the
//! transactions that keep each step consistent.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    admin::insert_audit_on,
    auth::insert_security_event,
    bootstrap::{BootstrapError, ensure_organization, grant_membership},
    invitations::{InvitationToken, raise_to, token_digest},
};
use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    auth::{SecurityEvent, SessionRecord},
    identity::{MembershipRole, OrganizationId, UserAccountId},
    password::{LOCAL_ISSUER, lockout_after},
};

/// SQL true for organization `o` when its members may act in it with a
/// password session: it does not require its own, enabled provider.
pub(crate) const PASSWORD_ALLOWED: &str = "NOT (o.require_sso AND EXISTS (
    SELECT 1 FROM gateway.organization_identity_provider p
    WHERE p.organization_id = o.id AND p.enabled))";

/// A local account as a sign-in sees it.
#[derive(Clone, Debug)]
pub struct StoredCredential {
    pub user_id: UserAccountId,
    pub email: String,
    pub password_hash: String,
    pub locked_until: Option<DateTime<Utc>>,
    pub enabled: bool,
}

impl StoredCredential {
    pub fn locked_at(&self, now: DateTime<Utc>) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }
}

/// How a password sign-in with the right password ended.
#[derive(Debug, Eq, PartialEq)]
pub enum PasswordSignIn {
    SignedIn,
    /// Disabled, or without an enabled membership anywhere.
    NoMembership,
    /// Every organization the person belongs to requires its own provider.
    SsoRequired,
    /// The password changed after the one given was checked.
    Stale,
}

/// The first organization and its owner, from the setup form.
pub struct Setup<'a> {
    pub token: &'a str,
    pub organization_slug: &'a str,
    pub organization_name: &'a str,
    pub email: &'a str,
    pub display_name: Option<&'a str>,
    pub password_hash: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// Unknown, used or expired link.
    #[error("the setup link is not valid")]
    InvalidLink,
    /// An organization exists: setup has been done.
    #[error("the gateway is already set up")]
    AlreadyDone,
    #[error(transparent)]
    Store(#[from] BootstrapError),
}

impl From<sqlx::Error> for SetupError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error.into())
    }
}

/// What first-run setup created.
#[derive(Debug, Eq, PartialEq)]
pub struct SetUp {
    pub organization_id: OrganizationId,
    pub user_id: UserAccountId,
}

/// Accepting an invitation with a password.
pub struct PasswordAcceptance<'a> {
    pub invitation_token: &'a str,
    pub email: &'a str,
    pub display_name: Option<&'a str>,
}

/// How the account accepting an invitation is proven.
pub enum AcceptingAccount<'a> {
    /// A new local account with this password hash.
    New { password_hash: &'a str },
    /// The existing local account, whose password the caller verified
    /// against `verified_hash`, as this store returned it. Refused when the
    /// password changed since.
    Existing {
        user_id: UserAccountId,
        verified_hash: &'a str,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PasswordAcceptError {
    /// Unknown, expired, revoked or used; or one that must be accepted
    /// through the organization's own provider.
    #[error("the invitation is not valid for a password")]
    Invalid,
    /// A local account with that email exists (or was created meanwhile):
    /// its password is needed.
    #[error("an account with this email exists")]
    AccountExists,
    #[error("the account is disabled")]
    Disabled,
    /// The account's password changed after it was checked.
    #[error("the account's password changed")]
    StalePassword,
    #[error(transparent)]
    Store(#[from] BootstrapError),
}

impl From<sqlx::Error> for PasswordAcceptError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error.into())
    }
}

impl From<super::invitations::InvitationError> for PasswordAcceptError {
    fn from(error: super::invitations::InvitationError) -> Self {
        use super::invitations::InvitationError;
        match error {
            InvitationError::Provision(BootstrapError::UserDisabled) => Self::Disabled,
            InvitationError::Provision(error) => Self::Store(error),
            _ => Self::Invalid,
        }
    }
}

/// What a pending invitation offers to whoever holds its link.
#[derive(Debug, Eq, PartialEq)]
pub struct InvitationOffer {
    pub organization_id: OrganizationId,
    pub organization_name: String,
    pub role: String,
    /// It may be accepted with a local account.
    pub password: bool,
    /// The issuer its sign-in must otherwise come from.
    pub issuer: String,
}

/// Why an owner may not make a reset link for someone.
#[derive(Debug, Eq, PartialEq)]
pub enum ResetRefusal {
    /// Not an enabled member with a local account.
    NotFound,
    /// The person belongs to an organization the owner does not own, or
    /// administers the deployment: only the operator may reset them.
    OperatorOnly,
}

#[derive(Clone)]
pub struct LocalAccountStore {
    pool: PgPool,
}

impl LocalAccountStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Whether any organization exists. First-run setup is offered only
    /// while none does.
    pub async fn any_organization(&self) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM gateway.organization)")
            .fetch_one(&self.pool)
            .await
    }

    /// A new first-run setup link, valid for `valid_for`, while no
    /// organization exists; `None` once one does. Each call makes another
    /// link: every unused one works until setup completes.
    pub async fn create_setup_link(
        &self,
        valid_for: chrono::Duration,
    ) -> Result<Option<InvitationToken>, sqlx::Error> {
        let token = InvitationToken::generate();
        let digest = token_digest(token.as_str()).expect("generated tokens are well-formed");
        let inserted = sqlx::query(
            "INSERT INTO gateway.password_link (id, purpose, token_digest, expires_at)
             SELECT $1, 'setup', $2, CURRENT_TIMESTAMP + $3 * INTERVAL '1 second'
             WHERE NOT EXISTS (SELECT 1 FROM gateway.organization)",
        )
        .bind(Uuid::new_v4())
        .bind(digest)
        .bind(valid_for.num_seconds())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok((inserted == 1).then_some(token))
    }

    /// Whether `token` is an unused, unexpired setup link and setup is
    /// still to be done.
    pub async fn setup_link_valid(&self, token: &str) -> Result<bool, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(false);
        };
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.password_link
                            WHERE token_digest = $1 AND purpose = 'setup' AND used_at IS NULL
                              AND expires_at > CURRENT_TIMESTAMP)
                    AND NOT EXISTS (SELECT 1 FROM gateway.organization)",
        )
        .bind(digest)
        .fetch_one(&self.pool)
        .await
    }

    /// First-run setup, in one transaction: the organization, its owner as
    /// a local account (also a deployment administrator), every setup link
    /// used up, and `session` for the owner. Serialized, so two setups
    /// cannot both succeed.
    pub async fn complete_setup(
        &self,
        setup: &Setup<'_>,
        session: &SessionRecord,
    ) -> Result<SetUp, SetupError> {
        let digest = token_digest(setup.token).ok_or(SetupError::InvalidLink)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gateway.first_run_setup'))")
            .execute(&mut *tx)
            .await?;
        let done: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM gateway.organization)")
            .fetch_one(&mut *tx)
            .await?;
        if done {
            return Err(SetupError::AlreadyDone);
        }
        let valid = sqlx::query(
            "UPDATE gateway.password_link SET used_at = CURRENT_TIMESTAMP
             WHERE token_digest = $1 AND purpose = 'setup' AND used_at IS NULL
               AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(&digest)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !valid {
            return Err(SetupError::InvalidLink);
        }
        // The other links stop working too.
        sqlx::query(
            "UPDATE gateway.password_link SET used_at = CURRENT_TIMESTAMP
             WHERE purpose = 'setup' AND used_at IS NULL",
        )
        .execute(&mut *tx)
        .await?;
        let (organization_id, _) = ensure_organization(
            &mut tx,
            setup.organization_slug,
            Some(setup.organization_name),
        )
        .await?;
        let user_id = insert_local_user(
            &mut tx,
            setup.email,
            setup.display_name,
            setup.password_hash,
        )
        .await?;
        grant_membership(
            &mut tx,
            organization_id,
            user_id.0,
            MembershipRole::Owner,
            AuditActor::System,
            "role_binding.setup",
        )
        .await?;
        sqlx::query("INSERT INTO gateway.deployment_admin (user_id) VALUES ($1)")
            .bind(user_id.0)
            .execute(&mut *tx)
            .await?;
        crate::persistence::deployment::record(
            &mut tx,
            Some(user_id),
            "setup.completed",
            Some(organization_id),
            Some(user_id.0),
            serde_json::json!({ "slug": setup.organization_slug, "name": setup.organization_name }),
        )
        .await?;
        insert_security_event(&mut tx, Some(user_id), SecurityEvent::SetupCompleted).await?;
        insert_password_session(&mut tx, user_id, session).await?;
        tx.commit().await?;
        Ok(SetUp {
            organization_id: OrganizationId(organization_id),
            user_id,
        })
    }

    /// The local account `email` (normalized) signs in as.
    pub async fn credential(&self, email: &str) -> Result<Option<StoredCredential>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Uuid, String, String, Option<DateTime<Utc>>, bool)>(
            "SELECT c.user_id, c.email, c.password_hash, c.locked_until, u.enabled
             FROM gateway.local_credential c
             JOIN gateway.user_account u ON u.id = c.user_id
             WHERE c.email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(user_id, email, password_hash, locked_until, enabled)| StoredCredential {
                user_id: UserAccountId(user_id),
                email,
                password_hash,
                locked_until,
                enabled,
            },
        ))
    }

    /// The local account of `user_id`, if it has one.
    pub async fn credential_of(
        &self,
        user_id: UserAccountId,
    ) -> Result<Option<StoredCredential>, sqlx::Error> {
        let email: Option<String> =
            sqlx::query_scalar("SELECT email FROM gateway.local_credential WHERE user_id = $1")
                .bind(user_id.0)
                .fetch_optional(&self.pool)
                .await?;
        match email {
            Some(email) => self.credential(&email).await,
            None => Ok(None),
        }
    }

    /// A wrong password for `user_id`: one more failure in a row, which
    /// locks the account for a while once there are enough of them.
    pub async fn record_failure(&self, user_id: UserAccountId) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let failures: i32 = sqlx::query_scalar(
            "UPDATE gateway.local_credential SET failed_attempts = failed_attempts + 1
             WHERE user_id = $1 RETURNING failed_attempts",
        )
        .bind(user_id.0)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(lockout) = lockout_after(u32::try_from(failures).unwrap_or(u32::MAX)) {
            sqlx::query(
                "UPDATE gateway.local_credential
                 SET locked_until = CURRENT_TIMESTAMP + $2 * INTERVAL '1 second'
                 WHERE user_id = $1",
            )
            .bind(user_id.0)
            .bind(lockout.as_secs() as i64)
            .execute(&mut *tx)
            .await?;
        }
        insert_security_event(&mut tx, Some(user_id), SecurityEvent::PasswordRejected).await?;
        tx.commit().await
    }

    /// A sign-in refused because the account is locked.
    pub async fn record_locked(&self, user_id: UserAccountId) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        insert_security_event(&mut tx, Some(user_id), SecurityEvent::LoginLocked).await?;
        tx.commit().await
    }

    /// The right password for `user_id` (it matched `verified_hash`): the
    /// failure count is cleared and, if the password is still that one and
    /// the person is enabled and may act with a password in at least one
    /// organization, `session` is created. The credential stays locked
    /// until commit, so a reset or change either ends this session or
    /// comes first and refuses it.
    pub async fn finish_sign_in(
        &self,
        user_id: UserAccountId,
        verified_hash: &str,
        session: &SessionRecord,
    ) -> Result<PasswordSignIn, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let current: Option<String> = sqlx::query_scalar(
            "SELECT password_hash FROM gateway.local_credential WHERE user_id = $1 FOR UPDATE",
        )
        .bind(user_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        if current.as_deref() != Some(verified_hash) {
            return Ok(PasswordSignIn::Stale);
        }
        sqlx::query(
            "UPDATE gateway.local_credential SET failed_attempts = 0, locked_until = NULL
             WHERE user_id = $1",
        )
        .bind(user_id.0)
        .execute(&mut *tx)
        .await?;
        let (member, allowed): (bool, bool) = sqlx::query_as(&format!(
            "SELECT COALESCE(bool_or(true), false), COALESCE(bool_or({PASSWORD_ALLOWED}), false)
             FROM gateway.user_account u
             JOIN gateway.organization_membership om ON om.user_id = u.id AND om.enabled
             JOIN gateway.organization o ON o.id = om.organization_id AND o.suspended_at IS NULL
             WHERE u.id = $1 AND u.enabled"
        ))
        .bind(user_id.0)
        .fetch_one(&mut *tx)
        .await?;
        // A deployment administrator signs in without an organization to act
        // in (theirs may all be suspended): the deployment's routes need none.
        let administrator: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.deployment_admin d
                            JOIN gateway.user_account u ON u.id = d.user_id AND u.enabled
                            WHERE d.user_id = $1)",
        )
        .bind(user_id.0)
        .fetch_one(&mut *tx)
        .await?;
        let outcome = match (member, allowed) {
            _ if administrator => {
                insert_password_session(&mut tx, user_id, session).await?;
                PasswordSignIn::SignedIn
            }
            (false, _) => PasswordSignIn::NoMembership,
            (true, false) => PasswordSignIn::SsoRequired,
            (true, true) => {
                insert_password_session(&mut tx, user_id, session).await?;
                PasswordSignIn::SignedIn
            }
        };
        if outcome != PasswordSignIn::SignedIn {
            insert_security_event(&mut tx, Some(user_id), SecurityEvent::PasswordLoginDisabled)
                .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Changes the password of `user_id` from `verified_hash` (the one the
    /// caller checked the current password against) to `password_hash`, and
    /// ends their other sessions (all of them when `keep` is `None`).
    /// `event` says why. False when the password changed meanwhile.
    pub async fn change_password(
        &self,
        user_id: UserAccountId,
        verified_hash: &str,
        password_hash: &str,
        keep: Option<Uuid>,
        event: SecurityEvent,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_account(&mut tx, user_id).await?;
        let current: Option<String> = sqlx::query_scalar(
            "SELECT password_hash FROM gateway.local_credential WHERE user_id = $1 FOR UPDATE",
        )
        .bind(user_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        if current.as_deref() != Some(verified_hash) {
            return Ok(false);
        }
        let changed = set_password_on(&mut tx, user_id, password_hash, keep, event).await?;
        tx.commit().await?;
        Ok(changed)
    }

    /// The id of the session `token_digest` names, with how it signed in.
    pub async fn session(
        &self,
        token_digest: &[u8],
    ) -> Result<Option<(Uuid, String)>, sqlx::Error> {
        sqlx::query_as("SELECT id, method FROM gateway.session WHERE token_digest = $1")
            .bind(token_digest)
            .fetch_optional(&self.pool)
            .await
    }

    /// Whether an owner (`actor`, owner of `organization_id`) may make a
    /// reset link for `target`: an enabled member there with a local
    /// account, who belongs to no organization the owner does not own and
    /// does not administer the deployment (unless the owner does).
    pub async fn reset_allowed(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        target: UserAccountId,
    ) -> Result<Result<(), ResetRefusal>, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool, bool, bool)>(&format!(
            "SELECT
                 EXISTS (SELECT 1 FROM gateway.organization_membership om
                         JOIN gateway.local_credential c ON c.user_id = om.user_id
                         WHERE om.organization_id = $2 AND om.user_id = $3 AND om.enabled),
                 {OWNER_COVERS_TARGET},
                 {TARGET_NOT_ABOVE_OWNER}"
        ))
        .bind(actor.0)
        .bind(organization_id.0)
        .bind(target.0)
        .fetch_one(&self.pool)
        .await?;
        Ok(match row {
            (false, _, _) => Err(ResetRefusal::NotFound),
            (true, true, true) => Ok(()),
            _ => Err(ResetRefusal::OperatorOnly),
        })
    }

    /// A password-reset link for `user_id`, valid for `valid_for`; earlier
    /// unused ones stop working. `audit` is the organization (and owner)
    /// to record it under; `None` for the operator.
    pub async fn create_reset_link(
        &self,
        user_id: UserAccountId,
        valid_for: chrono::Duration,
        audit: Option<(OrganizationId, UserAccountId)>,
    ) -> Result<InvitationToken, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let token =
            insert_reset_link(&mut tx, user_id, valid_for, audit.map(|(_, actor)| actor)).await?;
        if let Some((organization_id, actor)) = audit {
            insert_audit_on(
                &mut tx,
                &AdminAuditEvent::new(
                    AuditActor::User(actor),
                    organization_id,
                    None,
                    AuditTargetResource::Membership,
                    AuditAction::new("membership.password_reset_link_created")
                        .expect("audit action constant is valid"),
                    user_id.0,
                    AuditOutcome::Succeeded,
                    Uuid::new_v4(),
                    None,
                    None,
                ),
            )
            .await
            .map_err(store_error)?;
        }
        tx.commit().await?;
        Ok(token)
    }

    /// The account an unused, unexpired reset link is for, with its email
    /// (which the new password is checked against).
    pub async fn reset_link_target(
        &self,
        token: &str,
    ) -> Result<Option<(UserAccountId, String)>, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(None);
        };
        let row = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT l.user_id, c.email FROM gateway.password_link l
             JOIN gateway.local_credential c ON c.user_id = l.user_id
             WHERE l.token_digest = $1 AND l.purpose = 'reset' AND l.used_at IS NULL
               AND l.expires_at > CURRENT_TIMESTAMP",
        )
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(user_id, email)| (UserAccountId(user_id), email)))
    }

    /// Uses the reset link `token` to set its account's password to
    /// `password_hash`, ending every session of the account. False when the
    /// link is no longer valid (used meanwhile, say).
    pub async fn reset_with_link(
        &self,
        token: &str,
        password_hash: &str,
    ) -> Result<bool, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await?;
        // Which account, unlocked: every password write locks the account,
        // then its credential, then its links (see `lock_account`), so this
        // one takes the account before it touches the link.
        let Some(user_id) = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT user_id FROM gateway.password_link
             WHERE token_digest = $1 AND purpose = 'reset'",
        )
        .bind(&digest)
        .fetch_optional(&mut *tx)
        .await?
        .flatten() else {
            return Ok(false);
        };
        lock_account(&mut tx, UserAccountId(user_id)).await?;
        sqlx::query("SELECT 1 FROM gateway.local_credential WHERE user_id = $1 FOR UPDATE")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        let link = sqlx::query_scalar::<_, Option<Uuid>>(
            "UPDATE gateway.password_link SET used_at = CURRENT_TIMESTAMP
             WHERE token_digest = $1 AND purpose = 'reset' AND used_at IS NULL
               AND expires_at > CURRENT_TIMESTAMP AND user_id = $2
             RETURNING created_by",
        )
        .bind(&digest)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(created_by) = link else {
            return Ok(false);
        };
        // An owner's link works only while the owner could still make it:
        // the account has joined no organization the owner does not own,
        // and has not become a deployment administrator. The operator's
        // links (no maker) always do.
        if let Some(owner) = created_by {
            if !owner_may_reset(&mut tx, UserAccountId(owner), UserAccountId(user_id)).await? {
                tx.commit().await?;
                return Ok(false);
            }
        }
        let changed = set_password_on(
            &mut tx,
            UserAccountId(user_id),
            password_hash,
            None,
            SecurityEvent::PasswordReset,
        )
        .await?;
        tx.commit().await?;
        Ok(changed)
    }

    /// What the pending invitation `token` offers: its organization and
    /// role, and whether it may be accepted with a local account (it is not
    /// bound to the organization's own, enabled provider, and the
    /// organization does not require that provider).
    pub async fn invitation_offer(
        &self,
        token: &str,
    ) -> Result<Option<InvitationOffer>, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(None);
        };
        let row = sqlx::query_as::<_, (Uuid, String, String, String, bool)>(&format!(
            "SELECT o.id, o.name, i.role, i.issuer, {INVITATION_PASSWORD_OK}
             FROM gateway.organization_invitation i
             JOIN gateway.organization o ON o.id = i.organization_id AND o.suspended_at IS NULL
             WHERE i.token_digest = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL
               AND i.expires_at > CURRENT_TIMESTAMP"
        ))
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(organization_id, organization_name, role, issuer, password)| InvitationOffer {
                organization_id: OrganizationId(organization_id),
                organization_name,
                role,
                password,
                issuer,
            },
        ))
    }

    /// Accepts the invitation `token` for a local account, in one
    /// transaction: a new account (refused if the email is taken) or the
    /// existing one the caller proved, the invited access, the invitation
    /// used up and audited, and `session` for the person.
    pub async fn accept_invitation(
        &self,
        acceptance: &PasswordAcceptance<'_>,
        account: AcceptingAccount<'_>,
        session: &SessionRecord,
    ) -> Result<UserAccountId, PasswordAcceptError> {
        let digest =
            token_digest(acceptance.invitation_token).ok_or(PasswordAcceptError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        // The organization first, then the invitation: the lock order of
        // every other acceptance and revoke.
        let organization_id: Uuid = sqlx::query_scalar(
            "SELECT o.id FROM gateway.organization o
             JOIN gateway.organization_invitation i ON i.organization_id = o.id
             WHERE i.token_digest = $1 AND o.suspended_at IS NULL
             
             FOR UPDATE OF o",
        )
        .bind(&digest)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PasswordAcceptError::Invalid)?;
        let (invitation_id, role) = sqlx::query_as::<_, (Uuid, String)>(&format!(
            "SELECT i.id, i.role FROM gateway.organization_invitation i
             JOIN gateway.organization o ON o.id = i.organization_id
             WHERE i.token_digest = $1 AND i.organization_id = $2
               AND i.accepted_at IS NULL AND i.revoked_at IS NULL
               AND i.expires_at > CURRENT_TIMESTAMP
               AND {INVITATION_PASSWORD_OK}
             FOR UPDATE OF i"
        ))
        .bind(&digest)
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PasswordAcceptError::Invalid)?;
        let invited = role
            .parse::<MembershipRole>()
            .map_err(|_| PasswordAcceptError::Invalid)?;
        let user_id = match account {
            AcceptingAccount::New { password_hash } => {
                let taken: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM gateway.local_credential WHERE email = $1)",
                )
                .bind(acceptance.email)
                .fetch_one(&mut *tx)
                .await?;
                if taken {
                    return Err(PasswordAcceptError::AccountExists);
                }
                insert_local_user(
                    &mut tx,
                    acceptance.email,
                    acceptance.display_name,
                    password_hash,
                )
                .await?
            }
            AcceptingAccount::Existing {
                user_id,
                verified_hash,
            } => {
                let row: Option<(bool, String)> = sqlx::query_as(
                    "SELECT u.enabled, c.password_hash FROM gateway.user_account u
                     JOIN gateway.local_credential c ON c.user_id = u.id
                     WHERE u.id = $1 AND c.email = $2 FOR UPDATE OF u, c",
                )
                .bind(user_id.0)
                .bind(acceptance.email)
                .fetch_optional(&mut *tx)
                .await?;
                match row {
                    Some((_, current)) if current != verified_hash => {
                        return Err(PasswordAcceptError::StalePassword);
                    }
                    Some((true, _)) => user_id,
                    Some((false, _)) => return Err(PasswordAcceptError::Disabled),
                    None => return Err(PasswordAcceptError::Invalid),
                }
            }
        };
        raise_to(&mut tx, organization_id, user_id.0, invited).await?;
        sqlx::query(
            "UPDATE gateway.organization_invitation
             SET accepted_at = CURRENT_TIMESTAMP, accepted_by = $2 WHERE id = $1",
        )
        .bind(invitation_id)
        .bind(user_id.0)
        .execute(&mut *tx)
        .await?;
        super::invitations::audit(
            &mut tx,
            AuditActor::User(user_id),
            organization_id,
            invitation_id,
            "invitation.accepted",
        )
        .await?;
        sqlx::query(
            "UPDATE gateway.local_credential SET failed_attempts = 0, locked_until = NULL
             WHERE user_id = $1",
        )
        .bind(user_id.0)
        .execute(&mut *tx)
        .await?;
        insert_password_session(&mut tx, user_id, session).await?;
        tx.commit().await?;
        Ok(user_id)
    }

    /// The organization's sign-in policy: whether it requires its own
    /// provider, and whether that provider is enabled (without which the
    /// requirement has no effect). `None` for no such organization.
    pub async fn sign_in_policy(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<(bool, bool)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT o.require_sso, EXISTS (
                 SELECT 1 FROM gateway.organization_identity_provider p
                 WHERE p.organization_id = o.id AND p.enabled)
             FROM gateway.organization o WHERE o.id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&self.pool)
        .await
    }

    /// Sets whether the organization requires its own provider, audited as
    /// `actor`'s. Requiring it needs that provider enabled, so nobody is
    /// locked out: `Ok(false)` when it is not. Under the organization's
    /// lock, which enabling and disabling the provider also take.
    pub async fn set_require_sso(
        &self,
        organization_id: OrganizationId,
        actor: AuditActor,
        require: bool,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let (current, provider): (bool, bool) = sqlx::query_as(
            "SELECT o.require_sso, EXISTS (
                 SELECT 1 FROM gateway.organization_identity_provider p
                 WHERE p.organization_id = o.id AND p.enabled)
             FROM gateway.organization o WHERE o.id = $1 FOR UPDATE OF o",
        )
        .bind(organization_id.0)
        .fetch_one(&mut *tx)
        .await?;
        if require && !provider {
            return Ok(false);
        }
        if current == require {
            return Ok(true);
        }
        sqlx::query("UPDATE gateway.organization SET require_sso = $2 WHERE id = $1")
            .bind(organization_id.0)
            .bind(require)
            .execute(&mut *tx)
            .await?;
        insert_audit_on(
            &mut tx,
            &AdminAuditEvent::new(
                actor,
                organization_id,
                None,
                AuditTargetResource::Organization,
                AuditAction::new(if require {
                    "organization.sso_required"
                } else {
                    "organization.sso_not_required"
                })
                .expect("audit action constant is valid"),
                organization_id.0,
                AuditOutcome::Succeeded,
                Uuid::new_v4(),
                None,
                None,
            ),
        )
        .await
        .map_err(store_error)?;
        tx.commit().await?;
        Ok(true)
    }

    /// The organization with `slug`, for the operator's commands.
    pub async fn organization_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<OrganizationId>, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM gateway.organization WHERE slug = $1 AND deleted_at IS NULL",
        )
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?
        .map(OrganizationId))
    }

    /// Whether `user_id` administers the deployment.
    pub async fn is_deployment_admin(&self, user_id: UserAccountId) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.deployment_admin WHERE user_id = $1)",
        )
        .bind(user_id.0)
        .fetch_one(&self.pool)
        .await
    }

    /// Makes the account with local email `email`, or the user `(issuer,
    /// subject)`, a deployment administrator. Returns the user, or `None`
    /// when there is no such account; granting twice changes nothing.
    pub async fn grant_deployment_admin(
        &self,
        account: AdminAccount<'_>,
    ) -> Result<Option<(UserAccountId, bool)>, sqlx::Error> {
        let user_id: Option<Uuid> = match account {
            AdminAccount::Email(email) => {
                sqlx::query_scalar("SELECT user_id FROM gateway.local_credential WHERE email = $1")
                    .bind(email)
                    .fetch_optional(&self.pool)
                    .await?
            }
            AdminAccount::Oidc { issuer, subject } => {
                sqlx::query_scalar(
                    "SELECT id FROM gateway.user_account
                     WHERE oidc_issuer = $1 AND oidc_subject = $2",
                )
                .bind(issuer)
                .bind(subject)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        let Some(user_id) = user_id else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await?;
        let granted = sqlx::query(
            "INSERT INTO gateway.deployment_admin (user_id) VALUES ($1)
             ON CONFLICT (user_id) DO NOTHING",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if granted {
            // By the operator.
            crate::persistence::deployment::record(
                &mut tx,
                None,
                "deployment_admin.granted",
                None,
                Some(user_id),
                serde_json::json!({}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(Some((UserAccountId(user_id), granted)))
    }
}

/// Who `gateway admin grant` names.
pub enum AdminAccount<'a> {
    Email(&'a str),
    Oidc { issuer: &'a str, subject: &'a str },
}

/// SQL true for invitation `i` of organization `o` when a local account may
/// accept it: it is not bound to the organization's own, enabled provider
/// (whose issuer it would then name), and passwords are allowed there.
const INVITATION_PASSWORD_OK: &str = "(NOT EXISTS (
        SELECT 1 FROM gateway.organization_identity_provider p
        JOIN gateway.identity_provider_configuration c ON c.id = p.active_configuration_id
        WHERE p.organization_id = i.organization_id AND p.enabled AND c.issuer = i.issuer)
    AND NOT (o.require_sso AND EXISTS (
        SELECT 1 FROM gateway.organization_identity_provider p
        WHERE p.organization_id = o.id AND p.enabled)))";

/// A new local user and its credential; its subject is its own id.
pub(crate) async fn insert_local_user(
    tx: &mut PgConnection,
    email: &str,
    display_name: Option<&str>,
    password_hash: &str,
) -> Result<UserAccountId, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.user_account (id, oidc_issuer, oidc_subject, email, display_name)
         VALUES ($1, $2, $1::text, $3, $4)",
    )
    .bind(id)
    .bind(LOCAL_ISSUER)
    .bind(email)
    .bind(display_name)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO gateway.local_credential (user_id, email, password_hash) VALUES ($1, $2, $3)",
    )
    .bind(id)
    .bind(email)
    .bind(password_hash)
    .execute(&mut *tx)
    .await?;
    Ok(UserAccountId(id))
}

fn store_error(error: super::admin::AdminStoreError) -> sqlx::Error {
    match error {
        super::admin::AdminStoreError::Database(error) => error,
        other => sqlx::Error::Protocol(format!("{other:?}")),
    }
}

/// Lock order. Every transaction that changes a local account's password
/// or its reset links locks, in this order and as far as it needs: the
/// `user_account` row, its `local_credential` row, then its `password_link`
/// rows. Sign-in locks only the credential, which keeps the order.
async fn lock_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: UserAccountId,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM gateway.user_account WHERE id = $1 FOR UPDATE")
        .bind(user_id.0)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// A reset link for `user_id` on the caller's transaction, replacing its
/// unused ones; `created_by` is the owner who made it (`None`: the operator
/// or a deployment administrator). The organization audit is the caller's.
pub(crate) async fn insert_reset_link(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: UserAccountId,
    valid_for: chrono::Duration,
    created_by: Option<UserAccountId>,
) -> Result<InvitationToken, sqlx::Error> {
    let token = InvitationToken::generate();
    let digest = token_digest(token.as_str()).expect("generated tokens are well-formed");
    // One maker at a time per account, so a newer link always replaces the
    // earlier one.
    lock_account(tx, user_id).await?;
    sqlx::query(
        "UPDATE gateway.password_link SET used_at = CURRENT_TIMESTAMP
         WHERE user_id = $1 AND purpose = 'reset' AND used_at IS NULL",
    )
    .bind(user_id.0)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO gateway.password_link
             (id, purpose, token_digest, user_id, created_by, expires_at)
         VALUES ($1, 'reset', $2, $3, $4, CURRENT_TIMESTAMP + $5 * INTERVAL '1 second')",
    )
    .bind(Uuid::new_v4())
    .bind(digest)
    .bind(user_id.0)
    .bind(created_by.map(|actor| actor.0))
    .bind(valid_for.num_seconds())
    .execute(&mut **tx)
    .await?;
    insert_security_event(tx, Some(user_id), SecurityEvent::PasswordResetLinkCreated).await?;
    Ok(token)
}

/// `$1` (an owner) owns every organization `$3` belongs to.
const OWNER_COVERS_TARGET: &str = "NOT EXISTS (SELECT 1 FROM gateway.organization_membership t
             WHERE t.user_id = $3 AND t.enabled
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.organization_membership a
                   WHERE a.user_id = $1 AND a.enabled AND a.role = 'owner'
                     AND a.organization_id = t.organization_id))";

/// `$3` is no deployment administrator, unless `$1` is one too.
const TARGET_NOT_ABOVE_OWNER: &str =
    "(NOT EXISTS (SELECT 1 FROM gateway.deployment_admin WHERE user_id = $3)
             OR EXISTS (SELECT 1 FROM gateway.deployment_admin WHERE user_id = $1))";

/// Whether `owner` may still reset `target`'s password (see
/// [`LocalAccountStore::reset_allowed`]), apart from where they meet.
async fn owner_may_reset(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: UserAccountId,
    target: UserAccountId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT {OWNER_COVERS_TARGET} AND {TARGET_NOT_ABOVE_OWNER}
         FROM (SELECT $1::UUID AS a, $2::UUID AS b, $3::UUID AS c) AS parameters"
    ))
    .bind(owner.0)
    .bind(Option::<Uuid>::None)
    .bind(target.0)
    .fetch_one(&mut **tx)
    .await
}

/// Sets a password and ends the account's sessions but `keep`.
async fn set_password_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: UserAccountId,
    password_hash: &str,
    keep: Option<Uuid>,
    event: SecurityEvent,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE gateway.local_credential
         SET password_hash = $2, password_changed_at = CURRENT_TIMESTAMP,
             failed_attempts = 0, locked_until = NULL
         WHERE user_id = $1",
    )
    .bind(user_id.0)
    .bind(password_hash)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if !changed {
        return Ok(false);
    }
    // Links made for the old password stop working.
    sqlx::query(
        "UPDATE gateway.password_link SET used_at = CURRENT_TIMESTAMP
         WHERE user_id = $1 AND purpose = 'reset' AND used_at IS NULL",
    )
    .bind(user_id.0)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE gateway.session SET revoked_at = CURRENT_TIMESTAMP
         WHERE user_id = $1 AND revoked_at IS NULL AND ($2::UUID IS NULL OR id <> $2)",
    )
    .bind(user_id.0)
    .bind(keep)
    .execute(&mut **tx)
    .await?;
    insert_security_event(tx, Some(user_id), event).await?;
    Ok(true)
}

/// A process-wide session signed in with a password, and its event.
async fn insert_password_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: UserAccountId,
    session: &SessionRecord,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gateway.session
         (id, token_digest, user_id, browser_binding_digest, expires_at, method)
         VALUES ($1, $2, $3, $4, to_timestamp($5), 'password')",
    )
    .bind(session.id)
    .bind(&session.token_digest)
    .bind(user_id.0)
    .bind(&session.browser_binding_digest)
    .bind(session.expires_at as i64)
    .execute(&mut **tx)
    .await?;
    insert_security_event(tx, Some(user_id), SecurityEvent::PasswordLoginSucceeded).await
}
