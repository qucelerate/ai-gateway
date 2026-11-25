//! Organizations' own identity providers. Each save is an immutable
//! configuration row with the next version; the organization's row points at
//! the active configuration (sign-in uses it while enabled) and at a pending
//! one (saved, waiting for its test sign-in). Only sealed client secrets are
//! stored, and nothing here returns them to an admin caller: the sign-in path
//! opens them with the credential cipher.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    credentials::SealedCredential,
    identity::{OrganizationId, UserAccountId},
    identity_provider::ProviderPointers,
};

/// One saved configuration, with its sealed secret.
#[derive(Clone, FromRow)]
pub struct StoredConfiguration {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub version: i64,
    pub issuer: String,
    pub client_id: String,
    pub secret_key_id: String,
    pub secret_ciphertext: Vec<u8>,
    pub extra_scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub tested_at: Option<DateTime<Utc>>,
    pub tested_by: Option<Uuid>,
}

impl fmt::Debug for StoredConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredConfiguration")
            .field("id", &self.id)
            .field("organization_id", &self.organization_id)
            .field("version", &self.version)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// An organization's provider: which configurations it points at.
#[derive(Clone, Debug)]
pub struct ProviderState {
    /// The admin resource id.
    pub id: Uuid,
    pub pointers: ProviderPointers,
    pub active: Option<StoredConfiguration>,
    pub pending: Option<StoredConfiguration>,
}

#[derive(FromRow)]
struct ProviderRow {
    id: Uuid,
    organization_id: Uuid,
    enabled: bool,
    active_configuration_id: Option<Uuid>,
    pending_configuration_id: Option<Uuid>,
}

impl ProviderRow {
    fn pointers(&self) -> ProviderPointers {
        ProviderPointers {
            organization_id: OrganizationId(self.organization_id),
            enabled: self.enabled,
            active: self.active_configuration_id,
            pending: self.pending_configuration_id,
        }
    }
}

const CONFIGURATION_COLUMNS: &str = "c.id, c.organization_id, c.version, c.issuer, c.client_id, \
     c.secret_key_id, c.secret_ciphertext, c.extra_scopes, c.created_at, c.created_by, \
     c.tested_at, c.tested_by";

#[derive(Clone)]
pub struct IdentityProviderStore {
    pool: PgPool,
}

impl IdentityProviderStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's provider, or `None` if it never saved one.
    pub async fn state(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<ProviderState>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::load(&mut conn, organization_id, false).await
    }

    /// The provider and its admin resource version (0 before the first
    /// save), read from one snapshot, so the version is always the one of
    /// the state returned with it.
    pub async fn state_with_version(
        &self,
        organization_id: OrganizationId,
        resource_id: Uuid,
    ) -> Result<(Option<ProviderState>, u64), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT version FROM gateway.admin_resource_version
             WHERE organization_id = $1 AND resource_type = 'identity_provider'
               AND resource_id = $2",
        )
        .bind(organization_id.0)
        .bind(resource_id)
        .fetch_optional(&mut *tx)
        .await?;
        let state = Self::load(&mut tx, organization_id, false).await?;
        tx.commit().await?;
        Ok((
            state,
            version.and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
        ))
    }

    /// The same, locked for the caller's transaction.
    pub async fn state_for_update(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<ProviderState>, sqlx::Error> {
        Self::load(conn, organization_id, true).await
    }

    async fn load(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        lock: bool,
    ) -> Result<Option<ProviderState>, sqlx::Error> {
        let row = sqlx::query_as::<_, ProviderRow>(&format!(
            "SELECT id, organization_id, enabled, active_configuration_id,
                    pending_configuration_id
             FROM gateway.organization_identity_provider
             WHERE organization_id = $1{}",
            if lock { " FOR UPDATE" } else { "" }
        ))
        .bind(organization_id.0)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let active = match row.active_configuration_id {
            Some(id) => Self::configuration_on(conn, organization_id, id).await?,
            None => None,
        };
        let pending = match row.pending_configuration_id {
            Some(id) => Self::configuration_on(conn, organization_id, id).await?,
            None => None,
        };
        Ok(Some(ProviderState {
            id: row.id,
            pointers: row.pointers(),
            active,
            pending,
        }))
    }

    async fn configuration_on(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<StoredConfiguration>, sqlx::Error> {
        sqlx::query_as::<_, StoredConfiguration>(&format!(
            "SELECT {CONFIGURATION_COLUMNS}
             FROM gateway.identity_provider_configuration c
             WHERE c.organization_id = $1 AND c.id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(conn)
        .await
    }

    /// A configuration and what the organization's provider points at now,
    /// for a callback to check that it is still the one it started with.
    pub async fn configuration(
        &self,
        organization_id: OrganizationId,
        configuration_id: Uuid,
    ) -> Result<Option<(StoredConfiguration, ProviderPointers)>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        let Some(state) = Self::load(&mut conn, organization_id, false).await? else {
            return Ok(None);
        };
        let configuration =
            Self::configuration_on(&mut conn, organization_id, configuration_id).await?;
        Ok(configuration.map(|configuration| (configuration, state.pointers)))
    }

    /// The active configuration of the organization with `slug`, if its
    /// provider is enabled. Sign-in to that organization goes through it.
    pub async fn active_for_slug(
        &self,
        slug: &str,
    ) -> Result<Option<StoredConfiguration>, sqlx::Error> {
        sqlx::query_as::<_, StoredConfiguration>(&format!(
            "SELECT {CONFIGURATION_COLUMNS}
             FROM gateway.organization o
             JOIN gateway.organization_identity_provider p ON p.organization_id = o.id
             JOIN gateway.identity_provider_configuration c
               ON c.organization_id = p.organization_id AND c.id = p.active_configuration_id
             WHERE o.slug = $1 AND o.deleted_at IS NULL AND o.suspended_at IS NULL AND p.enabled"
        ))
        .bind(slug)
        .fetch_optional(&self.pool)
        .await
    }

    /// The active configuration of an organization whose provider is enabled.
    pub async fn active_for_organization(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<StoredConfiguration>, sqlx::Error> {
        sqlx::query_as::<_, StoredConfiguration>(&format!(
            "SELECT {CONFIGURATION_COLUMNS}
             FROM gateway.organization_identity_provider p
             JOIN gateway.organization o ON o.id = p.organization_id AND o.suspended_at IS NULL
             JOIN gateway.identity_provider_configuration c
               ON c.organization_id = p.organization_id AND c.id = p.active_configuration_id
             WHERE p.organization_id = $1 AND p.enabled"
        ))
        .bind(organization_id.0)
        .fetch_optional(&self.pool)
        .await
    }

    /// The issuer new invitations to the organization name: its own while its
    /// provider is enabled. On the caller's transaction, which has locked the
    /// organization.
    pub async fn active_issuer(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT c.issuer
             FROM gateway.organization_identity_provider p
             JOIN gateway.identity_provider_configuration c
               ON c.organization_id = p.organization_id AND c.id = p.active_configuration_id
             WHERE p.organization_id = $1 AND p.enabled",
        )
        .bind(organization_id.0)
        .fetch_optional(conn)
        .await
    }

    /// Stores a new configuration with the organization's next version and
    /// returns that version. Pointing the provider at it is separate.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_configuration(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        configuration_id: Uuid,
        issuer: &str,
        client_id: &str,
        secret: &SealedCredential,
        extra_scopes: &[String],
        created_by: UserAccountId,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO gateway.identity_provider_configuration
                 (id, organization_id, version, issuer, client_id, secret_key_id,
                  secret_ciphertext, extra_scopes, created_by)
             SELECT $1, $2,
                    COALESCE((SELECT max(version) FROM gateway.identity_provider_configuration
                              WHERE organization_id = $2), 0) + 1,
                    $3, $4, $5, $6, $7, $8
             RETURNING version",
        )
        .bind(configuration_id)
        .bind(organization_id.0)
        .bind(issuer)
        .bind(client_id)
        .bind(&secret.key_id)
        .bind(&secret.ciphertext)
        .bind(extra_scopes)
        .bind(created_by.0)
        .fetch_one(conn)
        .await
    }

    /// Makes `configuration_id` the provider's pending configuration,
    /// creating the provider (disabled) if it has none. Returns whether it
    /// was created. Whatever was pending before is replaced.
    pub async fn set_pending(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        resource_id: Uuid,
        configuration_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO gateway.organization_identity_provider
                 (organization_id, id, pending_configuration_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (organization_id) DO UPDATE
                 SET pending_configuration_id = EXCLUDED.pending_configuration_id,
                     updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(resource_id)
        .bind(configuration_id)
        .fetch_one(conn)
        .await
    }

    /// Activates the pending configuration `configuration_id` after its test
    /// sign-in by `tested_by`: it becomes the active one and the provider is
    /// enabled. False if it is no longer the pending configuration.
    pub async fn activate(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        configuration_id: Uuid,
        tested_by: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        let changed = sqlx::query(
            "UPDATE gateway.organization_identity_provider
             SET active_configuration_id = pending_configuration_id,
                 pending_configuration_id = NULL,
                 enabled = TRUE,
                 disabled_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND pending_configuration_id = $2",
        )
        .bind(organization_id.0)
        .bind(configuration_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if changed != 1 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE gateway.identity_provider_configuration
             SET tested_at = CURRENT_TIMESTAMP, tested_by = $3
             WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(configuration_id)
        .bind(tested_by.0)
        .execute(conn)
        .await?;
        Ok(true)
    }

    /// Disables the provider: sign-in falls back to the process-wide one,
    /// and its sessions end. A pending configuration is dropped too, so no
    /// test still in flight can turn it back on. Configurations, users and
    /// memberships are kept.
    pub async fn disable(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE gateway.organization_identity_provider
             SET enabled = FALSE, pending_configuration_id = NULL,
                 disabled_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Ends a test sign-in's login transaction on the caller's transaction,
    /// so it commits with the activation it led to. False if the claim was
    /// lost.
    pub async fn complete_test_login(
        conn: &mut PgConnection,
        transaction_id: Uuid,
        claim_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.oidc_login_transaction
             SET status = 'completed', consumed_at = CURRENT_TIMESTAMP,
                 completed_at = CURRENT_TIMESTAMP, claim_lease_until = NULL
             WHERE id = $1 AND claim_id = $2 AND status = 'exchange_started'
               AND purpose = 'provider_test'",
        )
        .bind(transaction_id)
        .bind(claim_id)
        .execute(conn)
        .await?
        .rows_affected()
            == 1)
    }
}
