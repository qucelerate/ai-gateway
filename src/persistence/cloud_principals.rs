//! Operators' bindings of cloud principals to organizations. A credential
//! that has the gateway act as an Entra directory or a Google service
//! account is used only while its principal is bound to its organization;
//! see [`crate::domain::credentials::CloudPrincipal`]. Each change is
//! audited in the organization, by the system actor: the operator acts
//! from the command line, outside any organization.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::admin::{AdminStoreError, insert_audit_on};
use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    credentials::CloudPrincipal,
    identity::OrganizationId,
};

#[derive(Clone)]
pub struct CloudPrincipalStore {
    pool: PgPool,
}

/// One binding, as an operator lists it.
#[derive(Debug, FromRow)]
pub struct CloudPrincipalBinding {
    pub kind: String,
    pub principal: String,
    pub created_at: DateTime<Utc>,
}

impl CloudPrincipalStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Binds `principal` to the organization. Returns false when it already
    /// was; then nothing is audited.
    pub async fn allow(
        &self,
        organization_id: OrganizationId,
        principal: &CloudPrincipal,
    ) -> Result<bool, AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO gateway.cloud_principal_binding
                 (id, organization_id, kind, principal, principal_digest)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (organization_id, kind, principal) DO NOTHING
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(principal.kind())
        .bind(principal.value())
        .bind(principal.digest())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Ok(false);
        };
        audit(
            &mut tx,
            organization_id,
            id,
            "cloud_principal_binding.created",
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Unbinds `principal` from the organization: its credentials stop
    /// working at the next request. Returns false when it was not bound.
    pub async fn revoke(
        &self,
        organization_id: OrganizationId,
        principal: &CloudPrincipal,
    ) -> Result<bool, AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        let id: Option<Uuid> = sqlx::query_scalar(
            "DELETE FROM gateway.cloud_principal_binding
             WHERE organization_id = $1 AND kind = $2 AND principal = $3
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(principal.kind())
        .bind(principal.value())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Ok(false);
        };
        audit(
            &mut tx,
            organization_id,
            id,
            "cloud_principal_binding.revoked",
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The organization's bindings, oldest first.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<CloudPrincipalBinding>, sqlx::Error> {
        sqlx::query_as(
            "SELECT kind, principal, created_at FROM gateway.cloud_principal_binding
             WHERE organization_id = $1 ORDER BY created_at, kind, principal",
        )
        .bind(organization_id.0)
        .fetch_all(&self.pool)
        .await
    }
}

/// The audit event names the binding by id only: the principal itself is
/// not recorded in the audit log.
async fn audit(
    conn: &mut sqlx::PgConnection,
    organization_id: OrganizationId,
    id: Uuid,
    action: &'static str,
) -> Result<(), AdminStoreError> {
    insert_audit_on(
        conn,
        &AdminAuditEvent::new(
            AuditActor::System,
            organization_id,
            None,
            AuditTargetResource::CloudPrincipalBinding,
            AuditAction::new(action).expect("audit action constant is valid"),
            id,
            AuditOutcome::Succeeded,
            Uuid::new_v4(),
            None,
            None,
        ),
    )
    .await
}
