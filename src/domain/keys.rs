use std::fmt;

use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

use super::identity::{ApiKeyMetadata, ApiKeyScope};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const BEARER_PREFIX: &str = "gw_";
const PUBLIC_PREFIX_HEX_LEN: usize = 20;
const KEY_BYTES: usize = 32;

#[derive(Clone)]
pub struct ApiKeyPepper(Vec<u8>);

impl ApiKeyPepper {
    pub(crate) fn from_config(value: &str) -> Result<Self, KeyError> {
        if value.trim().is_empty() {
            return Err(KeyError::MissingPepper);
        }
        Ok(Self(value.as_bytes().to_vec()))
    }

    /// Construct an opaque pepper for application composition and tests.
    pub fn from_bytes(value: &[u8]) -> Result<Self, KeyError> {
        if value.is_empty() {
            return Err(KeyError::MissingPepper);
        }
        Ok(Self(value.to_vec()))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ApiKeyPepper {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKeyPepper(REDACTED)")
    }
}

impl fmt::Display for ApiKeyPepper {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("REDACTED")
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum KeyError {
    #[error("API_KEY_PEPPER is required and must not be empty")]
    MissingPepper,
    #[error("an API key must have at least one scope")]
    EmptyScopes,
    #[error("API key scope is not in the supported vocabulary")]
    InvalidScope,
    #[error("API key expiry must be in the future")]
    InvalidExpiry,
    #[error("bearer key has an invalid format")]
    InvalidBearer,
}

/// Request metadata accepted by create and rotate lanes. Unknown fields are
/// rejected so a secret can never be smuggled through a metadata DTO.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyMutationRequest {
    pub scopes: Vec<ApiKeyScope>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl ApiKeyMutationRequest {
    pub fn validate(&self) -> Result<(), KeyError> {
        validate_scopes(&self.scopes)?;
        if self.expires_at.is_some_and(|value| value <= Utc::now()) {
            return Err(KeyError::InvalidExpiry);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApiKeyReplayError {
    #[serde(rename = "secret_unavailable_on_replay")]
    SecretUnavailableOnReplay,
}

impl ApiKeyReplayError {
    pub const fn code(self) -> &'static str {
        "secret_unavailable_on_replay"
    }
}

/// A create/rotate response has a secret only for the transaction that
/// generated it. A replay carries metadata and an explicit problem code.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyOperationResponse {
    pub metadata: ApiKeyMetadata,
    pub secret: Option<String>,
    pub replay: bool,
    pub error: Option<ApiKeyReplayError>,
}

impl std::fmt::Debug for ApiKeyOperationResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyOperationResponse")
            .field("metadata", &self.metadata)
            .field("secret", &self.secret.as_ref().map(|_| "REDACTED"))
            .field("replay", &self.replay)
            .field("error", &self.error)
            .finish()
    }
}

impl ApiKeyOperationResponse {
    pub fn created(metadata: ApiKeyMetadata, secret: String) -> Self {
        Self {
            metadata,
            secret: Some(secret),
            replay: false,
            error: None,
        }
    }

    pub fn replayed(metadata: ApiKeyMetadata) -> Self {
        Self {
            metadata,
            secret: None,
            replay: true,
            error: Some(ApiKeyReplayError::SecretUnavailableOnReplay),
        }
    }
}

impl std::fmt::Display for ApiKeyOperationResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.replay {
            f.write_str(ApiKeyReplayError::SecretUnavailableOnReplay.code())
        } else {
            f.write_str("api key created")
        }
    }
}

pub(crate) struct GeneratedKey {
    pub(crate) bearer: String,
    pub(crate) public_prefix: String,
    pub(crate) digest: Vec<u8>,
}

impl fmt::Debug for GeneratedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedKey")
            .field("bearer", &"REDACTED")
            .field("public_prefix", &self.public_prefix)
            .field("digest", &"REDACTED")
            .finish()
    }
}

pub(crate) fn validate_scopes(scopes: &[ApiKeyScope]) -> Result<(), KeyError> {
    if scopes.is_empty() {
        return Err(KeyError::EmptyScopes);
    }
    Ok(())
}

pub(crate) fn generate(pepper: &ApiKeyPepper) -> GeneratedKey {
    let mut bytes = [0_u8; KEY_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let encoded = hex_encode(&bytes);
    let bearer = format!("{BEARER_PREFIX}{encoded}");
    let public_prefix = bearer[..BEARER_PREFIX.len() + PUBLIC_PREFIX_HEX_LEN].to_owned();
    let digest = digest(pepper, bearer.as_bytes());
    GeneratedKey {
        bearer,
        public_prefix,
        digest: digest.to_vec(),
    }
}

pub(crate) fn verify_digest(
    pepper: &ApiKeyPepper,
    bearer: &str,
    expected_digest: &[u8],
) -> Result<bool, KeyError> {
    if !is_valid_bearer(bearer) {
        return Err(KeyError::InvalidBearer);
    }
    let actual = digest(pepper, bearer.as_bytes());
    Ok(actual.as_slice().ct_eq(expected_digest).into())
}

pub(crate) fn prefix_from_bearer(bearer: &str) -> Option<&str> {
    if is_valid_bearer(bearer) {
        Some(&bearer[..BEARER_PREFIX.len() + PUBLIC_PREFIX_HEX_LEN])
    } else {
        None
    }
}

fn digest(pepper: &ApiKeyPepper, bearer: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper.as_bytes())
        .expect("HMAC accepts every non-empty pepper length");
    mac.update(bearer);
    mac.finalize().into_bytes().into()
}

fn is_valid_bearer(bearer: &str) -> bool {
    bearer.len() == BEARER_PREFIX.len() + KEY_BYTES * 2
        && bearer.starts_with(BEARER_PREFIX)
        && bearer[BEARER_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use crate::domain::identity::{ApiKeyId, OrganizationId, ServiceAccountId};

    #[test]
    fn replay_never_returns_or_debug_prints_a_secret() {
        let metadata = ApiKeyMetadata {
            id: ApiKeyId::new(),
            organization_id: OrganizationId::new(),
            service_account_id: ServiceAccountId::new(),
            successor_of: None,
            public_prefix: "gw_0123456789abcdef0123".into(),
            scopes: vec![ApiKeyScope::AdminRead],
            expires_at: None,
            revoked_at: None,
            disabled_at: None,
            status: "active".into(),
        };
        let replay = ApiKeyOperationResponse::replayed(metadata);
        assert_eq!(
            replay.error,
            Some(ApiKeyReplayError::SecretUnavailableOnReplay)
        );
        assert!(replay.secret.is_none());
        assert!(!format!("{replay:?}").contains("gw_plaintext"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_verifies_only_with_same_pepper_and_bearer() {
        let pepper = ApiKeyPepper::from_bytes(b"test-pepper").unwrap();
        let other = ApiKeyPepper::from_bytes(b"other-pepper").unwrap();
        let generated = generate(&pepper);
        assert_eq!(generated.bearer.len(), 67);
        assert_eq!(generated.digest.len(), 32);
        let generated_debug = format!("{generated:?}");
        assert!(!generated_debug.contains(&generated.bearer));
        assert!(generated_debug.contains("REDACTED"));
        assert!(verify_digest(&pepper, &generated.bearer, &generated.digest).unwrap());
        assert!(!verify_digest(&other, &generated.bearer, &generated.digest).unwrap());
        let mut altered = generated.bearer.clone().into_bytes();
        let last = altered.len() - 1;
        altered[last] = if altered[last] == b'0' { b'1' } else { b'0' };
        let altered = String::from_utf8(altered).unwrap();
        assert!(!verify_digest(&pepper, &altered, &generated.digest).unwrap());
    }

    #[test]
    fn malformed_bearer_and_missing_pepper_fail_closed() {
        let pepper = ApiKeyPepper::from_bytes(b"pepper").unwrap();
        assert!(matches!(
            ApiKeyPepper::from_bytes(b""),
            Err(KeyError::MissingPepper)
        ));
        assert_eq!(
            verify_digest(&pepper, "not-a-key", &[0; 32]),
            Err(KeyError::InvalidBearer)
        );
        assert_eq!(validate_scopes(&[]), Err(KeyError::EmptyScopes));
    }
}
