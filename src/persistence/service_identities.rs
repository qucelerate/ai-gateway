// Service-account and API-key persistence used by the service-identity admin
// lane.
//
// Read queries are always scope-clamped by the caller: `None` means the actor
// holds an org-wide grant (no team restriction) and `Some(teams)` clamps the
// collection to exactly those teams. Every mutation executes on the foundation
// mutation coordinator's transaction so authorization, typed audit,
// idempotency, and resource-version semantics stay shared with the rest of the
// admin surface. No method here ever reads or writes a bearer/digest: key
// material is created only by the shared identity primitive, which returns the
// plaintext exactly once and persists only its HMAC digest.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::identity::{
    ApiKeyId, ApiKeyScope, CreatedApiKey, OrganizationId, ServiceAccountId, TeamId, UserAccountId,
};
use crate::domain::keys::KeyError;
use crate::persistence::{IdentityError, IdentityStore};

#[derive(Debug, Error)]
pub enum ServiceIdentityStoreError {
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("identity store operation failed: {0}")]
    Identity(#[from] IdentityError),
    #[error("key operation failed: {0}")]
    Key(#[from] KeyError),
    #[error("resource does not exist in the requested scope")]
    NotFound,
    #[error("resource state does not permit this operation")]
    InvalidState,
    #[error("service account still has active or unexpired API keys")]
    ActiveApiKeysExist,
}

/// What deleting a service account did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceAccountRemoval {
    Deleted,
    /// It has made requests, so it is kept, disabled, with its usage: the
    /// account as the transaction left it.
    Disabled(ServiceAccountRecord),
}

/// Stable cursor value used by both collections. The keyset is
/// `(created_at, id)` so ordering stays stable and unambiguous even when two
/// rows share the same creation instant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Keyset {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

impl Keyset {
    pub fn to_claim(&self) -> String {
        format!("{}|{}", self.created_at.to_rfc3339(), self.id)
    }

    pub fn parse(claim: &str) -> Result<Self, ServiceIdentityStoreError> {
        let (created_at, id) = claim
            .split_once('|')
            .ok_or(ServiceIdentityStoreError::InvalidState)?;
        let created_at = DateTime::parse_from_rfc3339(created_at)
            .map_err(|_| ServiceIdentityStoreError::InvalidState)?
            .with_timezone(&Utc);
        let id = Uuid::parse_str(id).map_err(|_| ServiceIdentityStoreError::InvalidState)?;
        Ok(Self { created_at, id })
    }
}

/// Public, secret-free service-account projection returned by reads and
/// mutations. The digest/bearer are never present, and the resource version is
/// always the optimistic-concurrency version from `admin_resource_version`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceAccountRecord {
    pub id: ServiceAccountId,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub owner_user_id: UserAccountId,
    pub name: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub version: u64,
    /// Lists only: its keys that can be used now (active, not expired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_keys: Option<u64>,
    /// Lists only: when any of its keys was last used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
}

impl ServiceAccountRecord {
    pub fn keyset(&self) -> Keyset {
        Keyset {
            created_at: self.created_at,
            id: self.id.0,
        }
    }
}

/// Public, secret-free API-key projection. Only the prefix, scopes, expiry,
/// lifecycle state, last-used time, and version are exposed; never the digest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApiKeyRecord {
    pub id: ApiKeyId,
    pub organization_id: OrganizationId,
    pub service_account_id: ServiceAccountId,
    pub successor_of: Option<ApiKeyId>,
    pub public_prefix: String,
    pub scopes: Vec<ApiKeyScope>,
    pub status: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub version: u64,
}

impl ApiKeyRecord {
    pub fn keyset(&self) -> Keyset {
        Keyset {
            created_at: self.created_at,
            id: self.id.0,
        }
    }
}

/// Persistence for the service-identity lane. Holds the shared `IdentityStore`
/// because the only key-material-producing primitive (`create_api_key_*`)
/// lives there; this store adds coordinator-transaction mutations and
/// scope-clamped reads without duplicating key generation.
#[derive(Clone)]
pub struct ServiceIdentityStore {
    pool: PgPool,
    identity: IdentityStore,
}

impl ServiceIdentityStore {
    pub fn new(pool: PgPool, identity: IdentityStore) -> Self {
        Self { pool, identity }
    }

    // ---- service accounts: reads ----

    /// Cursor-list service accounts. `authorized_teams` is `None` for an
    /// org-wide grant (all teams plus org-level rows) or the exact set of
    /// teams the actor may read. `keyset` continues a previous page.
    pub async fn list_service_accounts(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        enabled: Option<bool>,
        page_size: u32,
        keyset: Option<Keyset>,
    ) -> Result<(Vec<ServiceAccountRecord>, bool), ServiceIdentityStoreError> {
        let limit: i32 = (page_size + 1) as i32;
        let (created_at, id) = match keyset {
            Some(keyset) => (Some(keyset.created_at), Some(keyset.id)),
            None => (None, None),
        };
        let rows = match authorized_teams {
            None => {
                sqlx::query_as::<_, ServiceAccountViewRow>(
                    "SELECT sa.id, sa.organization_id, sa.team_id, sa.owner_user_id, sa.name,
                            sa.enabled, sa.created_at, v.version,
                            keys.active_keys, keys.last_used_at
                     FROM gateway.service_account sa
                     JOIN gateway.admin_resource_version v
                       ON v.organization_id = sa.organization_id
                      AND v.resource_type = 'service_account'
                      AND v.resource_id = sa.id
                     CROSS JOIN LATERAL (
                       SELECT count(*) FILTER (
                                WHERE k.status = 'active'
                                  AND (k.expires_at IS NULL OR k.expires_at > now())
                              ) AS active_keys,
                              max(k.last_used_at) AS last_used_at
                       FROM gateway.api_key k
                       WHERE k.organization_id = sa.organization_id
                         AND k.service_account_id = sa.id
                     ) keys
                     WHERE sa.organization_id = $1
                       AND ($2::timestamptz IS NULL
                            OR (sa.created_at, sa.id) > ($2::timestamptz, $3::uuid))
                       AND ($5::boolean IS NULL OR sa.enabled = $5)
                     ORDER BY sa.created_at ASC, sa.id ASC
                     LIMIT $4",
                )
                .bind(organization_id.0)
                .bind(created_at)
                .bind(id)
                .bind(limit)
                .bind(enabled)
                .fetch_all(&self.pool)
                .await?
            }
            Some(teams) => {
                let team_ids: Vec<Uuid> = teams.iter().map(|team| team.0).collect();
                sqlx::query_as::<_, ServiceAccountViewRow>(
                    "SELECT sa.id, sa.organization_id, sa.team_id, sa.owner_user_id, sa.name,
                            sa.enabled, sa.created_at, v.version,
                            keys.active_keys, keys.last_used_at
                     FROM gateway.service_account sa
                     JOIN gateway.admin_resource_version v
                       ON v.organization_id = sa.organization_id
                      AND v.resource_type = 'service_account'
                      AND v.resource_id = sa.id
                     CROSS JOIN LATERAL (
                       SELECT count(*) FILTER (
                                WHERE k.status = 'active'
                                  AND (k.expires_at IS NULL OR k.expires_at > now())
                              ) AS active_keys,
                              max(k.last_used_at) AS last_used_at
                       FROM gateway.api_key k
                       WHERE k.organization_id = sa.organization_id
                         AND k.service_account_id = sa.id
                     ) keys
                     WHERE sa.organization_id = $1
                       AND sa.team_id = ANY($2)
                       AND ($3::timestamptz IS NULL
                            OR (sa.created_at, sa.id) > ($3::timestamptz, $4::uuid))
                       AND ($6::boolean IS NULL OR sa.enabled = $6)
                     ORDER BY sa.created_at ASC, sa.id ASC
                     LIMIT $5",
                )
                .bind(organization_id.0)
                .bind(&team_ids)
                .bind(created_at)
                .bind(id)
                .bind(limit)
                .bind(enabled)
                .fetch_all(&self.pool)
                .await?
            }
        };
        let has_more = rows.len() as u32 > page_size;
        let items: Vec<ServiceAccountRecord> = rows
            .into_iter()
            .take(page_size as usize)
            .map(Into::into)
            .collect();
        // A keyset continuation page is only reachable when it has rows.
        if keyset.is_some() && items.is_empty() && !has_more {
            return Err(ServiceIdentityStoreError::NotFound);
        }
        Ok((items, has_more))
    }

    /// Detail a single service account, clamped to the authorized team set.
    pub async fn get_service_account(
        &self,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        authorized_teams: Option<&[TeamId]>,
    ) -> Result<Option<ServiceAccountRecord>, ServiceIdentityStoreError> {
        let row = match authorized_teams {
            None => {
                sqlx::query_as::<_, ServiceAccountViewRow>(
                    "SELECT sa.id, sa.organization_id, sa.team_id, sa.owner_user_id, sa.name,
                            sa.enabled, sa.created_at, v.version
                     FROM gateway.service_account sa
                     JOIN gateway.admin_resource_version v
                       ON v.organization_id = sa.organization_id
                      AND v.resource_type = 'service_account'
                      AND v.resource_id = sa.id
                     WHERE sa.organization_id = $1 AND sa.id = $2",
                )
                .bind(organization_id.0)
                .bind(service_account_id.0)
                .fetch_optional(&self.pool)
                .await?
            }
            Some(teams) => {
                let team_ids: Vec<Uuid> = teams.iter().map(|team| team.0).collect();
                sqlx::query_as::<_, ServiceAccountViewRow>(
                    "SELECT sa.id, sa.organization_id, sa.team_id, sa.owner_user_id, sa.name,
                            sa.enabled, sa.created_at, v.version
                     FROM gateway.service_account sa
                     JOIN gateway.admin_resource_version v
                       ON v.organization_id = sa.organization_id
                      AND v.resource_type = 'service_account'
                      AND v.resource_id = sa.id
                     WHERE sa.organization_id = $1 AND sa.id = $2
                       AND sa.team_id = ANY($3)",
                )
                .bind(organization_id.0)
                .bind(service_account_id.0)
                .bind(&team_ids)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        Ok(row.map(Into::into))
    }

    // ---- service accounts: coordinator-transaction mutations ----

    pub async fn insert_service_account(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        team_id: Option<TeamId>,
        owner_user_id: UserAccountId,
        name: &str,
    ) -> Result<(), ServiceIdentityStoreError> {
        sqlx::query(
            "INSERT INTO gateway.service_account
                 (id, organization_id, team_id, owner_user_id, name)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(service_account_id.0)
        .bind(organization_id.0)
        .bind(team_id.map(|team| team.0))
        .bind(owner_user_id.0)
        .bind(name)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    /// Rename a service account. Returns `false` when the row is absent (the
    /// caller must then fail the mutation so the coordinator rolls back).
    pub async fn update_service_account_name(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        name: &str,
    ) -> Result<bool, ServiceIdentityStoreError> {
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.service_account SET name = $3
             WHERE organization_id = $1 AND id = $2
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .bind(name)
        .fetch_optional(&mut **transaction)
        .await?;
        Ok(updated.is_some())
    }

    pub async fn set_service_account_enabled(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        enabled: bool,
    ) -> Result<bool, ServiceIdentityStoreError> {
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.service_account SET enabled = $3
             WHERE organization_id = $1 AND id = $2
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .bind(enabled)
        .fetch_optional(&mut **transaction)
        .await?;
        Ok(updated.is_some())
    }

    /// Delete a service account and its resource-version row so the id can be
    /// reused by a future create under the same idempotency key. Deletion is
    /// rejected while any API key is still active (including successors), so an
    /// active credential can never be cascade-deleted; the caller must revoke
    /// every key first. An account that has made requests is disabled instead:
    /// its accounting records are append-only and keep referring to it.
    pub async fn delete_service_account(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
    ) -> Result<Option<ServiceAccountRemoval>, ServiceIdentityStoreError> {
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.api_key
             WHERE organization_id = $1 AND service_account_id = $2 AND status = 'active'",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .fetch_one(&mut **transaction)
        .await?;
        if active > 0 {
            return Err(ServiceIdentityStoreError::ActiveApiKeysExist);
        }
        let used: bool = sqlx::query_scalar(
            // Credit grants are an append-only ledger (D-028): an account
            // that granted any is kept, disabled, like one that was used.
            "SELECT EXISTS (SELECT 1 FROM gateway.accounting_reservation
                            WHERE organization_id = $1 AND service_account_id = $2)
                 OR EXISTS (SELECT 1 FROM gateway.end_user_credit_grant
                            WHERE organization_id = $1 AND service_account_id = $2)",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .fetch_one(&mut **transaction)
        .await?;
        if used {
            let row = sqlx::query_as::<_, ServiceAccountViewRow>(
                "UPDATE gateway.service_account sa SET enabled = false
                 FROM gateway.admin_resource_version v
                 WHERE sa.organization_id = $1 AND sa.id = $2
                   AND v.organization_id = sa.organization_id
                   AND v.resource_type = 'service_account' AND v.resource_id = sa.id
                 RETURNING sa.id, sa.organization_id, sa.team_id, sa.owner_user_id, sa.name,
                           sa.enabled, sa.created_at, v.version",
            )
            .bind(organization_id.0)
            .bind(service_account_id.0)
            .fetch_optional(&mut **transaction)
            .await?;
            return Ok(row.map(|row| ServiceAccountRemoval::Disabled(row.into())));
        }
        let updated = sqlx::query_scalar::<_, Uuid>(
            "DELETE FROM gateway.service_account
             WHERE organization_id = $1 AND id = $2
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .fetch_optional(&mut **transaction)
        .await?;
        if updated.is_some() {
            sqlx::query(
                "DELETE FROM gateway.admin_resource_version
                 WHERE organization_id = $1 AND resource_type = 'service_account'
                   AND resource_id = $2",
            )
            .bind(organization_id.0)
            .bind(service_account_id.0)
            .execute(&mut **transaction)
            .await?;
        }
        Ok(updated.map(|_| ServiceAccountRemoval::Deleted))
    }

    // ---- API keys: reads ----

    pub async fn list_api_keys(
        &self,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        usable_only: bool,
        page_size: u32,
        keyset: Option<Keyset>,
    ) -> Result<(Vec<ApiKeyRecord>, bool), ServiceIdentityStoreError> {
        let limit: i32 = (page_size + 1) as i32;
        let (created_at, id) = match keyset {
            Some(keyset) => (Some(keyset.created_at), Some(keyset.id)),
            None => (None, None),
        };
        let rows = sqlx::query_as::<_, ApiKeyViewRow>(
            "SELECT k.id, k.organization_id, k.service_account_id, k.successor_of,
                    k.public_prefix, k.scopes, k.status, k.expires_at, k.revoked_at,
                    k.disabled_at, k.last_used_at, k.created_at, v.version
             FROM gateway.api_key k
             JOIN gateway.admin_resource_version v
               ON v.organization_id = k.organization_id
              AND v.resource_type = 'api_key' AND v.resource_id = k.id
             WHERE k.organization_id = $1 AND k.service_account_id = $2
               AND ($3::timestamptz IS NULL
                    OR (k.created_at, k.id) > ($3::timestamptz, $4::uuid))
               AND (NOT $6
                    OR (k.status = 'active'
                        AND (k.expires_at IS NULL OR k.expires_at > now())))
             ORDER BY k.created_at ASC, k.id ASC
             LIMIT $5",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .bind(created_at)
        .bind(id)
        .bind(limit)
        .bind(usable_only)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() as u32 > page_size;
        let items: Vec<ApiKeyRecord> = rows
            .into_iter()
            .take(page_size as usize)
            .map(Into::into)
            .collect();
        if keyset.is_some() && items.is_empty() && !has_more {
            return Err(ServiceIdentityStoreError::NotFound);
        }
        Ok((items, has_more))
    }

    pub async fn get_api_key(
        &self,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        key_id: ApiKeyId,
    ) -> Result<Option<ApiKeyRecord>, ServiceIdentityStoreError> {
        let row = sqlx::query_as::<_, ApiKeyViewRow>(
            "SELECT k.id, k.organization_id, k.service_account_id, k.successor_of,
                    k.public_prefix, k.scopes, k.status, k.expires_at, k.revoked_at,
                    k.disabled_at, k.last_used_at, k.created_at, v.version
             FROM gateway.api_key k
             JOIN gateway.admin_resource_version v
               ON v.organization_id = k.organization_id
              AND v.resource_type = 'api_key' AND v.resource_id = k.id
             WHERE k.organization_id = $1 AND k.service_account_id = $2 AND k.id = $3",
        )
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .bind(key_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    // ---- API keys: coordinator-transaction mutations ----

    /// Whether the predecessor key is still active. Rotation must refuse to
    /// chain a successor onto a revoked/disabled/expired key, and the active
    /// check runs inside the coordinator transaction so a concurrent revoke
    /// cannot race it.
    pub async fn is_api_key_active(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        key_id: ApiKeyId,
    ) -> Result<bool, ServiceIdentityStoreError> {
        let active = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1 FROM gateway.api_key
                WHERE organization_id = $1 AND id = $2
                  AND status = 'active' AND revoked_at IS NULL AND disabled_at IS NULL
             )",
        )
        .bind(organization_id.0)
        .bind(key_id.0)
        .fetch_one(&mut **transaction)
        .await?;
        Ok(active)
    }

    /// Create a key on the coordinator transaction. The `key_id` is supplied by
    /// the caller so the coordinator's target/version/audit/idempotency all
    /// reference the actual created key; the shared identity primitive owns
    /// generation, scope validation, and returns the plaintext exactly once.
    pub async fn create_api_key_on_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        scopes: &[ApiKeyScope],
        expires_at: Option<DateTime<Utc>>,
        predecessor_id: Option<ApiKeyId>,
        key_id: ApiKeyId,
    ) -> Result<CreatedApiKey, ServiceIdentityStoreError> {
        Ok(self
            .identity
            .create_api_key_in_transaction(
                transaction,
                organization_id,
                service_account_id,
                scopes,
                expires_at,
                predecessor_id,
                key_id,
            )
            .await?)
    }

    pub async fn revoke_api_key_on_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        key_id: ApiKeyId,
    ) -> Result<bool, ServiceIdentityStoreError> {
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.api_key
             SET status = 'revoked', revoked_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND id = $2
               AND status = 'active' AND revoked_at IS NULL
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(key_id.0)
        .fetch_optional(&mut **transaction)
        .await?;
        Ok(updated.is_some())
    }
}

#[derive(FromRow)]
struct ServiceAccountViewRow {
    id: Uuid,
    organization_id: Uuid,
    team_id: Option<Uuid>,
    owner_user_id: Uuid,
    name: String,
    enabled: bool,
    created_at: DateTime<Utc>,
    version: i64,
    #[sqlx(default)]
    active_keys: Option<i64>,
    #[sqlx(default)]
    last_used_at: Option<DateTime<Utc>>,
}

impl From<ServiceAccountViewRow> for ServiceAccountRecord {
    fn from(row: ServiceAccountViewRow) -> Self {
        Self {
            id: row.id.into(),
            organization_id: row.organization_id.into(),
            team_id: row.team_id.map(Into::into),
            owner_user_id: row.owner_user_id.into(),
            name: row.name,
            enabled: row.enabled,
            created_at: row.created_at,
            version: row.version as u64,
            active_keys: row.active_keys.map(|count| count.max(0) as u64),
            last_used_at: row.last_used_at,
        }
    }
}

#[derive(FromRow)]
struct ApiKeyViewRow {
    id: Uuid,
    organization_id: Uuid,
    service_account_id: Uuid,
    successor_of: Option<Uuid>,
    public_prefix: String,
    scopes: Vec<String>,
    status: String,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    disabled_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    version: i64,
}

impl From<ApiKeyViewRow> for ApiKeyRecord {
    fn from(row: ApiKeyViewRow) -> Self {
        Self {
            id: row.id.into(),
            organization_id: row.organization_id.into(),
            service_account_id: row.service_account_id.into(),
            successor_of: row.successor_of.map(Into::into),
            public_prefix: row.public_prefix,
            scopes: row
                .scopes
                .iter()
                .filter_map(|scope| scope.parse().ok())
                .collect(),
            status: row.status,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            disabled_at: row.disabled_at,
            last_used_at: row.last_used_at,
            created_at: row.created_at,
            version: row.version as u64,
        }
    }
}
