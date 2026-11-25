//! What the AWS, Azure and Google credential sources share: a cache of
//! short-lived credentials renewed [`REFRESH_BEFORE`] ahead of their expiry,
//! with one renewal in flight at a time, and a bounded HTTP client for the
//! services that issue them.
//!
//! No credential, token or response body reaches an error or the logs.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

/// Renew temporary credentials this long before they expire.
pub const REFRESH_BEFORE: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Why credentials could not be obtained. Carries no response content.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialsError {
    #[error("no cloud credentials are available")]
    Unavailable,
    #[error("the credential service refused the request (HTTP {0})")]
    Refused(u16),
    #[error("the credential service answered with something unexpected")]
    InvalidResponse,
    #[error("the credential service did not answer in time")]
    TimedOut,
}

/// A credential with its expiry; `None` for one that does not expire.
#[derive(Clone)]
pub(crate) struct Expiring<T> {
    pub(crate) value: T,
    pub(crate) expires: Option<DateTime<Utc>>,
}

#[async_trait]
pub(crate) trait Fetch<T>: Send + Sync {
    async fn fetch(&self) -> Result<Expiring<T>, CredentialsError>;
    fn describe(&self) -> &'static str;
}

/// The current credential of one source, renewed when it nears expiry.
pub(crate) struct Refreshing<T> {
    fetch: Box<dyn Fetch<T>>,
    current: Mutex<Option<Expiring<T>>>,
    /// A copy of `current`, readable while a renewal holds it.
    last: std::sync::Mutex<Option<Expiring<T>>>,
}

impl<T: Clone + Send> Refreshing<T> {
    pub(crate) fn new(fetch: impl Fetch<T> + 'static) -> Self {
        Self {
            fetch: Box::new(fetch),
            current: Mutex::new(None),
            last: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn describe(&self) -> &'static str {
        self.fetch.describe()
    }

    /// The credential to use now.
    pub(crate) async fn get(&self) -> Result<T, CredentialsError> {
        self.get_within(None).await
    }

    /// The credential to use now, waited for at most `deadline`. A renewal
    /// that does not finish in time leaves a credential that has not
    /// expired in use, as a failed one does.
    pub(crate) async fn get_within(
        &self,
        deadline: Option<Duration>,
    ) -> Result<T, CredentialsError> {
        let until = deadline.map(|deadline| tokio::time::Instant::now() + deadline);
        let mut current = match until {
            Some(until) => match tokio::time::timeout_at(until, self.current.lock()).await {
                Ok(current) => current,
                // Another request's renewal is taking too long.
                Err(_) => return self.unexpired().ok_or(CredentialsError::TimedOut),
            },
            None => self.current.lock().await,
        };
        if let Some(cached) = current.as_ref() {
            let valid = cached.expires.is_none_or(|expires| {
                expires - Utc::now()
                    > chrono::Duration::from_std(REFRESH_BEFORE).expect("refresh window fits")
            });
            if valid {
                return Ok(cached.value.clone());
            }
        }
        let fetched = match until {
            Some(until) => tokio::time::timeout_at(until, self.fetch.fetch())
                .await
                .unwrap_or(Err(CredentialsError::TimedOut)),
            None => self.fetch.fetch().await,
        };
        match fetched {
            Ok(fresh) => {
                let value = fresh.value.clone();
                *self
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fresh.clone());
                *current = Some(fresh);
                Ok(value)
            }
            // A credential that has not expired yet is still usable while a
            // renewal fails.
            Err(error) => match current.as_ref() {
                Some(cached) if cached.expires.is_some_and(|expires| expires > Utc::now()) => {
                    tracing::warn!(
                        error = %error,
                        source = self.describe(),
                        "credential renewal failed; using the current credential"
                    );
                    Ok(cached.value.clone())
                }
                _ => Err(error),
            },
        }
    }

    /// The latest credential, if it has not expired.
    fn unexpired(&self) -> Option<T> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|cached| cached.expires.is_none_or(|expires| expires > Utc::now()))
            .map(|cached| cached.value.clone())
    }

    /// Whether the current credential is still valid. One being renewed
    /// counts as valid.
    pub(crate) fn valid(&self) -> bool {
        self.current.try_lock().map_or(true, |current| {
            current
                .as_ref()
                .is_some_and(|cached| cached.expires.is_none_or(|expires| expires > Utc::now()))
        })
    }
}

/// A client for credential services: bounded, never redirected, never
/// proxied.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// The body of a successful response, at most 64 KiB.
pub(crate) async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, CredentialsError> {
    let status = response.status();
    if !status.is_success() {
        return Err(CredentialsError::Refused(status.as_u16()));
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CredentialsError::Unavailable)?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(CredentialsError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The form encoding of `pairs`.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// `http` is only for loopback: a test or local stand-in.
pub(crate) fn https_or_loopback(url: &url::Url) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && crate::net::is_loopback_url(url))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// Answers its first fetch with `first-1`, expiring in `lifetime`, and
    /// never answers again.
    struct OnceThenStall {
        calls: Arc<AtomicUsize>,
        lifetime: chrono::Duration,
    }

    #[async_trait]
    impl Fetch<String> for OnceThenStall {
        async fn fetch(&self) -> Result<Expiring<String>, CredentialsError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(Expiring {
                    value: "first-1".into(),
                    expires: Some(Utc::now() + self.lifetime),
                });
            }
            std::future::pending().await
        }

        fn describe(&self) -> &'static str {
            "test"
        }
    }

    fn stalling(lifetime: chrono::Duration) -> (Arc<Refreshing<String>>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Refreshing::new(OnceThenStall {
            calls: calls.clone(),
            lifetime,
        });
        (Arc::new(source), calls)
    }

    #[tokio::test]
    async fn a_slow_renewal_leaves_an_unexpired_credential_in_use() {
        let deadline = Some(Duration::from_millis(50));
        // Inside the renewal window, but not expired.
        let (source, calls) = stalling(chrono::Duration::minutes(2));
        assert_eq!(source.get_within(deadline).await.unwrap(), "first-1");
        assert_eq!(source.get_within(deadline).await.unwrap(), "first-1");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a renewal was tried");

        // While another request's renewal holds the cache.
        let holder = source.clone();
        let stalled = tokio::spawn(async move { holder.get().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(source.get_within(deadline).await.unwrap(), "first-1");
        stalled.abort();

        // An expired credential is not used: the wait times out.
        let (expired, _) = stalling(chrono::Duration::seconds(-1));
        assert_eq!(expired.get_within(deadline).await.unwrap(), "first-1");
        assert_eq!(
            expired.get_within(deadline).await.unwrap_err(),
            CredentialsError::TimedOut
        );
    }
}
