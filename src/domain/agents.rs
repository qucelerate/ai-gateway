//! Agents: non-human administrators that automation (CI, coding agents, the
//! `gwctl` CLI) uses to drive the admin API without a browser. An agent is
//! a user account without an OIDC identity and a member of exactly one
//! organization, so role bindings, permission checks and the audit trail
//! apply to it as to a person. It authenticates with an agent token,
//! `Authorization: Bearer gwa_…`, which acts only in its organization.

use super::identity::{OrganizationId, UserAccountId};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Every agent token starts with this, so it is told apart from API keys
/// (`gw_`) and secret scanners can recognise it.
pub const AGENT_TOKEN_PREFIX: &str = "gwa_";
/// Characters after the prefix: 32 random bytes, base64url.
const TOKEN_SECRET_LEN: usize = 43;
/// Most agents an organization may have, disabled ones included.
pub const MAX_AGENTS: i64 = 200;
/// Most tokens an agent may hold that are neither revoked nor expired.
pub const MAX_LIVE_TOKENS: i64 = 20;
/// Longest a token may be valid for.
pub const MAX_TOKEN_DAYS: u32 = 365;

/// A new agent token. Shown once, never stored or logged.
pub struct AgentToken(Zeroizing<String>);

impl AgentToken {
    pub fn generate() -> Self {
        let mut bytes = Zeroizing::new([0_u8; 32]);
        rand::rngs::OsRng.fill_bytes(bytes.as_mut());
        Self(Zeroizing::new(format!(
            "{AGENT_TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_ref())
        )))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `gwa_` and the first eight characters of the secret.
    pub fn public_prefix(&self) -> String {
        self.0[..AGENT_TOKEN_PREFIX.len() + 8].to_owned()
    }

    pub fn digest(&self) -> Vec<u8> {
        token_digest(self.as_str()).expect("a generated token is well-formed")
    }
}

impl std::fmt::Debug for AgentToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentToken(REDACTED)")
    }
}

/// The stored digest of a token as presented, or `None` for anything not
/// shaped like an agent token (so it can never match).
pub fn token_digest(token: &str) -> Option<Vec<u8>> {
    let secret = token.strip_prefix(AGENT_TOKEN_PREFIX)?;
    (secret.len() == TOKEN_SECRET_LEN
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then(|| Sha256::digest(token.as_bytes()).to_vec())
}

/// A name or description: trimmed, without control characters, within
/// `max` bytes.
pub fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// `POST …/agents`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInput {
    pub name: String,
    /// `owner`, `admin`, `auditor`, `member` or `read-only`.
    pub role: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `POST …/agents/{agent_id}/tokens`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTokenInput {
    pub name: String,
    /// Days until it expires, 1 to 365; it never expires when absent.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
    /// Limits the token to this service account's end-user settings, plans
    /// and users (D-028); the agent's whole organization when absent.
    #[serde(default)]
    pub service_account_id: Option<Uuid>,
}

impl AgentTokenInput {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_text(&self.name, 100) {
            return Err(
                "name must be 1 to 100 bytes without surrounding spaces or control characters",
            );
        }
        if self
            .expires_in_days
            .is_some_and(|days| !(1..=MAX_TOKEN_DAYS).contains(&days))
        {
            return Err("expires_in_days must be between 1 and 365");
        }
        Ok(())
    }
}

/// An agent as the API shows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentView {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    /// Its organization-wide membership role.
    pub role: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub tokens: Vec<AgentTokenView>,
}

/// A token's metadata: never the token or its digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTokenView {
    pub id: Uuid,
    pub name: String,
    pub public_prefix: String,
    /// `active`, `expired` or `revoked`.
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// The one service account the token is limited to, if any.
    pub service_account_id: Option<Uuid>,
}

impl AgentTokenView {
    pub fn status_at(
        revoked_at: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> &'static str {
        match (revoked_at, expires_at) {
            (Some(_), _) => "revoked",
            (None, Some(expires)) if expires <= now => "expired",
            _ => "active",
        }
    }
}

/// Whom a presented agent token acts as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentPrincipal {
    pub agent_id: UserAccountId,
    pub organization_id: OrganizationId,
    /// The one service account the token is limited to, if any.
    pub service_account_id: Option<Uuid>,
}

/// A created token: its metadata and, only in the response that created it,
/// the token itself.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreatedAgentToken {
    #[serde(flatten)]
    pub metadata: AgentTokenView,
    pub token: Option<String>,
    pub replay: bool,
}

impl std::fmt::Debug for CreatedAgentToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedAgentToken")
            .field("metadata", &self.metadata)
            .field("token", &self.token.as_ref().map(|_| "REDACTED"))
            .field("replay", &self.replay)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_random_and_digested() {
        let (a, b) = (AgentToken::generate(), AgentToken::generate());
        assert_ne!(a.as_str(), b.as_str());
        assert!(a.as_str().starts_with("gwa_"));
        assert_eq!(a.as_str().len(), 4 + 43);
        assert_eq!(a.public_prefix().len(), 12);
        assert!(a.as_str().starts_with(&a.public_prefix()));
        assert_eq!(a.digest().len(), 32);
        assert_eq!(token_digest(a.as_str()), Some(a.digest()));
        assert_eq!(format!("{a:?}"), "AgentToken(REDACTED)");
        for malformed in [
            "",
            "gwa_",
            "gw_0123456789abcdef0123",
            &a.as_str()[4..],
            &format!("{}=", a.as_str()),
            &format!("gwa_{}", "+".repeat(43)),
        ] {
            assert!(token_digest(malformed).is_none(), "{malformed}");
        }
    }

    #[test]
    fn token_input_is_bounded() {
        let input = |name: &str, days| AgentTokenInput {
            name: name.into(),
            expires_in_days: days,
            service_account_id: None,
        };
        assert!(input("ci", None).validate().is_ok());
        assert!(input("ci", Some(365)).validate().is_ok());
        for bad in [
            input("", None),
            input(" ci", None),
            input("c\ni", None),
            input(&"x".repeat(101), None),
            input("ci", Some(0)),
            input("ci", Some(366)),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn token_status_follows_revocation_then_expiry() {
        let now: DateTime<Utc> = "2026-10-27T12:00:00Z".parse().unwrap();
        let past = Some(now - chrono::Duration::days(1));
        let future = Some(now + chrono::Duration::days(1));
        assert_eq!(AgentTokenView::status_at(None, None, now), "active");
        assert_eq!(AgentTokenView::status_at(None, future, now), "active");
        assert_eq!(AgentTokenView::status_at(None, past, now), "expired");
        assert_eq!(AgentTokenView::status_at(past, future, now), "revoked");
    }
}
