//! Model availability evidence (D-018, `docs/model-availability-design.md`).
//!
//! A provider's listing is not proof that an account can call a model: Bedrock
//! lists models the account is denied or that are retired for it, and
//! Google's publisher catalog is not per location. Evidence is what the
//! gateway has itself seen, per organization, provider account and model
//! identifier: a call that succeeded (`verified`) or one the provider refused
//! for that model (`refused`, with a reason).
//!
//! Only refusals that are about the model count: access denied (403) or not
//! found (404). A bad credential is about the account, whether the provider
//! answers 401 or, like AWS, 403 with a credential error type, and a
//! rejected request, a throttle or an outage says nothing about whether the
//! account may call the model, so they record nothing. A 403 can still be
//! account-wide (an identity that lost its inference role refuses every
//! model alike), so automatic offers act on a refusal only once another
//! model of the account has succeeded since.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    identity::OrganizationId,
    inference::{ProviderError, ProviderErrorCategory},
};

pub const MAX_PROVIDER_CODE_BYTES: usize = 128;

/// The provider code an adapter gives a refusal because the account's
/// per-model quota is used up or zero (Agent Platform's
/// `RESOURCE_EXHAUSTED` for `..._per_base_model`). A probe makes one call,
/// so its 429 of this kind records `requires_action: quota_zero`; traffic's
/// never does, since busy traffic exhausts a quota that is not zero.
pub const BASE_MODEL_QUOTA_CODE: &str = "RESOURCE_EXHAUSTED:base_model_quota";

/// The provider code an adapter gives a refusal because the account has not
/// enabled the model in Model Garden (its terms are not accepted), which
/// records `requires_action: enable_in_model_garden` from any call.
pub const MODEL_GARDEN_CODE_SUFFIX: &str = ":model_garden";

/// Error types providers give a 403 for credentials that are invalid,
/// expired or badly signed: the account's, never the model's. AWS answers
/// a revoked or unknown key with `UnrecognizedClientException`.
const CREDENTIAL_ERROR_TYPES: &[&str] = &[
    "UnrecognizedClientException",
    "InvalidSignatureException",
    "IncompleteSignatureException",
    "SignatureDoesNotMatch",
    "ExpiredTokenException",
    "ExpiredToken",
    "InvalidClientTokenId",
    "MissingAuthenticationTokenException",
    "InvalidAccessKeyId",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    /// A call succeeded.
    Verified,
    /// A call was refused for this model.
    Refused,
    /// A read-only control-plane check says the account may call it.
    Entitled,
    /// Offered by the provider, but something the tenant does stands in the
    /// way.
    RequiresAction,
}

impl EvidenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Refused => "refused",
            Self::Entitled => "entitled",
            Self::RequiresAction => "requires_action",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceReason {
    /// The provider denied the account access to the model (403): Bedrock's
    /// `AccessDeniedException` for a model the account may not use, or an
    /// identity without permission to call it.
    AccessDenied,
    /// The provider does not serve the model where the gateway calls it
    /// (404): retired for the account, absent in the location, or a
    /// deployment that no longer exists.
    NotFound,
    /// A control-plane check found the identity the gateway calls with
    /// lacks the permission to call the model (`requires_action`): on Azure,
    /// the inference role on the resource; on Bedrock, IAM authorization
    /// for the model.
    PermissionMissing,
    /// The account has not accepted the model's offer terms (`requires_action`):
    /// on Bedrock, the model's AWS Marketplace agreement, which a call
    /// refuses until it is in place.
    AgreementRequired,
    /// The provider does not entitle the account to the model
    /// (`requires_action`): on Bedrock, an account-level restriction the
    /// tenant lifts with AWS, not with a role or an agreement.
    NotEntitled,
    /// The account's quota for the model is zero (`requires_action`): on
    /// Agent Platform, a partner model whose per-model request quota the
    /// tenant has not had raised, so every call is refused as exhausted.
    QuotaZero,
    /// An organization policy forbids the account to call the model
    /// (`requires_action`): on Agent Platform, the project's effective
    /// `vertexai.allowedModels` policy, which the tenant's policy
    /// administrator changes.
    PolicyDenied,
    /// The account has not enabled the model where its provider requires it
    /// (`requires_action`): on Agent Platform, a Model Garden partner model
    /// whose terms the project has not accepted.
    EnableInModelGarden,
}

impl EvidenceReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
            Self::NotFound => "not_found",
            Self::PermissionMissing => "permission_missing",
            Self::AgreementRequired => "agreement_required",
            Self::NotEntitled => "not_entitled",
            Self::QuotaZero => "quota_zero",
            Self::PolicyDenied => "policy_denied",
            Self::EnableInModelGarden => "enable_in_model_garden",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Traffic,
    Probe,
    ControlPlane,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Traffic => "traffic",
            Self::Probe => "probe",
            Self::ControlPlane => "control_plane",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct UnknownEvidenceValue;

impl FromStr for EvidenceState {
    type Err = UnknownEvidenceValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [
            Self::Verified,
            Self::Refused,
            Self::Entitled,
            Self::RequiresAction,
        ]
        .into_iter()
        .find(|state| state.as_str() == value)
        .ok_or(UnknownEvidenceValue)
    }
}

impl FromStr for EvidenceReason {
    type Err = UnknownEvidenceValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [
            Self::AccessDenied,
            Self::NotFound,
            Self::PermissionMissing,
            Self::AgreementRequired,
            Self::NotEntitled,
            Self::QuotaZero,
            Self::PolicyDenied,
            Self::EnableInModelGarden,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == value)
        .ok_or(UnknownEvidenceValue)
    }
}

impl FromStr for EvidenceSource {
    type Err = UnknownEvidenceValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [Self::Traffic, Self::Probe, Self::ControlPlane]
            .into_iter()
            .find(|source| source.as_str() == value)
            .ok_or(UnknownEvidenceValue)
    }
}

/// What an observation says about whether the account can call the model:
/// a call's outcome (verified, refused), or a control-plane check's
/// (entitled, requires action).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Verified,
    Refused(EvidenceReason),
    Entitled,
    RequiresAction(EvidenceReason),
}

/// What a read-only control-plane check says about calling a listed model
/// (D-018): nothing it reads stands in the way, or something the tenant can
/// change does. Carried by a discovered model, recorded as evidence. A
/// check is not a call: `Entitled` can still be refused (Bedrock reports
/// some vendor denials only on the call).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Entitlement {
    Entitled,
    /// The identity the gateway calls with lacks the permission.
    PermissionMissing,
    /// The account has not accepted the model's offer terms.
    AgreementRequired,
    /// The provider restricts the model for the account.
    NotEntitled,
    /// The account's quota for the model is zero.
    QuotaZero,
    /// An organization policy forbids the account to call the model.
    PolicyDenied,
}

impl Entitlement {
    pub fn verdict(self) -> Verdict {
        match self {
            Self::Entitled => Verdict::Entitled,
            Self::PermissionMissing => Verdict::RequiresAction(EvidenceReason::PermissionMissing),
            Self::AgreementRequired => Verdict::RequiresAction(EvidenceReason::AgreementRequired),
            Self::NotEntitled => Verdict::RequiresAction(EvidenceReason::NotEntitled),
            Self::QuotaZero => Verdict::RequiresAction(EvidenceReason::QuotaZero),
            Self::PolicyDenied => Verdict::RequiresAction(EvidenceReason::PolicyDenied),
        }
    }
}

impl Verdict {
    /// The verdict a failed call gives, if any: only a refusal about the
    /// model itself counts.
    pub fn of_error(error: &ProviderError) -> Option<Self> {
        // A request that never reached the provider says nothing.
        if error.was_not_sent() {
            return None;
        }
        // Bad credentials are the account's, not the model's.
        // Bedrock gives the type with its first letter lowercased.
        if error.error_type().is_some_and(|code| {
            CREDENTIAL_ERROR_TYPES
                .iter()
                .any(|credential| credential.eq_ignore_ascii_case(code))
        }) {
            return None;
        }
        if error
            .error_type()
            .is_some_and(|code| code.ends_with(MODEL_GARDEN_CODE_SUFFIX))
        {
            return Some(Self::RequiresAction(EvidenceReason::EnableInModelGarden));
        }
        match (error.category(), error.http_status()) {
            (ProviderErrorCategory::Authentication, _) => None,
            (ProviderErrorCategory::Authorization, _) | (_, Some(403)) => {
                Some(Self::Refused(EvidenceReason::AccessDenied))
            }
            (_, Some(404)) => Some(Self::Refused(EvidenceReason::NotFound)),
            _ => None,
        }
    }

    pub fn state(self) -> EvidenceState {
        match self {
            Self::Verified => EvidenceState::Verified,
            Self::Refused(_) => EvidenceState::Refused,
            Self::Entitled => EvidenceState::Entitled,
            Self::RequiresAction(_) => EvidenceState::RequiresAction,
        }
    }

    pub fn reason(self) -> Option<EvidenceReason> {
        match self {
            Self::Verified | Self::Entitled => None,
            Self::Refused(reason) | Self::RequiresAction(reason) => Some(reason),
        }
    }
}

/// The account, model and deployment an observation is about.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EvidenceKey {
    pub organization_id: OrganizationId,
    pub adapter_instance_key: String,
    /// A release may redeclare an account key with another family, which is
    /// another account.
    pub family: String,
    pub model: String,
}

/// One observation, as recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub key: EvidenceKey,
    pub verdict: Verdict,
    pub source: EvidenceSource,
    pub deployment: Option<String>,
    pub provider_status: Option<u16>,
    pub provider_code: Option<String>,
    pub observed_at: DateTime<Utc>,
}

impl Observation {
    /// What a control-plane check during discovery found for a listed
    /// model; the deployment is the model identifier requests name.
    pub fn of_entitlement(
        key: EvidenceKey,
        entitlement: Entitlement,
        observed_at: DateTime<Utc>,
    ) -> Self {
        let deployment = (key.model.len() <= 255).then(|| key.model.clone());
        Self {
            key,
            verdict: entitlement.verdict(),
            source: EvidenceSource::ControlPlane,
            deployment,
            provider_status: None,
            provider_code: None,
            observed_at,
        }
    }

    /// What a call through `deployment` showed. `None` when the call shows
    /// nothing about the model.
    pub fn of_call(
        key: EvidenceKey,
        deployment: &str,
        result: Result<(), &ProviderError>,
        observed_at: DateTime<Utc>,
    ) -> Option<Self> {
        Self::of_attempt(
            key,
            deployment,
            result,
            EvidenceSource::Traffic,
            observed_at,
        )
    }

    /// What the gateway's own probe of `model` showed; the deployment is the
    /// model identifier probed. `None` when it shows nothing.
    pub fn of_probe(
        key: EvidenceKey,
        result: Result<(), &ProviderError>,
        observed_at: DateTime<Utc>,
    ) -> Option<Self> {
        let model = key.model.clone();
        Self::of_attempt(key, &model, result, EvidenceSource::Probe, observed_at)
    }

    fn of_attempt(
        key: EvidenceKey,
        deployment: &str,
        result: Result<(), &ProviderError>,
        source: EvidenceSource,
        observed_at: DateTime<Utc>,
    ) -> Option<Self> {
        let (verdict, provider_status, provider_code) = match result {
            Ok(()) => (Verdict::Verified, None, None),
            Err(error) => (
                match source {
                    EvidenceSource::Probe if is_base_model_quota(error) => {
                        Verdict::RequiresAction(EvidenceReason::QuotaZero)
                    }
                    _ => Verdict::of_error(error)?,
                },
                error.http_status(),
                error
                    .error_type()
                    .filter(|code| !code.is_empty() && code.len() <= MAX_PROVIDER_CODE_BYTES)
                    .map(str::to_owned),
            ),
        };
        Some(Self {
            key,
            verdict,
            source,
            deployment: (!deployment.is_empty() && deployment.len() <= 255)
                .then(|| deployment.to_owned()),
            provider_status: provider_status.filter(|status| (100..=599).contains(status)),
            provider_code,
            observed_at,
        })
    }
}

/// Whether `error` is a refusal for the account's per-model quota (see
/// [`BASE_MODEL_QUOTA_CODE`]).
pub fn is_base_model_quota(error: &ProviderError) -> bool {
    error.http_status() == Some(429) && error.error_type() == Some(BASE_MODEL_QUOTA_CODE)
}

/// The latest evidence about a model, as the admin API shows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceView {
    pub state: EvidenceState,
    pub reason: Option<EvidenceReason>,
    pub source: EvidenceSource,
    pub deployment: Option<String>,
    pub provider_status: Option<u16>,
    pub provider_code: Option<String>,
    pub observed_at: DateTime<Utc>,
    /// The last time a call succeeded, also when the model has since been
    /// refused.
    pub verified_at: Option<DateTime<Utc>>,
}

impl EvidenceView {
    pub fn refusal(&self) -> Option<EvidenceReason> {
        (self.state == EvidenceState::Refused)
            .then_some(self.reason)
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn error(category: ProviderErrorCategory, status: Option<u16>) -> ProviderError {
        ProviderError::from_parts(category, None, None, status, None, None, None).unwrap()
    }

    #[test]
    fn only_refusals_about_the_model_count() {
        use ProviderErrorCategory::*;
        let cases = [
            (Authorization, Some(403), Some(EvidenceReason::AccessDenied)),
            (Authorization, None, Some(EvidenceReason::AccessDenied)),
            (InvalidRequest, Some(404), Some(EvidenceReason::NotFound)),
            (Provider, Some(404), Some(EvidenceReason::NotFound)),
            (Authentication, Some(401), None),
            (Authentication, Some(403), None),
            (InvalidRequest, Some(400), None),
            (RateLimited, Some(429), None),
            (Unavailable, Some(503), None),
            (Timeout, None, None),
            (Cancelled, None, None),
        ];
        for (category, status, expected) in cases {
            assert_eq!(
                Verdict::of_error(&error(category, status)).and_then(Verdict::reason),
                expected,
                "{category:?} {status:?}"
            );
        }
    }

    #[test]
    fn credential_errors_are_the_accounts_not_the_models() {
        for code in [
            "UnrecognizedClientException",
            "ExpiredTokenException",
            // As the Bedrock adapter reports them.
            "unrecognizedClientException",
            "invalidSignatureException",
        ] {
            let error = ProviderError::from_parts(
                ProviderErrorCategory::Authorization,
                None,
                None,
                Some(403),
                Some(code.into()),
                None,
                None,
            )
            .unwrap();
            assert_eq!(Verdict::of_error(&error), None, "{code}");
        }
        let denied = ProviderError::from_parts(
            ProviderErrorCategory::Authorization,
            None,
            None,
            Some(403),
            Some("AccessDeniedException".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            Verdict::of_error(&denied),
            Some(Verdict::Refused(EvidenceReason::AccessDenied))
        );
    }

    #[test]
    fn model_garden_refusals_and_a_probes_base_model_quota_need_action() {
        let with = |category, status, code: &str| {
            ProviderError::from_parts(
                category,
                None,
                None,
                Some(status),
                Some(code.into()),
                None,
                None,
            )
            .unwrap()
        };
        let key = EvidenceKey {
            organization_id: OrganizationId(Uuid::nil()),
            adapter_instance_key: "gemini-1".into(),
            family: "gemini".into(),
            model: "anthropic/claude-haiku-4-5".into(),
        };
        let now = Utc::now();
        for status in [400, 403] {
            let terms = with(
                ProviderErrorCategory::InvalidRequest,
                status,
                "FAILED_PRECONDITION:model_garden",
            );
            assert_eq!(
                Verdict::of_error(&terms),
                Some(Verdict::RequiresAction(EvidenceReason::EnableInModelGarden))
            );
        }
        let quota = with(
            ProviderErrorCategory::RateLimited,
            429,
            BASE_MODEL_QUOTA_CODE,
        );
        assert!(is_base_model_quota(&quota));
        let probed = Observation::of_probe(key.clone(), Err(&quota), now).unwrap();
        assert_eq!(
            probed.verdict,
            Verdict::RequiresAction(EvidenceReason::QuotaZero)
        );
        assert_eq!(probed.provider_status, Some(429));
        // Traffic exhausts quotas that are not zero.
        assert_eq!(
            Observation::of_call(key.clone(), "dep", Err(&quota), now),
            None
        );
        // Another throttle says nothing, probe or not.
        let throttled = with(
            ProviderErrorCategory::RateLimited,
            429,
            "RESOURCE_EXHAUSTED",
        );
        assert!(!is_base_model_quota(&throttled));
        assert_eq!(Observation::of_probe(key, Err(&throttled), now), None);
    }

    #[test]
    fn a_request_never_sent_says_nothing() {
        let error = ProviderError::not_sent(ProviderErrorCategory::Authorization);
        assert_eq!(Verdict::of_error(&error), None);
    }

    #[test]
    fn a_call_records_its_verdict_and_the_providers_status() {
        let key = EvidenceKey {
            organization_id: OrganizationId(Uuid::nil()),
            adapter_instance_key: "bedrock-1".into(),
            family: "bedrock".into(),
            model: "us.anthropic.claude-sonnet-5-5".into(),
        };
        let now = Utc::now();
        let ok = Observation::of_call(key.clone(), "dep", Ok(()), now).unwrap();
        assert_eq!(ok.verdict, Verdict::Verified);
        assert_eq!(ok.deployment.as_deref(), Some("dep"));
        let denied = error(ProviderErrorCategory::Authorization, Some(403));
        let refused = Observation::of_call(key.clone(), "dep", Err(&denied), now).unwrap();
        assert_eq!(
            refused.verdict,
            Verdict::Refused(EvidenceReason::AccessDenied)
        );
        assert_eq!(refused.provider_status, Some(403));
        let throttled = error(ProviderErrorCategory::RateLimited, Some(429));
        assert_eq!(Observation::of_call(key, "dep", Err(&throttled), now), None);
    }

    #[test]
    fn values_round_trip() {
        for state in [
            EvidenceState::Verified,
            EvidenceState::Refused,
            EvidenceState::Entitled,
            EvidenceState::RequiresAction,
        ] {
            assert_eq!(state.as_str().parse(), Ok(state));
        }
        for reason in [
            EvidenceReason::AccessDenied,
            EvidenceReason::NotFound,
            EvidenceReason::PermissionMissing,
            EvidenceReason::AgreementRequired,
            EvidenceReason::NotEntitled,
            EvidenceReason::QuotaZero,
            EvidenceReason::PolicyDenied,
            EvidenceReason::EnableInModelGarden,
        ] {
            assert_eq!(reason.as_str().parse(), Ok(reason));
        }
        for source in [
            EvidenceSource::Traffic,
            EvidenceSource::Probe,
            EvidenceSource::ControlPlane,
        ] {
            assert_eq!(source.as_str().parse(), Ok(source));
        }
    }
}
