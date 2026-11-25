//! Provider-agnostic OIDC, session, and administrative authorization contracts.
//! Network discovery and token exchange remain application-owned integrations.

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::identity::{MembershipRole, OrganizationId, TeamId, UserAccountId};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct AuthCrypto {
    hmac_key: Vec<u8>,
    encryption_key: [u8; 32],
}

impl AuthCrypto {
    pub fn new(hmac_key: &[u8], encryption_key: &[u8]) -> Result<Self, AuthCryptoError> {
        if hmac_key.len() < 32 || encryption_key.len() != 32 {
            return Err(AuthCryptoError::InvalidKeyLength);
        }
        let key: [u8; 32] = encryption_key
            .try_into()
            .map_err(|_| AuthCryptoError::InvalidKeyLength)?;
        Ok(Self {
            hmac_key: hmac_key.to_vec(),
            encryption_key: key,
        })
    }
    pub fn opaque_token(&self) -> OpaqueToken {
        let mut bytes = [0_u8; 32];
        crate::os_random::fill(&mut bytes);
        OpaqueToken(URL_SAFE_NO_PAD.encode(bytes))
    }
    pub fn digest(&self, value: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.hmac_key).expect("validated HMAC key");
        mac.update(value.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
    pub fn verify_digest(&self, value: &str, expected: &[u8]) -> bool {
        self.digest(value).as_slice().ct_eq(expected).into()
    }
    pub fn pkce_challenge(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }
    /// Encrypt a PKCE verifier for storage. The returned bytes contain a fresh
    /// nonce followed by ciphertext; neither is a bearer or persisted plaintext.
    pub fn encrypt_verifier(&self, verifier: &str) -> Result<Vec<u8>, AuthCryptoError> {
        let nonce = crate::os_random::bytes::<12>();
        self.encrypt_verifier_with_nonce(verifier, nonce)
    }
    /// `encrypt_verifier` under a given nonce; only `encrypt_verifier` (a
    /// fresh random nonce) and the known-answer tests call it.
    fn encrypt_verifier_with_nonce(
        &self,
        verifier: &str,
        nonce: [u8; 12],
    ) -> Result<Vec<u8>, AuthCryptoError> {
        use aes_gcm::{Aes256Gcm, Nonce, aead::Aead};
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|_| AuthCryptoError::Encryption)?;
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce), verifier.as_bytes())
            .map_err(|_| AuthCryptoError::Encryption)?;
        Ok([nonce.as_slice(), ciphertext.as_slice()].concat())
    }
    pub fn decrypt_verifier(&self, encrypted: &[u8]) -> Result<String, AuthCryptoError> {
        use aes_gcm::{Aes256Gcm, Nonce, aead::Aead};
        if encrypted.len() < 12 {
            return Err(AuthCryptoError::Decryption);
        }
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key)
            .map_err(|_| AuthCryptoError::Decryption)?;
        let plaintext = cipher
            .decrypt(
                &Nonce::try_from(&encrypted[..12]).map_err(|_| AuthCryptoError::Decryption)?,
                &encrypted[12..],
            )
            .map_err(|_| AuthCryptoError::Decryption)?;
        String::from_utf8(plaintext).map_err(|_| AuthCryptoError::Decryption)
    }
}
impl fmt::Debug for AuthCrypto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthCrypto(REDACTED)")
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum AuthCryptoError {
    #[error("OIDC/session keys have invalid lengths")]
    InvalidKeyLength,
    #[error("secret encryption failed")]
    Encryption,
    #[error("secret decryption failed")]
    Decryption,
}

#[derive(Clone, Eq, PartialEq)]
pub struct OpaqueToken(String);
impl OpaqueToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for OpaqueToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpaqueToken(REDACTED)")
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct OidcLoginTransaction {
    pub id: Uuid,
    pub state_digest: Vec<u8>,
    pub nonce_digest: Vec<u8>,
    pub verifier_ciphertext: Vec<u8>,
    pub browser_binding_digest: Vec<u8>,
    pub expires_at: u64,
    /// The invitation the sign-in was started from, accepted when it
    /// completes.
    pub invitation_id: Option<Uuid>,
    /// The organization's identity provider configuration the sign-in goes
    /// through; `None` for the process-wide provider.
    #[serde(default)]
    pub provider: Option<super::identity_provider::ProviderBinding>,
}
impl fmt::Debug for OidcLoginTransaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcLoginTransaction")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .field("provider", &self.provider)
            .finish()
    }
}

impl AuthCrypto {
    pub fn new_login_transaction(
        &self,
        browser_binding: &str,
        ttl: Duration,
    ) -> Result<(OidcLoginTransaction, OpaqueToken, OpaqueToken, String), AuthCryptoError> {
        let state = self.opaque_token();
        let nonce = self.opaque_token();
        let verifier = self.opaque_token();
        Ok((
            OidcLoginTransaction {
                id: Uuid::new_v4(),
                state_digest: self.digest(state.as_str()),
                nonce_digest: self.digest(nonce.as_str()),
                verifier_ciphertext: self.encrypt_verifier(verifier.as_str())?,
                browser_binding_digest: self.digest(browser_binding),
                expires_at: now_epoch().saturating_add(ttl.as_secs()),
                invitation_id: None,
                provider: None,
            },
            state,
            nonce,
            verifier.0,
        ))
    }
    pub fn new_session(
        &self,
        user_id: UserAccountId,
        browser_binding: &str,
        ttl: Duration,
    ) -> SessionRecord {
        let token = self.opaque_token();
        SessionRecord {
            id: Uuid::new_v4(),
            token: token.0.clone(),
            token_digest: self.digest(&token.0),
            user_id,
            browser_binding_digest: self.digest(browser_binding),
            expires_at: now_epoch().saturating_add(ttl.as_secs()),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionRecord {
    pub id: Uuid,
    pub token: String,
    pub token_digest: Vec<u8>,
    pub user_id: UserAccountId,
    pub browser_binding_digest: Vec<u8>,
    pub expires_at: u64,
}
impl fmt::Debug for SessionRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionRecord")
            .field("id", &self.id)
            .field("token", &"REDACTED")
            .field("user_id", &self.user_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Claims after an issuer-specific signature/JWKS verifier has succeeded.
/// This contract does not perform discovery or network token exchange.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct OidcClaims {
    pub issuer: String,
    pub subject: String,
    pub audience: Vec<String>,
    pub nonce: String,
    pub expires_at: u64,
    /// Display details only; never used to identify or authorize anyone.
    #[serde(default)]
    pub profile: OidcProfile,
}

/// Who the provider says the person is, for display: refreshed on every
/// sign-in, never trusted for identity (that is `(issuer, subject)`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OidcProfile {
    /// The `name` claim.
    pub name: Option<String>,
    /// The `email` claim, else a `preferred_username` that looks like one.
    pub email: Option<String>,
    pub photo: ProfilePhoto,
}

/// The person's photo, when the provider is asked for one.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProfilePhoto {
    /// Not asked for, or the provider could not be reached: keep what is stored.
    #[default]
    Unknown,
    /// The provider has none: clear what is stored.
    Absent,
    Present {
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// The application owns the OIDC protocol state, while this boundary owns the
/// issuer-specific code exchange and signature verification.  Production uses
/// [`UnavailableOidcBackend`] until a real, audited provider integration is
/// installed; accepting claims supplied by an HTTP client is never a
/// production fallback.
#[async_trait]
pub trait OidcBackend: Send + Sync {
    /// Where to send the browser to sign in. A real provider names it in
    /// its discovery document; the default suits test backends.
    async fn authorization_endpoint(&self, issuer: &str) -> Result<url::Url, OidcBackendError> {
        url::Url::parse(&format!("{}/authorize", issuer.trim_end_matches('/')))
            .map_err(|_| OidcBackendError::NotDispatched)
    }

    async fn authenticate(
        &self,
        issuer: &str,
        audience: &str,
        client_id: &str,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
    ) -> Result<OidcClaims, OidcBackendError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OidcBackendError {
    /// The adapter guarantees that no request was dispatched.
    NotDispatched,
    Unavailable,
    InvalidResponse,
}

pub struct UnavailableOidcBackend;

#[async_trait]
impl OidcBackend for UnavailableOidcBackend {
    async fn authenticate(
        &self,
        _issuer: &str,
        _audience: &str,
        _client_id: &str,
        _redirect_uri: &str,
        _code: &str,
        _verifier: &str,
    ) -> Result<OidcClaims, OidcBackendError> {
        Err(OidcBackendError::Unavailable)
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct MockOidcBackend {
    claims: OidcClaims,
}

#[cfg(test)]
impl MockOidcBackend {
    pub fn new(claims: OidcClaims) -> Self {
        Self { claims }
    }
}

#[cfg(test)]
#[async_trait]
impl OidcBackend for MockOidcBackend {
    async fn authenticate(
        &self,
        _issuer: &str,
        _audience: &str,
        _client_id: &str,
        _redirect_uri: &str,
        _code: &str,
        _verifier: &str,
    ) -> Result<OidcClaims, OidcBackendError> {
        Ok(self.claims.clone())
    }
}
impl fmt::Debug for OidcClaims {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OidcClaims(REDACTED)")
    }
}
impl fmt::Display for OidcClaims {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OIDC subject authenticated")
    }
}

pub fn validate_claims(
    claims: &OidcClaims,
    expected_issuer: &str,
    expected_audience: &str,
    expected_nonce: &str,
    now: u64,
) -> Result<(), ClaimError> {
    if claims.issuer != expected_issuer {
        return Err(ClaimError::Issuer);
    }
    if !claims
        .audience
        .iter()
        .any(|audience| audience == expected_audience)
    {
        return Err(ClaimError::Audience);
    }
    if claims.nonce != expected_nonce {
        return Err(ClaimError::Nonce);
    }
    if claims.subject.trim().is_empty() {
        return Err(ClaimError::Subject);
    }
    if claims.expires_at <= now {
        return Err(ClaimError::Expired);
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimError {
    Issuer,
    Audience,
    Nonce,
    Subject,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecurityEvent {
    LoginSucceeded,
    LoginReplayOrInvalidState,
    LoginBackendUnavailable,
    LoginBackendRejected,
    LoginClaimsRejected,
    LoginUnprovisioned,
    LoginDisabled,
    LoginIdentityUnavailable,
    LogoutRequested,
    /// A password sign-in or confirmation with the wrong password.
    PasswordRejected,
    /// A password sign-in refused because the account is locked.
    LoginLocked,
    /// A password sign-in that succeeded.
    PasswordLoginSucceeded,
    /// The right password, but the person is disabled or may not act
    /// anywhere with a password.
    PasswordLoginDisabled,
    PasswordChanged,
    PasswordReset,
    PasswordResetLinkCreated,
    SetupCompleted,
}

/// Closed callback failure outcomes. These are deliberately the only values
/// accepted by the persistence finish action; issuer, subject, metadata, and
/// event names never cross that boundary from request handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginFailure {
    ReplayOrInvalidState,
    BackendUnavailable,
    BackendRejected,
    ClaimsRejected,
    Unprovisioned,
    Disabled,
    IdentityUnavailable,
}

impl LoginFailure {
    /// Map the closed Rust outcome to a migration-controlled event without
    /// persisting attacker-controlled detail.
    pub const fn persisted_event(self) -> SecurityEvent {
        match self {
            Self::ReplayOrInvalidState => SecurityEvent::LoginReplayOrInvalidState,
            Self::BackendUnavailable => SecurityEvent::LoginBackendUnavailable,
            Self::BackendRejected => SecurityEvent::LoginBackendRejected,
            Self::ClaimsRejected => SecurityEvent::LoginClaimsRejected,
            Self::Unprovisioned => SecurityEvent::LoginUnprovisioned,
            Self::Disabled => SecurityEvent::LoginDisabled,
            Self::IdentityUnavailable => SecurityEvent::LoginIdentityUnavailable,
        }
    }
}

impl SecurityEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LoginSucceeded => "login.succeeded",
            Self::LoginReplayOrInvalidState => "login.replay_or_invalid_state",
            Self::LoginBackendUnavailable => "login.backend_unavailable",
            Self::LoginBackendRejected => "login.backend_rejected",
            Self::LoginClaimsRejected => "login.claims_rejected",
            Self::LoginUnprovisioned => "login.unprovisioned",
            Self::LoginDisabled => "login.disabled",
            Self::LoginIdentityUnavailable => "login.identity_unavailable",
            Self::LogoutRequested => "logout.requested",
            Self::PasswordRejected => "login.password_rejected",
            Self::LoginLocked => "login.locked",
            Self::PasswordLoginSucceeded => "login.succeeded",
            Self::PasswordLoginDisabled => "login.disabled",
            Self::PasswordChanged => "password.changed",
            Self::PasswordReset => "password.reset",
            Self::PasswordResetLinkCreated => "password.reset_link_created",
            Self::SetupCompleted => "setup.completed",
        }
    }

    pub const fn method(self) -> &'static str {
        match self {
            Self::LogoutRequested => "browser",
            Self::PasswordRejected
            | Self::LoginLocked
            | Self::PasswordLoginSucceeded
            | Self::PasswordLoginDisabled
            | Self::PasswordChanged
            | Self::PasswordReset
            | Self::PasswordResetLinkCreated
            | Self::SetupCompleted => "password",
            _ => "oidc",
        }
    }
}

pub use super::admin::AdminPermission;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HumanPrincipal {
    pub user_id: UserAccountId,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub role: MembershipRole,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crypto() -> AuthCrypto {
        AuthCrypto::new(b"01234567890123456789012345678901", &[3_u8; 32]).unwrap()
    }

    #[test]
    fn encrypted_verifiers_use_unique_nonces_and_reject_invalid_inputs() {
        let crypto = crypto();
        let first = crypto.encrypt_verifier("verifier").unwrap();
        let second = crypto.encrypt_verifier("verifier").unwrap();

        assert_ne!(&first[..12], &second[..12]);
        assert_eq!(crypto.decrypt_verifier(&first).unwrap(), "verifier");
        assert_eq!(crypto.decrypt_verifier(&second).unwrap(), "verifier");

        let wrong_key = AuthCrypto::new(b"01234567890123456789012345678901", &[4_u8; 32]).unwrap();
        assert!(wrong_key.decrypt_verifier(&first).is_err());

        let mut tampered = first.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(crypto.decrypt_verifier(&tampered).is_err());
    }

    #[test]
    fn pkce_and_encrypted_verifier_never_expose_plaintext_storage() {
        let crypto = crypto();
        let (transaction, state, nonce, verifier) = crypto
            .new_login_transaction("browser-a", Duration::from_secs(300))
            .unwrap();
        assert!(crypto.verify_digest(state.as_str(), &transaction.state_digest));
        assert!(crypto.verify_digest(nonce.as_str(), &transaction.nonce_digest));
        assert!(
            !transaction
                .verifier_ciphertext
                .windows(verifier.len())
                .any(|window| window == verifier.as_bytes())
        );
        assert_eq!(
            crypto
                .decrypt_verifier(&transaction.verifier_ciphertext)
                .unwrap(),
            verifier
        );
        assert!(!crypto.verify_digest(state.as_str(), &crypto.digest("browser-b")));
    }

    #[test]
    fn session_tokens_are_digestable_and_browser_bound() {
        let crypto = crypto();
        let user_id = UserAccountId::new();
        let session = crypto.new_session(user_id, "browser-a", Duration::from_secs(60));
        assert!(crypto.verify_digest(&session.token, &session.token_digest));
        assert!(crypto.verify_digest("browser-a", &session.browser_binding_digest));
        assert!(!crypto.verify_digest("browser-b", &session.browser_binding_digest));
        assert_eq!(format!("{session:?}").contains(&session.token), false);
    }

    #[test]
    fn claims_fail_closed_and_roles_are_explicit() {
        let claims = OidcClaims {
            issuer: "https://issuer".into(),
            subject: "subject".into(),
            audience: vec!["gateway".into()],
            nonce: "nonce".into(),
            expires_at: 10,
            profile: OidcProfile::default(),
        };
        assert!(validate_claims(&claims, "https://issuer", "gateway", "nonce", 9).is_ok());
        assert_eq!(
            validate_claims(&claims, "https://other", "gateway", "nonce", 9),
            Err(ClaimError::Issuer)
        );
        assert_eq!(
            validate_claims(&claims, "https://issuer", "other", "nonce", 9),
            Err(ClaimError::Audience)
        );
        assert_eq!(
            validate_claims(&claims, "https://issuer", "gateway", "other", 9),
            Err(ClaimError::Nonce)
        );
        assert_eq!(
            validate_claims(&claims, "https://issuer", "gateway", "nonce", 10),
            Err(ClaimError::Expired)
        );
    }

    #[tokio::test]
    async fn test_backend_is_injectable_without_an_http_claim_header() {
        let claims = OidcClaims {
            issuer: "https://issuer".into(),
            subject: "subject".into(),
            audience: vec!["gateway".into()],
            nonce: "nonce".into(),
            expires_at: u64::MAX,
            profile: OidcProfile::default(),
        };
        let backend = MockOidcBackend::new(claims.clone());
        assert_eq!(
            backend
                .authenticate(
                    "https://issuer",
                    "gateway",
                    "client",
                    "https://gateway/cb",
                    "code",
                    "verifier"
                )
                .await
                .unwrap(),
            claims
        );
    }

    #[test]
    fn callback_failure_outcomes_use_closed_persisted_categories() {
        assert_eq!(
            LoginFailure::ReplayOrInvalidState.persisted_event(),
            SecurityEvent::LoginReplayOrInvalidState
        );
        assert_eq!(
            LoginFailure::BackendUnavailable.persisted_event(),
            SecurityEvent::LoginBackendUnavailable
        );
        assert_eq!(
            LoginFailure::BackendRejected.persisted_event(),
            SecurityEvent::LoginBackendRejected
        );
        assert_eq!(
            LoginFailure::ClaimsRejected.persisted_event(),
            SecurityEvent::LoginClaimsRejected
        );
        assert_eq!(
            LoginFailure::Unprovisioned.persisted_event(),
            SecurityEvent::LoginUnprovisioned
        );
        assert_eq!(
            LoginFailure::Disabled.persisted_event(),
            SecurityEvent::LoginDisabled
        );
        assert_eq!(
            LoginFailure::IdentityUnavailable.persisted_event(),
            SecurityEvent::LoginIdentityUnavailable
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    // Known answers recorded before the aes-gcm 0.11 / hmac 0.13 / sha2 0.11
    // / base64 0.23 upgrade: stored session and login digests and sealed
    // PKCE verifiers depend on them.
    #[test]
    fn digests_and_pkce_challenges_match_known_answers() {
        let crypto = crypto();
        // HMAC-SHA256; also matches Python's hmac module.
        assert_eq!(
            hex(&crypto.digest("session-token")),
            "bd5b0919fc2e6e49289943534aad9e1d9578ffdffb890d4c40383a07e11b4975"
        );
        assert!(crypto.verify_digest("session-token", &crypto.digest("session-token")));
        assert!(!crypto.verify_digest("session-tokeN", &crypto.digest("session-token")));
        // RFC 7636 appendix B.
        assert_eq!(
            AuthCrypto::pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // Opaque tokens: 32 bytes, base64url without padding.
        let token = crypto.opaque_token();
        assert_eq!(token.as_str().len(), 43);
        assert!(
            token
                .as_str()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn sealed_verifiers_match_known_answers() {
        let crypto = crypto();
        let sealed = crypto
            .encrypt_verifier_with_nonce("verifier-known-answer", [0x5c; 12])
            .unwrap();
        assert_eq!(
            hex(&sealed),
            "5c5c5c5c5c5c5c5c5c5c5c5c46b9c9e7e8e5c793221b33ea3208e85b8362ca75da352f7eb31f6338a22b6a66d0e7f262ae"
        );
        assert_eq!(
            crypto.decrypt_verifier(&sealed).unwrap(),
            "verifier-known-answer"
        );
        // Sealed with a random nonce by the pre-upgrade code.
        assert_eq!(
            crypto
                .decrypt_verifier(&unhex(
                    "ab0db69dbd4fcfe7ae5c27ac71ca9a5a16425e38cae93563af9a722ec9770e9c7276369f3f3f919fcc34b517948689fc5fe8"
                ))
                .unwrap(),
            "verifier-sealed-before"
        );
    }

    #[test]
    #[ignore = "prints a fresh sealed verifier for sealed_verifiers_match_known_answers"]
    fn print_sealed_verifier() {
        let sealed = crypto().encrypt_verifier("verifier-sealed-before").unwrap();
        println!("VECTOR {}", hex(&sealed));
    }
}
