//! Signed, scope-bound cursor pagination shared by administrative lanes.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

use super::{
    admin::{AdminScope, RequestFingerprint},
    auth::AuthCrypto,
    identity::{OrganizationId, TeamId, UserAccountId},
};

pub const DEFAULT_PAGE_SIZE: u32 = 50;
pub const MAX_PAGE_SIZE: u32 = 100;
const CURSOR_VERSION: u8 = 1;
const MAX_TOKEN_LENGTH: usize = 4096;
const MAX_CLAIM_LENGTH: usize = 2048;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PageRequest {
    #[serde(
        default = "default_page_size",
        rename = "page_size",
        deserialize_with = "deserialize_page_size"
    )]
    pub page_size: u32,
    #[serde(
        default,
        rename = "page_token",
        deserialize_with = "deserialize_page_token"
    )]
    pub page_token: Option<String>,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            page_token: None,
        }
    }
}

impl PageRequest {
    pub fn validate(&self) -> Result<(), PaginationError> {
        if !(1..=MAX_PAGE_SIZE).contains(&self.page_size) {
            return Err(PaginationError::LimitOutOfBounds);
        }
        if self
            .page_token
            .as_ref()
            .is_some_and(|token| token.len() > MAX_TOKEN_LENGTH)
        {
            return Err(PaginationError::OversizedToken);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(rename = "next_page_token")]
    pub next_page_token: Option<String>,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, next_page_token: Option<String>) -> Self {
        Self {
            items,
            next_page_token,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CursorClaims {
    pub version: u8,
    pub actor_id: UserAccountId,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub collection: String,
    pub filters: String,
    pub order: String,
    pub last_keyset: String,
    pub upper_watermark: Option<String>,
}

impl CursorClaims {
    pub fn new(
        actor_id: UserAccountId,
        scope: AdminScope,
        collection: impl Into<String>,
        filters: impl Into<String>,
        order: impl Into<String>,
        last_keyset: impl Into<String>,
        upper_watermark: Option<String>,
    ) -> Result<Self, PaginationError> {
        let claims = Self {
            version: CURSOR_VERSION,
            actor_id,
            organization_id: scope.organization_id(),
            team_id: scope.team_id(),
            collection: collection.into(),
            filters: filters.into(),
            order: order.into(),
            last_keyset: last_keyset.into(),
            upper_watermark,
        };
        claims.validate_size()?;
        Ok(claims)
    }

    fn validate_size(&self) -> Result<(), PaginationError> {
        let encoded = serde_json::to_vec(self).map_err(|_| PaginationError::InvalidToken)?;
        if encoded.len() > MAX_CLAIM_LENGTH || self.version != CURSOR_VERSION {
            return Err(PaginationError::OversizedToken);
        }
        Ok(())
    }
}

/// HMAC-signed versioned tokens. The payload is bound to the authenticated
/// actor, tenant/team scope, collection, normalized filters/order and keyset.
#[derive(Clone)]
pub struct SignedCursorCodec {
    crypto: AuthCrypto,
}

impl SignedCursorCodec {
    pub fn new(crypto: AuthCrypto) -> Self {
        Self { crypto }
    }

    pub fn encode(&self, claims: &CursorClaims) -> Result<String, PaginationError> {
        claims.validate_size()?;
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).map_err(|_| PaginationError::InvalidToken)?);
        let signature = URL_SAFE_NO_PAD.encode(self.crypto.digest(&payload));
        let token = format!("v{CURSOR_VERSION}.{payload}.{signature}");
        if token.len() > MAX_TOKEN_LENGTH {
            return Err(PaginationError::OversizedToken);
        }
        Ok(token)
    }

    pub fn decode_and_validate(
        &self,
        token: &str,
        actor_id: UserAccountId,
        scope: AdminScope,
        collection: &str,
        normalized_filters: &str,
        normalized_order: &str,
    ) -> Result<CursorClaims, PaginationError> {
        if token.len() > MAX_TOKEN_LENGTH {
            return Err(PaginationError::OversizedToken);
        }
        let mut parts = token.split('.');
        let version = parts.next().ok_or(PaginationError::InvalidToken)?;
        let payload = parts.next().ok_or(PaginationError::InvalidToken)?;
        let signature = parts.next().ok_or(PaginationError::InvalidToken)?;
        if parts.next().is_some() || version != "v1" || payload.len() > MAX_CLAIM_LENGTH {
            return Err(PaginationError::InvalidToken);
        }
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| PaginationError::TamperedToken)?;
        if !self.crypto.verify_digest(payload, &signature) {
            return Err(PaginationError::TamperedToken);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| PaginationError::InvalidToken)?;
        let claims: CursorClaims =
            serde_json::from_slice(&bytes).map_err(|_| PaginationError::InvalidToken)?;
        claims.validate_size()?;
        if claims.actor_id != actor_id
            || claims.organization_id != scope.organization_id()
            || claims.team_id != scope.team_id()
        {
            return Err(PaginationError::ScopeMismatch);
        }
        if claims.collection != collection
            || claims.filters != normalized_filters
            || claims.order != normalized_order
        {
            return Err(PaginationError::QueryMismatch);
        }
        Ok(claims)
    }
}

/// Canonical request fingerprint input used by mutation idempotency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminRequestFingerprintInput<'a> {
    pub method: &'a str,
    pub route: &'a str,
    pub operation: &'a str,
    pub target_ids: &'a [Uuid],
    pub if_match: Option<&'a str>,
    pub body: &'a serde_json::Value,
}

pub fn canonical_admin_fingerprint(input: AdminRequestFingerprintInput<'_>) -> RequestFingerprint {
    let mut target_ids: Vec<String> = input.target_ids.iter().map(ToString::to_string).collect();
    target_ids.sort();
    let value = serde_json::json!({
        "body": input.body,
        "if_match": input.if_match,
        "method": input.method,
        "operation": input.operation,
        "route": input.route,
        "target_ids": target_ids,
    });
    RequestFingerprint::from_request(&value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaginationError {
    InvalidToken,
    TamperedToken,
    ScopeMismatch,
    QueryMismatch,
    OversizedToken,
    LimitOutOfBounds,
}

impl PaginationError {
    pub const fn problem_code(self) -> &'static str {
        match self {
            Self::InvalidToken => "invalid_page_token",
            Self::TamperedToken => "invalid_page_token_signature",
            Self::ScopeMismatch => "page_token_scope_mismatch",
            Self::QueryMismatch => "page_token_query_mismatch",
            Self::OversizedToken => "page_token_too_large",
            Self::LimitOutOfBounds => "pagination_limit_invalid",
        }
    }
}

impl fmt::Display for PaginationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidToken => "invalid page token",
            Self::TamperedToken => "page token signature is invalid",
            Self::ScopeMismatch => "page token does not belong to this scope",
            Self::QueryMismatch => "page token does not match this collection query",
            Self::OversizedToken => "page token is too large",
            Self::LimitOutOfBounds => "page size must be between 1 and 100",
        })
    }
}
impl std::error::Error for PaginationError {}

fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}

fn deserialize_page_size<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if (1..=MAX_PAGE_SIZE).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "page_size must be between 1 and 100",
        ))
    }
}

fn deserialize_page_token<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if value
        .as_ref()
        .is_some_and(|token| token.len() > MAX_TOKEN_LENGTH)
    {
        Err(serde::de::Error::custom("page_token is too large"))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::auth::AuthCrypto;

    fn crypto() -> AuthCrypto {
        AuthCrypto::new(b"cursor-hmac-key-which-is-at-least-32-bytes", &[1; 32]).unwrap()
    }

    #[test]
    fn signed_cursor_rejects_tampering_and_cross_team_reuse() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        let other = TeamId::new();
        let actor = UserAccountId::new();
        let scope = AdminScope::Team {
            organization_id: org,
            team_id: team,
        };
        let claims = CursorClaims::new(
            actor,
            scope,
            "keys",
            "status=active",
            "created_at,id",
            "k1",
            None,
        )
        .unwrap();
        let codec = SignedCursorCodec::new(crypto());
        let token = codec.encode(&claims).unwrap();
        assert!(
            codec
                .decode_and_validate(
                    &token,
                    actor,
                    scope,
                    "keys",
                    "status=active",
                    "created_at,id"
                )
                .is_ok()
        );
        let mut tampered = token.clone();
        tampered.replace_range(3..4, "x");
        assert!(matches!(
            codec.decode_and_validate(
                &tampered,
                actor,
                scope,
                "keys",
                "status=active",
                "created_at,id"
            ),
            Err(PaginationError::TamperedToken) | Err(PaginationError::InvalidToken)
        ));
        assert_eq!(
            codec.decode_and_validate(
                &token,
                actor,
                AdminScope::Team {
                    organization_id: org,
                    team_id: other
                },
                "keys",
                "status=active",
                "created_at,id"
            ),
            Err(PaginationError::ScopeMismatch)
        );
    }

    #[test]
    fn wire_contract_uses_page_names_and_bounded_size() {
        let request: PageRequest =
            serde_json::from_str(r#"{"page_size":100,"page_token":"x"}"#).unwrap();
        assert_eq!(request.page_size, 100);
        assert!(
            serde_json::to_string(&Page::<u8>::new(vec![1], Some("next".into())))
                .unwrap()
                .contains("next_page_token")
        );
        assert!(serde_json::from_str::<PageRequest>(r#"{"page_size":101}"#).is_err());
    }
}
