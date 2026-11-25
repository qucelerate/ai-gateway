//! Email domains an organization has proven it controls, so the sign-in page
//! can send `ada@example.com` to the organization that owns `example.com`.
//!
//! An owner adds a domain; the gateway hands back a TXT record to publish at
//! [`record_name`]. Verification looks the record up; the first organization
//! to verify a domain owns it, and a domain has at most one owner.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

/// The label the verification record lives under.
pub const RECORD_LABEL: &str = "_gateway-verification";
/// What the record's value starts with.
pub const VALUE_PREFIX: &str = "gateway-domain-verification=";

/// The longest domain whose verification name ([`record_name`]) still fits
/// DNS's 253-character limit.
pub const MAX_DOMAIN_LEN: usize = 253 - RECORD_LABEL.len() - 1;

/// A domain as it is stored: trimmed, lowercased, without a trailing dot.
/// `None` unless it is a host name of at least two labels whose top-level
/// label is alphabetic (or an IDNA `xn--` label), short enough that its
/// verification record can be published.
pub fn normalize_domain(value: &str) -> Option<String> {
    let domain = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if !(3..=MAX_DOMAIN_LEN).contains(&domain.len()) {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    let label_ok = |label: &&str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let top = labels[labels.len() - 1];
    let top_ok = top.starts_with("xn--") || top.bytes().all(|b| b.is_ascii_lowercase());
    (labels.iter().all(label_ok) && top_ok).then_some(domain)
}

/// Where the verification record of `domain` is published.
pub fn record_name(domain: &str) -> String {
    format!("{RECORD_LABEL}.{domain}")
}

/// The verification record's value for `token`.
pub fn record_value(token: &str) -> String {
    format!("{VALUE_PREFIX}{token}")
}

/// Whether any of a name's TXT records (each the concatenation of its
/// strings) is the expected value.
pub fn records_prove(records: &[String], token: &str) -> bool {
    let expected = record_value(token);
    records.iter().any(|record| record.trim() == expected)
}

/// Why a TXT lookup gave no answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupError {
    /// The resolver could not answer (timeout, server failure).
    Unavailable,
}

pub type LookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<String>, LookupError>> + Send + 'a>>;

/// Looks up the TXT records of a name. A name without records (or that does
/// not exist) answers an empty list.
pub trait TxtLookup: Send + Sync + std::fmt::Debug {
    fn txt<'a>(&'a self, name: &'a str) -> LookupFuture<'a>;
}

/// The system's resolver (`/etc/resolv.conf`), with a short timeout. Built
/// on first use.
#[derive(Debug, Default)]
pub struct SystemResolver {
    resolver: std::sync::OnceLock<Option<hickory_resolver::TokioResolver>>,
}

impl SystemResolver {
    pub fn new() -> Self {
        Self::default()
    }

    fn resolver(&self) -> Option<&hickory_resolver::TokioResolver> {
        self.resolver
            .get_or_init(|| {
                let built = hickory_resolver::Resolver::builder_tokio().and_then(|mut builder| {
                    builder.options_mut().timeout = Duration::from_secs(5);
                    builder.options_mut().attempts = 2;
                    builder.build()
                });
                built
                    .inspect_err(|error| {
                        tracing::warn!(error = %error, "no DNS resolver; domain verification is unavailable");
                    })
                    .ok()
            })
            .as_ref()
    }
}

impl TxtLookup for SystemResolver {
    fn txt<'a>(&'a self, name: &'a str) -> LookupFuture<'a> {
        Box::pin(async move {
            let Some(resolver) = self.resolver() else {
                return Err(LookupError::Unavailable);
            };
            match resolver.txt_lookup(name).await {
                Ok(lookup) => Ok(lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        hickory_resolver::proto::rr::RData::TXT(txt) => Some(
                            txt.txt_data
                                .iter()
                                .map(|part| String::from_utf8_lossy(part))
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect()),
                Err(error) if error.is_no_records_found() || error.is_nx_domain() => Ok(Vec::new()),
                Err(_) => Err(LookupError::Unavailable),
            }
        })
    }
}

/// Fixed answers, for tests: `name → records`.
#[derive(Debug, Default)]
pub struct StaticTxt {
    pub records: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl StaticTxt {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn publish(&self, name: &str, value: &str) {
        self.records
            .lock()
            .expect("static TXT records")
            .entry(name.to_owned())
            .or_default()
            .push(value.to_owned());
    }
}

impl TxtLookup for StaticTxt {
    fn txt<'a>(&'a self, name: &'a str) -> LookupFuture<'a> {
        let records = self
            .records
            .lock()
            .expect("static TXT records")
            .get(name)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(records) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_normalized_or_refused() {
        assert_eq!(
            normalize_domain(" Example.COM. "),
            Some("example.com".into())
        );
        assert_eq!(
            normalize_domain("mail.example.co.uk"),
            Some("mail.example.co.uk".into())
        );
        assert_eq!(
            normalize_domain("xn--bcher-kva.xn--p1ai"),
            Some("xn--bcher-kva.xn--p1ai".into())
        );
        for bad in [
            "",
            "com",
            "localhost",
            "example",
            "-a.com",
            "a-.com",
            "a..com",
            "a.com/x",
            "ada@example.com",
            "10.0.0.1",
            "exa mple.com",
            "*.example.com",
        ] {
            assert_eq!(normalize_domain(bad), None, "{bad}");
        }
        assert_eq!(normalize_domain(&format!("{}.com", "a".repeat(64))), None);
    }

    #[test]
    fn a_domain_is_only_accepted_when_its_record_name_fits_dns() {
        // Three labels of 63 characters, then an alphabetic top-level label
        // making up the rest.
        let of_length = |len: usize| {
            let label = "a".repeat(63);
            format!("{label}.{label}.{label}.{}", "a".repeat(len - 3 * 64))
        };
        let longest = of_length(MAX_DOMAIN_LEN);
        assert_eq!(longest.len(), MAX_DOMAIN_LEN);
        assert_eq!(
            normalize_domain(&longest).as_deref(),
            Some(longest.as_str())
        );
        assert_eq!(record_name(&longest).len(), 253);
        let too_long = of_length(MAX_DOMAIN_LEN + 1);
        assert_eq!(too_long.len(), MAX_DOMAIN_LEN + 1);
        assert_eq!(normalize_domain(&too_long), None);
    }

    #[test]
    fn a_record_proves_only_its_token() {
        assert_eq!(
            record_name("example.com"),
            "_gateway-verification.example.com"
        );
        let records = vec![
            "v=spf1 -all".to_owned(),
            " gateway-domain-verification=abc ".to_owned(),
        ];
        assert!(records_prove(&records, "abc"));
        assert!(!records_prove(&records, "abcd"));
        assert!(!records_prove(&[], "abc"));
    }
}
