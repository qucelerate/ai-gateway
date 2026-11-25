//! Outbound HTTP from the worker to URLs it does not control: webhook
//! endpoints and operator price sheets.
//!
//! DNS can change after a URL is validated, so every request resolves the
//! host itself, requires every address to be public, and pins the connection
//! to those addresses. Redirects and proxies are never followed.

use std::{net::SocketAddr, time::Duration};

use url::{Host, Url};

use crate::domain::webhook::is_public_ip;

const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Which destinations a request may reach.
#[derive(Clone, Copy, Debug)]
pub struct DeliveryPolicy {
    allow_private_targets: bool,
}

impl DeliveryPolicy {
    /// HTTPS to public addresses only. The only policy for real requests.
    pub const fn public_only() -> Self {
        Self {
            allow_private_targets: false,
        }
    }

    /// Also admits plain HTTP and private or loopback addresses: for a
    /// destination the operator named (an OpenAI-compatible endpoint on its
    /// allowlist), and for tests to reach a local listener.
    pub const fn operator_allowed() -> Self {
        Self {
            allow_private_targets: true,
        }
    }

    /// Also admits plain HTTP and private or loopback addresses, so tests can
    /// reach a local listener. Never use outside tests.
    pub const fn allow_private_targets_for_tests() -> Self {
        Self {
            allow_private_targets: true,
        }
    }

    /// Whether plain HTTP and private addresses are admitted (tests only).
    pub(crate) const fn allows_private_targets(self) -> bool {
        self.allow_private_targets
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EgressError {
    InvalidUrl,
    DestinationNotAllowed,
    DnsTimedOut,
    DnsFailed,
    ClientUnavailable,
}

/// A client from `builder` that can only reach `url`'s host at addresses
/// resolved and checked here.
pub(crate) async fn pinned_client(
    url: &Url,
    policy: DeliveryPolicy,
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, EgressError> {
    let allow_private = policy.allow_private_targets;
    if url.scheme() != "https" && !(allow_private && url.scheme() == "http") {
        return Err(EgressError::InvalidUrl);
    }
    let port = url.port_or_known_default().ok_or(EgressError::InvalidUrl)?;
    let builder = builder
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    let builder = match url.host() {
        Some(Host::Ipv4(address)) => {
            if !allow_private && !is_public_ip(address.into()) {
                return Err(EgressError::DestinationNotAllowed);
            }
            builder
        }
        Some(Host::Ipv6(address)) => {
            if !allow_private && !is_public_ip(address.into()) {
                return Err(EgressError::DestinationNotAllowed);
            }
            builder
        }
        Some(Host::Domain(domain)) => {
            let addresses: Vec<SocketAddr> =
                tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((domain, port)))
                    .await
                    .map_err(|_| EgressError::DnsTimedOut)?
                    .map_err(|_| EgressError::DnsFailed)?
                    .collect();
            if addresses.is_empty() {
                return Err(EgressError::DnsFailed);
            }
            if !allow_private && addresses.iter().any(|address| !is_public_ip(address.ip())) {
                return Err(EgressError::DestinationNotAllowed);
            }
            builder.resolve_to_addrs(domain, &addresses)
        }
        None => return Err(EgressError::InvalidUrl),
    };
    builder.build().map_err(|_| EgressError::ClientUnavailable)
}

/// Reads at most `limit` bytes of `response`'s body; `Ok(None)` when it is
/// longer, without reading the rest.
pub(crate) async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pin(url: &str, policy: DeliveryPolicy) -> Result<reqwest::Client, EgressError> {
        pinned_client(
            &Url::parse(url).unwrap(),
            policy,
            reqwest::Client::builder(),
        )
        .await
    }

    #[tokio::test]
    async fn ipv6_destinations_that_are_not_public_are_refused() {
        for url in [
            "https://[::1]/hook",
            "https://[fd00:ec2::254]/latest/meta-data",
            "https://[fe80::1]/",
            "https://[::ffff:10.0.0.1]/",
            "https://[64:ff9b::a9fe:a9fe]/",
            "https://[2002:a9fe:a9fe::1]/",
        ] {
            assert_eq!(
                pin(url, DeliveryPolicy::public_only()).await.err(),
                Some(EgressError::DestinationNotAllowed),
                "{url}"
            );
        }
        // `localhost` resolves to ::1 and 127.0.0.1: neither is allowed.
        assert_eq!(
            pin("https://localhost/", DeliveryPolicy::public_only())
                .await
                .err(),
            Some(EgressError::DestinationNotAllowed)
        );
        // A public IPv6 literal is pinned like an IPv4 one.
        assert!(
            pin(
                "https://[2606:4700:4700::1111]/",
                DeliveryPolicy::public_only()
            )
            .await
            .is_ok()
        );
        // Tests may reach an IPv6 loopback listener.
        assert!(
            pin(
                "http://[::1]:9/",
                DeliveryPolicy::allow_private_targets_for_tests()
            )
            .await
            .is_ok()
        );
    }
}
