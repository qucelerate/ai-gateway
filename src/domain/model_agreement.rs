//! Model agreements (D-035): the offer terms a provider requires an account
//! to accept before it serves a model, and what accepting them through the
//! gateway did.
//!
//! On Bedrock, a third-party model sold through AWS Marketplace is refused
//! (`agreementAvailability` not `AVAILABLE`, evidence `agreement_required`)
//! until the account has an agreement for one of the model's offers. Bedrock
//! starts one on the first call when the caller may subscribe, but the
//! gateway's role usually may not, and a call is a poor way to agree to
//! terms. An administrator instead reads each model's offers (rate card,
//! legal and support terms) and accepts them; the gateway then watches the
//! agreement until Bedrock reports it available and proves it with one
//! probe.
//!
//! Accepting an agreement is not the same as the provider allowing the
//! account to use the model: AWS can still refuse a model for an account it
//! has not enabled, which only a request to AWS changes. A probe refused
//! after an accepted agreement says so (`access_request_required`).
//!
//! Offer tokens are what the provider accepts an agreement with. They are
//! opaque, never stored, logged or returned: [`OfferToken`] has no
//! serialization and a redacted `Debug`.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Most models one acceptance names.
pub const MAX_ACCEPT_MODELS: usize = 100;
/// Most rates of one offer's rate card shown (the region's, else the first).
pub const MAX_RATES: usize = 100;
/// Longest provider-supplied text kept (an error message, a term).
pub const MAX_DETAIL_BYTES: usize = 512;

/// A provider's opaque token for one offer: sent back to accept it, never
/// shown or kept.
#[derive(Clone, Eq, PartialEq)]
pub struct OfferToken(String);

impl OfferToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OfferToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OfferToken(<redacted>)")
    }
}

/// One rate of an offer's usage-based pricing.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Rate {
    pub dimension: String,
    pub price: Option<String>,
    pub unit: Option<String>,
    pub description: Option<String>,
}

/// What accepting an offer agrees to, as the provider describes it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OfferTerms {
    /// The rates billed in the account's region where the provider's rate
    /// card names it (`rate_card_region`), else the first
    /// [`MAX_RATES`] of the whole card.
    pub rate_card: Vec<Rate>,
    /// The rate card's region code (Bedrock's `USE2` for `us-east-2`), when
    /// the card was narrowed to it.
    pub rate_card_region: Option<String>,
    /// How many rates the whole card has.
    pub rate_card_total: usize,
    /// The legal terms (EULA): a link the provider signs, which expires.
    pub legal_url: Option<String>,
    pub refund_policy: Option<String>,
    pub agreement_duration: Option<String>,
}

/// One offer for a model.
#[derive(Clone, Debug)]
pub struct AgreementOffer {
    pub offer_id: String,
    pub token: OfferToken,
    pub terms: OfferTerms,
}

/// An offer as the admin API shows it: everything but the token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OfferView {
    pub offer_id: String,
    #[serde(flatten)]
    pub terms: OfferTerms,
}

impl From<&AgreementOffer> for OfferView {
    fn from(offer: &AgreementOffer) -> Self {
        Self {
            offer_id: offer.offer_id.clone(),
            terms: offer.terms.clone(),
        }
    }
}

/// The provider's state of a model's agreement for the account.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAgreementStatus {
    Available,
    Pending,
    NotAvailable,
    Error,
}

/// What the provider's availability check says about a model's agreement
/// and whatever else stands in the way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgreementAvailability {
    pub agreement: ProviderAgreementStatus,
    /// The provider's explanation of an `Error`, bounded.
    pub error_message: Option<String>,
    /// Whether the provider entitles the account to the model; `false` is an
    /// account-level restriction only the provider lifts.
    pub entitled: Option<bool>,
    /// Whether the role may call the model (IAM authorization).
    pub authorized: Option<bool>,
    /// Whether the region serves the model.
    pub region_available: Option<bool>,
    /// What the whole answer says about calling the model, by the rule
    /// discovery's listing uses: `Entitled` only when the agreement, the
    /// role's authorization, the account's entitlement and the region all
    /// allow it; `None` when a field is unknown or the region does not
    /// serve it.
    pub entitlement: Option<crate::domain::model_evidence::Entitlement>,
}

/// Where an agreement accepted through the gateway stands.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgreementStatus {
    /// Accepted; the provider has not reported it available yet.
    Pending,
    /// The provider reports it available, but no probe has proved a call.
    Available,
    /// A probe after acceptance succeeded.
    Verified,
    /// A probe after acceptance was refused: see the reason.
    Refused,
    /// The agreement could not be made, or the provider reports it failed.
    Failed,
}

impl AgreementStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Available => "available",
            Self::Verified => "verified",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Pending,
            Self::Available,
            Self::Verified,
            Self::Refused,
            Self::Failed,
        ]
        .into_iter()
        .find(|status| status.as_str() == value)
    }
}

/// Why an agreement is where it is, or why accepting it failed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgreementReason {
    /// The connection's role may not read offers or make agreements: the
    /// opt-in "accept model agreements" permissions are missing.
    AgreementPermissionMissing,
    /// The agreement is in place but the provider still refuses the model
    /// for the account, or does not entitle the account to it: the
    /// provider has not enabled the model for the account, which takes a
    /// request to the provider (sales or a support case).
    AccessRequestRequired,
    /// The agreement is in place, but the role the gateway calls with may
    /// not invoke the model (IAM authorization).
    InvokePermissionMissing,
    /// The provider does not serve the model where the gateway called it.
    NotFound,
    /// The provider reports the agreement failed (`detail` says why).
    AgreementError,
    /// The provider still reports no agreement after the gateway stopped
    /// checking.
    AgreementNotAvailable,
    /// Still pending when the gateway stopped checking.
    StillPending,
    /// Available per the provider's check, but not called: no probe was
    /// asked for, or the probe settled nothing.
    NotVerified,
    /// The request ran out of time before the model was accepted; nothing
    /// was sent to the provider for it.
    NotAttempted,
    /// The request ended (the client left, or it failed) after recording
    /// that it would accept the model, and the provider reports no
    /// agreement.
    AcceptanceInterrupted,
    /// The model needs no agreement the gateway knows of.
    NotAgreementRequired,
    /// The provider failed the request (throttled, unavailable, rejected).
    ProviderError,
}

impl AgreementReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgreementPermissionMissing => "agreement_permission_missing",
            Self::AccessRequestRequired => "access_request_required",
            Self::InvokePermissionMissing => "invoke_permission_missing",
            Self::NotFound => "not_found",
            Self::AgreementError => "agreement_error",
            Self::AgreementNotAvailable => "agreement_not_available",
            Self::StillPending => "still_pending",
            Self::NotVerified => "not_verified",
            Self::NotAttempted => "not_attempted",
            Self::AcceptanceInterrupted => "acceptance_interrupted",
            Self::NotAgreementRequired => "not_agreement_required",
            Self::ProviderError => "provider_error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::AgreementPermissionMissing,
            Self::AccessRequestRequired,
            Self::InvokePermissionMissing,
            Self::NotFound,
            Self::AgreementError,
            Self::AgreementNotAvailable,
            Self::StillPending,
            Self::NotVerified,
            Self::NotAttempted,
            Self::AcceptanceInterrupted,
            Self::NotAgreementRequired,
            Self::ProviderError,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == value)
    }

    /// What an administrator does about it, in a sentence.
    pub fn detail(self) -> &'static str {
        match self {
            Self::AgreementPermissionMissing => {
                "The connection's role may not accept model agreements. Grant the optional \
                 \"accept model agreements\" permissions (bedrock:ListFoundationModelAgreementOffers, \
                 bedrock:CreateFoundationModelAgreement and the aws-marketplace subscription \
                 actions): see Connect a provider, or docs/operations.md."
            }
            Self::AccessRequestRequired => {
                "The agreement is in place, but AWS still does not let this account use the \
                 model. Accepting an agreement does not enable a model AWS has not made \
                 available to the account: request access from AWS (your account team, or a \
                 support case). See \"Model access on your cloud account\" in docs/operations.md."
            }
            Self::InvokePermissionMissing => {
                "The agreement is in place, but AWS reports the gateway's role may not call the \
                 model. Grant the role Bedrock's invoke permissions for it (see Connect a \
                 provider)."
            }
            Self::NotFound => "Bedrock does not serve the model where the gateway called it.",
            Self::AgreementError => "AWS reports the agreement failed.",
            Self::AgreementNotAvailable => {
                "AWS still reports no agreement for the model. Accept it again, or check the \
                 account's AWS Marketplace subscriptions and payment method."
            }
            Self::StillPending => {
                "AWS still reports the agreement pending; the gateway stopped checking. \
                 Discovery records the model once AWS makes it available."
            }
            Self::NotVerified => {
                "Available per AWS, not yet called. AWS's availability check can report a model \
                 available that a call still refuses, so the first real request is the true check."
            }
            Self::NotAttempted => {
                "The request ran out of time before this model was accepted; nothing was sent \
                 to AWS for it. Accept it again."
            }
            Self::AcceptanceInterrupted => {
                "The acceptance was interrupted before AWS answered, and AWS reports no \
                 agreement. Accept it again."
            }
            Self::NotAgreementRequired => {
                "The gateway has no evidence that this model needs an agreement."
            }
            Self::ProviderError => "AWS failed the request; try again later.",
        }
    }
}

/// What accepting one model's agreement did.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptOutcome {
    Accepted,
    /// The provider already had an agreement (available or pending).
    AlreadyAccepted,
    /// AWS did not answer in time: the agreement may or may not exist. The
    /// gateway checks with AWS and records which.
    Unconfirmed,
    Failed,
}

impl AcceptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::AlreadyAccepted => "already_accepted",
            Self::Unconfirmed => "unconfirmed",
            Self::Failed => "failed",
        }
    }
}

/// One model of an acceptance request: the model and the offer the
/// administrator saw.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptModel {
    pub model: String,
    pub offer_id: String,
}

/// `POST .../agreements/{key}/accept`: the models to accept, each with the
/// offer the administrator was shown. There is no "all": the server never
/// accepts an offer nobody named.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptInput {
    pub models: Vec<AcceptModel>,
    /// Once AWS reports an agreement available, also call the model once
    /// (one 1-token request per foundation model, billed by the provider).
    /// Without it only the free availability check runs.
    #[serde(default)]
    pub verify: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AcceptInputError {
    #[error("name at least one model, each with the offer id shown")]
    Selection,
    #[error("at most {MAX_ACCEPT_MODELS} models")]
    TooMany,
    #[error("a model is named more than once")]
    Duplicate,
    #[error("a model identifier is not a Bedrock foundation model identifier")]
    Model,
    #[error("an offer identifier is invalid")]
    Offer,
}

impl AcceptInput {
    pub fn validate(&self) -> Result<(), AcceptInputError> {
        if self.models.is_empty() {
            return Err(AcceptInputError::Selection);
        }
        if self.models.len() > MAX_ACCEPT_MODELS {
            return Err(AcceptInputError::TooMany);
        }
        let mut seen = std::collections::HashSet::new();
        for model in &self.models {
            if !foundation_model_id(&model.model) {
                return Err(AcceptInputError::Model);
            }
            if !offer_id(&model.offer_id) {
                return Err(AcceptInputError::Offer);
            }
            if !seen.insert(model.model.as_str()) {
                return Err(AcceptInputError::Duplicate);
            }
        }
        Ok(())
    }
}

/// A Bedrock foundation model identifier as `BedrockModelId` allows it,
/// without an ARN: `provider.name[:version[:context]]`, lowercase.
pub fn foundation_model_id(value: &str) -> bool {
    let (name, versions) = value.split_once(':').unwrap_or((value, ""));
    let label = |part: &str| {
        (1..=63).contains(&part.len())
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let parts: Vec<&str> = name.split('.').collect();
    value.len() <= 140
        && (2..=4).contains(&parts.len())
        && parts.iter().all(|part| label(part))
        && (versions.is_empty() || versions.split(':').count() <= 2)
        && (versions.is_empty() || versions.split(':').all(label))
}

/// An offer identifier: ASCII letters, digits and `-`, at most 64 bytes.
pub fn offer_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What one model's acceptance did, as the API returns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptResult {
    pub model: String,
    pub offer_id: Option<String>,
    pub outcome: AcceptOutcome,
    /// Where the agreement stands now: `pending` for one just made.
    pub status: AgreementStatus,
    pub reason: Option<AgreementReason>,
    pub detail: Option<String>,
    /// The provider's error code, for a failure.
    pub provider_code: Option<String>,
}

/// The response of an acceptance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptResponse {
    pub adapter_instance_key: String,
    pub results: Vec<AcceptResult>,
}

/// An agreement accepted through the gateway, and where it stands.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgreementView {
    pub status: AgreementStatus,
    pub reason: Option<AgreementReason>,
    pub detail: Option<String>,
    pub provider_code: Option<String>,
    pub offer_id: String,
    pub accepted_by: Option<uuid::Uuid>,
    pub accepted_at: DateTime<Utc>,
    pub checked_at: Option<DateTime<Utc>>,
    pub verified_at: Option<DateTime<Utc>>,
    /// Whether a billed 1-token probe was asked for.
    pub probe: bool,
    /// The model identifier the verification probe called.
    pub probed_model: Option<String>,
    /// Whether the gateway is still checking the agreement.
    pub checking: bool,
}

/// One model needing (or given) an agreement, as `GET .../agreements/{key}`
/// shows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgreementModelView {
    /// The foundation model the agreement is for.
    pub model: String,
    /// The listed identifiers (the model and its inference profiles) whose
    /// evidence says an agreement is required, or that were when accepted.
    pub unblocks: Vec<String>,
    /// The automatically offered deployments serving them.
    pub deployments: Vec<String>,
    /// Whether the model's evidence still says `agreement_required`.
    pub agreement_required: bool,
    pub offers: Vec<OfferView>,
    /// Why offers could not be read, if they could not.
    pub offers_error: Option<AgreementReason>,
    pub agreement: Option<AgreementView>,
}

/// Whether the connection's role may read offers (and so, with the same
/// opt-in permissions, accept them).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgreementPermission {
    /// Offers could be read.
    Granted,
    /// The role may not read offers.
    Missing,
    /// There was nothing to read, or the provider failed.
    Unknown,
}

/// `GET .../agreements/{key}`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgreementsView {
    pub adapter_instance_key: String,
    pub family: String,
    pub permission: AgreementPermission,
    pub models: Vec<AgreementModelView>,
}

/// Bounds provider-supplied text to [`MAX_DETAIL_BYTES`] on a character
/// boundary, turning line breaks and tabs into spaces and dropping other
/// control characters.
pub fn bounded_text(value: &str) -> String {
    let mut out = String::new();
    // Line breaks and tabs become spaces; other control characters go.
    for c in value
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
    {
        if out.len() + c.len_utf8() > MAX_DETAIL_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_never_print() {
        let token = OfferToken::new("AQICAHsecret");
        assert_eq!(format!("{token:?}"), "OfferToken(<redacted>)");
        let offer = AgreementOffer {
            offer_id: "offer-1".into(),
            token,
            terms: OfferTerms::default(),
        };
        assert!(!format!("{offer:?}").contains("AQICAH"));
        let view = serde_json::to_string(&OfferView::from(&offer)).unwrap();
        assert!(!view.contains("AQICAH"), "{view}");
    }

    #[test]
    fn model_identifiers_are_foundation_models() {
        for good in [
            "anthropic.claude-haiku-5-5",
            "anthropic.claude-3-5-sonnet-20240620-v1:0",
            "cohere.embed-v4:0",
            "meta.llama3-70b-instruct-v1:0:128k",
        ] {
            assert!(foundation_model_id(good), "{good}");
        }
        for bad in [
            "",
            "anthropic",
            "Anthropic.claude",
            "arn:aws:bedrock:us-east-1::foundation-model/a.b",
            "a.b/../c",
            "a..b",
            "a.b:1:2:3",
        ] {
            assert!(!foundation_model_id(bad), "{bad}");
        }
        assert!(offer_id("offer-u3aih6zr7uw5u"));
        assert!(!offer_id("offer 1"));
        assert!(!offer_id(&"o".repeat(65)));
    }

    #[test]
    fn a_request_names_each_model_with_its_offer() {
        let one = AcceptModel {
            model: "anthropic.claude-haiku-5-5".into(),
            offer_id: "offer-1".into(),
        };
        assert_eq!(
            AcceptInput::default().validate(),
            Err(AcceptInputError::Selection)
        );
        assert_eq!(
            AcceptInput {
                models: vec![one.clone(), one.clone()],
                verify: false
            }
            .validate(),
            Err(AcceptInputError::Duplicate)
        );
        let many = (0..=MAX_ACCEPT_MODELS)
            .map(|index| AcceptModel {
                model: format!("anthropic.claude-{index}"),
                offer_id: "offer-1".into(),
            })
            .collect();
        assert_eq!(
            AcceptInput {
                models: many,
                verify: false
            }
            .validate(),
            Err(AcceptInputError::TooMany)
        );
        assert!(
            AcceptInput {
                models: vec![one],
                verify: true
            }
            .validate()
            .is_ok()
        );
        // "all" is not a field: the server accepts only named offers.
        assert!(
            serde_json::from_value::<AcceptInput>(serde_json::json!({ "all": true, "models": [] }))
                .is_err()
        );
    }

    #[test]
    fn values_round_trip() {
        for status in ["pending", "available", "verified", "refused", "failed"] {
            assert_eq!(AgreementStatus::parse(status).unwrap().as_str(), status);
        }
        for reason in [
            "agreement_permission_missing",
            "access_request_required",
            "not_found",
            "agreement_error",
            "agreement_not_available",
            "still_pending",
            "not_verified",
            "not_attempted",
            "acceptance_interrupted",
            "not_agreement_required",
            "provider_error",
        ] {
            let parsed = AgreementReason::parse(reason).unwrap();
            assert_eq!(parsed.as_str(), reason);
            assert_eq!(
                serde_json::to_value(parsed).unwrap(),
                serde_json::json!(reason)
            );
        }
        assert_eq!(bounded_text(&"é".repeat(400)).len(), 512);
        assert_eq!(bounded_text("a\nb\tc\u{7}d"), "a b cd");
    }
}
