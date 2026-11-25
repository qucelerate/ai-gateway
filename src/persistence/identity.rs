use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    auth::{HumanPrincipal, OidcProfile, ProfilePhoto},
    identity::{
        ApiKeyId, ApiKeyMetadata, ApiKeyScope, CreatedApiKey, MembershipRole, Organization,
        OrganizationId, ServiceAccount, ServiceAccountId, Team, TeamId, UserAccount, UserAccountId,
        VerifiedIdentity,
    },
    keys::{self, ApiKeyPepper, KeyError},
};

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("key operation failed: {0}")]
    Key(#[from] KeyError),
    #[error("requested organization scope is invalid")]
    InvalidOrganizationScope,
    #[error("key lifecycle actor is not an enabled member of the organization")]
    InvalidAuditActor,
    #[error("requested key expiry is invalid")]
    InvalidKeyExpiry,
}

#[derive(Clone)]
pub struct IdentityStore {
    pool: PgPool,
    pepper: ApiKeyPepper,
}

impl IdentityStore {
    pub fn new(pool: PgPool, pepper: ApiKeyPepper) -> Self {
        Self { pool, pepper }
    }

    pub async fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Organization, IdentityError> {
        let row = sqlx::query_as::<_, OrganizationRow>(
            "INSERT INTO gateway.organization (id, slug, name) VALUES ($1, $2, $3) RETURNING id, slug, name",
        )
        .bind(Uuid::new_v4())
        .bind(slug)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    pub async fn create_team(
        &self,
        organization_id: OrganizationId,
        name: &str,
    ) -> Result<Team, IdentityError> {
        let row = sqlx::query_as::<_, TeamRow>(
            "INSERT INTO gateway.team (id, organization_id, name) VALUES ($1, $2, $3) RETURNING id, organization_id, name",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    pub async fn create_user(
        &self,
        requester_organization_id: OrganizationId,
        oidc_issuer: Option<&str>,
        oidc_subject: Option<&str>,
        email: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<UserAccount, IdentityError> {
        // The requester scope is deliberately checked before creating a global
        // identity; email is profile metadata, never an identity key.
        ensure_organization(&self.pool, requester_organization_id).await?;
        let row = sqlx::query_as::<_, UserAccountRow>(
            "INSERT INTO gateway.user_account (id, oidc_issuer, oidc_subject, email, display_name) VALUES ($1, $2, $3, $4, $5) RETURNING id, oidc_issuer, oidc_subject, email, display_name, enabled",
        )
        .bind(Uuid::new_v4())
        .bind(oidc_issuer)
        .bind(oidc_subject)
        .bind(email)
        .bind(display_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    pub async fn add_organization_membership(
        &self,
        requester_organization_id: OrganizationId,
        user_id: UserAccountId,
        role: MembershipRole,
    ) -> Result<(), IdentityError> {
        ensure_organization(&self.pool, requester_organization_id).await?;
        sqlx::query(
            "INSERT INTO gateway.organization_membership (organization_id, user_id, role) VALUES ($1, $2, $3)",
        )
        .bind(requester_organization_id.0)
        .bind(user_id.0)
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn add_team_membership(
        &self,
        requester_organization_id: OrganizationId,
        team_id: TeamId,
        user_id: UserAccountId,
        role: MembershipRole,
    ) -> Result<(), IdentityError> {
        sqlx::query(
            "INSERT INTO gateway.team_membership (organization_id, team_id, user_id, role) VALUES ($1, $2, $3, $4)",
        )
        .bind(requester_organization_id.0)
        .bind(team_id.0)
        .bind(user_id.0)
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_service_account(
        &self,
        requester_organization_id: OrganizationId,
        owner_user_id: UserAccountId,
        team_id: Option<TeamId>,
        name: &str,
    ) -> Result<ServiceAccount, IdentityError> {
        let row = sqlx::query_as::<_, ServiceAccountRow>(
            "INSERT INTO gateway.service_account (id, organization_id, team_id, owner_user_id, name) VALUES ($1, $2, $3, $4, $5) RETURNING id, organization_id, team_id, owner_user_id, name, enabled",
        )
        .bind(Uuid::new_v4())
        .bind(requester_organization_id.0)
        .bind(team_id.map(|id| id.0))
        .bind(owner_user_id.0)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    pub async fn find_service_account(
        &self,
        requester_organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
    ) -> Result<Option<ServiceAccount>, IdentityError> {
        let row = sqlx::query_as::<_, ServiceAccountRow>(
            "SELECT id, organization_id, team_id, owner_user_id, name, enabled FROM gateway.service_account WHERE organization_id = $1 AND id = $2",
        )
        .bind(requester_organization_id.0)
        .bind(service_account_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn set_organization_membership_enabled(
        &self,
        requester_organization_id: OrganizationId,
        user_id: UserAccountId,
        enabled: bool,
    ) -> Result<bool, IdentityError> {
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.organization_membership SET enabled = $3 WHERE organization_id = $1 AND user_id = $2 RETURNING user_id",
        )
        .bind(requester_organization_id.0)
        .bind(user_id.0)
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await?;
        Ok(updated.is_some())
    }

    pub async fn set_team_membership_enabled(
        &self,
        organization_id: OrganizationId,
        team_id: TeamId,
        user_id: UserAccountId,
        enabled: bool,
    ) -> Result<bool, IdentityError> {
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.team_membership
             SET enabled = $4
             WHERE organization_id = $1 AND team_id = $2 AND user_id = $3
             RETURNING user_id",
        )
        .bind(organization_id.0)
        .bind(team_id.0)
        .bind(user_id.0)
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await?;
        Ok(updated.is_some())
    }

    pub async fn set_service_account_enabled(
        &self,
        requester_organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        enabled: bool,
    ) -> Result<bool, IdentityError> {
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.service_account SET enabled = $3 WHERE organization_id = $1 AND id = $2 RETURNING id",
        )
        .bind(requester_organization_id.0)
        .bind(service_account_id.0)
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await?;
        Ok(updated.is_some())
    }

    pub async fn create_api_key(
        &self,
        requester_organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        scopes: &[ApiKeyScope],
        expires_at: Option<DateTime<Utc>>,
        actor_user_id: UserAccountId,
    ) -> Result<CreatedApiKey, IdentityError> {
        self.create_api_key_with_predecessor(
            requester_organization_id,
            service_account_id,
            scopes,
            expires_at,
            actor_user_id,
            None,
        )
        .await
    }

    async fn create_api_key_with_predecessor(
        &self,
        requester_organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        scopes: &[ApiKeyScope],
        expires_at: Option<DateTime<Utc>>,
        actor_user_id: UserAccountId,
        predecessor_id: Option<ApiKeyId>,
    ) -> Result<CreatedApiKey, IdentityError> {
        keys::validate_scopes(scopes)?;
        if expires_at.is_some_and(|value| value <= Utc::now()) {
            return Err(IdentityError::InvalidKeyExpiry);
        }
        let key_id = ApiKeyId::new();
        let mut transaction = self.pool.begin().await?;
        let created = self
            .create_api_key_in_transaction(
                &mut transaction,
                requester_organization_id,
                service_account_id,
                scopes,
                expires_at,
                predecessor_id,
                key_id,
            )
            .await?;
        insert_audit(
            &mut transaction,
            requester_organization_id,
            actor_user_id,
            "api_key.created",
            created.id.0,
            json!({
                "api_key_id": created.id.0,
                "public_prefix": created.public_prefix,
                "scope_count": created.scopes.len(),
            }),
        )
        .await?;
        transaction.commit().await?;
        Ok(created)
    }

    /// Generate a successor key without changing the predecessor. Revocation
    /// is an explicit, separate operation so rotation cannot unexpectedly
    /// interrupt an in-flight consumer.
    pub async fn rotate_api_key(
        &self,
        requester_organization_id: OrganizationId,
        predecessor_id: ApiKeyId,
        actor_user_id: UserAccountId,
    ) -> Result<CreatedApiKey, IdentityError> {
        let mut transaction = self.pool.begin().await?;
        let predecessor = sqlx::query_as::<_, ApiKeyMetadataRow>(
            "SELECT id, organization_id, service_account_id, successor_of,
                    public_prefix, scopes, expires_at, revoked_at, disabled_at, status
             FROM gateway.api_key
             WHERE organization_id = $1 AND id = $2
             FOR UPDATE",
        )
        .bind(requester_organization_id.0)
        .bind(predecessor_id.0)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(IdentityError::InvalidOrganizationScope)?;
        let scopes: Vec<ApiKeyScope> = predecessor
            .scopes
            .iter()
            .filter_map(|value| value.parse().ok())
            .collect();
        let successor = self
            .create_api_key_in_transaction(
                &mut transaction,
                requester_organization_id,
                predecessor.service_account_id.into(),
                &scopes,
                predecessor.expires_at,
                Some(predecessor_id),
                ApiKeyId::new(),
            )
            .await?;
        insert_audit(
            &mut transaction,
            requester_organization_id,
            actor_user_id,
            "api_key.created",
            successor.id.0,
            serde_json::json!({
                "api_key_id": successor.id.0,
                "public_prefix": successor.public_prefix,
                "scope_count": successor.scopes.len(),
            }),
        )
        .await?;
        transaction.commit().await?;
        Ok(successor)
    }

    /// Coordinator-facing key creation primitive. It performs only the
    /// successor insert and version-row seeding on the caller's transaction;
    /// authorization, typed audit, idempotency and commit belong to
    /// `AdminMutation`. The `key_id` is supplied by the caller so the
    /// coordinator's target/version/audit/idempotency reference the actual
    /// created key, and the successor's version row is seeded atomically so a
    /// later revoke/disable can resolve its optimistic-concurrency version.
    pub async fn create_api_key_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        scopes: &[ApiKeyScope],
        expires_at: Option<DateTime<Utc>>,
        predecessor_id: Option<ApiKeyId>,
        key_id: ApiKeyId,
    ) -> Result<CreatedApiKey, IdentityError> {
        keys::validate_scopes(scopes)?;
        if expires_at.is_some_and(|value| value <= Utc::now()) {
            return Err(IdentityError::InvalidKeyExpiry);
        }
        if let Some(predecessor_id) = predecessor_id {
            let predecessor_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM gateway.api_key WHERE organization_id = $1 AND id = $2)",
            )
            .bind(organization_id.0)
            .bind(predecessor_id.0)
            .fetch_one(&mut **transaction)
            .await?;
            if !predecessor_exists {
                return Err(IdentityError::InvalidOrganizationScope);
            }
        }
        let generated = keys::generate(&self.pepper);
        let scope_values: Vec<&str> = scopes.iter().map(|scope| scope.as_str()).collect();
        let row = sqlx::query_as::<_, CreatedApiKeyRow>(
            "INSERT INTO gateway.api_key
                 (id, organization_id, service_account_id, successor_of,
                  public_prefix, digest, scopes, expires_at)
             SELECT $1, organization_id, id, $3, $4, $5, $6, $7
             FROM gateway.service_account
             WHERE organization_id = $2 AND id = $8
             RETURNING id, organization_id, service_account_id, scopes, expires_at",
        )
        .bind(key_id.0)
        .bind(organization_id.0)
        .bind(predecessor_id.map(|value| value.0))
        .bind(&generated.public_prefix)
        .bind(&generated.digest)
        .bind(&scope_values)
        .bind(expires_at)
        .bind(service_account_id.0)
        .fetch_one(&mut **transaction)
        .await?;
        sqlx::query(
            "INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
             VALUES ($1, 'api_key', $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(organization_id.0)
        .bind(key_id.0)
        .execute(&mut **transaction)
        .await?;
        Ok(CreatedApiKey {
            id: row.id.into(),
            organization_id: row.organization_id.into(),
            service_account_id: row.service_account_id.into(),
            public_prefix: generated.public_prefix,
            bearer: generated.bearer,
            scopes: row
                .scopes
                .iter()
                .filter_map(|value| value.parse().ok())
                .collect(),
            expires_at: row.expires_at,
        })
    }

    pub async fn find_api_key(
        &self,
        requester_organization_id: OrganizationId,
        key_id: ApiKeyId,
    ) -> Result<Option<ApiKeyMetadata>, IdentityError> {
        let row = sqlx::query_as::<_, ApiKeyMetadataRow>(
            "SELECT id, organization_id, service_account_id, successor_of, public_prefix, scopes, expires_at, revoked_at, disabled_at, status FROM gateway.api_key WHERE organization_id = $1 AND id = $2",
        )
        .bind(requester_organization_id.0)
        .bind(key_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn revoke_api_key(
        &self,
        requester_organization_id: OrganizationId,
        key_id: ApiKeyId,
        actor_user_id: UserAccountId,
    ) -> Result<bool, IdentityError> {
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.api_key SET status = 'revoked', revoked_at = CURRENT_TIMESTAMP WHERE organization_id = $1 AND id = $2 AND revoked_at IS NULL AND status = 'active' RETURNING id",
        )
        .bind(requester_organization_id.0)
        .bind(key_id.0)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(id) = updated {
            insert_audit(
                &mut transaction,
                requester_organization_id,
                actor_user_id,
                "api_key.revoked",
                id,
                json!({ "api_key_id": id }),
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(updated.is_some())
    }

    pub async fn disable_api_key(
        &self,
        requester_organization_id: OrganizationId,
        key_id: ApiKeyId,
        actor_user_id: UserAccountId,
    ) -> Result<bool, IdentityError> {
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.api_key SET status = 'disabled', disabled_at = CURRENT_TIMESTAMP WHERE organization_id = $1 AND id = $2 AND status = 'active' RETURNING id",
        )
        .bind(requester_organization_id.0)
        .bind(key_id.0)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(id) = updated {
            insert_audit(
                &mut transaction,
                requester_organization_id,
                actor_user_id,
                "api_key.disabled",
                id,
                json!({ "api_key_id": id }),
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(updated.is_some())
    }

    /// Authenticate without applying an endpoint authorization decision. This
    /// lets transport distinguish an invalid bearer (401) from a valid key
    /// lacking the requested capability (403).
    pub async fn authenticate_bearer_key(
        &self,
        bearer: &str,
    ) -> Result<Option<VerifiedIdentity>, IdentityError> {
        for scope in [
            ApiKeyScope::Inference,
            ApiKeyScope::Embeddings,
            ApiKeyScope::CatalogRead,
            ApiKeyScope::AdminRead,
        ] {
            if let Some(identity) = self.verify_bearer_key(bearer, scope).await? {
                return Ok(Some(identity));
            }
        }
        Ok(None)
    }

    /// The only organization-unscoped operation: the globally unique prefix
    /// narrows the candidate before constant-time HMAC verification.
    pub async fn verify_bearer_key(
        &self,
        bearer: &str,
        requested_scope: ApiKeyScope,
    ) -> Result<Option<VerifiedIdentity>, IdentityError> {
        let Some(prefix) = keys::prefix_from_bearer(bearer) else {
            return Ok(None);
        };
        let Some(row) = sqlx::query_as::<_, VerificationRow>(
            "SELECT k.id, k.organization_id, k.service_account_id, k.digest, k.scopes, k.expires_at, k.revoked_at, k.disabled_at, k.status, sa.team_id, sa.enabled AS service_account_enabled, COALESCE(u.enabled, false) AND COALESCE(om.enabled, false) AS owner_enabled FROM gateway.api_key k JOIN gateway.service_account sa ON sa.organization_id = k.organization_id AND sa.id = k.service_account_id JOIN gateway.user_account u ON u.id = sa.owner_user_id JOIN gateway.organization_membership om ON om.organization_id = sa.organization_id AND om.user_id = sa.owner_user_id JOIN gateway.organization o ON o.id = k.organization_id AND o.suspended_at IS NULL WHERE k.public_prefix = $1",
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        if !keys::verify_digest(&self.pepper, bearer, &row.digest)?
            || row.status != "active"
            || row.revoked_at.is_some()
            || row.disabled_at.is_some()
            || !row.service_account_enabled
            || !row.owner_enabled
            || row.expires_at.is_some_and(|value| value <= Utc::now())
            || !row
                .scopes
                .iter()
                .any(|scope| scope == requested_scope.as_str())
        {
            return Ok(None);
        }

        // Last-used telemetry is deliberately best-effort and throttled. A
        // database hiccup must never turn an already verified key into an
        // authentication failure.
        let _ = sqlx::query("UPDATE gateway.api_key SET last_used_at = CURRENT_TIMESTAMP WHERE id = $1 AND organization_id = $2 AND (last_used_at IS NULL OR last_used_at < CURRENT_TIMESTAMP - INTERVAL '5 minutes')")
            .bind(row.id)
            .bind(row.organization_id)
            .execute(&self.pool)
            .await;
        Ok(Some(VerifiedIdentity {
            organization_id: row.organization_id.into(),
            service_account_id: row.service_account_id.into(),
            team_id: row.team_id.map(Into::into),
            api_key_id: Some(row.id.into()),
            scopes: row
                .scopes
                .iter()
                .filter_map(|value| value.parse().ok())
                .collect(),
            end_user: None,
        }))
    }

    pub async fn find_enabled_oidc_user(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<UserAccountId>, IdentityError> {
        let id = sqlx::query_scalar::<_, Uuid>("SELECT u.id FROM gateway.user_account u WHERE u.oidc_issuer = $1 AND u.oidc_subject = $2 AND u.enabled AND EXISTS (SELECT 1 FROM gateway.organization_membership om WHERE om.user_id = u.id AND om.enabled)")
            .bind(issuer).bind(subject).fetch_optional(&self.pool).await?;
        Ok(id.map(Into::into))
    }

    pub async fn oidc_user_status(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<(UserAccountId, bool, bool)>, IdentityError> {
        let row = sqlx::query_as::<_, OidcUserStatusRow>(
            "SELECT u.id, u.enabled, EXISTS (SELECT 1 FROM gateway.organization_membership om WHERE om.user_id = u.id AND om.enabled) AS has_enabled_membership FROM gateway.user_account u WHERE u.oidc_issuer = $1 AND u.oidc_subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| (row.id.into(), row.enabled, row.has_enabled_membership)))
    }

    /// Records what the provider said about the person at sign-in. A name
    /// or email it left out keeps the stored one; so does a photo it could
    /// not be asked for.
    pub async fn record_oidc_profile(
        &self,
        user_id: UserAccountId,
        profile: &OidcProfile,
    ) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE gateway.user_account
             SET display_name = COALESCE($2, display_name), email = COALESCE($3, email)
             WHERE id = $1",
        )
        .bind(user_id.0)
        .bind(profile.name.as_deref())
        .bind(profile.email.as_deref())
        .execute(&mut *tx)
        .await?;
        match &profile.photo {
            ProfilePhoto::Unknown => {}
            ProfilePhoto::Absent => {
                sqlx::query("DELETE FROM gateway.user_profile_photo WHERE user_id = $1")
                    .bind(user_id.0)
                    .execute(&mut *tx)
                    .await?;
            }
            ProfilePhoto::Present {
                content_type,
                bytes,
            } => {
                sqlx::query(
                    "INSERT INTO gateway.user_profile_photo (user_id, content_type, photo)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (user_id) DO UPDATE
                     SET content_type = EXCLUDED.content_type, photo = EXCLUDED.photo,
                         updated_at = CURRENT_TIMESTAMP",
                )
                .bind(user_id.0)
                .bind(content_type)
                .bind(bytes)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// The person's name and email, and whether they have a photo.
    pub async fn user_profile(
        &self,
        user_id: UserAccountId,
    ) -> Result<Option<UserProfile>, IdentityError> {
        Ok(sqlx::query_as::<_, UserProfile>(
            "SELECT u.display_name, u.email,
                    EXISTS (SELECT 1 FROM gateway.user_profile_photo p WHERE p.user_id = u.id)
                        AS has_photo
             FROM gateway.user_account u WHERE u.id = $1",
        )
        .bind(user_id.0)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The person's photo: its content type and bytes.
    pub async fn user_photo(
        &self,
        user_id: UserAccountId,
    ) -> Result<Option<(String, Vec<u8>)>, IdentityError> {
        Ok(sqlx::query_as::<_, (String, Vec<u8>)>(
            "SELECT content_type, photo FROM gateway.user_profile_photo WHERE user_id = $1",
        )
        .bind(user_id.0)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn organizations_for_user(
        &self,
        user_id: UserAccountId,
    ) -> Result<Vec<Organization>, IdentityError> {
        let rows = sqlx::query_as::<_, OrganizationRow>("SELECT o.id, o.slug, o.name FROM gateway.organization o JOIN gateway.organization_membership om ON om.organization_id = o.id JOIN gateway.user_account u ON u.id = om.user_id AND u.enabled WHERE om.user_id = $1 AND om.enabled AND o.suspended_at IS NULL ORDER BY o.slug")
            .bind(user_id.0).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn human_principal(
        &self,
        user_id: UserAccountId,
        organization_id: OrganizationId,
        team_id: Option<TeamId>,
    ) -> Result<Option<HumanPrincipal>, IdentityError> {
        let row = sqlx::query_as::<_, MembershipRow>("SELECT COALESCE(tm.role, om.role) AS role, tm.team_id FROM gateway.organization_membership om JOIN gateway.user_account u ON u.id = om.user_id AND u.enabled JOIN gateway.organization o ON o.id = om.organization_id AND o.suspended_at IS NULL LEFT JOIN gateway.team_membership tm ON tm.organization_id = om.organization_id AND tm.user_id = om.user_id AND tm.team_id = $3 AND tm.enabled WHERE om.organization_id = $1 AND om.user_id = $2 AND om.enabled AND ($3 IS NULL OR tm.team_id IS NOT NULL)")
            .bind(organization_id.0).bind(user_id.0).bind(team_id.map(|id| id.0)).fetch_optional(&self.pool).await?;
        Ok(row.and_then(|row| {
            row.role.parse().ok().map(|role| HumanPrincipal {
                user_id,
                organization_id,
                team_id: row.team_id.map(Into::into),
                role,
            })
        }))
    }
}

#[derive(FromRow)]
struct MembershipRow {
    role: String,
    team_id: Option<Uuid>,
}

/// What the admin UI shows for the signed-in person.
#[derive(Debug, FromRow, serde::Serialize)]
pub struct UserProfile {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub has_photo: bool,
}

#[derive(FromRow)]
struct OidcUserStatusRow {
    id: Uuid,
    enabled: bool,
    has_enabled_membership: bool,
}

async fn ensure_organization(
    pool: &PgPool,
    organization_id: OrganizationId,
) -> Result<(), IdentityError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM gateway.organization WHERE id = $1)",
    )
    .bind(organization_id.0)
    .fetch_one(pool)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(IdentityError::InvalidOrganizationScope)
    }
}

async fn insert_audit(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    actor_user_id: UserAccountId,
    action: &str,
    entity_id: Uuid,
    metadata: serde_json::Value,
) -> Result<(), IdentityError> {
    let inserted = sqlx::query(
        "INSERT INTO gateway.audit_event (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata) SELECT $1, $2, $3, $4, $5, $6, $7 WHERE EXISTS (SELECT 1 FROM gateway.organization_membership om JOIN gateway.user_account u ON u.id = om.user_id WHERE om.organization_id = $2 AND om.user_id = $3 AND om.enabled AND u.enabled AND om.role IN ('owner', 'admin'))",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id.0)
    .bind(actor_user_id.0)
    .bind(action)
    .bind("api_key")
    .bind(entity_id)
    .bind(metadata)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(IdentityError::InvalidAuditActor);
    }
    Ok(())
}

#[derive(FromRow)]
struct OrganizationRow {
    id: Uuid,
    slug: String,
    name: String,
}
impl From<OrganizationRow> for Organization {
    fn from(value: OrganizationRow) -> Self {
        Self {
            id: value.id.into(),
            slug: value.slug,
            name: value.name,
        }
    }
}
#[derive(FromRow)]
struct TeamRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
}
impl From<TeamRow> for Team {
    fn from(value: TeamRow) -> Self {
        Self {
            id: value.id.into(),
            organization_id: value.organization_id.into(),
            name: value.name,
        }
    }
}
#[derive(FromRow)]
struct UserAccountRow {
    id: Uuid,
    oidc_issuer: Option<String>,
    oidc_subject: Option<String>,
    email: Option<String>,
    display_name: Option<String>,
    enabled: bool,
}
impl From<UserAccountRow> for UserAccount {
    fn from(value: UserAccountRow) -> Self {
        Self {
            id: value.id.into(),
            oidc_issuer: value.oidc_issuer,
            oidc_subject: value.oidc_subject,
            email: value.email,
            display_name: value.display_name,
            enabled: value.enabled,
        }
    }
}
#[derive(FromRow)]
struct ServiceAccountRow {
    id: Uuid,
    organization_id: Uuid,
    team_id: Option<Uuid>,
    owner_user_id: Uuid,
    name: String,
    enabled: bool,
}
impl From<ServiceAccountRow> for ServiceAccount {
    fn from(value: ServiceAccountRow) -> Self {
        Self {
            id: value.id.into(),
            organization_id: value.organization_id.into(),
            team_id: value.team_id.map(Into::into),
            owner_user_id: value.owner_user_id.into(),
            name: value.name,
            enabled: value.enabled,
        }
    }
}
#[derive(FromRow)]
struct CreatedApiKeyRow {
    id: Uuid,
    organization_id: Uuid,
    service_account_id: Uuid,
    scopes: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
}
#[derive(FromRow)]
struct ApiKeyMetadataRow {
    id: Uuid,
    organization_id: Uuid,
    service_account_id: Uuid,
    successor_of: Option<Uuid>,
    public_prefix: String,
    scopes: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    disabled_at: Option<DateTime<Utc>>,
    status: String,
}
impl From<ApiKeyMetadataRow> for ApiKeyMetadata {
    fn from(value: ApiKeyMetadataRow) -> Self {
        Self {
            id: value.id.into(),
            organization_id: value.organization_id.into(),
            service_account_id: value.service_account_id.into(),
            successor_of: value.successor_of.map(Into::into),
            public_prefix: value.public_prefix,
            scopes: value
                .scopes
                .iter()
                .filter_map(|scope| scope.parse().ok())
                .collect(),
            expires_at: value.expires_at,
            revoked_at: value.revoked_at,
            disabled_at: value.disabled_at,
            status: value.status,
        }
    }
}
#[derive(FromRow)]
struct VerificationRow {
    id: Uuid,
    organization_id: Uuid,
    service_account_id: Uuid,
    digest: Vec<u8>,
    scopes: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    disabled_at: Option<DateTime<Utc>>,
    status: String,
    team_id: Option<Uuid>,
    service_account_enabled: bool,
    owner_enabled: bool,
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
    async fn the_profile_follows_the_provider_but_keeps_what_it_left_out() {
        let Some(pool) = pool().await else {
            return;
        };
        let store = IdentityStore::new(
            pool.clone(),
            ApiKeyPepper::from_bytes(b"profile-test-pepper").unwrap(),
        );
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.user_account (id, oidc_issuer, oidc_subject)
             VALUES ($1, 'https://profile.example', $2)",
        )
        .bind(id)
        .bind(id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let user = UserAccountId(id);
        let shown = |profile: UserProfile| (profile.display_name, profile.email, profile.has_photo);
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();

        store
            .record_oidc_profile(
                user,
                &OidcProfile {
                    name: Some("Ada".into()),
                    email: Some("ada@example.com".into()),
                    photo: ProfilePhoto::Present {
                        content_type: "image/png".into(),
                        bytes: png.clone(),
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(
            shown(store.user_profile(user).await.unwrap().unwrap()),
            (Some("Ada".into()), Some("ada@example.com".into()), true)
        );
        assert_eq!(
            store.user_photo(user).await.unwrap(),
            Some(("image/png".into(), png.clone()))
        );

        // A new name replaces the old; a missing email, and a photo the
        // provider could not be asked for, are kept.
        store
            .record_oidc_profile(
                user,
                &OidcProfile {
                    name: Some("Ada Lovelace".into()),
                    email: None,
                    photo: ProfilePhoto::Unknown,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            shown(store.user_profile(user).await.unwrap().unwrap()),
            (
                Some("Ada Lovelace".into()),
                Some("ada@example.com".into()),
                true
            )
        );

        // A photo the provider no longer has is removed.
        store
            .record_oidc_profile(
                user,
                &OidcProfile {
                    photo: ProfilePhoto::Absent,
                    ..OidcProfile::default()
                },
            )
            .await
            .unwrap();
        assert!(!store.user_profile(user).await.unwrap().unwrap().has_photo);
        assert_eq!(store.user_photo(user).await.unwrap(), None);
        assert!(
            store
                .user_profile(UserAccountId(Uuid::new_v4()))
                .await
                .unwrap()
                .is_none()
        );
    }
}
