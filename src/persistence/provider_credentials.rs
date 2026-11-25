//! Encrypted provider-credential rows. This store only moves ciphertext and
//! metadata; sealing happens in the domain cipher before a write reaches it,
//! and no read here returns ciphertext to an admin caller. Only the inference
//! path reads ciphertext, through [`ProviderCredentialStore::sealed`].

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use std::str::FromStr as _;

use crate::domain::{
    catalog::ProviderFamily,
    credentials::{
        CloudPrincipalStatus, CredentialBinding, CredentialCipher, ProviderCredentialMetadata,
        SealedCredential,
    },
    identity::{OrganizationId, UserAccountId},
};

#[derive(FromRow)]
struct UnrecordedRow {
    id: Uuid,
    organization_id: Uuid,
    adapter_instance_key: String,
    family: String,
    key_id: String,
    ciphertext: Vec<u8>,
}

#[derive(Clone)]
pub struct ProviderCredentialStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct MetadataRow {
    adapter_instance_key: String,
    family: String,
    key_id: String,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    rotated_at: DateTime<Utc>,
    /// NULL for a credential that acts as no cloud principal.
    principal_approved: Option<bool>,
}

impl From<MetadataRow> for ProviderCredentialMetadata {
    fn from(row: MetadataRow) -> Self {
        Self {
            adapter_instance_key: row.adapter_instance_key,
            family: row.family,
            key_id: row.key_id,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            created_at: row.created_at,
            rotated_at: row.rotated_at,
            cloud_principal: row.principal_approved.map(|approved| {
                if approved {
                    CloudPrincipalStatus::Approved
                } else {
                    CloudPrincipalStatus::Pending
                }
            }),
        }
    }
}

/// Ciphertext and the identity it was sealed for, as the inference path needs
/// it to open the credential.
#[derive(FromRow)]
pub struct StoredCredential {
    pub family: String,
    pub key_id: String,
    pub ciphertext: Vec<u8>,
    /// The cloud principal the credential acts as, by digest.
    pub principal_digest: Option<Vec<u8>>,
    /// Whether an operator has bound that principal to the organization.
    pub principal_approved: bool,
}

/// Whether credential `c`'s cloud principal is bound to its organization;
/// NULL when it acts as none.
const PRINCIPAL_APPROVED: &str = "CASE WHEN c.principal_digest IS NULL THEN NULL
        ELSE EXISTS (SELECT 1 FROM gateway.cloud_principal_binding b
                     WHERE b.organization_id = c.organization_id
                       AND b.principal_digest = c.principal_digest) END";

const METADATA_SELECT: &str = "SELECT c.adapter_instance_key, c.family, c.key_id, v.version,
        c.created_at, c.rotated_at, (PRINCIPAL_APPROVED) AS principal_approved
    FROM gateway.provider_credential c
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = c.organization_id
     AND v.resource_type = 'provider_credential' AND v.resource_id = c.id";

fn metadata_select() -> String {
    METADATA_SELECT.replace("PRINCIPAL_APPROVED", PRINCIPAL_APPROVED)
}

impl ProviderCredentialStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One page of metadata ordered by adapter instance key, after the
    /// `after` keyset. The flag reports whether more rows follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<&str>,
    ) -> Result<(Vec<ProviderCredentialMetadata>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, MetadataRow>(&format!(
            "{}
             WHERE c.organization_id = $1
               AND ($2::text IS NULL OR c.adapter_instance_key > $2)
             ORDER BY c.adapter_instance_key
             LIMIT $3",
            metadata_select()
        ))
        .bind(organization_id.0)
        .bind(after)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok((
            rows.into_iter()
                .take(page_size as usize)
                .map(Into::into)
                .collect(),
            has_more,
        ))
    }

    /// Metadata of one credential on the caller's connection, so a mutation
    /// can read the row and version its own transaction wrote.
    pub async fn metadata(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<ProviderCredentialMetadata>, sqlx::Error> {
        let row = sqlx::query_as::<_, MetadataRow>(&format!(
            "{} WHERE c.organization_id = $1 AND c.id = $2",
            metadata_select()
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Whether the operator has bound `principal` to the organization.
    pub async fn principal_bound(
        &self,
        organization_id: OrganizationId,
        principal: &crate::domain::credentials::CloudPrincipal,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.cloud_principal_binding
                            WHERE organization_id = $1 AND principal_digest = $2)",
        )
        .bind(organization_id.0)
        .bind(principal.digest())
        .fetch_one(&self.pool)
        .await
    }

    pub async fn sealed(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<Option<StoredCredential>, sqlx::Error> {
        sqlx::query_as::<_, StoredCredential>(&format!(
            "SELECT c.family, c.key_id, c.ciphertext, c.principal_digest,
                    coalesce({PRINCIPAL_APPROVED}, false) AS principal_approved
             FROM gateway.provider_credential c
             WHERE c.organization_id = $1 AND c.adapter_instance_key = $2"
        ))
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .fetch_optional(&self.pool)
        .await
    }

    /// Family that `adapter_instance_key` must be sealed for. The active
    /// release decides when it declares the key, so a staged release cannot
    /// change the family of a credential the active catalog depends on.
    /// Otherwise the newest declaring release decides, which lets a
    /// credential be staged for a release before it is activated.
    pub async fn declared_family(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT p.family FROM gateway.catalog_provider_account p
             JOIN gateway.catalog_release r
               ON r.organization_id = p.organization_id AND r.id = p.release_id
             LEFT JOIN gateway.catalog_active_release a
               ON a.organization_id = r.organization_id AND a.release_id = r.id
             WHERE p.organization_id = $1 AND p.adapter_instance_key = $2
             ORDER BY (a.release_id IS NOT NULL) DESC, r.created_at DESC, r.id DESC
             LIMIT 1",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Insert or replace the sealed secret. Returns true when a row was
    /// created rather than rotated.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        adapter_instance_key: &str,
        family: &str,
        sealed: &SealedCredential,
        principal_digest: Option<&[u8]>,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.provider_credential
                 (id, organization_id, adapter_instance_key, family, key_id, ciphertext,
                  created_by, rotated_by, principal_digest, principal_recorded)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $8, true)
             ON CONFLICT (organization_id, adapter_instance_key) DO UPDATE
             SET family = EXCLUDED.family, key_id = EXCLUDED.key_id,
                 ciphertext = EXCLUDED.ciphertext, rotated_by = EXCLUDED.rotated_by,
                 principal_digest = EXCLUDED.principal_digest, principal_recorded = true,
                 rotated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family)
        .bind(&sealed.key_id)
        .bind(&sealed.ciphertext)
        .bind(actor.0)
        .bind(principal_digest)
        .fetch_one(&mut *conn)
        .await
    }

    /// Records the cloud principal of every credential whose principal has
    /// not been worked out yet (rows stored before principals were
    /// recorded), by opening it with `cipher`. A row rotated meanwhile is
    /// left to its writer, which records its own. Rows that do not open are
    /// skipped; they stay refused. Returns how many were recorded.
    pub async fn record_principals(&self, cipher: &CredentialCipher) -> Result<u64, sqlx::Error> {
        let mut recorded = 0;
        let mut after = Uuid::nil();
        loop {
            let rows: Vec<UnrecordedRow> = sqlx::query_as(
                "SELECT id, organization_id, adapter_instance_key, family, key_id, ciphertext
                 FROM gateway.provider_credential
                 WHERE NOT principal_recorded AND id > $1
                 ORDER BY id LIMIT 100",
            )
            .bind(after)
            .fetch_all(&self.pool)
            .await?;
            let Some(last) = rows.last() else {
                return Ok(recorded);
            };
            after = last.id;
            for row in rows {
                let Ok(family) = ProviderFamily::from_str(&row.family) else {
                    continue;
                };
                let binding = CredentialBinding {
                    organization_id: OrganizationId(row.organization_id),
                    adapter_instance_key: &row.adapter_instance_key,
                    family,
                };
                let Ok(secret) = cipher.open(binding, &row.key_id, &row.ciphertext) else {
                    tracing::warn!(
                        organization_id = %row.organization_id,
                        adapter_instance_key = row.adapter_instance_key,
                        "stored provider credential does not open; its cloud principal is unrecorded"
                    );
                    continue;
                };
                let digest = secret.cloud_principal().map(|principal| principal.digest());
                recorded += sqlx::query(
                    "UPDATE gateway.provider_credential
                     SET principal_digest = $2, principal_recorded = true
                     WHERE id = $1 AND ciphertext = $3 AND NOT principal_recorded",
                )
                .bind(row.id)
                .bind(digest)
                .bind(&row.ciphertext)
                .execute(&self.pool)
                .await?
                .rows_affected();
            }
        }
    }

    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "DELETE FROM gateway.provider_credential WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }
}
