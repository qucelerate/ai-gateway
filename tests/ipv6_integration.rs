//! The gateway's listeners serve IPv6 clients (D-026): bound to `[::1]`, and
//! bound dual-stack to `[::]`, where IPv4 clients are served too. Skipped
//! where the host has no IPv6 loopback.

use std::net::SocketAddr;

async fn serve(address: &str) -> Option<SocketAddr> {
    let listener = match gateway::net::bind_listener(address.parse().unwrap()) {
        Ok(listener) => listener,
        // Only a host without IPv6 skips; any other failure is a failure.
        Err(error) if gateway::net::ipv6_unavailable(&error) => {
            eprintln!("skipping: no IPv6 to bind {address}: {error}");
            return None;
        }
        Err(error) => panic!("cannot bind {address}: {error}"),
    };
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, gateway::metrics::router())
            .await
            .unwrap();
    });
    Some(bound)
}

async fn status(url: &str) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn a_listener_on_ipv6_loopback_serves_requests() {
    let Some(bound) = serve("[::1]:0").await else {
        return;
    };
    assert!(bound.is_ipv6());
    assert_eq!(
        status(&format!("http://[::1]:{}/metrics", bound.port())).await,
        200
    );
}

#[tokio::test]
async fn a_dual_stack_listener_serves_both_families() {
    let Some(bound) = serve("[::]:0").await else {
        return;
    };
    let port = bound.port();
    assert_eq!(
        status(&format!("http://127.0.0.1:{port}/metrics")).await,
        200
    );
    if bound.is_ipv6() && tokio::net::TcpStream::connect(("::1", port)).await.is_ok() {
        assert_eq!(status(&format!("http://[::1]:{port}/metrics")).await, 200);
    }
}
