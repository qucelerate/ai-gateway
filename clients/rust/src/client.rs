use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Problem};
use crate::sse::{MAX_EVENT_BYTES, Parser, RawEvent};
use crate::types::{
    Batch, BatchList, BatchRequest, ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse,
    FileObject, StreamEvent,
};

const CHAT_PATH: &str = "/api/inference/v1/chat/completions";
const STREAM_PATH: &str = "/api/inference/v1/chat/completions/stream";
const EMBEDDINGS_PATH: &str = "/api/inference/v1/embeddings";
const FILES_PATH: &str = "/api/inference/v1/files";
const BATCHES_PATH: &str = "/api/inference/v1/batches";
const REQUEST_ID: &str = "x-request-id";
const IDEMPOTENCY_KEY: &str = "idempotency-key";
const MAX_WAIT_SECONDS: u64 = 60;
const MAX_KEY_CHARS: usize = 255;
const MAX_REQUEST_ID_CHARS: usize = 128;
const MAX_ERROR_BODY_BYTES: usize = 1 << 20;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 << 20;

/// Problem codes for which the gateway refused the request before reserving
/// anything, so the same `Idempotency-Key` is still unused and a retry is
/// safe (`retry: same_key` in `clients/conformance/errors.json`).
const SAME_KEY_RETRY_CODES: [&str; 3] = [
    "rate_limit_exceeded",
    "concurrency_limit_exceeded",
    "provider_circuit_open",
];

type SleepFn = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Per-call options.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RequestOptions {
    /// The `Idempotency-Key`. Left unset, the client generates one per call
    /// and reuses it for that call's retries. Set it to make your own retries
    /// of the same operation safe across process restarts.
    pub idempotency_key: Option<String>,
    /// An `X-Request-Id` to correlate the call with your logs.
    pub request_id: Option<String>,
}

impl RequestOptions {
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    base_url: String,
    api_key: Option<String>,
    max_retries: u32,
    timeout: Option<Duration>,
    http: Option<reqwest::Client>,
    sleep: SleepFn,
    max_response_bytes: usize,
}

impl ClientBuilder {
    /// The `gw_` API key sent as the bearer credential.
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Automatic retries of requests the gateway refused before reserving
    /// anything (default 2). Other failures are never retried automatically.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// The time limit for each attempt of a non-streaming call (default 120
    /// seconds). A stream's response must start within it too, failing with
    /// [`Error::Timeout`]; after that there is no limit between events, so
    /// drop the stream to stop one.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The largest successful non-streaming response body read (default 64
    /// MiB); a larger one fails the call with [`Error::Decode`].
    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes;
        self
    }

    /// Use your own `reqwest::Client`, for example for a proxy or custom TLS
    /// roots.
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Replace the wait between retries. For tests.
    #[doc(hidden)]
    pub fn sleep_with<F, Fut>(mut self, sleep: F) -> Self
    where
        F: Fn(Duration) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.sleep = Arc::new(move |duration| Box::pin(sleep(duration)));
        self
    }

    pub fn build(self) -> Result<Client, Error> {
        if self.max_response_bytes == 0 {
            return Err(Error::Config("max_response_bytes must be positive".into()));
        }
        let api_key = self
            .api_key
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| Error::Config("an API key is required".into()))?;
        let base_url = self.base_url.trim_end_matches('/').to_owned();
        reqwest::Url::parse(&base_url)
            .map_err(|error| Error::Config(format!("base URL: {error}")))?;
        let http = match self.http {
            Some(http) => http,
            None => reqwest::Client::builder()
                .user_agent(concat!(
                    "qucelerate-ai-gateway-rust/",
                    env!("CARGO_PKG_VERSION")
                ))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
        };
        Ok(Client {
            inner: Arc::new(Inner {
                base_url,
                api_key,
                max_retries: self.max_retries,
                timeout: self.timeout.unwrap_or(Duration::from_secs(120)),
                http,
                sleep: self.sleep,
                max_response_bytes: self.max_response_bytes,
            }),
        })
    }
}

/// A client for the Gateway inference API. Cheap to clone; clones share one
/// connection pool.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    base_url: String,
    api_key: String,
    max_retries: u32,
    timeout: Duration,
    http: reqwest::Client,
    sleep: SleepFn,
    max_response_bytes: usize,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("base_url", &self.inner.base_url)
            .field("api_key", &"<redacted>")
            .field("max_retries", &self.inner.max_retries)
            .finish()
    }
}

impl Client {
    /// Start building a client for the gateway at `base_url`, such as
    /// `https://gateway.example.com`.
    pub fn builder(base_url: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            base_url: base_url.into(),
            api_key: None,
            max_retries: 2,
            timeout: None,
            http: None,
            sleep: Arc::new(|duration| Box::pin(tokio::time::sleep(duration))),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }

    /// Create a chat completion.
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, Error> {
        self.chat_with(request, RequestOptions::default()).await
    }

    pub async fn chat_with(
        &self,
        request: &ChatRequest,
        options: RequestOptions,
    ) -> Result<ChatResponse, Error> {
        let (headers, body) = self.send(CHAT_PATH, request, &options, false).await?;
        let mut response: ChatResponse = decode(&body)?;
        response.request_id = request_id(&headers);
        Ok(response)
    }

    /// Create embeddings, one per input.
    pub async fn embeddings(&self, request: &EmbeddingRequest) -> Result<EmbeddingResponse, Error> {
        self.embeddings_with(request, RequestOptions::default())
            .await
    }

    pub async fn embeddings_with(
        &self,
        request: &EmbeddingRequest,
        options: RequestOptions,
    ) -> Result<EmbeddingResponse, Error> {
        let (headers, body) = self.send(EMBEDDINGS_PATH, request, &options, false).await?;
        let mut response: EmbeddingResponse = decode(&body)?;
        response.request_id = request_id(&headers);
        Ok(response)
    }

    /// Stream a chat completion. The returned stream yields events until
    /// `response.completed`; a terminal error or an early close is an `Err`.
    /// Dropping the stream closes the connection.
    pub async fn chat_stream(&self, request: &ChatRequest) -> Result<ChatStream, Error> {
        self.chat_stream_with(request, RequestOptions::default())
            .await
    }

    pub async fn chat_stream_with(
        &self,
        request: &ChatRequest,
        options: RequestOptions,
    ) -> Result<ChatStream, Error> {
        let response = self.attempts(STREAM_PATH, request, &options, true).await?;
        let request_id = request_id(response.headers());
        Ok(ChatStream {
            body: Box::pin(response.bytes_stream()),
            parser: Parser::default(),
            pending: VecDeque::new(),
            done: false,
            request_id,
        })
    }

    /// Upload a batch input file: JSONL, one request per line, each
    /// `{"custom_id", "method": "POST", "url", "body"}`.
    pub async fn upload_batch_file(
        &self,
        filename: &str,
        content: impl Into<Vec<u8>>,
    ) -> Result<FileObject, Error> {
        if filename.is_empty() || filename.contains(['"', '\r', '\n']) {
            return Err(Error::Config(format!("not a file name: {filename:?}")));
        }
        let boundary = format!("gateway-{}", uuid::Uuid::new_v4().simple());
        let mut body = format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
             --{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             content-type: application/jsonl\r\n\r\n"
        )
        .into_bytes();
        body.extend(content.into());
        body.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let (_, body) = self
            .plain(
                reqwest::Method::POST,
                FILES_PATH.into(),
                Some((content_type, body)),
                None,
                &RequestOptions::default(),
            )
            .await?;
        decode(&body)
    }

    /// Create a batch from an uploaded file. The whole batch is checked,
    /// routed and reserved when it is created, or refused with nothing
    /// held. Retry with the same idempotency key to get the same batch.
    pub async fn create_batch(
        &self,
        request: &BatchRequest,
        options: RequestOptions,
    ) -> Result<Batch, Error> {
        let key = match &options.idempotency_key {
            Some(key) => key.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let body = serde_json::to_vec(request).map_err(|error| Error::Config(error.to_string()))?;
        let (headers, body) = self
            .plain(
                reqwest::Method::POST,
                BATCHES_PATH.into(),
                Some(("application/json".into(), body)),
                Some(key),
                &options,
            )
            .await?;
        batch(&headers, &body)
    }

    /// A batch, with the gateway's account of how it runs and its cost.
    pub async fn batch(&self, batch_id: &str) -> Result<Batch, Error> {
        let path = format!("{BATCHES_PATH}/{}", segment(batch_id)?);
        let (headers, body) = self
            .plain(
                reqwest::Method::GET,
                path,
                None,
                None,
                &RequestOptions::default(),
            )
            .await?;
        batch(&headers, &body)
    }

    /// The API key's service account's batches, newest first: at most
    /// `limit`, after the batch `after`.
    pub async fn list_batches(
        &self,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<BatchList, Error> {
        let mut query = Vec::new();
        if let Some(after) = after {
            query.push(format!("after={}", segment(after)?));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let path = if query.is_empty() {
            BATCHES_PATH.to_owned()
        } else {
            format!("{BATCHES_PATH}?{}", query.join("&"))
        };
        let (_, body) = self
            .plain(
                reqwest::Method::GET,
                path,
                None,
                None,
                &RequestOptions::default(),
            )
            .await?;
        decode(&body)
    }

    /// Ask a batch to stop: lines not yet run end without a charge.
    pub async fn cancel_batch(&self, batch_id: &str) -> Result<Batch, Error> {
        let path = format!("{BATCHES_PATH}/{}/cancel", segment(batch_id)?);
        let (headers, body) = self
            .plain(
                reqwest::Method::POST,
                path,
                None,
                None,
                &RequestOptions::default(),
            )
            .await?;
        batch(&headers, &body)
    }

    /// A file's content: a batch's output or error file is JSONL, one line
    /// per request line, matched by `custom_id`.
    pub async fn file_content(&self, file_id: &str) -> Result<Bytes, Error> {
        let path = format!("{FILES_PATH}/{}/content", segment(file_id)?);
        let (_, body) = self
            .plain(
                reqwest::Method::GET,
                path,
                None,
                None,
                &RequestOptions::default(),
            )
            .await?;
        Ok(body)
    }

    /// A files or batches call, retried only as `attempts` retries.
    async fn plain(
        &self,
        method: reqwest::Method,
        path: String,
        body: Option<(String, Vec<u8>)>,
        key: Option<String>,
        options: &RequestOptions,
    ) -> Result<(HeaderMap, Bytes), Error> {
        if let Some(key) = &key {
            if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
                return Err(Error::Config(format!(
                    "an Idempotency-Key must be 1 to {MAX_KEY_CHARS} characters"
                )));
            }
        }
        if let Some(id) = &options.request_id {
            if id.chars().count() > MAX_REQUEST_ID_CHARS {
                return Err(Error::Config(format!(
                    "an X-Request-Id must be at most {MAX_REQUEST_ID_CHARS} characters"
                )));
            }
        }
        let url = format!("{}{path}", self.inner.base_url);
        let mut retries = 0;
        loop {
            let mut request = self
                .inner
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.inner.api_key)
                .timeout(self.inner.timeout);
            if let Some((content_type, body)) = &body {
                request = request
                    .header(CONTENT_TYPE, content_type)
                    .body(body.clone());
            }
            if let Some(key) = &key {
                request = request.header(IDEMPOTENCY_KEY, key);
            }
            if let Some(id) = &options.request_id {
                request = request.header(REQUEST_ID, id);
            }
            let error = match checked(request).await {
                Ok(response) => {
                    let headers = response.headers().clone();
                    return match read_limited(response, self.inner.max_response_bytes).await? {
                        (body, false) => Ok((headers, body)),
                        (_, true) => Err(Error::Decode(format!(
                            "the response body exceeded {} bytes",
                            self.inner.max_response_bytes
                        ))),
                    };
                }
                Err(error) => error,
            };
            match &error {
                Error::Problem(problem)
                    if retries < self.inner.max_retries
                        && SAME_KEY_RETRY_CODES.contains(&problem.code.as_str()) =>
                {
                    retries += 1;
                    let wait = problem.retry_after.unwrap_or(1).min(MAX_WAIT_SECONDS);
                    (self.inner.sleep)(Duration::from_secs(wait)).await;
                }
                _ => return Err(error),
            }
        }
    }

    async fn send<T: Serialize>(
        &self,
        path: &str,
        body: &T,
        options: &RequestOptions,
        stream: bool,
    ) -> Result<(HeaderMap, Bytes), Error> {
        let response = self.attempts(path, body, options, stream).await?;
        let headers = response.headers().clone();
        match read_limited(response, self.inner.max_response_bytes).await? {
            (body, false) => Ok((headers, body)),
            (_, true) => Err(Error::Decode(format!(
                "the response body exceeded {} bytes",
                self.inner.max_response_bytes
            ))),
        }
    }

    /// Send the request, retrying with the same key only when nothing was
    /// reserved; return the first successful response.
    async fn attempts<T: Serialize>(
        &self,
        path: &str,
        body: &T,
        options: &RequestOptions,
        stream: bool,
    ) -> Result<reqwest::Response, Error> {
        let key = match &options.idempotency_key {
            Some(key) if key.is_empty() || key.chars().count() > MAX_KEY_CHARS => {
                return Err(Error::Config(format!(
                    "an Idempotency-Key must be 1 to {MAX_KEY_CHARS} characters"
                )));
            }
            Some(key) => key.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        if let Some(id) = &options.request_id {
            if id.chars().count() > MAX_REQUEST_ID_CHARS {
                return Err(Error::Config(format!(
                    "an X-Request-Id must be at most {MAX_REQUEST_ID_CHARS} characters"
                )));
            }
        }
        let body = serde_json::to_vec(body).map_err(|error| Error::Config(error.to_string()))?;
        let url = format!("{}{path}", self.inner.base_url);
        let mut retries = 0;
        loop {
            let mut request = self
                .inner
                .http
                .post(&url)
                .bearer_auth(&self.inner.api_key)
                .header(CONTENT_TYPE, "application/json")
                .header(IDEMPOTENCY_KEY, &key)
                .body(body.clone());
            if let Some(id) = &options.request_id {
                request = request.header(REQUEST_ID, id);
            }
            let timeout = self.inner.timeout;
            let result = if stream {
                // No limit between a stream's events, but its response
                // headers, or an error body, must arrive within the timeout.
                tokio::time::timeout(timeout, checked(request))
                    .await
                    .unwrap_or(Err(Error::Timeout(timeout)))
            } else {
                checked(request.timeout(timeout)).await
            };
            let error = match result {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            match &error {
                Error::Problem(problem)
                    if retries < self.inner.max_retries
                        && SAME_KEY_RETRY_CODES.contains(&problem.code.as_str()) =>
                {
                    retries += 1;
                    let wait = problem.retry_after.unwrap_or(1).min(MAX_WAIT_SECONDS);
                    (self.inner.sleep)(Duration::from_secs(wait)).await;
                }
                _ => return Err(error),
            }
        }
    }
}

/// Read at most `limit` bytes of the body; the flag says the body was longer,
/// in which case the rest is not read.
async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<(Bytes, bool), Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            body.extend_from_slice(&chunk[..limit - body.len()]);
            return Ok((Bytes::from(body), true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((Bytes::from(body), false))
}

/// Send one attempt: the response if it succeeded, else its error.
async fn checked(request: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
    let response = request.send().await?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(error_from(response).await)
    }
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn batch(headers: &HeaderMap, body: &[u8]) -> Result<Batch, Error> {
    let mut batch: Batch = decode(body)?;
    batch.request_id = request_id(headers);
    Ok(batch)
}

/// An id as one path segment: it can never name another path.
fn segment(id: &str) -> Result<&str, Error> {
    let plain = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if plain {
        Ok(id)
    } else {
        Err(Error::Config(format!("not an id: {id:?}")))
    }
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|error| Error::Decode(error.to_string()))
}

async fn error_from(response: reqwest::Response) -> Error {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let retry_after = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    // Only the start of an error body is read: enough for its problem.
    let body = match read_limited(response, MAX_ERROR_BODY_BYTES).await {
        Ok((body, _)) => body,
        Err(error) => return error,
    };
    if json {
        if let Ok(mut problem) = serde_json::from_slice::<Problem>(&body) {
            problem.status = status;
            problem.retry_after = retry_after;
            return Error::Problem(Box::new(problem));
        }
    }
    Error::Http {
        status,
        request_id: request_id(&headers),
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The events of a streamed chat completion.
pub struct ChatStream {
    body: ByteStream,
    parser: Parser,
    pending: VecDeque<Result<StreamEvent, Error>>,
    done: bool,
    request_id: Option<String>,
}

impl ChatStream {
    /// The `X-Request-Id` response header.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    fn accept(&mut self, chunk: &[u8]) {
        let mut raws = Vec::new();
        let oversized = self.parser.feed(chunk, &mut raws).is_err();
        self.decode(raws);
        if oversized && !self.done {
            self.done = true;
            self.pending.push_back(Err(Error::Protocol(format!(
                "a stream event exceeded {MAX_EVENT_BYTES} bytes"
            ))));
        }
    }

    fn decode(&mut self, raws: Vec<RawEvent>) {
        for raw in raws {
            if self.done {
                return;
            }
            if raw.name == "error" {
                self.done = true;
                let error = match serde_json::from_str::<serde_json::Value>(&raw.data) {
                    Ok(data) => Error::Stream {
                        code: data["code"].as_str().unwrap_or("unknown").to_owned(),
                        status: data["status"].as_u64().unwrap_or(0) as u16,
                    },
                    Err(error) => Error::Decode(error.to_string()),
                };
                self.pending.push_back(Err(error));
                return;
            }
            match StreamEvent::decode(&raw.name, &raw.data) {
                None => {}
                Some(Ok(event)) => {
                    self.done = matches!(event, StreamEvent::ResponseCompleted(_));
                    self.pending.push_back(Ok(event));
                }
                Some(Err(error)) => {
                    self.done = true;
                    self.pending
                        .push_back(Err(Error::Decode(error.to_string())));
                }
            }
        }
    }
}

impl Stream for ChatStream {
    type Item = Result<StreamEvent, Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Poll::Ready(Some(item));
            }
            if self.done {
                return Poll::Ready(None);
            }
            match self.body.poll_next_unpin(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => self.accept(&chunk),
                Poll::Ready(Some(Err(error))) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(Error::Transport(error))));
                }
                Poll::Ready(None) => {
                    // A final event may lack its blank line only if the
                    // stream was cut; either way it never completed.
                    self.done = true;
                    return Poll::Ready(Some(Err(Error::Protocol(
                        "the stream closed before a terminal event".into(),
                    ))));
                }
            }
        }
    }
}
