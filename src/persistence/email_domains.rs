//! Email domains organizations claim for sign-in. See
//! [`crate::domain::email_domains`].

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    domain::{
        audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
        identity::{OrganizationId, UserAccountId},
    },
    persistence::admin::{AdminStoreError, insert_audit_on},
};

/// A domain as an owner sees it.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct StoredDomain {
    pub id: Uuid,
    pub domain: String,
    #[serde(skip)]
    pub verification_token: String,
    pub created_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// The organization lists the domain already.
    #[error("the organization lists this domain already")]
    Exists,
    /// Another organization verified it first.
    #[error("another organization owns this domain")]
    Claimed,
    #[error("no such domain")]
    NotFound,
    #[error(transparent)]
    Store(#[from] sqlx::Error),
    #[error("the audit event could not be written")]
    Audit,
}

impl From<AdminStoreError> for DomainError {
    fn from(_: AdminStoreError) -> Self {
        Self::Audit
    }
}

/// Where an email domain's sign-in goes: the organization that verified it.
#[derive(Clone, Debug)]
pub struct DomainOwner {
    pub organization_id: OrganizationId,
}

const COLUMNS: &str = "id, domain, verification_token, created_at, verified_at, last_checked_at";

#[derive(Clone, Debug)]
pub struct EmailDomainStore {
    pool: PgPool,
}

impl EmailDomainStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's domains, not removed, by name.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<StoredDomain>, sqlx::Error> {
        sqlx::query_as::<_, StoredDomain>(&format!(
            "SELECT {COLUMNS} FROM gateway.organization_domain
             WHERE organization_id = $1 AND deleted_at IS NULL
             ORDER BY domain"
        ))
        .bind(organization_id.0)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get(
        &self,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<StoredDomain>, sqlx::Error> {
        sqlx::query_as::<_, StoredDomain>(&format!(
            "SELECT {COLUMNS} FROM gateway.organization_domain
             WHERE organization_id = $1 AND id = $2 AND deleted_at IS NULL"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Adds an unverified domain with a fresh verification token. Refused
    /// when the organization lists it already or another one verified it.
    pub async fn add(
        &self,
        organization_id: OrganizationId,
        actor: UserAccountId,
        domain: &str,
    ) -> Result<StoredDomain, DomainError> {
        let mut transaction = self.pool.begin().await?;
        let claimed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.organization_domain
             WHERE domain = $1 AND verified_at IS NOT NULL AND deleted_at IS NULL
               AND organization_id <> $2)",
        )
        .bind(domain)
        .bind(organization_id.0)
        .fetch_one(&mut *transaction)
        .await?;
        if claimed {
            return Err(DomainError::Claimed);
        }
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let inserted = sqlx::query_as::<_, StoredDomain>(&format!(
            "INSERT INTO gateway.organization_domain
                 (id, organization_id, domain, verification_token, created_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (organization_id, domain) WHERE deleted_at IS NULL DO NOTHING
             RETURNING {COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(domain)
        .bind(&token)
        .bind(actor.0)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(stored) = inserted else {
            return Err(DomainError::Exists);
        };
        audit(
            &mut transaction,
            organization_id,
            actor,
            "organization_domain.added",
            stored.id,
        )
        .await?;
        transaction.commit().await?;
        Ok(stored)
    }

    /// Records a verification attempt: verified when `proven`, else only
    /// checked. A domain another organization verified meanwhile is
    /// [`DomainError::Claimed`].
    pub async fn record_check(
        &self,
        organization_id: OrganizationId,
        actor: UserAccountId,
        id: Uuid,
        proven: bool,
    ) -> Result<StoredDomain, DomainError> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query_as::<_, StoredDomain>(&format!(
            "UPDATE gateway.organization_domain
             SET last_checked_at = CURRENT_TIMESTAMP,
                 verified_at = CASE WHEN $3 THEN COALESCE(verified_at, CURRENT_TIMESTAMP)
                                    ELSE verified_at END
             WHERE organization_id = $1 AND id = $2 AND deleted_at IS NULL
             RETURNING {COLUMNS}"
        ))
        .bind(organization_id.0)
        .bind(id)
        .bind(proven)
        .fetch_optional(&mut *transaction)
        .await;
        let stored = match result {
            Ok(Some(stored)) => stored,
            Ok(None) => return Err(DomainError::NotFound),
            Err(sqlx::Error::Database(error))
                if error.constraint() == Some("organization_domain_verified_uq") =>
            {
                return Err(DomainError::Claimed);
            }
            Err(error) => return Err(error.into()),
        };
        if proven {
            audit(
                &mut transaction,
                organization_id,
                actor,
                "organization_domain.verified",
                id,
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(stored)
    }

    /// Removes a domain from the organization (kept, marked deleted). Sign-in
    /// stops routing it here at once.
    pub async fn remove(
        &self,
        organization_id: OrganizationId,
        actor: UserAccountId,
        id: Uuid,
    ) -> Result<(), DomainError> {
        let mut transaction = self.pool.begin().await?;
        let removed = sqlx::query(
            "UPDATE gateway.organization_domain
             SET deleted_at = CURRENT_TIMESTAMP, deleted_by = $3
             WHERE organization_id = $1 AND id = $2 AND deleted_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(actor.0)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if removed == 0 {
            return Err(DomainError::NotFound);
        }
        audit(
            &mut transaction,
            organization_id,
            actor,
            "organization_domain.removed",
            id,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// The organization that verified `domain`, if any.
    pub async fn owner(&self, domain: &str) -> Result<Option<DomainOwner>, sqlx::Error> {
        let organization: Option<Uuid> = sqlx::query_scalar(
            "SELECT d.organization_id FROM gateway.organization_domain d
             JOIN gateway.organization o ON o.id = d.organization_id AND o.suspended_at IS NULL
             WHERE d.domain = $1 AND d.verified_at IS NOT NULL AND d.deleted_at IS NULL",
        )
        .bind(domain)
        .fetch_optional(&self.pool)
        .await?;
        Ok(organization.map(|id| DomainOwner {
            organization_id: OrganizationId(id),
        }))
    }
}

async fn audit(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: OrganizationId,
    actor: UserAccountId,
    action: &'static str,
    target: Uuid,
) -> Result<(), DomainError> {
    insert_audit_on(
        transaction,
        &AdminAuditEvent::new(
            AuditActor::User(actor),
            organization_id,
            None,
            AuditTargetResource::OrganizationDomain,
            AuditAction::new(action).expect("audit action constant is valid"),
            target,
            AuditOutcome::Succeeded,
            Uuid::new_v4(),
            None,
            None,
        ),
    )
    .await
    .map_err(DomainError::from)
}
