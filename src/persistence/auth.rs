use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::domain::{
    auth::{LoginFailure, OidcLoginTransaction, SecurityEvent, SessionRecord},
    identity::{OrganizationId, UserAccountId},
    identity_provider::{ProviderBinding, SignInPurpose},
};

const CLAIM_LEASE_SECONDS: i64 = 30;

/// What the router needs of a session before the route checks it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionScope {
    /// Whose session (or agent token) it is.
    pub user_id: UserAccountId,
    /// The one organization a session from an organization's provider may
    /// act in.
    pub organization_id: Option<OrganizationId>,
    /// Signed in with a local account's password.
    pub password: bool,
}

#[derive(Clone)]
pub struct AuthStore {
    pool: PgPool,
    /// Whether password sessions count; off when local accounts are.
    local_accounts: bool,
}

impl AuthStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            local_accounts: true,
        }
    }

    /// With local accounts off, sessions signed in with a password no
    /// longer authenticate anyone.
    pub fn with_local_accounts(mut self, enabled: bool) -> Self {
        self.local_accounts = enabled;
        self
    }

    pub fn local_accounts(&self) -> crate::persistence::local_accounts::LocalAccountStore {
        crate::persistence::local_accounts::LocalAccountStore::new(self.pool.clone())
    }

    pub(crate) async fn insert_login_transaction(
        &self,
        transaction: &OidcLoginTransaction,
    ) -> Result<(), sqlx::Error> {
        let provider = transaction.provider.as_ref();
        sqlx::query(
            "INSERT INTO gateway.oidc_login_transaction
             (id, state_digest, nonce_digest, verifier_ciphertext,
              browser_binding_digest, expires_at, invitation_id,
              organization_id, identity_provider_configuration_id, purpose,
              started_by, expected_version)
             VALUES ($1,$2,$3,$4,$5,to_timestamp($6),$7,$8,$9,$10,$11,$12)",
        )
        .bind(transaction.id)
        .bind(&transaction.state_digest)
        .bind(&transaction.nonce_digest)
        .bind(&transaction.verifier_ciphertext)
        .bind(&transaction.browser_binding_digest)
        .bind(transaction.expires_at as i64)
        .bind(transaction.invitation_id)
        .bind(provider.map(|binding| binding.organization_id.0))
        .bind(provider.map(|binding| binding.configuration_id))
        .bind(
            provider
                .map_or(SignInPurpose::SignIn, |binding| binding.purpose)
                .as_str(),
        )
        .bind(provider.and_then(|binding| binding.started_by.map(|user| user.0)))
        .bind(provider.and_then(|binding| binding.expected_version.map(|v| v as i64)))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically claims a callback before any verifier decryption or OIDC
    /// backend work.  An expired *pre-dispatch* claim may be fenced and
    /// replaced; exchange_started and completed rows never match this CAS.
    pub async fn claim_login_callback(
        &self,
        state_digest: &[u8],
        browser_binding_digest: &[u8],
    ) -> Result<Option<(OidcLoginTransaction, Uuid)>, sqlx::Error> {
        let claim_id = Uuid::new_v4();
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query_as::<_, LoginTransactionRow>(
            "WITH candidate AS (
                 SELECT id
                 FROM gateway.oidc_login_transaction
                 WHERE state_digest = $1
                   AND browser_binding_digest = $2
                   AND expires_at > CURRENT_TIMESTAMP
                   AND (status = 'pending'
                        OR (status = 'claimed'
                            AND claim_lease_until <= CURRENT_TIMESTAMP))
                 ORDER BY created_at
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             UPDATE gateway.oidc_login_transaction AS t
             SET status = 'claimed',
                 claim_id = $3,
                 claimed_at = CURRENT_TIMESTAMP,
                 claim_lease_until = CURRENT_TIMESTAMP + ($4 * INTERVAL '1 second')
             FROM candidate
             WHERE t.id = candidate.id
             RETURNING t.id, t.state_digest, t.nonce_digest,
                       t.verifier_ciphertext, t.browser_binding_digest,
                       extract(epoch from t.expires_at)::BIGINT AS expires_at,
                       t.invitation_id, t.organization_id,
                       t.identity_provider_configuration_id, t.purpose,
                       t.started_by, t.expected_version",
        )
        .bind(state_digest)
        .bind(browser_binding_digest)
        .bind(claim_id)
        .bind(CLAIM_LEASE_SECONDS)
        .fetch_optional(&mut *transaction)
        .await?;

        if let Some(row) = row {
            let login = OidcLoginTransaction::try_from(row)?;
            transaction.commit().await?;
            return Ok(Some((login, claim_id)));
        }

        // A callback that lost the claim race is itself an audit event.  Do
        // not include either digest or any request-controlled value.
        insert_security_event(
            &mut transaction,
            None,
            SecurityEvent::LoginReplayOrInvalidState,
        )
        .await?;
        transaction.commit().await?;
        Ok(None)
    }

    /// Fence the claim immediately before dispatching to the issuer.
    pub async fn start_login_exchange(
        &self,
        transaction_id: Uuid,
        claim_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let changed = sqlx::query(
            "UPDATE gateway.oidc_login_transaction
             SET status = 'exchange_started',
                 exchange_started_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND claim_id = $2 AND status = 'claimed'
               AND claim_lease_until > CURRENT_TIMESTAMP",
        )
        .bind(transaction_id)
        .bind(claim_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    /// Release is legal only for a backend error which guarantees that no
    /// request was sent. It is also fenced, and the release audit is in the
    /// same transaction as the state transition.
    pub async fn release_login_callback_claim(
        &self,
        transaction_id: Uuid,
        claim_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE gateway.oidc_login_transaction
             SET status = 'pending', claim_id = NULL, claimed_at = NULL,
                 claim_lease_until = NULL, exchange_started_at = NULL
             WHERE id = $1 AND claim_id = $2
               AND status IN ('claimed', 'exchange_started')",
        )
        .bind(transaction_id)
        .bind(claim_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed == 0 {
            insert_security_event(
                &mut transaction,
                None,
                SecurityEvent::LoginReplayOrInvalidState,
            )
            .await?;
        } else {
            insert_security_event(
                &mut transaction,
                None,
                SecurityEvent::LoginBackendUnavailable,
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(changed == 1)
    }

    /// Atomically terminally consumes a callback and records its outcome.
    /// This accepts both pre-dispatch and after-dispatch failures, but never
    /// a pending row or a row fenced by another claim.
    pub async fn finish_login_callback_failure_claimed(
        &self,
        transaction_id: Uuid,
        claim_id: Uuid,
        failure: LoginFailure,
    ) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE gateway.oidc_login_transaction
             SET status = 'completed', consumed_at = CURRENT_TIMESTAMP,
                 completed_at = CURRENT_TIMESTAMP, claim_lease_until = NULL
             WHERE id = $1 AND claim_id = $2
               AND status IN ('claimed', 'exchange_started')",
        )
        .bind(transaction_id)
        .bind(claim_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed == 0 {
            insert_security_event(
                &mut transaction,
                None,
                SecurityEvent::LoginReplayOrInvalidState,
            )
            .await?;
        } else {
            insert_security_event(&mut transaction, None, failure.persisted_event()).await?;
        }
        transaction.commit().await?;
        Ok(changed == 1)
    }

    /// Atomically revalidates the exact issuer/subject mapping, enabled user,
    /// and enabled membership while consuming the claim and creating the
    /// session/audit record. There is no JIT provisioning or account linking.
    pub async fn finish_login_callback_success_claimed(
        &self,
        transaction_id: Uuid,
        claim_id: Uuid,
        issuer: &str,
        subject: &str,
        session: &SessionRecord,
    ) -> Result<bool, sqlx::Error> {
        self.finish_login_success_claimed(transaction_id, claim_id, issuer, subject, session, None)
            .await
    }

    /// [`Self::finish_login_callback_success_claimed`], for a sign-in that
    /// went through `scope`'s provider configuration.
    pub async fn finish_organization_login_success_claimed(
        &self,
        transaction_id: Uuid,
        claim_id: Uuid,
        issuer: &str,
        subject: &str,
        session: &SessionRecord,
        scope: (OrganizationId, Uuid),
    ) -> Result<bool, sqlx::Error> {
        self.finish_login_success_claimed(
            transaction_id,
            claim_id,
            issuer,
            subject,
            session,
            Some(scope),
        )
        .await
    }

    /// `scope` is the organization and configuration of a sign-in through
    /// an organization's provider: the membership must be in that
    /// organization, the configuration must still be its active, enabled
    /// one, and the session is limited to it. `None` (the process-wide
    /// provider) checks exactly what it always did.
    async fn finish_login_success_claimed(
        &self,
        transaction_id: Uuid,
        claim_id: Uuid,
        issuer: &str,
        subject: &str,
        session: &SessionRecord,
        scope: Option<(OrganizationId, Uuid)>,
    ) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let (scope_organization, scope_configuration) = scope.unzip();
        let principal = sqlx::query_scalar::<_, Uuid>(
            "SELECT u.id
             FROM gateway.user_account u
             JOIN gateway.organization_membership om ON om.user_id = u.id
             WHERE u.id = $1 AND u.oidc_issuer = $2 AND u.oidc_subject = $3
               AND u.enabled AND om.enabled
               AND ($4::UUID IS NULL OR (
                   om.organization_id = $4
                   AND EXISTS (SELECT 1 FROM gateway.organization_identity_provider p
                               JOIN gateway.organization o
                                 ON o.id = p.organization_id AND o.suspended_at IS NULL
                               WHERE p.organization_id = $4 AND p.enabled
                                 AND p.active_configuration_id = $5)))
             FOR UPDATE OF u, om",
        )
        .bind(session.user_id.0)
        .bind(issuer)
        .bind(subject)
        .bind(scope_organization.map(|id| id.0))
        .bind(scope_configuration)
        .fetch_optional(&mut *transaction)
        .await?;

        let Some(principal) = principal else {
            let changed = consume_claim(&mut transaction, transaction_id, claim_id).await?;
            if changed {
                insert_security_event(&mut transaction, None, SecurityEvent::LoginUnprovisioned)
                    .await?;
            } else {
                insert_security_event(
                    &mut transaction,
                    None,
                    SecurityEvent::LoginReplayOrInvalidState,
                )
                .await?;
            }
            transaction.commit().await?;
            return Ok(false);
        };

        let changed = sqlx::query(
            "UPDATE gateway.oidc_login_transaction
             SET status = 'completed', consumed_at = CURRENT_TIMESTAMP,
                 completed_at = CURRENT_TIMESTAMP, claim_lease_until = NULL
             WHERE id = $1 AND claim_id = $2 AND status = 'exchange_started'
             RETURNING id",
        )
        .bind(transaction_id)
        .bind(claim_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed == 0 {
            insert_security_event(
                &mut transaction,
                None,
                SecurityEvent::LoginReplayOrInvalidState,
            )
            .await?;
            transaction.commit().await?;
            return Ok(false);
        }

        sqlx::query(
            "INSERT INTO gateway.session
             (id, token_digest, user_id, browser_binding_digest, expires_at,
              organization_id, identity_provider_configuration_id)
             VALUES ($1,$2,$3,$4,to_timestamp($5),$6,$7)",
        )
        .bind(session.id)
        .bind(&session.token_digest)
        .bind(principal)
        .bind(&session.browser_binding_digest)
        .bind(session.expires_at as i64)
        .bind(scope_organization.map(|id| id.0))
        .bind(scope_configuration)
        .execute(&mut *transaction)
        .await?;
        insert_security_event(
            &mut transaction,
            Some(UserAccountId(principal)),
            SecurityEvent::LoginSucceeded,
        )
        .await?;
        transaction.commit().await?;
        Ok(true)
    }

    /// Whom an agent token acts as; see
    /// [`AgentStore::authenticate`](crate::persistence::agents::AgentStore::authenticate).
    pub async fn find_agent_token(
        &self,
        token: &str,
    ) -> Result<Option<crate::domain::agents::AgentPrincipal>, sqlx::Error> {
        crate::persistence::agents::AgentStore::authenticate(&self.pool, token).await
    }

    pub async fn find_session(
        &self,
        token_digest: &[u8],
        browser_binding_digest: &[u8],
    ) -> Result<Option<UserAccountId>, sqlx::Error> {
        Ok(self
            .find_scoped_session(token_digest, browser_binding_digest)
            .await?
            .map(|(user_id, _)| user_id))
    }

    /// The session's user and, for a session from an organization's
    /// provider, the only organization it may act in. Such a session needs
    /// an enabled membership there and ends once the configuration it came
    /// from is no longer the organization's active, enabled one. A
    /// process-wide session (no organization) is checked as it always was.
    pub async fn find_scoped_session(
        &self,
        token_digest: &[u8],
        browser_binding_digest: &[u8],
    ) -> Result<Option<(UserAccountId, Option<OrganizationId>)>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT s.user_id, s.organization_id FROM gateway.session s
             JOIN gateway.user_account u ON u.id = s.user_id AND u.enabled
             WHERE s.token_digest = $1 AND s.browser_binding_digest = $2
               AND s.revoked_at IS NULL AND s.expires_at > CURRENT_TIMESTAMP
               AND (s.method = 'oidc' OR $3)
               AND (EXISTS (SELECT 1 FROM gateway.organization_membership om
                            WHERE om.user_id = s.user_id AND om.enabled
                              AND (s.organization_id IS NULL
                                   OR om.organization_id = s.organization_id))
                    -- A deployment administrator needs no organization.
                    OR (s.organization_id IS NULL AND EXISTS (
                        SELECT 1 FROM gateway.deployment_admin d WHERE d.user_id = s.user_id)))
               AND (s.organization_id IS NULL OR EXISTS (
                   SELECT 1 FROM gateway.organization_identity_provider p
                   JOIN gateway.organization o
                     ON o.id = p.organization_id AND o.suspended_at IS NULL
                   WHERE p.organization_id = s.organization_id AND p.enabled
                     AND p.active_configuration_id = s.identity_provider_configuration_id))",
        )
        .bind(token_digest)
        .bind(browser_binding_digest)
        .bind(self.local_accounts)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(user_id, organization_id)| {
            (user_id.into(), organization_id.map(OrganizationId))
        }))
    }

    /// The organization a session token is limited to, whatever the
    /// session's state: `Some(None)` for a process-wide session, `None` for
    /// no such session. The router uses it to keep organization sessions in
    /// their organization; whether the session is valid at all is the
    /// route's own check.
    pub async fn session_organization(
        &self,
        token_digest: &[u8],
    ) -> Result<Option<Option<OrganizationId>>, sqlx::Error> {
        Ok(self
            .session_scope(token_digest)
            .await?
            .map(|scope| scope.organization_id))
    }

    /// The organization a session token is limited to and whether it signed
    /// in with a password, whatever the session's state.
    pub async fn session_scope(
        &self,
        token_digest: &[u8],
    ) -> Result<Option<SessionScope>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Uuid, Option<Uuid>, String)>(
            "SELECT user_id, organization_id, method FROM gateway.session WHERE token_digest = $1",
        )
        .bind(token_digest)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(user_id, organization_id, method)| SessionScope {
            user_id: UserAccountId(user_id),
            organization_id: organization_id.map(OrganizationId),
            password: method == "password",
        }))
    }

    /// When a live session ends, in Unix seconds; `None` for a token that is
    /// no live session.
    pub async fn session_expires_at(
        &self,
        token_digest: &[u8],
    ) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT extract(epoch from expires_at)::BIGINT FROM gateway.session
             WHERE token_digest = $1 AND revoked_at IS NULL AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(token_digest)
        .fetch_optional(&self.pool)
        .await
    }

    /// Whether a password session may act in `organization_id`: it does not
    /// require its own, enabled identity provider.
    pub async fn password_allowed_in(
        &self,
        organization_id: OrganizationId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(&format!(
            "SELECT COALESCE((SELECT {} FROM gateway.organization o WHERE o.id = $1), true)",
            crate::persistence::local_accounts::PASSWORD_ALLOWED
        ))
        .bind(organization_id.0)
        .fetch_one(&self.pool)
        .await
    }

    /// Whether `organization_id` is suspended and whether it is deleted;
    /// `None` for no such organization.
    pub async fn organization_state(
        &self,
        organization_id: OrganizationId,
        member: UserAccountId,
    ) -> Result<Option<(bool, bool)>, sqlx::Error> {
        // Only to its members: to anyone else it is as unknown as any
        // organization they cannot see.
        sqlx::query_as(
            "SELECT o.suspended_at IS NOT NULL, o.deleted_at IS NOT NULL
             FROM gateway.organization o
             WHERE o.id = $1 AND EXISTS (
                 SELECT 1 FROM gateway.organization_membership om
                 WHERE om.organization_id = o.id AND om.user_id = $2)",
        )
        .bind(organization_id.0)
        .bind(member.0)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn logout_session(&self, token_digest: &[u8]) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let user_id = sqlx::query_scalar::<_, Uuid>(
            "UPDATE gateway.session SET revoked_at = CURRENT_TIMESTAMP
             WHERE token_digest = $1 AND revoked_at IS NULL RETURNING user_id",
        )
        .bind(token_digest)
        .fetch_optional(&mut *transaction)
        .await?;
        let found = user_id.is_some();
        insert_security_event(
            &mut transaction,
            user_id.map(Into::into),
            SecurityEvent::LogoutRequested,
        )
        .await?;
        transaction.commit().await?;
        Ok(found)
    }
}

async fn consume_claim(
    transaction: &mut Transaction<'_, Postgres>,
    transaction_id: Uuid,
    claim_id: Uuid,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE gateway.oidc_login_transaction
         SET status = 'completed', consumed_at = CURRENT_TIMESTAMP,
             completed_at = CURRENT_TIMESTAMP, claim_lease_until = NULL
         WHERE id = $1 AND claim_id = $2 AND status = 'exchange_started'",
    )
    .bind(transaction_id)
    .bind(claim_id)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        == 1)
}

pub(crate) async fn insert_security_event(
    transaction: &mut Transaction<'_, Postgres>,
    user_id: Option<UserAccountId>,
    event: SecurityEvent,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gateway.security_event
         (id, user_id, event_type, correlation_id, metadata)
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id.map(|id| id.0))
    .bind(event.as_str())
    .bind(Uuid::new_v4())
    .bind(serde_json::json!({"method": event.method()}))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[derive(FromRow)]
struct LoginTransactionRow {
    id: Uuid,
    state_digest: Vec<u8>,
    nonce_digest: Vec<u8>,
    verifier_ciphertext: Vec<u8>,
    browser_binding_digest: Vec<u8>,
    expires_at: i64,
    invitation_id: Option<Uuid>,
    organization_id: Option<Uuid>,
    identity_provider_configuration_id: Option<Uuid>,
    purpose: String,
    started_by: Option<Uuid>,
    expected_version: Option<i64>,
}

impl TryFrom<LoginTransactionRow> for OidcLoginTransaction {
    type Error = sqlx::Error;

    /// Fails for a row whose provider binding is inconsistent (the
    /// migration's shape check prevents one), so such a row can never pass
    /// for a process-wide sign-in.
    fn try_from(value: LoginTransactionRow) -> Result<Self, Self::Error> {
        let inconsistent = || sqlx::Error::Protocol("login transaction binding is invalid".into());
        let purpose = SignInPurpose::parse(&value.purpose).ok_or_else(inconsistent)?;
        let provider = match (
            value.organization_id,
            value.identity_provider_configuration_id,
        ) {
            (Some(organization_id), Some(configuration_id)) => Some(ProviderBinding {
                organization_id: OrganizationId(organization_id),
                configuration_id,
                purpose,
                started_by: value.started_by.map(UserAccountId),
                expected_version: value
                    .expected_version
                    .and_then(|version| u64::try_from(version).ok()),
            }),
            (None, None) if purpose == SignInPurpose::SignIn => None,
            _ => return Err(inconsistent()),
        };
        Ok(Self {
            provider,
            id: value.id,
            state_digest: value.state_digest,
            nonce_digest: value.nonce_digest,
            verifier_ciphertext: value.verifier_ciphertext,
            browser_binding_digest: value.browser_binding_digest,
            expires_at: value.expires_at as u64,
            invitation_id: value.invitation_id,
        })
    }
}
