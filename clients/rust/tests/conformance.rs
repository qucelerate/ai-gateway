//! Runs every case of `clients/conformance` against this client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use qucelerate_ai_gateway::{ChatRequest, Client, EmbeddingRequest, Error, RequestOptions};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../conformance")
}

#[derive(Debug)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A one-connection-per-request HTTP/1.1 server that serves the case's
/// responses in order and records what it received.
async fn serve(responses: Vec<Value>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let log = recorded.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending a request");
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position;
                }
            };
            let head = String::from_utf8(buffer[..head_end].to_vec()).unwrap();
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next().unwrap().split(' ');
            let method = request_line.next().unwrap().to_owned();
            let path = request_line.next().unwrap().to_owned();
            let headers: Vec<(String, String)> = lines
                .map(|line| {
                    let (name, value) = line.split_once(':').unwrap();
                    (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                })
                .collect();
            let length: usize = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map(|(_, value)| value.parse().unwrap())
                .unwrap_or(0);
            let mut body = buffer[head_end + 4..].to_vec();
            while body.len() < length {
                let read = socket.read(&mut chunk).await.unwrap();
                body.extend_from_slice(&chunk[..read]);
            }
            log.lock().unwrap().push(Recorded {
                method,
                path,
                headers,
                body,
            });

            let status = response["status"].as_u64().unwrap();
            let (content_type, payload) = if let Some(json) = response.get("json") {
                let kind = if status >= 400 {
                    "application/problem+json"
                } else {
                    "application/json"
                };
                (kind.to_owned(), serde_json::to_vec(json).unwrap())
            } else if let Some(file) = response.get("sse") {
                let path = suite().join("fixtures").join(file.as_str().unwrap());
                ("text/event-stream".to_owned(), std::fs::read(path).unwrap())
            } else {
                (
                    "text/plain".to_owned(),
                    response["text"].as_str().unwrap().as_bytes().to_vec(),
                )
            };
            let mut extra = response["headers"].as_object().cloned().unwrap_or_default();
            let content_type = extra
                .remove("content-type")
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or(content_type);
            let mut out = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
                payload.len()
            );
            for (name, value) in extra {
                out.push_str(&format!("{name}: {}\r\n", value.as_str().unwrap()));
            }
            out.push_str("\r\n");
            socket.write_all(out.as_bytes()).await.unwrap();
            socket.write_all(&payload).await.unwrap();
            socket.shutdown().await.ok();
        }
    });
    (url, recorded)
}

/// The suite's subset match: listed keys must be present and equal; arrays
/// must have the listed length.
fn subset(expected: &Value, actual: &Value, path: &str) {
    match expected {
        Value::Object(fields) => {
            let object = actual
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expected an object, got {actual}"));
            for (key, value) in fields {
                let inner = object
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}.{key}: missing in {actual}"));
                subset(value, inner, &format!("{path}.{key}"));
            }
        }
        Value::Array(items) => {
            let array = actual
                .as_array()
                .unwrap_or_else(|| panic!("{path}: expected an array, got {actual}"));
            assert_eq!(array.len(), items.len(), "{path}: length");
            for (index, (left, right)) in items.iter().zip(array).enumerate() {
                subset(left, right, &format!("{path}[{index}]"));
            }
        }
        _ => assert_eq!(expected, actual, "{path}"),
    }
}

fn check_error(expected: &Value, error: &Error, case: &str) {
    match expected["kind"].as_str().unwrap() {
        "problem" => {
            let problem = error
                .problem()
                .unwrap_or_else(|| panic!("{case}: expected a problem, got {error:?}"));
            assert_eq!(expected["status"], json!(problem.status), "{case}: status");
            assert_eq!(expected["code"], json!(problem.code), "{case}: code");
            if let Some(id) = expected.get("request_id") {
                assert_eq!(id, &json!(problem.request_id), "{case}: request_id");
            }
            if let Some(wait) = expected.get("retry_after") {
                assert_eq!(wait, &json!(problem.retry_after), "{case}: retry_after");
            }
        }
        "http" => match error {
            Error::Http { status, .. } => assert_eq!(expected["status"], json!(status), "{case}"),
            other => panic!("{case}: expected an HTTP error, got {other:?}"),
        },
        "stream" => match error {
            Error::Stream { code, status } => {
                assert_eq!(expected["code"], json!(code), "{case}");
                assert_eq!(expected["status"], json!(status), "{case}");
            }
            other => panic!("{case}: expected a stream error, got {other:?}"),
        },
        "protocol" => assert!(matches!(error, Error::Protocol(_)), "{case}: got {error:?}"),
        kind => panic!("{case}: unknown error kind {kind}"),
    }
}

async fn run(case: Value) {
    let name = case["name"].as_str().unwrap().to_owned();
    let (url, recorded) = serve(case["responses"].as_array().unwrap().clone()).await;
    let sleeps = Arc::new(Mutex::new(Vec::<u64>::new()));
    let sleep_log = sleeps.clone();
    let mut builder = Client::builder(url)
        .api_key("gw_conformance_key")
        .sleep_with(move |duration: Duration| {
            sleep_log.lock().unwrap().push(duration.as_secs());
            async {}
        });
    if let Some(retries) = case["client"]["max_retries"].as_u64() {
        builder = builder.max_retries(retries as u32);
    }
    let client = builder.build().unwrap();

    let call = &case["call"];
    let mut options = RequestOptions::default();
    if let Some(key) = call["idempotency_key"].as_str() {
        options = options.idempotency_key(key);
    }
    if let Some(id) = call["request_id"].as_str() {
        options = options.request_id(id);
    }
    let expect = &case["expect"];
    match case["operation"].as_str().unwrap() {
        "chat" => {
            let request: ChatRequest = serde_json::from_value(call["body"].clone()).unwrap();
            match client.chat_with(&request, options).await {
                Ok(response) => subset(
                    &expect["result"],
                    &serde_json::to_value(&response).unwrap(),
                    &name,
                ),
                Err(error) => check_error(&expect["error"], &error, &name),
            }
        }
        "embeddings" => {
            let request: EmbeddingRequest = serde_json::from_value(call["body"].clone()).unwrap();
            match client.embeddings_with(&request, options).await {
                Ok(response) => subset(
                    &expect["result"],
                    &serde_json::to_value(&response).unwrap(),
                    &name,
                ),
                Err(error) => check_error(&expect["error"], &error, &name),
            }
        }
        "chat_stream" => {
            let request: ChatRequest = serde_json::from_value(call["body"].clone()).unwrap();
            let mut events = Vec::new();
            let mut failure = None;
            match client.chat_stream_with(&request, options).await {
                Ok(mut stream) => {
                    let served = case["responses"]
                        .as_array()
                        .and_then(|responses| responses.last())
                        .and_then(|response| response["headers"]["x-request-id"].as_str());
                    assert_eq!(stream.request_id(), served, "{name}: stream request_id");
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(event) => {
                                events.push(json!({"event": event.name(), "data": event.data()}))
                            }
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                }
                Err(error) => failure = Some(error),
            }
            subset(&expect["events"], &Value::Array(events), &name);
            match (expect.get("error"), failure) {
                (Some(expected), Some(error)) => check_error(expected, &error, &name),
                (None, None) => {}
                (expected, actual) => panic!("{name}: expected error {expected:?}, got {actual:?}"),
            }
        }
        other => panic!("{name}: unknown operation {other}"),
    }

    let recorded = recorded.lock().unwrap();
    let request = &case["expect_request"];
    assert_eq!(
        recorded.len() as u64,
        expect["attempts"].as_u64().unwrap(),
        "{name}: attempts"
    );
    for attempt in recorded.iter() {
        assert_eq!(
            attempt.method,
            request["method"].as_str().unwrap(),
            "{name}"
        );
        assert_eq!(attempt.path, request["path"].as_str().unwrap(), "{name}");
        assert_eq!(
            attempt.header("content-type"),
            Some("application/json"),
            "{name}"
        );
        for (header, value) in request["headers"].as_object().unwrap() {
            assert_eq!(
                attempt.header(header),
                value.as_str(),
                "{name}: header {header}"
            );
        }
        for header in request["absent_headers"].as_array().into_iter().flatten() {
            assert!(
                attempt.header(header.as_str().unwrap()).is_none(),
                "{name}: {header} sent"
            );
        }
        if request["generated_idempotency_key"] == json!(true) {
            let key = attempt.header("idempotency-key").expect("a generated key");
            assert!(
                !key.is_empty() && key.chars().count() <= 255,
                "{name}: key {key}"
            );
        }
        let body: Value = serde_json::from_slice(&attempt.body).unwrap();
        assert_eq!(body, request["body"], "{name}: body");
    }
    let keys: Vec<_> = recorded
        .iter()
        .map(|attempt| attempt.header("idempotency-key"))
        .collect();
    assert!(
        keys.windows(2).all(|pair| pair[0] == pair[1]),
        "{name}: keys differ {keys:?}"
    );
    let expected_sleeps: Vec<u64> = expect["sleeps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap())
        .collect();
    assert_eq!(*sleeps.lock().unwrap(), expected_sleeps, "{name}: sleeps");
}

#[tokio::test]
async fn every_conformance_case_passes() {
    let mut paths: Vec<_> = std::fs::read_dir(suite().join("cases"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    for path in &paths {
        let case: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        run(case).await;
    }
    println!("{} conformance cases passed", paths.len());
}
