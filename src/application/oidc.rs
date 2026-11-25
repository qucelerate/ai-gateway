//! The OpenID Connect provider integration: discovery, the authorization code
//! exchange (with PKCE, and the client secret when one is configured), and
//! ID token verification against the provider's published keys.
//!
//! Only what the callback needs is trusted from the provider: the discovery
//! document must name the configured issuer exactly; the ID token must be
//! signed with RS256 or ES256 by a key in the provider's JWKS (`none` and
//! shared-secret algorithms are refused); and its `iss`, `aud`, `azp`,
//! `nonce` and `exp` are checked here and again by the callback. The
//! discovery document is cached for an hour. Signing keys are cached for at
//! most [`KEYS_MAX_AGE`], so a key the provider revokes stops being trusted
//! within minutes; if they cannot be refreshed then, sign-in fails closed.
//! An unknown key id refreshes them early, at most once a minute after a
//! successful fetch (and every few seconds while fetches fail), so a key
//! rotation is picked up without letting forged tokens force a fetch per
//! request.
//!
//! Responses are size-limited, redirects are never followed, and no response
//! body, code, token or secret reaches the logs or an error.
//!
//! The `name` and `email` claims, and optionally the Microsoft Graph photo,
//! are passed on for display only. Fetching the photo never fails a sign-in.
//!
//! An organization's own provider (configured through the admin API, not by
//! the operator) is reached under the outbound rules for URLs the gateway
//! does not control ([`HttpOidcBackend::for_organization`]): https only, and
//! every request resolves the host, requires every address to be public and
//! pins the connection to them, so a tenant cannot point the gateway at an
//! internal service.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::domain::auth::{OidcBackend, OidcBackendError, OidcClaims, OidcProfile, ProfilePhoto};
use crate::jobs::egress::{DeliveryPolicy, pinned_client};

const DISCOVERY_TTL: Duration = Duration::from_secs(60 * 60);
const JWKS_REFRESH_MIN: Duration = Duration::from_secs(60);
/// Longest a fetched signing key is trusted without fetching the keys again.
const KEYS_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Pause between failed key fetches.
const JWKS_RETRY_MIN: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// The signed-in person's photo (the token is delegated, so `/me` is them).
const MICROSOFT_GRAPH_PHOTO: &str = "https://graph.microsoft.com/v1.0/me/photo/$value";
const MAX_NAME_CHARS: usize = 256;
const MAX_EMAIL_CHARS: usize = 320;

#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

struct Cached {
    discovery: Discovery,
    fetched: Instant,
}

/// The provider's signing keys. Kept apart from the discovery document, so
/// a slow key fetch never holds up sign-in starts.
#[derive(Default)]
struct KeyCache {
    /// The JWKS URL the keys came from; other keys are not used.
    jwks_uri: String,
    keys: HashMap<String, Jwk>,
    /// When the keys were last fetched successfully.
    fetched: Option<Instant>,
    /// When a fetch last failed.
    failed: Option<Instant>,
}

#[derive(Clone, Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

/// The production [`OidcBackend`].
pub struct HttpOidcBackend {
    client: reqwest::Client,
    client_secret: Option<Zeroizing<String>>,
    /// Plain http to a loopback issuer, for tests only.
    allow_insecure: bool,
    cache: Arc<Mutex<Option<Cached>>>,
    keys: Arc<Mutex<KeyCache>>,
    /// Where to fetch the person's photo with the access token, if anywhere.
    photo_url: Option<Url>,
    /// For an organization's provider: the outbound rules every request
    /// follows, with a client pinned to that request's checked addresses.
    egress: Option<DeliveryPolicy>,
}

impl HttpOidcBackend {
    pub fn new(client_secret: Option<Zeroizing<String>>) -> Result<Self, OidcBackendError> {
        Self::build(client_secret, false)
    }

    /// Also accepts plain http to loopback addresses. Never use outside tests.
    pub fn insecure_for_tests(
        client_secret: Option<Zeroizing<String>>,
    ) -> Result<Self, OidcBackendError> {
        Self::build(client_secret, true)
    }

    /// For an organization's own provider: every request follows `policy`
    /// (public https destinations only, outside tests). Never fetches a photo.
    pub fn for_organization(
        client_secret: Zeroizing<String>,
        policy: DeliveryPolicy,
    ) -> Result<Self, OidcBackendError> {
        let mut backend = Self::build(Some(client_secret), false)?;
        backend.egress = Some(policy);
        Ok(backend)
    }

    fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("qucelerate-gateway/", env!("CARGO_PKG_VERSION")))
    }

    fn build(
        client_secret: Option<Zeroizing<String>>,
        allow_insecure: bool,
    ) -> Result<Self, OidcBackendError> {
        let client = Self::client_builder()
            .build()
            .map_err(|_| OidcBackendError::NotDispatched)?;
        Ok(Self {
            client,
            client_secret,
            allow_insecure,
            cache: Arc::default(),
            keys: Arc::default(),
            photo_url: None,
            egress: None,
        })
    }

    /// The client to reach `url` with: under the outbound rules, one pinned
    /// to `url`'s host at addresses resolved and checked now. Failing here
    /// sends nothing.
    async fn client_for(&self, url: &Url) -> Result<reqwest::Client, OidcBackendError> {
        match self.egress {
            None => Ok(self.client.clone()),
            Some(policy) => pinned_client(url, policy, Self::client_builder())
                .await
                .map_err(|_| OidcBackendError::NotDispatched),
        }
    }

    /// Also fetch the person's photo from Microsoft Graph at sign-in. The
    /// sign-in must then request the `User.Read` scope.
    pub fn with_microsoft_graph_photo(mut self) -> Self {
        self.photo_url = Url::parse(MICROSOFT_GRAPH_PHOTO).ok();
        self
    }

    #[cfg(test)]
    fn with_photo_url(mut self, url: Url) -> Self {
        self.photo_url = Some(url);
        self
    }

    /// An endpoint the provider names, if it is one we may call.
    fn endpoint(&self, value: &str) -> Option<Url> {
        let url = Url::parse(value).ok()?;
        let loopback = crate::net::is_loopback_url(&url);
        let allowed = match self.egress {
            // Where it may connect is checked per request, on the resolved
            // addresses; plain http only where the policy admits tests.
            Some(policy) => {
                url.scheme() == "https"
                    || (policy.allows_private_targets() && url.scheme() == "http")
            }
            None => url.scheme() == "https" || (self.allow_insecure && loopback),
        };
        (allowed
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none())
        .then_some(url)
    }

    async fn get_json(&self, url: Url) -> Result<Value, OidcBackendError> {
        let client = self
            .client_for(&url)
            .await
            .map_err(|_| OidcBackendError::Unavailable)?;
        let response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| OidcBackendError::Unavailable)?;
        if !response.status().is_success() {
            return Err(OidcBackendError::Unavailable);
        }
        read_json(response).await
    }

    /// The person's photo, fetched with the sign-in's access token. Anything
    /// but a JPEG or PNG within [`MAX_RESPONSE_BYTES`] is ignored.
    async fn photo(&self, url: Url, access_token: &str) -> ProfilePhoto {
        let Ok(response) = self.client.get(url).bearer_auth(access_token).send().await else {
            return ProfilePhoto::Unknown;
        };
        if response.status() == StatusCode::NOT_FOUND {
            return ProfilePhoto::Absent;
        }
        if !response.status().is_success() {
            return ProfilePhoto::Unknown;
        }
        let Ok(bytes) = read_body(response).await else {
            return ProfilePhoto::Unknown;
        };
        let content_type = if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else {
            return ProfilePhoto::Unknown;
        };
        ProfilePhoto::Present {
            content_type: content_type.to_owned(),
            bytes,
        }
    }

    /// The provider's discovery document, cached for [`DISCOVERY_TTL`]. It
    /// must name `issuer` exactly.
    async fn discovery(&self, issuer: &str) -> Result<Discovery, OidcBackendError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.discovery.issuer == issuer && cached.fetched.elapsed() < DISCOVERY_TTL {
                return Ok(cached.discovery.clone());
            }
        }
        let url = self
            .endpoint(&format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            ))
            .ok_or(OidcBackendError::NotDispatched)?;
        let document: Discovery = serde_json::from_value(self.get_json(url).await?)
            .map_err(|_| OidcBackendError::InvalidResponse)?;
        let endpoints = [
            &document.authorization_endpoint,
            &document.token_endpoint,
            &document.jwks_uri,
        ];
        if document.issuer != issuer || endpoints.iter().any(|url| self.endpoint(url).is_none()) {
            return Err(OidcBackendError::InvalidResponse);
        }
        *cache = Some(Cached {
            discovery: document.clone(),
            fetched: Instant::now(),
        });
        Ok(document)
    }

    /// The key `kid` names. The keys are fetched again once they are
    /// [`KEYS_MAX_AGE`] old, failing closed if that fails, and early for an
    /// unknown `kid`, at most once per [`JWKS_REFRESH_MIN`] after a success
    /// and per [`JWKS_RETRY_MIN`] after a failure.
    async fn key(&self, kid: Option<&str>) -> Result<Jwk, OidcBackendError> {
        let jwks_uri = self.current_jwks_uri().await?;
        let mut cached = self.keys.lock().await;
        if cached.jwks_uri != jwks_uri {
            *cached = KeyCache {
                jwks_uri,
                ..KeyCache::default()
            };
        }
        let age = cached.fetched.map(|fetched| fetched.elapsed());
        let fresh = age.is_some_and(|age| age < KEYS_MAX_AGE);
        if fresh {
            if let Some(key) = find_key(&cached.keys, kid) {
                return Ok(key);
            }
        }
        let recently =
            |at: Option<Instant>, within: Duration| at.is_some_and(|at| at.elapsed() < within);
        // An unknown kid among fresh keys: refresh only if allowed.
        if fresh
            && (recently(cached.fetched, JWKS_REFRESH_MIN)
                || recently(cached.failed, JWKS_RETRY_MIN))
        {
            return Err(OidcBackendError::InvalidResponse);
        }
        if !fresh && recently(cached.failed, JWKS_RETRY_MIN) {
            return Err(OidcBackendError::Unavailable);
        }
        let url = self
            .endpoint(&cached.jwks_uri)
            .ok_or(OidcBackendError::InvalidResponse)?;
        let keys = match self.fetch_keys(url).await {
            Ok(keys) => keys,
            Err(error) => {
                cached.failed = Some(Instant::now());
                // Expired keys are not trusted, whatever the reason.
                if !fresh {
                    cached.keys.clear();
                }
                return Err(error);
            }
        };
        // Discovery may have moved to another key set while this fetch ran;
        // keys from a superseded set are never trusted.
        if self.current_jwks_uri().await? != cached.jwks_uri {
            *cached = KeyCache::default();
            return Err(OidcBackendError::InvalidResponse);
        }
        cached.keys = keys;
        cached.fetched = Some(Instant::now());
        cached.failed = None;
        find_key(&cached.keys, kid).ok_or(OidcBackendError::InvalidResponse)
    }

    /// The JWKS URL of the cached discovery document.
    async fn current_jwks_uri(&self) -> Result<String, OidcBackendError> {
        self.cache
            .lock()
            .await
            .as_ref()
            .map(|cached| cached.discovery.jwks_uri.clone())
            .ok_or(OidcBackendError::InvalidResponse)
    }

    async fn fetch_keys(&self, url: Url) -> Result<HashMap<String, Jwk>, OidcBackendError> {
        let document = self.get_json(url).await?;
        let keys: Vec<Jwk> = serde_json::from_value(
            document
                .get("keys")
                .cloned()
                .ok_or(OidcBackendError::InvalidResponse)?,
        )
        .map_err(|_| OidcBackendError::InvalidResponse)?;
        Ok(keys
            .into_iter()
            .filter(|key| key.usage.as_deref().is_none_or(|usage| usage == "sig"))
            .enumerate()
            .map(|(index, key)| (key.kid.clone().unwrap_or_else(|| format!("#{index}")), key))
            .collect())
    }
}

/// The key named `kid`; without a kid, the only key there is.
fn find_key(keys: &HashMap<String, Jwk>, kid: Option<&str>) -> Option<Jwk> {
    match kid {
        Some(kid) => keys.get(kid).cloned(),
        None if keys.len() == 1 => keys.values().next().cloned(),
        None => None,
    }
}

/// The response as JSON, reading at most [`MAX_RESPONSE_BYTES`] whatever
/// its `Content-Length` says.
async fn read_json(response: reqwest::Response) -> Result<Value, OidcBackendError> {
    let body = read_body(response).await?;
    serde_json::from_slice(&body).map_err(|_| OidcBackendError::InvalidResponse)
}

/// The response body, at most [`MAX_RESPONSE_BYTES`] of it.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, OidcBackendError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(OidcBackendError::InvalidResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OidcBackendError::Unavailable)?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(OidcBackendError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[async_trait]
impl OidcBackend for HttpOidcBackend {
    async fn authorization_endpoint(&self, issuer: &str) -> Result<Url, OidcBackendError> {
        let discovery = self.discovery(issuer).await?;
        Url::parse(&discovery.authorization_endpoint).map_err(|_| OidcBackendError::InvalidResponse)
    }

    async fn authenticate(
        &self,
        issuer: &str,
        audience: &str,
        client_id: &str,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
    ) -> Result<OidcClaims, OidcBackendError> {
        // Everything before the token request either fails without sending
        // the code, or not at all.
        let discovery = self
            .discovery(issuer)
            .await
            .map_err(|_| OidcBackendError::NotDispatched)?;
        let token_endpoint = self
            .endpoint(&discovery.token_endpoint)
            .ok_or(OidcBackendError::NotDispatched)?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        let mut request = self
            .client_for(&token_endpoint)
            .await?
            .post(token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json");
        match &self.client_secret {
            // client_secret_basic, the default authentication method.
            Some(secret) => {
                request = request.basic_auth(
                    form_urlencode(client_id),
                    Some(form_urlencode(secret.as_str())),
                );
            }
            None => form.push(("client_id", client_id)),
        }
        let response = request
            .form(&form)
            .send()
            .await
            .map_err(|_| OidcBackendError::Unavailable)?;
        let status = response.status();
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(OidcBackendError::Unavailable);
        }
        if !status.is_success() {
            return Err(OidcBackendError::InvalidResponse);
        }
        let token = read_json(response).await?;
        let id_token = token
            .get("id_token")
            .and_then(Value::as_str)
            .ok_or(OidcBackendError::InvalidResponse)?;
        let claims = self.verify(id_token).await?;
        let mut claims = check_claims(claims, issuer, audience, client_id)?;
        if let (Some(url), Some(access_token)) = (
            self.photo_url.clone(),
            token.get("access_token").and_then(Value::as_str),
        ) {
            claims.profile.photo = self.photo(url, access_token).await;
        }
        Ok(claims)
    }
}

/// `application/x-www-form-urlencoded`, as client_secret_basic requires for
/// the id and secret before they are base64-encoded.
fn form_urlencode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    azp: Option<String>,
    nonce: Option<String>,
    exp: u64,
    name: Option<String>,
    email: Option<String>,
    preferred_username: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl HttpOidcBackend {
    /// The claims of a compact JWS whose signature a provider key verifies.
    async fn verify(&self, token: &str) -> Result<Claims, OidcBackendError> {
        let invalid = || OidcBackendError::InvalidResponse;
        if token.len() > 16 * 1024 {
            return Err(invalid());
        }
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|_| invalid());
        let parsed: Header = serde_json::from_slice(&decode(header)?).map_err(|_| invalid())?;
        let key = self.key(parsed.kid.as_deref()).await?;
        if key.alg.as_deref().is_some_and(|alg| alg != parsed.alg) {
            return Err(invalid());
        }
        let signed = &token.as_bytes()[..header.len() + 1 + payload.len()];
        verify_signature(&parsed.alg, &key, signed, &decode(signature)?)?;
        serde_json::from_slice(&decode(payload)?).map_err(|_| invalid())
    }
}

fn verify_signature(
    alg: &str,
    key: &Jwk,
    message: &[u8],
    signature: &[u8],
) -> Result<(), OidcBackendError> {
    use ring::signature::{
        ECDSA_P256_SHA256_FIXED, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents,
        UnparsedPublicKey,
    };
    let invalid = || OidcBackendError::InvalidResponse;
    let field = |value: &Option<String>| {
        value
            .as_deref()
            .ok_or_else(invalid)
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid()))
    };
    match (alg, key.kty.as_str()) {
        ("RS256", "RSA") => RsaPublicKeyComponents {
            n: field(&key.n)?,
            e: field(&key.e)?,
        }
        .verify(&RSA_PKCS1_2048_8192_SHA256, message, signature)
        .map_err(|_| invalid()),
        ("ES256", "EC") if key.crv.as_deref() == Some("P-256") => {
            let point = [vec![4], field(&key.x)?, field(&key.y)?].concat();
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
                .verify(message, signature)
                .map_err(|_| invalid())
        }
        // `none`, shared-secret (HS*) and anything else are refused.
        _ => Err(invalid()),
    }
}

/// The token's claims for the callback, after the checks this backend owns:
/// the issuer, that the client is an audience, and `azp` when there are
/// several audiences.
fn check_claims(
    claims: Claims,
    issuer: &str,
    audience: &str,
    client_id: &str,
) -> Result<OidcClaims, OidcBackendError> {
    let audiences = match claims.aud {
        Audience::One(one) => vec![one],
        Audience::Many(many) => many,
    };
    let azp_ok = audiences.len() == 1 || claims.azp.as_deref() == Some(client_id);
    if claims.iss != issuer
        || !audiences.iter().any(|aud| aud == audience)
        || !azp_ok
        || claims.sub.is_empty()
    {
        return Err(OidcBackendError::InvalidResponse);
    }
    Ok(OidcClaims {
        issuer: claims.iss,
        subject: claims.sub,
        audience: audiences,
        nonce: claims.nonce.ok_or(OidcBackendError::InvalidResponse)?,
        expires_at: claims.exp,
        profile: OidcProfile {
            name: display_text(claims.name, MAX_NAME_CHARS),
            email: claims
                .email
                .or(claims.preferred_username)
                .and_then(|value| display_text(Some(value), MAX_EMAIL_CHARS))
                .filter(|value| value.contains('@') && !value.contains(char::is_whitespace)),
            photo: ProfilePhoto::Unknown,
        },
    })
}

/// A claim fit to display: trimmed, without control characters, and not
/// empty or over `max` characters.
fn display_text(value: Option<String>, max: usize) -> Option<String> {
    let value: String = value?.chars().filter(|c| !c.is_control()).collect();
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= max).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Form, Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode as HttpStatus},
        routing::{get, post},
    };
    use ring::{
        rand::SystemRandom,
        signature::{
            ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair, RSA_PKCS1_SHA256, RsaKeyPair,
            RsaPublicKeyComponents,
        },
    };
    use serde_json::json;

    use super::*;

    /// A throwaway 2048-bit RSA key generated for these tests only.
    const RSA_TEST_KEY: &[u8] = include_bytes!("testdata/oidc-test-rsa.pk8");

    struct Provider {
        issuer: String,
        rsa: RsaKeyPair,
        ec: EcdsaKeyPair,
        /// What the token endpoint answers: status and the id_token claims.
        token: StdMutex<(u16, Value, &'static str)>,
        /// The discovery document's issuer, when it should differ.
        discovered_issuer: StdMutex<Option<String>>,
        jwks_fetches: AtomicUsize,
        /// Answer key requests with 503.
        jwks_down: std::sync::atomic::AtomicBool,
        /// Answer key requests after 200 ms.
        jwks_slow: std::sync::atomic::AtomicBool,
        /// What the photo endpoint answers, and the Authorization it last saw.
        photo: StdMutex<(u16, Vec<u8>)>,
        photo_auth: StdMutex<Option<String>>,
        /// The last token request: form fields and Authorization header.
        seen: StdMutex<Option<(HashMap<String, String>, Option<String>)>>,
    }

    type Shared = Arc<Provider>;

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    impl Provider {
        /// A compact JWS of `claims`, signed per `alg` (`RS256`, `ES256`,
        /// or anything else unsigned).
        fn sign(&self, alg: &str, kid: &str, claims: &Value) -> String {
            let header = b64(json!({"alg": alg, "kid": kid, "typ": "JWT"})
                .to_string()
                .as_bytes());
            let payload = b64(claims.to_string().as_bytes());
            let input = format!("{header}.{payload}");
            let signature = match alg {
                "RS256" => {
                    let mut out = vec![0; self.rsa.public().modulus_len()];
                    self.rsa
                        .sign(
                            &RSA_PKCS1_SHA256,
                            &SystemRandom::new(),
                            input.as_bytes(),
                            &mut out,
                        )
                        .unwrap();
                    out
                }
                "ES256" => self
                    .ec
                    .sign(&SystemRandom::new(), input.as_bytes())
                    .unwrap()
                    .as_ref()
                    .to_vec(),
                _ => Vec::new(),
            };
            format!("{input}.{}", b64(&signature))
        }

        fn jwks(&self) -> Value {
            let rsa = RsaPublicKeyComponents::<Vec<u8>>::from(self.rsa.public());
            let point = self.ec.public_key().as_ref();
            json!({"keys": [
                {"kty": "RSA", "kid": "rsa-1", "use": "sig", "alg": "RS256",
                 "n": b64(&rsa.n), "e": b64(&rsa.e)},
                {"kty": "EC", "kid": "ec-1", "use": "sig", "crv": "P-256",
                 "x": b64(&point[1..33]), "y": b64(&point[33..65])},
            ]})
        }
    }

    async fn discovery(State(provider): State<Shared>) -> Json<Value> {
        let issuer = provider
            .discovered_issuer
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| provider.issuer.clone());
        Json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/oauth2/authorize", provider.issuer),
            "token_endpoint": format!("{}/oauth2/token", provider.issuer),
            "jwks_uri": format!("{}/keys", provider.issuer),
        }))
    }

    async fn keys(State(provider): State<Shared>) -> (HttpStatus, Json<Value>) {
        provider.jwks_fetches.fetch_add(1, Ordering::SeqCst);
        if provider.jwks_slow.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if provider.jwks_down.load(Ordering::SeqCst) {
            return (HttpStatus::SERVICE_UNAVAILABLE, Json(Value::Null));
        }
        (HttpStatus::OK, Json(provider.jwks()))
    }

    /// 300 KiB of JSON, streamed without a Content-Length.
    async fn huge() -> axum::body::Body {
        let chunks = (0..300).map(|index| {
            Ok::<_, std::io::Error>(if index == 0 {
                bytes::Bytes::from(format!("{{\"pad\":\"{}", "x".repeat(1023)))
            } else {
                bytes::Bytes::from("x".repeat(1024))
            })
        });
        axum::body::Body::from_stream(futures::stream::iter(chunks))
    }

    async fn token(
        State(provider): State<Shared>,
        headers: HeaderMap,
        Form(form): Form<HashMap<String, String>>,
    ) -> (HttpStatus, Json<Value>) {
        let authorization = headers
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_owned());
        *provider.seen.lock().unwrap() = Some((form, authorization));
        let (status, claims, alg) = provider.token.lock().unwrap().clone();
        let kid = if alg == "ES256" { "ec-1" } else { "rsa-1" };
        (
            HttpStatus::from_u16(status).unwrap(),
            Json(json!({
                "id_token": provider.sign(alg, kid, &claims),
                "access_token": "graph-token",
                "token_type": "Bearer",
            })),
        )
    }

    async fn photo(State(provider): State<Shared>, headers: HeaderMap) -> (HttpStatus, Vec<u8>) {
        *provider.photo_auth.lock().unwrap() = headers
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_owned());
        let (status, body) = provider.photo.lock().unwrap().clone();
        (HttpStatus::from_u16(status).unwrap(), body)
    }

    async fn start() -> Shared {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let provider = Arc::new(Provider {
            issuer,
            rsa: RsaKeyPair::from_pkcs8(RSA_TEST_KEY).unwrap(),
            ec: EcdsaKeyPair::from_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                EcdsaKeyPair::generate_pkcs8(
                    &ECDSA_P256_SHA256_FIXED_SIGNING,
                    &SystemRandom::new(),
                )
                .unwrap()
                .as_ref(),
                &SystemRandom::new(),
            )
            .unwrap(),
            token: StdMutex::new((200, Value::Null, "RS256")),
            discovered_issuer: StdMutex::new(None),
            jwks_fetches: AtomicUsize::new(0),
            jwks_down: std::sync::atomic::AtomicBool::new(false),
            jwks_slow: std::sync::atomic::AtomicBool::new(false),
            seen: StdMutex::new(None),
            photo: StdMutex::new((404, Vec::new())),
            photo_auth: StdMutex::new(None),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/keys", get(keys))
            .route("/oauth2/token", post(token))
            .route("/photo", get(photo))
            .route("/huge/.well-known/openid-configuration", get(huge))
            .with_state(provider.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        provider
    }

    fn claims(provider: &Provider) -> Value {
        json!({
            "iss": provider.issuer, "sub": "user-123", "aud": "gateway-client",
            "nonce": "nonce-1", "exp": 4_000_000_000_u64, "iat": 1_700_000_000_u64,
        })
    }

    async fn login(
        backend: &HttpOidcBackend,
        provider: &Provider,
    ) -> Result<OidcClaims, OidcBackendError> {
        backend
            .authenticate(
                &provider.issuer,
                "gateway-client",
                "gateway-client",
                "https://gateway.example/api/admin/v1/oidc/callback",
                "code-1",
                "verifier-1",
            )
            .await
    }

    #[tokio::test]
    async fn the_profile_and_photo_are_passed_on_for_display() {
        let provider = start().await;
        let mut with_profile = claims(&provider);
        with_profile["name"] = json!("  Ada\u{7}  Lovelace ");
        with_profile["preferred_username"] = json!("ada@example.com");
        *provider.token.lock().unwrap() = (200, with_profile.clone(), "RS256");

        // Without a photo source, only the claims; the photo is unknown.
        let plain = HttpOidcBackend::insecure_for_tests(None).unwrap();
        let profile = login(&plain, &provider).await.unwrap().profile;
        assert_eq!(profile.name.as_deref(), Some("Ada  Lovelace"));
        assert_eq!(profile.email.as_deref(), Some("ada@example.com"));
        assert_eq!(profile.photo, ProfilePhoto::Unknown);
        assert_eq!(*provider.photo_auth.lock().unwrap(), None);

        let url = Url::parse(&format!("{}/photo", provider.issuer)).unwrap();
        let backend = HttpOidcBackend::insecure_for_tests(None)
            .unwrap()
            .with_photo_url(url);
        let jpeg = [&[0xFF, 0xD8, 0xFF, 0xE0][..], b"rest"].concat();
        *provider.photo.lock().unwrap() = (200, jpeg.clone());
        let profile = login(&backend, &provider).await.unwrap().profile;
        assert_eq!(
            profile.photo,
            ProfilePhoto::Present {
                content_type: "image/jpeg".into(),
                bytes: jpeg
            }
        );
        // Fetched with the sign-in's access token.
        assert_eq!(
            provider.photo_auth.lock().unwrap().as_deref(),
            Some("Bearer graph-token")
        );

        // No photo: absent. Not an image, an error, or too large: unknown,
        // and the sign-in still succeeds.
        *provider.photo.lock().unwrap() = (404, Vec::new());
        assert_eq!(
            login(&backend, &provider).await.unwrap().profile.photo,
            ProfilePhoto::Absent
        );
        for answer in [
            (200, b"<html>".to_vec()),
            (500, Vec::new()),
            (
                200,
                [&[0xFF, 0xD8, 0xFF][..], &vec![0; 300 * 1024]].concat(),
            ),
        ] {
            *provider.photo.lock().unwrap() = answer;
            assert_eq!(
                login(&backend, &provider).await.unwrap().profile.photo,
                ProfilePhoto::Unknown
            );
        }

        // An email claim wins; values that are not fit to show are dropped.
        with_profile["email"] = json!("a.lovelace@example.com");
        *provider.token.lock().unwrap() = (200, with_profile.clone(), "RS256");
        let profile = login(&plain, &provider).await.unwrap().profile;
        assert_eq!(profile.email.as_deref(), Some("a.lovelace@example.com"));
        with_profile["name"] = json!("x".repeat(MAX_NAME_CHARS + 1));
        with_profile["email"] = json!("not an email");
        *provider.token.lock().unwrap() = (200, with_profile, "RS256");
        let profile = login(&plain, &provider).await.unwrap().profile;
        assert_eq!((profile.name, profile.email), (None, None));
    }

    #[tokio::test]
    async fn signed_id_tokens_from_the_provider_sign_in() {
        let provider = start().await;
        let backend =
            HttpOidcBackend::insecure_for_tests(Some(Zeroizing::new("s3cr:et".to_owned())))
                .unwrap();
        // The browser is sent to the endpoint discovery names.
        assert_eq!(
            backend
                .authorization_endpoint(&provider.issuer)
                .await
                .unwrap()
                .as_str(),
            format!("{}/oauth2/authorize", provider.issuer)
        );
        for alg in ["RS256", "ES256"] {
            *provider.token.lock().unwrap() = (200, claims(&provider), alg);
            let signed_in = login(&backend, &provider).await.unwrap();
            assert_eq!(signed_in.subject, "user-123", "{alg}");
            assert_eq!(signed_in.nonce, "nonce-1");
            assert_eq!(signed_in.audience, ["gateway-client"]);
        }
        // The code exchange carries PKCE and authenticates the client with
        // HTTP Basic, form-encoding the id and secret first.
        let (form, authorization) = provider.seen.lock().unwrap().clone().unwrap();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "code-1");
        assert_eq!(form["code_verifier"], "verifier-1");
        assert!(!form.contains_key("client_id"));
        let expected = base64::engine::general_purpose::STANDARD.encode("gateway-client:s3cr%3Aet");
        assert_eq!(authorization, Some(format!("Basic {expected}")));
        // Keys are fetched once and cached.
        assert_eq!(provider.jwks_fetches.load(Ordering::SeqCst), 1);

        // A public client sends its id in the form instead.
        let public = HttpOidcBackend::insecure_for_tests(None).unwrap();
        login(&public, &provider).await.unwrap();
        let (form, authorization) = provider.seen.lock().unwrap().clone().unwrap();
        assert_eq!(form["client_id"], "gateway-client");
        assert_eq!(authorization, None);
    }

    #[tokio::test]
    async fn forged_or_mismatched_tokens_are_refused() {
        let provider = start().await;
        let backend = HttpOidcBackend::insecure_for_tests(None).unwrap();
        let base = claims(&provider);
        let mut cases: Vec<(&str, Value, &'static str)> = vec![
            ("unsigned", base.clone(), "none"),
            ("shared secret", base.clone(), "HS256"),
        ];
        for (name, field, value) in [
            ("other issuer", "iss", json!("https://evil.example")),
            ("other audience", "aud", json!("someone-else")),
            (
                "several audiences without azp",
                "aud",
                json!(["gateway-client", "other"]),
            ),
            ("no nonce", "nonce", Value::Null),
        ] {
            let mut claims = base.clone();
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(field);
            } else {
                claims[field] = value;
            }
            cases.push((name, claims, "RS256"));
        }
        for (name, claims, alg) in cases {
            *provider.token.lock().unwrap() = (200, claims, alg);
            assert_eq!(
                login(&backend, &provider).await.unwrap_err(),
                OidcBackendError::InvalidResponse,
                "{name}"
            );
        }
        // Several audiences are fine when this client is the authorized party.
        let mut claims = base.clone();
        claims["aud"] = json!(["gateway-client", "other"]);
        claims["azp"] = json!("gateway-client");
        *provider.token.lock().unwrap() = (200, claims, "RS256");
        assert!(login(&backend, &provider).await.is_ok());

        // A token whose payload was changed after signing.
        let token = provider.sign("RS256", "rsa-1", &base);
        let mut parts: Vec<&str> = token.split('.').collect();
        let mut forged = base.clone();
        forged["sub"] = json!("admin");
        let payload = b64(forged.to_string().as_bytes());
        parts[1] = &payload;
        assert_eq!(
            backend.verify(&parts.join(".")).await.err(),
            Some(OidcBackendError::InvalidResponse)
        );
        // An unknown key id refreshes the keys, but at most once a minute:
        // forged kids cannot make every request fetch.
        let fetches = provider.jwks_fetches.load(Ordering::SeqCst);
        let unknown = provider.sign("RS256", "rotated-away", &base);
        for _ in 0..3 {
            assert!(backend.verify(&unknown).await.is_err());
        }
        assert_eq!(provider.jwks_fetches.load(Ordering::SeqCst), fetches);
        let a_while_ago = Instant::now().checked_sub(JWKS_REFRESH_MIN * 2);
        backend.keys.lock().await.fetched = a_while_ago;
        for _ in 0..3 {
            assert!(backend.verify(&unknown).await.is_err());
        }
        assert_eq!(provider.jwks_fetches.load(Ordering::SeqCst), fetches + 1);
    }

    #[tokio::test]
    async fn provider_failures_are_classified() {
        let provider = start().await;
        let backend = HttpOidcBackend::insecure_for_tests(None).unwrap();
        for (status, error) in [
            (500, OidcBackendError::Unavailable),
            (429, OidcBackendError::Unavailable),
            (400, OidcBackendError::InvalidResponse),
        ] {
            *provider.token.lock().unwrap() = (status, claims(&provider), "RS256");
            assert_eq!(
                login(&backend, &provider).await.unwrap_err(),
                error,
                "{status}"
            );
        }
        // A discovery document for another issuer: the code is never sent.
        let impostor = HttpOidcBackend::insecure_for_tests(None).unwrap();
        *provider.discovered_issuer.lock().unwrap() = Some("https://other.example".to_owned());
        *provider.seen.lock().unwrap() = None;
        assert_eq!(
            login(&impostor, &provider).await.unwrap_err(),
            OidcBackendError::NotDispatched
        );
        assert!(provider.seen.lock().unwrap().is_none());
        // Plain http is refused outside tests.
        let strict = HttpOidcBackend::new(None).unwrap();
        assert_eq!(
            login(&strict, &provider).await.unwrap_err(),
            OidcBackendError::NotDispatched
        );
    }

    #[tokio::test]
    async fn cached_keys_expire_and_failed_fetches_do_not_block_recovery() {
        let provider = start().await;
        let backend = HttpOidcBackend::insecure_for_tests(None).unwrap();
        *provider.token.lock().unwrap() = (200, claims(&provider), "RS256");
        login(&backend, &provider).await.unwrap();
        let token = provider.sign("RS256", "rsa-1", &claims(&provider));
        let fetches = provider.jwks_fetches.load(Ordering::SeqCst);
        let age = |by: Duration| Instant::now().checked_sub(by);

        // A known key is refetched once it is too old to trust...
        backend.keys.lock().await.fetched = age(KEYS_MAX_AGE * 2);
        assert!(backend.verify(&token).await.is_ok());
        assert_eq!(provider.jwks_fetches.load(Ordering::SeqCst), fetches + 1);
        // ...and if the provider cannot confirm it then, sign-in fails
        // closed rather than trusting a possibly revoked key.
        provider.jwks_down.store(true, Ordering::SeqCst);
        backend.keys.lock().await.fetched = age(KEYS_MAX_AGE * 2);
        assert!(backend.verify(&token).await.is_err());
        assert!(
            backend.verify(&token).await.is_err(),
            "no retry within seconds"
        );
        assert_eq!(provider.jwks_fetches.load(Ordering::SeqCst), fetches + 2);

        // A failed fetch does not count against the once-a-minute refresh:
        // once the provider is back, sign-in recovers after a short pause.
        provider.jwks_down.store(false, Ordering::SeqCst);
        backend.keys.lock().await.failed = age(JWKS_RETRY_MIN * 2);
        assert!(backend.verify(&token).await.is_ok());
        assert_eq!(provider.jwks_fetches.load(Ordering::SeqCst), fetches + 3);
    }

    #[tokio::test]
    async fn keys_from_a_superseded_key_set_are_not_trusted() {
        let provider = start().await;
        let backend = Arc::new(HttpOidcBackend::insecure_for_tests(None).unwrap());
        *provider.token.lock().unwrap() = (200, claims(&provider), "RS256");
        login(&backend, &provider).await.unwrap();
        let token = provider.sign("RS256", "rsa-1", &claims(&provider));
        // Expire the keys and slow the key endpoint, then publish a new key
        // set location while the refresh is in flight.
        backend.keys.lock().await.fetched = Instant::now().checked_sub(KEYS_MAX_AGE * 2);
        provider.jwks_slow.store(true, Ordering::SeqCst);
        let verifying = {
            let (backend, token) = (backend.clone(), token.clone());
            tokio::spawn(async move { backend.verify(&token).await.err() })
        };
        while provider.jwks_fetches.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        backend
            .cache
            .lock()
            .await
            .as_mut()
            .unwrap()
            .discovery
            .jwks_uri = format!("{}/keys-v2", provider.issuer);
        assert_eq!(
            verifying.await.unwrap(),
            Some(OidcBackendError::InvalidResponse)
        );
        assert!(backend.keys.lock().await.keys.is_empty());
    }

    #[tokio::test]
    async fn organization_providers_follow_the_outbound_rules() {
        let provider = start().await;
        *provider.token.lock().unwrap() = (200, claims(&provider), "RS256");
        // Public destinations only: a loopback provider is never contacted,
        // and the code is never sent.
        let strict = HttpOidcBackend::for_organization(
            Zeroizing::new("s".into()),
            DeliveryPolicy::public_only(),
        )
        .unwrap();
        *provider.seen.lock().unwrap() = None;
        assert_eq!(
            login(&strict, &provider).await.unwrap_err(),
            OidcBackendError::NotDispatched
        );
        assert!(provider.seen.lock().unwrap().is_none());
        assert!(
            strict
                .authorization_endpoint(&provider.issuer)
                .await
                .is_err()
        );
        // Where the policy admits it (tests), the same checks as ever apply.
        let local = HttpOidcBackend::for_organization(
            Zeroizing::new("s".into()),
            DeliveryPolicy::allow_private_targets_for_tests(),
        )
        .unwrap();
        assert_eq!(login(&local, &provider).await.unwrap().subject, "user-123");
        *provider.discovered_issuer.lock().unwrap() = Some(format!("{}/", provider.issuer));
        let fresh = HttpOidcBackend::for_organization(
            Zeroizing::new("s".into()),
            DeliveryPolicy::allow_private_targets_for_tests(),
        )
        .unwrap();
        // The discovery document must name the issuer exactly: not even a
        // trailing slash differs.
        assert_eq!(
            login(&fresh, &provider).await.unwrap_err(),
            OidcBackendError::NotDispatched
        );
    }

    #[tokio::test]
    async fn oversized_responses_are_cut_off_while_read() {
        let provider = start().await;
        let backend = HttpOidcBackend::insecure_for_tests(None).unwrap();
        assert_eq!(
            backend
                .authorization_endpoint(&format!("{}/huge", provider.issuer))
                .await
                .unwrap_err(),
            OidcBackendError::InvalidResponse
        );
    }
}
