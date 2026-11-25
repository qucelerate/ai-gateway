//! The gateway's own store for traced content: an S3 bucket, or a bucket of
//! an S3-compatible store (`GATEWAY_TRACE_STORE_*`). Objects are already
//! encrypted when they get here (see `domain::trace_content`).
//!
//! Object names are `{prefix}{organization}/{trace}`: UUIDs and the
//! configured prefix, characters no S3 implementation escapes, so each name
//! is sent and signed as written. Errors are fixed phrases without URLs,
//! credentials or response bodies.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{aws_credentials::AwsCredentialSource, config::TraceStoreConfig};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CREDENTIALS_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("qucelerate-gateway-traces/", env!("CARGO_PKG_VERSION"));
const CONTENT_TYPE: &str = "application/octet-stream";

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum TraceObjectError {
    #[error("trace store credentials are unavailable")]
    Credentials,
    #[error("trace store request could not be built")]
    Request,
    #[error("trace store did not answer")]
    Unreachable,
    #[error("trace store refused the request ({0})")]
    Status(u16),
}

/// Where traced content is kept.
#[async_trait]
pub trait TraceObjects: Send + Sync {
    async fn put(&self, name: &str, body: Vec<u8>) -> Result<(), TraceObjectError>;
    /// `None` when there is no such object.
    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, TraceObjectError>;
    /// Deleting an object that is not there succeeds.
    async fn delete(&self, name: &str) -> Result<(), TraceObjectError>;
}

/// An S3 bucket, or a bucket of an S3-compatible store.
pub struct S3TraceObjects {
    client: reqwest::Client,
    /// Everything before the object name, ending in `/`.
    base: Url,
    region: String,
    credentials: AwsCredentialSource,
}

impl S3TraceObjects {
    /// The store `config` names, signed with its own keys or else
    /// `runtime`. `None` when neither gives credentials.
    pub fn new(config: &TraceStoreConfig, runtime: Option<AwsCredentialSource>) -> Option<Self> {
        let credentials = match &config.credentials {
            Some(keys) => AwsCredentialSource::fixed(keys.clone()),
            None => runtime?,
        };
        let base = match &config.endpoint {
            // Path-style: `<endpoint>/<bucket>/<prefix><name>`.
            Some(endpoint) => {
                let mut base = endpoint.clone();
                let path = format!(
                    "{}/{}/{}",
                    base.path().trim_end_matches('/'),
                    config.bucket,
                    config.prefix
                );
                base.set_path(&path);
                base
            }
            None => Url::parse(&format!(
                "https://{}.s3.{}.amazonaws.com/{}",
                config.bucket, config.region, config.prefix
            ))
            .ok()?,
        };
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        Some(Self {
            client,
            base,
            region: config.region.clone(),
            credentials,
        })
    }

    fn url(&self, name: &str) -> Result<Url, TraceObjectError> {
        check_name(name)?;
        self.base.join(name).map_err(|_| TraceObjectError::Request)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        name: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, TraceObjectError> {
        let credentials = self
            .credentials
            .get_within(CREDENTIALS_TIMEOUT)
            .await
            .map_err(|_| TraceObjectError::Credentials)?;
        let url = self.url(name)?;
        let digest = hex(&Sha256::digest(&body));
        let mut headers = vec![("x-amz-content-sha256", digest.as_str())];
        if method == reqwest::Method::PUT {
            headers.push(("content-type", CONTENT_TYPE));
        }
        let signed = crate::aws::sign(
            &credentials,
            &self.region,
            "s3",
            &crate::aws::SignedRequest {
                method: method.as_str(),
                url: &url,
                headers: &headers,
                body: &body,
            },
            &crate::aws::amz_date_now(),
        )
        .map_err(|_| TraceObjectError::Request)?;
        let mut request = self.client.request(method, url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        for (name, value) in signed {
            request = request.header(name, value);
        }
        request
            .body(body)
            .send()
            .await
            .map_err(|_| TraceObjectError::Unreachable)
    }
}

#[async_trait]
impl TraceObjects for S3TraceObjects {
    async fn put(&self, name: &str, body: Vec<u8>) -> Result<(), TraceObjectError> {
        let response = self.send(reqwest::Method::PUT, name, body).await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(TraceObjectError::Status(response.status().as_u16()))
        }
    }

    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, TraceObjectError> {
        let response = self.send(reqwest::Method::GET, name, Vec::new()).await?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => response
                .bytes()
                .await
                .map(|bytes| Some(bytes.to_vec()))
                .map_err(|_| TraceObjectError::Unreachable),
            status => Err(TraceObjectError::Status(status.as_u16())),
        }
    }

    async fn delete(&self, name: &str) -> Result<(), TraceObjectError> {
        let response = self.send(reqwest::Method::DELETE, name, Vec::new()).await?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(()),
            status if status.is_success() => Ok(()),
            status => Err(TraceObjectError::Status(status.as_u16())),
        }
    }
}

/// The object names every store accepts: letters, digits, `-` and `/`.
fn check_name(name: &str) -> Result<(), TraceObjectError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'/'))
    {
        return Err(TraceObjectError::Request);
    }
    Ok(())
}

/// Objects kept in memory; for tests. Names are held to the bucket's rules,
/// so a test catches a name production would refuse.
#[doc(hidden)]
#[derive(Default)]
pub struct MemoryTraceObjects {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    /// When set, every call fails with it.
    pub failing: Mutex<Option<TraceObjectError>>,
}

impl MemoryTraceObjects {
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.lock().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn object(&self, name: &str) -> Option<Vec<u8>> {
        self.lock().get(name).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn check(&self, name: &str) -> Result<(), TraceObjectError> {
        check_name(name)?;
        match *self
            .failing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl TraceObjects for MemoryTraceObjects {
    async fn put(&self, name: &str, body: Vec<u8>) -> Result<(), TraceObjectError> {
        self.check(name)?;
        self.lock().insert(name.to_owned(), body);
        Ok(())
    }

    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, TraceObjectError> {
        self.check(name)?;
        Ok(self.lock().get(name).cloned())
    }

    async fn delete(&self, name: &str) -> Result<(), TraceObjectError> {
        self.check(name)?;
        self.lock().remove(name);
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::AwsCredentials;
    use axum::{Router, body::Bytes, extract::Path, http::HeaderMap, routing::any};
    use std::sync::Arc;

    fn config(endpoint: &str) -> TraceStoreConfig {
        TraceStoreConfig {
            bucket: "traces".into(),
            region: "us-east-1".into(),
            endpoint: Some(Url::parse(endpoint).unwrap()),
            prefix: "gw/".into(),
            credentials: Some(AwsCredentials {
                access_key_id: "AKIDEXAMPLE".into(),
                secret_access_key: zeroize::Zeroizing::new("secret".into()),
                session_token: None,
            }),
        }
    }

    #[test]
    fn names_go_under_the_bucket_and_prefix() {
        let path_style = S3TraceObjects::new(&config("http://store:8333/s3"), None).unwrap();
        assert_eq!(
            path_style.url("org/trace").unwrap().as_str(),
            "http://store:8333/s3/traces/gw/org/trace"
        );
        let mut aws = config("http://unused");
        aws.endpoint = None;
        let aws = S3TraceObjects::new(&aws, None).unwrap();
        assert_eq!(
            aws.url("org/trace").unwrap().as_str(),
            "https://traces.s3.us-east-1.amazonaws.com/gw/org/trace"
        );
        assert_eq!(aws.url("../x"), Err(TraceObjectError::Request));
        assert_eq!(aws.url(""), Err(TraceObjectError::Request));
        let mut keyless = config("http://unused");
        keyless.credentials = None;
        assert!(S3TraceObjects::new(&keyless, None).is_none());
    }

    #[tokio::test]
    async fn objects_are_written_read_and_deleted_signed() {
        let seen: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::default();
        let stored: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let app = Router::new().route(
            "/{*path}",
            any({
                let (seen, stored) = (seen.clone(), stored.clone());
                move |method: axum::http::Method,
                      Path(path): Path<String>,
                      headers: HeaderMap,
                      body: Bytes| {
                    let (seen, stored) = (seen.clone(), stored.clone());
                    async move {
                        let signed = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .is_some_and(|value| {
                                value.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
                            });
                        seen.lock()
                            .unwrap()
                            .push((method.to_string(), path.clone(), signed));
                        let mut stored = stored.lock().unwrap();
                        match method.as_str() {
                            "PUT" => {
                                stored.insert(path, body.to_vec());
                                (StatusCode::OK, Vec::new())
                            }
                            "GET" => match stored.get(&path) {
                                Some(body) => (StatusCode::OK, body.clone()),
                                None => (StatusCode::NOT_FOUND, Vec::new()),
                            },
                            "DELETE" => {
                                stored.remove(&path);
                                (StatusCode::NO_CONTENT, Vec::new())
                            }
                            _ => (StatusCode::METHOD_NOT_ALLOWED, Vec::new()),
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = S3TraceObjects::new(&config(&format!("http://{address}")), None).unwrap();

        store.put("org/trace", b"sealed".to_vec()).await.unwrap();
        assert_eq!(
            store.get("org/trace").await.unwrap().as_deref(),
            Some(&b"sealed"[..])
        );
        store.delete("org/trace").await.unwrap();
        assert_eq!(store.get("org/trace").await.unwrap(), None);
        store.delete("org/trace").await.unwrap();
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .all(|(_, path, signed)| { path == "traces/gw/org/trace" && *signed })
        );
        assert_eq!(
            seen.iter()
                .map(|(method, ..)| method.as_str())
                .collect::<Vec<_>>(),
            ["PUT", "GET", "DELETE", "GET", "DELETE"]
        );
    }
}
