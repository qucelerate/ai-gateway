//! Where the gateway's AWS credentials come from, and how they are renewed.
//!
//! An [`AwsCredentialSource`] yields credentials to sign one request with.
//! Static keys never change. Every other source returns temporary
//! credentials, cached and renewed ahead of their expiry
//! ([`crate::cloud_credentials`]):
//!
//! - the process's own runtime identity, found the way the AWS SDKs find it:
//!   web identity (EKS IRSA: `AWS_WEB_IDENTITY_TOKEN_FILE` and
//!   `AWS_ROLE_ARN`), then container credentials (ECS task roles and EKS Pod
//!   Identity: `AWS_CONTAINER_CREDENTIALS_FULL_URI` or `..._RELATIVE_URI`),
//!   then the EC2 instance profile through IMDSv2;
//! - an IAM role in a tenant's account, assumed through STS by another
//!   source with the tenant's external ID.
//!
//! No credential, token or response body reaches an error or the logs.

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

pub use crate::cloud_credentials::{CredentialsError, REFRESH_BEFORE};
use crate::{
    aws::{AwsCredentials, SignedRequest, amz_date_now, sign},
    cloud_credentials::{Expiring, Fetch, Refreshing, client, form, https_or_loopback, read_body},
};

/// Lifetime requested for an assumed role's session.
const SESSION_SECONDS: u32 = 3600;
const IMDS_ENDPOINT: &str = "http://169.254.169.254";
/// IMDS on its IPv6 address, for `AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE=IPv6`
/// (Nitro instances in IPv6-only subnets), as the AWS SDKs do.
const IMDS_ENDPOINT_IPV6: &str = "http://[fd00:ec2::254]";

/// A cloneable handle to a source of AWS credentials; clones share one
/// cache.
#[derive(Clone)]
pub struct AwsCredentialSource {
    inner: Arc<Cached>,
}

struct Cached {
    credentials: Refreshing<AwsCredentials>,
    /// Roles assumed with this source, so that an adapter built again for
    /// the same role reuses its session. An entry is dropped once no adapter
    /// uses it and its session has expired.
    assumed: std::sync::Mutex<HashMap<RoleKey, AwsCredentialSource>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RoleKey {
    role_arn: String,
    external_id: Option<String>,
    session_name: String,
    region: String,
    sts: Url,
}

impl fmt::Debug for AwsCredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AwsCredentialSource({})",
            self.inner.credentials.describe()
        )
    }
}

impl AwsCredentialSource {
    fn new(fetch: impl Fetch<AwsCredentials> + 'static) -> Self {
        Self {
            inner: Arc::new(Cached {
                credentials: Refreshing::new(fetch),
                assumed: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Keys that never change.
    pub fn fixed(credentials: AwsCredentials) -> Self {
        Self::new(Fixed(credentials))
    }

    /// The credentials to sign a request with now.
    pub async fn get(&self) -> Result<AwsCredentials, CredentialsError> {
        self.inner.credentials.get().await
    }

    /// The credentials to sign a request with now, waited for at most
    /// `deadline`; see [`Refreshing::get_within`].
    pub async fn get_within(
        &self,
        deadline: std::time::Duration,
    ) -> Result<AwsCredentials, CredentialsError> {
        self.inner.credentials.get_within(Some(deadline)).await
    }

    /// The process's runtime identity, from the AWS environment variables
    /// `lookup` returns, in the order the AWS SDKs use. Web identity uses
    /// STS in `AWS_REGION`, else `region`. `None` when the environment names
    /// no identity and IMDS is disabled.
    pub fn runtime(lookup: &impl Fn(&str) -> Option<String>, region: &str) -> Option<Self> {
        let set = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        if let (Some(token_file), Some(role_arn)) =
            (set("AWS_WEB_IDENTITY_TOKEN_FILE"), set("AWS_ROLE_ARN"))
        {
            let region = set("AWS_REGION")
                .or_else(|| set("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|| region.to_owned());
            return Some(Self::new(WebIdentity {
                client: client(),
                sts: sts_endpoint(&region, set("AWS_STS_ENDPOINT").as_deref())?,
                token_file,
                role_arn,
                session_name: set("AWS_ROLE_SESSION_NAME")
                    .unwrap_or_else(|| "qucelerate-gateway".to_owned()),
            }));
        }
        let container = match (
            set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
            set("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
        ) {
            // A path on the ECS agent: nothing that could name another host.
            (Some(relative), _) => Url::parse(&format!("http://169.254.170.2{relative}"))
                .ok()
                .filter(|url| relative.starts_with('/') && url.host_str() == Some("169.254.170.2")),
            (None, Some(full)) => Url::parse(&full).ok().filter(container_endpoint_allowed),
            (None, None) => None,
        };
        if let Some(url) = container {
            return Some(Self::new(Container {
                client: client(),
                url,
                token: set("AWS_CONTAINER_AUTHORIZATION_TOKEN").map(Zeroizing::new),
                token_file: set("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"),
            }));
        }
        if set("AWS_EC2_METADATA_DISABLED").is_some_and(|value| value.eq_ignore_ascii_case("true"))
        {
            return None;
        }
        let endpoint = imds_endpoint(
            set("AWS_EC2_METADATA_SERVICE_ENDPOINT"),
            set("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE"),
        );
        Some(Self::new(Imds {
            client: client(),
            endpoint: Url::parse(&endpoint).ok()?,
        }))
    }

    /// `role_arn` in another account, assumed by this source through STS in
    /// `region` (or at `sts_override`), with the account's external ID.
    pub fn assume_role(
        &self,
        role_arn: String,
        external_id: Option<String>,
        session_name: String,
        region: &str,
        sts_override: Option<&str>,
    ) -> Option<Self> {
        let key = RoleKey {
            role_arn,
            external_id,
            session_name,
            region: region.to_owned(),
            sts: sts_endpoint(region, sts_override)?,
        };
        let mut assumed = self
            .inner
            .assumed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assumed.retain(|_, source| source.in_use());
        if let Some(source) = assumed.get(&key) {
            return Some(source.clone());
        }
        // The role's source holds only a weak handle on this one: the
        // map above would otherwise keep both alive forever.
        let source = Self::new(AssumeRole {
            client: client(),
            source: Arc::downgrade(&self.inner),
            sts: key.sts.clone(),
            region: key.region.clone(),
            role_arn: key.role_arn.clone(),
            external_id: key.external_id.clone(),
            session_name: key.session_name.clone(),
        });
        assumed.insert(key, source.clone());
        Some(source)
    }
}

impl AwsCredentialSource {
    /// Whether anything besides the map holding it uses this source, or its
    /// credentials are still valid. A source busy renewing counts as valid.
    fn in_use(&self) -> bool {
        Arc::strong_count(&self.inner) > 1 || self.inner.credentials.valid()
    }
}

/// The regional STS endpoint in the region's partition, or `explicit`
/// (https, or http to loopback).
fn sts_endpoint(region: &str, explicit: Option<&str>) -> Option<Url> {
    match explicit {
        Some(url) => {
            let url = Url::parse(url).ok()?;
            https_or_loopback(&url).then_some(url)
        }
        None => region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            .then(|| {
                let suffix = if region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                Url::parse(&format!("https://sts.{region}.{suffix}/")).ok()
            })?,
    }
}

/// IMDS's address: the one configured, else IPv4's or, in `IPv6` mode, IPv6's.
fn imds_endpoint(endpoint: Option<String>, mode: Option<String>) -> String {
    endpoint.unwrap_or_else(|| match mode {
        Some(mode) if mode.trim().eq_ignore_ascii_case("ipv6") => IMDS_ENDPOINT_IPV6.to_owned(),
        _ => IMDS_ENDPOINT.to_owned(),
    })
}

/// The hosts the AWS SDKs accept for a full container credentials URI:
/// https anywhere, or http to loopback or the ECS/EKS agent addresses.
fn container_endpoint_allowed(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => matches!(
            url.host_str(),
            Some(
                "localhost"
                    | "127.0.0.1"
                    | "[::1]"
                    | "169.254.170.2"
                    | "169.254.170.23"
                    | "[fd00:ec2::23]"
            )
        ),
        _ => false,
    }
}

struct Fixed(AwsCredentials);

#[async_trait]
impl Fetch<AwsCredentials> for Fixed {
    async fn fetch(&self) -> Result<Expiring<AwsCredentials>, CredentialsError> {
        Ok(Expiring {
            value: self.0.clone(),
            expires: None,
        })
    }

    fn describe(&self) -> &'static str {
        "static keys"
    }
}

/// The JSON shape of container and instance credentials.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JsonCredentials {
    access_key_id: String,
    secret_access_key: Zeroizing<String>,
    token: Option<Zeroizing<String>>,
    expiration: Option<DateTime<Utc>>,
}

impl From<JsonCredentials> for Expiring<AwsCredentials> {
    fn from(json: JsonCredentials) -> Self {
        Self {
            value: AwsCredentials {
                access_key_id: json.access_key_id,
                secret_access_key: json.secret_access_key,
                session_token: json.token,
            },
            expires: json.expiration,
        }
    }
}

fn parse_json(body: &[u8]) -> Result<Expiring<AwsCredentials>, CredentialsError> {
    serde_json::from_slice::<JsonCredentials>(body)
        .map(Into::into)
        .map_err(|_| CredentialsError::InvalidResponse)
}

struct Container {
    client: reqwest::Client,
    url: Url,
    token: Option<Zeroizing<String>>,
    /// Read on every fetch: EKS Pod Identity rotates the token.
    token_file: Option<String>,
}

#[async_trait]
impl Fetch<AwsCredentials> for Container {
    async fn fetch(&self) -> Result<Expiring<AwsCredentials>, CredentialsError> {
        let token = match (&self.token_file, &self.token) {
            (Some(file), _) => Some(Zeroizing::new(
                tokio::fs::read_to_string(file)
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?
                    .trim()
                    .to_owned(),
            )),
            (None, token) => token.clone(),
        };
        let mut request = self.client.get(self.url.clone());
        if let Some(token) = token {
            let mut value =
                header::HeaderValue::from_str(&token).map_err(|_| CredentialsError::Unavailable)?;
            value.set_sensitive(true);
            request = request.header(header::AUTHORIZATION, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        parse_json(&read_body(response).await?)
    }

    fn describe(&self) -> &'static str {
        "container credentials"
    }
}

struct Imds {
    client: reqwest::Client,
    endpoint: Url,
}

#[async_trait]
impl Fetch<AwsCredentials> for Imds {
    async fn fetch(&self) -> Result<Expiring<AwsCredentials>, CredentialsError> {
        let url = |path: &str| {
            self.endpoint
                .join(path)
                .map_err(|_| CredentialsError::Unavailable)
        };
        // IMDSv2: a session token first; v1 is never used.
        let token = self
            .client
            .put(url("/latest/api/token")?)
            .header("x-aws-ec2-metadata-token-ttl-seconds", "300")
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        let token = String::from_utf8(read_body(token).await?)
            .map_err(|_| CredentialsError::InvalidResponse)?;
        let mut token = header::HeaderValue::from_str(token.trim())
            .map_err(|_| CredentialsError::InvalidResponse)?;
        token.set_sensitive(true);
        let get = |path: String| {
            let token = token.clone();
            async move {
                let response = self
                    .client
                    .get(url(&path)?)
                    .header("x-aws-ec2-metadata-token", token)
                    .send()
                    .await
                    .map_err(|_| CredentialsError::Unavailable)?;
                read_body(response).await
            }
        };
        let roles =
            String::from_utf8(get("/latest/meta-data/iam/security-credentials/".into()).await?)
                .map_err(|_| CredentialsError::InvalidResponse)?;
        let role = roles
            .lines()
            .next()
            .filter(|role| !role.is_empty() && !role.contains(['/', '?', '#']))
            .ok_or(CredentialsError::Unavailable)?;
        parse_json(&get(format!("/latest/meta-data/iam/security-credentials/{role}")).await?)
    }

    fn describe(&self) -> &'static str {
        "EC2 instance profile"
    }
}

/// The text of `<tag>` in an STS XML response, unescaped.
fn xml_text(body: &str, tag: &str) -> Option<String> {
    let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + body[start..].find(&format!("</{tag}>"))?;
    Some(
        body[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// The credentials in an STS `AssumeRole*` response.
fn parse_sts(body: &[u8]) -> Result<Expiring<AwsCredentials>, CredentialsError> {
    let body = std::str::from_utf8(body).map_err(|_| CredentialsError::InvalidResponse)?;
    let field = |tag| xml_text(body, tag).ok_or(CredentialsError::InvalidResponse);
    let expires = DateTime::parse_from_rfc3339(&field("Expiration")?)
        .map_err(|_| CredentialsError::InvalidResponse)?
        .with_timezone(&Utc);
    Ok(Expiring {
        value: AwsCredentials {
            access_key_id: field("AccessKeyId")?,
            secret_access_key: Zeroizing::new(field("SecretAccessKey")?),
            session_token: Some(Zeroizing::new(field("SessionToken")?)),
        },
        expires: Some(expires),
    })
}

const FORM: &str = "application/x-www-form-urlencoded; charset=utf-8";

struct WebIdentity {
    client: reqwest::Client,
    sts: Url,
    token_file: String,
    role_arn: String,
    session_name: String,
}

#[async_trait]
impl Fetch<AwsCredentials> for WebIdentity {
    async fn fetch(&self) -> Result<Expiring<AwsCredentials>, CredentialsError> {
        // Read on every fetch: the kubelet rotates the token.
        let token = Zeroizing::new(
            tokio::fs::read_to_string(&self.token_file)
                .await
                .map_err(|_| CredentialsError::Unavailable)?,
        );
        let body = form(&[
            ("Action", "AssumeRoleWithWebIdentity"),
            ("Version", "2011-06-15"),
            ("RoleArn", &self.role_arn),
            ("RoleSessionName", &self.session_name),
            ("WebIdentityToken", token.trim()),
        ]);
        // Unsigned: the token is the authentication.
        let response = self
            .client
            .post(self.sts.clone())
            .header(header::CONTENT_TYPE, FORM)
            .body(body)
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        parse_sts(&read_body(response).await?)
    }

    fn describe(&self) -> &'static str {
        "web identity"
    }
}

struct AssumeRole {
    client: reqwest::Client,
    source: Weak<Cached>,
    sts: Url,
    region: String,
    role_arn: String,
    external_id: Option<String>,
    session_name: String,
}

#[async_trait]
impl Fetch<AwsCredentials> for AssumeRole {
    async fn fetch(&self) -> Result<Expiring<AwsCredentials>, CredentialsError> {
        let source = AwsCredentialSource {
            inner: self.source.upgrade().ok_or(CredentialsError::Unavailable)?,
        };
        let credentials = source.get().await?;
        let duration = SESSION_SECONDS.to_string();
        let mut pairs = vec![
            ("Action", "AssumeRole"),
            ("Version", "2011-06-15"),
            ("RoleArn", self.role_arn.as_str()),
            ("RoleSessionName", self.session_name.as_str()),
            ("DurationSeconds", duration.as_str()),
        ];
        if let Some(external_id) = &self.external_id {
            pairs.push(("ExternalId", external_id));
        }
        let body = form(&pairs);
        let headers = sign(
            &credentials,
            &self.region,
            "sts",
            &SignedRequest {
                method: "POST",
                url: &self.sts,
                headers: &[("content-type", FORM)],
                body: body.as_bytes(),
            },
            &amz_date_now(),
        )
        .map_err(|_| CredentialsError::Unavailable)?;
        let mut request = self
            .client
            .post(self.sts.clone())
            .header(header::CONTENT_TYPE, FORM)
            .body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| CredentialsError::Unavailable)?;
        parse_sts(&read_body(response).await?)
    }

    fn describe(&self) -> &'static str {
        "assumed role"
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::{get, post, put},
    };

    use super::*;

    /// A fake STS (AssumeRole and AssumeRoleWithWebIdentity), container
    /// endpoint and IMDS, issuing credentials that expire after `lifetime`.
    #[derive(Clone)]
    pub(crate) struct FakeAws {
        pub(crate) base: String,
        pub(crate) calls: Arc<AtomicUsize>,
        pub(crate) last: Arc<std::sync::Mutex<Option<(HeaderMap, String)>>>,
        pub(crate) lifetime: Arc<std::sync::Mutex<chrono::Duration>>,
        pub(crate) refuse: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeAws {
        fn expiry(&self) -> DateTime<Utc> {
            Utc::now() + *self.lifetime.lock().unwrap()
        }

        fn record(&self, headers: &HeaderMap, body: String) -> Option<StatusCode> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some((headers.clone(), body));
            let _ = n;
            self.refuse
                .load(Ordering::SeqCst)
                .then_some(StatusCode::FORBIDDEN)
        }
    }

    async fn sts(
        State(fake): State<FakeAws>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, String) {
        if let Some(status) = fake.record(&headers, body.clone()) {
            return (status, "<Error><Code>AccessDenied</Code></Error>".into());
        }
        let n = fake.calls.load(Ordering::SeqCst);
        let action = if body.contains("Action=AssumeRoleWithWebIdentity") {
            "AssumeRoleWithWebIdentity"
        } else {
            "AssumeRole"
        };
        (
            StatusCode::OK,
            format!(
                "<{action}Response><{action}Result><Credentials>\
                 <AccessKeyId>ASIA{n}</AccessKeyId>\
                 <SecretAccessKey>secret&amp;{n}</SecretAccessKey>\
                 <SessionToken>token-{n}</SessionToken>\
                 <Expiration>{}</Expiration>\
                 </Credentials></{action}Result></{action}Response>",
                fake.expiry().to_rfc3339()
            ),
        )
    }

    fn json(fake: &FakeAws) -> String {
        let n = fake.calls.load(Ordering::SeqCst);
        serde_json::json!({
            "AccessKeyId": format!("ASIA{n}"), "SecretAccessKey": format!("secret{n}"),
            "Token": format!("token-{n}"), "Expiration": fake.expiry().to_rfc3339(),
        })
        .to_string()
    }

    async fn container(State(fake): State<FakeAws>, headers: HeaderMap) -> (StatusCode, String) {
        if let Some(status) = fake.record(&headers, String::new()) {
            return (status, String::new());
        }
        (StatusCode::OK, json(&fake))
    }

    async fn imds_token(State(fake): State<FakeAws>, headers: HeaderMap) -> (StatusCode, String) {
        let ttl = headers.contains_key("x-aws-ec2-metadata-token-ttl-seconds");
        let _ = fake;
        if ttl {
            (StatusCode::OK, "imds-token".into())
        } else {
            (StatusCode::BAD_REQUEST, String::new())
        }
    }

    async fn imds_roles(headers: HeaderMap) -> (StatusCode, String) {
        if headers
            .get("x-aws-ec2-metadata-token")
            .is_some_and(|v| v == "imds-token")
        {
            (StatusCode::OK, "gateway-role\n".into())
        } else {
            (StatusCode::UNAUTHORIZED, String::new())
        }
    }

    async fn imds_role(State(fake): State<FakeAws>, headers: HeaderMap) -> (StatusCode, String) {
        if headers
            .get("x-aws-ec2-metadata-token")
            .is_none_or(|v| v != "imds-token")
        {
            return (StatusCode::UNAUTHORIZED, String::new());
        }
        fake.record(&headers, String::new());
        (StatusCode::OK, json(&fake))
    }

    pub(crate) async fn start() -> FakeAws {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = FakeAws {
            base: format!("http://{}", listener.local_addr().unwrap()),
            calls: Arc::default(),
            last: Arc::default(),
            lifetime: Arc::new(std::sync::Mutex::new(chrono::Duration::hours(1))),
            refuse: Arc::default(),
        };
        let app = Router::new()
            .route("/sts/", post(sts))
            .route("/creds", get(container))
            .route("/latest/api/token", put(imds_token))
            .route(
                "/latest/meta-data/iam/security-credentials/",
                get(imds_roles),
            )
            .route(
                "/latest/meta-data/iam/security-credentials/gateway-role",
                get(imds_role),
            )
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    fn lookup(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn keys(id: &str) -> AwsCredentials {
        AwsCredentials {
            access_key_id: id.into(),
            secret_access_key: Zeroizing::new("base-secret".into()),
            session_token: None,
        }
    }

    #[tokio::test]
    async fn an_assumed_role_is_cached_and_renewed_before_it_expires() {
        let fake = start().await;
        let base = AwsCredentialSource::fixed(keys("AKIDBASE"));
        let role = base
            .assume_role(
                "arn:aws:iam::123456789012:role/tenant".into(),
                Some("ext-123".into()),
                "qucelerate-org".into(),
                "us-east-1",
                Some(&format!("{}/sts/", fake.base)),
            )
            .unwrap();
        let first = role.get().await.unwrap();
        assert!(first.access_key_id.starts_with("ASIA"));
        assert_eq!(first.secret_access_key.as_str(), "secret&1");
        assert_eq!(
            first.session_token.as_deref().map(String::as_str),
            Some("token-1")
        );
        // The request is signed by the base identity for STS.
        let (headers, body) = fake.last.lock().unwrap().clone().unwrap();
        let authorization = headers["authorization"].to_str().unwrap();
        assert!(authorization.contains("Credential=AKIDBASE/"));
        assert!(authorization.contains("/us-east-1/sts/aws4_request"));
        for field in [
            "Action=AssumeRole",
            "RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Ftenant",
            "ExternalId=ext-123",
            "RoleSessionName=qucelerate-org",
            "DurationSeconds=3600",
        ] {
            assert!(body.contains(field), "{field} in {body}");
        }
        // Cached while fresh, shared by clones.
        let clone = role.clone();
        assert_eq!(
            clone.get().await.unwrap().access_key_id,
            first.access_key_id
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        // An adapter built again for the same role, after the last one was
        // dropped, reuses its still-valid session.
        drop((role, clone));
        let again = base
            .assume_role(
                "arn:aws:iam::123456789012:role/tenant".into(),
                Some("ext-123".into()),
                "qucelerate-org".into(),
                "us-east-1",
                Some(&format!("{}/sts/", fake.base)),
            )
            .unwrap();
        assert_eq!(
            again.get().await.unwrap().access_key_id,
            first.access_key_id
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

        // Renewed once inside the refresh window.
        *fake.lifetime.lock().unwrap() = chrono::Duration::minutes(2);
        let fake_short = base
            .assume_role(
                "arn:aws:iam::123456789012:role/tenant".into(),
                None,
                "qucelerate-org".into(),
                "us-east-1",
                Some(&format!("{}/sts/", fake.base)),
            )
            .unwrap();
        let a = fake_short.get().await.unwrap();
        let b = fake_short.get().await.unwrap();
        assert_ne!(
            a.access_key_id, b.access_key_id,
            "renewed: within 5 minutes of expiry"
        );
        assert!(
            !fake
                .last
                .lock()
                .unwrap()
                .clone()
                .unwrap()
                .1
                .contains("ExternalId")
        );

        // A failed renewal keeps credentials that have not expired yet...
        fake.refuse.store(true, Ordering::SeqCst);
        assert_eq!(
            fake_short.get().await.unwrap().access_key_id,
            b.access_key_id
        );
        // ...but a source with nothing valid reports the refusal.
        let fresh = base
            .assume_role(
                "arn:aws:iam::123456789012:role/tenant".into(),
                None,
                "s".into(),
                "us-east-1",
                Some(&format!("{}/sts/", fake.base)),
            )
            .unwrap();
        assert_eq!(
            fresh.get().await.unwrap_err(),
            CredentialsError::Refused(403)
        );
    }

    #[tokio::test]
    async fn the_runtime_identity_is_found_like_the_aws_sdks() {
        let fake = start().await;
        let dir = std::env::temp_dir().join(format!("gateway-aws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "web-identity-jwt\n").unwrap();

        // Web identity (IRSA): unsigned AssumeRoleWithWebIdentity.
        let web = AwsCredentialSource::runtime(
            &lookup(vec![
                (
                    "AWS_WEB_IDENTITY_TOKEN_FILE",
                    token_file.display().to_string(),
                ),
                (
                    "AWS_ROLE_ARN",
                    "arn:aws:iam::123456789012:role/gateway".into(),
                ),
                ("AWS_STS_ENDPOINT", format!("{}/sts/", fake.base)),
            ]),
            "us-east-1",
        )
        .unwrap();
        assert!(web.get().await.unwrap().session_token.is_some());
        let (headers, body) = fake.last.lock().unwrap().clone().unwrap();
        assert!(headers.get("authorization").is_none());
        assert!(body.contains("Action=AssumeRoleWithWebIdentity"));
        assert!(body.contains("WebIdentityToken=web-identity-jwt"));

        // Container credentials, with the token read from its file.
        std::fs::write(&token_file, "pod-identity-token").unwrap();
        let container = AwsCredentialSource::runtime(
            &lookup(vec![
                (
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                    format!("{}/creds", fake.base),
                ),
                (
                    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
                    token_file.display().to_string(),
                ),
            ]),
            "us-east-1",
        )
        .unwrap();
        let credentials = container.get().await.unwrap();
        assert_eq!(
            credentials
                .session_token
                .as_deref()
                .map(String::as_str)
                .map(|t| t.starts_with("token-")),
            Some(true)
        );
        let (headers, _) = fake.last.lock().unwrap().clone().unwrap();
        assert_eq!(headers["authorization"], "pod-identity-token");
        // A relative URI cannot name another host.
        for relative in ["@evil.example/creds", ":80@evil.example/", ".evil.example/"] {
            assert!(
                AwsCredentialSource::runtime(
                    &lookup(vec![
                        ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", relative.into()),
                        ("AWS_EC2_METADATA_DISABLED", "true".into()),
                    ]),
                    "us-east-1"
                )
                .is_none(),
                "{relative}"
            );
        }
        // A full URI to an arbitrary http host is not accepted.
        assert!(
            AwsCredentialSource::runtime(
                &lookup(vec![
                    (
                        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                        "http://evil.example/creds".into()
                    ),
                    ("AWS_EC2_METADATA_DISABLED", "true".into()),
                ]),
                "us-east-1"
            )
            .is_none()
        );

        // IMDSv2, token first.
        let imds = AwsCredentialSource::runtime(
            &lookup(vec![(
                "AWS_EC2_METADATA_SERVICE_ENDPOINT",
                fake.base.clone(),
            )]),
            "us-east-1",
        )
        .unwrap();
        assert!(imds.get().await.unwrap().access_key_id.starts_with("ASIA"));
        assert!(
            AwsCredentialSource::runtime(
                &lookup(vec![("AWS_EC2_METADATA_DISABLED", "true".into())]),
                "us-east-1"
            )
            .is_none()
        );
        let debug = format!("{web:?} {container:?} {imds:?}");
        assert_eq!(
            debug,
            "AwsCredentialSource(web identity) AwsCredentialSource(container credentials) AwsCredentialSource(EC2 instance profile)"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imds_is_reached_over_ipv6_in_ipv6_mode() {
        assert_eq!(imds_endpoint(None, None), "http://169.254.169.254");
        assert_eq!(
            imds_endpoint(None, Some("IPv4".into())),
            "http://169.254.169.254"
        );
        assert_eq!(
            imds_endpoint(None, Some("IPv6".into())),
            "http://[fd00:ec2::254]"
        );
        assert_eq!(
            imds_endpoint(Some("http://10.0.0.1".into()), Some("IPv6".into())),
            "http://10.0.0.1"
        );
        assert!(Url::parse(IMDS_ENDPOINT_IPV6).is_ok());
        assert!(container_endpoint_allowed(
            &Url::parse("http://[fd00:ec2::23]/v1").unwrap()
        ));
    }

    #[test]
    fn sts_responses_parse_and_errors_carry_nothing() {
        let parsed = parse_sts(
            b"<R><AccessKeyId>ASIA1</AccessKeyId><SecretAccessKey>a&amp;b&lt;c</SecretAccessKey>\
              <SessionToken>t</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></R>",
        )
        .unwrap();
        assert_eq!(parsed.value.secret_access_key.as_str(), "a&b<c");
        assert!(parse_sts(b"<Error>denied secret-ish text</Error>").is_err());
        assert_eq!(
            CredentialsError::Refused(403).to_string(),
            "the credential service refused the request (HTTP 403)"
        );
    }

    #[test]
    fn sts_is_regional_in_the_region_partition() {
        assert_eq!(
            sts_endpoint("eu-west-1", None).unwrap().as_str(),
            "https://sts.eu-west-1.amazonaws.com/"
        );
        assert_eq!(
            sts_endpoint("cn-north-1", None).unwrap().as_str(),
            "https://sts.cn-north-1.amazonaws.com.cn/"
        );
        assert!(sts_endpoint("us-east-1/../x", None).is_none());
        assert!(sts_endpoint("us-east-1", Some("http://sts.example.com/")).is_none());
        assert_eq!(
            sts_endpoint("us-east-1", Some("http://127.0.0.1:9/sts/"))
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9/sts/"
        );
        // Web identity without AWS_REGION uses the Bedrock region.
        let web = AwsCredentialSource::runtime(
            &lookup(vec![
                ("AWS_WEB_IDENTITY_TOKEN_FILE", "/var/run/token".into()),
                ("AWS_ROLE_ARN", "arn:aws:iam::123456789012:role/gw".into()),
            ]),
            "eu-west-1",
        );
        assert!(web.is_some());
    }
}
