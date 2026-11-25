//! Microsoft Entra ID tokens for Azure OpenAI and Azure AI Foundry.
//!
//! An [`AzureIdentity`] is the gateway's own Entra identity, found the way
//! the Azure SDKs find it: a client secret (`AZURE_TENANT_ID`,
//! `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`), then workload identity
//! federation (AKS: `AZURE_FEDERATED_TOKEN_FILE` instead of the secret), then
//! a managed identity (App Service and Container Apps: `IDENTITY_ENDPOINT`
//! and `IDENTITY_HEADER`; VMs and AKS node identities: IMDS, with
//! `AZURE_CLIENT_ID` naming a user-assigned identity).
//!
//! An [`AzureTokenSource`] yields the bearer token for one request, cached
//! and renewed ahead of its expiry ([`crate::cloud_credentials`]):
//!
//! - in the gateway's own tenant;
//! - in a tenant's directory as the gateway's own (multi-tenant) app, which
//!   the tenant has consented to;
//! - as an app in the tenant's directory that trusts the gateway through a
//!   federated identity credential: the gateway presents its federated
//!   token, or its managed identity's token for `api://AzureADTokenExchange`.
//!
//! No token, secret or response body reaches an error or the logs.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Url, header};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::cloud_credentials::{
    CredentialsError, Expiring, Fetch, Refreshing, client, form, https_or_loopback, read_body,
};

/// The scope of Azure OpenAI and Azure AI Foundry inference in the public
/// cloud; [`AzureIdentity::cognitive_services_scope`] is the identity's.
pub const COGNITIVE_SERVICES_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
/// The audience of a managed identity token used as a federated assertion.
const TOKEN_EXCHANGE_SCOPE: &str = "api://AzureADTokenExchange/.default";
const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com/";
const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// The gateway's own Entra identity. Clones share its tokens.
#[derive(Clone)]
pub struct AzureIdentity {
    inner: Arc<Identity>,
}

struct Identity {
    kind: Kind,
    client: reqwest::Client,
    authority: Url,
    /// Token sources made from this identity, so that an adapter built
    /// again reuses a token. An entry is dropped once no adapter uses it
    /// and its token has expired.
    tokens: std::sync::Mutex<HashMap<TokenKey, AzureTokenSource>>,
}

enum Kind {
    ClientSecret {
        tenant_id: String,
        client_id: String,
        secret: Zeroizing<String>,
    },
    WorkloadIdentity {
        tenant_id: String,
        client_id: String,
        token_file: String,
    },
    AppService {
        endpoint: Url,
        header: Zeroizing<String>,
        client_id: Option<String>,
    },
    Imds {
        endpoint: Url,
        client_id: Option<String>,
    },
}

impl Kind {
    fn describe(&self) -> &'static str {
        match self {
            Self::ClientSecret { .. } => "client secret",
            Self::WorkloadIdentity { .. } => "workload identity",
            Self::AppService { .. } => "App Service managed identity",
            Self::Imds { .. } => "managed identity",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum TokenKey {
    /// As the gateway, in its own tenant or in `tenant_id`.
    Own {
        tenant_id: Option<String>,
        scope: String,
    },
    /// As the tenant's app `client_id`, which trusts the gateway.
    Federated {
        tenant_id: String,
        client_id: String,
        scope: String,
    },
}

impl fmt::Debug for AzureIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AzureIdentity({})", self.inner.kind.describe())
    }
}

/// A tenant or client identifier: safe in a token endpoint path.
fn directory_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

impl AzureIdentity {
    /// The identity named by the Azure environment variables `lookup`
    /// returns. `None` when they are incomplete or invalid; with none set,
    /// the managed identity of the host.
    pub fn from_env(lookup: &impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let authority = match set("AZURE_AUTHORITY_HOST") {
            Some(host) => Url::parse(&format!("{}/", host.trim_end_matches('/'))).ok()?,
            None => Url::parse(DEFAULT_AUTHORITY).ok()?,
        };
        if !https_or_loopback(&authority) {
            return None;
        }
        let app = || {
            let (tenant_id, client_id) = (set("AZURE_TENANT_ID")?, set("AZURE_CLIENT_ID")?);
            (directory_id(&tenant_id) && directory_id(&client_id)).then_some((tenant_id, client_id))
        };
        let kind = if let Some(secret) = set("AZURE_CLIENT_SECRET") {
            let (tenant_id, client_id) = app()?;
            Kind::ClientSecret {
                tenant_id,
                client_id,
                secret: Zeroizing::new(secret),
            }
        } else if let Some(token_file) = set("AZURE_FEDERATED_TOKEN_FILE") {
            let (tenant_id, client_id) = app()?;
            Kind::WorkloadIdentity {
                tenant_id,
                client_id,
                token_file,
            }
        } else if let (Some(endpoint), Some(header)) =
            (set("IDENTITY_ENDPOINT"), set("IDENTITY_HEADER"))
        {
            let endpoint = Url::parse(&endpoint).ok().filter(https_or_loopback)?;
            Kind::AppService {
                endpoint,
                header: Zeroizing::new(header),
                client_id: set("AZURE_CLIENT_ID"),
            }
        } else {
            let endpoint =
                Url::parse(&set("AZURE_IMDS_ENDPOINT").unwrap_or_else(|| IMDS_ENDPOINT.to_owned()))
                    .ok()
                    .filter(|url| {
                        https_or_loopback(url) || url.host_str() == Some("169.254.169.254")
                    })?;
            Kind::Imds {
                endpoint,
                client_id: set("AZURE_CLIENT_ID"),
            }
        };
        Some(Self {
            inner: Arc::new(Identity {
                kind,
                client: client(),
                authority,
                tokens: std::sync::Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The scope of Azure OpenAI and Azure AI Foundry in the cloud the
    /// identity signs in to: Azure Government and Azure operated by 21Vianet
    /// have their own.
    pub fn cognitive_services_scope(&self) -> &'static str {
        match self.inner.authority.host_str() {
            Some("login.microsoftonline.us") => "https://cognitiveservices.azure.us/.default",
            Some("login.chinacloudapi.cn" | "login.partner.microsoftonline.cn") => {
                "https://cognitiveservices.azure.cn/.default"
            }
            _ => COGNITIVE_SERVICES_SCOPE,
        }
    }

    /// Azure Resource Manager in the cloud the identity signs in to, which
    /// lists a resource's deployments (D-018).
    pub fn management_endpoint(&self) -> &'static str {
        match self.inner.authority.host_str() {
            Some("login.microsoftonline.us") => "https://management.usgovcloudapi.net",
            Some("login.chinacloudapi.cn" | "login.partner.microsoftonline.cn") => {
                "https://management.chinacloudapi.cn"
            }
            _ => "https://management.azure.com",
        }
    }

    /// The scope of tokens for [`Self::management_endpoint`].
    pub fn management_scope(&self) -> &'static str {
        match self.inner.authority.host_str() {
            Some("login.microsoftonline.us") => "https://management.usgovcloudapi.net/.default",
            Some("login.chinacloudapi.cn" | "login.partner.microsoftonline.cn") => {
                "https://management.chinacloudapi.cn/.default"
            }
            _ => "https://management.azure.com/.default",
        }
    }

    /// The storage endpoint suffix of the cloud the identity signs in to:
    /// Blob Storage is at `<account>.blob.<suffix>`.
    pub fn storage_suffix(&self) -> &'static str {
        match self.inner.authority.host_str() {
            Some("login.microsoftonline.us") => "core.usgovcloudapi.net",
            Some("login.chinacloudapi.cn" | "login.partner.microsoftonline.cn") => {
                "core.chinacloudapi.cn"
            }
            _ => "core.windows.net",
        }
    }

    /// Whether `endpoint` is in the cloud the identity signs in to, so that
    /// its tokens are for the endpoint's audience.
    pub fn serves(&self, endpoint: &str) -> bool {
        let cloud = self
            .cognitive_services_scope()
            .trim_start_matches("https://cognitiveservices.")
            .trim_end_matches("/.default");
        Url::parse(endpoint)
            .ok()
            .and_then(|url| {
                url.host_str()
                    .map(|host| host.ends_with(&format!(".{cloud}")))
            })
            .unwrap_or(false)
    }

    /// Tokens for `scope` in the gateway's own tenant.
    pub fn token(&self, scope: &str) -> AzureTokenSource {
        self.source(TokenKey::Own {
            tenant_id: None,
            scope: scope.to_owned(),
        })
    }

    /// Tokens for `scope` in the tenant's directory `tenant_id`: as the
    /// tenant's app `client_id` if given, else as the gateway's own app.
    /// `None` for an identity that cannot sign in elsewhere (a managed
    /// identity without a tenant app to trust it).
    pub fn in_tenant(
        &self,
        tenant_id: &str,
        client_id: Option<&str>,
        scope: &str,
    ) -> Option<AzureTokenSource> {
        if !directory_id(tenant_id) || client_id.is_some_and(|id| !directory_id(id)) {
            return None;
        }
        let key = match client_id {
            Some(client_id) => TokenKey::Federated {
                tenant_id: tenant_id.to_owned(),
                client_id: client_id.to_owned(),
                scope: scope.to_owned(),
            },
            None if matches!(
                self.inner.kind,
                Kind::ClientSecret { .. } | Kind::WorkloadIdentity { .. }
            ) =>
            {
                TokenKey::Own {
                    tenant_id: Some(tenant_id.to_owned()),
                    scope: scope.to_owned(),
                }
            }
            None => return None,
        };
        Some(self.source(key))
    }

    fn source(&self, key: TokenKey) -> AzureTokenSource {
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
        let source = AzureTokenSource {
            inner: Arc::new(Refreshing::new(Token {
                identity: Arc::downgrade(&self.inner),
                key: key.clone(),
            })),
        };
        tokens.insert(key, source.clone());
        source
    }
}

/// A cloneable handle to the bearer token for one scope and directory;
/// clones share one cache.
#[derive(Clone)]
pub struct AzureTokenSource {
    inner: Arc<Refreshing<Zeroizing<String>>>,
}

impl fmt::Debug for AzureTokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AzureTokenSource({})", self.inner.describe())
    }
}

impl AzureTokenSource {
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
            TokenKey::Own { tenant_id, scope } => {
                identity.own_token(tenant_id.as_deref(), scope).await
            }
            TokenKey::Federated {
                tenant_id,
                client_id,
                scope,
            } => {
                let assertion = identity.assertion().await?;
                identity
                    .token_request(
                        tenant_id,
                        &[
                            ("grant_type", "client_credentials"),
                            ("client_id", client_id),
                            ("client_assertion_type", JWT_BEARER),
                            ("client_assertion", &assertion),
                            ("scope", scope),
                        ],
                    )
                    .await
            }
        }
    }

    fn describe(&self) -> &'static str {
        match self.key {
            TokenKey::Own {
                tenant_id: None, ..
            } => "Entra token",
            TokenKey::Own { .. } => "Entra token in a tenant directory",
            TokenKey::Federated { .. } => "Entra token for a tenant app",
        }
    }
}

impl Identity {
    /// A token as the gateway itself, in its tenant or in `tenant_id`.
    async fn own_token(
        self: &Arc<Self>,
        tenant_id: Option<&str>,
        scope: &str,
    ) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
        match &self.kind {
            Kind::ClientSecret {
                tenant_id: home,
                client_id,
                secret,
            } => {
                self.token_request(
                    tenant_id.unwrap_or(home),
                    &[
                        ("grant_type", "client_credentials"),
                        ("client_id", client_id),
                        ("client_secret", secret),
                        ("scope", scope),
                    ],
                )
                .await
            }
            Kind::WorkloadIdentity {
                tenant_id: home,
                client_id,
                ..
            } => {
                let assertion = self.assertion().await?;
                self.token_request(
                    tenant_id.unwrap_or(home),
                    &[
                        ("grant_type", "client_credentials"),
                        ("client_id", client_id),
                        ("client_assertion_type", JWT_BEARER),
                        ("client_assertion", &assertion),
                        ("scope", scope),
                    ],
                )
                .await
            }
            // A managed identity has no other tenant.
            Kind::AppService { .. } | Kind::Imds { .. } if tenant_id.is_some() => {
                Err(CredentialsError::Unavailable)
            }
            Kind::AppService {
                endpoint,
                header,
                client_id,
            } => {
                let mut url = endpoint.clone();
                url.query_pairs_mut()
                    .append_pair("api-version", "2019-08-01")
                    .append_pair("resource", resource(scope));
                if let Some(client_id) = client_id {
                    url.query_pairs_mut().append_pair("client_id", client_id);
                }
                let mut value = header::HeaderValue::from_str(header)
                    .map_err(|_| CredentialsError::Unavailable)?;
                value.set_sensitive(true);
                let response = self
                    .client
                    .get(url)
                    .header("x-identity-header", value)
                    .send()
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                parse_token(&read_body(response).await?)
            }
            Kind::Imds {
                endpoint,
                client_id,
            } => {
                let mut url = endpoint.clone();
                url.query_pairs_mut()
                    .append_pair("api-version", "2018-02-01")
                    .append_pair("resource", resource(scope));
                if let Some(client_id) = client_id {
                    url.query_pairs_mut().append_pair("client_id", client_id);
                }
                let response = self
                    .client
                    .get(url)
                    .header("metadata", "true")
                    .send()
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                parse_token(&read_body(response).await?)
            }
        }
    }

    /// What the gateway presents to an app that trusts it: its federated
    /// token, else its own token for `api://AzureADTokenExchange`.
    async fn assertion(self: &Arc<Self>) -> Result<Zeroizing<String>, CredentialsError> {
        match &self.kind {
            // Read on every use: the kubelet rotates the token.
            Kind::WorkloadIdentity { token_file, .. } => Ok(Zeroizing::new(
                tokio::fs::read_to_string(token_file)
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?
                    .trim()
                    .to_owned(),
            )),
            _ => {
                AzureIdentity {
                    inner: self.clone(),
                }
                .token(TOKEN_EXCHANGE_SCOPE)
                .get()
                .await
            }
        }
    }

    async fn token_request(
        &self,
        tenant_id: &str,
        pairs: &[(&str, &str)],
    ) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
        let url = self
            .authority
            .join(&format!("{tenant_id}/oauth2/v2.0/token"))
            .map_err(|_| CredentialsError::Unavailable)?;
        let response = self
            .client
            .post(url)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form(pairs))
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        parse_token(&read_body(response).await?)
    }
}

/// A managed identity takes the resource, not the scope.
fn resource(scope: &str) -> &str {
    scope.strip_suffix("/.default").unwrap_or(scope)
}

/// A number, or a number in a string (managed identity endpoints).
#[derive(Deserialize)]
#[serde(untagged)]
enum Seconds {
    Number(i64),
    Text(String),
}

impl Seconds {
    fn value(&self) -> Option<i64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(value) => value.parse().ok(),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Zeroizing<String>,
    expires_in: Option<Seconds>,
    expires_on: Option<Seconds>,
}

/// An Entra or managed identity token response: the expiry from
/// `expires_on` (Unix seconds) or `expires_in`.
fn parse_token(body: &[u8]) -> Result<Expiring<Zeroizing<String>>, CredentialsError> {
    let response = serde_json::from_slice::<TokenResponse>(body)
        .map_err(|_| CredentialsError::InvalidResponse)?;
    let expires = match (&response.expires_on, &response.expires_in) {
        (Some(on), _) => on.value().and_then(|on| DateTime::from_timestamp(on, 0)),
        (None, Some(within)) => within
            .value()
            .map(|within| Utc::now() + chrono::Duration::seconds(within)),
        (None, None) => None,
    }
    .ok_or(CredentialsError::InvalidResponse)?;
    if response.access_token.is_empty() {
        return Err(CredentialsError::InvalidResponse);
    }
    Ok(Expiring {
        value: response.access_token,
        expires: Some(expires),
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

    /// A stand-in for the Entra token endpoint and the managed identity
    /// endpoints. Tokens are `token-<n>`, numbered by request.
    #[derive(Clone, Default)]
    pub(crate) struct FakeEntra {
        pub(crate) base: String,
        pub(crate) calls: Arc<AtomicUsize>,
        /// Tenant and form of every token request.
        pub(crate) requests: Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
        pub(crate) refuse: Arc<std::sync::atomic::AtomicBool>,
        pub(crate) expires_in: Arc<std::sync::atomic::AtomicI64>,
    }

    async fn token(
        State(fake): State<FakeEntra>,
        Path(tenant): Path<String>,
        body: String,
    ) -> (StatusCode, String) {
        let n = fake.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        fake.requests.lock().unwrap().push((tenant, form));
        if fake.refuse.load(Ordering::SeqCst) {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_client"}"#.into(),
            );
        }
        (
            StatusCode::OK,
            serde_json::json!({
                "token_type": "Bearer",
                "access_token": format!("token-{n}"),
                "expires_in": fake.expires_in.load(Ordering::SeqCst),
            })
            .to_string(),
        )
    }

    async fn managed(
        State(fake): State<FakeEntra>,
        headers: HeaderMap,
        Query(query): Query<HashMap<String, String>>,
    ) -> (StatusCode, String) {
        let imds = headers.get("metadata").is_some_and(|v| v == "true");
        let app_service = headers
            .get("x-identity-header")
            .is_some_and(|v| v == "identity-secret");
        if !(imds || app_service) {
            return (StatusCode::BAD_REQUEST, String::new());
        }
        let n = fake.calls.fetch_add(1, Ordering::SeqCst) + 1;
        fake.requests
            .lock()
            .unwrap()
            .push(("managed".into(), query.clone()));
        let on = Utc::now().timestamp() + fake.expires_in.load(Ordering::SeqCst);
        (
            StatusCode::OK,
            serde_json::json!({
                "access_token": format!("mi-{n}"),
                "expires_on": on.to_string(),
                "resource": query.get("resource"),
            })
            .to_string(),
        )
    }

    pub(crate) async fn start() -> FakeEntra {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = FakeEntra {
            base: format!("http://{}", listener.local_addr().unwrap()),
            ..FakeEntra::default()
        };
        fake.expires_in.store(3600, Ordering::SeqCst);
        let app = Router::new()
            .route("/{tenant}/oauth2/v2.0/token", post(token))
            .route("/msi/token", get(managed))
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    pub(crate) fn lookup(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// An identity for tests that never use it: a managed identity at a
    /// closed port.
    pub(crate) fn unused_identity() -> AzureIdentity {
        AzureIdentity::from_env(&lookup(vec![(
            "AZURE_IMDS_ENDPOINT",
            "http://127.0.0.1:9/".into(),
        )]))
        .unwrap()
    }

    pub(crate) fn secret_identity(fake: &FakeEntra) -> AzureIdentity {
        AzureIdentity::from_env(&lookup(vec![
            ("AZURE_AUTHORITY_HOST", fake.base.clone()),
            ("AZURE_TENANT_ID", "gateway-tenant".into()),
            ("AZURE_CLIENT_ID", "gateway-app".into()),
            ("AZURE_CLIENT_SECRET", "gateway-secret".into()),
        ]))
        .unwrap()
    }

    #[tokio::test]
    async fn a_client_secret_signs_in_at_home_and_in_tenant_directories() {
        let fake = start().await;
        let identity = secret_identity(&fake);
        let home = identity.token(COGNITIVE_SERVICES_SCOPE);
        assert_eq!(home.get().await.unwrap().as_str(), "token-1");
        // Cached, and shared with a source made again for the same scope.
        assert_eq!(
            identity
                .token(COGNITIVE_SERVICES_SCOPE)
                .get()
                .await
                .unwrap()
                .as_str(),
            "token-1"
        );
        let tenant = identity
            .in_tenant("tenant-a", None, COGNITIVE_SERVICES_SCOPE)
            .unwrap();
        assert_eq!(tenant.get().await.unwrap().as_str(), "token-2");
        let requests = fake.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "gateway-tenant");
        assert_eq!(requests[1].0, "tenant-a");
        for (_, form) in &requests {
            assert_eq!(form["grant_type"], "client_credentials");
            assert_eq!(form["client_id"], "gateway-app");
            assert_eq!(form["client_secret"], "gateway-secret");
            assert_eq!(form["scope"], COGNITIVE_SERVICES_SCOPE);
        }
        assert_eq!(
            format!("{identity:?} {tenant:?}"),
            "AzureIdentity(client secret) AzureTokenSource(Entra token in a tenant directory)"
        );
        // Unsafe directory ids are refused before any request.
        assert!(
            identity
                .in_tenant("../evil", None, COGNITIVE_SERVICES_SCOPE)
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_tenant_app_trusts_the_gateway_federated_token() {
        let fake = start().await;
        let dir = std::env::temp_dir().join(format!("gateway-azure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "kubernetes-sa-jwt\n").unwrap();
        let identity = AzureIdentity::from_env(&lookup(vec![
            ("AZURE_AUTHORITY_HOST", fake.base.clone()),
            ("AZURE_TENANT_ID", "gateway-tenant".into()),
            ("AZURE_CLIENT_ID", "gateway-app".into()),
            (
                "AZURE_FEDERATED_TOKEN_FILE",
                token_file.display().to_string(),
            ),
        ]))
        .unwrap();
        let token = identity
            .in_tenant("tenant-a", Some("tenant-app"), COGNITIVE_SERVICES_SCOPE)
            .unwrap();
        assert_eq!(token.get().await.unwrap().as_str(), "token-1");
        let (tenant, form) = fake.requests.lock().unwrap()[0].clone();
        assert_eq!(tenant, "tenant-a");
        assert_eq!(form["client_id"], "tenant-app");
        assert_eq!(form["client_assertion_type"], JWT_BEARER);
        assert_eq!(form["client_assertion"], "kubernetes-sa-jwt");
        assert!(!form.contains_key("client_secret"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_managed_identity_federates_through_the_token_exchange_audience() {
        let fake = start().await;
        let identity = AzureIdentity::from_env(&lookup(vec![
            ("AZURE_AUTHORITY_HOST", fake.base.clone()),
            ("AZURE_IMDS_ENDPOINT", format!("{}/msi/token", fake.base)),
            ("AZURE_CLIENT_ID", "user-assigned".into()),
        ]))
        .unwrap();
        assert_eq!(format!("{identity:?}"), "AzureIdentity(managed identity)");
        assert_eq!(
            identity
                .token(COGNITIVE_SERVICES_SCOPE)
                .get()
                .await
                .unwrap()
                .as_str(),
            "mi-1"
        );
        // Not as itself in another directory...
        assert!(
            identity
                .in_tenant("tenant-a", None, COGNITIVE_SERVICES_SCOPE)
                .is_none()
        );
        // ...but through a tenant app that trusts it.
        let token = identity
            .in_tenant("tenant-a", Some("tenant-app"), COGNITIVE_SERVICES_SCOPE)
            .unwrap();
        assert_eq!(token.get().await.unwrap().as_str(), "token-3");
        let requests = fake.requests.lock().unwrap().clone();
        assert_eq!(
            requests[0].1["resource"],
            "https://cognitiveservices.azure.com"
        );
        assert_eq!(requests[0].1["client_id"], "user-assigned");
        assert_eq!(requests[1].1["resource"], "api://AzureADTokenExchange");
        assert_eq!(requests[2].1["client_assertion"], "mi-2");

        // App Service: the identity header, never IMDS's.
        let app_service = AzureIdentity::from_env(&lookup(vec![
            ("IDENTITY_ENDPOINT", format!("{}/msi/token", fake.base)),
            ("IDENTITY_HEADER", "identity-secret".into()),
        ]))
        .unwrap();
        assert_eq!(
            app_service
                .token(COGNITIVE_SERVICES_SCOPE)
                .get()
                .await
                .unwrap()
                .as_str(),
            "mi-4"
        );
    }

    #[tokio::test]
    async fn tokens_renew_before_expiry_and_refusals_carry_nothing() {
        let fake = start().await;
        fake.expires_in.store(120, Ordering::SeqCst);
        let identity = secret_identity(&fake);
        let token = identity.token(COGNITIVE_SERVICES_SCOPE);
        assert_eq!(token.get().await.unwrap().as_str(), "token-1");
        assert_eq!(token.get().await.unwrap().as_str(), "token-2");
        // A failed renewal keeps a token that has not expired.
        fake.refuse.store(true, Ordering::SeqCst);
        assert_eq!(token.get().await.unwrap().as_str(), "token-2");
        let other = identity
            .in_tenant("tenant-b", None, COGNITIVE_SERVICES_SCOPE)
            .unwrap();
        let error = other.get().await.unwrap_err();
        assert_eq!(error, CredentialsError::Refused(400));
        assert!(!error.to_string().contains("invalid_client"));
    }

    #[test]
    fn the_environment_names_one_identity() {
        // A secret without its tenant and app is not an identity.
        assert!(
            AzureIdentity::from_env(&lookup(vec![("AZURE_CLIENT_SECRET", "s".into())])).is_none()
        );
        assert!(
            AzureIdentity::from_env(&lookup(vec![
                ("AZURE_AUTHORITY_HOST", "http://login.example.com".into()),
                ("AZURE_TENANT_ID", "t".into()),
                ("AZURE_CLIENT_ID", "c".into()),
                ("AZURE_CLIENT_SECRET", "s".into()),
            ]))
            .is_none()
        );
        assert!(
            AzureIdentity::from_env(&lookup(vec![
                ("AZURE_TENANT_ID", "t/../x".into()),
                ("AZURE_CLIENT_ID", "c".into()),
                ("AZURE_CLIENT_SECRET", "s".into()),
            ]))
            .is_none()
        );
        assert!(
            AzureIdentity::from_env(&lookup(vec![
                ("IDENTITY_ENDPOINT", "http://evil.example/msi".into()),
                ("IDENTITY_HEADER", "h".into()),
            ]))
            .is_none()
        );
        // Nothing set: the host's managed identity.
        assert_eq!(
            format!("{:?}", AzureIdentity::from_env(&lookup(vec![])).unwrap()),
            "AzureIdentity(managed identity)"
        );
    }

    #[test]
    fn token_responses_parse_both_expiry_shapes() {
        let entra = parse_token(br#"{"access_token":"a","expires_in":3599}"#).unwrap();
        assert!(entra.expires.unwrap() > Utc::now() + chrono::Duration::minutes(59));
        let managed =
            parse_token(br#"{"access_token":"a","expires_on":"4102444800","expires_in":"3599"}"#)
                .unwrap();
        assert_eq!(managed.expires.unwrap().timestamp(), 4_102_444_800);
        assert!(parse_token(br#"{"access_token":"a"}"#).is_err());
        assert!(parse_token(br#"{"access_token":"","expires_in":1}"#).is_err());
    }
}
