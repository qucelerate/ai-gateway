//! What a request asks of the fake provider, carried in its own content so
//! the fake stays stateless and the generator knows exactly what each
//! request met: `[[load out=120 lat=300 ttfb=200 dur=5000 fault=none ra=1]]`
//! at the start of the last chat message or the first embedding input.
//! The gateway forwards content unchanged, so the directive reaches the
//! fake on every attempt, failover included.

use std::fmt;

/// Provider trouble to inject.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum Fault {
    #[default]
    None,
    /// 429 with `Retry-After` from the route's first deployment only, so
    /// failover to the second one answers.
    RateLimited,
    /// 429 from every deployment: the request is refused.
    RateLimitedAll,
    /// 500 before any output.
    ServerError,
    /// 503 before any output.
    Unavailable,
    /// A stream (or a unary body) cut midway.
    Dropped,
    /// No answer until long after the gateway's provider timeout.
    Timeout,
}

impl Fault {
    pub const ALL: [Fault; 7] = [
        Fault::None,
        Fault::RateLimited,
        Fault::RateLimitedAll,
        Fault::ServerError,
        Fault::Unavailable,
        Fault::Dropped,
        Fault::Timeout,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Fault::None => "none",
            Fault::RateLimited => "429",
            Fault::RateLimitedAll => "429all",
            Fault::ServerError => "500",
            Fault::Unavailable => "503",
            Fault::Dropped => "drop",
            Fault::Timeout => "timeout",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|fault| fault.as_str() == value)
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Directive {
    /// Output tokens (chat).
    pub out: u64,
    /// Milliseconds before a unary answer.
    pub lat_ms: u64,
    /// Milliseconds before a stream's first chunk.
    pub ttfb_ms: u64,
    /// Milliseconds a stream lasts after its first chunk.
    pub dur_ms: u64,
    pub fault: Fault,
    /// `Retry-After` seconds of a 429.
    pub retry_after: u64,
}

impl Directive {
    pub fn render(&self) -> String {
        format!(
            "[[load out={} lat={} ttfb={} dur={} fault={} ra={}]]",
            self.out, self.lat_ms, self.ttfb_ms, self.dur_ms, self.fault, self.retry_after
        )
    }

    /// The directive at the start of `text`, if there is one.
    pub fn parse(text: &str) -> Option<Self> {
        let body = text.strip_prefix("[[load ")?;
        let (body, _) = body.split_once("]]")?;
        let mut directive = Directive::default();
        for field in body.split_whitespace() {
            let (name, value) = field.split_once('=')?;
            match name {
                "out" => directive.out = value.parse().ok()?,
                "lat" => directive.lat_ms = value.parse().ok()?,
                "ttfb" => directive.ttfb_ms = value.parse().ok()?,
                "dur" => directive.dur_ms = value.parse().ok()?,
                "fault" => directive.fault = Fault::parse(value)?,
                "ra" => directive.retry_after = value.parse().ok()?,
                _ => return None,
            }
        }
        Some(directive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directive_round_trips_and_ignores_the_text_after_it() {
        let directive = Directive {
            out: 120,
            lat_ms: 300,
            ttfb_ms: 200,
            dur_ms: 5_000,
            fault: Fault::Dropped,
            retry_after: 1,
        };
        let text = format!("{} lorem ipsum", directive.render());
        assert_eq!(Directive::parse(&text), Some(directive));
        assert_eq!(Directive::parse("hello"), None);
        assert_eq!(Directive::parse("[[load fault=boom]]"), None);
        for fault in Fault::ALL {
            assert_eq!(Fault::parse(fault.as_str()), Some(fault));
        }
    }
}
