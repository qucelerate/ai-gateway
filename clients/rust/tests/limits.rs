//! A successful body over the response limit fails as a decode error, and
//! an error body is read only up to 1 MiB.

use qucelerate_ai_gateway::{ChatRequest, Client, Error, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one response with `status`, a JSON content type, and `body`.
async fn serve(status: u16, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 65536];
        let _ = socket.read(&mut buffer).await;
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&body).await;
    });
    url
}

fn hello() -> ChatRequest {
    ChatRequest::new("m", vec![Message::user("hi")])
}

#[tokio::test]
async fn a_body_over_the_response_limit_is_a_decode_error() {
    let body = format!("{{\"id\":\"{}\"}}", "x".repeat(4096)).into_bytes();
    let client = Client::builder(serve(200, body).await)
        .api_key("gw_key")
        .max_response_bytes(1024)
        .build()
        .unwrap();
    match client.chat(&hello()).await {
        Err(Error::Decode(message)) => assert!(message.contains("1024"), "{message}"),
        other => panic!("expected a decode error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_huge_error_body_is_cut_at_one_mebibyte() {
    // A problem that only parses whole: cut at 1 MiB it is not a problem.
    let body = format!("{{\"code\":\"x\",\"detail\":\"{}\"}}", "y".repeat(2 << 20)).into_bytes();
    let client = Client::builder(serve(500, body).await)
        .api_key("gw_key")
        .build()
        .unwrap();
    match client.chat(&hello()).await {
        Err(Error::Http { status: 500, .. }) => {}
        other => panic!("expected an HTTP error, got {other:?}"),
    }
}

#[test]
fn a_zero_limit_is_rejected_when_building() {
    let result = Client::builder("https://gateway.example.com")
        .api_key("gw_key")
        .max_response_bytes(0)
        .build();
    assert!(matches!(result, Err(Error::Config(_))));
}
