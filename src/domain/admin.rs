//! Shared contracts for administrative authorization and request coordination.
//!
//! Authorization is intentionally based on role bindings loaded from the
//! database.  A browser-selected context is only a requested resource scope;
//! it is never a source of roles or permissions.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

use super::identity::{MembershipRole, OrganizationId, TeamId, UserAccountId};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    OrganizationOwner,
    OrganizationAdmin,
    TeamAdmin,
    Operator,
    Auditor,
    ReadOnlyMember,
}

impl AdminRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrganizationOwner => "organization_owner",
            Self::OrganizationAdmin => "organization_admin",
            Self::TeamAdmin => "team_admin",
            Self::Operator => "operator",
            Self::Auditor => "auditor",
            Self::ReadOnlyMember => "read_only_member",
        }
    }

    pub const fn from_membership(role: MembershipRole) -> Self {
        match role {
            MembershipRole::Owner => Self::OrganizationOwner,
            MembershipRole::Admin => Self::OrganizationAdmin,
            MembershipRole::Auditor => Self::Auditor,
            MembershipRole::Member | MembershipRole::ReadOnly => Self::ReadOnlyMember,
        }
    }

    /// Ordered privilege rank used by the anti-escalation grant ceiling. A
    /// binding must never grant a role ranked above the role it holds.
    pub const fn rank(self) -> u8 {
        match self {
            Self::OrganizationOwner => 6,
            Self::OrganizationAdmin => 5,
            Self::Auditor => 4,
            Self::Operator => 3,
            Self::ReadOnlyMember => 2,
            Self::TeamAdmin => 1,
        }
    }

    /// Whether `self` may grant `granted` without escalation. Role-binding
    /// lanes combine this ceiling with `RoleBindingMutate` authorization.
    pub const fn can_grant(self, granted: AdminRole) -> bool {
        self.rank() >= granted.rank()
    }
}

impl FromStr for AdminRole {
    type Err = AdminRoleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "organization_owner" => Ok(Self::OrganizationOwner),
            "organization_admin" => Ok(Self::OrganizationAdmin),
            "team_admin" => Ok(Self::TeamAdmin),
            "operator" => Ok(Self::Operator),
            "auditor" => Ok(Self::Auditor),
            "read_only_member" => Ok(Self::ReadOnlyMember),
            _ => Err(AdminRoleError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminRoleError;

impl fmt::Display for AdminRoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unsupported administrative role")
    }
}
impl std::error::Error for AdminRoleError {}

/// Permission names are endpoint-independent so all administrative lanes use
/// the same matrix.  The `*_list` permissions are deliberately distinct from
/// a single-resource read: a team binding cannot accidentally become an
/// organization-wide list grant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminPermission {
    Me,
    OrganizationsRead,
    ContextSelect,
    OrganizationRead,
    OrganizationMutate,
    TeamsList,
    TeamRead,
    TeamMutate,
    MembersList,
    MemberRead,
    MemberMutate,
    RoleBindingsList,
    RoleBindingMutate,
    ServiceAccountsList,
    ServiceAccountRead,
    ServiceAccountMutate,
    ApiKeysList,
    ApiKeyRead,
    ApiKeyCreate,
    ApiKeyRotate,
    ApiKeyRevoke,
    ApiKeyDisable,
    AuditRead,
    UsageRead,
    TraceRead,
    ReconciliationRead,
    CatalogRead,
    CatalogWrite,
    CredentialWrite,
    RateLimitRead,
    RateLimitWrite,
    BudgetRead,
    BudgetWrite,
    ReconciliationResolve,
    WebhookRead,
    WebhookWrite,
    RetentionRead,
    RetentionWrite,
    /// See which requests are traced and how long content is kept.
    TracingRead,
    /// Turn tracing on or off, choose what it covers, and set content
    /// retention.
    TracingWrite,
    /// Read what a traced request sent and got back. Every read is audited.
    TraceContentRead,
    /// See the organization's usage export and its health.
    UsageExportRead,
    /// Set, change or delete it.
    UsageExportWrite,
    InvitationRead,
    InvitationWrite,
    /// See an organization's own identity provider (never its secret).
    IdentityProviderRead,
    /// Configure, test, activate and disable it.
    IdentityProviderWrite,
    /// Assign an app's end users to plans, and grant and reverse their
    /// credits (D-028); an app's backend does this with an agent token
    /// without being able to change budgets.
    EndUserWrite,
}

impl AdminPermission {
    /// Permissions that are inherently organization-wide and therefore never
    /// grantable to a team-scoped binding.
    pub const fn is_organization_wide(self) -> bool {
        matches!(
            self,
            Self::OrganizationsRead
                | Self::OrganizationRead
                | Self::OrganizationMutate
                | Self::TeamsList
                | Self::RoleBindingsList
                | Self::RoleBindingMutate
                | Self::AuditRead
                | Self::ReconciliationRead
                | Self::CatalogRead
                | Self::CatalogWrite
                | Self::CredentialWrite
                | Self::RateLimitRead
                | Self::RateLimitWrite
                | Self::BudgetRead
                | Self::BudgetWrite
                | Self::ReconciliationResolve
                | Self::WebhookRead
                | Self::WebhookWrite
                | Self::RetentionRead
                | Self::RetentionWrite
                | Self::TracingRead
                | Self::TracingWrite
                | Self::TraceContentRead
                | Self::UsageExportRead
                | Self::UsageExportWrite
                | Self::InvitationRead
                | Self::InvitationWrite
                | Self::IdentityProviderRead
                | Self::IdentityProviderWrite
                | Self::EndUserWrite
        )
    }

    /// Permissions a matching team-scoped binding may hold. Each maps only to
    /// the bound team's collections/actions; a team binding can never turn a
    /// list permission into an organization-wide read or mutation.
    pub const fn is_team_scoped(self) -> bool {
        matches!(
            self,
            Self::ContextSelect
                | Self::TeamRead
                | Self::TeamMutate
                | Self::MembersList
                | Self::MemberRead
                | Self::MemberMutate
                | Self::ServiceAccountsList
                | Self::ServiceAccountRead
                | Self::ServiceAccountMutate
                | Self::ApiKeysList
                | Self::ApiKeyRead
                | Self::ApiKeyCreate
                | Self::ApiKeyRotate
                | Self::ApiKeyRevoke
                | Self::ApiKeyDisable
                | Self::UsageRead
                | Self::TraceRead
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AdminScope {
    Organization(OrganizationId),
    Team {
        organization_id: OrganizationId,
        team_id: TeamId,
    },
}

impl AdminScope {
    pub const fn organization_id(self) -> OrganizationId {
        match self {
            Self::Organization(id) => id,
            Self::Team {
                organization_id, ..
            } => organization_id,
        }
    }

    pub const fn team_id(self) -> Option<TeamId> {
        match self {
            Self::Organization(_) => None,
            Self::Team { team_id, .. } => Some(team_id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminRoleBinding {
    pub organization_id: OrganizationId,
    pub user_id: UserAccountId,
    pub team_id: Option<TeamId>,
    pub role: AdminRole,
    pub enabled: bool,
}

impl AdminRoleBinding {
    pub const fn organization(
        organization_id: OrganizationId,
        user_id: UserAccountId,
        role: AdminRole,
    ) -> Self {
        Self {
            organization_id,
            user_id,
            team_id: None,
            role,
            enabled: true,
        }
    }

    pub const fn team(
        organization_id: OrganizationId,
        team_id: TeamId,
        user_id: UserAccountId,
        role: AdminRole,
    ) -> Self {
        Self {
            organization_id,
            user_id,
            team_id: Some(team_id),
            role,
            enabled: true,
        }
    }
}

/// The authenticated actor and server-loaded bindings.  There is no public
/// constructor that accepts a client role; application/persistence code builds
/// this value from the authenticated user and database rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminAuthorizationContext {
    pub actor_id: UserAccountId,
    pub organization_id: OrganizationId,
    pub bindings: Vec<AdminRoleBinding>,
}

pub type RoleBinding = AdminRoleBinding;
pub type AuthorizationContext = AdminAuthorizationContext;

impl AdminAuthorizationContext {
    pub fn allows(&self, permission: AdminPermission, scope: AdminScope) -> bool {
        if scope.organization_id() != self.organization_id {
            return false;
        }
        self.bindings.iter().any(|binding| {
            binding.enabled
                && binding.organization_id == self.organization_id
                && binding.user_id == self.actor_id
                && role_allows(binding.role, binding.team_id, permission, scope)
        })
    }

    /// Anti-escalation ceiling for role-binding lanes: the highest role the
    /// actor holds at `scope`. A lane must never grant a role ranked above
    /// this ceiling (`AdminRole::can_grant`), and only after `authorize`
    /// confirms `RoleBindingMutate` at the same scope.
    pub fn grant_ceiling(&self, scope: AdminScope) -> Option<AdminRole> {
        if scope.organization_id() != self.organization_id {
            return None;
        }
        self.bindings
            .iter()
            .filter(|binding| binding.enabled && binding_covers(binding.team_id, scope))
            .map(|binding| binding.role)
            .max_by_key(|role| role.rank())
    }

    /// The scopes at which the actor holds `permission`. List lanes clamp
    /// their collection to these scopes: the organization scope is returned
    /// only when an org-wide binding grants the permission; a team binding
    /// resolves to its exact team, never to the parent organization.
    pub fn authorized_scopes(&self, permission: AdminPermission) -> Vec<AdminScope> {
        let organization_id = self.organization_id;
        if self.bindings.iter().any(|binding| {
            binding.enabled
                && binding.team_id.is_none()
                && org_role_allows(binding.role, permission)
        }) {
            return vec![AdminScope::Organization(organization_id)];
        }
        let mut team_ids: Vec<TeamId> = self
            .bindings
            .iter()
            .filter_map(|binding| {
                let team_id = binding.team_id?;
                if binding.enabled
                    && binding.organization_id == organization_id
                    && binding.role == AdminRole::TeamAdmin
                    && permission.is_team_scoped()
                {
                    Some(team_id)
                } else {
                    None
                }
            })
            .collect();
        team_ids.sort_unstable_by_key(|team_id| team_id.0);
        team_ids.dedup();
        team_ids
            .into_iter()
            .map(|team_id| AdminScope::Team {
                organization_id,
                team_id,
            })
            .collect()
    }
}

/// Whether a binding with `binding_team` covers `scope`. An org-wide binding
/// (NULL team) covers the organization and inherits into every team.
fn binding_covers(binding_team: Option<TeamId>, scope: AdminScope) -> bool {
    match scope {
        AdminScope::Organization(_) => binding_team.is_none(),
        AdminScope::Team {
            team_id: scope_team,
            ..
        } => match binding_team {
            None => true,
            Some(bound) => bound == scope_team,
        },
    }
}

/// Explicit org-wide permission allowlists. Every role is a positive list;
/// there is no "allow everything except" arm anywhere in the matrix.
const ORGANIZATION_OWNER_PERMISSIONS: &[AdminPermission] = &[
    AdminPermission::Me,
    AdminPermission::OrganizationsRead,
    AdminPermission::ContextSelect,
    AdminPermission::OrganizationRead,
    AdminPermission::OrganizationMutate,
    AdminPermission::TeamsList,
    AdminPermission::TeamRead,
    AdminPermission::TeamMutate,
    AdminPermission::MembersList,
    AdminPermission::MemberRead,
    AdminPermission::MemberMutate,
    AdminPermission::RoleBindingsList,
    AdminPermission::RoleBindingMutate,
    AdminPermission::ServiceAccountsList,
    AdminPermission::ServiceAccountRead,
    AdminPermission::ServiceAccountMutate,
    AdminPermission::ApiKeysList,
    AdminPermission::ApiKeyRead,
    AdminPermission::ApiKeyCreate,
    AdminPermission::ApiKeyRotate,
    AdminPermission::ApiKeyRevoke,
    AdminPermission::ApiKeyDisable,
    AdminPermission::AuditRead,
    AdminPermission::UsageRead,
    AdminPermission::TraceRead,
    AdminPermission::ReconciliationRead,
    AdminPermission::CatalogRead,
    AdminPermission::CatalogWrite,
    AdminPermission::CredentialWrite,
    AdminPermission::RateLimitRead,
    AdminPermission::BudgetRead,
    AdminPermission::RateLimitWrite,
    AdminPermission::BudgetWrite,
    AdminPermission::ReconciliationResolve,
    AdminPermission::WebhookRead,
    AdminPermission::WebhookWrite,
    AdminPermission::RetentionRead,
    AdminPermission::RetentionWrite,
    AdminPermission::TracingRead,
    AdminPermission::TracingWrite,
    AdminPermission::TraceContentRead,
    AdminPermission::UsageExportRead,
    AdminPermission::UsageExportWrite,
    AdminPermission::InvitationRead,
    AdminPermission::InvitationWrite,
    AdminPermission::IdentityProviderRead,
    AdminPermission::IdentityProviderWrite,
    AdminPermission::EndUserWrite,
];

const ORGANIZATION_ADMIN_PERMISSIONS: &[AdminPermission] = &[
    AdminPermission::Me,
    AdminPermission::OrganizationsRead,
    AdminPermission::ContextSelect,
    AdminPermission::OrganizationRead,
    AdminPermission::TeamsList,
    AdminPermission::TeamRead,
    AdminPermission::TeamMutate,
    AdminPermission::MembersList,
    AdminPermission::MemberRead,
    AdminPermission::MemberMutate,
    AdminPermission::RoleBindingsList,
    AdminPermission::RoleBindingMutate,
    AdminPermission::ServiceAccountsList,
    AdminPermission::ServiceAccountRead,
    AdminPermission::ServiceAccountMutate,
    AdminPermission::ApiKeysList,
    AdminPermission::ApiKeyRead,
    AdminPermission::ApiKeyCreate,
    AdminPermission::ApiKeyRotate,
    AdminPermission::ApiKeyRevoke,
    AdminPermission::ApiKeyDisable,
    AdminPermission::AuditRead,
    AdminPermission::UsageRead,
    AdminPermission::TraceRead,
    AdminPermission::ReconciliationRead,
    AdminPermission::CatalogRead,
    AdminPermission::CatalogWrite,
    AdminPermission::CredentialWrite,
    AdminPermission::RateLimitRead,
    AdminPermission::BudgetRead,
    AdminPermission::RateLimitWrite,
    AdminPermission::BudgetWrite,
    AdminPermission::ReconciliationResolve,
    AdminPermission::WebhookRead,
    AdminPermission::WebhookWrite,
    AdminPermission::RetentionRead,
    AdminPermission::RetentionWrite,
    AdminPermission::TracingRead,
    AdminPermission::TracingWrite,
    AdminPermission::TraceContentRead,
    AdminPermission::UsageExportRead,
    AdminPermission::UsageExportWrite,
    AdminPermission::InvitationRead,
    AdminPermission::InvitationWrite,
    AdminPermission::IdentityProviderRead,
    AdminPermission::EndUserWrite,
];

const OPERATOR_PERMISSIONS: &[AdminPermission] = &[
    AdminPermission::Me,
    AdminPermission::ContextSelect,
    AdminPermission::OrganizationsRead,
    AdminPermission::OrganizationRead,
    AdminPermission::TeamsList,
    AdminPermission::TeamRead,
    AdminPermission::ServiceAccountsList,
    AdminPermission::ServiceAccountRead,
    AdminPermission::ApiKeysList,
    AdminPermission::ApiKeyRead,
    AdminPermission::ApiKeyCreate,
    AdminPermission::ApiKeyRotate,
    AdminPermission::ApiKeyRevoke,
    AdminPermission::ApiKeyDisable,
    AdminPermission::UsageRead,
    AdminPermission::TraceRead,
    AdminPermission::ReconciliationRead,
    AdminPermission::CatalogRead,
    AdminPermission::RateLimitRead,
    AdminPermission::BudgetRead,
    AdminPermission::WebhookRead,
    AdminPermission::RetentionRead,
    AdminPermission::TracingRead,
    AdminPermission::UsageExportRead,
    AdminPermission::EndUserWrite,
];

const AUDITOR_PERMISSIONS: &[AdminPermission] = &[
    AdminPermission::Me,
    AdminPermission::ContextSelect,
    AdminPermission::OrganizationsRead,
    AdminPermission::OrganizationRead,
    AdminPermission::TeamsList,
    AdminPermission::TeamRead,
    AdminPermission::MembersList,
    AdminPermission::MemberRead,
    AdminPermission::ServiceAccountsList,
    AdminPermission::ServiceAccountRead,
    AdminPermission::ApiKeysList,
    AdminPermission::ApiKeyRead,
    AdminPermission::AuditRead,
    AdminPermission::UsageRead,
    AdminPermission::TraceRead,
    AdminPermission::ReconciliationRead,
    AdminPermission::CatalogRead,
    AdminPermission::RateLimitRead,
    AdminPermission::BudgetRead,
    AdminPermission::WebhookRead,
    AdminPermission::RetentionRead,
    AdminPermission::TracingRead,
    AdminPermission::UsageExportRead,
    AdminPermission::InvitationRead,
    AdminPermission::IdentityProviderRead,
];

const READ_ONLY_MEMBER_PERMISSIONS: &[AdminPermission] = &[
    AdminPermission::Me,
    AdminPermission::ContextSelect,
    AdminPermission::OrganizationsRead,
    AdminPermission::OrganizationRead,
    AdminPermission::TeamsList,
    AdminPermission::TeamRead,
    AdminPermission::ServiceAccountsList,
    AdminPermission::ServiceAccountRead,
    AdminPermission::ApiKeysList,
    AdminPermission::ApiKeyRead,
    AdminPermission::UsageRead,
    AdminPermission::CatalogRead,
    AdminPermission::RateLimitRead,
    AdminPermission::BudgetRead,
    AdminPermission::WebhookRead,
    AdminPermission::RetentionRead,
    AdminPermission::TracingRead,
    AdminPermission::UsageExportRead,
];

/// Central typed RBAC matrix. Organization bindings inherit into every team;
/// team bindings are accepted only for that exact team scope and only for
/// team-scoped permissions. The migration's scope check mirrors this: team
/// bindings are `team_admin` and org-wide bindings are never `team_admin`.
pub fn role_allows(
    role: AdminRole,
    binding_team: Option<TeamId>,
    permission: AdminPermission,
    requested_scope: AdminScope,
) -> bool {
    if let Some(binding_team) = binding_team {
        let Some(requested_team) = requested_scope.team_id() else {
            return false;
        };
        return binding_team == requested_team
            && role == AdminRole::TeamAdmin
            && permission.is_team_scoped();
    }
    org_role_allows(role, permission)
}

fn org_role_allows(role: AdminRole, permission: AdminPermission) -> bool {
    match role {
        AdminRole::OrganizationOwner => ORGANIZATION_OWNER_PERMISSIONS.contains(&permission),
        AdminRole::OrganizationAdmin => ORGANIZATION_ADMIN_PERMISSIONS.contains(&permission),
        AdminRole::Operator => OPERATOR_PERMISSIONS.contains(&permission),
        AdminRole::Auditor => AUDITOR_PERMISSIONS.contains(&permission),
        AdminRole::ReadOnlyMember => READ_ONLY_MEMBER_PERMISSIONS.contains(&permission),
        // team_admin is only ever a team-scoped binding (see the scope check).
        AdminRole::TeamAdmin => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationError {
    OrganizationMismatch,
    TeamScopeRequired,
    Forbidden,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OrganizationMismatch => "authorization organization scope mismatch",
            Self::TeamScopeRequired => "permission requires a matching team scope",
            Self::Forbidden => "administrative permission denied",
        })
    }
}
impl std::error::Error for AuthorizationError {}

pub fn authorize(
    context: &AdminAuthorizationContext,
    permission: AdminPermission,
    scope: AdminScope,
) -> Result<(), AuthorizationError> {
    if scope.organization_id() != context.organization_id {
        return Err(AuthorizationError::OrganizationMismatch);
    }
    if context.allows(permission, scope) {
        Ok(())
    } else if permission.is_organization_wide() && scope.team_id().is_none() {
        Err(AuthorizationError::Forbidden)
    } else {
        Err(AuthorizationError::TeamScopeRequired)
    }
}

/// A stable operation name used by idempotency records and never copied into
/// audit metadata.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AdminOperation(String);

impl AdminOperation {
    pub fn new(value: impl Into<String>) -> Result<Self, AdminOperationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 96
            || !value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._:-".contains(&b))
        {
            return Err(AdminOperationError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminOperationError;

impl fmt::Display for AdminOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid admin operation")
    }
}
impl std::error::Error for AdminOperationError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AdminIdempotencyKey(String);

impl AdminIdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, AdminIdempotencyKeyError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 255
            || value.trim() != value
            || value.bytes().any(|b| b.is_ascii_control())
        {
            return Err(AdminIdempotencyKeyError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminIdempotencyKeyError;

impl fmt::Display for AdminIdempotencyKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid admin idempotency key")
    }
}
impl std::error::Error for AdminIdempotencyKeyError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestFingerprint(pub [u8; 32]);

impl RequestFingerprint {
    pub fn from_request(value: &serde_json::Value) -> Self {
        let canonical = canonical_json(value);
        use sha2::{Digest, Sha256};
        Self(Sha256::digest(canonical.as_bytes()).into())
    }

    pub fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        serde_json::Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotencyDecision {
    New {
        claim_id: Uuid,
    },
    Replay {
        fingerprint: RequestFingerprint,
        resource_id: Option<Uuid>,
    },
    Conflict,
    InProgress,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdempotencyResultMetadata {
    pub resource_id: Uuid,
    /// Omitted when absent so the persisted JSON never carries a `null`
    /// `version`, which the idempotency response check would reject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    pub secret_unavailable_on_replay: bool,
    /// Non-secret response body recorded for resources whose replay must
    /// return the original result rather than the resource's current state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
}

/// Version-row identity for a team membership. This must match the migration's
/// backfill identity (`md5(team_id::text || ':' || user_id::text)::uuid`) so
/// the coordinator and migration reference the same membership resource.
pub fn team_membership_version_id(team_id: TeamId, user_id: UserAccountId) -> Uuid {
    use md5::{Digest, Md5};
    use std::fmt::Write as _;
    let mut hasher = Md5::new();
    hasher.update(format!("{}:{}", team_id.0, user_id.0).as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter() {
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Uuid::parse_str(&hex).expect("an md5 digest hex is a valid uuid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn organization_grants_inherit_but_team_grants_do_not_escape_team() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        let other = TeamId::new();
        let user = UserAccountId::new();
        let context = AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings: vec![AdminRoleBinding::organization(
                org,
                user,
                AdminRole::OrganizationAdmin,
            )],
        };
        assert!(context.allows(
            AdminPermission::ApiKeyRotate,
            AdminScope::Team {
                organization_id: org,
                team_id: team
            }
        ));
        let team_context = AdminAuthorizationContext {
            bindings: vec![AdminRoleBinding::team(
                org,
                team,
                user,
                AdminRole::TeamAdmin,
            )],
            ..context
        };
        assert!(team_context.allows(
            AdminPermission::ApiKeyRotate,
            AdminScope::Team {
                organization_id: org,
                team_id: team
            }
        ));
        assert!(!team_context.allows(AdminPermission::ApiKeysList, AdminScope::Organization(org)));
        assert!(!team_context.allows(
            AdminPermission::ApiKeyRotate,
            AdminScope::Team {
                organization_id: org,
                team_id: other
            }
        ));
        // A team binding can neither read nor write the organization itself
        // or any other team's collections.
        assert!(!team_context.allows(
            AdminPermission::OrganizationRead,
            AdminScope::Organization(org)
        ));
        assert!(!team_context.allows(AdminPermission::ApiKeysList, AdminScope::Organization(org)));
        assert!(!team_context.allows(
            AdminPermission::TeamRead,
            AdminScope::Team {
                organization_id: org,
                team_id: other
            }
        ));
        assert!(!team_context.allows(
            AdminPermission::ApiKeyRead,
            AdminScope::Team {
                organization_id: org,
                team_id: other
            }
        ));
        // A non-team_admin team binding is inert, mirroring the DB scope check.
        let operator_team = AdminAuthorizationContext {
            bindings: vec![AdminRoleBinding::team(org, team, user, AdminRole::Operator)],
            ..context
        };
        assert!(!operator_team.allows(
            AdminPermission::ApiKeyRotate,
            AdminScope::Team {
                organization_id: org,
                team_id: team
            }
        ));
    }

    #[test]
    fn role_lists_are_explicit_and_scope_classes_partition_permissions() {
        assert!(org_role_allows(
            AdminRole::OrganizationOwner,
            AdminPermission::OrganizationMutate
        ));
        assert!(!org_role_allows(
            AdminRole::OrganizationAdmin,
            AdminPermission::OrganizationMutate
        ));
        assert!(org_role_allows(
            AdminRole::OrganizationAdmin,
            AdminPermission::RoleBindingMutate
        ));
        assert!(!org_role_allows(
            AdminRole::Operator,
            AdminPermission::RoleBindingMutate
        ));
        assert!(!org_role_allows(
            AdminRole::Operator,
            AdminPermission::AuditRead
        ));
        // A read_only_member only exists when explicitly granted; the backfill
        // never manufactures org-wide read from a generic membership.
        assert!(org_role_allows(
            AdminRole::ReadOnlyMember,
            AdminPermission::OrganizationRead
        ));
        assert!(!org_role_allows(
            AdminRole::ReadOnlyMember,
            AdminPermission::MembersList
        ));

        // Every permission is exactly one of org-wide, team-scoped, or Me.
        const ALL_PERMISSIONS: &[AdminPermission] = &[
            AdminPermission::Me,
            AdminPermission::OrganizationsRead,
            AdminPermission::ContextSelect,
            AdminPermission::OrganizationRead,
            AdminPermission::OrganizationMutate,
            AdminPermission::TeamsList,
            AdminPermission::TeamRead,
            AdminPermission::TeamMutate,
            AdminPermission::MembersList,
            AdminPermission::MemberRead,
            AdminPermission::MemberMutate,
            AdminPermission::RoleBindingsList,
            AdminPermission::RoleBindingMutate,
            AdminPermission::ServiceAccountsList,
            AdminPermission::ServiceAccountRead,
            AdminPermission::ServiceAccountMutate,
            AdminPermission::ApiKeysList,
            AdminPermission::ApiKeyRead,
            AdminPermission::ApiKeyCreate,
            AdminPermission::ApiKeyRotate,
            AdminPermission::ApiKeyRevoke,
            AdminPermission::ApiKeyDisable,
            AdminPermission::AuditRead,
            AdminPermission::UsageRead,
            AdminPermission::TraceRead,
            AdminPermission::ReconciliationRead,
            AdminPermission::CatalogRead,
            AdminPermission::CatalogWrite,
            AdminPermission::CredentialWrite,
            AdminPermission::RateLimitRead,
            AdminPermission::BudgetRead,
            AdminPermission::RateLimitWrite,
            AdminPermission::BudgetWrite,
            AdminPermission::ReconciliationResolve,
            AdminPermission::WebhookRead,
            AdminPermission::WebhookWrite,
            AdminPermission::RetentionRead,
            AdminPermission::RetentionWrite,
            AdminPermission::TracingRead,
            AdminPermission::TracingWrite,
            AdminPermission::TraceContentRead,
            AdminPermission::UsageExportRead,
            AdminPermission::UsageExportWrite,
            AdminPermission::InvitationRead,
            AdminPermission::InvitationWrite,
            AdminPermission::IdentityProviderRead,
            AdminPermission::IdentityProviderWrite,
        ];
        for permission in ALL_PERMISSIONS {
            let count = usize::from(permission.is_organization_wide())
                + usize::from(permission.is_team_scoped());
            match permission {
                AdminPermission::Me => assert_eq!(count, 0),
                _ => assert_eq!(count, 1, "misclassified {permission:?}"),
            }
        }
    }

    #[test]
    fn grant_ceiling_and_authorized_scopes_clamp_team_grants() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        let other = TeamId::new();
        let user = UserAccountId::new();
        let owner = AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings: vec![AdminRoleBinding::organization(
                org,
                user,
                AdminRole::OrganizationOwner,
            )],
        };
        assert_eq!(
            owner.grant_ceiling(AdminScope::Organization(org)),
            Some(AdminRole::OrganizationOwner)
        );
        assert_eq!(
            owner.authorized_scopes(AdminPermission::ApiKeysList),
            vec![AdminScope::Organization(org)]
        );

        let team_admin = AdminAuthorizationContext {
            bindings: vec![AdminRoleBinding::team(
                org,
                team,
                user,
                AdminRole::TeamAdmin,
            )],
            ..owner
        };
        assert_eq!(
            team_admin.grant_ceiling(AdminScope::Organization(org)),
            None,
            "team binding never covers the organization scope"
        );
        assert_eq!(
            team_admin.grant_ceiling(AdminScope::Team {
                organization_id: org,
                team_id: team
            }),
            Some(AdminRole::TeamAdmin)
        );
        assert_eq!(
            team_admin.authorized_scopes(AdminPermission::ApiKeysList),
            vec![AdminScope::Team {
                organization_id: org,
                team_id: team
            }]
        );
        assert!(
            team_admin
                .authorized_scopes(AdminPermission::OrganizationRead)
                .is_empty()
        );
        assert_eq!(
            team_admin.authorized_scopes(AdminPermission::TeamRead),
            vec![AdminScope::Team {
                organization_id: org,
                team_id: team
            }]
        );

        let two_teams = AdminAuthorizationContext {
            bindings: vec![
                AdminRoleBinding::team(org, team, user, AdminRole::TeamAdmin),
                AdminRoleBinding::team(org, other, user, AdminRole::TeamAdmin),
            ],
            ..owner
        };
        let scopes = two_teams.authorized_scopes(AdminPermission::ApiKeyRead);
        assert_eq!(scopes.len(), 2);
        assert!(scopes.contains(&AdminScope::Team {
            organization_id: org,
            team_id: team
        }));
        assert!(scopes.contains(&AdminScope::Team {
            organization_id: org,
            team_id: other
        }));
    }

    #[test]
    fn can_grant_is_an_anti_escalation_ceiling() {
        assert!(AdminRole::OrganizationOwner.can_grant(AdminRole::OrganizationOwner));
        assert!(AdminRole::OrganizationOwner.can_grant(AdminRole::TeamAdmin));
        assert!(!AdminRole::OrganizationAdmin.can_grant(AdminRole::OrganizationOwner));
        assert!(AdminRole::OrganizationAdmin.can_grant(AdminRole::OrganizationAdmin));
        assert!(AdminRole::TeamAdmin.can_grant(AdminRole::TeamAdmin));
        assert!(!AdminRole::TeamAdmin.can_grant(AdminRole::ReadOnlyMember));
        assert!(!AdminRole::ReadOnlyMember.can_grant(AdminRole::Operator));
    }

    #[test]
    fn fingerprints_are_canonical_and_replays_never_contain_a_secret() {
        let a = serde_json::json!({"b": 2, "a": 1});
        let b = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(
            RequestFingerprint::from_request(&a),
            RequestFingerprint::from_request(&b)
        );
        assert_eq!(AdminIdempotencyKey::new("key").unwrap().as_str(), "key");
    }

    #[test]
    fn idempotency_metadata_omits_none_version_instead_of_null() {
        let resource_id = Uuid::new_v4();
        let with_version = IdempotencyResultMetadata {
            resource_id,
            version: Some(1),
            secret_unavailable_on_replay: false,
            snapshot: None,
        };
        let json = serde_json::to_value(&with_version).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["resource_id"], resource_id.to_string());
        let without = IdempotencyResultMetadata {
            resource_id,
            version: None,
            secret_unavailable_on_replay: false,
            snapshot: None,
        };
        let json = serde_json::to_value(&without).unwrap();
        let object = json.as_object().unwrap();
        assert!(
            !object.contains_key("version"),
            "version must be omitted: {json}"
        );
        assert_eq!(object.len(), 2);
    }

    #[test]
    fn team_membership_version_id_matches_migration_backfill_identity() {
        let team = TeamId::new();
        let user = UserAccountId::new();
        let first = team_membership_version_id(team, user);
        let second = team_membership_version_id(team, user);
        assert_eq!(first, second);
        assert_ne!(
            first,
            team_membership_version_id(team, UserAccountId::new()),
            "a different user must produce a different membership identity"
        );
    }
}
