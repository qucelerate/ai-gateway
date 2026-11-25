//! The deployment as a whole, above its organizations: deployment admins
//! create, suspend, delete and restore organizations without seeing inside
//! them, and every such change is recorded in the deployment audit log.
//!
//! An organization is active, suspended (every way in refused; see the
//! `suspended_at IS NULL` checks of API keys, agent tokens, sessions,
//! invitations and organization identity providers) or deleted (only from
//! suspended; hidden, kept, restorable). Deletion keeps every secret the
//! organization stored, sealed and unused, so a restore brings it back
//! whole; nothing reaches them while it is suspended.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    domain::{
        audit::AuditActor,
        identity::{MembershipRole, OrganizationId, UserAccountId},
    },
    persistence::{
        bootstrap::{BootstrapError, grant_membership, seed_version},
        invitations::{InvitationStore, InvitationToken, audit as invitation_audit},
        local_accounts::{insert_local_user, insert_reset_link},
    },
};

/// An organization as a deployment admin sees it: its name and state, never
/// its contents.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct OrganizationSummary {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub deletion_requested_at: Option<DateTime<Utc>>,
    pub members: i64,
    /// The secrets it keeps stored, counted, never shown.
    #[sqlx(flatten)]
    pub stored: StoredSecrets,
}

/// What an organization keeps stored, sealed, through deletion: counts only.
/// Its administrators revoke these at each provider if it will not be
/// restored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct StoredSecrets {
    /// Provider credentials holding a key.
    pub provider_credentials: i64,
    /// Provider credentials acting as a cloud principal (a role, an app
    /// registration, a service account).
    pub cloud_connections: i64,
    pub webhook_signing_secrets: i64,
    /// Whether any of its identity-provider configurations holds a client
    /// secret, in use or not.
    pub identity_provider_client_secret: bool,
}

/// `StoredSecrets` columns for organization `o`.
const STORED: &str = "(SELECT count(*) FROM gateway.provider_credential c
          WHERE c.organization_id = o.id AND c.principal_digest IS NULL) AS provider_credentials,
        (SELECT count(*) FROM gateway.provider_credential c
          WHERE c.organization_id = o.id AND c.principal_digest IS NOT NULL) AS cloud_connections,
        (SELECT count(*) FROM gateway.webhook_endpoint w
          WHERE w.organization_id = o.id) AS webhook_signing_secrets,
        -- Every configuration ever saved keeps its sealed secret, in use or
        -- not (a disabled provider's, a superseded one's).
        EXISTS (SELECT 1 FROM gateway.identity_provider_configuration i
                WHERE i.organization_id = o.id) AS identity_provider_client_secret";

const SUMMARY: &str = "SELECT o.id, o.slug, o.name,
        CASE WHEN o.deleted_at IS NOT NULL THEN 'deleted'
             WHEN o.suspended_at IS NOT NULL THEN 'suspended'
             ELSE 'active' END AS status,
        o.created_at, o.suspended_at, o.deleted_at, o.deletion_requested_at,
        (SELECT count(*) FROM gateway.organization_membership m
         JOIN gateway.user_account u ON u.id = m.user_id
         WHERE m.organization_id = o.id AND m.enabled
           AND NOT EXISTS (SELECT 1 FROM gateway.agent a WHERE a.user_id = u.id)) AS members,
        STORED
    FROM gateway.organization o";

fn summary() -> String {
    SUMMARY.replace("STORED", STORED)
}

/// A deployment audit record.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct DeploymentEvent {
    pub id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub actor_user_id: Option<Uuid>,
    pub actor_email: Option<String>,
    pub action: String,
    pub organization_id: Option<Uuid>,
    pub target_user_id: Option<Uuid>,
    pub detail: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("no such organization")]
    NotFound,
    /// A live organization has the slug.
    #[error("the slug is taken")]
    SlugTaken,
    /// Not in a state that allows it (delete needs suspended; unsuspend,
    /// suspend and delete need not deleted; restore needs deleted).
    #[error("the organization is {0}")]
    WrongState(&'static str),
    #[error("the confirmation does not match the slug")]
    ConfirmationMismatch,
    #[error("the last organization cannot be deleted")]
    LastOrganization,
    #[error("the owner's account is disabled or not a person")]
    OwnerUnavailable,
    #[error(transparent)]
    Store(#[from] sqlx::Error),
    #[error("the membership could not be granted")]
    Grant,
}

impl From<BootstrapError> for LifecycleError {
    fn from(error: BootstrapError) -> Self {
        match error {
            BootstrapError::Store(crate::persistence::admin::AdminStoreError::Database(error)) => {
                Self::Store(error)
            }
            _ => Self::Grant,
        }
    }
}

/// The first owner of a new organization, given as a local account.
pub enum LocalOwner<'a> {
    /// The local account with this email, made now if missing (with
    /// `unusable_hash`, so it signs in only after setting a password).
    Email {
        email: &'a str,
        unusable_hash: &'a str,
    },
}

/// An owner invitation made with the organization.
pub struct OwnerInvitation<'a> {
    pub issuer: &'a str,
    pub valid_for: chrono::Duration,
}

/// What [`DeploymentStore::create`] makes, all in one transaction.
pub struct NewOrganization<'a> {
    pub name: &'a str,
    pub slug: &'a str,
    /// A local account as its first owner.
    pub local_owner: Option<LocalOwner<'a>>,
    /// How long a new local owner's set-password link works.
    pub set_password_valid_for: chrono::Duration,
    /// An existing person as its first owner (self-serve creation).
    pub owner_user: Option<UserAccountId>,
    /// An invitation for its first owner.
    pub owner_invitation: Option<OwnerInvitation<'a>>,
    pub action: &'static str,
}

/// What creating an organization did (tokens included, so no `Debug`).
pub struct Created {
    pub organization_id: OrganizationId,
    /// The local owner, and whether their account is new (it then needs a
    /// set-password link).
    pub owner: Option<(UserAccountId, bool)>,
    /// The new local owner's set-password link token.
    pub set_password: Option<InvitationToken>,
    /// The owner invitation's token.
    pub invitation: Option<InvitationToken>,
}

#[derive(Clone, Debug)]
pub struct DeploymentStore {
    pool: PgPool,
}

impl DeploymentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn is_admin(&self, user_id: UserAccountId) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.deployment_admin d
             JOIN gateway.user_account u ON u.id = d.user_id AND u.enabled
             WHERE d.user_id = $1)",
        )
        .bind(user_id.0)
        .fetch_one(&self.pool)
        .await
    }

    /// Every organization, by slug; deleted ones only with `deleted`.
    pub async fn organizations(
        &self,
        deleted: bool,
    ) -> Result<Vec<OrganizationSummary>, sqlx::Error> {
        sqlx::query_as::<_, OrganizationSummary>(&format!(
            "{} WHERE ($1 OR o.deleted_at IS NULL) ORDER BY o.deleted_at IS NOT NULL, o.slug, o.id",
            summary()
        ))
        .bind(deleted)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn organization(&self, id: Uuid) -> Result<Option<OrganizationSummary>, sqlx::Error> {
        sqlx::query_as::<_, OrganizationSummary>(&format!("{} WHERE o.id = $1", summary()))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// A new organization, optionally with a local account as its first
    /// owner. `actor` is the deployment admin (or, for self-serve creation,
    /// the person, who then becomes its owner through `owner_user`).
    /// Nothing is kept unless all of it is made, so a failure leaves no
    /// owner-less organization behind holding the slug.
    pub async fn create(
        &self,
        actor: Option<UserAccountId>,
        new: NewOrganization<'_>,
    ) -> Result<Created, LifecycleError> {
        let NewOrganization {
            name,
            slug,
            local_owner,
            set_password_valid_for,
            owner_user,
            owner_invitation,
            action,
        } = new;
        let mut tx = self.pool.begin().await?;
        let id = Uuid::new_v4();
        let inserted = sqlx::query(
            "INSERT INTO gateway.organization (id, slug, name) VALUES ($1, $2, $3)
             ON CONFLICT (slug) WHERE deleted_at IS NULL DO NOTHING",
        )
        .bind(id)
        .bind(slug)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Err(LifecycleError::SlugTaken);
        }
        seed_version(&mut *tx, id, "organization", id)
            .await
            .map_err(|_| LifecycleError::Grant)?;
        // The organization's own audit log names only its members: a
        // deployment admin's grant is the system's there (and the admin's in
        // the deployment log).
        let audit_actor = AuditActor::System;
        let mut owner = None;
        if let Some(LocalOwner::Email {
            email,
            unusable_hash,
        }) = local_owner
        {
            let existing: Option<(Uuid, bool)> = sqlx::query_as(
                "SELECT u.id, u.enabled FROM gateway.local_credential c
                 JOIN gateway.user_account u ON u.id = c.user_id WHERE c.email = $1",
            )
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
            let (user_id, created) = match existing {
                Some((_, false)) => return Err(LifecycleError::OwnerUnavailable),
                Some((user_id, true)) => (UserAccountId(user_id), false),
                None => (
                    insert_local_user(&mut tx, email, None, unusable_hash).await?,
                    true,
                ),
            };
            grant_membership(
                &mut tx,
                id,
                user_id.0,
                MembershipRole::Owner,
                audit_actor.clone(),
                "role_binding.first_owner",
            )
            .await?;
            owner = Some((user_id, created));
        }
        if let Some(user_id) = owner_user {
            grant_membership(
                &mut tx,
                id,
                user_id.0,
                MembershipRole::Owner,
                AuditActor::User(user_id),
                "role_binding.first_owner",
            )
            .await?;
        }
        record(
            &mut tx,
            actor,
            action,
            Some(id),
            owner
                .map(|(user, _)| user.0)
                .or(owner_user.map(|user| user.0)),
            serde_json::json!({ "slug": slug, "name": name }),
        )
        .await?;
        let set_password = match owner {
            Some((user_id, true)) => {
                Some(insert_reset_link(&mut tx, user_id, set_password_valid_for, None).await?)
            }
            _ => None,
        };
        let invitation = match owner_invitation {
            Some(OwnerInvitation { issuer, valid_for }) => {
                let invitation_id = Uuid::new_v4();
                let token = InvitationStore::insert(
                    &mut tx,
                    OrganizationId(id),
                    invitation_id,
                    MembershipRole::Owner,
                    issuer,
                    valid_for,
                    // Not a member: the deployment log names the admin.
                    None,
                )
                .await?;
                invitation_audit(
                    &mut tx,
                    AuditActor::System,
                    id,
                    invitation_id,
                    "invitation.created",
                )
                .await
                .map_err(|_| LifecycleError::Grant)?;
                Some(token)
            }
            None => None,
        };
        tx.commit().await?;
        Ok(Created {
            organization_id: OrganizationId(id),
            owner,
            set_password,
            invitation,
        })
    }

    /// Suspends an active organization: every way into it is refused until
    /// it is unsuspended.
    pub async fn suspend(&self, actor: UserAccountId, id: Uuid) -> Result<(), LifecycleError> {
        self.transition(
            actor,
            id,
            "UPDATE gateway.organization SET suspended_at = CURRENT_TIMESTAMP, suspended_by = $2
             WHERE id = $1 AND deleted_at IS NULL AND suspended_at IS NULL
             RETURNING id",
            "organization.suspended",
        )
        .await
    }

    /// Lifts a suspension. Nothing inside the organization changed while it
    /// was suspended (or deleted): its keys, sessions, webhooks and identity
    /// provider are refused by its state alone, so they work again exactly
    /// as they were.
    pub async fn unsuspend(&self, actor: UserAccountId, id: Uuid) -> Result<(), LifecycleError> {
        self.transition(
            actor,
            id,
            "UPDATE gateway.organization SET suspended_at = NULL, suspended_by = NULL
             WHERE id = $1 AND deleted_at IS NULL AND suspended_at IS NOT NULL
               AND $2::UUID IS NOT NULL
             RETURNING id",
            "organization.unsuspended",
        )
        .await
    }

    async fn transition(
        &self,
        actor: UserAccountId,
        id: Uuid,
        sql: &str,
        action: &'static str,
    ) -> Result<(), LifecycleError> {
        let mut tx = self.pool.begin().await?;
        let state = lock(&mut tx, id).await?;
        // `sql` returns the organization's id when it changed it.
        let changed: Option<Uuid> = sqlx::query_scalar(sql)
            .bind(id)
            .bind(actor.0)
            .fetch_optional(&mut *tx)
            .await?;
        if changed.is_none() {
            return Err(LifecycleError::WrongState(state.status()));
        }
        record(
            &mut tx,
            Some(actor),
            action,
            Some(id),
            None,
            serde_json::json!({ "slug": state.slug, "name": state.name }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes a suspended organization: hidden everywhere, its slug and
    /// email domains freed, its pending invitations revoked. Its secrets stay
    /// stored, sealed, for a restore; the audit record counts them. `confirm` must be its slug. The last
    /// organization of the deployment cannot be deleted.
    pub async fn delete(
        &self,
        actor: UserAccountId,
        id: Uuid,
        confirm: &str,
    ) -> Result<(), LifecycleError> {
        let mut tx = self.pool.begin().await?;
        // Every live organization, so two deletes cannot both pass the
        // last-organization check.
        sqlx::query("SELECT id FROM gateway.organization WHERE deleted_at IS NULL FOR UPDATE")
            .execute(&mut *tx)
            .await?;
        let state = lock(&mut tx, id).await?;
        if state.deleted {
            return Err(LifecycleError::WrongState("deleted"));
        }
        if !state.suspended {
            return Err(LifecycleError::WrongState("active"));
        }
        if confirm != state.slug {
            return Err(LifecycleError::ConfirmationMismatch);
        }
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.organization WHERE deleted_at IS NULL",
        )
        .fetch_one(&mut *tx)
        .await?;
        if live <= 1 {
            return Err(LifecycleError::LastOrganization);
        }
        sqlx::query(
            "UPDATE gateway.organization
             SET deleted_at = CURRENT_TIMESTAMP, deleted_by = $2,
                 deletion_requested_at = NULL, deletion_requested_by = NULL
             WHERE id = $1",
        )
        .bind(id)
        .bind(actor.0)
        .execute(&mut *tx)
        .await?;
        let stored = stored_secrets(&mut tx, id).await?;
        sqlx::query(
            "UPDATE gateway.organization_domain SET deleted_at = CURRENT_TIMESTAMP, deleted_by = $2
             WHERE organization_id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(actor.0)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE gateway.organization_invitation SET revoked_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            Some(actor),
            "organization.deleted",
            Some(id),
            None,
            serde_json::json!({ "slug": state.slug, "name": state.name, "secrets_kept": stored }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Restores a deleted organization, still suspended, under its slug or,
    /// when a live organization took that, under `new_slug`.
    pub async fn restore(
        &self,
        actor: UserAccountId,
        id: Uuid,
        new_slug: Option<&str>,
    ) -> Result<String, LifecycleError> {
        let mut tx = self.pool.begin().await?;
        let state = lock(&mut tx, id).await?;
        if !state.deleted {
            return Err(LifecycleError::WrongState(state.status()));
        }
        let slug = new_slug.unwrap_or(&state.slug);
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.organization WHERE slug = $1 AND deleted_at IS NULL)",
        )
        .bind(slug)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(LifecycleError::SlugTaken);
        }
        sqlx::query(
            "UPDATE gateway.organization SET deleted_at = NULL, deleted_by = NULL, slug = $2
             WHERE id = $1",
        )
        .bind(id)
        .bind(slug)
        .execute(&mut *tx)
        .await
        .map_err(|error| match &error {
            // Taken meanwhile, by a create or restore that committed first.
            sqlx::Error::Database(database)
                if database.constraint() == Some("organization_slug_live_uq") =>
            {
                LifecycleError::SlugTaken
            }
            _ => LifecycleError::Store(error),
        })?;
        record(
            &mut tx,
            Some(actor),
            "organization.restored",
            Some(id),
            None,
            serde_json::json!({ "slug": slug, "previous_slug": state.slug, "name": state.name }),
        )
        .await?;
        tx.commit().await?;
        Ok(slug.to_owned())
    }

    /// An owner asks the deployment's admins to delete the organization
    /// (`request`), or withdraws the request.
    pub async fn request_deletion(
        &self,
        actor: UserAccountId,
        id: OrganizationId,
        request: bool,
    ) -> Result<(), LifecycleError> {
        let mut tx = self.pool.begin().await?;
        let state = lock(&mut tx, id.0).await?;
        if state.deleted {
            return Err(LifecycleError::NotFound);
        }
        let changed = sqlx::query(
            "UPDATE gateway.organization
             SET deletion_requested_at = CASE WHEN $2 THEN CURRENT_TIMESTAMP END,
                 deletion_requested_by = CASE WHEN $2 THEN $3::UUID END
             WHERE id = $1 AND (deletion_requested_at IS NULL) = $2",
        )
        .bind(id.0)
        .bind(request)
        .bind(actor.0)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 1 {
            record(
                &mut tx,
                Some(actor),
                if request {
                    "organization.deletion_requested"
                } else {
                    "organization.deletion_request_withdrawn"
                },
                Some(id.0),
                None,
                serde_json::json!({ "slug": state.slug, "name": state.name }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn deletion_requested(
        &self,
        id: OrganizationId,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        sqlx::query_scalar("SELECT deletion_requested_at FROM gateway.organization WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map(Option::flatten)
    }

    /// The deployment audit log, newest first.
    pub async fn events(
        &self,
        limit: i64,
        before: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<Vec<DeploymentEvent>, sqlx::Error> {
        sqlx::query_as::<_, DeploymentEvent>(
            "SELECT e.id, e.occurred_at, e.actor_user_id, u.email AS actor_email, e.action,
                    e.organization_id, e.target_user_id, e.detail
             FROM gateway.deployment_audit_event e
             LEFT JOIN gateway.user_account u ON u.id = e.actor_user_id
             WHERE ($2::TIMESTAMPTZ IS NULL OR (e.occurred_at, e.id) < ($2, $3))
             ORDER BY e.occurred_at DESC, e.id DESC
             LIMIT $1",
        )
        .bind(limit)
        .bind(before.map(|(at, _)| at))
        .bind(before.map(|(_, id)| id))
        .fetch_all(&self.pool)
        .await
    }
}

struct Locked {
    slug: String,
    name: String,
    suspended: bool,
    deleted: bool,
}

impl Locked {
    fn status(&self) -> &'static str {
        if self.deleted {
            "deleted"
        } else if self.suspended {
            "suspended"
        } else {
            "active"
        }
    }
}

async fn lock(tx: &mut PgConnection, id: Uuid) -> Result<Locked, LifecycleError> {
    let row: Option<(String, String, bool, bool)> = sqlx::query_as(
        "SELECT slug, name, suspended_at IS NOT NULL, deleted_at IS NOT NULL
         FROM gateway.organization WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (slug, name, suspended, deleted) = row.ok_or(LifecycleError::NotFound)?;
    Ok(Locked {
        slug,
        name,
        suspended,
        deleted,
    })
}

/// What organization `id` keeps stored.
pub(crate) async fn stored_secrets(
    tx: &mut PgConnection,
    id: Uuid,
) -> Result<StoredSecrets, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {STORED} FROM gateway.organization o WHERE o.id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await
}

/// A deployment audit record, in the caller's transaction.
pub(crate) async fn record(
    tx: &mut PgConnection,
    actor: Option<UserAccountId>,
    action: &'static str,
    organization_id: Option<Uuid>,
    target_user_id: Option<Uuid>,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gateway.deployment_audit_event
             (id, actor_user_id, action, organization_id, target_user_id, detail)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(actor.map(|actor| actor.0))
    .bind(action)
    .bind(organization_id)
    .bind(target_user_id)
    .bind(detail)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
