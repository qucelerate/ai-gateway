//! The fake provider: OpenAI's chat completions (unary and streamed) and
//! embeddings, answering as each request's directive asks (latency, stream
//! length, output size, trouble). The gateway accepts plain HTTP only to a
//! loopback address, so the fake runs on each gateway host.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{
    args::Flags,
    directive::{Directive, Fault},
};

/// Chunk spacing of a stream.
const CHUNK_INTERVAL: Duration = Duration::from_millis(250);
/// How long a `timeout` fault holds the request: past any provider timeout
/// the gateway allows (300 s at most).
const HANG: Duration = Duration::from_secs(330);

const OPS: [&str; 3] = ["chat", "chat_stream", "embeddings"];

#[derive(Default)]
struct Stats {
    /// Requests by operation and fault, `OPS` by `Fault::ALL`.
    requests: [[AtomicU64; 7]; 3],
    open_streams: AtomicU64,
    open_streams_max: AtomicU64,
}

impl Stats {
    fn count(&self, op: usize, fault: Fault) {
        let fault = Fault::ALL.iter().position(|f| *f == fault).unwrap_or(0);
        self.requests[op][fault].fetch_add(1, Ordering::Relaxed);
    }

    fn json(&self) -> Value {
        let mut requests = serde_json::Map::new();
        for (op, name) in OPS.iter().enumerate() {
            let mut faults = serde_json::Map::new();
            for (index, fault) in Fault::ALL.iter().enumerate() {
                let count = self.requests[op][index].load(Ordering::Relaxed);
                if count > 0 {
                    faults.insert(fault.as_str().to_owned(), count.into());
                }
            }
            requests.insert((*name).to_owned(), faults.into());
        }
        json!({
            "requests": requests,
            "open_streams": self.open_streams.load(Ordering::Relaxed),
            "open_streams_max": self.open_streams_max.load(Ordering::Relaxed),
        })
    }
}

struct Fake {
    stats: Stats,
    default_latency: Duration,
}

type Shared = Arc<Fake>;

pub async fn serve(flags: &Flags) -> Result<(), String> {
    let listen: SocketAddr = flags
        .string("listen", "127.0.0.1:24599")
        .parse()
        .map_err(|_| "--listen: not an address".to_owned())?;
    let fake = Arc::new(Fake {
        stats: Stats::default(),
        default_latency: flags.duration("default-latency", Duration::from_millis(300))?,
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/embeddings", post(embeddings))
        .route("/__stats", get(stats))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("bind {listen}: {error}"))?;
    eprintln!("fake provider listening on http://{listen}");
    axum::serve(listener, app)
        .await
        .map_err(|error| error.to_string())
}

async fn stats(State(fake): State<Shared>) -> Json<Value> {
    Json(fake.stats.json())
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        // Content parts: the text of each.
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// A token for every 4 bytes, at least one.
fn tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4).max(1)
}

fn error(status: StatusCode, kind: &str, retry_after: Option<u64>) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(seconds) = retry_after {
        builder = builder.header("retry-after", seconds.to_string());
    }
    builder
        .body(Body::from(
            json!({"error": {"message": "injected by the load test", "type": kind, "code": kind}})
                .to_string(),
        ))
        .unwrap()
}

/// The refusal a fault calls for before any output, if any.
fn refusal(directive: &Directive, model: &str) -> Option<Response> {
    let first_deployment = model.ends_with("-a");
    match directive.fault {
        Fault::RateLimited if first_deployment => Some(error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            Some(directive.retry_after),
        )),
        Fault::RateLimitedAll => Some(error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            Some(directive.retry_after),
        )),
        Fault::ServerError => Some(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            None,
        )),
        Fault::Unavailable => Some(error(StatusCode::SERVICE_UNAVAILABLE, "server_error", None)),
        _ => None,
    }
}

/// A body that sends `head` and then fails, so the connection is cut.
fn cut(head: String) -> Body {
    let stream = futures::stream::iter([
        Ok::<Bytes, std::io::Error>(Bytes::from(head)),
        Err(std::io::Error::other("injected drop")),
    ]);
    Body::from_stream(stream)
}

async fn chat(State(fake): State<Shared>, Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("model").to_owned();
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let input_bytes: usize = messages.iter().map(|m| text_of(&m["content"]).len()).sum();
    let last = messages
        .last()
        .map(|m| text_of(&m["content"]))
        .unwrap_or_default();
    let stream = body["stream"] == true;
    let max_tokens = body["max_tokens"]
        .as_u64()
        .or_else(|| body["max_completion_tokens"].as_u64())
        .unwrap_or(16);
    let directive = Directive::parse(&last).unwrap_or(Directive {
        out: max_tokens.min(16),
        lat_ms: fake.default_latency.as_millis() as u64,
        ttfb_ms: fake.default_latency.as_millis() as u64,
        dur_ms: 0,
        fault: Fault::None,
        retry_after: 1,
    });
    let out = directive.out.clamp(1, max_tokens.max(1));
    let usage = json!({"prompt_tokens": tokens(input_bytes), "completion_tokens": out,
                       "total_tokens": tokens(input_bytes) + out});
    fake.stats
        .count(if stream { 1 } else { 0 }, directive.fault);

    let wait = Duration::from_millis(if stream {
        directive.ttfb_ms
    } else {
        directive.lat_ms
    });
    tokio::time::sleep(wait).await;
    if directive.fault == Fault::Timeout {
        tokio::time::sleep(HANG).await;
    }
    if let Some(response) = refusal(&directive, &model) {
        return response;
    }

    if !stream {
        let text = json!({
            "id": "chatcmpl-load", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "tok ".repeat(out.min(4096) as usize)},
                         "finish_reason": "stop"}],
            "usage": usage,
        })
        .to_string();
        let body = if directive.fault == Fault::Dropped {
            cut(text[..text.len() / 2].to_owned())
        } else {
            Body::from(text)
        };
        return Response::builder()
            .header("content-type", "application/json")
            .header("x-request-id", "fake-load")
            .body(body)
            .unwrap();
    }

    let chunks = (directive.dur_ms / CHUNK_INTERVAL.as_millis() as u64).max(1);
    let per_chunk = out / chunks;
    let remainder = out % chunks;
    let drop_after = (directive.fault == Fault::Dropped).then_some(chunks / 2);
    let shared = fake.clone();
    let events = async_stream::stream! {
        let open = shared.stats.open_streams.fetch_add(1, Ordering::Relaxed) + 1;
        shared.stats.open_streams_max.fetch_max(open, Ordering::Relaxed);
        let _guard = OpenStream(shared.clone());
        let frame = |value: Value| Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {value}\n\n")));
        for chunk in 0..chunks {
            if drop_after == Some(chunk) {
                yield Err(std::io::Error::other("injected drop"));
                return;
            }
            if chunk > 0 {
                tokio::time::sleep(CHUNK_INTERVAL).await;
            }
            let count = per_chunk + u64::from(chunk < remainder);
            yield frame(json!({"id": "chatcmpl-load", "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"content": "tok ".repeat(count.min(4096) as usize)},
                             "finish_reason": null}]}));
        }
        yield frame(json!({"id": "chatcmpl-load", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}));
        yield frame(json!({"id": "chatcmpl-load", "object": "chat.completion.chunk", "model": model,
            "choices": [], "usage": usage}));
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("x-request-id", "fake-load")
        .body(Body::from_stream(events))
        .unwrap()
}

/// Counts a stream as open until its body is dropped, however it ends.
struct OpenStream(Shared);

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.0.stats.open_streams.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn embeddings(State(fake): State<Shared>, Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("embed").to_owned();
    let inputs: Vec<String> = body["input"]
        .as_array()
        .map(|values| values.iter().map(text_of).collect())
        .unwrap_or_else(|| vec![text_of(&body["input"])]);
    let directive = inputs
        .first()
        .and_then(|first| Directive::parse(first))
        .unwrap_or(Directive {
            lat_ms: fake.default_latency.as_millis() as u64,
            retry_after: 1,
            ..Directive::default()
        });
    fake.stats.count(2, directive.fault);
    tokio::time::sleep(Duration::from_millis(directive.lat_ms)).await;
    if directive.fault == Fault::Timeout {
        tokio::time::sleep(HANG).await;
    }
    if let Some(response) = refusal(&directive, &model) {
        return response;
    }
    let prompt: u64 = inputs.iter().map(|input| tokens(input.len())).sum();
    let text = json!({
        "object": "list", "model": model,
        "data": inputs.iter().enumerate().map(|(index, _)| json!({
            "object": "embedding", "index": index,
            "embedding": [0.125, -0.25, 0.5, 0.0625, -0.5, 0.25, 0.75, -0.125]
        })).collect::<Vec<_>>(),
        "usage": {"prompt_tokens": prompt, "total_tokens": prompt},
    })
    .to_string();
    let body = if directive.fault == Fault::Dropped {
        cut(text[..text.len() / 2].to_owned())
    } else {
        Body::from(text)
    };
    Response::builder()
        .header("content-type", "application/json")
        .header("x-request-id", "fake-load")
        .body(body)
        .unwrap()
}
