//! Safe administrative audit drafts.
//!
//! These values are deliberately distinct from persisted audit events and
//! inference traces. Details are a closed enum of identifiers and booleans;
//! there is no user-provided map, prompt, completion, credential, or heuristic
//! "sanitization" step.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

use super::{
    admin::AdminRole,
    identity::{
        ApiKeyId, ApiKeyScope, MembershipRole, OrganizationId, ServiceAccountId, TeamId,
        UserAccountId,
    },
    inference::CorrelationId,
};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuditActor {
    User(UserAccountId),
    ServiceAccount(ServiceAccountId),
    System,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuditTarget {
    Organization(OrganizationId),
    Team(TeamId),
    ServiceAccount(ServiceAccountId),
    ApiKey(ApiKeyId),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuditOutcome {
    Succeeded,
    Failed,
    Denied,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditTargetResource {
    Organization,
    Team,
    Membership,
    RoleBinding,
    ServiceAccount,
    ApiKey,
    UsageReport,
    InferenceTrace,
    ReconciliationItem,
    CatalogRelease,
    /// The organization's active-release pointer. Its version resource is the
    /// organization; audit events name the activated release.
    CatalogActivation,
    ProviderCredential,
    PriceOverride,
    RateLimitPolicy,
    /// A spending budget of the organization, a team or a service account.
    BudgetPolicy,
    WebhookEndpoint,
    WebhookDelivery,
    /// A model discovery run; only a manual request is audited.
    CatalogSyncJob,
    /// Which published price item prices a deployment.
    PriceMapping,
    /// A request to fetch a gateway-wide price source now.
    PriceSourceFetch,
    /// How long the organization's traces, usage and audit events are kept.
    RetentionPolicy,
    /// Which requests are traced and how long their content is kept.
    TracePolicy,
    /// An export of usage, traces or audit events; audited as a read.
    DataExport,
    /// An operator's binding of a cloud principal to an organization.
    CloudPrincipalBinding,
    /// An invitation to join an organization.
    Invitation,
    /// The organization's own OIDC identity provider.
    IdentityProvider,
    /// The organization's configuration pin. Its version resource is the
    /// organization; a pin's audit event names the pinned version.
    ConfigPin,
    /// The organization's usage export to its own object storage.
    UsageExport,
    /// A provider account's opt-in to automatic model offers.
    CatalogAutoOffer,
    /// A non-human administrator of the organization.
    Agent,
    /// A token an agent signs in with.
    AgentToken,
    /// An email domain the organization claims for sign-in.
    OrganizationDomain,
    /// Whether the organization's catalog prices follow their sources.
    CatalogPricePolicy,
    /// A published price held for an admin rather than adopted.
    CatalogPriceHold,
    /// A service account's settings for budgets per end user (D-028).
    EndUserSetting,
    /// A plan of a service account's end users.
    EndUserPlan,
    /// An end user of an app.
    EndUser,
    /// Prepaid credits granted to an end user, and their reversals.
    EndUserCreditGrant,
    /// The provider families and locations the organization's requests may
    /// be sent to.
    RoutingPolicy,
    /// The organization's note on a model's card.
    ModelCardNote,
    /// Public facts imported from a Hugging Face model card.
    ModelCardImport,
    /// An organization's evaluation set.
    ModelEvaluationSet,
    /// One scoring of a batch against an evaluation set.
    ModelEvaluationRun,
    /// An organization's own master model record (D-033).
    ModelMaster,
    /// An organization's link from a provider family and model identifier
    /// to a master record (D-033).
    ModelMasterLink,
    /// The agreements accepted for a provider account's models (D-035).
    ModelAgreement,
    /// Whether the organization's batches may run emulated (D-021).
    BatchSettings,
    /// Where a provider connection's batch jobs stage their files.
    BatchStaging,
    /// A batch an administrator cancelled.
    Batch,
}

impl AuditTargetResource {
    /// Every target type, in declaration order: the audit filters accept
    /// each one.
    pub const ALL: &'static [Self] = &[
        Self::Organization,
        Self::Team,
        Self::Membership,
        Self::RoleBinding,
        Self::ServiceAccount,
        Self::ApiKey,
        Self::UsageReport,
        Self::InferenceTrace,
        Self::ReconciliationItem,
        Self::CatalogRelease,
        Self::CatalogActivation,
        Self::ProviderCredential,
        Self::PriceOverride,
        Self::RateLimitPolicy,
        Self::BudgetPolicy,
        Self::WebhookEndpoint,
        Self::WebhookDelivery,
        Self::CatalogSyncJob,
        Self::PriceMapping,
        Self::PriceSourceFetch,
        Self::RetentionPolicy,
        Self::TracePolicy,
        Self::DataExport,
        Self::CloudPrincipalBinding,
        Self::Invitation,
        Self::IdentityProvider,
        Self::ConfigPin,
        Self::UsageExport,
        Self::CatalogAutoOffer,
        Self::Agent,
        Self::AgentToken,
        Self::OrganizationDomain,
        Self::CatalogPricePolicy,
        Self::CatalogPriceHold,
        Self::EndUserSetting,
        Self::EndUserPlan,
        Self::EndUser,
        Self::EndUserCreditGrant,
        Self::RoutingPolicy,
        Self::ModelCardNote,
        Self::ModelCardImport,
        Self::ModelEvaluationSet,
        Self::ModelEvaluationRun,
        Self::ModelMaster,
        Self::ModelMasterLink,
        Self::ModelAgreement,
        Self::BatchSettings,
        Self::BatchStaging,
        Self::Batch,
    ];

    /// The target type's name, as serialized and stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Team => "team",
            Self::Membership => "membership",
            Self::RoleBinding => "role_binding",
            Self::ServiceAccount => "service_account",
            Self::ApiKey => "api_key",
            Self::UsageReport => "usage_report",
            Self::InferenceTrace => "inference_trace",
            Self::ReconciliationItem => "reconciliation_item",
            Self::CatalogRelease => "catalog_release",
            Self::CatalogActivation => "catalog_activation",
            Self::ProviderCredential => "provider_credential",
            Self::PriceOverride => "price_override",
            Self::RateLimitPolicy => "rate_limit_policy",
            Self::BudgetPolicy => "budget_policy",
            Self::WebhookEndpoint => "webhook_endpoint",
            Self::WebhookDelivery => "webhook_delivery",
            Self::CatalogSyncJob => "catalog_sync_job",
            Self::PriceMapping => "price_mapping",
            Self::PriceSourceFetch => "price_source_fetch",
            Self::RetentionPolicy => "retention_policy",
            Self::TracePolicy => "trace_policy",
            Self::DataExport => "data_export",
            Self::CloudPrincipalBinding => "cloud_principal_binding",
            Self::Invitation => "invitation",
            Self::IdentityProvider => "identity_provider",
            Self::ConfigPin => "config_pin",
            Self::UsageExport => "usage_export",
            Self::CatalogAutoOffer => "catalog_auto_offer",
            Self::Agent => "agent",
            Self::AgentToken => "agent_token",
            Self::OrganizationDomain => "organization_domain",
            Self::CatalogPricePolicy => "catalog_price_policy",
            Self::CatalogPriceHold => "catalog_price_hold",
            Self::EndUserSetting => "end_user_setting",
            Self::EndUserPlan => "end_user_plan",
            Self::EndUser => "end_user",
            Self::EndUserCreditGrant => "end_user_credit_grant",
            Self::RoutingPolicy => "routing_policy",
            Self::ModelCardNote => "model_card_note",
            Self::ModelCardImport => "model_card_import",
            Self::ModelEvaluationSet => "model_evaluation_set",
            Self::ModelEvaluationRun => "model_evaluation_run",
            Self::ModelMaster => "model_master",
            Self::ModelMasterLink => "model_master_link",
            Self::ModelAgreement => "model_agreement",
            Self::BatchSettings => "batch_settings",
            Self::BatchStaging => "batch_staging",
            Self::Batch => "batch",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Active,
    Revoked,
    Disabled,
    Pending,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKeyPrefix(String);

impl PublicKeyPrefix {
    pub fn new(value: impl Into<String>) -> Result<Self, AuditActionError> {
        let value = value.into();
        if value.len() != 23
            || !value.starts_with("gw_")
            || !value[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AuditActionError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PublicKeyPrefix {
    type Error = AuditActionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PublicKeyPrefix> for String {
    fn from(value: PublicKeyPrefix) -> Self {
        value.0
    }
}

/// An opaque action name with a deliberately narrow, non-secret grammar.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct AuditAction(String);

impl AuditAction {
    pub fn new(value: impl Into<String>) -> Result<Self, AuditActionError> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 {
            return Err(AuditActionError::Invalid);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._:-".contains(&byte)
        }) {
            return Err(AuditActionError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AuditAction {
    type Error = AuditActionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl FromStr for AuditAction {
    type Err = AuditActionError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditActionError {
    Invalid,
}

impl fmt::Display for AuditActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid audit action")
    }
}
impl std::error::Error for AuditActionError {}

/// Closed, action-oriented detail vocabulary. Every member is safe to audit
/// by construction and carries no arbitrary user string.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuditDetails {
    None,
    ApiKeyCreated { key_id: ApiKeyId },
    ApiKeyRevoked { key_id: ApiKeyId },
    ServiceAccountStateChanged { enabled: bool },
    MembershipRoleChanged { role: MembershipRole },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdminAuditDraft {
    pub organization_id: OrganizationId,
    pub actor: AuditActor,
    pub target: AuditTarget,
    pub action: AuditAction,
    pub outcome: AuditOutcome,
    pub correlation_id: CorrelationId,
    pub team_id: Option<TeamId>,
    pub details: AuditDetails,
}

/// Allowlisted before/after values for extended admin audit events. Keeping
/// this a closed struct prevents credentials, digests, request bodies, and
/// idempotency keys from reaching audit storage by construction.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSnapshot {
    pub enabled: Option<bool>,
    pub status: Option<AuditStatus>,
    pub role: Option<AdminRole>,
    pub scopes: Option<Vec<ApiKeyScope>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub public_prefix: Option<PublicKeyPrefix>,
    pub version: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdminAuditEvent {
    pub actor: AuditActor,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub target: AuditTargetResource,
    pub action: AuditAction,
    pub target_id: Uuid,
    pub outcome: AuditOutcome,
    pub request_id: Uuid,
    pub before: Option<AuditSnapshot>,
    pub after: Option<AuditSnapshot>,
}

impl AdminAuditEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        actor: AuditActor,
        organization_id: OrganizationId,
        team_id: Option<TeamId>,
        target: AuditTargetResource,
        action: AuditAction,
        target_id: Uuid,
        outcome: AuditOutcome,
        request_id: Uuid,
        before: Option<AuditSnapshot>,
        after: Option<AuditSnapshot>,
    ) -> Self {
        Self {
            actor,
            organization_id,
            team_id,
            target,
            action,
            target_id,
            outcome,
            request_id,
            before,
            after,
        }
    }
}

impl AdminAuditDraft {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        organization_id: OrganizationId,
        actor: AuditActor,
        target: AuditTarget,
        action: AuditAction,
        outcome: AuditOutcome,
        correlation_id: CorrelationId,
        team_id: Option<TeamId>,
        details: AuditDetails,
    ) -> Self {
        Self {
            organization_id,
            actor,
            target,
            action,
            outcome,
            correlation_id,
            team_id,
            details,
        }
    }
}

#[cfg(test)]
mod audit_target_names {
    use super::AuditTargetResource;

    #[test]
    fn every_target_type_is_listed_once_under_its_serialized_name() {
        let mut seen = std::collections::BTreeSet::new();
        for target in AuditTargetResource::ALL {
            let serialized = serde_json::to_value(target).unwrap();
            assert_eq!(serialized.as_str(), Some(target.as_str()));
            assert!(
                seen.insert(target.as_str()),
                "{} listed twice",
                target.as_str()
            );
        }
        // `as_str` is an exhaustive match, so a new variant must be named
        // there; this keeps `ALL` as long as the enum.
        assert_eq!(seen.len(), 49);
    }
}
