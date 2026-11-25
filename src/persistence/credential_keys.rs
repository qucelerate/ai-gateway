//! KMS-wrapped data keys, and the stored secrets sealed under them: provider
//! credentials, webhook signing secrets and organizations' identity-provider
//! client secrets. Re-sealing locks each row with
//! `SKIP LOCKED` and writes only if the row still holds the ciphertext it
//! read, so it never overwrites a secret rotated meanwhile.

use chrono::{DateTime, Utc};

use super::admin::{AdminStoreError, insert_audit_on};
use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    identity::OrganizationId,
};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

#[derive(Clone)]
pub struct CredentialKeyStore {
    pool: PgPool,
}

/// One data key as stored: only its KMS-wrapped form.
#[derive(Clone, Debug, FromRow)]
pub struct DataKeyRow {
    pub id: Uuid,
    pub key_id: String,
    pub kms_key_id: String,
    pub wrapped_key: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

/// A stored secret to re-seal. `kind` says which table it is in.
#[derive(Clone, Debug, FromRow)]
pub struct SealedRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    /// The provider credential's adapter instance key; empty for the others.
    pub adapter_instance_key: String,
    /// The provider credential's family; empty for the others.
    pub family: String,
    pub key_id: String,
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretKind {
    ProviderCredential,
    WebhookSecret,
    /// An identity-provider configuration's client secret; the row id is the
    /// configuration's.
    IdentityProviderSecret,
}

impl SecretKind {
    pub const ALL: [Self; 3] = [
        Self::ProviderCredential,
        Self::WebhookSecret,
        Self::IdentityProviderSecret,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderCredential => "provider_credential",
            Self::WebhookSecret => "webhook_secret",
            Self::IdentityProviderSecret => "identity_provider_secret",
        }
    }
}

const DATA_KEY_SELECT: &str = "SELECT id, key_id, kms_key_id, wrapped_key, created_at
    FROM gateway.credential_data_key WHERE retired_at IS NULL ORDER BY created_at, id";

impl CredentialKeyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Every data key not retired, oldest first.
    pub async fn data_keys(&self) -> Result<Vec<DataKeyRow>, sqlx::Error> {
        sqlx::query_as(DATA_KEY_SELECT).fetch_all(&self.pool).await
    }

    /// Serializes data-key creation across processes for the transaction.
    pub async fn lock(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gateway.credential_data_key'))")
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    pub async fn data_keys_on(conn: &mut PgConnection) -> Result<Vec<DataKeyRow>, sqlx::Error> {
        sqlx::query_as(DATA_KEY_SELECT).fetch_all(&mut *conn).await
    }

    pub async fn insert(
        conn: &mut PgConnection,
        id: Uuid,
        key_id: &str,
        kms_key_id: &str,
        wrapped_key: &[u8],
    ) -> Result<DataKeyRow, sqlx::Error> {
        sqlx::query_as(
            "INSERT INTO gateway.credential_data_key (id, key_id, kms_key_id, wrapped_key)
             VALUES ($1, $2, $3, $4)
             RETURNING id, key_id, kms_key_id, wrapped_key, created_at",
        )
        .bind(id)
        .bind(key_id)
        .bind(kms_key_id)
        .bind(wrapped_key)
        .fetch_one(&mut *conn)
        .await
    }

    /// Up to `limit` secrets of `kind` sealed under one of `stale` key ids,
    /// other than `skip`, locked for re-sealing in the caller's transaction.
    pub async fn stale(
        conn: &mut PgConnection,
        kind: SecretKind,
        stale: &[String],
        skip: &[Uuid],
        limit: i64,
    ) -> Result<Vec<SealedRow>, sqlx::Error> {
        let sql = match kind {
            SecretKind::ProviderCredential => {
                "SELECT id, organization_id, adapter_instance_key, family, key_id, ciphertext
                 FROM gateway.provider_credential WHERE key_id = ANY($1) AND id <> ALL($2)
                 ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED"
            }
            SecretKind::WebhookSecret => {
                "SELECT id, organization_id, '' AS adapter_instance_key, '' AS family,
                        secret_key_id AS key_id, secret_ciphertext AS ciphertext
                 FROM gateway.webhook_endpoint WHERE secret_key_id = ANY($1) AND id <> ALL($2)
                 ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED"
            }
            SecretKind::IdentityProviderSecret => {
                "SELECT id, organization_id, '' AS adapter_instance_key, '' AS family,
                        secret_key_id AS key_id, secret_ciphertext AS ciphertext
                 FROM gateway.identity_provider_configuration
                 WHERE secret_key_id = ANY($1) AND id <> ALL($2)
                 ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED"
            }
        };
        sqlx::query_as(sql)
            .bind(stale)
            .bind(skip)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await
    }

    /// Replaces a secret's ciphertext with the same secret sealed under
    /// `key_id`. Neither the secret nor its version changes. A re-seal is
    /// audited by the system in the same transaction (`run_id` is the
    /// event's request id).
    pub async fn reseal(
        conn: &mut PgConnection,
        kind: SecretKind,
        row: &SealedRow,
        key_id: &str,
        ciphertext: &[u8],
        run_id: Uuid,
    ) -> Result<bool, AdminStoreError> {
        let sql = match kind {
            SecretKind::ProviderCredential => {
                "UPDATE gateway.provider_credential SET key_id = $2, ciphertext = $3
                 WHERE id = $1 AND key_id = $4 AND ciphertext = $5"
            }
            SecretKind::WebhookSecret => {
                "UPDATE gateway.webhook_endpoint SET secret_key_id = $2, secret_ciphertext = $3
                 WHERE id = $1 AND secret_key_id = $4 AND secret_ciphertext = $5"
            }
            SecretKind::IdentityProviderSecret => {
                "UPDATE gateway.identity_provider_configuration
                 SET secret_key_id = $2, secret_ciphertext = $3
                 WHERE id = $1 AND secret_key_id = $4 AND secret_ciphertext = $5"
            }
        };
        let resealed = sqlx::query(sql)
            .bind(row.id)
            .bind(key_id)
            .bind(ciphertext)
            .bind(&row.key_id)
            .bind(&row.ciphertext)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            == 1;
        if resealed {
            let (target, action) = match kind {
                SecretKind::ProviderCredential => (
                    AuditTargetResource::ProviderCredential,
                    "provider_credential.resealed",
                ),
                SecretKind::WebhookSecret => (
                    AuditTargetResource::WebhookEndpoint,
                    "webhook_endpoint.secret_resealed",
                ),
                SecretKind::IdentityProviderSecret => (
                    AuditTargetResource::IdentityProvider,
                    "identity_provider.secret_resealed",
                ),
            };
            insert_audit_on(
                conn,
                &AdminAuditEvent::new(
                    AuditActor::System,
                    OrganizationId(row.organization_id),
                    None,
                    target,
                    AuditAction::new(action).expect("audit action constant is valid"),
                    row.id,
                    AuditOutcome::Succeeded,
                    run_id,
                    None,
                    None,
                ),
            )
            .await?;
        }
        Ok(resealed)
    }

    /// Secrets not sealed under `active`.
    pub async fn pending(&self, active: &str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM gateway.provider_credential WHERE key_id <> $1)
                  + (SELECT count(*) FROM gateway.webhook_endpoint WHERE secret_key_id <> $1)
                  + (SELECT count(*) FROM gateway.identity_provider_configuration
                     WHERE secret_key_id <> $1)",
        )
        .bind(active)
        .fetch_one(&self.pool)
        .await
    }

    /// Retires every data key older than `active`, which must be the newest
    /// live one, that no secret uses. The newest is never retired. Trace
    /// content keys are not re-sealed: a data key that sealed one stays
    /// until that content expires. Returns how many were retired.
    pub async fn retire_unused(&self, active: &str) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.credential_data_key k SET retired_at = CURRENT_TIMESTAMP
             WHERE k.retired_at IS NULL AND k.key_id <> $1
               AND EXISTS (SELECT 1 FROM gateway.credential_data_key n
                           WHERE n.retired_at IS NULL AND n.key_id = $1
                             AND (n.created_at, n.id) > (k.created_at, k.id))
               AND NOT EXISTS (SELECT 1 FROM gateway.provider_credential c
                               WHERE c.key_id = k.key_id)
               AND NOT EXISTS (SELECT 1 FROM gateway.webhook_endpoint w
                               WHERE w.secret_key_id = k.key_id)
               AND NOT EXISTS (SELECT 1 FROM gateway.identity_provider_configuration i
                               WHERE i.secret_key_id = k.key_id)
               AND NOT EXISTS (SELECT 1 FROM gateway.inference_trace_content t
                               WHERE t.key_id = k.key_id AND t.state IN ('writing', 'stored'))",
        )
        .bind(active)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}
