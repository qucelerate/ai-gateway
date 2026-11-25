//! A stream whose response never starts fails with `Error::Timeout`.

use std::time::{Duration, Instant};

use qucelerate_ai_gateway::{ChatRequest, Client, Error, Message};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn a_stream_that_never_starts_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 65536];
        let _ = socket.read(&mut buffer).await;
        // Hold the connection open without answering.
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });
    let client = Client::builder(url)
        .api_key("gw_key")
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let request = ChatRequest::new("m", vec![Message::user("hi")]);
    let started = Instant::now();
    let result = client.chat_stream(&request).await;
    assert!(
        matches!(result, Err(Error::Timeout(_))),
        "{:?}",
        result.err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}
