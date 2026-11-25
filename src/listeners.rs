//! The API's optional TLS (HTTP/1.1 and HTTP/2) and QUIC (HTTP/3) listeners.
//!
//! The plain listener (`BIND_ADDR`) always runs and speaks HTTP/1.1 and
//! HTTP/2 with prior knowledge (h2c), for a proxy or ingress that terminates
//! TLS. A deployment without such an edge, or one whose edge cannot serve
//! HTTP/3, may also terminate TLS in the gateway (D-027):
//!
//! - `GATEWAY_TLS_ADDR`: a TCP listener that negotiates HTTP/2 or HTTP/1.1
//!   by ALPN;
//! - `GATEWAY_HTTP3_ADDR`: a UDP listener serving HTTP/3 over QUIC;
//! - `GATEWAY_TLS_CERT_FILE`, `GATEWAY_TLS_KEY_FILE`: the PEM certificate
//!   chain and private key both use, read again when either file changes;
//! - `GATEWAY_ALT_SVC`: the `Alt-Svc` value responses carry, so clients
//!   find the HTTP/3 listener; `h3=":{port}"; ma=86400` by default while it
//!   runs, `off` to send none. Behind a proxy that publishes the QUIC port
//!   elsewhere, set it to the address clients reach.
//!
//! Every listener serves the same router, so routes, body limits, metrics
//! and streaming behave alike. A request on the TLS or QUIC listener comes
//! straight from its client, so its `X-Forwarded-For` is never trusted
//! ([`DirectConnection`]).

use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{HeaderValue, Request, Response, header},
};
use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use thiserror::Error;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tower::ServiceExt;

/// How often the certificate files are checked for a change.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);
/// A TLS handshake that takes longer is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// QUIC keep-alives, so a stream that is quiet while a model thinks is not
/// closed as idle.
const QUIC_KEEP_ALIVE: Duration = Duration::from_secs(15);
const QUIC_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// HTTP/3's code for a connection closed without error.
const H3_NO_ERROR: u32 = 0x100;

/// Marks a request that arrived on the TLS or QUIC listener: straight from
/// its client, never through the proxy `GATEWAY_TRUST_X_FORWARDED_FOR` trusts.
#[derive(Clone, Copy, Debug)]
pub struct DirectConnection;

/// The TLS and QUIC listeners to run beside the plain one; all off by
/// default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListenerConfig {
    pub tls_addr: Option<SocketAddr>,
    pub http3_addr: Option<SocketAddr>,
    /// Both set exactly when a TLS or QUIC listener is.
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    /// The `Alt-Svc` header value responses carry, if any.
    pub alt_svc: Option<String>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ListenerConfigError {
    #[error("{name} is not a valid socket address: {value}")]
    InvalidAddr { name: &'static str, value: String },
    #[error("{name} must differ from {other}")]
    SameAddr {
        name: &'static str,
        other: &'static str,
    },
    #[error("GATEWAY_TLS_CERT_FILE and GATEWAY_TLS_KEY_FILE are required with {name}")]
    MissingCertificate { name: &'static str },
    #[error(
        "GATEWAY_TLS_CERT_FILE and GATEWAY_TLS_KEY_FILE are used only with GATEWAY_TLS_ADDR or GATEWAY_HTTP3_ADDR"
    )]
    UnusedCertificate,
    #[error("GATEWAY_ALT_SVC is not a valid header value")]
    InvalidAltSvc,
}

impl ListenerConfig {
    /// Reads the listeners' settings; `bind_addr` and `metrics_addr` are the
    /// TCP addresses already taken.
    pub fn from_env(
        config_env: &impl Fn(&str) -> Result<String, env::VarError>,
        bind_addr: SocketAddr,
        metrics_addr: Option<SocketAddr>,
    ) -> Result<Self, ListenerConfigError> {
        let set = |name: &str| {
            config_env(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let addr = |name: &'static str| -> Result<Option<SocketAddr>, ListenerConfigError> {
            set(name)
                .map(|value| {
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| ListenerConfigError::InvalidAddr { name, value })
                })
                .transpose()
        };
        let tls_addr = addr("GATEWAY_TLS_ADDR")?;
        let http3_addr = addr("GATEWAY_HTTP3_ADDR")?;
        if let Some(tls) = tls_addr {
            if crate::net::listeners_overlap(tls, bind_addr) {
                return Err(ListenerConfigError::SameAddr {
                    name: "GATEWAY_TLS_ADDR",
                    other: "BIND_ADDR",
                });
            }
            if metrics_addr.is_some_and(|metrics| crate::net::listeners_overlap(tls, metrics)) {
                return Err(ListenerConfigError::SameAddr {
                    name: "GATEWAY_TLS_ADDR",
                    other: "GATEWAY_METRICS_ADDR",
                });
            }
        }
        let cert_file = set("GATEWAY_TLS_CERT_FILE").map(PathBuf::from);
        let key_file = set("GATEWAY_TLS_KEY_FILE").map(PathBuf::from);
        let needs = match (tls_addr, http3_addr) {
            (Some(_), _) => Some("GATEWAY_TLS_ADDR"),
            (None, Some(_)) => Some("GATEWAY_HTTP3_ADDR"),
            (None, None) => None,
        };
        match (needs, cert_file.is_some() && key_file.is_some()) {
            (Some(name), false) => return Err(ListenerConfigError::MissingCertificate { name }),
            (None, _) if cert_file.is_some() || key_file.is_some() => {
                return Err(ListenerConfigError::UnusedCertificate);
            }
            _ => {}
        }
        let alt_svc = match set("GATEWAY_ALT_SVC") {
            Some(value) if value.eq_ignore_ascii_case("off") => None,
            Some(value) => {
                HeaderValue::from_str(&value).map_err(|_| ListenerConfigError::InvalidAltSvc)?;
                Some(value)
            }
            None => http3_addr.map(|quic| format!("h3=\":{}\"; ma=86400", quic.port())),
        };
        Ok(Self {
            tls_addr,
            http3_addr,
            cert_file,
            key_file,
            alt_svc,
        })
    }
}

#[derive(Debug, Error)]
pub enum ListenerError {
    #[error("failed to read the TLS certificate or key: {0}")]
    Certificate(String),
    #[error("failed to bind the TLS listener at {address}: {source}")]
    TlsBind {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("failed to bind the HTTP/3 listener at {address}: {source}")]
    QuicBind {
        address: SocketAddr,
        source: std::io::Error,
    },
}

/// Adds the configured `Alt-Svc` header to every response that lacks one.
pub fn with_alt_svc(router: Router, config: &ListenerConfig) -> Router {
    let Some(value) = config
        .alt_svc
        .as_deref()
        .and_then(|value| HeaderValue::from_str(value).ok())
    else {
        return router;
    };
    router.layer(axum::middleware::map_response(
        move |mut response: axum::response::Response| {
            let value = value.clone();
            async move {
                response
                    .headers_mut()
                    .entry(header::ALT_SVC)
                    .or_insert(value);
                response
            }
        },
    ))
}

/// The certificate the TLS and QUIC listeners present, replaced when its
/// files change (as when cert-manager renews a mounted secret).
#[derive(Debug)]
pub struct CertificateFiles {
    cert_file: PathBuf,
    key_file: PathBuf,
    provider: Arc<CryptoProvider>,
    current: RwLock<(Arc<CertifiedKey>, Option<(SystemTime, SystemTime)>)>,
}

impl CertificateFiles {
    pub fn load(cert_file: &Path, key_file: &Path) -> Result<Arc<Self>, ListenerError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = certified_key(&provider, cert_file, key_file)?;
        Ok(Arc::new(Self {
            cert_file: cert_file.to_owned(),
            key_file: key_file.to_owned(),
            provider,
            current: RwLock::new((Arc::new(key), modified(cert_file, key_file))),
        }))
    }

    /// Reads the files again if either changed; a pair that does not load
    /// leaves the current certificate in place.
    pub fn reload_if_changed(&self) -> bool {
        let stamp = modified(&self.cert_file, &self.key_file);
        if stamp.is_none() || self.read().1 == stamp {
            return false;
        }
        match certified_key(&self.provider, &self.cert_file, &self.key_file) {
            Ok(key) => {
                *self
                    .current
                    .write()
                    .unwrap_or_else(|error| error.into_inner()) = (Arc::new(key), stamp);
                tracing::info!("TLS certificate reloaded");
                true
            }
            Err(error) => {
                tracing::warn!(%error, "TLS certificate not reloaded; keeping the current one");
                false
            }
        }
    }

    fn read(&self) -> (Arc<CertifiedKey>, Option<(SystemTime, SystemTime)>) {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// A rustls configuration presenting this certificate, offering `alpn`.
    pub fn server_config(self: &Arc<Self>, alpn: &[&[u8]]) -> Result<ServerConfig, ListenerError> {
        let mut config = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|error| ListenerError::Certificate(error.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(self.clone());
        config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
        Ok(config)
    }

    /// Checks the files every [`RELOAD_INTERVAL`] until shutdown.
    pub async fn watch(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(RELOAD_INTERVAL);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => { self.reload_if_changed(); }
                _ = wait_for(&mut shutdown) => return,
            }
        }
    }
}

impl ResolvesServerCert for CertificateFiles {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.read().0)
    }
}

fn modified(cert_file: &Path, key_file: &Path) -> Option<(SystemTime, SystemTime)> {
    let stamp = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    };
    Some((stamp(cert_file)?, stamp(key_file)?))
}

fn certified_key(
    provider: &CryptoProvider,
    cert_file: &Path,
    key_file: &Path,
) -> Result<CertifiedKey, ListenerError> {
    let failed = |what: &str, error: &dyn std::fmt::Display| {
        ListenerError::Certificate(format!("{what}: {error}"))
    };
    let chain = CertificateDer::pem_file_iter(cert_file)
        .map_err(|error| failed("certificate file", &error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed("certificate file", &error))?;
    if chain.is_empty() {
        return Err(ListenerError::Certificate(
            "certificate file: no certificate".to_owned(),
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_file).map_err(|error| failed("key file", &error))?;
    CertifiedKey::from_der(chain, key, provider).map_err(|error| failed("key", &error))
}

/// Resolves once shutdown is signalled.
pub async fn wait_for(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// The router as a per-connection service: the peer's address feeds
/// sign-in rate limits as on the plain listener, marked as direct.
fn direct_request<B>(mut request: Request<B>, remote: SocketAddr) -> Request<Body>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<axum::BoxError>,
{
    request.extensions_mut().insert(ConnectInfo(remote));
    request.extensions_mut().insert(DirectConnection);
    request.map(Body::new)
}

/// Serves `router` over TLS on `listener`, HTTP/2 or HTTP/1.1 by ALPN, until
/// shutdown; then finishes the requests in flight.
pub async fn serve_tls(
    listener: TcpListener,
    certificate: Arc<CertificateFiles>,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ListenerError> {
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::{conn::auto, graceful::GracefulShutdown},
    };

    let config = certificate.server_config(&[b"h2", b"http/1.1"])?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let graceful = GracefulShutdown::new();
    let builder = auto::Builder::new(TokioExecutor::new());
    loop {
        let (stream, remote) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Such as too many open files: back off rather than spin.
                    tracing::warn!(%error, "TLS listener accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = wait_for(&mut shutdown) => break,
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        let builder = builder.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let Ok(Ok(tls)) =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
            else {
                return;
            };
            let service =
                hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                    router.clone().oneshot(direct_request(request, remote))
                });
            let connection = builder.serve_connection_with_upgrades(TokioIo::new(tls), service);
            if let Err(error) = watcher.watch(connection.into_owned()).await {
                tracing::debug!(error = %error, "TLS connection ended with an error");
            }
        });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// A QUIC endpoint for [`serve_quic`], bound to `address`.
pub fn bind_quic(
    address: SocketAddr,
    certificate: &Arc<CertificateFiles>,
) -> Result<quinn::Endpoint, ListenerError> {
    let tls = certificate.server_config(&[b"h3"])?;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|error| ListenerError::Certificate(error.to_string()))?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(QUIC_KEEP_ALIVE));
    transport.max_idle_timeout(Some(
        QUIC_IDLE_TIMEOUT
            .try_into()
            .expect("the idle timeout fits a QUIC varint"),
    ));
    server.transport_config(Arc::new(transport));
    let socket = crate::net::bind_udp(address)
        .map_err(|source| ListenerError::QuicBind { address, source })?;
    let runtime = quinn::default_runtime().ok_or_else(|| ListenerError::QuicBind {
        address,
        source: std::io::Error::other("no async runtime for QUIC"),
    })?;
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server),
        socket,
        runtime,
    )
    .map_err(|source| ListenerError::QuicBind { address, source })
}

/// Serves `router` over HTTP/3 on `endpoint` until shutdown; then sends
/// each connection GOAWAY and finishes the requests in flight.
pub async fn serve_quic(
    endpoint: quinn::Endpoint,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ListenerError> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                Some(incoming) => {
                    connections.spawn(quic_connection(incoming, router.clone(), shutdown.clone()));
                }
                None => break,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = wait_for(&mut shutdown) => break,
        }
    }
    while connections.join_next().await.is_some() {}
    endpoint.close(quinn::VarInt::from_u32(H3_NO_ERROR), b"");
    endpoint.wait_idle().await;
    Ok(())
}

async fn quic_connection(
    incoming: quinn::Incoming,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) {
    let Ok(connection) = incoming.await else {
        return;
    };
    let remote = connection.remote_address();
    let quic = connection.clone();
    let Ok(mut h3) =
        h3::server::Connection::<_, Bytes>::new(h3_quinn::Connection::new(connection)).await
    else {
        return;
    };
    let mut requests = JoinSet::new();
    let mut closing = false;
    loop {
        tokio::select! {
            _ = wait_for(&mut shutdown), if !closing => {
                closing = true;
                // GOAWAY: requests already accepted finish, no new ones start.
                let _ = h3.shutdown(0).await;
            }
            Some(_) = requests.join_next(), if !requests.is_empty() => {}
            accepted = h3.accept() => match accepted {
                Ok(Some(resolver)) => {
                    requests.spawn(quic_request(resolver, router.clone(), remote, quic.clone()));
                }
                Ok(None) => break,
                Err(error) => {
                    if !error.is_h3_no_error() {
                        tracing::debug!(%error, "HTTP/3 connection ended with an error");
                    }
                    break;
                }
            },
        }
        if closing && requests.is_empty() {
            break;
        }
    }
    while requests.join_next().await.is_some() {}
    quic.close(quinn::VarInt::from_u32(H3_NO_ERROR), b"");
}

/// Serves one HTTP/3 request. The work behind it (a provider call, a
/// stream) is dropped once the client is known to have left: at once when
/// the connection closes, else when a write fails. h3 surfaces a peer's
/// STOP_SENDING only on a write, and h3-quinn does not expose quinn's
/// `stopped()`, so a request cancelled on a connection that stays open is
/// noticed at its next frame, bounded by the provider timeouts. (Probing
/// with empty DATA frames is not an option: the `h3` crate's client reads
/// one as the end of the body.)
async fn quic_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    router: Router,
    remote: SocketAddr,
    connection: quinn::Connection,
) {
    let closed = connection.closed();
    tokio::pin!(closed);
    let Ok((request, stream)) = resolver.resolve_request().await else {
        return;
    };
    let (mut send, mut recv) = stream.split();
    let body = Body::from_stream(async_stream::stream! {
        loop {
            match recv.recv_data().await {
                Ok(Some(mut chunk)) => yield Ok(chunk.copy_to_bytes(chunk.remaining())),
                Ok(None) => break,
                Err(error) => {
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
    });
    let request = direct_request(request.map(|()| body), remote);
    let response = tokio::select! {
        Ok(response) = router.oneshot(request) => response,
        _ = &mut closed => return,
    };
    let (mut head, mut body) = response.into_parts();
    // Connection-specific headers have no meaning in HTTP/3 (RFC 9114 §4.2).
    for name in [
        header::CONNECTION,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        header::HeaderName::from_static("keep-alive"),
        header::HeaderName::from_static("proxy-connection"),
    ] {
        head.headers.remove(name);
    }
    if send
        .send_response(Response::from_parts(head, ()))
        .await
        .is_err()
    {
        return;
    }
    // Returning drops the body, which stops the work behind it.
    loop {
        let frame = tokio::select! {
            frame = body.frame() => frame,
            _ = &mut closed => return,
        };
        let Some(frame) = frame else {
            break;
        };
        let Ok(frame) = frame else {
            send.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
            return;
        };
        let sent = match frame.into_data() {
            Ok(data) => send.send_data(data).await,
            Err(frame) => match frame.into_trailers() {
                Ok(trailers) => send.send_trailers(trailers).await,
                Err(_) => Ok(()),
            },
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = send.finish().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(vars: &[(&str, &str)]) -> Result<ListenerConfig, ListenerConfigError> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        ListenerConfig::from_env(
            &|name: &str| {
                vars.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
                    .ok_or(env::VarError::NotPresent)
            },
            "0.0.0.0:8080".parse().unwrap(),
            Some("0.0.0.0:9464".parse().unwrap()),
        )
    }

    const CERT: [(&str, &str); 2] = [
        ("GATEWAY_TLS_CERT_FILE", "/tls/tls.crt"),
        ("GATEWAY_TLS_KEY_FILE", "/tls/tls.key"),
    ];

    #[test]
    fn listeners_are_off_unless_configured() {
        assert_eq!(load(&[]).unwrap(), ListenerConfig::default());
    }

    #[test]
    fn http3_advertises_its_port_unless_told_otherwise() {
        let config = load(&[CERT[0], CERT[1], ("GATEWAY_HTTP3_ADDR", "0.0.0.0:8443")]).unwrap();
        assert_eq!(config.alt_svc.as_deref(), Some("h3=\":8443\"; ma=86400"));
        assert_eq!(config.tls_addr, None);
        let config = load(&[
            CERT[0],
            CERT[1],
            ("GATEWAY_TLS_ADDR", "0.0.0.0:8443"),
            ("GATEWAY_HTTP3_ADDR", "0.0.0.0:8443"),
            ("GATEWAY_ALT_SVC", "h3=\":443\"; ma=3600"),
        ])
        .unwrap();
        assert_eq!(config.alt_svc.as_deref(), Some("h3=\":443\"; ma=3600"));
        let config = load(&[
            CERT[0],
            CERT[1],
            ("GATEWAY_HTTP3_ADDR", "[::]:8443"),
            ("GATEWAY_ALT_SVC", "off"),
        ])
        .unwrap();
        assert_eq!(config.alt_svc, None);
        // TLS alone advertises nothing.
        let config = load(&[CERT[0], CERT[1], ("GATEWAY_TLS_ADDR", "0.0.0.0:8443")]).unwrap();
        assert_eq!(config.alt_svc, None);
    }

    #[test]
    fn listeners_need_a_certificate_and_their_own_addresses() {
        assert_eq!(
            load(&[("GATEWAY_HTTP3_ADDR", "0.0.0.0:8443")]),
            Err(ListenerConfigError::MissingCertificate {
                name: "GATEWAY_HTTP3_ADDR"
            })
        );
        assert_eq!(
            load(&[CERT[0], ("GATEWAY_TLS_ADDR", "0.0.0.0:8443")]),
            Err(ListenerConfigError::MissingCertificate {
                name: "GATEWAY_TLS_ADDR"
            })
        );
        assert_eq!(load(&CERT), Err(ListenerConfigError::UnusedCertificate));
        assert_eq!(
            load(&[CERT[0], CERT[1], ("GATEWAY_TLS_ADDR", "0.0.0.0:8080")]),
            Err(ListenerConfigError::SameAddr {
                name: "GATEWAY_TLS_ADDR",
                other: "BIND_ADDR"
            })
        );
        // A dual-stack address claims the IPv4 one's port too.
        assert_eq!(
            load(&[CERT[0], CERT[1], ("GATEWAY_TLS_ADDR", "[::]:8080")]),
            Err(ListenerConfigError::SameAddr {
                name: "GATEWAY_TLS_ADDR",
                other: "BIND_ADDR"
            })
        );
        assert_eq!(
            load(&[CERT[0], CERT[1], ("GATEWAY_TLS_ADDR", "0.0.0.0:9464")]),
            Err(ListenerConfigError::SameAddr {
                name: "GATEWAY_TLS_ADDR",
                other: "GATEWAY_METRICS_ADDR"
            })
        );
        assert!(matches!(
            load(&[("GATEWAY_TLS_ADDR", "8443")]),
            Err(ListenerConfigError::InvalidAddr { .. })
        ));
        assert_eq!(
            load(&[
                CERT[0],
                CERT[1],
                ("GATEWAY_HTTP3_ADDR", "0.0.0.0:8443"),
                ("GATEWAY_ALT_SVC", "bad\nvalue"),
            ]),
            Err(ListenerConfigError::InvalidAltSvc)
        );
    }

    /// Installs the test fixture pair `name` as the files the listener reads.
    fn write_certificate(dir: &Path, name: &str) -> Vec<u8> {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
        let cert = fixtures.join(format!("{name}.crt"));
        std::fs::copy(&cert, dir.join("tls.crt")).unwrap();
        std::fs::copy(fixtures.join(format!("{name}.key")), dir.join("tls.key")).unwrap();
        CertificateDer::from_pem_file(cert).unwrap().to_vec()
    }

    #[test]
    fn a_renewed_certificate_replaces_the_one_presented() {
        let dir = std::env::temp_dir().join(format!("gateway-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = write_certificate(&dir, "localhost");
        let files = CertificateFiles::load(&dir.join("tls.crt"), &dir.join("tls.key")).unwrap();
        assert_eq!(files.read().0.cert[0].as_ref(), &first[..]);
        assert!(!files.reload_if_changed());

        // Renewed: a new pair, written later.
        std::thread::sleep(Duration::from_millis(20));
        let second = write_certificate(&dir, "renewed");
        assert!(files.reload_if_changed());
        assert_eq!(files.read().0.cert[0].as_ref(), &second[..]);

        // A half-written renewal keeps the current certificate.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("tls.key"), "not a key").unwrap();
        assert!(!files.reload_if_changed());
        assert_eq!(files.read().0.cert[0].as_ref(), &second[..]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_certificate_fails_at_startup() {
        let missing = Path::new("/nonexistent/tls.crt");
        assert!(matches!(
            CertificateFiles::load(missing, missing),
            Err(ListenerError::Certificate(_))
        ));
    }
}
