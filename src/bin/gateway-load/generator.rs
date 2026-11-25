//! The open-loop generator: requests are due at a set arrival rate
//! (Poisson by default) whatever the gateway does, each is sent on its own
//! task, and latency is measured from when it was due, so a slow gateway
//! cannot hide by being sent fewer requests (coordinated omission).
//!
//! Every request carries a directive for the fake provider (see
//! `directive.rs`), so the generator knows what the provider did and what
//! the gateway added: for a unary call, its latency less the provider's;
//! for a stream, the time to the first content event less the provider's
//! time to first byte.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures::StreamExt;
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    args::Flags,
    directive::{Directive, Fault},
    hist::Histogram,
    setup::{
        CHAT_ALIAS, CHAT_INPUT_PRICE, CHAT_OUTPUT_PRICE, EMBED_ALIAS, EMBED_INPUT_PRICE, Tenants,
    },
};

const CHAT_PATH: &str = "/api/inference/v1/chat/completions";
const STREAM_PATH: &str = "/api/inference/v1/chat/completions/stream";
const EMBED_PATH: &str = "/api/inference/v1/embeddings";
/// Prompts are split into messages of at most this many bytes (as long
/// conversations are), and kept, with room for the JSON around them, within
/// the gateway's default inference body limit
/// (`GATEWAY_INFERENCE_MAX_BODY_BYTES`, 4 MiB).
const MAX_TEXT_BYTES: usize = 100_000;
const MAX_BODY_BYTES: usize = 4_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Chat,
    Stream,
    Embed,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::Chat, Class::Stream, Class::Embed];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Chat => "chat",
            Class::Stream => "chat_stream",
            Class::Embed => "embeddings",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// How a request ended, as the client saw it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Answered in full (a stream with `response.completed`).
    Ok,
    /// An error status before any output.
    Refused,
    /// A stream that started and then sent an `error` event.
    StreamError,
    /// A stream that started and ended without completing.
    StreamCut,
    /// No response: the connection failed.
    Transport,
    /// No response within the client's timeout.
    Timeout,
    /// Not sent: too many requests were already in flight.
    NotSent,
}

#[derive(Clone, Debug, Serialize)]
pub struct Mix {
    pub chat: f64,
    pub stream: f64,
    pub embed: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FaultRates {
    pub rate_limited: f64,
    pub rate_limited_all: f64,
    pub server_error: f64,
    pub unavailable: f64,
    pub dropped: f64,
    pub timeout: f64,
}

impl FaultRates {
    fn pick(&self, roll: f64) -> Fault {
        let mut edge = 0.0;
        for (rate, fault) in [
            (self.rate_limited, Fault::RateLimited),
            (self.rate_limited_all, Fault::RateLimitedAll),
            (self.server_error, Fault::ServerError),
            (self.unavailable, Fault::Unavailable),
            (self.dropped, Fault::Dropped),
            (self.timeout, Fault::Timeout),
        ] {
            edge += rate;
            if roll < edge {
                return fault;
            }
        }
        Fault::None
    }
}

/// Everything a run is configured with (from flags and the profile).
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub profile: String,
    pub targets: Vec<String>,
    /// Requests a second.
    pub rate: f64,
    pub duration: Duration,
    /// Linear ramp from zero to `rate`; counted in `duration`.
    pub ramp: Duration,
    /// Excluded from latency and error figures (still checked for money).
    pub warmup: Duration,
    pub poisson: bool,
    pub seed: u64,
    pub mix: Mix,
    pub faults: FaultRates,
    pub retry_after: u64,
    /// `spread`: every organization alike; `single`: all on the first.
    pub tenancy: String,
    /// Share of requests sent to the tight organizations.
    pub tight_share: f64,
    pub end_user_share: f64,
    pub end_users: u32,
    /// End users of a tight organization: few, so their small allowances
    /// run out while requests race for them.
    pub tight_end_users: u32,
    pub prompt_bytes: (f64, f64),
    pub output_tokens: (f64, f64),
    pub chat_latency_ms: (f64, f64),
    pub stream_ttfb_ms: (f64, f64),
    pub stream_seconds: (f64, f64),
    pub embed_inputs: (f64, f64),
    pub embed_input_bytes: (f64, f64),
    pub embed_latency_ms: (f64, f64),
    pub max_in_flight: u64,
    pub request_timeout: Duration,
    pub window: Duration,
    pub audit_max: usize,
    pub events: Vec<Event>,
    pub event_grace: Duration,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub at: Duration,
    pub label: String,
    pub command: String,
}

/// `<at>:<label>:<shell command>`, e.g. `90s:gateway-restart:tests/load/run.sh restart-gateway 0`.
pub fn parse_event(value: &str) -> Result<Event, String> {
    let mut parts = value.splitn(3, ':');
    let (Some(at), Some(label), Some(command)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!(
            "--event {value:?}: expected <at>:<label>:<command>"
        ));
    };
    Ok(Event {
        at: crate::args::parse_duration(at).ok_or_else(|| format!("--event {value:?}: time"))?,
        label: label.trim().to_owned(),
        command: command.trim().to_owned(),
    })
}

impl Plan {
    pub fn from_flags(flags: &Flags) -> Result<Self, String> {
        let duration = flags.duration("duration", Duration::from_secs(60))?;
        let targets: Vec<String> = flags
            .required("targets")?
            .split(',')
            .map(|target| target.trim().trim_end_matches('/').to_owned())
            .filter(|target| !target.is_empty())
            .collect();
        if targets.is_empty() {
            return Err("--targets: no gateway URL".into());
        }
        let events = flags
            .all("event")
            .iter()
            .map(|value| parse_event(value))
            .collect::<Result<Vec<_>, _>>()?;
        // An event needs traffic after it to show recovery.
        if let Some(late) = events.iter().find(|event| event.at >= duration) {
            return Err(format!(
                "--event {}: at {:?}, not within the {duration:?} run",
                late.label, late.at
            ));
        }
        let rate: f64 = flags.parsed("rate", 20.0)?;
        if !(rate > 0.0 && rate.is_finite()) {
            return Err("--rate must be positive".into());
        }
        let default_window = Duration::from_secs((duration.as_secs() / 60).clamp(10, 300));
        Ok(Self {
            profile: flags.string("profile", "smoke"),
            targets,
            rate,
            duration,
            ramp: flags.duration("ramp", Duration::from_secs(5))?,
            warmup: flags.duration("warmup", Duration::from_secs(10))?,
            poisson: flags.string("arrival", "poisson") == "poisson",
            seed: flags.parsed("seed", 1)?,
            mix: Mix {
                chat: flags.parsed("mix-chat", 0.35)?,
                stream: flags.parsed("mix-stream", 0.45)?,
                embed: flags.parsed("mix-embed", 0.20)?,
            },
            faults: FaultRates {
                rate_limited: flags.parsed("fault-429", 0.0)?,
                rate_limited_all: flags.parsed("fault-429-all", 0.0)?,
                server_error: flags.parsed("fault-500", 0.0)?,
                unavailable: flags.parsed("fault-503", 0.0)?,
                dropped: flags.parsed("fault-drop", 0.0)?,
                timeout: flags.parsed("fault-timeout", 0.0)?,
            },
            retry_after: flags.parsed("retry-after", 1)?,
            tenancy: flags.string("tenancy", "spread"),
            tight_share: flags.parsed("tight-share", 0.02)?,
            end_user_share: flags.parsed("end-user-share", 0.5)?,
            end_users: flags.parsed("end-user-pool", 1_000)?,
            tight_end_users: flags.parsed("tight-end-user-pool", 10)?,
            prompt_bytes: flags.range("prompt-bytes", (400.0, 16_000.0))?,
            output_tokens: flags.range("output-tokens", (16.0, 512.0))?,
            chat_latency_ms: flags.range("chat-latency-ms", (200.0, 2_000.0))?,
            stream_ttfb_ms: flags.range("stream-ttfb-ms", (200.0, 1_500.0))?,
            stream_seconds: flags.range("stream-seconds", (2.0, 20.0))?,
            embed_inputs: flags.range("embed-inputs", (1.0, 32.0))?,
            embed_input_bytes: flags.range("embed-input-bytes", (100.0, 2_000.0))?,
            embed_latency_ms: flags.range("embed-latency-ms", (50.0, 300.0))?,
            max_in_flight: flags.parsed("max-in-flight", 100_000)?,
            request_timeout: flags.duration("request-timeout", Duration::from_secs(120))?,
            window: flags.duration("window", default_window)?,
            audit_max: flags.parsed("audit-max", 2_000_000)?,
            events,
            event_grace: flags.duration("event-grace", Duration::from_secs(15))?,
        })
    }
}

/// One request as planned: what it is, where it goes, and what the fake
/// provider will do.
#[derive(Clone, Debug)]
struct Spec {
    class: Class,
    tenant: usize,
    key: usize,
    target: usize,
    end_user: Option<u32>,
    directive: Directive,
    prompt_bytes: usize,
    embed_inputs: Vec<usize>,
}

fn uniform(rng: &mut StdRng, (low, high): (f64, f64)) -> f64 {
    if high <= low {
        low
    } else {
        rng.gen_range(low..high)
    }
}

/// Log-uniform: as many requests between 1 and 10 KB as between 10 and 100.
fn log_uniform(rng: &mut StdRng, (low, high): (f64, f64)) -> f64 {
    let low = low.max(1.0);
    if high <= low {
        return low;
    }
    (rng.gen_range(low.ln()..high.ln())).exp()
}

struct TenantPick {
    regular: Vec<usize>,
    tight: Vec<usize>,
}

fn spec(plan: &Plan, tenants: &Tenants, pick: &TenantPick, index: u64) -> Spec {
    let mut rng = StdRng::seed_from_u64(plan.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ index);
    let tenant = if !pick.tight.is_empty() && rng.gen_bool(plan.tight_share.clamp(0.0, 1.0)) {
        pick.tight[rng.gen_range(0..pick.tight.len())]
    } else if plan.tenancy == "single" || pick.regular.len() == 1 {
        pick.regular[0]
    } else {
        pick.regular[rng.gen_range(0..pick.regular.len())]
    };
    let key = rng.gen_range(0..tenants.tenants[tenant].keys.len());
    let total = plan.mix.chat + plan.mix.stream + plan.mix.embed;
    let roll = rng.gen_range(0.0..total.max(f64::MIN_POSITIVE));
    let class = if roll < plan.mix.chat {
        Class::Chat
    } else if roll < plan.mix.chat + plan.mix.stream {
        Class::Stream
    } else {
        Class::Embed
    };
    let tight = tenants.tenants[tenant].tight;
    let pool = if tight {
        plan.tight_end_users
    } else {
        plan.end_users
    };
    let end_user =
        (tenants.end_users && pool > 0 && rng.gen_bool(plan.end_user_share.clamp(0.0, 1.0)))
            .then(|| rng.gen_range(0..pool));
    let fault = plan.faults.pick(rng.gen_range(0.0..1.0));
    let cap = (tenants.max_output_tokens as f64).max(1.0);
    let out = uniform(&mut rng, plan.output_tokens).clamp(1.0, cap) as u64;
    let mut directive = Directive {
        out,
        fault,
        retry_after: plan.retry_after,
        ..Directive::default()
    };
    let limit = (tenants.max_input_bytes as usize)
        .saturating_sub(512)
        .min(MAX_BODY_BYTES);
    let mut prompt_bytes = 0;
    let mut embed_inputs = Vec::new();
    match class {
        Class::Chat => {
            directive.lat_ms = log_uniform(&mut rng, plan.chat_latency_ms) as u64;
            prompt_bytes = (log_uniform(&mut rng, plan.prompt_bytes) as usize).min(limit);
        }
        Class::Stream => {
            directive.ttfb_ms = log_uniform(&mut rng, plan.stream_ttfb_ms) as u64;
            directive.dur_ms = (uniform(&mut rng, plan.stream_seconds) * 1e3) as u64;
            prompt_bytes = (log_uniform(&mut rng, plan.prompt_bytes) as usize).min(limit);
        }
        Class::Embed => {
            directive.out = 0;
            directive.lat_ms = log_uniform(&mut rng, plan.embed_latency_ms) as u64;
            let count = uniform(&mut rng, plan.embed_inputs)
                .round()
                .clamp(1.0, 128.0) as usize;
            let mut budget = limit;
            for _ in 0..count {
                let bytes = (log_uniform(&mut rng, plan.embed_input_bytes) as usize)
                    .clamp(80, MAX_TEXT_BYTES)
                    .min(budget);
                if bytes < 80 {
                    break;
                }
                budget -= bytes;
                embed_inputs.push(bytes);
            }
        }
    }
    Spec {
        class,
        tenant,
        key,
        target: (index % plan.targets.len() as u64) as usize,
        end_user,
        directive,
        prompt_bytes,
        embed_inputs,
    }
}

/// `bytes` of text that starts with the directive.
fn text(filler: &str, directive: Option<&Directive>, bytes: usize) -> String {
    let mut text = directive.map(Directive::render).unwrap_or_default();
    text.push(' ');
    let rest = bytes.saturating_sub(text.len()).min(filler.len());
    text.push_str(&filler[..rest]);
    text
}

fn body(spec: &Spec, filler: &str) -> Value {
    match spec.class {
        Class::Chat | Class::Stream => {
            // Texts are bounded, so a long prompt is several messages; the
            // directive leads the last, which the fake reads.
            let mut sizes = Vec::new();
            let mut left = spec.prompt_bytes.max(64);
            while left > MAX_TEXT_BYTES {
                sizes.push(MAX_TEXT_BYTES);
                left -= MAX_TEXT_BYTES;
            }
            sizes.push(left);
            let last = sizes.len() - 1;
            let messages: Vec<Value> = sizes
                .iter()
                .enumerate()
                .map(|(index, bytes)| {
                    let directive = (index == last).then_some(&spec.directive);
                    json!({"role": "user", "content": text(filler, directive, *bytes)})
                })
                .collect();
            json!({"model": CHAT_ALIAS, "messages": messages,
                   "max_tokens": spec.directive.out.max(1)})
        }
        Class::Embed => {
            let input: Vec<String> = spec
                .embed_inputs
                .iter()
                .enumerate()
                .map(|(index, bytes)| text(filler, (index == 0).then_some(&spec.directive), *bytes))
                .collect();
            json!({"model": EMBED_ALIAS, "input": input})
        }
    }
}

/// What the request should cost, from the usage the gateway reported.
fn expected_charge(class: Class, usage: &Value) -> Option<i64> {
    let input = usage["input_tokens"].as_i64()?;
    match class {
        Class::Embed => Some(input * EMBED_INPUT_PRICE),
        _ => {
            let output = usage["output_tokens"].as_i64()?;
            Some(input * CHAT_INPUT_PRICE + output * CHAT_OUTPUT_PRICE)
        }
    }
}

/// The result of sending one request.
#[derive(Clone, Debug)]
struct Outcome {
    kind: Kind,
    status: u16,
    /// The problem code, or the stream's error code.
    code: Option<String>,
    usage: Option<Value>,
    /// A stream's first content event.
    first_content: Option<Instant>,
    detail: Option<String>,
}

impl Outcome {
    fn failed(kind: Kind, detail: String) -> Self {
        Self {
            kind,
            status: 0,
            code: None,
            usage: None,
            first_content: None,
            detail: Some(detail),
        }
    }

    fn label(&self) -> String {
        match self.kind {
            Kind::Ok => "ok".into(),
            Kind::Refused => format!(
                "http_{}_{}",
                self.status,
                self.code.as_deref().unwrap_or("unknown")
            ),
            Kind::StreamError => {
                format!("stream_error_{}", self.code.as_deref().unwrap_or("unknown"))
            }
            Kind::StreamCut => "stream_cut".into(),
            Kind::Transport => "transport".into(),
            Kind::Timeout => "client_timeout".into(),
            Kind::NotSent => "not_sent".into(),
        }
    }
}

/// Whether the outcome is the one the injected fault calls for.
fn expected(spec: &Spec, outcome: &Outcome, tight: bool) -> bool {
    let refused_with =
        |statuses: &[u16]| outcome.kind == Kind::Refused && statuses.contains(&outcome.status);
    if tight && refused_with(&[402, 429]) && outcome.code.as_deref() == Some("quota_exceeded") {
        return true;
    }
    match spec.directive.fault {
        Fault::None => outcome.kind == Kind::Ok,
        // Failover answers, unless the second deployment's circuit is open
        // (other requests' `429all` trip it): then the 429 stands.
        Fault::RateLimited => outcome.kind == Kind::Ok || refused_with(&[429]),
        Fault::RateLimitedAll => refused_with(&[429]),
        Fault::ServerError | Fault::Unavailable | Fault::Timeout => {
            refused_with(&[500, 502, 503, 504])
                || matches!(outcome.kind, Kind::StreamError | Kind::StreamCut)
        }
        Fault::Dropped => {
            refused_with(&[500, 502, 503, 504])
                || matches!(outcome.kind, Kind::StreamError | Kind::StreamCut)
        }
    }
}

async fn send(
    client: &reqwest::Client,
    plan: &Plan,
    tenants: &Tenants,
    spec: &Spec,
    filler: &str,
    correlation: Uuid,
) -> Outcome {
    let path = match spec.class {
        Class::Chat => CHAT_PATH,
        Class::Stream => STREAM_PATH,
        Class::Embed => EMBED_PATH,
    };
    let tenant = &tenants.tenants[spec.tenant];
    let mut request = client
        .post(format!("{}{path}", plan.targets[spec.target]))
        .bearer_auth(&tenant.keys[spec.key].bearer)
        .header("idempotency-key", correlation.to_string())
        .header("x-request-id", correlation.to_string())
        .json(&body(spec, filler));
    if let Some(user) = spec.end_user {
        request = request.header("gateway-end-user", format!("load-user-{user}"));
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Outcome::failed(Kind::Timeout, error.to_string());
        }
        Err(error) => return Outcome::failed(Kind::Transport, error.to_string()),
    };
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        let code = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value["code"].as_str().map(str::to_owned));
        return Outcome {
            kind: Kind::Refused,
            status,
            code,
            usage: None,
            first_content: None,
            detail: Some(text.chars().take(300).collect()),
        };
    }
    if spec.class != Class::Stream {
        return match response.bytes().await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => Outcome {
                    kind: Kind::Ok,
                    status,
                    code: None,
                    usage: Some(value["usage"].clone()),
                    first_content: None,
                    detail: None,
                },
                Err(error) => Outcome::failed(Kind::Transport, format!("body: {error}")),
            },
            Err(error) if error.is_timeout() => {
                Outcome::failed(Kind::Timeout, format!("body: {error}"))
            }
            Err(error) => Outcome::failed(Kind::Transport, format!("body: {error}")),
        };
    }
    let mut first_content = None;
    let mut buffer = String::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = match stream.next().await {
            None => break,
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => {
                let mut outcome = Outcome::failed(Kind::StreamCut, format!("stream: {error}"));
                outcome.status = status;
                outcome.first_content = first_content;
                return outcome;
            }
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let mut name = "";
            let mut data = "";
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    name = value;
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = value;
                }
            }
            match name {
                "choice.delta" if first_content.is_none() => first_content = Some(Instant::now()),
                "response.completed" => {
                    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
                    return Outcome {
                        kind: Kind::Ok,
                        status,
                        code: None,
                        usage: Some(value["usage"].clone()),
                        first_content,
                        detail: None,
                    };
                }
                "error" => {
                    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
                    return Outcome {
                        kind: Kind::StreamError,
                        status,
                        code: value["code"].as_str().map(str::to_owned),
                        usage: None,
                        first_content,
                        detail: Some(data.to_owned()),
                    };
                }
                _ => {}
            }
        }
    }
    let mut outcome = Outcome::failed(Kind::StreamCut, "ended without response.completed".into());
    outcome.status = status;
    outcome.first_content = first_content;
    outcome
}

/// A request kept for the accounting audit after the run.
#[derive(Clone, Debug)]
pub struct AuditRecord {
    pub correlation: Uuid,
    pub class: Class,
    pub fault: Fault,
    pub kind: Kind,
    pub status: u16,
    pub code: Option<String>,
    /// What an answered request must have cost.
    pub expected_minor: Option<i64>,
    /// In flight while an injected failure was under way.
    pub disrupted: bool,
}

#[derive(Default)]
struct ClassStats {
    /// What the gateway added: latency less the provider's (to the first
    /// content event, for a stream), of clean answered requests.
    overhead: Histogram,
    /// Latency from due to the full answer.
    latency: Histogram,
    /// A stream's time from due to its first content event.
    first_content: Histogram,
}

#[derive(Clone, Default, Serialize)]
pub struct Second {
    pub due: u64,
    pub ok: u64,
    pub unexpected: u64,
    pub disrupted: u64,
}

pub struct Window {
    pub start: Duration,
    pub overhead: Histogram,
    pub completed: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventRecord {
    pub label: String,
    pub command: String,
    pub started_s: f64,
    pub finished_s: Option<f64>,
    pub exit_ok: Option<bool>,
    pub output: String,
    /// Seconds after the command finished until the first full second with
    /// no unexpected outcome.
    pub recovery_s: Option<f64>,
}

#[derive(Default)]
pub struct Stats {
    classes: [ClassStats; 3],
    /// Outcome counts by class, fault and outcome label.
    pub outcomes: HashMap<(Class, Fault, String), u64>,
    pub due: u64,
    pub measured: u64,
    pub ok: u64,
    pub expected_failures: u64,
    pub unexpected: u64,
    pub disrupted: u64,
    pub not_sent: u64,
    pub send_lag: Histogram,
    /// Latency of clean answered requests in flight during an injected
    /// failure.
    pub during_failures: Histogram,
    pub seconds: Vec<Second>,
    /// Outcomes by the second their answer came (recovery is counted from
    /// answers, not from when the requests were due).
    pub answered: Vec<Second>,
    /// Request and event tasks that panicked instead of recording an
    /// outcome; any fails the run.
    pub crashed: u64,
    pub windows: Vec<Window>,
    pub audit: Vec<AuditRecord>,
    pub audit_dropped: u64,
    pub first_unexpected: Vec<String>,
    pub events: Vec<EventRecord>,
    pub in_flight_max: u64,
    pub elapsed: Duration,
}

impl Stats {
    pub fn class_summary(&self) -> Value {
        let mut map = serde_json::Map::new();
        for class in Class::ALL {
            let stats = &self.classes[class.index()];
            map.insert(
                class.as_str().to_owned(),
                json!({
                    "overhead": stats.overhead.summary(),
                    "latency": stats.latency.summary(),
                    "first_content": stats.first_content.summary(),
                }),
            );
        }
        map.into()
    }

    pub fn overhead(&self, class: Class) -> &Histogram {
        &self.classes[class.index()].overhead
    }
}

struct Shared {
    plan: Plan,
    tenants: Tenants,
    pick: TenantPick,
    filler: String,
    client: reqwest::Client,
    start: Instant,
    in_flight: AtomicU64,
    stats: Mutex<Stats>,
    /// Injected failures: (start, end) as offsets from the start.
    disruptions: Mutex<Vec<(Duration, Option<Duration>)>>,
}

impl Shared {
    /// Whether `[due, done]` overlaps an injected failure or its grace.
    fn disrupted(&self, due: Duration, done: Duration) -> bool {
        self.disruptions.lock().unwrap().iter().any(|(start, end)| {
            *start <= done && end.is_none_or(|end| due <= end + self.plan.event_grace)
        })
    }
}

fn filler() -> String {
    const WORDS: [&str; 12] = [
        "lorem",
        "ipsum",
        "dolor",
        "sit",
        "amet",
        "consectetur",
        "adipiscing",
        "elit",
        "sed",
        "do",
        "eiusmod",
        "tempor",
    ];
    let mut text = String::with_capacity(MAX_BODY_BYTES + 64);
    let mut index = 0usize;
    while text.len() < MAX_BODY_BYTES {
        text.push_str(WORDS[index % WORDS.len()]);
        text.push(' ');
        index = index.wrapping_mul(31).wrapping_add(7) % 9_973;
    }
    text
}

async fn one(shared: Arc<Shared>, index: u64, due: Instant) {
    let plan = &shared.plan;
    let spec = spec(plan, &shared.tenants, &shared.pick, index);
    let correlation = Uuid::new_v4();
    let lag = due.elapsed();
    let outcome = if shared.in_flight.fetch_add(1, Ordering::Relaxed) >= plan.max_in_flight {
        Outcome::failed(Kind::NotSent, "max in flight".into())
    } else {
        tokio::time::timeout(
            plan.request_timeout,
            send(
                &shared.client,
                plan,
                &shared.tenants,
                &spec,
                &shared.filler,
                correlation,
            ),
        )
        .await
        .unwrap_or_else(|_| Outcome::failed(Kind::Timeout, "client timeout".into()))
    };
    let in_flight = shared.in_flight.fetch_sub(1, Ordering::Relaxed);
    let done = Instant::now();
    let due_offset = due.duration_since(shared.start);
    let done_offset = done.duration_since(shared.start);
    let tight = shared.tenants.tenants[spec.tenant].tight;
    let is_expected = expected(&spec, &outcome, tight);
    let during_failure = shared.disrupted(due_offset, done_offset);
    let disrupted = !is_expected && during_failure;
    let measured = due_offset >= plan.warmup;
    let latency = done.duration_since(due);

    let mut stats = shared.stats.lock().unwrap();
    stats.in_flight_max = stats.in_flight_max.max(in_flight);
    stats.send_lag.record(lag);
    *stats
        .outcomes
        .entry((spec.class, spec.directive.fault, outcome.label()))
        .or_default() += 1;
    let second = due_offset.as_secs() as usize;
    if stats.seconds.len() <= second {
        stats.seconds.resize(second + 1, Second::default());
    }
    stats.seconds[second].due += 1;
    if outcome.kind == Kind::NotSent {
        stats.not_sent += 1;
    }
    let answered = done_offset.as_secs() as usize;
    if stats.answered.len() <= answered {
        stats.answered.resize(answered + 1, Second::default());
    }
    stats.answered[answered].due += 1;
    if is_expected {
        if outcome.kind == Kind::Ok {
            stats.seconds[second].ok += 1;
            stats.answered[answered].ok += 1;
        }
    } else if disrupted {
        stats.seconds[second].disrupted += 1;
        stats.answered[answered].disrupted += 1;
    } else {
        stats.seconds[second].unexpected += 1;
        stats.answered[answered].unexpected += 1;
    }
    if measured {
        stats.measured += 1;
        match (is_expected, outcome.kind, disrupted) {
            (true, Kind::Ok, _) => stats.ok += 1,
            (true, _, _) => stats.expected_failures += 1,
            (false, _, true) => stats.disrupted += 1,
            (false, _, false) => {
                stats.unexpected += 1;
                if stats.first_unexpected.len() < 20 {
                    let line = format!(
                        "{} fault={} tenant={} -> {} {}",
                        spec.class.as_str(),
                        spec.directive.fault,
                        spec.tenant,
                        outcome.label(),
                        outcome.detail.clone().unwrap_or_default()
                    );
                    stats.first_unexpected.push(line);
                }
            }
        }
        // Latency figures are of clean answered requests: a failover's
        // retries and a refusal's timing are the fault's, not the gateway's,
        // and a request in flight during an injected failure is counted
        // apart (the failure's recovery is its own gate).
        if outcome.kind == Kind::Ok && spec.directive.fault == Fault::None && during_failure {
            stats.during_failures.record(latency);
        } else if outcome.kind == Kind::Ok && spec.directive.fault == Fault::None {
            let class = &mut stats.classes[spec.class.index()];
            class.latency.record(latency);
            let overhead = match (spec.class, outcome.first_content) {
                (Class::Stream, Some(first)) => {
                    let to_first = first.duration_since(due);
                    class.first_content.record(to_first);
                    Some(to_first.saturating_sub(Duration::from_millis(spec.directive.ttfb_ms)))
                }
                (Class::Stream, None) => None,
                _ => Some(latency.saturating_sub(Duration::from_millis(spec.directive.lat_ms))),
            };
            if let Some(overhead) = overhead {
                class.overhead.record(overhead);
                let window = (due_offset.as_secs_f64() / plan.window.as_secs_f64()) as usize;
                while stats.windows.len() <= window {
                    let start = plan.window * stats.windows.len() as u32;
                    stats.windows.push(Window {
                        start,
                        overhead: Histogram::default(),
                        completed: 0,
                    });
                }
                stats.windows[window].overhead.record(overhead);
                stats.windows[window].completed += 1;
            }
        }
    }
    if outcome.kind != Kind::NotSent {
        if stats.audit.len() < plan.audit_max {
            let expected_minor = match outcome.kind {
                Kind::Ok => outcome
                    .usage
                    .as_ref()
                    .and_then(|usage| expected_charge(spec.class, usage)),
                _ => None,
            };
            stats.audit.push(AuditRecord {
                correlation,
                class: spec.class,
                fault: spec.directive.fault,
                kind: outcome.kind,
                status: outcome.status,
                code: outcome.code.clone(),
                expected_minor,
                disrupted: during_failure,
            });
        } else {
            stats.audit_dropped += 1;
        }
    }
}

async fn run_event(shared: Arc<Shared>, event: Event) {
    tokio::time::sleep_until((shared.start + event.at).into()).await;
    let started = shared.start.elapsed();
    let slot = {
        let mut disruptions = shared.disruptions.lock().unwrap();
        disruptions.push((started, None));
        disruptions.len() - 1
    };
    eprintln!(
        "[{:>6.1}s] event {}: {}",
        started.as_secs_f64(),
        event.label,
        event.command
    );
    let command = event.command.clone();
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
    })
    .await;
    let finished = shared.start.elapsed();
    shared.disruptions.lock().unwrap()[slot].1 = Some(finished);
    let (exit_ok, output) = match result {
        Ok(Ok(output)) => (
            Some(output.status.success()),
            String::from_utf8_lossy(&[output.stdout, output.stderr].concat())
                .chars()
                .rev()
                .take(2_000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
        ),
        Ok(Err(error)) => (Some(false), error.to_string()),
        Err(error) => (Some(false), error.to_string()),
    };
    eprintln!(
        "[{:>6.1}s] event {} done ({})",
        finished.as_secs_f64(),
        event.label,
        if exit_ok == Some(true) {
            "ok"
        } else {
            "FAILED"
        }
    );
    shared.stats.lock().unwrap().events.push(EventRecord {
        label: event.label,
        command: event.command,
        started_s: started.as_secs_f64(),
        finished_s: Some(finished.as_secs_f64()),
        exit_ok,
        output,
        recovery_s: None,
    });
}

/// Sends the plan's load, then waits for every request to finish.
pub async fn run(plan: Plan, tenants: Tenants) -> Result<Stats, String> {
    let regular: Vec<usize> = (0..tenants.tenants.len())
        .filter(|index| !tenants.tenants[*index].tight)
        .collect();
    let tight: Vec<usize> = (0..tenants.tenants.len())
        .filter(|index| tenants.tenants[*index].tight)
        .collect();
    if regular.is_empty() {
        return Err("the tenants file has no regular organization".into());
    }
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(4_096)
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| error.to_string())?;
    let shared = Arc::new(Shared {
        pick: TenantPick { regular, tight },
        tenants,
        filler: filler(),
        client,
        start: Instant::now(),
        in_flight: AtomicU64::new(0),
        stats: Mutex::new(Stats::default()),
        disruptions: Mutex::new(Vec::new()),
        plan,
    });
    let plan = &shared.plan;
    eprintln!(
        "load: {:.1} requests/s ({:.0}/min) for {:?} against {} gateway(s), {} organizations",
        plan.rate,
        plan.rate * 60.0,
        plan.duration,
        plan.targets.len(),
        shared.tenants.tenants.len()
    );
    let mut events = Vec::new();
    for event in plan.events.clone() {
        events.push(tokio::spawn(run_event(shared.clone(), event)));
    }

    let mut rng = StdRng::seed_from_u64(plan.seed ^ 0xA5A5_5A5A);
    let mut tasks = tokio::task::JoinSet::new();
    let mut index = 0u64;
    let mut crashed = 0u64;
    let mut next = Duration::ZERO;
    let mut last_report = Instant::now();
    while next < plan.duration {
        let due = shared.start + next;
        if due > Instant::now() {
            tokio::time::sleep_until(due.into()).await;
        }
        tasks.spawn(one(shared.clone(), index, due));
        index += 1;
        // The rate at this point of the ramp.
        let ramp = if plan.ramp.is_zero() {
            1.0
        } else {
            (next.as_secs_f64() / plan.ramp.as_secs_f64()).clamp(0.02, 1.0)
        };
        let rate = plan.rate * ramp;
        let gap = if plan.poisson {
            -(1.0 - rng.gen_range(0.0..1.0f64)).ln() / rate
        } else {
            1.0 / rate
        };
        next += Duration::from_secs_f64(gap);
        while let Some(result) = tasks.try_join_next() {
            crashed += u64::from(result.is_err());
        }
        if last_report.elapsed() >= Duration::from_secs(10) {
            last_report = Instant::now();
            let stats = shared.stats.lock().unwrap();
            eprintln!(
                "[{:>6.1}s] due {index} in flight {} ok {} unexpected {} disrupted {}",
                shared.start.elapsed().as_secs_f64(),
                shared.in_flight.load(Ordering::Relaxed),
                stats.ok,
                stats.unexpected,
                stats.disrupted
            );
        }
    }
    eprintln!(
        "[{:>6.1}s] all {index} requests due; waiting for {} in flight",
        shared.start.elapsed().as_secs_f64(),
        shared.in_flight.load(Ordering::Relaxed)
    );
    while let Some(result) = tasks.join_next().await {
        crashed += u64::from(result.is_err());
    }
    for event in events {
        crashed += u64::from(event.await.is_err());
    }
    if crashed > 0 {
        eprintln!("{crashed} request or event task(s) panicked; the run fails");
    }
    let elapsed = shared.start.elapsed();
    let shared = Arc::into_inner(shared).ok_or("generator state still shared")?;
    let mut stats = shared.stats.into_inner().unwrap();
    stats.due = index;
    stats.crashed = crashed;
    stats.elapsed = elapsed;
    // Recovery: from each event's end to the first full second after it
    // whose answers held no unexpected outcome and some answered request
    // (by answer time, so a request queued through the failure and
    // answered later does not count as recovery before it was answered).
    for event in &mut stats.events {
        let Some(finished) = event.finished_s else {
            continue;
        };
        let first = finished.ceil() as usize;
        event.recovery_s = stats
            .answered
            .iter()
            .enumerate()
            .skip(first)
            .find(|(_, second)| second.unexpected == 0 && second.disrupted == 0 && second.ok > 0)
            .map(|(index, _)| index as f64 - finished);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(kind: Kind, status: u16, code: Option<&str>) -> Outcome {
        Outcome {
            kind,
            status,
            code: code.map(str::to_owned),
            usage: None,
            first_content: None,
            detail: None,
        }
    }

    fn spec_with(fault: Fault) -> Spec {
        Spec {
            class: Class::Stream,
            tenant: 0,
            key: 0,
            target: 0,
            end_user: None,
            directive: Directive {
                fault,
                ..Directive::default()
            },
            prompt_bytes: 100,
            embed_inputs: Vec::new(),
        }
    }

    #[test]
    fn each_fault_expects_its_own_outcome() {
        let ok = outcome(Kind::Ok, 200, None);
        let refused_429 = outcome(Kind::Refused, 429, Some("provider_rate_limited"));
        let bad_gateway = outcome(Kind::Refused, 502, Some("provider_unavailable"));
        let cut = outcome(Kind::StreamCut, 200, None);
        let quota = outcome(Kind::Refused, 429, Some("quota_exceeded"));
        assert!(expected(&spec_with(Fault::None), &ok, false));
        assert!(!expected(&spec_with(Fault::None), &bad_gateway, false));
        assert!(expected(&spec_with(Fault::RateLimited), &ok, false));
        assert!(expected(
            &spec_with(Fault::RateLimitedAll),
            &refused_429,
            false
        ));
        assert!(!expected(&spec_with(Fault::RateLimitedAll), &ok, false));
        assert!(expected(
            &spec_with(Fault::Unavailable),
            &bad_gateway,
            false
        ));
        assert!(expected(&spec_with(Fault::Dropped), &cut, false));
        assert!(!expected(&spec_with(Fault::None), &quota, false));
        assert!(expected(&spec_with(Fault::None), &quota, true));
    }

    #[test]
    fn faults_are_picked_in_proportion() {
        let rates = FaultRates {
            rate_limited: 0.1,
            dropped: 0.1,
            ..FaultRates::default()
        };
        assert_eq!(rates.pick(0.05), Fault::RateLimited);
        assert_eq!(rates.pick(0.15), Fault::Dropped);
        assert_eq!(rates.pick(0.5), Fault::None);
    }

    #[test]
    fn a_long_prompt_is_split_and_carries_its_directive_last() {
        let mut spec = spec_with(Fault::Timeout);
        spec.prompt_bytes = 230_000;
        let body = body(&spec, &filler());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let last = messages[2]["content"].as_str().unwrap();
        assert_eq!(Directive::parse(last).unwrap().fault, Fault::Timeout);
        let total: usize = messages
            .iter()
            .map(|m| m["content"].as_str().unwrap().len())
            .sum();
        assert!((229_000..=231_000).contains(&total), "{total}");
        assert!(
            messages
                .iter()
                .all(|m| m["content"].as_str().unwrap().len() <= MAX_TEXT_BYTES)
        );
    }

    #[test]
    fn events_parse() {
        let event = parse_event("90s:gateway-restart:run.sh restart-gateway 0").unwrap();
        assert_eq!(event.at, Duration::from_secs(90));
        assert_eq!(event.label, "gateway-restart");
        assert_eq!(event.command, "run.sh restart-gateway 0");
        assert!(parse_event("90s").is_err());
    }
}
