//! The API's protocols end to end (D-027): HTTP/1.1 and h2c on the plain
//! listener, HTTP/2 and HTTP/1.1 by ALPN on the TLS listener, and HTTP/3 on
//! the QUIC listener, all serving the same router with a certificate made
//! here. Also the provider side: the gateway's HTTP client negotiates
//! HTTP/2 with a provider that offers it.
//!
//! The last test streams a completion through the gateway against a fake
//! OpenAI provider over every protocol and needs `GATEWAY_TEST_DATABASE_URL`.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Request},
    http::{StatusCode, Version},
    response::Response,
    routing::{get, post},
};
use bytes::{Buf, Bytes};
use gateway::listeners::{self, CertificateFiles, DirectConnection, ListenerConfig};
use rustls_pki_types::pem::PemObject;
use serde_json::{Value, json};
use tokio::sync::watch;

struct Tls {
    files: Arc<CertificateFiles>,
    der: Vec<u8>,
}

/// The committed test certificate for `localhost` (tests/fixtures/tls).
fn certificate() -> Tls {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
    let cert = fixtures.join("localhost.crt");
    let files = CertificateFiles::load(&cert, &fixtures.join("localhost.key")).unwrap();
    let der = rustls_pki_types::CertificateDer::from_pem_file(&cert)
        .unwrap()
        .to_vec();
    Tls { files, der }
}

/// The three listeners serving `router`, as the process runs them.
struct Servers {
    plain: SocketAddr,
    tls: SocketAddr,
    quic: SocketAddr,
    der: Vec<u8>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Servers {
    async fn start(router: Router) -> Self {
        let tls = certificate();
        let plain_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listeners::bind_quic("127.0.0.1:0".parse().unwrap(), &tls.files).unwrap();
        let (plain, tls_addr, quic) = (
            plain_listener.local_addr().unwrap(),
            tls_listener.local_addr().unwrap(),
            endpoint.local_addr().unwrap(),
        );
        let config = ListenerConfig {
            tls_addr: Some(tls_addr),
            http3_addr: Some(quic),
            alt_svc: Some(format!("h3=\":{}\"; ma=86400", quic.port())),
            ..ListenerConfig::default()
        };
        let router = listeners::with_alt_svc(router, &config);
        let (shutdown, stop) = watch::channel(false);
        let service = router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        let plain_stop = stop.clone();
        let tasks = vec![
            tokio::spawn(async move {
                let mut stop = plain_stop;
                axum::serve(plain_listener, service)
                    .with_graceful_shutdown(async move { listeners::wait_for(&mut stop).await })
                    .await
                    .unwrap();
            }),
            tokio::spawn({
                let (files, router, stop) = (tls.files.clone(), router.clone(), stop.clone());
                async move {
                    listeners::serve_tls(tls_listener, files, router, stop)
                        .await
                        .unwrap()
                }
            }),
            tokio::spawn(
                async move { listeners::serve_quic(endpoint, router, stop).await.unwrap() },
            ),
        ];
        Self {
            plain,
            tls: tls_addr,
            quic,
            der: tls.der.clone(),
            shutdown,
            tasks,
        }
    }

    /// A client trusting the test certificate; HTTP/2 by ALPN unless
    /// `http1`.
    fn tls_client(&self, http1: bool) -> reqwest::Client {
        let builder = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_der(&self.der).unwrap());
        let builder = if http1 { builder.http1_only() } else { builder };
        builder.build().unwrap()
    }

    fn tls_url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.tls.port())
    }

    async fn h3(&self) -> H3Client {
        H3Client::connect(self.quic, &self.der).await
    }

    /// Stops every listener and waits for them to finish.
    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        for task in self.tasks {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("listener stops after shutdown")
                .unwrap();
        }
    }
}

/// A minimal HTTP/3 client.
struct H3Client {
    send: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    _endpoint: quinn::Endpoint,
}

struct H3Response {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
}

impl H3Response {
    async fn chunk(&mut self) -> Option<Bytes> {
        let mut chunk = self.stream.recv_data().await.unwrap()?;
        Some(chunk.copy_to_bytes(chunk.remaining()))
    }

    async fn text(mut self) -> String {
        let mut body = Vec::new();
        while let Some(chunk) = self.chunk().await {
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).unwrap()
    }
}

impl H3Client {
    async fn connect(address: SocketAddr, der: &[u8]) -> Self {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls_pki_types::CertificateDer::from(der.to_vec()))
            .unwrap();
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        let connection = endpoint
            .connect(address, "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(connection))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });
        Self {
            send,
            _endpoint: endpoint,
        }
    }

    async fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Bytes>,
    ) -> H3Response {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(format!("https://localhost{path}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut stream = self
            .send
            .send_request(request.body(()).unwrap())
            .await
            .unwrap();
        if let Some(body) = body {
            stream.send_data(body).await.unwrap();
        }
        stream.finish().await.unwrap();
        let response = stream.recv_response().await.unwrap();
        H3Response {
            status: response.status(),
            headers: response.headers().clone(),
            stream,
        }
    }
}

/// Echoes the request: its protocol, whether it came straight from the
/// client, the peer's address and the body's length.
async fn echo(request: Request) -> Json<Value> {
    let version = format!("{:?}", request.version());
    let direct = request.extensions().get::<DirectConnection>().is_some();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip().to_string());
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap();
    Json(json!({"version": version, "direct": direct, "peer": peer, "length": body.len()}))
}

/// Sets its flag when dropped: the response body was abandoned or finished.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Three server-sent events 150 ms apart; `ended` is set once the body is
/// dropped.
fn slow(ended: Arc<AtomicBool>) -> Response {
    let stream = async_stream::stream! {
        let _guard = Dropped(ended);
        for index in 0..3 {
            yield Ok::<_, std::convert::Infallible>(Bytes::from(format!("data: {index}\n\n")));
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Events every 50 ms until the client goes away; `ended` is set once the
/// body is dropped.
fn endless(ended: Arc<AtomicBool>) -> Response {
    let stream = async_stream::stream! {
        let _guard = Dropped(ended);
        loop {
            yield Ok::<_, std::convert::Infallible>(Bytes::from_static(b"data: tick\n\n"));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// One event, then nothing for a minute, as a model thinking; `ended` is
/// set once the body is dropped.
fn stalled(ended: Arc<AtomicBool>) -> Response {
    let stream = async_stream::stream! {
        let _guard = Dropped(ended);
        yield Ok::<_, std::convert::Infallible>(Bytes::from_static(b"data: first\n\n"));
        tokio::time::sleep(Duration::from_secs(60)).await;
        yield Ok(Bytes::from_static(b"data: late\n\n"));
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Two events 4.5 seconds apart.
fn paused() -> Response {
    let stream = async_stream::stream! {
        yield Ok::<_, std::convert::Infallible>(Bytes::from_static(b"data: before\n\n"));
        tokio::time::sleep(Duration::from_millis(4500)).await;
        yield Ok(Bytes::from_static(b"data: after\n\n"));
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn test_router(ended: Arc<AtomicBool>) -> Router {
    let endless_ended = ended.clone();
    let stalled_ended = ended.clone();
    Router::new()
        .route("/paused", get(|| async { paused() }))
        .route(
            "/stalled",
            get(move || async move { stalled(stalled_ended.clone()) }),
        )
        .route(
            "/endless",
            get(move || async move { endless(endless_ended.clone()) }),
        )
        .route("/echo", post(echo).get(echo))
        .route(
            "/limited",
            post(|body: Bytes| async move { body.len().to_string() })
                .layer(DefaultBodyLimit::max(1024)),
        )
        .route("/slow", get(move || async move { slow(ended.clone()) }))
}

#[tokio::test]
async fn every_listener_serves_the_same_router_over_its_protocols() {
    let servers = Servers::start(test_router(Arc::default())).await;
    let large = Bytes::from(vec![b'x'; 300_000]);

    // The plain listener: HTTP/1.1, and HTTP/2 with prior knowledge as an
    // ingress may speak it. Not direct: a proxy stands in front.
    let plain = format!("http://{}/echo", servers.plain);
    let body: Value = reqwest::get(&plain).await.unwrap().json().await.unwrap();
    assert_eq!(body["version"], "HTTP/1.1");
    assert_eq!(body["direct"], false);
    let h2c = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let response = h2c.post(&plain).body(large.clone()).send().await.unwrap();
    assert_eq!(response.version(), Version::HTTP_2);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        (body["version"].as_str(), body["length"].as_u64()),
        (Some("HTTP/2.0"), Some(300_000))
    );

    // TLS: HTTP/2 by ALPN, HTTP/1.1 for a client that offers only that,
    // each told where HTTP/3 is.
    let alt_svc = format!("h3=\":{}\"; ma=86400", servers.quic.port());
    for (http1, version) in [(false, "HTTP/2.0"), (true, "HTTP/1.1")] {
        let response = servers
            .tls_client(http1)
            .post(servers.tls_url("/echo"))
            .body(large.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["alt-svc"], alt_svc.as_str());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["version"], version);
        assert_eq!(body["direct"], true);
        assert_eq!(body["peer"], "127.0.0.1");
        assert_eq!(body["length"], 300_000);
    }

    // HTTP/3, with a body larger than one QUIC flow-control window.
    let mut h3 = servers.h3().await;
    let response = h3.request("POST", "/echo", &[], Some(large)).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers["alt-svc"], alt_svc.as_str());
    let body: Value = serde_json::from_str(&response.text().await).unwrap();
    assert_eq!(body["version"], "HTTP/3.0");
    assert_eq!(body["direct"], true);
    assert_eq!(body["peer"], "127.0.0.1");
    assert_eq!(body["length"], 300_000);

    // Body limits apply alike.
    let response = h3
        .request("POST", "/limited", &[], Some(Bytes::from(vec![0; 4096])))
        .await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    let response = servers
        .tls_client(false)
        .post(servers.tls_url("/limited"))
        .body(vec![0; 4096])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    servers.stop().await;
}

#[tokio::test]
async fn streams_arrive_as_they_are_written_and_a_closed_stream_stops_the_work() {
    let ended = Arc::new(AtomicBool::new(false));
    let servers = Servers::start(test_router(ended.clone())).await;

    // HTTP/2 over TLS: the first event arrives well before the last is
    // written (300 ms later), not buffered with it.
    let mut response = servers
        .tls_client(false)
        .get(servers.tls_url("/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), Version::HTTP_2);
    let first = response.chunk().await.unwrap().unwrap();
    assert_eq!(&first[..], b"data: 0\n\n");
    let first_at = std::time::Instant::now();
    let mut rest = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(&rest[..], b"data: 1\n\ndata: 2\n\n");
    assert!(first_at.elapsed() >= Duration::from_millis(200));

    // HTTP/3 likewise.
    let mut h3 = servers.h3().await;
    let mut response = h3.request("GET", "/slow", &[], None).await;
    assert_eq!(&response.chunk().await.unwrap()[..], b"data: 0\n\n");
    let first_at = std::time::Instant::now();
    assert_eq!(&response.chunk().await.unwrap()[..], b"data: 1\n\n");
    assert!(first_at.elapsed() >= Duration::from_millis(100));

    // The client abandons a stream that never ends by itself: the server
    // drops the body behind it, as when an HTTP/1.1 client disconnects.
    // (Dropping the stream stops it: quinn sends STOP_SENDING.)
    drop(response);
    for _ in 0..40 {
        if ended.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ended.store(false, Ordering::SeqCst);
    let mut response = h3.request("GET", "/endless", &[], None).await;
    assert_eq!(&response.chunk().await.unwrap()[..], b"data: tick\n\n");
    assert!(!ended.load(Ordering::SeqCst));
    drop(response);
    let mut stopped = false;
    for _ in 0..40 {
        if ended.load(Ordering::SeqCst) {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        stopped,
        "the abandoned HTTP/3 stream's body was not dropped"
    );

    servers.stop().await;
}

#[tokio::test]
async fn a_quiet_stream_lives_while_its_client_does() {
    let ended = Arc::new(AtomicBool::new(false));
    let servers = Servers::start(test_router(ended.clone())).await;

    // A stream quiet for seconds, as a model thinking, reaches its end.
    let mut h3 = servers.h3().await;
    let response = h3.request("GET", "/paused", &[], None).await;
    assert_eq!(response.text().await, "data: before\n\ndata: after\n\n");

    // The client's connection going away ends a quiet stream without
    // waiting for its next event.
    let mut response = h3.request("GET", "/stalled", &[], None).await;
    assert_eq!(&response.chunk().await.unwrap()[..], b"data: first\n\n");
    drop(response);
    drop(h3);
    let started = std::time::Instant::now();
    while !ended.load(Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the closed connection's quiet stream was not dropped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    servers.stop().await;
}

#[tokio::test]
async fn shutdown_lets_streams_in_flight_finish() {
    let servers = Servers::start(test_router(Arc::default())).await;
    let mut h3 = servers.h3().await;
    let mut quic = h3.request("GET", "/slow", &[], None).await;
    let mut tls = servers
        .tls_client(false)
        .get(servers.tls_url("/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(&quic.chunk().await.unwrap()[..], b"data: 0\n\n");
    assert_eq!(&tls.chunk().await.unwrap().unwrap()[..], b"data: 0\n\n");

    let plain = servers.plain;
    let stopping = tokio::spawn(servers.stop());
    let mut rest = Vec::new();
    while let Some(chunk) = quic.chunk().await {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(&rest[..], b"data: 1\n\ndata: 2\n\n");
    let mut rest = Vec::new();
    while let Some(chunk) = tls.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(&rest[..], b"data: 1\n\ndata: 2\n\n");
    stopping.await.unwrap();
    // Every listener has stopped.
    assert!(reqwest::get(format!("http://{plain}/echo")).await.is_err());
}

/// A provider that offers HTTP/2 is called over it, its events streamed as
/// they come, and an idle read still times out.
#[tokio::test]
async fn provider_calls_negotiate_http2_and_keep_their_timeouts() {
    let provider = Router::new()
        .route("/slow", get(|| async { slow(Arc::default()) }))
        .route(
            "/stall",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        );
    let servers = Servers::start(provider).await;
    // As the provider adapters build their clients, plus the test's root.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_millis(500))
        .add_root_certificate(reqwest::Certificate::from_der(&servers.der).unwrap())
        .build()
        .unwrap();
    let mut response = client.get(servers.tls_url("/slow")).send().await.unwrap();
    assert_eq!(response.version(), Version::HTTP_2);
    let mut events = 0;
    while let Some(chunk) = response.chunk().await.unwrap() {
        events += chunk
            .windows(6)
            .filter(|window| window == b"data: ")
            .count();
    }
    assert_eq!(events, 3);
    let error = client
        .get(servers.tls_url("/stall"))
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout(), "{error}");
    servers.stop().await;
}

// The gateway itself: a streamed completion against a fake OpenAI provider
// over HTTP/1.1, h2c, HTTP/2 and HTTP/3.

const PEPPER: &str = "protocols-test-pepper-000000000000";

async fn provider(Json(body): Json<Value>) -> Response {
    let usage = json!({"prompt_tokens": 5, "completion_tokens": 7});
    assert_eq!(body["stream"], true);
    let chunk = |value: Value| format!("data: {value}\n\n");
    let text = [
        chunk(json!({"id": "c", "model": "m", "choices": [
            {"index": 0, "delta": {"content": "hello"}, "finish_reason": null}]})),
        chunk(json!({"id": "c", "model": "m", "choices": [
            {"index": 0, "delta": {}, "finish_reason": "stop"}]})),
        chunk(json!({"id": "c", "model": "m", "choices": [], "usage": usage})),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(text))
        .unwrap()
}

async fn seed(pool: &sqlx::PgPool) -> (uuid::Uuid, String) {
    use gateway::{
        ApiKeyPepper, CatalogManifest, CatalogStore, IdentityStore,
        domain::identity::{ApiKeyScope, MembershipRole},
    };
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("protocols-{suffix}"), "Protocols test")
        .await
        .unwrap()
        .id;
    let user = identity
        .create_user(org, None, None, None, Some("organization_owner"))
        .await
        .unwrap()
        .id;
    identity
        .add_organization_membership(org, user, MembershipRole::Owner)
        .await
        .unwrap();
    let account = identity
        .create_service_account(org, user, None, "protocols")
        .await
        .unwrap()
        .id;
    let bearer = identity
        .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
        .await
        .unwrap()
        .bearer;
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, scope_kind, period_start, period_end, currency, limit_minor)
         VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '1 day',
                 'USD', 1000000000000)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    let capability = json!({
        "operation": "chat_stream", "max_input_bytes": 4096, "max_output_tokens": 64,
        "max_tokens_per_byte": 1, "tokenizer": "configured", "reservation_ttl_seconds": 60,
        "chat_input_count_mode": "unsupported"
    });
    let manifest: CatalogManifest = serde_json::from_value(json!({
        "release": {"version": "protocols-1"},
        "providers": [{"adapter_instance_key": "openai-main", "family": "openai"}],
        "deployments": [{"name": "chat-a", "model": "gpt-protocols", "location": null,
                         "adapter_instance_key": "openai-main", "capabilities": [capability]}],
        "prices": [{"deployment": "chat-a", "operation": "chat_stream",
                    "input_minor_per_token": 3, "output_minor_per_token": 11,
                    "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
                    "other_minor_per_token": 0, "currency": "USD", "source": "protocols-test",
                    "source_version": "v1", "confidence": "known"}],
        "routes": [{"alias": "chat", "operation": "chat_stream", "deployment": "chat-a"}]
    }))
    .unwrap();
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    (org.0, bearer)
}

#[tokio::test]
async fn the_gateway_streams_a_completion_over_every_protocol() {
    let Ok(database_url) = std::env::var("GATEWAY_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "GATEWAY_TEST_DATABASE_URL must be set in CI"
        );
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let (org, bearer) = seed(&pool).await;

    let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_base = format!("http://{}", provider_listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            provider_listener,
            Router::new().route("/v1/chat/completions", post(provider)),
        )
        .await
        .unwrap()
    });
    let vars = [
        ("DATABASE_URL", database_url.clone()),
        ("GATEWAY_PROFILE", "api".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-protocols".to_owned()),
        ("OPENAI_BASE_URL", format!("{provider_base}/v1")),
    ];
    let config = gateway::AppConfig::from_lookup(gateway::GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config).await.unwrap();
    let servers = Servers::start(gateway::transport::router(state)).await;

    let request = |stream_id: usize| {
        json!({
            "model": "chat",
            "messages": [{"role": "user", "content": format!("hello {stream_id}")}],
            "max_tokens": 16
        })
        .to_string()
    };
    let path = "/api/inference/v1/chat/completions/stream";
    let authorization = format!("Bearer {bearer}");
    let h2c = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let clients = [
        (
            reqwest::Client::new(),
            format!("http://{}{path}", servers.plain),
            Version::HTTP_11,
        ),
        (
            h2c,
            format!("http://{}{path}", servers.plain),
            Version::HTTP_2,
        ),
        (
            servers.tls_client(false),
            servers.tls_url(path),
            Version::HTTP_2,
        ),
        (
            servers.tls_client(true),
            servers.tls_url(path),
            Version::HTTP_11,
        ),
    ];
    for (index, (client, url, version)) in clients.into_iter().enumerate() {
        let response = client
            .post(url)
            .header("authorization", &authorization)
            .header("idempotency-key", uuid::Uuid::new_v4().to_string())
            .header("content-type", "application/json")
            .body(request(index))
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), version);
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("hello"), "{body}");
        assert!(body.contains("event: response.completed"), "{body}");
    }
    let mut h3 = servers.h3().await;
    let idempotency = uuid::Uuid::new_v4().to_string();
    let response = h3
        .request(
            "POST",
            path,
            &[
                ("authorization", &authorization),
                ("idempotency-key", &idempotency),
                ("content-type", "application/json"),
            ],
            Some(Bytes::from(request(9))),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.text().await;
    assert!(body.contains("event: response.completed"), "{body}");

    let settled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.accounting_reservation
         WHERE organization_id = $1 AND state = 'settled'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settled, 5);
    servers.stop().await;
}
