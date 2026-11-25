//! What a Gemini deployment on Agent Platform names, and how the gateway
//! calls it (`docs/custom-deployments-design.md`).
//!
//! A deployment of a Gemini account names one of three things:
//!
//! - a Google model (`gemini-2.5-flash`), called through Gemini's API;
//! - a Model Garden partner model (`anthropic/claude-haiku-4-5`), called
//!   through its publisher's own API, a fixed table in code
//!   ([`publisher_wire_api`]);
//! - one of the project's own endpoints (`endpoints/<id>`), called through
//!   whatever it serves, which the adapter learns from the endpoint itself.
//!
//! The catalog refuses a partner model whose publisher has no wire API, and
//! an exact input count where the wire API has no free count.

use serde::{Deserialize, Serialize};

/// How the gateway calls a model on Agent Platform.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    /// Gemini's `generateContent` and `streamGenerateContent`.
    Gemini,
    /// Anthropic's Messages API through `rawPredict` and `streamRawPredict`.
    AnthropicMessages,
    /// Mistral's chat API (OpenAI-shaped) through `rawPredict` and
    /// `streamRawPredict`.
    MistralChat,
    /// OpenAI chat completions: Agent Platform's
    /// `endpoints/openapi/chat/completions` for partner models, an
    /// endpoint's own `chat/completions` for an endpoint.
    OpenAiChat,
}

impl WireApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::AnthropicMessages => "anthropic_messages",
            Self::MistralChat => "mistral_chat",
            Self::OpenAiChat => "openai_chat",
        }
    }

    /// Whether the wire API has a free input token count the budgets can use
    /// (`chat_input_count_mode: provider_api_exact`).
    pub fn counts_input_exactly(self) -> bool {
        matches!(self, Self::Gemini | Self::AnthropicMessages)
    }
}

/// The wire API of a Model Garden publisher's models, or `None` for a
/// publisher the gateway does not call. Adding a row is a code change with a
/// test against the publisher's wire format.
pub fn publisher_wire_api(publisher: &str) -> Option<WireApi> {
    match publisher {
        "google" => Some(WireApi::Gemini),
        "anthropic" => Some(WireApi::AnthropicMessages),
        "mistralai" => Some(WireApi::MistralChat),
        "xai" | "meta" | "deepseek-ai" | "qwen" | "openai" => Some(WireApi::OpenAiChat),
        _ => None,
    }
}

/// What a Gemini deployment's model names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentPlatformModel<'a> {
    /// A Google model, by its identifier.
    Google(&'a str),
    /// A partner model, `<publisher>/<model>`, with its publisher's wire API
    /// if the gateway calls it.
    Partner {
        publisher: &'a str,
        model: &'a str,
        wire_api: Option<WireApi>,
    },
    /// One of the project's own endpoints, `endpoints/<id>`.
    Endpoint(&'a str),
}

impl<'a> AgentPlatformModel<'a> {
    /// `None` for an identifier of no known shape: empty parts, more than one
    /// `/`, or characters a URL path segment would need escaped.
    pub fn parse(model: &'a str) -> Option<Self> {
        let segment = |part: &str| {
            !part.is_empty()
                && part.len() <= 128
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
                && part != "."
                && part != ".."
        };
        match model.split_once('/') {
            None => segment(model).then_some(Self::Google(model)),
            Some(("endpoints", id)) => (segment(id)
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            .then_some(Self::Endpoint(id)),
            Some((publisher, rest)) => {
                let publisher_ok = publisher
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
                (publisher_ok && segment(publisher) && segment(rest)).then(|| Self::Partner {
                    publisher,
                    model: rest,
                    wire_api: publisher_wire_api(publisher),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployments_name_a_google_model_a_partner_model_or_an_endpoint() {
        assert_eq!(
            AgentPlatformModel::parse("gemini-2.5-flash"),
            Some(AgentPlatformModel::Google("gemini-2.5-flash"))
        );
        assert_eq!(
            AgentPlatformModel::parse("anthropic/claude-haiku-4-5@20251001"),
            Some(AgentPlatformModel::Partner {
                publisher: "anthropic",
                model: "claude-haiku-4-5@20251001",
                wire_api: Some(WireApi::AnthropicMessages),
            })
        );
        assert_eq!(
            AgentPlatformModel::parse("mistralai/codestral-2"),
            Some(AgentPlatformModel::Partner {
                publisher: "mistralai",
                model: "codestral-2",
                wire_api: Some(WireApi::MistralChat),
            })
        );
        assert_eq!(
            AgentPlatformModel::parse("xai/grok-4.7"),
            Some(AgentPlatformModel::Partner {
                publisher: "xai",
                model: "grok-4.7",
                wire_api: Some(WireApi::OpenAiChat),
            })
        );
        // Listed, but not called.
        assert_eq!(
            AgentPlatformModel::parse("ai21/jamba-1.5"),
            Some(AgentPlatformModel::Partner {
                publisher: "ai21",
                model: "jamba-1.5",
                wire_api: None,
            })
        );
        assert_eq!(
            AgentPlatformModel::parse("endpoints/1234567890"),
            Some(AgentPlatformModel::Endpoint("1234567890"))
        );
        assert_eq!(
            AgentPlatformModel::parse("endpoints/mg-endpoint-1"),
            Some(AgentPlatformModel::Endpoint("mg-endpoint-1"))
        );
        for bad in [
            "",
            "a/b/c",
            "/claude",
            "anthropic/",
            "endpoints/",
            "endpoints/a.b",
            "anthropic/../x",
            "anthropic/..",
            "Anthropic/claude",
            "anthropic/claude?x",
            "gemini 2",
            "endpoints/1:predict",
        ] {
            assert_eq!(AgentPlatformModel::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_gemini_and_anthropic_count_for_free() {
        assert!(WireApi::Gemini.counts_input_exactly());
        assert!(WireApi::AnthropicMessages.counts_input_exactly());
        assert!(!WireApi::MistralChat.counts_input_exactly());
        assert!(!WireApi::OpenAiChat.counts_input_exactly());
        assert_eq!(publisher_wire_api("google"), Some(WireApi::Gemini));
        assert_eq!(publisher_wire_api("cohere"), None);
    }
}
