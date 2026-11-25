//! Google OAuth access tokens for the Gemini API.
//!
//! A [`GoogleIdentity`] is the gateway's own Google identity, found the way
//! Application Default Credentials find it: the credentials file named by
//! `GOOGLE_APPLICATION_CREDENTIALS`, else gcloud's
//! `application_default_credentials.json`, else the metadata server of the
//! Google Cloud host. A credentials file holds one of:
//!
//! - a service account key, exchanged with a signed JWT;
//! - a user's refresh token (`gcloud auth application-default login`);
//! - workload identity federation (`external_account`): a token from another
//!   identity provider, read from a file or URL, exchanged at Google's STS,
//!   then optionally for a service account's token.
//!
//! A [`GoogleTokenSource`] yields the bearer token for one request, cached
//! and renewed ahead of its expiry ([`crate::cloud_credentials`]): the
//! gateway's own, or a tenant's service account's, which the gateway is
//! allowed to impersonate (`roles/iam.serviceAccountTokenCreator`).
//!
//! No token, key or response body reaches an error or the logs.

use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use reqwest::{Url, header};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::cloud_credentials::{
    CredentialsError, Expiring, Fetch, Refreshing, client, form, https_or_loopback, read_body,
};

/// The scope of the gateway's Google tokens: the Gemini API accepts it.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const IAM_CREDENTIALS: &str = "https://iamcredentials.googleapis.com/";
/// The metadata server, by address: no DNS lookup to spoof.
const METADATA_HOST: &str = "169.254.169.254";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
/// Lifetime of service account tokens the gateway asks for.
const TOKEN_SECONDS: i64 = 3600;
/// The most a credentials file, or a federated subject token file, may
/// hold.
const MAX_CREDENTIALS_FILE: u64 = 64 * 1024;

/// The gateway's own Google identity. Clones share its tokens.
#[derive(Clone)]
pub struct GoogleIdentity {
    inner: Arc<Identity>,
}

struct Identity {
    kind: Kind,
    client: reqwest::Client,
    /// The IAM Service Account Credentials API, which issues the tokens of
    /// service accounts the gateway impersonates.
    iam_credentials: Url,
    /// The quota project the credentials file names, if any.
    quota_project: Option<String>,
    /// Token sources made from this identity, so that an adapter built
    /// again reuses a token. An entry is dropped once no adapter uses it and
    /// its token has expired.
    tokens: std::sync::Mutex<HashMap<TokenKey, GoogleTokenSource>>,
}

enum Kind {
    ServiceAccount {
        client_email: String,
        key: Arc<ring::signature::RsaKeyPair>,
        token_uri: Url,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: Zeroizing<String>,
        refresh_token: Zeroizing<String>,
        token_uri: Url,
    },
    ExternalAccount {
        audience: String,
        subject_token_type: String,
        token_url: Url,
        source: SubjectSource,
        impersonation_url: Option<Url>,
    },
    Metadata {
        endpoint: Url,
    },
}

/// Where a federated identity's subject token comes from. Read on every
/// exchange: the issuer rotates it.
enum SubjectSource {
    File {
        path: String,
        json_field: Option<String>,
    },
    Url {
        url: Url,
        headers: Vec<(header::HeaderName, header::HeaderValue)>,
        json_field: Option<String>,
    },
}

impl Kind {
    fn describe(&self) -> &'static str {
        match self {
            Self::ServiceAccount { .. } => "service account key",
            Self::AuthorizedUser { .. } => "user credentials",
            Self::ExternalAccount { .. } => "workload identity federation",
            Self::Metadata { .. } => "metadata server",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum TokenKey {
    /// As the gateway.
    Own { scope: String },
    /// As the service account `email`, which the gateway impersonates.
    Impersonated { email: String, scope: String },
}

impl fmt::Debug for GoogleIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GoogleIdentity({})", self.inner.kind.describe())
    }
}

/// The fields of a credentials file this module reads.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "type")]
    kind: String,
    // service_account
    client_email: Option<String>,
    private_key: Option<Zeroizing<String>>,
    token_uri: Option<String>,
    // authorized_user
    client_id: Option<String>,
    client_secret: Option<Zeroizing<String>>,
    refresh_token: Option<Zeroizing<String>>,
    quota_project_id: Option<String>,
    // external_account
    audience: Option<String>,
    subject_token_type: Option<String>,
    token_url: Option<String>,
    service_account_impersonation_url: Option<String>,
    credential_source: Option<CredentialSource>,
}

#[derive(Deserialize)]
struct CredentialSource {
    file: Option<String>,
    url: Option<String>,
    headers: Option<HashMap<String, String>>,
    format: Option<SourceFormat>,
    /// AWS and executable sources are not supported.
    environment_id: Option<String>,
    executable: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SourceFormat {
    #[serde(rename = "type")]
    kind: String,
    subject_token_field_name: Option<String>,
}

/// An `https` URL, or `http` to loopback for a local stand-in.
fn endpoint(value: &str) -> Option<Url> {
    Url::parse(value).ok().filter(https_or_loopback)
}

impl GoogleIdentity {
    /// The identity Application Default Credentials name, from the
    /// environment `lookup` returns. `None` when the credentials file cannot
    /// be read or is of a kind not supported; with no file, the metadata
    /// server of the host.
    pub fn from_env(lookup: &impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let file = set("GOOGLE_APPLICATION_CREDENTIALS")
            .map(PathBuf::from)
            .or_else(|| {
                // gcloud's well-known file, if it exists.
                let dir = set("CLOUDSDK_CONFIG").map(PathBuf::from).or_else(|| {
                    set("HOME").map(|home| PathBuf::from(home).join(".config/gcloud"))
                })?;
                Some(dir.join("application_default_credentials.json")).filter(|path| path.is_file())
            });
        let iam_credentials = endpoint(
            &set("GOOGLE_IAM_CREDENTIALS_ENDPOINT").unwrap_or_else(|| IAM_CREDENTIALS.to_owned()),
        )?;
        let (kind, quota_project) = match file {
            Some(path) => Self::from_file(&path)?,
            None => {
                let host = set("GCE_METADATA_HOST").unwrap_or_else(|| METADATA_HOST.to_owned());
                // An IPv6 address (`fd20:ce::254` on IPv6-only nodes) goes in
                // brackets.
                let endpoint = Url::parse(&format!("http://{}/", crate::net::url_host(&host)))
                    .ok()
                    .filter(|url| url.path() == "/" && url.query().is_none())?;
                (Kind::Metadata { endpoint }, None)
            }
        };
        Some(Self {
            inner: Arc::new(Identity {
                kind,
                client: client(),
                iam_credentials,
                quota_project,
                tokens: std::sync::Mutex::new(HashMap::new()),
            }),
        })
    }

    fn from_file(path: &std::path::Path) -> Option<(Kind, Option<String>)> {
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.len() > MAX_CREDENTIALS_FILE {
            return None;
        }
        let text = Zeroizing::new(std::fs::read_to_string(path).ok()?);
        let file: CredentialsFile = serde_json::from_str(&text).ok()?;
        let token_uri = || endpoint(file.token_uri.as_deref().unwrap_or(DEFAULT_TOKEN_URI));
        let kind = match file.kind.as_str() {
            "service_account" => {
                let pem = file.private_key.as_ref()?;
                Kind::ServiceAccount {
                    client_email: file.client_email.clone()?,
                    key: Arc::new(rsa_key(pem)?),
                    token_uri: token_uri()?,
                }
            }
            "authorized_user" => Kind::AuthorizedUser {
                client_id: file.client_id.clone()?,
                client_secret: file.client_secret.clone()?,
                refresh_token: file.refresh_token.clone()?,
                token_uri: token_uri()?,
            },
            "external_account" => {
                let source = file.credential_source.as_ref()?;
                if source.environment_id.is_some() || source.executable.is_some() {
                    return None;
                }
                let json_field = match &source.format {
                    Some(format) if format.kind == "json" => {
                        Some(format.subject_token_field_name.clone()?)
                    }
                    Some(format) if format.kind != "text" => return None,
                    _ => None,
                };
                let source = match (&source.file, &source.url) {
                    (Some(path), None) => SubjectSource::File {
                        path: path.clone(),
                        json_field,
                    },
                    (None, Some(url)) => SubjectSource::Url {
                        // Often the host's own metadata endpoint, over http.
                        url: Url::parse(url)
                            .ok()
                            .filter(|url| matches!(url.scheme(), "http" | "https"))?,
                        headers: source
                            .headers
                            .iter()
                            .flatten()
                            .map(|(name, value)| {
                                Some((
                                    header::HeaderName::from_bytes(name.as_bytes()).ok()?,
                                    header::HeaderValue::from_str(value).ok()?,
                                ))
                            })
                            .collect::<Option<_>>()?,
                        json_field,
                    },
                    _ => return None,
                };
                Kind::ExternalAccount {
                    audience: file.audience.clone()?,
                    subject_token_type: file.subject_token_type.clone()?,
                    token_url: endpoint(file.token_url.as_deref()?)?,
                    source,
                    impersonation_url: match &file.service_account_impersonation_url {
                        Some(url) => Some(endpoint(url)?),
                        None => None,
                    },
                }
            }
            _ => return None,
        };
        Some((kind, file.quota_project_id.clone()))
    }

    /// The quota project the credentials file names, if any.
    pub fn quota_project(&self) -> Option<&str> {
        self.inner.quota_project.as_deref()
    }

    /// Tokens for `scope` as the gateway.
    pub fn token(&self, scope: &str) -> GoogleTokenSource {
        self.source(TokenKey::Own {
            scope: scope.to_owned(),
        })
    }

    /// Tokens for `scope` as the service account `email`, which the
    /// gateway impersonates. `None` for an address that is not a service
    /// account's.
    pub fn impersonate(&self, email: &str, scope: &str) -> Option<GoogleTokenSource> {
        service_account_email(email).then(|| {
            self.source(TokenKey::Impersonated {
                email: email.to_owned(),
                scope: scope.to_owned(),
            })
        })
    }

    fn source(&self, key: TokenKey) -> GoogleTokenSource {
        let mut tokens = self
            .inner
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.retain(|_, source| source.in_use());
        if let Some(source) = tokens.get(&key) {
            return source.clone();
        }
        // The token's source holds only a weak handle on the identity: the
        // map above would otherwise keep both alive forever.
        let source = GoogleTokenSource {
            inner: Arc::new(Refreshing::new(Token {
                identity: Arc::downgrade(&self.inner),
                key: key.clone(),
            })),
        };
        tokens.insert(key, source.clone());
        source
    }
}

/// `<name>@<project>.iam.gserviceaccount.com`, or a default service
/// account (`<number>-compute@developer.gserviceaccount.com`,
/// `<project>@appspot.gserviceaccount.com`): safe in a request path.
pub fn service_account_email(value: &str) -> bool {
    let Some((name, domain)) = value.split_once('@') else {
        return false;
    };
    let label = |text: &str| {
        !text.is_empty()
            && text.len() <= 63
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let project = domain.strip_suffix(".iam.gserviceaccount.com");
    value.len() <= 254
        && label(name)
        && match domain {
            "developer.gserviceaccount.com" | "appspot.gserviceaccount.com" => true,
            _ => project.is_some_and(label),
        }
}

/// The project of a service account `<name>@<project>.iam.gserviceaccount.com`;
/// `None` for a default service account, whose address names none.
pub fn service_account_project(email: &str) -> Option<&str> {
    email
        .split_once('@')?
        .1
        .strip_suffix(".iam.gserviceaccount.com")
}

/// The RSA key of a service account's PEM `private_key` (PKCS #8).
fn rsa_key(pem: &str) -> Option<ring::signature::RsaKeyPair> {
    let body: Zeroizing<String> = Zeroizing::new(
        pem.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("-----"))
            .collect(),
    );
    let der = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(body.as_bytes())
            .ok()?,
    );
    ring::signature::RsaKeyPair::from_pkcs8(&der).ok()
}

/// A cloneable handle to the bearer token for one scope and principal;
/// clones share one cache.
#[derive(Clone)]
pub struct GoogleTokenSource {
    inner: Arc<Refreshing<Zeroizing<String>>>,
}

impl fmt::Debug for GoogleTokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GoogleTokenSource({})", self.inner.describe())
    }
}

impl GoogleTokenSource {
    /// The bearer token to send now.
    pub async fn get(&self) -> Result<Zeroizing<String>, CredentialsError> {
        self.inner.get().await
    }

    /// The bearer token to send now, waited for at most `deadline`; a token
    /// that has not expired is still used if its renewal takes longer.
    pub async fn get_within(
        &self,
        deadline: std::time::Duration,
    ) -> Result<Zeroizing<String>, CredentialsError> {
        self.inner.get_within(Some(deadline)).await
    }

    /// Whether anything besides the identity's map uses this source, or its
    /// token is still valid.
    fn in_use(&self) -> bool {
        Arc::strong_count(&self.inner) > 1 || self.inner.valid()
    }
}

struct Token {
    identity: Weak<Identity>,
    key: TokenKey,
}

#[async_trait]
impl Fetch<Zeroizing<String>> for Token {
    async fn fetch(&self) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
        let identity = self
            .identity
            .upgrade()
            .ok_or(CredentialsError::Unavailable)?;
        match &self.key {
            TokenKey::Own { scope } => identity.own_token(scope).await,
            TokenKey::Impersonated { email, scope } => {
                let own = GoogleIdentity {
                    inner: identity.clone(),
                }
                .token(CLOUD_PLATFORM_SCOPE)
                .get()
                .await?;
                let url = identity
                    .iam_credentials
                    .join(&format!(
                        "v1/projects/-/serviceAccounts/{email}:generateAccessToken"
                    ))
                    .map_err(|_| CredentialsError::Unavailable)?;
                identity.generate_access_token(url, &own, scope).await
            }
        }
    }

    fn describe(&self) -> &'static str {
        match self.key {
            TokenKey::Own { .. } => "Google token",
            TokenKey::Impersonated { .. } => "Google token for a tenant service account",
        }
    }
}

impl Identity {
    async fn own_token(
        &self,
        scope: &str,
    ) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
        match &self.kind {
            Kind::ServiceAccount {
                client_email,
                key,
                token_uri,
            } => {
                let assertion = signed_jwt(key, client_email, scope, token_uri.as_str())?;
                self.token_request(
                    token_uri,
                    &[("grant_type", JWT_BEARER), ("assertion", &assertion)],
                )
                .await
            }
            // A user's refresh token carries the scopes it was granted.
            Kind::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
            } => {
                self.token_request(
                    token_uri,
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("refresh_token", refresh_token),
                    ],
                )
                .await
            }
            Kind::ExternalAccount {
                audience,
                subject_token_type,
                token_url,
                source,
                impersonation_url,
            } => {
                let subject = self.subject_token(source).await?;
                // A federated token is scoped to cloud-platform; impersonation
                // narrows it.
                let federated = self
                    .token_request(
                        token_url,
                        &[
                            ("grant_type", TOKEN_EXCHANGE),
                            ("audience", audience),
                            ("scope", CLOUD_PLATFORM_SCOPE),
                            ("requested_token_type", ACCESS_TOKEN_TYPE),
                            ("subject_token", &subject),
                            ("subject_token_type", subject_token_type),
                        ],
                    )
                    .await?;
                match impersonation_url {
                    Some(url) => {
                        self.generate_access_token(url.clone(), &federated.value, scope)
                            .await
                    }
                    None => Ok(federated),
                }
            }
            Kind::Metadata { endpoint } => {
                let mut url = endpoint
                    .join("computeMetadata/v1/instance/service-accounts/default/token")
                    .map_err(|_| CredentialsError::Unavailable)?;
                url.query_pairs_mut().append_pair("scopes", scope);
                let response = self
                    .client
                    .get(url)
                    .header("metadata-flavor", "Google")
                    .send()
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                parse_oauth(&read_body(response).await?)
            }
        }
    }

    async fn subject_token(
        &self,
        source: &SubjectSource,
    ) -> Result<Zeroizing<String>, CredentialsError> {
        let (raw, json_field) = match source {
            SubjectSource::File { path, json_field } => {
                use tokio::io::AsyncReadExt as _;
                // At most one byte past the limit, to tell an oversized
                // file from one exactly at it.
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                let mut raw = Zeroizing::new(Vec::new());
                file.take(MAX_CREDENTIALS_FILE + 1)
                    .read_to_end(&mut raw)
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                if raw.len() as u64 > MAX_CREDENTIALS_FILE {
                    return Err(CredentialsError::InvalidResponse);
                }
                (raw, json_field)
            }
            SubjectSource::Url {
                url,
                headers,
                json_field,
            } => {
                let mut request = self.client.get(url.clone());
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                (Zeroizing::new(read_body(response).await?), json_field)
            }
        };
        let token = match json_field {
            Some(field) => serde_json::from_slice::<serde_json::Value>(&raw)
                .ok()
                .and_then(|value| value.get(field)?.as_str().map(str::to_owned))
                .ok_or(CredentialsError::InvalidResponse)?,
            None => String::from_utf8(raw.to_vec())
                .map_err(|_| CredentialsError::InvalidResponse)?
                .trim()
                .to_owned(),
        };
        if token.is_empty() {
            return Err(CredentialsError::Unavailable);
        }
        Ok(Zeroizing::new(token))
    }

    async fn token_request(
        &self,
        url: &Url,
        pairs: &[(&str, &str)],
    ) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
        let response = self
            .client
            .post(url.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form(pairs))
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        parse_oauth(&read_body(response).await?)
    }

    /// A service account's token from the IAM Service Account Credentials
    /// API, authorized by `bearer`.
    async fn generate_access_token(
        &self,
        url: Url,
        bearer: &str,
        scope: &str,
    ) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {bearer}"))
            .map_err(|_| CredentialsError::Unavailable)?;
        authorization.set_sensitive(true);
        let response = self
            .client
            .post(url)
            .header(header::AUTHORIZATION, authorization)
            .json(&serde_json::json!({
                "scope": [scope],
                "lifetime": format!("{TOKEN_SECONDS}s"),
            }))
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        parse_generated(&read_body(response).await?)
    }
}

/// A JWT, signed with the service account's key, that asks `audience` for
/// a token for `scope`.
fn signed_jwt(
    key: &ring::signature::RsaKeyPair,
    issuer: &str,
    scope: &str,
    audience: &str,
) -> Result<Zeroizing<String>, CredentialsError> {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let now = Utc::now().timestamp();
    let header = encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = encode(
        serde_json::json!({
            "iss": issuer,
            "scope": scope,
            "aud": audience,
            "iat": now,
            "exp": now + TOKEN_SECONDS,
        })
        .to_string()
        .as_bytes(),
    );
    let message = format!("{header}.{claims}");
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| CredentialsError::Unavailable)?;
    Ok(Zeroizing::new(format!("{message}.{}", encode(&signature))))
}

#[derive(Deserialize)]
struct OAuthToken {
    access_token: Zeroizing<String>,
    expires_in: i64,
}

/// An OAuth token response.
fn parse_oauth(body: &[u8]) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
    let token = serde_json::from_slice::<OAuthToken>(body)
        .map_err(|_| CredentialsError::InvalidResponse)?;
    if token.access_token.is_empty() || token.expires_in <= 0 {
        return Err(CredentialsError::InvalidResponse);
    }
    Ok(Expiring {
        value: token.access_token,
        expires: Some(Utc::now() + chrono::Duration::seconds(token.expires_in)),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedToken {
    access_token: Zeroizing<String>,
    expire_time: DateTime<Utc>,
}

/// A `generateAccessToken` response.
fn parse_generated(body: &[u8]) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
    let token = serde_json::from_slice::<GeneratedToken>(body)
        .map_err(|_| CredentialsError::InvalidResponse)?;
    if token.access_token.is_empty() {
        return Err(CredentialsError::InvalidResponse);
    }
    Ok(Expiring {
        value: token.access_token,
        expires: Some(token.expire_time),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        Router,
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{get, post},
    };

    use super::*;

    /// A stand-in for Google's OAuth, STS, IAM Service Account Credentials
    /// and metadata endpoints. Tokens are `token-<n>`, numbered by request.
    #[derive(Clone, Default)]
    pub(crate) struct FakeGoogle {
        pub(crate) base: String,
        pub(crate) calls: Arc<AtomicUsize>,
        /// Path, authorization and body of every request.
        pub(crate) requests: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
        pub(crate) refuse: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeGoogle {
        fn record(&self, path: &str, headers: &HeaderMap, body: String) -> Option<usize> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let authorization = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            self.requests
                .lock()
                .unwrap()
                .push((path.to_owned(), authorization, body));
            (!self.refuse.load(Ordering::SeqCst)).then_some(n)
        }
    }

    async fn oauth(
        State(fake): State<FakeGoogle>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, String) {
        match fake.record("/token", &headers, body) {
            Some(n) => (
                StatusCode::OK,
                format!(
                    r#"{{"access_token":"token-{n}","expires_in":3599,"token_type":"Bearer"}}"#
                ),
            ),
            None => (
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant"}"#.into(),
            ),
        }
    }

    async fn sts(
        State(fake): State<FakeGoogle>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, String) {
        match fake.record("/v1/token", &headers, body) {
            Some(n) => (
                StatusCode::OK,
                format!(
                    r#"{{"access_token":"federated-{n}","expires_in":3600,"issued_token_type":"{ACCESS_TOKEN_TYPE}"}}"#
                ),
            ),
            None => (StatusCode::FORBIDDEN, String::new()),
        }
    }

    async fn generate(
        State(fake): State<FakeGoogle>,
        Path(name): Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, String) {
        match fake.record(
            &format!("/v1/projects/-/serviceAccounts/{name}"),
            &headers,
            body,
        ) {
            Some(n) => (
                StatusCode::OK,
                serde_json::json!({
                    "accessToken": format!("sa-{n}"),
                    "expireTime": (Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                })
                .to_string(),
            ),
            None => (
                StatusCode::FORBIDDEN,
                r#"{"error":{"status":"PERMISSION_DENIED"}}"#.into(),
            ),
        }
    }

    async fn metadata(
        State(fake): State<FakeGoogle>,
        headers: HeaderMap,
        Query(query): Query<HashMap<String, String>>,
    ) -> (StatusCode, String) {
        if headers.get("metadata-flavor").is_none_or(|v| v != "Google") {
            return (StatusCode::FORBIDDEN, String::new());
        }
        match fake.record("/metadata", &headers, query["scopes"].clone()) {
            Some(n) => (
                StatusCode::OK,
                format!(r#"{{"access_token":"md-{n}","expires_in":3599,"token_type":"Bearer"}}"#),
            ),
            None => (StatusCode::NOT_FOUND, String::new()),
        }
    }

    pub(crate) async fn start() -> FakeGoogle {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = FakeGoogle {
            base: format!("http://{}", listener.local_addr().unwrap()),
            ..FakeGoogle::default()
        };
        let app = Router::new()
            .route("/token", post(oauth))
            .route("/v1/token", post(sts))
            .route("/v1/projects/-/serviceAccounts/{name}", post(generate))
            .route(
                "/computeMetadata/v1/instance/service-accounts/default/token",
                get(metadata),
            )
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    /// A directory of its own for one test's files.
    pub(crate) struct TempDir(pub(crate) std::path::PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("gateway-gcp-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn write(&self, name: &str, contents: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path.display().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn identity(fake: &FakeGoogle, file: &str) -> GoogleIdentity {
        let file = file.to_owned();
        let iam = format!("{}/", fake.base);
        GoogleIdentity::from_env(&move |name: &str| match name {
            "GOOGLE_APPLICATION_CREDENTIALS" => Some(file.clone()),
            "GOOGLE_IAM_CREDENTIALS_ENDPOINT" => Some(iam.clone()),
            _ => None,
        })
        .unwrap()
    }

    /// User credentials, as `gcloud auth application-default login` writes
    /// them, against `fake`.
    pub(crate) fn user_credentials(fake: &FakeGoogle) -> String {
        serde_json::json!({
            "type": "authorized_user",
            "client_id": "client.apps.googleusercontent.com",
            "client_secret": "client-secret",
            "refresh_token": "refresh-token",
            "token_uri": format!("{}/token", fake.base),
            "quota_project_id": "quota-project",
        })
        .to_string()
    }

    /// The OIDC tests' RSA key, as a service account key file's PEM.
    fn test_pem() -> String {
        let der = include_bytes!("application/testdata/oidc-test-rsa.pk8");
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        format!(
            "-----BEGIN {kind}-----\n{}\n-----END {kind}-----\n",
            lines.join("\n"),
            kind = "PRIVATE KEY"
        )
    }

    fn form_of(body: &str) -> HashMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect()
    }

    #[tokio::test]
    async fn user_credentials_refresh_and_impersonate_a_tenant_service_account() {
        let fake = start().await;
        let dir = TempDir::new();
        let identity = identity(&fake, &dir.write("adc.json", &user_credentials(&fake)));
        assert_eq!(format!("{identity:?}"), "GoogleIdentity(user credentials)");
        assert_eq!(identity.quota_project(), Some("quota-project"));
        let own = identity.token(CLOUD_PLATFORM_SCOPE);
        assert_eq!(own.get().await.unwrap().as_str(), "token-1");
        let (_, _, body) = fake.requests.lock().unwrap()[0].clone();
        let form = form_of(&body);
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "refresh-token");

        let email = "tenant-gemini@tenant-project.iam.gserviceaccount.com";
        let tenant = identity.impersonate(email, CLOUD_PLATFORM_SCOPE).unwrap();
        assert_eq!(tenant.get().await.unwrap().as_str(), "sa-2");
        let (path, authorization, body) = fake.requests.lock().unwrap()[1].clone();
        assert_eq!(
            path,
            format!("/v1/projects/-/serviceAccounts/{email}:generateAccessToken")
        );
        // Authorized by the gateway's own token, which is cached.
        assert_eq!(authorization, "Bearer token-1");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["scope"][0], CLOUD_PLATFORM_SCOPE);
        assert_eq!(body["lifetime"], "3600s");
        // Shared with a source made again for the same account.
        assert_eq!(
            identity
                .impersonate(email, CLOUD_PLATFORM_SCOPE)
                .unwrap()
                .get()
                .await
                .unwrap()
                .as_str(),
            "sa-2"
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
        // Only service accounts, and nothing that could leave the path.
        for invalid in [
            "person@example.com",
            "x@evil.example/../a.iam.gserviceaccount.com",
            "a@b.iam.gserviceaccount.com:generateAccessToken?",
        ] {
            assert!(
                identity
                    .impersonate(invalid, CLOUD_PLATFORM_SCOPE)
                    .is_none(),
                "{invalid}"
            );
        }
    }

    #[tokio::test]
    async fn a_service_account_key_signs_its_own_assertion() {
        let fake = start().await;
        let dir = TempDir::new();
        let file = serde_json::json!({
            "type": "service_account",
            "client_email": "gateway@gateway-project.iam.gserviceaccount.com",
            "private_key": test_pem(),
            "token_uri": format!("{}/token", fake.base),
        });
        let identity = identity(&fake, &dir.write("key.json", &file.to_string()));
        assert_eq!(
            format!("{identity:?}"),
            "GoogleIdentity(service account key)"
        );
        let token = identity.token(CLOUD_PLATFORM_SCOPE).get().await.unwrap();
        assert_eq!(token.as_str(), "token-1");
        let (_, _, body) = fake.requests.lock().unwrap()[0].clone();
        let form = form_of(&body);
        assert_eq!(form["grant_type"], JWT_BEARER);
        let parts: Vec<&str> = form["assertion"].split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            claims["iss"],
            "gateway@gateway-project.iam.gserviceaccount.com"
        );
        assert_eq!(claims["scope"], CLOUD_PLATFORM_SCOPE);
        assert_eq!(claims["aud"], format!("{}/token", fake.base));
        // The signature verifies with the key's public half.
        let key = rsa_key(&test_pem()).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            ring::signature::KeyPair::public_key(&key).as_ref().to_vec(),
        );
        public
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(parts[2])
                    .unwrap(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn workload_identity_federation_exchanges_then_impersonates() {
        let fake = start().await;
        let dir = TempDir::new();
        let subject = dir.write("subject.json", r#"{"id_token":"external-oidc-jwt"}"#);
        let file = serde_json::json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/k8s",
            "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
            "token_url": format!("{}/v1/token", fake.base),
            "service_account_impersonation_url": format!(
                "{}/v1/projects/-/serviceAccounts/gateway@gateway-project.iam.gserviceaccount.com:generateAccessToken",
                fake.base
            ),
            "credential_source": {
                "file": subject,
                "format": {"type": "json", "subject_token_field_name": "id_token"}
            },
        });
        let identity = identity(&fake, &dir.write("wif.json", &file.to_string()));
        assert_eq!(
            format!("{identity:?}"),
            "GoogleIdentity(workload identity federation)"
        );
        let token = identity.token(CLOUD_PLATFORM_SCOPE).get().await.unwrap();
        assert_eq!(token.as_str(), "sa-2");
        let requests = fake.requests.lock().unwrap().clone();
        let exchange = form_of(&requests[0].2);
        assert_eq!(exchange["grant_type"], TOKEN_EXCHANGE);
        assert_eq!(exchange["subject_token"], "external-oidc-jwt");
        assert!(exchange["audience"].ends_with("/providers/k8s"));
        assert_eq!(requests[1].1, "Bearer federated-1");
    }

    #[tokio::test]
    async fn an_oversized_subject_token_file_is_refused() {
        let fake = start().await;
        let dir = TempDir::new();
        let subject = dir.write("subject", &"x".repeat(MAX_CREDENTIALS_FILE as usize + 1));
        let file = serde_json::json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/k8s",
            "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
            "token_url": format!("{}/v1/token", fake.base),
            "credential_source": {"file": subject},
        });
        let identity = identity(&fake, &dir.write("wif.json", &file.to_string()));
        assert_eq!(
            identity
                .token(CLOUD_PLATFORM_SCOPE)
                .get()
                .await
                .unwrap_err(),
            CredentialsError::InvalidResponse
        );
        // Nothing reached the token exchange.
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_metadata_server_and_refusals() {
        let fake = start().await;
        let host = fake.base.trim_start_matches("http://").to_owned();
        let identity = GoogleIdentity::from_env(&move |name: &str| match name {
            "GCE_METADATA_HOST" => Some(host.clone()),
            // No gcloud file to find.
            "HOME" => Some("/nonexistent".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(format!("{identity:?}"), "GoogleIdentity(metadata server)");
        assert_eq!(
            identity
                .token(CLOUD_PLATFORM_SCOPE)
                .get()
                .await
                .unwrap()
                .as_str(),
            "md-1"
        );
        assert_eq!(fake.requests.lock().unwrap()[0].2, CLOUD_PLATFORM_SCOPE);

        // A refused impersonation carries nothing of the response.
        fake.refuse.store(true, Ordering::SeqCst);
        let error = identity
            .impersonate("t-sa@t-proj.iam.gserviceaccount.com", "other-scope")
            .unwrap()
            .get()
            .await
            .unwrap_err();
        assert!(matches!(error, CredentialsError::Refused(_)));
        assert!(!error.to_string().contains("PERMISSION_DENIED"));
    }

    #[test]
    fn unusable_credentials_files_name_no_identity() {
        let dir = TempDir::new();
        let from = |contents: &str| {
            let file = dir.write("c.json", contents);
            GoogleIdentity::from_env(&move |name: &str| {
                (name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| file.clone())
            })
        };
        assert!(from("not json").is_none());
        assert!(from(r#"{"type":"impersonated_service_account"}"#).is_none());
        assert!(from(r#"{"type":"service_account","client_email":"a@b.iam.gserviceaccount.com","private_key":"not a key"}"#).is_none());
        // AWS and executable sources are not supported.
        assert!(from(r#"{"type":"external_account","audience":"a","subject_token_type":"t","token_url":"https://sts.googleapis.com/v1/token","credential_source":{"environment_id":"aws1","url":"http://169.254.169.254"}}"#).is_none());
        // Credentials never go to plain http off the host.
        assert!(from(r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r","token_uri":"http://evil.example/token"}"#).is_none());
        assert!(
            GoogleIdentity::from_env(&|name: &str| {
                (name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| "/nonexistent/c.json".to_owned())
            })
            .is_none()
        );
    }

    #[test]
    fn service_account_addresses() {
        for valid in [
            "gemini-tenant@tenant-project.iam.gserviceaccount.com",
            "123456789-compute@developer.gserviceaccount.com",
            "tenant-project@appspot.gserviceaccount.com",
        ] {
            assert!(service_account_email(valid), "{valid}");
        }
        for invalid in [
            "",
            "a@gmail.com",
            "UPPER@p.iam.gserviceaccount.com",
            "a@.iam.gserviceaccount.com",
            "a/b@p.iam.gserviceaccount.com",
            "a@p.iam.gserviceaccount.com.evil.example",
        ] {
            assert!(!service_account_email(invalid), "{invalid}");
        }
    }

    #[test]
    fn token_responses_parse() {
        assert!(parse_oauth(br#"{"access_token":"a","expires_in":3599}"#).is_ok());
        assert!(parse_oauth(br#"{"access_token":"","expires_in":3599}"#).is_err());
        assert!(parse_oauth(br#"{"access_token":"a"}"#).is_err());
        let generated =
            parse_generated(br#"{"accessToken":"a","expireTime":"2030-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(generated.expires.unwrap().timestamp(), 1_893_456_000);
    }
}
