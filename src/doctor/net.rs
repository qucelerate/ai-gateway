//! Network probes for `gateway doctor`: resolve a host, connect, and
//! complete a TLS handshake, the way the gateway's own HTTP clients would
//! reach it. No request is ever sent to the target.
//!
//! The gateway's provider, trace-store and ClickHouse clients honour the
//! standard proxy variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and
//! `NO_PROXY`, upper or lower case), so a probe does too: through an
//! `http://` proxy it sends `CONNECT host:port` and completes the TLS
//! handshake through the tunnel. A proxy it cannot tunnel through (an
//! `https://` or SOCKS proxy, or a plain-HTTP target, which the proxy
//! forwards rather than tunnels) is reported as not probed.

use std::{net::IpAddr, sync::Arc, time::Duration};

use base64::Engine as _;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use url::Url;

/// How long any one step (DNS, TCP, proxy CONNECT, TLS) may take.
pub(super) const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// What a probe found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Probe {
    /// Reached, and how (`TLS 443`, `TLS 443 via proxy proxy.internal:3128`).
    Reached(String),
    /// Could not be reached, and why.
    Failed(String),
    /// Deliberately not probed, and why (a proxy it cannot tunnel through).
    NotProbed(String),
}

/// The proxy the gateway's HTTP clients would use for `url`, as reqwest
/// picks it from the environment, unless `NO_PROXY` exempts the host.
pub(super) fn proxy_for(url: &Url, lookup: &dyn Fn(&str) -> Option<String>) -> Option<Url> {
    let get = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| lookup(name).filter(|value| !value.trim().is_empty()))
    };
    let raw = match url.scheme() {
        "https" => get(&["HTTPS_PROXY", "https_proxy"]),
        "http" => get(&["HTTP_PROXY", "http_proxy"]),
        _ => None,
    }
    .or_else(|| get(&["ALL_PROXY", "all_proxy"]))?;
    let host = url.host_str()?;
    if let Some(no_proxy) = get(&["NO_PROXY", "no_proxy"])
        && no_proxy_matches(host, &no_proxy)
    {
        return None;
    }
    let raw = raw.trim();
    // A bare `host:port` is an http proxy, as reqwest takes it.
    let parsed = if raw.contains("://") {
        Url::parse(raw)
    } else {
        Url::parse(&format!("http://{raw}"))
    };
    parsed.ok()
}

/// Whether `NO_PROXY` (comma-separated: `*`, domains matching themselves
/// and their subdomains, with or without a leading `.` or `*.`, IP
/// addresses and CIDR ranges) exempts `host`.
pub(super) fn no_proxy_matches(host: &str, no_proxy: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let address: Option<IpAddr> = host.parse().ok();
    no_proxy.split(',').map(str::trim).any(|entry| {
        if entry.is_empty() {
            return false;
        }
        if entry == "*" {
            return true;
        }
        let entry = entry.to_ascii_lowercase();
        if let Some(address) = address {
            return ip_matches(address, &entry);
        }
        let domain = entry.trim_start_matches("*.").trim_start_matches('.');
        // A port on the entry is not part of the match, as in reqwest.
        let domain = domain.split(':').next().unwrap_or(domain);
        host == domain || host.ends_with(&format!(".{domain}"))
    })
}

fn ip_matches(address: IpAddr, entry: &str) -> bool {
    let entry = entry.trim_start_matches('[').trim_end_matches(']');
    let (network, bits) = match entry.split_once('/') {
        Some((network, bits)) => (network, bits.parse::<u32>().ok()),
        None => (entry, None),
    };
    let Ok(network) = network.parse::<IpAddr>() else {
        return false;
    };
    match (address, network) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let bits = bits.unwrap_or(32).min(32);
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let bits = bits.unwrap_or(128).min(128);
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}

/// `host:port` of a URL, for messages: never its credentials.
fn endpoint(url: &Url) -> String {
    match (url.host_str(), url.port_or_known_default()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => "?".to_owned(),
    }
}

/// Probes `url` as the gateway would reach it, through the proxy the
/// environment names for it. `extra_roots` is a PEM bundle trusted besides
/// the public roots.
pub(super) async fn probe(
    url: &str,
    extra_roots: Option<&[u8]>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Probe {
    let Ok(url) = Url::parse(url) else {
        return Probe::Failed("not a valid URL".into());
    };
    let Some(host) = url.host_str().map(|host| {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned()
    }) else {
        return Probe::Failed("the URL has no host".into());
    };
    let Some(port) = url.port_or_known_default() else {
        return Probe::Failed("the URL has no port".into());
    };
    let https = url.scheme() == "https";
    let (stream, via) = match proxy_for(&url, lookup) {
        None => match connect_direct(&host, port).await {
            Ok(stream) => (stream, String::new()),
            Err(reason) => return Probe::Failed(reason),
        },
        Some(proxy) => {
            let at = endpoint(&proxy);
            if proxy.scheme() != "http" || !https {
                return Probe::NotProbed(format!(
                    "not probed: traffic goes through a proxy ({at})"
                ));
            }
            match connect_via_proxy(&proxy, &host, port).await {
                Ok(stream) => (stream, format!(" via proxy {at}")),
                Err(reason) => return Probe::Failed(format!("through proxy {at}: {reason}")),
            }
        }
    };
    if !https {
        return Probe::Reached(format!("TCP {port}{via}"));
    }
    match tls_handshake(stream, &host, extra_roots).await {
        Ok(()) => Probe::Reached(format!("TLS {port}{via}")),
        Err(reason) => Probe::Failed(reason),
    }
}

async fn connect_direct(host: &str, port: u16) -> Result<TcpStream, String> {
    let addresses: Vec<_> =
        match tokio::time::timeout(STEP_TIMEOUT, tokio::net::lookup_host((host, port))).await {
            Ok(Ok(addresses)) => addresses.collect(),
            Ok(Err(error)) => return Err(format!("DNS lookup failed: {error}")),
            Err(_) => return Err("DNS lookup timed out".into()),
        };
    if addresses.is_empty() {
        return Err("DNS returned no addresses".into());
    }
    match tokio::time::timeout(STEP_TIMEOUT, TcpStream::connect(addresses.as_slice())).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(format!("connect to port {port} failed: {error}")),
        Err(_) => Err(format!("connect to port {port} timed out")),
    }
}

/// Opens a tunnel to `host:port` through an `http://` proxy with `CONNECT`.
async fn connect_via_proxy(proxy: &Url, host: &str, port: u16) -> Result<TcpStream, String> {
    let proxy_host = proxy
        .host_str()
        .ok_or("the proxy URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let mut stream = connect_direct(&proxy_host, proxy_port).await?;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.username().is_empty() {
        let credentials = format!(
            "{}:{}",
            percent_decode(proxy.username()),
            percent_decode(proxy.password().unwrap_or(""))
        );
        let token = base64::engine::general_purpose::STANDARD.encode(credentials);
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    let exchange = async {
        stream.write_all(request.as_bytes()).await?;
        // The response head only: the tunnel starts right after it.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
            if stream.read(&mut byte).await? == 0 {
                break;
            }
            head.push(byte[0]);
        }
        Ok::<_, std::io::Error>(head)
    };
    let head = match tokio::time::timeout(STEP_TIMEOUT, exchange).await {
        Ok(Ok(head)) => head,
        Ok(Err(error)) => return Err(format!("CONNECT failed: {error}")),
        Err(_) => return Err("CONNECT timed out".into()),
    };
    let status_line = String::from_utf8_lossy(&head);
    let status_line = status_line.lines().next().unwrap_or("");
    let status = status_line.split_whitespace().nth(1).unwrap_or("");
    if status.starts_with('2') {
        Ok(stream)
    } else if status.is_empty() {
        Err("the proxy closed the connection without answering CONNECT".into())
    } else {
        Err(format!("the proxy refused CONNECT with status {status}"))
    }
}

/// Percent-decodes a URL's user or password, as reqwest does.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 3 <= bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn tls_handshake(
    stream: TcpStream,
    host: &str,
    extra_roots: Option<&[u8]>,
) -> Result<(), String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_roots {
        use rustls_pki_types::{CertificateDer, pem::PemObject};
        for certificate in CertificateDer::pem_slice_iter(pem).flatten() {
            let _ = roots.add(certificate);
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| "TLS is unavailable".to_owned())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls_pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| "the host is not a valid TLS server name".to_owned())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
    match tokio::time::timeout(STEP_TIMEOUT, connector.connect(name, stream)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("TLS handshake failed: {error}")),
        Err(_) => Err("TLS handshake timed out".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn picks_the_proxy_reqwest_would() {
        let vars = [
            ("HTTPS_PROXY", "http://proxy.internal:3128"),
            ("http_proxy", "plain:8080"),
        ];
        let proxy = proxy_for(&url("https://api.openai.com/v1"), &env(&vars)).unwrap();
        assert_eq!(endpoint(&proxy), "proxy.internal:3128");
        let proxy = proxy_for(&url("http://ch:8123"), &env(&vars)).unwrap();
        assert_eq!(endpoint(&proxy), "plain:8080");
        let all = [("ALL_PROXY", "socks5://s:1080")];
        assert_eq!(
            proxy_for(&url("https://x.example"), &env(&all))
                .unwrap()
                .scheme(),
            "socks5"
        );
        assert!(proxy_for(&url("https://x.example"), &env(&[])).is_none());
    }

    #[test]
    fn no_proxy_exempts_domains_addresses_and_ranges() {
        for (host, list) in [
            ("api.openai.com", "openai.com"),
            ("api.openai.com", ".openai.com"),
            ("api.openai.com", "*.openai.com"),
            ("openai.com", "localhost, openai.com"),
            ("anything", "*"),
            ("10.2.3.4", "10.0.0.0/8"),
            ("192.0.2.7", "192.0.2.7"),
            ("[::1]", "::1"),
            ("fd00::5", "fd00::/8"),
        ] {
            assert!(no_proxy_matches(host, list), "{host} in {list}");
        }
        for (host, list) in [
            ("notopenai.com", "openai.com"),
            ("api.openai.com", "anthropic.com"),
            ("11.0.0.1", "10.0.0.0/8"),
            ("api.openai.com", ""),
        ] {
            assert!(!no_proxy_matches(host, list), "{host} not in {list}");
        }
        let vars = [("HTTPS_PROXY", "http://p:3128"), ("NO_PROXY", "openai.com")];
        assert!(proxy_for(&url("https://api.openai.com"), &env(&vars)).is_none());
    }

    #[test]
    fn decodes_proxy_credentials() {
        assert_eq!(percent_decode("us%40er"), "us@er");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("trail%2"), "trail%2");
    }

    #[tokio::test]
    async fn an_untunnelable_proxy_is_not_probed() {
        let vars = [("HTTPS_PROXY", "https://p.internal:443")];
        let result = probe("https://api.openai.com/v1", None, &env(&vars)).await;
        assert_eq!(
            result,
            Probe::NotProbed("not probed: traffic goes through a proxy (p.internal:443)".into())
        );
        let vars = [("HTTP_PROXY", "http://p.internal:3128")];
        assert!(matches!(
            probe("http://ch:8123", None, &env(&vars)).await,
            Probe::NotProbed(_)
        ));
    }

    #[tokio::test]
    async fn connects_through_an_http_proxy_with_connect() {
        // A proxy that answers CONNECT and then closes: the tunnel opens,
        // and the TLS handshake through it fails, which shows the probe
        // went through the proxy.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut buffer = vec![0u8; 1024];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            request
        });
        let proxy_url = format!("http://user:p%40ss@127.0.0.1:{port}");
        let vars = [("HTTPS_PROXY", proxy_url.as_str())];
        let result = probe("https://api.example.invalid/v1", None, &env(&vars)).await;
        let request = server.await.unwrap();
        assert!(
            request.starts_with("CONNECT api.example.invalid:443 HTTP/1.1\r\n"),
            "{request}"
        );
        let token = base64::engine::general_purpose::STANDARD.encode("user:p@ss");
        assert!(request.contains(&format!("Proxy-Authorization: Basic {token}")));
        match result {
            Probe::Failed(reason) => assert!(reason.starts_with("TLS handshake"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refusing_proxy_fails_with_its_status() {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut buffer = vec![0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
        });
        let proxy_url = format!("http://127.0.0.1:{port}");
        let vars = [("https_proxy", proxy_url.as_str())];
        let result = probe("https://api.example.invalid", None, &env(&vars)).await;
        assert_eq!(
            result,
            Probe::Failed(format!(
                "through proxy 127.0.0.1:{port}: the proxy refused CONNECT with status 403"
            ))
        );
    }

    #[tokio::test]
    async fn reports_a_refused_port_and_a_reachable_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let none = env(&[]);
        assert_eq!(
            probe(&format!("http://127.0.0.1:{port}/"), None, &none).await,
            Probe::Reached(format!("TCP {port}"))
        );
        drop(listener);
        match probe(&format!("http://127.0.0.1:{port}/"), None, &none).await {
            Probe::Failed(reason) => {
                assert!(
                    reason.starts_with(&format!("connect to port {port} failed")),
                    "{reason}"
                )
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            probe("not a url", None, &none).await,
            Probe::Failed("not a valid URL".into())
        );
    }
}
