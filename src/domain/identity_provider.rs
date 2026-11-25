//! An organization's own OpenID Connect identity provider: what an owner may
//! configure, and which configuration a sign-in in flight is bound to.
//!
//! Only the issuer, client ID, extra scopes and whether it is enabled are
//! ever shown. The client secret is write-only: it is sealed before it is
//! stored and never returned, logged, audited or kept in an idempotency
//! record. Every save is a new, immutable configuration; it serves sign-in
//! only after the owner who saved it completes a test sign-in through it.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::{Host, Url};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::identity::{OrganizationId, UserAccountId};

pub const MAX_ISSUER_BYTES: usize = 2048;
pub const MAX_CLIENT_ID_BYTES: usize = 255;
pub const MAX_CLIENT_SECRET_BYTES: usize = 1024;
pub const MAX_EXTRA_SCOPES: usize = 10;
pub const MAX_SCOPE_BYTES: usize = 64;
/// Always requested; never accepted as extra scopes.
pub const BASE_SCOPES: [&str; 3] = ["openid", "profile", "email"];

/// `PUT …/identity-provider`. Absent `client_secret` keeps the secret of the
/// current configuration, but only while the issuer and client ID stay the
/// same: a secret is never sent to a token endpoint it was not issued for.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityProviderInput {
    pub issuer: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<Zeroizing<String>>,
    #[serde(default)]
    pub extra_scopes: Vec<String>,
}

impl fmt::Debug for IdentityProviderInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityProviderInput")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "REDACTED"),
            )
            .field("extra_scopes", &self.extra_scopes)
            .finish()
    }
}

/// A validated configuration to save. The secret, when given, is only ever
/// sealed or fingerprinted.
pub struct ValidIdentityProvider {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<Zeroizing<String>>,
    pub extra_scopes: Vec<String>,
}

impl fmt::Debug for ValidIdentityProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidIdentityProvider")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "REDACTED"),
            )
            .field("extra_scopes", &self.extra_scopes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityProviderError {
    InvalidIssuer,
    PrivateIssuer,
    ProcessWideIssuer,
    InvalidClientId,
    InvalidClientSecret,
    ClientSecretRequired,
    InvalidScopes,
}

impl IdentityProviderError {
    /// What the caller can fix, in plain English.
    pub const fn detail(self) -> &'static str {
        match self {
            Self::InvalidIssuer => {
                "issuer must be an https URL with a host name and no user name, password, query or fragment"
            }
            Self::PrivateIssuer => {
                "issuer must be on a public host name, not an IP address, localhost or a single-label name"
            }
            Self::ProcessWideIssuer => {
                "issuer is this gateway's own sign-in provider; people already sign in through it"
            }
            Self::InvalidClientId => {
                "client_id must be 1 to 255 characters without spaces or control characters"
            }
            Self::InvalidClientSecret => {
                "client_secret must be 1 to 1024 characters without control characters"
            }
            Self::ClientSecretRequired => {
                "client_secret is required for a new provider and whenever the issuer or client ID changes"
            }
            Self::InvalidScopes => {
                "extra_scopes must be at most 10 distinct scope names, each up to 64 characters, other than openid, profile and email"
            }
        }
    }
}

impl IdentityProviderInput {
    /// Checks the shape of every field. `process_wide_issuer` is the
    /// gateway's own `OIDC_ISSUER`, which an organization may not reuse.
    /// Whether an absent secret can be kept is decided against the stored
    /// configuration, by [`require_secret`].
    pub fn validate(
        self,
        process_wide_issuer: Option<&str>,
    ) -> Result<ValidIdentityProvider, IdentityProviderError> {
        self.validate_with(process_wide_issuer, IssuerRule::PublicHttps)
    }

    /// [`Self::validate`] under `rule`; only tests use another rule than
    /// [`IssuerRule::PublicHttps`].
    pub fn validate_with(
        self,
        process_wide_issuer: Option<&str>,
        rule: IssuerRule,
    ) -> Result<ValidIdentityProvider, IdentityProviderError> {
        let issuer = validate_issuer(&self.issuer, rule)?;
        if process_wide_issuer.is_some_and(|own| same_issuer(own, &issuer)) {
            return Err(IdentityProviderError::ProcessWideIssuer);
        }
        let client_id = self.client_id;
        if client_id.is_empty()
            || client_id.len() > MAX_CLIENT_ID_BYTES
            || client_id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(IdentityProviderError::InvalidClientId);
        }
        if let Some(secret) = &self.client_secret {
            if secret.is_empty()
                || secret.len() > MAX_CLIENT_SECRET_BYTES
                || secret.chars().any(char::is_control)
            {
                return Err(IdentityProviderError::InvalidClientSecret);
            }
        }
        Ok(ValidIdentityProvider {
            issuer,
            client_id,
            client_secret: self.client_secret,
            extra_scopes: validate_scopes(self.extra_scopes)?,
        })
    }
}

/// Which issuers may be configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssuerRule {
    /// https on a public host name. The only rule for real requests.
    PublicHttps,
    /// Also `http://127.0.0.1:{port}`, for a test provider on loopback. Only
    /// where the outbound policy admits loopback too, which is tests only.
    AlsoLoopbackForTests,
}

/// Accepts an absolute https issuer on a public host name and returns it
/// exactly as given: the discovery document must name it byte for byte, so
/// it is never normalized (no trailing slash is added or removed).
pub fn validate_issuer(value: &str, rule: IssuerRule) -> Result<String, IdentityProviderError> {
    if value.is_empty() || value.len() > MAX_ISSUER_BYTES || value.trim() != value {
        return Err(IdentityProviderError::InvalidIssuer);
    }
    let url = Url::parse(value).map_err(|_| IdentityProviderError::InvalidIssuer)?;
    if rule == IssuerRule::AlsoLoopbackForTests
        && url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && value.starts_with("http://127.0.0.1:")
    {
        return Ok(value.to_owned());
    }
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.contains('?')
        || value.contains('#')
    {
        return Err(IdentityProviderError::InvalidIssuer);
    }
    match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.');
            if domain == "localhost"
                || domain.ends_with(".localhost")
                || !domain.contains('.')
                || domain.ends_with(".internal")
                || domain.ends_with(".local")
            {
                return Err(IdentityProviderError::PrivateIssuer);
            }
        }
        // An IP literal is never a real provider's issuer, and checking it
        // here would only repeat what the egress rules refuse anyway.
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => return Err(IdentityProviderError::PrivateIssuer),
        None => return Err(IdentityProviderError::InvalidIssuer),
    }
    Ok(value.to_owned())
}

/// Whether two issuers name the same provider: exact, apart from a trailing
/// slash, so a slash cannot turn the process-wide issuer into an
/// organization's.
fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn validate_scopes(scopes: Vec<String>) -> Result<Vec<String>, IdentityProviderError> {
    if scopes.len() > MAX_EXTRA_SCOPES {
        return Err(IdentityProviderError::InvalidScopes);
    }
    let mut seen: Vec<String> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        // RFC 6749 scope-token: %x21 / %x23-5B / %x5D-7E.
        let token = !scope.is_empty()
            && scope.len() <= MAX_SCOPE_BYTES
            && scope
                .bytes()
                .all(|b| b == 0x21 || (0x23..=0x5B).contains(&b) || (0x5D..=0x7E).contains(&b));
        if !token || BASE_SCOPES.contains(&scope.as_str()) || seen.contains(&scope) {
            return Err(IdentityProviderError::InvalidScopes);
        }
        seen.push(scope);
    }
    Ok(seen)
}

/// Whether a save without a secret may keep `current`'s: only for the same
/// issuer and client ID. A new provider, or one pointed elsewhere, needs it.
pub fn require_secret(
    input: &ValidIdentityProvider,
    current: Option<(&str, &str)>,
) -> Result<(), IdentityProviderError> {
    if input.client_secret.is_some() {
        return Ok(());
    }
    match current {
        Some((issuer, client_id)) if issuer == input.issuer && client_id == input.client_id => {
            Ok(())
        }
        _ => Err(IdentityProviderError::ClientSecretRequired),
    }
}

/// The scope a sign-in through the provider requests.
pub fn scope(extra_scopes: &[String]) -> String {
    BASE_SCOPES
        .iter()
        .map(|scope| (*scope).to_owned())
        .chain(extra_scopes.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a sign-in through an organization's provider is for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInPurpose {
    /// Sign a member of the organization in.
    SignIn,
    /// Prove a pending configuration works, then activate it.
    ProviderTest,
}

impl SignInPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::ProviderTest => "provider_test",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sign_in" => Some(Self::SignIn),
            "provider_test" => Some(Self::ProviderTest),
            _ => None,
        }
    }
}

/// The organization and configuration a login transaction (the `state`) is
/// bound to. The callback validates against exactly this configuration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderBinding {
    pub organization_id: OrganizationId,
    pub configuration_id: Uuid,
    pub purpose: SignInPurpose,
    /// The owner who started a test, whose session must finish it.
    pub started_by: Option<UserAccountId>,
    /// The provider's resource version when the test started; activation
    /// must still find it.
    pub expected_version: Option<u64>,
}

/// Which configurations an organization's provider points at now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderPointers {
    pub organization_id: OrganizationId,
    pub enabled: bool,
    pub active: Option<Uuid>,
    pub pending: Option<Uuid>,
}

impl ProviderBinding {
    /// Whether the configuration this sign-in started with is still the
    /// one for its purpose: the active, enabled configuration for a sign-in,
    /// the pending one for a test. Anything else (disabled, replaced, saved
    /// again, another organization) fails closed.
    pub fn is_current(&self, now: &ProviderPointers) -> bool {
        if now.organization_id != self.organization_id {
            return false;
        }
        match self.purpose {
            SignInPurpose::SignIn => now.enabled && now.active == Some(self.configuration_id),
            SignInPurpose::ProviderTest => now.pending == Some(self.configuration_id),
        }
    }
}

/// The provider as the admin API shows it: never the secret.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IdentityProviderView {
    /// `not_configured` (never saved), `pending` (a saved change waits for
    /// its test while sign-in uses the process-wide provider), `active`,
    /// `active_with_pending_change` or `disabled`.
    pub status: String,
    pub enabled: bool,
    /// The configuration sign-in uses (or last used, when disabled).
    pub active: Option<ConfigurationView>,
    /// A saved configuration waiting for its test sign-in.
    pub pending: Option<ConfigurationView>,
    /// The last successful test sign-in.
    pub last_tested_at: Option<DateTime<Utc>>,
    pub last_tested_by: Option<Uuid>,
    /// Where the provider must send people back; register it with them.
    pub redirect_uri: Option<String>,
    /// The resource version (ETag); 0 before the first save.
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigurationView {
    /// Increases with every save.
    pub configuration_version: u64,
    pub issuer: String,
    pub client_id: String,
    /// Always true: the secret is required. It is never returned.
    pub client_secret_set: bool,
    pub extra_scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub tested_at: Option<DateTime<Utc>>,
}

impl IdentityProviderView {
    /// `configured` is whether the organization ever saved a provider: one
    /// that was saved and then disabled stays `disabled`, never
    /// `not_configured`.
    pub fn status_of(configured: bool, enabled: bool, has_pending: bool) -> &'static str {
        match (configured, enabled, has_pending) {
            (false, _, _) => "not_configured",
            (true, true, false) => "active",
            (true, true, true) => "active_with_pending_change",
            // Sign-in uses the process-wide provider until it is tested.
            (true, false, true) => "pending",
            (true, false, false) => "disabled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(issuer: &str) -> IdentityProviderInput {
        IdentityProviderInput {
            issuer: issuer.into(),
            client_id: "gateway-client".into(),
            client_secret: Some(Zeroizing::new("s3cret".into())),
            extra_scopes: Vec::new(),
        }
    }

    #[test]
    fn issuers_must_be_https_on_a_public_host_name_and_are_kept_exactly() {
        for good in [
            "https://login.microsoftonline.com/0f1e2d3c-0000-4000-8000-000000000000/v2.0",
            "https://example.okta.com",
            "https://example.okta.com/oauth2/default",
            "https://accounts.google.com",
            // Kept as given: the discovery document must match it exactly.
            "https://idp.example.com/",
        ] {
            assert_eq!(input(good).validate(None).unwrap().issuer, good, "{good}");
        }
        for (bad, error) in [
            (
                "http://idp.example.com",
                IdentityProviderError::InvalidIssuer,
            ),
            ("idp.example.com", IdentityProviderError::InvalidIssuer),
            (
                "https://user:pw@idp.example.com",
                IdentityProviderError::InvalidIssuer,
            ),
            (
                "https://idp.example.com?tenant=1",
                IdentityProviderError::InvalidIssuer,
            ),
            (
                "https://idp.example.com#x",
                IdentityProviderError::InvalidIssuer,
            ),
            (
                " https://idp.example.com",
                IdentityProviderError::InvalidIssuer,
            ),
            ("https://localhost", IdentityProviderError::PrivateIssuer),
            (
                "https://idp.localhost",
                IdentityProviderError::PrivateIssuer,
            ),
            ("https://intranet", IdentityProviderError::PrivateIssuer),
            (
                "https://metadata.google.internal",
                IdentityProviderError::PrivateIssuer,
            ),
            ("https://10.0.0.1", IdentityProviderError::PrivateIssuer),
            ("https://8.8.8.8", IdentityProviderError::PrivateIssuer),
            ("https://[::1]", IdentityProviderError::PrivateIssuer),
        ] {
            assert_eq!(input(bad).validate(None).unwrap_err(), error, "{bad}");
        }
        let long = format!("https://idp.example.com/{}", "a".repeat(MAX_ISSUER_BYTES));
        assert!(input(&long).validate(None).is_err());
        // A loopback test provider only under the tests' rule.
        let local = "http://127.0.0.1:4100";
        assert!(input(local).validate(None).is_err());
        assert_eq!(
            input(local)
                .validate_with(None, IssuerRule::AlsoLoopbackForTests)
                .unwrap()
                .issuer,
            local
        );
        assert!(
            input("http://10.0.0.1:80")
                .validate_with(None, IssuerRule::AlsoLoopbackForTests)
                .is_err()
        );
    }

    #[test]
    fn the_process_wide_issuer_cannot_become_an_organizations() {
        let own = "https://login.example.com/tenant/v2.0";
        for same in [own, "https://login.example.com/tenant/v2.0/"] {
            assert_eq!(
                input(same).validate(Some(own)).unwrap_err(),
                IdentityProviderError::ProcessWideIssuer
            );
        }
        assert!(
            input("https://login.example.com/other/v2.0")
                .validate(Some(own))
                .is_ok()
        );
    }

    #[test]
    fn client_ids_secrets_and_scopes_are_checked() {
        let mut bad = input("https://idp.example.com");
        bad.client_id = "has space".into();
        assert_eq!(
            bad.validate(None).unwrap_err(),
            IdentityProviderError::InvalidClientId
        );
        let mut bad = input("https://idp.example.com");
        bad.client_secret = Some(Zeroizing::new("line\nbreak".into()));
        assert_eq!(
            bad.validate(None).unwrap_err(),
            IdentityProviderError::InvalidClientSecret
        );
        for scopes in [
            vec!["openid".to_owned()],
            vec!["groups".to_owned(), "groups".to_owned()],
            vec!["has space".to_owned()],
            vec!["quote\"".to_owned()],
            (0..11).map(|i| format!("s{i}")).collect(),
        ] {
            let mut bad = input("https://idp.example.com");
            bad.extra_scopes = scopes.clone();
            assert_eq!(
                bad.validate(None).unwrap_err(),
                IdentityProviderError::InvalidScopes,
                "{scopes:?}"
            );
        }
        let mut good = input("https://idp.example.com");
        good.extra_scopes = vec!["groups".into(), "api://gateway/.default".into()];
        let valid = good.validate(None).unwrap();
        assert_eq!(
            scope(&valid.extra_scopes),
            "openid profile email groups api://gateway/.default"
        );
        // The secret never shows in debug output.
        assert!(!format!("{valid:?}").contains("s3cret"));
    }

    #[test]
    fn a_secret_is_kept_only_for_the_same_issuer_and_client() {
        let mut keep = input("https://idp.example.com");
        keep.client_secret = None;
        let keep = keep.validate(None).unwrap();
        assert_eq!(
            require_secret(&keep, None),
            Err(IdentityProviderError::ClientSecretRequired)
        );
        assert_eq!(
            require_secret(&keep, Some(("https://idp.example.com", "gateway-client"))),
            Ok(())
        );
        for other in [
            ("https://evil.example.com", "gateway-client"),
            ("https://idp.example.com", "another-client"),
        ] {
            assert_eq!(
                require_secret(&keep, Some(other)),
                Err(IdentityProviderError::ClientSecretRequired),
                "{other:?}"
            );
        }
    }

    #[test]
    fn the_state_is_bound_to_the_organization_and_configuration() {
        let organization_id = OrganizationId::new();
        let configuration_id = Uuid::new_v4();
        let sign_in = ProviderBinding {
            organization_id,
            configuration_id,
            purpose: SignInPurpose::SignIn,
            started_by: None,
            expected_version: None,
        };
        let now = ProviderPointers {
            organization_id,
            enabled: true,
            active: Some(configuration_id),
            pending: None,
        };
        assert!(sign_in.is_current(&now));
        // Changed mid-sign-in: disabled, replaced, or another organization.
        for changed in [
            ProviderPointers {
                enabled: false,
                ..now
            },
            ProviderPointers {
                active: Some(Uuid::new_v4()),
                ..now
            },
            ProviderPointers {
                organization_id: OrganizationId::new(),
                ..now
            },
            // Still saved, but only as a pending change: not for sign-in.
            ProviderPointers {
                active: None,
                pending: Some(configuration_id),
                enabled: false,
                ..now
            },
        ] {
            assert!(!sign_in.is_current(&changed), "{changed:?}");
        }

        let test = ProviderBinding {
            purpose: SignInPurpose::ProviderTest,
            started_by: Some(UserAccountId::new()),
            expected_version: Some(3),
            ..sign_in
        };
        let pending = ProviderPointers {
            enabled: false,
            active: None,
            pending: Some(configuration_id),
            organization_id,
        };
        assert!(test.is_current(&pending));
        // Saved again during the test: the old pending configuration is
        // no longer the one to activate.
        assert!(!test.is_current(&ProviderPointers {
            pending: Some(Uuid::new_v4()),
            ..pending
        }));
        // Already active is not pending: a test cannot activate twice.
        assert!(!test.is_current(&now));
    }

    #[test]
    fn status_names_each_state() {
        let status = IdentityProviderView::status_of;
        assert_eq!(status(false, false, false), "not_configured");
        assert_eq!(status(true, false, true), "pending");
        assert_eq!(status(true, true, false), "active");
        assert_eq!(status(true, true, true), "active_with_pending_change");
        // Saved and then disabled, whether it was ever active or not.
        assert_eq!(status(true, false, false), "disabled");
    }
}
