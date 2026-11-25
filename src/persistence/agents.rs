//! Agents and their tokens. An agent is a user account of kind `agent` with
//! an enabled membership (and so a derived role binding) in its one
//! organization. Creating and disabling agents and creating and revoking
//! tokens run on the caller's transaction, inside the admin mutation
//! coordinator or an operator's bootstrap; only token digests are stored.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::{
    admin::{AdminStoreError, insert_audit_on},
    bootstrap::{
        BootstrapError, bump_version, ensure_organization, grant_membership, seed_version,
    },
};
use crate::domain::{
    admin::AdminRole,
    agents::{
        AgentPrincipal, AgentToken, AgentTokenInput, AgentTokenView, AgentView, MAX_AGENTS,
        MAX_LIVE_TOKENS, token_digest,
    },
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditTargetResource,
    },
    identity::{MembershipRole, OrganizationId, UserAccountId},
};

#[derive(Debug, thiserror::Error)]
pub enum AgentStoreError {
    #[error("an agent with this name already exists in the organization")]
    NameTaken,
    #[error("the organization has as many agents as it may")]
    TooManyAgents,
    #[error("the agent has as many live tokens as it may")]
    TooManyTokens,
    #[error("the agent does not exist or is disabled")]
    AgentUnavailable,
    #[error("the service account is not one of the organization's")]
    UnknownServiceAccount,
    #[error("the agent is the organization's last owner")]
    LastOwner,
    #[error(transparent)]
    Audit(#[from] AdminStoreError),
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(FromRow)]
struct AgentRow {
    user_id: Uuid,
    name: String,
    description: Option<String>,
    role: String,
    enabled: bool,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    disabled_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct TokenRow {
    id: Uuid,
    agent_user_id: Uuid,
    name: String,
    public_prefix: String,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    service_account_id: Option<Uuid>,
}

impl TokenRow {
    fn view(self, now: DateTime<Utc>) -> AgentTokenView {
        AgentTokenView {
            status: AgentTokenView::status_at(self.revoked_at, self.expires_at, now).to_owned(),
            id: self.id,
            name: self.name,
            public_prefix: self.public_prefix,
            created_at: self.created_at,
            created_by: self.created_by,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            last_used_at: self.last_used_at,
            service_account_id: self.service_account_id,
        }
    }
}

const AGENT_COLUMNS: &str = "a.user_id, a.name, a.description, m.role, u.enabled,
     a.created_at, a.created_by, a.disabled_at";
const AGENT_FROM: &str = "gateway.agent a
     JOIN gateway.user_account u ON u.id = a.user_id
     JOIN gateway.organization_membership m
       ON m.organization_id = a.organization_id AND m.user_id = a.user_id";
const TOKEN_COLUMNS: &str = "id, agent_user_id, name, public_prefix, created_at, created_by,
     expires_at, revoked_at, last_used_at, service_account_id";

/// What an operator's `gateway agent create` made.
#[derive(Debug)]
pub struct BootstrappedAgent {
    pub organization_id: OrganizationId,
    pub agent_id: Uuid,
    pub created_agent: bool,
    pub token: AgentToken,
}

#[derive(Clone)]
pub struct AgentStore {
    pool: PgPool,
}

impl AgentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's agents with their tokens, by name.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<AgentView>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::agents(&mut conn, organization_id, None).await
    }

    /// One agent of the organization, with its tokens.
    pub async fn get(
        &self,
        organization_id: OrganizationId,
        agent_id: Uuid,
    ) -> Result<Option<AgentView>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::view(&mut conn, organization_id, agent_id).await
    }

    /// One agent of the organization, with its tokens, on the caller's
    /// connection.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        agent_id: Uuid,
    ) -> Result<Option<AgentView>, sqlx::Error> {
        Ok(Self::agents(conn, organization_id, Some(agent_id))
            .await?
            .into_iter()
            .next())
    }

    async fn agents(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        agent_id: Option<Uuid>,
    ) -> Result<Vec<AgentView>, sqlx::Error> {
        let agents = sqlx::query_as::<_, AgentRow>(&format!(
            "SELECT {AGENT_COLUMNS} FROM {AGENT_FROM}
             WHERE a.organization_id = $1 AND ($2::uuid IS NULL OR a.user_id = $2)
             ORDER BY a.name"
        ))
        .bind(organization_id.0)
        .bind(agent_id)
        .fetch_all(&mut *conn)
        .await?;
        let tokens = sqlx::query_as::<_, TokenRow>(&format!(
            "SELECT {TOKEN_COLUMNS} FROM gateway.agent_token
             WHERE organization_id = $1 AND ($2::uuid IS NULL OR agent_user_id = $2)
             ORDER BY created_at DESC, id"
        ))
        .bind(organization_id.0)
        .bind(agent_id)
        .fetch_all(&mut *conn)
        .await?;
        let now = Utc::now();
        let mut tokens: Vec<_> = tokens.into_iter().map(Some).collect();
        Ok(agents
            .into_iter()
            .map(|agent| AgentView {
                tokens: tokens
                    .iter_mut()
                    .filter(|t| t.as_ref().is_some_and(|t| t.agent_user_id == agent.user_id))
                    .filter_map(Option::take)
                    .map(|t| t.view(now))
                    .collect(),
                id: agent.user_id,
                name: agent.name,
                description: agent.description,
                role: agent.role,
                enabled: agent.enabled,
                created_at: agent.created_at,
                created_by: agent.created_by,
                disabled_at: agent.disabled_at,
            })
            .collect())
    }

    /// A token's metadata.
    pub async fn token_view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        token_id: Uuid,
    ) -> Result<Option<AgentTokenView>, sqlx::Error> {
        Ok(sqlx::query_as::<_, TokenRow>(&format!(
            "SELECT {TOKEN_COLUMNS} FROM gateway.agent_token
             WHERE organization_id = $1 AND id = $2"
        ))
        .bind(organization_id.0)
        .bind(token_id)
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.view(Utc::now())))
    }

    /// The agent's membership role, if it is an enabled agent of the
    /// organization. Locks the agent.
    pub async fn enabled_role(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        agent_id: Uuid,
    ) -> Result<Option<MembershipRole>, sqlx::Error> {
        let role = sqlx::query_scalar::<_, String>(&format!(
            "SELECT m.role FROM {AGENT_FROM}
             WHERE a.organization_id = $1 AND a.user_id = $2
               AND u.enabled AND m.enabled AND a.disabled_at IS NULL
             FOR UPDATE OF a"
        ))
        .bind(organization_id.0)
        .bind(agent_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(role.and_then(|role| role.parse().ok()))
    }

    /// The highest organization-wide administrative role the agent holds:
    /// its membership's, or a role binding granted to it beyond that.
    pub async fn effective_role(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        agent_id: Uuid,
        membership: MembershipRole,
    ) -> Result<AdminRole, sqlx::Error> {
        let bindings: Vec<String> = sqlx::query_scalar(
            "SELECT role FROM gateway.admin_role_binding
             WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL AND enabled",
        )
        .bind(organization_id.0)
        .bind(agent_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(bindings
            .iter()
            .filter_map(|role| role.parse::<AdminRole>().ok())
            .chain([AdminRole::from_membership(membership)])
            .max_by_key(|role| role.rank())
            .expect("the membership role is always present"))
    }

    /// Creates agent `id` in the organization with an enabled membership in
    /// `role`, its role binding audited as granted by `actor`.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        name: &str,
        description: Option<&str>,
        role: MembershipRole,
        created_by: Option<UserAccountId>,
        actor: AuditActor,
    ) -> Result<(), AgentStoreError> {
        // Serializes creation within the organization, so the cap holds.
        sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
            .bind(organization_id.0)
            .execute(&mut *conn)
            .await?;
        let (count, taken) = sqlx::query_as::<_, (i64, bool)>(
            "SELECT count(*), bool_or(name = $2) IS TRUE FROM gateway.agent
             WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .bind(name)
        .fetch_one(&mut *conn)
        .await?;
        if taken {
            return Err(AgentStoreError::NameTaken);
        }
        if count >= MAX_AGENTS {
            return Err(AgentStoreError::TooManyAgents);
        }
        sqlx::query(
            "INSERT INTO gateway.user_account (id, display_name, kind) VALUES ($1, $2, 'agent')",
        )
        .bind(id)
        .bind(name)
        .execute(&mut *conn)
        .await?;
        grant_membership(
            &mut *conn,
            organization_id.0,
            id,
            role,
            actor,
            "role_binding.agent_created",
        )
        .await?;
        sqlx::query(
            "INSERT INTO gateway.agent (user_id, organization_id, name, description, created_by)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(name)
        .bind(description)
        .bind(created_by.map(|user| user.0))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Disables the agent, its membership and its role bindings, and revokes
    /// its tokens. Refused while it is the organization's last enabled
    /// owner. Returns whether it was enabled.
    pub async fn disable(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        agent_id: Uuid,
    ) -> Result<bool, AgentStoreError> {
        // Owners are counted under the organization's lock, so two owners
        // cannot disable each other at once and leave none.
        sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
            .bind(organization_id.0)
            .execute(&mut *conn)
            .await?;
        let (owner, others) = sqlx::query_as::<_, (bool, i64)>(
            "SELECT bool_or(user_id = $2) IS TRUE, count(*) FILTER (WHERE user_id <> $2)
             FROM gateway.admin_role_binding
             WHERE organization_id = $1 AND team_id IS NULL AND enabled
               AND role = 'organization_owner'",
        )
        .bind(organization_id.0)
        .bind(agent_id)
        .fetch_one(&mut *conn)
        .await?;
        if owner && others == 0 {
            return Err(AgentStoreError::LastOwner);
        }
        let disabled = sqlx::query(
            "UPDATE gateway.agent SET disabled_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND user_id = $2 AND disabled_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(agent_id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1;
        if disabled {
            sqlx::query("UPDATE gateway.user_account SET enabled = false WHERE id = $1")
                .bind(agent_id)
                .execute(&mut *conn)
                .await?;
            sqlx::query(
                "UPDATE gateway.organization_membership SET enabled = false
                 WHERE organization_id = $1 AND user_id = $2",
            )
            .bind(organization_id.0)
            .bind(agent_id)
            .execute(&mut *conn)
            .await?;
            bump_version(&mut *conn, organization_id.0, "membership", agent_id).await?;
            let bindings: Vec<Uuid> = sqlx::query_scalar(
                "UPDATE gateway.admin_role_binding
                 SET enabled = false, updated_at = CURRENT_TIMESTAMP
                 WHERE organization_id = $1 AND user_id = $2 AND enabled
                 RETURNING id",
            )
            .bind(organization_id.0)
            .bind(agent_id)
            .fetch_all(&mut *conn)
            .await?;
            for binding in bindings {
                bump_version(&mut *conn, organization_id.0, "role_binding", binding).await?;
            }
            sqlx::query(
                "UPDATE gateway.agent_token SET revoked_at = CURRENT_TIMESTAMP
                 WHERE organization_id = $1 AND agent_user_id = $2 AND revoked_at IS NULL",
            )
            .bind(organization_id.0)
            .bind(agent_id)
            .execute(&mut *conn)
            .await?;
        }
        Ok(disabled)
    }

    /// Creates token `id` for an enabled agent; the caller has locked it
    /// (`enabled_role`). Returns the token, shown once.
    pub async fn insert_token(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        agent_id: Uuid,
        id: Uuid,
        input: &AgentTokenInput,
        created_by: Option<UserAccountId>,
    ) -> Result<AgentToken, AgentStoreError> {
        let live = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gateway.agent_token
             WHERE organization_id = $1 AND agent_user_id = $2 AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > CURRENT_TIMESTAMP)",
        )
        .bind(organization_id.0)
        .bind(agent_id)
        .fetch_one(&mut *conn)
        .await?;
        if live >= MAX_LIVE_TOKENS {
            return Err(AgentStoreError::TooManyTokens);
        }
        if let Some(account) = input.service_account_id {
            let known: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM gateway.service_account
                                WHERE organization_id = $1 AND id = $2)",
            )
            .bind(organization_id.0)
            .bind(account)
            .fetch_one(&mut *conn)
            .await?;
            if !known {
                return Err(AgentStoreError::UnknownServiceAccount);
            }
        }
        let token = AgentToken::generate();
        sqlx::query(
            "INSERT INTO gateway.agent_token
                 (id, organization_id, agent_user_id, name, token_digest, public_prefix,
                  created_by, expires_at, service_account_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7,
                     CURRENT_TIMESTAMP + make_interval(days => $8), $9)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(agent_id)
        .bind(&input.name)
        .bind(token.digest())
        .bind(token.public_prefix())
        .bind(created_by.map(|user| user.0))
        .bind(input.expires_in_days.map(|days| days as i32))
        .bind(input.service_account_id)
        .execute(&mut *conn)
        .await?;
        Ok(token)
    }

    /// Revokes a live token. Returns whether it was live.
    pub async fn revoke_token(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        token_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.agent_token SET revoked_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND id = $2 AND revoked_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(token_id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }

    /// The agent, organization and any service account a presented token
    /// acts as: a live token of an enabled agent with an enabled
    /// membership. Records its use at most once a minute.
    pub async fn authenticate(
        pool: &PgPool,
        token: &str,
    ) -> Result<Option<AgentPrincipal>, sqlx::Error> {
        let Some(digest) = token_digest(token) else {
            return Ok(None);
        };
        let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid, Option<Uuid>, bool)>(
            "SELECT t.id, t.agent_user_id, t.organization_id, t.service_account_id,
                    t.last_used_at IS NULL
                      OR t.last_used_at < CURRENT_TIMESTAMP - interval '1 minute'
             FROM gateway.agent_token t
             JOIN gateway.agent a ON a.user_id = t.agent_user_id
             JOIN gateway.user_account u ON u.id = a.user_id AND u.enabled
             JOIN gateway.organization_membership m
               ON m.organization_id = a.organization_id AND m.user_id = a.user_id AND m.enabled
             JOIN gateway.organization o ON o.id = t.organization_id AND o.suspended_at IS NULL
             WHERE t.token_digest = $1 AND t.revoked_at IS NULL AND a.disabled_at IS NULL
               AND (t.expires_at IS NULL OR t.expires_at > CURRENT_TIMESTAMP)",
        )
        .bind(digest)
        .fetch_optional(pool)
        .await?;
        let Some((token_id, agent_id, organization_id, service_account_id, stale)) = row else {
            return Ok(None);
        };
        if stale {
            // Re-checked in the update, so of concurrent requests only the
            // first writes.
            sqlx::query(
                "UPDATE gateway.agent_token SET last_used_at = CURRENT_TIMESTAMP
                 WHERE id = $1
                   AND (last_used_at IS NULL
                        OR last_used_at < CURRENT_TIMESTAMP - interval '1 minute')",
            )
            .bind(token_id)
            .execute(pool)
            .await?;
        }
        Ok(Some(AgentPrincipal {
            agent_id: UserAccountId(agent_id),
            organization_id: OrganizationId(organization_id),
            service_account_id,
        }))
    }

    /// An audit record of the operator's change, by the system actor.
    async fn audit_system(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        target: AuditTargetResource,
        action: &str,
        target_id: Uuid,
        after: AuditSnapshot,
    ) -> Result<(), AgentStoreError> {
        insert_audit_on(
            conn,
            &AdminAuditEvent::new(
                AuditActor::System,
                organization_id,
                None,
                target,
                AuditAction::new(action).expect("audit action constant is valid"),
                target_id,
                AuditOutcome::Succeeded,
                Uuid::new_v4(),
                None,
                Some(after),
            ),
        )
        .await?;
        Ok(())
    }

    /// The operator's `gateway agent create`: the organization `slug`
    /// (created as `organization_name`, else the slug, if missing), the agent
    /// `name` in `role` (created if missing; an existing one keeps its role)
    /// and a new token for it. Audited by the system actor.
    pub async fn bootstrap(
        &self,
        slug: &str,
        organization_name: Option<&str>,
        name: &str,
        role: MembershipRole,
        token: &AgentTokenInput,
    ) -> Result<BootstrappedAgent, AgentStoreError> {
        let mut tx = self.pool.begin().await?;
        let (organization_id, _) = ensure_organization(&mut tx, slug, organization_name).await?;
        let organization_id = OrganizationId(organization_id);
        let existing = sqlx::query_scalar::<_, Uuid>(
            "SELECT user_id FROM gateway.agent WHERE organization_id = $1 AND name = $2",
        )
        .bind(organization_id.0)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
        let (agent_id, created_agent) = match existing {
            Some(id) => (id, false),
            None => {
                let id = Uuid::new_v4();
                Self::insert_agent(
                    &mut tx,
                    organization_id,
                    id,
                    name,
                    None,
                    role,
                    None,
                    AuditActor::System,
                )
                .await?;
                seed_version(&mut tx, organization_id.0, "agent", id).await?;
                Self::audit_system(
                    &mut tx,
                    organization_id,
                    AuditTargetResource::Agent,
                    "agent.created",
                    id,
                    AuditSnapshot {
                        enabled: Some(true),
                        role: Some(AdminRole::from_membership(role)),
                        version: Some(1),
                        ..Default::default()
                    },
                )
                .await?;
                (id, true)
            }
        };
        if Self::enabled_role(&mut tx, organization_id, agent_id)
            .await?
            .is_none()
        {
            return Err(AgentStoreError::AgentUnavailable);
        }
        let token_id = Uuid::new_v4();
        let token =
            Self::insert_token(&mut tx, organization_id, agent_id, token_id, token, None).await?;
        seed_version(&mut tx, organization_id.0, "agent_token", token_id).await?;
        let expires_at = Self::token_view(&mut tx, organization_id, token_id)
            .await?
            .and_then(|view| view.expires_at);
        Self::audit_system(
            &mut tx,
            organization_id,
            AuditTargetResource::AgentToken,
            "agent_token.created",
            token_id,
            AuditSnapshot {
                enabled: Some(true),
                expires_at,
                version: Some(1),
                ..Default::default()
            },
        )
        .await?;
        tx.commit().await?;
        Ok(BootstrappedAgent {
            organization_id,
            agent_id,
            created_agent,
            token,
        })
    }
}
