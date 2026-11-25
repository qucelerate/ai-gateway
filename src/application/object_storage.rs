//! Writes usage export objects to an organization's own storage through its
//! stored cloud connection (D-014).
//!
//! The connection is one of the organization's provider credentials, used
//! exactly as inference uses it: an AWS role the gateway's AWS identity
//! assumes with the organization's external ID, an Entra app in the
//! tenant's directory, or a Google service account the gateway's Google
//! identity impersonates. Static keys are refused, so the gateway stores no
//! storage keys. A connection whose cloud principal is not approved for the
//! organization is refused as it is for inference.
//!
//! Object names hold only characters no service escapes (see
//! `domain::usage_export`), so each name is sent and signed as written.
//! Errors are fixed phrases without URLs, credentials or response bodies.

use std::{str::FromStr, time::Duration};

use async_trait::async_trait;
use reqwest::{StatusCode, header};
use sha2::{Digest, Sha256};
use url::Url;
use zeroize::Zeroizing;

use crate::{
    aws_credentials::AwsCredentialSource,
    azure_credentials::{AzureIdentity, AzureTokenSource},
    domain::{
        catalog::ProviderFamily,
        credentials::{
            CredentialBinding, CredentialCipher, ProviderSecret, aws_external_id,
            aws_role_session_name,
        },
        identity::OrganizationId,
        usage_export::{DestinationInput, ObjectWriter, StorageKind, WriteError},
    },
    gcp_credentials::{GoogleIdentity, GoogleTokenSource},
    persistence::provider_credentials::ProviderCredentialStore,
};

/// Azure Storage's Entra resource, the same in every cloud.
const AZURE_STORAGE_SCOPE: &str = "https://storage.azure.com/.default";
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const GCS_URL: &str = "https://storage.googleapis.com/";
/// Blob service version: bearer tokens need 2017-11-09 or later.
const AZURE_STORAGE_VERSION: &str = "2023-11-03";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CREDENTIALS_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!(
    "qucelerate-gateway-usage-export/",
    env!("CARGO_PKG_VERSION")
);

/// The gateway's own identities, which act for organizations' connections.
#[derive(Clone, Default)]
pub struct ExportIdentities {
    pub aws: Option<AwsIdentity>,
    pub azure: Option<AzureIdentity>,
    pub google: Option<GoogleIdentity>,
}

/// The gateway's AWS identity and the STS it assumes tenants' roles
/// through.
#[derive(Clone)]
pub struct AwsIdentity {
    pub credentials: AwsCredentialSource,
    pub sts_region: String,
    pub sts_endpoint: Option<String>,
}

/// Endpoints to use instead of each service's public one; for tests.
/// S3 and Blob Storage are then addressed path-style:
/// `<base>/<bucket>/<name>` and `<base>/<account>/<container>/<name>`.
#[derive(Clone, Debug, Default)]
pub struct StorageEndpoints {
    pub s3: Option<Url>,
    pub gcs: Option<Url>,
    pub azure_blob: Option<Url>,
}

/// A bearer token source; implemented by the Entra and Google sources.
#[async_trait]
pub trait BearerToken: Send + Sync {
    async fn token(&self) -> Result<Zeroizing<String>, WriteError>;
}

#[async_trait]
impl BearerToken for AzureTokenSource {
    async fn token(&self) -> Result<Zeroizing<String>, WriteError> {
        self.get_within(CREDENTIALS_TIMEOUT)
            .await
            .map_err(|_| WriteError::Credentials)
    }
}

#[async_trait]
impl BearerToken for GoogleTokenSource {
    async fn token(&self) -> Result<Zeroizing<String>, WriteError> {
        self.get_within(CREDENTIALS_TIMEOUT)
            .await
            .map_err(|_| WriteError::Credentials)
    }
}

/// How to authenticate one write.
pub enum Access {
    Aws(AwsCredentialSource),
    Google(Box<dyn BearerToken>),
    /// With the cloud's storage endpoint suffix.
    Azure(Box<dyn BearerToken>, &'static str),
}

#[derive(Clone)]
pub struct CloudObjectWriter {
    store: ProviderCredentialStore,
    cipher: Option<CredentialCipher>,
    identities: ExportIdentities,
    endpoints: StorageEndpoints,
    client: reqwest::Client,
}

impl CloudObjectWriter {
    pub fn new(
        store: ProviderCredentialStore,
        cipher: Option<CredentialCipher>,
        identities: ExportIdentities,
    ) -> Self {
        Self::with_endpoints(store, cipher, identities, StorageEndpoints::default())
    }

    #[doc(hidden)]
    pub fn with_endpoints(
        store: ProviderCredentialStore,
        cipher: Option<CredentialCipher>,
        identities: ExportIdentities,
        endpoints: StorageEndpoints,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // S3 answers a bucket in another region with a redirect; it
            // is reported, not followed.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            store,
            cipher,
            identities,
            endpoints,
            client,
        }
    }

    /// How the organization's connection authenticates to the destination.
    async fn access(
        &self,
        organization: OrganizationId,
        destination: &DestinationInput,
    ) -> Result<Access, WriteError> {
        let key = destination.connection.as_str();
        let stored = self
            .store
            .sealed(organization, key)
            .await
            .map_err(|_| WriteError::ConnectionUnreadable)?
            .ok_or(WriteError::ConnectionNotFound)?;
        if stored.principal_digest.is_some() && !stored.principal_approved {
            return Err(WriteError::PrincipalNotApproved);
        }
        let family = ProviderFamily::from_str(&stored.family)
            .map_err(|_| WriteError::ConnectionUnreadable)?;
        let cipher = self
            .cipher
            .as_ref()
            .ok_or(WriteError::ConnectionUnreadable)?;
        let secret = cipher
            .open(
                CredentialBinding {
                    organization_id: organization,
                    adapter_instance_key: key,
                    family,
                },
                &stored.key_id,
                &stored.ciphertext,
            )
            .map_err(|_| WriteError::ConnectionUnreadable)?;
        // The approval is for the digest the row records; it must be this
        // secret's.
        if secret.cloud_principal().map(|principal| principal.digest()) != stored.principal_digest {
            return Err(WriteError::PrincipalNotApproved);
        }
        access_for(&secret, destination.kind, organization, &self.identities)
    }

    /// Writes `body` as `name` with `access`.
    pub async fn put(
        &self,
        access: &Access,
        destination: &DestinationInput,
        name: &str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Result<(), WriteError> {
        let request = match access {
            Access::Aws(source) => {
                let credentials = source
                    .get_within(CREDENTIALS_TIMEOUT)
                    .await
                    .map_err(|_| WriteError::Credentials)?;
                let url = self.s3_url(destination, name)?;
                let region = destination.region.as_deref().ok_or(WriteError::Request)?;
                let digest = hex(&Sha256::digest(&body));
                let signed = crate::aws::sign(
                    &credentials,
                    region,
                    "s3",
                    &crate::aws::SignedRequest {
                        method: "PUT",
                        url: &url,
                        headers: &[
                            ("content-type", content_type),
                            ("x-amz-content-sha256", &digest),
                        ],
                        body: &body,
                    },
                    &crate::aws::amz_date_now(),
                )
                .map_err(|_| WriteError::Request)?;
                let mut request = self
                    .client
                    .put(url)
                    .header(header::CONTENT_TYPE, content_type)
                    .header("x-amz-content-sha256", digest);
                for (name, value) in signed {
                    request = request.header(name, value);
                }
                request
            }
            Access::Google(token) => {
                let token = token.token().await?;
                self.client
                    .post(self.gcs_url(destination, name)?)
                    .bearer_auth(token.as_str())
                    .header(header::CONTENT_TYPE, content_type)
            }
            Access::Azure(token, suffix) => {
                let token = token.token().await?;
                self.client
                    .put(self.blob_url(destination, name, suffix)?)
                    .bearer_auth(token.as_str())
                    .header(header::CONTENT_TYPE, content_type)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header("x-ms-version", AZURE_STORAGE_VERSION)
                    // Required on every authorized request, bearer included.
                    .header(
                        "x-ms-date",
                        chrono::Utc::now()
                            .format("%a, %d %b %Y %H:%M:%S GMT")
                            .to_string(),
                    )
            }
        };
        match request.body(body).send().await {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(status_error(response.status())),
            Err(error) if error.is_timeout() => Err(WriteError::Timeout),
            Err(error) if error.is_connect() => Err(WriteError::Connect),
            Err(_) => Err(WriteError::Request),
        }
    }

    /// Virtual-hosted, except for a bucket with dots, which no wildcard
    /// certificate covers.
    fn s3_url(&self, destination: &DestinationInput, name: &str) -> Result<Url, WriteError> {
        let bucket = &destination.bucket;
        let url = match (&self.endpoints.s3, destination.region.as_deref()) {
            (Some(base), _) => base.join(&format!("{bucket}/{name}")),
            (None, Some(region)) => {
                let domain = if region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                if bucket.contains('.') {
                    Url::parse(&format!("https://s3.{region}.{domain}/{bucket}/{name}"))
                } else {
                    Url::parse(&format!("https://{bucket}.s3.{region}.{domain}/{name}"))
                }
            }
            (None, None) => return Err(WriteError::Request),
        };
        url.map_err(|_| WriteError::Request)
    }

    fn gcs_url(&self, destination: &DestinationInput, name: &str) -> Result<Url, WriteError> {
        let base = match &self.endpoints.gcs {
            Some(base) => base.clone(),
            None => Url::parse(GCS_URL).map_err(|_| WriteError::Request)?,
        };
        let mut url = base
            .join(&format!("upload/storage/v1/b/{}/o", destination.bucket))
            .map_err(|_| WriteError::Request)?;
        url.query_pairs_mut()
            .append_pair("uploadType", "media")
            .append_pair("name", name);
        Ok(url)
    }

    fn blob_url(
        &self,
        destination: &DestinationInput,
        name: &str,
        suffix: &str,
    ) -> Result<Url, WriteError> {
        let account = destination.account.as_deref().ok_or(WriteError::Request)?;
        let container = &destination.bucket;
        match &self.endpoints.azure_blob {
            Some(base) => base.join(&format!("{account}/{container}/{name}")),
            None => Url::parse(&format!(
                "https://{account}.blob.{suffix}/{container}/{name}"
            )),
        }
        .map_err(|_| WriteError::Request)
    }
}

#[async_trait]
impl ObjectWriter for CloudObjectWriter {
    async fn write(
        &self,
        organization: OrganizationId,
        destination: &DestinationInput,
        name: &str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Result<(), WriteError> {
        let access = self.access(organization, destination).await?;
        self.put(&access, destination, name, content_type, body)
            .await
    }
}

/// The access `secret` gives to storage of `kind`: only a cloud identity
/// of the storage's own cloud qualifies.
fn access_for(
    secret: &ProviderSecret,
    kind: StorageKind,
    organization: OrganizationId,
    identities: &ExportIdentities,
) -> Result<Access, WriteError> {
    match (kind, secret) {
        (StorageKind::S3, ProviderSecret::AwsAssumeRole { role_arn }) => {
            let aws = identities.aws.as_ref().ok_or(WriteError::NoIdentity)?;
            aws.credentials
                .assume_role(
                    role_arn.clone(),
                    Some(aws_external_id(organization)),
                    aws_role_session_name(organization),
                    &aws.sts_region,
                    aws.sts_endpoint.as_deref(),
                )
                .map(Access::Aws)
                .ok_or(WriteError::NoIdentity)
        }
        (
            StorageKind::AzureBlob,
            ProviderSecret::AzureEntra {
                tenant_id,
                client_id,
                ..
            },
        ) => {
            let azure = identities.azure.as_ref().ok_or(WriteError::NoIdentity)?;
            let token = azure
                .in_tenant(tenant_id, client_id.as_deref(), AZURE_STORAGE_SCOPE)
                .ok_or(WriteError::NoIdentity)?;
            Ok(Access::Azure(Box::new(token), azure.storage_suffix()))
        }
        (
            StorageKind::Gcs,
            ProviderSecret::GoogleServiceAccount {
                service_account, ..
            },
        ) => {
            let google = identities.google.as_ref().ok_or(WriteError::NoIdentity)?;
            let token = google
                .impersonate(service_account, GCS_SCOPE)
                .ok_or(WriteError::NoIdentity)?;
            Ok(Access::Google(Box::new(token)))
        }
        _ => Err(WriteError::ConnectionKind),
    }
}

fn status_error(status: StatusCode) -> WriteError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => WriteError::Denied,
        StatusCode::NOT_FOUND => WriteError::NotFound,
        StatusCode::MOVED_PERMANENTLY | StatusCode::TEMPORARY_REDIRECT => WriteError::WrongRegion,
        _ => WriteError::Status,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Bytes,
        extract::{OriginalUri, State},
        http::{HeaderMap, Method, StatusCode as AxumStatus},
        routing::any,
    };
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[derive(Clone, Debug)]
    struct Seen {
        method: Method,
        uri: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct Fake {
        seen: Arc<Mutex<Vec<Seen>>>,
        status: Arc<Mutex<u16>>,
    }

    async fn record(
        State(fake): State<Fake>,
        method: Method,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> AxumStatus {
        fake.seen.lock().unwrap().push(Seen {
            method,
            uri: uri.to_string(),
            headers,
            body: body.to_vec(),
        });
        AxumStatus::from_u16(*fake.status.lock().unwrap()).unwrap()
    }

    async fn start(status: u16) -> (Fake, Url) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Fake::default();
        *fake.status.lock().unwrap() = status;
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().fallback(any(record)).with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, base)
    }

    fn writer(endpoints: StorageEndpoints) -> CloudObjectWriter {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://gateway_app:opaque-test-value@localhost/gateway")
            .unwrap();
        CloudObjectWriter::with_endpoints(
            ProviderCredentialStore::new(pool),
            None,
            ExportIdentities::default(),
            endpoints,
        )
    }

    struct Fixed;

    #[async_trait]
    impl BearerToken for Fixed {
        async fn token(&self) -> Result<Zeroizing<String>, WriteError> {
            Ok(Zeroizing::new("opaque-test-token".into()))
        }
    }

    fn destination(kind: StorageKind) -> DestinationInput {
        DestinationInput {
            kind,
            connection: "conn".into(),
            bucket: "usage-bucket".into(),
            account: (kind == StorageKind::AzureBlob).then(|| "usagestore".into()),
            region: (kind == StorageKind::S3).then(|| "eu-west-1".into()),
            prefix: "warehouse".into(),
        }
        .validate()
        .unwrap()
    }

    const NAME: &str = "warehouse/organization=0/date=2026-10-27/hour=09/1-2.ndjson.gz";

    #[tokio::test]
    async fn s3_writes_are_signed_with_the_payload_digest() {
        let (fake, base) = start(200).await;
        let writer = writer(StorageEndpoints {
            s3: Some(base),
            ..StorageEndpoints::default()
        });
        let access = Access::Aws(AwsCredentialSource::fixed(crate::aws::AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: Zeroizing::new("opaque-test-secret".into()),
            session_token: Some(Zeroizing::new("opaque-session".into())),
        }));
        let body = b"gzip bytes".to_vec();
        writer
            .put(
                &access,
                &destination(StorageKind::S3),
                NAME,
                "application/gzip",
                body.clone(),
            )
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(seen.method, Method::PUT);
        assert_eq!(seen.uri, format!("/usage-bucket/{NAME}"));
        assert_eq!(seen.body, body);
        let header = |name: &str| seen.headers.get(name).unwrap().to_str().unwrap().to_owned();
        assert_eq!(header("x-amz-content-sha256"), hex(&Sha256::digest(&body)));
        assert_eq!(header("content-type"), "application/gzip");
        assert_eq!(header("x-amz-security-token"), "opaque-session");
        let authorization = header("authorization");
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
        assert!(authorization.contains("/eu-west-1/s3/aws4_request"));
        assert!(authorization.contains(
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
        ));
    }

    #[tokio::test]
    async fn gcs_uploads_name_the_object_in_the_query() {
        let (fake, base) = start(200).await;
        let writer = writer(StorageEndpoints {
            gcs: Some(base),
            ..StorageEndpoints::default()
        });
        writer
            .put(
                &Access::Google(Box::new(Fixed)),
                &destination(StorageKind::Gcs),
                NAME,
                "application/gzip",
                b"x".to_vec(),
            )
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(seen.method, Method::POST);
        let url = Url::parse(&format!("http://host{}", seen.uri)).unwrap();
        assert_eq!(url.path(), "/upload/storage/v1/b/usage-bucket/o");
        let query: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            query,
            vec![
                ("uploadType".into(), "media".into()),
                ("name".into(), NAME.into())
            ]
        );
        assert_eq!(
            seen.headers.get("authorization").unwrap(),
            "Bearer opaque-test-token"
        );
    }

    #[tokio::test]
    async fn blob_writes_are_block_blobs() {
        let (fake, base) = start(201).await;
        let writer = writer(StorageEndpoints {
            azure_blob: Some(base),
            ..StorageEndpoints::default()
        });
        writer
            .put(
                &Access::Azure(Box::new(Fixed), "core.windows.net"),
                &destination(StorageKind::AzureBlob),
                NAME,
                "application/gzip",
                b"x".to_vec(),
            )
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(seen.method, Method::PUT);
        assert_eq!(seen.uri, format!("/usagestore/usage-bucket/{NAME}"));
        assert_eq!(seen.headers.get("x-ms-blob-type").unwrap(), "BlockBlob");
        assert_eq!(
            seen.headers.get("x-ms-version").unwrap(),
            AZURE_STORAGE_VERSION
        );
        let date = seen.headers.get("x-ms-date").unwrap().to_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc2822(date).is_ok(), "{date}");
        assert!(date.ends_with(" GMT"));
    }

    #[tokio::test]
    async fn refusals_map_to_fixed_errors() {
        for (status, expected) in [
            (403, WriteError::Denied),
            (404, WriteError::NotFound),
            (301, WriteError::WrongRegion),
            (500, WriteError::Status),
        ] {
            let (_fake, base) = start(status).await;
            let writer = writer(StorageEndpoints {
                gcs: Some(base),
                ..StorageEndpoints::default()
            });
            let result = writer
                .put(
                    &Access::Google(Box::new(Fixed)),
                    &destination(StorageKind::Gcs),
                    NAME,
                    "application/gzip",
                    Vec::new(),
                )
                .await;
            assert_eq!(result.err(), Some(expected), "{status}");
        }
    }

    #[tokio::test]
    async fn public_endpoints_follow_each_service() {
        let writer = writer(StorageEndpoints::default());
        let mut s3 = destination(StorageKind::S3);
        assert_eq!(
            writer.s3_url(&s3, "a/b").unwrap().as_str(),
            "https://usage-bucket.s3.eu-west-1.amazonaws.com/a/b"
        );
        s3.bucket = "usage.example.com".into();
        s3.region = Some("cn-north-1".into());
        assert_eq!(
            writer.s3_url(&s3, "a/b").unwrap().as_str(),
            "https://s3.cn-north-1.amazonaws.com.cn/usage.example.com/a/b"
        );
        assert_eq!(
            writer
                .blob_url(
                    &destination(StorageKind::AzureBlob),
                    "a",
                    "core.usgovcloudapi.net"
                )
                .unwrap()
                .as_str(),
            "https://usagestore.blob.core.usgovcloudapi.net/usage-bucket/a"
        );
        assert!(
            writer
                .gcs_url(&destination(StorageKind::Gcs), "a/b=c")
                .unwrap()
                .as_str()
                .starts_with("https://storage.googleapis.com/upload/storage/v1/b/usage-bucket/o?")
        );
    }

    #[test]
    fn only_a_cloud_identity_of_the_storage_cloud_is_accepted() {
        let organization = OrganizationId::new();
        let aws = ExportIdentities {
            aws: Some(AwsIdentity {
                credentials: AwsCredentialSource::fixed(crate::aws::AwsCredentials {
                    access_key_id: "AKIDEXAMPLE".into(),
                    secret_access_key: Zeroizing::new("opaque-test-secret".into()),
                    session_token: None,
                }),
                sts_region: "us-east-1".into(),
                sts_endpoint: None,
            }),
            ..ExportIdentities::default()
        };
        let role = ProviderSecret::AwsAssumeRole {
            role_arn: "arn:aws:iam::123456789012:role/usage-export".into(),
        };
        assert!(matches!(
            access_for(&role, StorageKind::S3, organization, &aws),
            Ok(Access::Aws(_))
        ));
        assert!(matches!(
            access_for(&role, StorageKind::Gcs, organization, &aws),
            Err(WriteError::ConnectionKind)
        ));
        assert!(matches!(
            access_for(
                &role,
                StorageKind::S3,
                organization,
                &ExportIdentities::default()
            ),
            Err(WriteError::NoIdentity)
        ));
        let keys = ProviderSecret::AwsSigV4 {
            access_key_id: Zeroizing::new("AKIDEXAMPLE".into()),
            secret_access_key: Zeroizing::new("opaque-test-secret".into()),
            session_token: None,
        };
        assert!(matches!(
            access_for(&keys, StorageKind::S3, organization, &aws),
            Err(WriteError::ConnectionKind)
        ));
    }
}
