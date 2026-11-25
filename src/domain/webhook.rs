//! Webhook endpoints: validation, signing secrets, and the delivery envelope.
//!
//! Deliveries follow the Standard Webhooks scheme: `webhook-id`,
//! `webhook-timestamp` and `webhook-signature` headers, where the signature is
//! `v1,` followed by the base64 HMAC-SHA256 of `{id}.{timestamp}.{body}` under
//! the endpoint's `whsec_` secret. Existing Standard Webhooks libraries verify
//! them unchanged.
//!
//! Endpoints must be https URLs to public hosts. The URL check here rejects
//! literal private addresses up front; the delivery path re-checks every
//! address the host resolves to, since DNS can change after creation.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use url::{Host, Url};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{identity::OrganizationId, outbox::OutboxEventType};

pub const MAX_ENDPOINTS_PER_ORGANIZATION: i64 = 20;
const MAX_URL_BYTES: usize = 2048;
const MAX_DESCRIPTION_CHARS: usize = 200;
const SECRET_PREFIX: &str = "whsec_";
const SECRET_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum WebhookError {
    #[error("url must be an absolute https URL of at most 2048 bytes")]
    InvalidUrl,
    #[error("url must not contain credentials or a fragment")]
    UrlCredentials,
    #[error("url must name a public host")]
    PrivateHost,
    #[error("description must be 1-200 characters without control characters")]
    InvalidDescription,
    #[error("event_types must list at least one event type, each once")]
    InvalidEventTypes,
    #[error("request must change at least one field")]
    EmptyUpdate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebhookEndpointInput {
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
    pub event_types: Vec<OutboxEventType>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl WebhookEndpointInput {
    pub fn validate(self) -> Result<ValidEndpoint, WebhookError> {
        Ok(ValidEndpoint {
            url: validate_url(&self.url)?,
            description: validate_description(self.description)?,
            event_types: validate_event_types(self.event_types)?,
            enabled: self.enabled.unwrap_or(true),
        })
    }
}

/// A partial update. An empty `description` clears it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebhookEndpointUpdate {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub event_types: Option<Vec<OutboxEventType>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl WebhookEndpointUpdate {
    pub fn validate(self) -> Result<ValidUpdate, WebhookError> {
        if self.url.is_none()
            && self.description.is_none()
            && self.event_types.is_none()
            && self.enabled.is_none()
        {
            return Err(WebhookError::EmptyUpdate);
        }
        Ok(ValidUpdate {
            url: self.url.as_deref().map(validate_url).transpose()?,
            description: match self.description {
                None => None,
                Some(value) if value.is_empty() => Some(None),
                Some(value) => Some(validate_description(Some(value))?),
            },
            event_types: self.event_types.map(validate_event_types).transpose()?,
            enabled: self.enabled,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidEndpoint {
    pub url: String,
    pub description: Option<String>,
    pub event_types: Vec<OutboxEventType>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidUpdate {
    pub url: Option<String>,
    /// `Some(None)` clears the description.
    pub description: Option<Option<String>>,
    pub event_types: Option<Vec<OutboxEventType>>,
    pub enabled: Option<bool>,
}

/// What the admin API says about an endpoint. There is no field that can
/// carry the signing secret or its ciphertext.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebhookEndpointView {
    pub id: Uuid,
    pub url: String,
    pub description: Option<String>,
    pub event_types: Vec<OutboxEventType>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Parked,
}

impl DeliveryStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "parked" => Some(Self::Parked),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebhookDeliveryView {
    pub id: Uuid,
    pub event_id: Uuid,
    pub event_type: OutboxEventType,
    pub status: DeliveryStatus,
    pub attempts: i32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_status_code: Option<i32>,
    pub last_error: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Accepts an absolute https URL with a host that is not a literal private,
/// loopback or otherwise non-public address, and returns it normalized.
pub fn validate_url(value: &str) -> Result<String, WebhookError> {
    if value.len() > MAX_URL_BYTES {
        return Err(WebhookError::InvalidUrl);
    }
    let url = Url::parse(value).map_err(|_| WebhookError::InvalidUrl)?;
    if url.scheme() != "https" {
        return Err(WebhookError::InvalidUrl);
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(WebhookError::UrlCredentials);
    }
    match url.host() {
        None => return Err(WebhookError::InvalidUrl),
        Some(Host::Ipv4(address)) if !is_public_ip(IpAddr::V4(address)) => {
            return Err(WebhookError::PrivateHost);
        }
        Some(Host::Ipv6(address)) if !is_public_ip(IpAddr::V6(address)) => {
            return Err(WebhookError::PrivateHost);
        }
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.');
            if domain == "localhost" || domain.ends_with(".localhost") || !domain.contains('.') {
                return Err(WebhookError::PrivateHost);
            }
        }
        Some(_) => {}
    }
    let normalized = String::from(url);
    if normalized.len() > MAX_URL_BYTES {
        return Err(WebhookError::InvalidUrl);
    }
    Ok(normalized)
}

/// Whether a webhook may connect to `address`. Anything loopback, private,
/// link-local, shared, reserved, multicast, or documentation-only is refused,
/// including IPv4 addresses embedded in IPv6 forms.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..128).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (18..20).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    // NAT64 (64:ff9b::/96) and SIIT's IPv4-translated form (::ffff:0:0/96)
    // embed the IPv4 destination in the low 32 bits.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || segments[..6] == [0, 0, 0, 0, 0xffff, 0] {
        let [.., high, low] = segments;
        return is_public_ipv4(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low)));
    }
    // 6to4 (2002::/16) embeds it in the second and third segments.
    if segments[0] == 0x2002 {
        return is_public_ipv4(Ipv4Addr::from(
            (u32::from(segments[1]) << 16) | u32::from(segments[2]),
        ));
    }
    let first = segments[0];
    !(address.is_unspecified()
        || address.is_loopback()
        // IPv4-compatible (::a.b.c.d), deprecated and never public.
        || segments[..6] == [0, 0, 0, 0, 0, 0]
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || (first & 0xff00) == 0xff00
        || (first == 0x2001 && segments[1] == 0x0db8)
        || (first == 0x2001 && segments[1] < 0x0200)
        // Local-use NAT64 (64:ff9b:1::/48): a site's own translator, which
        // may reach any IPv4 address behind it.
        || (first == 0x64 && segments[1] == 0xff9b && segments[2] == 1)
        // Documentation (3fff::/20) and SRv6 segment identifiers (5f00::/16).
        || (first == 0x3fff && (segments[1] & 0xf000) == 0)
        || first == 0x5f00
        || first == 0x0100)
}

fn validate_description(value: Option<String>) -> Result<Option<String>, WebhookError> {
    match value {
        None => Ok(None),
        Some(value) => {
            let count = value.chars().count();
            if count == 0 || count > MAX_DESCRIPTION_CHARS || value.chars().any(char::is_control) {
                Err(WebhookError::InvalidDescription)
            } else {
                Ok(Some(value))
            }
        }
    }
}

fn validate_event_types(
    mut event_types: Vec<OutboxEventType>,
) -> Result<Vec<OutboxEventType>, WebhookError> {
    let count = event_types.len();
    event_types.sort_by_key(|event_type| event_type.as_str());
    event_types.dedup();
    if event_types.is_empty() || event_types.len() != count {
        return Err(WebhookError::InvalidEventTypes);
    }
    Ok(event_types)
}

/// An endpoint's signing secret in its `whsec_<base64>` form. It is shown to
/// the caller once, on create or rotate, and otherwise only exists sealed.
pub struct WebhookSecret(Zeroizing<String>);

impl fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookSecret(REDACTED)")
    }
}

impl WebhookSecret {
    pub fn generate() -> Self {
        let mut bytes = Zeroizing::new([0_u8; SECRET_BYTES]);
        OsRng.fill_bytes(bytes.as_mut());
        Self(Zeroizing::new(format!(
            "{SECRET_PREFIX}{}",
            STANDARD.encode(bytes.as_ref())
        )))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The key bytes, which is what is sealed and what signs.
    pub fn key_bytes(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(
            STANDARD
                .decode(&self.0[SECRET_PREFIX.len()..])
                .expect("generated secrets are valid base64"),
        )
    }
}

/// The JSON body POSTed for an event.
#[derive(Debug, Serialize)]
pub struct DeliveryEnvelope<'a> {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub event_type: &'a str,
    pub organization_id: OrganizationId,
    pub occurred_at: DateTime<Utc>,
    pub data: &'a serde_json::Value,
}

/// The `webhook-signature` header value for a delivery.
pub fn sign(key: &[u8], message_id: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(format!("{message_id}.{timestamp}.").as_bytes());
    mac.update(body);
    format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_must_be_https_to_a_public_host() {
        assert_eq!(
            validate_url("https://hooks.example.com/gateway?x=1").unwrap(),
            "https://hooks.example.com/gateway?x=1"
        );
        assert_eq!(
            validate_url("https://93.184.216.34:8443/").unwrap(),
            "https://93.184.216.34:8443/"
        );
        for (url, error) in [
            ("http://hooks.example.com/", WebhookError::InvalidUrl),
            ("ftp://hooks.example.com/", WebhookError::InvalidUrl),
            ("hooks.example.com", WebhookError::InvalidUrl),
            (
                "https://user:pw@hooks.example.com/",
                WebhookError::UrlCredentials,
            ),
            (
                "https://hooks.example.com/#frag",
                WebhookError::UrlCredentials,
            ),
            ("https://localhost/", WebhookError::PrivateHost),
            ("https://api.localhost/", WebhookError::PrivateHost),
            ("https://intranet/", WebhookError::PrivateHost),
            ("https://127.0.0.1/", WebhookError::PrivateHost),
            ("https://10.1.2.3/", WebhookError::PrivateHost),
            ("https://169.254.169.254/latest", WebhookError::PrivateHost),
            ("https://[::1]/", WebhookError::PrivateHost),
            ("https://[::ffff:192.168.0.1]/", WebhookError::PrivateHost),
            ("https://[fd00:ec2::254]/latest", WebhookError::PrivateHost),
            ("https://[fe80::1]/", WebhookError::PrivateHost),
            ("https://[64:ff9b::a9fe:a9fe]/", WebhookError::PrivateHost),
            ("https://[::ffff:0:a00:1]/", WebhookError::PrivateHost),
            ("https://0x7f000001/", WebhookError::PrivateHost),
            ("https://2130706433/", WebhookError::PrivateHost),
        ] {
            assert_eq!(validate_url(url), Err(error), "{url}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(validate_url(&long), Err(WebhookError::InvalidUrl));
    }

    #[test]
    fn only_public_addresses_are_allowed() {
        for private in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:c0a8:0101::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001:10::1",
            "100::1",
            // Teredo, IPv4-translated, local-use NAT64, documentation, SRv6.
            "2001:0:4136:e378::1",
            "::ffff:0:a9fe:a9fe",
            "::ffff:0:7f00:1",
            "64:ff9b:1::808:808",
            "3fff::1",
            "3fff:0fff:ffff::1",
            "5f00::1",
            // IMDS on IPv6: AWS (fd00:ec2::254) and Google (fd20:ce::254).
            "fd00:ec2::254",
            "fd20:ce::254",
        ] {
            assert!(!is_public_ip(private.parse().unwrap()), "{private}");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.128.0.1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "::ffff:0:808:808",
            // Just outside 3fff::/20.
            "3fff:1000::1",
            "3ffe:ffff::1",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
        ] {
            assert!(is_public_ip(public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn inputs_are_validated_and_event_types_normalized() {
        let valid = WebhookEndpointInput {
            url: "https://hooks.example.com/".into(),
            description: Some("Billing".into()),
            event_types: vec![
                OutboxEventType::ReconciliationOpened,
                OutboxEventType::InferenceFinalized,
            ],
            enabled: None,
        }
        .validate()
        .unwrap();
        assert!(valid.enabled);
        assert_eq!(
            valid.event_types,
            [
                OutboxEventType::InferenceFinalized,
                OutboxEventType::ReconciliationOpened
            ]
        );
        let base = WebhookEndpointInput {
            url: "https://hooks.example.com/".into(),
            description: None,
            event_types: vec![OutboxEventType::InferenceFinalized],
            enabled: None,
        };
        let invalid = |change: fn(&mut WebhookEndpointInput)| {
            let mut input = base.clone();
            change(&mut input);
            input.validate().unwrap_err()
        };
        assert_eq!(
            invalid(|input| input.event_types.clear()),
            WebhookError::InvalidEventTypes
        );
        assert_eq!(
            invalid(|input| input.event_types.push(OutboxEventType::InferenceFinalized)),
            WebhookError::InvalidEventTypes
        );
        assert_eq!(
            invalid(|input| input.description = Some(String::new())),
            WebhookError::InvalidDescription
        );
        assert_eq!(
            invalid(|input| input.description = Some("a\nb".into())),
            WebhookError::InvalidDescription
        );
        assert_eq!(
            WebhookEndpointUpdate::default().validate(),
            Err(WebhookError::EmptyUpdate)
        );
        let clear = WebhookEndpointUpdate {
            description: Some(String::new()),
            ..Default::default()
        }
        .validate()
        .unwrap();
        assert_eq!(clear.description, Some(None));
    }

    #[test]
    fn unknown_event_types_and_fields_are_rejected() {
        assert!(
            serde_json::from_value::<WebhookEndpointInput>(serde_json::json!({
                "url": "https://hooks.example.com/",
                "event_types": ["inference.started"],
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<WebhookEndpointInput>(serde_json::json!({
                "url": "https://hooks.example.com/",
                "event_types": ["inference.finalized"],
                "secret": "whsec_x",
            }))
            .is_err()
        );
    }

    #[test]
    fn secrets_are_random_redacted_and_round_trip_their_key() {
        let secret = WebhookSecret::generate();
        assert!(secret.expose().starts_with("whsec_"));
        assert_eq!(secret.key_bytes().len(), SECRET_BYTES);
        assert_ne!(secret.expose(), WebhookSecret::generate().expose());
        assert_eq!(format!("{secret:?}"), "WebhookSecret(REDACTED)");
    }

    #[test]
    fn signatures_match_the_standard_webhooks_test_vector() {
        // The vector published with the Standard Webhooks reference libraries.
        let key = STANDARD.decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        assert_eq!(
            sign(
                &key,
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1_614_265_330,
                br#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
    }
}
