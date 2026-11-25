//! Files and batches calls: the request each sends and how it decodes.

use qucelerate_ai_gateway::{BatchEndpoint, BatchRequest, Client, Error, RequestOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Serve one response with `status` and `body`; hand back the request seen.
async fn serve(status: u16, body: &str) -> (Client, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_owned();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = vec![0; 65536];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request).into_owned();
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if rest.len() >= length {
                    let _ = sender.send(text);
                    break;
                }
            }
        }
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
    });
    let client = Client::builder(url).api_key("gw_key").build().unwrap();
    (client, receiver)
}

const BATCH: &str = r#"{"id":"batch_1","object":"batch","endpoint":"/v1/chat/completions","input_file_id":"file_1","completion_window":"24h","status":"completed","output_file_id":"file_2","error_file_id":null,"created_at":1,"expires_at":2,"request_counts":{"total":2,"completed":2,"failed":0},"metadata":{},"gateway":{"jobs":[{"mode":"native"}]},"new_field":1}"#;

#[tokio::test]
async fn a_batch_is_created_with_an_idempotency_key() {
    let (client, seen) = serve(200, BATCH).await;
    let request = BatchRequest::new("file_1", BatchEndpoint::ChatCompletions).metadata("run", "a");
    let batch = client
        .create_batch(&request, RequestOptions::default().idempotency_key("k-1"))
        .await
        .unwrap();
    assert!(batch.done());
    assert_eq!(batch.request_counts.completed, 2);
    assert_eq!(batch.gateway.unwrap()["jobs"][0]["mode"], "native");
    let seen = seen.await.unwrap();
    assert!(seen.starts_with("POST /api/inference/v1/batches HTTP/1.1"));
    assert!(seen.to_ascii_lowercase().contains("idempotency-key: k-1"));
    assert!(seen.ends_with(r#"{"input_file_id":"file_1","endpoint":"/v1/chat/completions","completion_window":"24h","metadata":{"run":"a"}}"#));
}

#[tokio::test]
async fn a_file_is_uploaded_as_multipart() {
    let (client, seen) = serve(
        200,
        r#"{"id":"file_1","object":"file","bytes":3,"filename":"b.jsonl","purpose":"batch","created_at":1}"#,
    )
    .await;
    let file = client
        .upload_batch_file("b.jsonl", b"{}\n".to_vec())
        .await
        .unwrap();
    assert_eq!(file.id, "file_1");
    let seen = seen.await.unwrap();
    assert!(seen.contains("multipart/form-data; boundary="));
    assert!(seen.contains("name=\"purpose\"\r\n\r\nbatch\r\n"));
    assert!(seen.contains("filename=\"b.jsonl\""));
    assert!(!seen.to_ascii_lowercase().contains("idempotency-key"));
}

#[tokio::test]
async fn cancel_list_and_content_use_their_paths() {
    let (client, seen) = serve(200, BATCH).await;
    client.cancel_batch("batch_1").await.unwrap();
    assert!(
        seen.await
            .unwrap()
            .starts_with("POST /api/inference/v1/batches/batch_1/cancel ")
    );
    let (client, seen) = serve(
        200,
        &format!(r#"{{"object":"list","data":[{BATCH}],"has_more":true}}"#),
    )
    .await;
    let list = client.list_batches(Some("batch_0"), Some(5)).await.unwrap();
    assert!(list.has_more && list.data.len() == 1);
    assert!(
        seen.await
            .unwrap()
            .starts_with("GET /api/inference/v1/batches?after=batch_0&limit=5 ")
    );
    let (client, seen) = serve(200, "{\"custom_id\":\"a\"}\n").await;
    assert_eq!(
        &client.file_content("file_2").await.unwrap()[..],
        b"{\"custom_id\":\"a\"}\n"
    );
    assert!(
        seen.await
            .unwrap()
            .starts_with("GET /api/inference/v1/files/file_2/content ")
    );
}

#[tokio::test]
async fn an_id_cannot_name_another_path() {
    let client = Client::builder("http://127.0.0.1:9")
        .api_key("gw_key")
        .build()
        .unwrap();
    assert!(matches!(
        client.batch("../files").await,
        Err(Error::Config(_))
    ));
}
