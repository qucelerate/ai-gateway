use serde::Deserialize;

/// An RFC 9457 problem the gateway returned.
///
/// `code` is an open string: match the codes you handle and fall back to
/// `status` for the rest. `clients/conformance/errors.json` lists the codes of
/// contract 1.0.0 and what each means for a retry.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct Problem {
    #[serde(rename = "type")]
    pub kind: String,
    pub code: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    pub instance: String,
    pub request_id: String,
    /// The `Retry-After` header in whole seconds, when the response had one.
    #[serde(skip)]
    pub retry_after: Option<u64>,
}

/// Every way a call can fail.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The gateway refused or failed the request with a problem.
    #[error("gateway problem {}: {} ({}; request {})", .0.status, .0.code, .0.title, .0.request_id)]
    Problem(Box<Problem>),
    /// An error response without a problem body, for example from a proxy.
    #[error("HTTP {status} without a problem body")]
    Http {
        status: u16,
        request_id: Option<String>,
    },
    /// A stream ended with a terminal `error` event. The request reached the
    /// provider, so it is never retried automatically.
    #[error("stream failed: {code} (status {status})")]
    Stream { code: String, status: u16 },
    /// A stream closed before its terminal event. Streams are not resumable.
    #[error("stream protocol error: {0}")]
    Protocol(String),
    /// The request could not be sent or the response could not be read.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// A stream's response did not start within the client's timeout.
    #[error("the stream did not start within {0:?}")]
    Timeout(std::time::Duration),
    /// A response body did not match the contract.
    #[error("could not decode the response: {0}")]
    Decode(String),
    /// The client or the call was configured with an invalid value.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl Error {
    /// The problem, when the gateway returned one.
    pub fn problem(&self) -> Option<&Problem> {
        match self {
            Self::Problem(problem) => Some(problem),
            _ => None,
        }
    }

    /// The HTTP status of an error response or a stream's terminal error.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Problem(problem) => Some(problem.status),
            Self::Http { status, .. } | Self::Stream { status, .. } => Some(*status),
            Self::Transport(error) => error.status().map(|status| status.as_u16()),
            _ => None,
        }
    }
}
