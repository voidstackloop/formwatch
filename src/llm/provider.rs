//! LLM provider request/response shaping.
//!
//! Three providers are supported, all over plain HTTPS with no vendor SDK:
//!
//! - [`Provider::OpenAi`] speaks the widely-cloned Chat Completions API,
//!   so it also works against OpenAI-compatible endpoints (Azure OpenAI,
//!   Ollama, vLLM, LM Studio, Groq, Together, ...) via `--llm-base-url`.
//! - [`Provider::Anthropic`] speaks the Messages API.
//! - [`Provider::Mock`] makes no network call at all and returns a fixed,
//!   well-formed verdict — used by tests and for offline dry runs.
//!
//! Building the request and parsing the response are pure functions so
//! they can be tested without any network access.

use serde::{Deserialize, Serialize};

/// Which LLM backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// OpenAI Chat Completions (and compatible endpoints).
    #[default]
    #[value(name = "openai")]
    OpenAi,
    /// Anthropic Messages API.
    #[value(name = "anthropic")]
    Anthropic,
    /// No network; a deterministic canned verdict (tests/offline).
    #[value(name = "mock")]
    Mock,
}

impl Provider {
    /// The model used when the operator doesn't name one.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::OpenAi => "gpt-4o-mini",
            Provider::Anthropic => "claude-3-5-haiku-latest",
            Provider::Mock => "mock",
        }
    }

    /// The API root used when `--llm-base-url` isn't set. Paths are
    /// appended without a leading slash.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::OpenAi => "https://api.openai.com/v1",
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Mock => "",
        }
    }

    /// Environment variables consulted (in order) for an API key when none
    /// is given on the command line or in config.
    pub fn api_key_envs(self) -> &'static [&'static str] {
        match self {
            Provider::OpenAi => &["FORMWATCH_LLM_API_KEY", "OPENAI_API_KEY"],
            Provider::Anthropic => &["FORMWATCH_LLM_API_KEY", "ANTHROPIC_API_KEY"],
            Provider::Mock => &[],
        }
    }

    /// Whether this provider needs an API key to make a call.
    pub fn requires_api_key(self) -> bool {
        !matches!(self, Provider::Mock)
    }

    /// The wire name (`openai`, `anthropic`, `mock`).
    pub fn label(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Mock => "mock",
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "open-ai" | "open_ai" => Ok(Provider::OpenAi),
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "mock" => Ok(Provider::Mock),
            other => Err(format!(
                "unknown provider {other:?} (expected openai, anthropic, or mock)"
            )),
        }
    }
}

/// A fully-shaped HTTP request for one completion.
pub struct BuiltRequest {
    /// Absolute URL to POST to.
    pub url: String,
    /// Headers to set (lowercase names).
    pub headers: Vec<(String, String)>,
    /// JSON request body.
    pub body: serde_json::Value,
}

/// Builds the provider-specific request for one system/user exchange.
/// `Mock` has no request and returns `Err`.
pub fn build_request(
    provider: Provider,
    base_url: &str,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
) -> std::result::Result<BuiltRequest, String> {
    let base = base_url.trim_end_matches('/');
    match provider {
        Provider::OpenAi => Ok(BuiltRequest {
            url: format!("{base}/chat/completions"),
            headers: vec![
                ("authorization".to_string(), format!("Bearer {api_key}")),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: serde_json::json!({
                "model": model,
                "temperature": 0,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user },
                ],
            }),
        }),
        Provider::Anthropic => Ok(BuiltRequest {
            url: format!("{base}/v1/messages"),
            headers: vec![
                ("x-api-key".to_string(), api_key.to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: serde_json::json!({
                "model": model,
                "max_tokens": 1024,
                "temperature": 0,
                "system": system,
                "messages": [{ "role": "user", "content": user }],
            }),
        }),
        Provider::Mock => Err("the mock provider builds no HTTP request".to_string()),
    }
}

/// Extracts the assistant's text from a provider response body.
pub fn parse_response(provider: Provider, body: &serde_json::Value) -> Option<String> {
    match provider {
        Provider::OpenAi => body
            .get("choices")?
            .as_array()?
            .first()?
            .get("message")?
            .get("content")?
            .as_str()
            .map(str::to_string),
        Provider::Anthropic => body
            .get("content")?
            .as_array()?
            .first()?
            .get("text")?
            .as_str()
            .map(str::to_string),
        Provider::Mock => None,
    }
}

/// Extracts the total token count from a provider response, if reported.
/// Used only for diagnostic accounting — never affects a verdict.
pub fn parse_usage(provider: Provider, body: &serde_json::Value) -> Option<u64> {
    match provider {
        Provider::OpenAi => body.get("usage")?.get("total_tokens")?.as_u64(),
        Provider::Anthropic => {
            let usage = body.get("usage")?;
            let input = usage.get("input_tokens").and_then(|v| v.as_u64())?;
            let output = usage.get("output_tokens").and_then(|v| v.as_u64())?;
            Some(input + output)
        }
        Provider::Mock => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_accepts_aliases_and_rejects_nonsense() {
        assert_eq!("openai".parse::<Provider>(), Ok(Provider::OpenAi));
        assert_eq!("Claude".parse::<Provider>(), Ok(Provider::Anthropic));
        assert_eq!("MOCK".parse::<Provider>(), Ok(Provider::Mock));
        assert!("gemini".parse::<Provider>().is_err());
    }

    #[test]
    fn openai_request_has_bearer_auth_and_both_messages() {
        let req = build_request(
            Provider::OpenAi,
            "https://api.openai.com/v1/",
            "sk-test",
            "gpt-4o-mini",
            "sys",
            "usr",
        )
        .expect("build");
        assert_eq!(req.url, "https://api.openai.com/v1/chat/completions");
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer sk-test")
        );
        assert_eq!(req.body["messages"][0]["content"], "sys");
        assert_eq!(req.body["messages"][1]["content"], "usr");
    }

    #[test]
    fn anthropic_request_uses_x_api_key_and_version_header() {
        let req = build_request(
            Provider::Anthropic,
            "https://api.anthropic.com",
            "sk-ant",
            "claude-3-5-haiku-latest",
            "sys",
            "usr",
        )
        .expect("build");
        assert_eq!(req.url, "https://api.anthropic.com/v1/messages");
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "sk-ant")
        );
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01")
        );
        assert_eq!(req.body["system"], "sys");
        assert_eq!(req.body["messages"][0]["content"], "usr");
    }

    #[test]
    fn responses_are_extracted_from_both_shapes() {
        let openai = serde_json::json!({
            "choices": [{ "message": { "content": "hello" } }]
        });
        assert_eq!(
            parse_response(Provider::OpenAi, &openai).as_deref(),
            Some("hello")
        );

        let anthropic = serde_json::json!({
            "content": [{ "type": "text", "text": "hi there" }]
        });
        assert_eq!(
            parse_response(Provider::Anthropic, &anthropic).as_deref(),
            Some("hi there")
        );

        assert!(parse_response(Provider::OpenAi, &serde_json::json!({})).is_none());
    }

    #[test]
    fn usage_is_summed_for_anthropic_and_read_for_openai() {
        let openai = serde_json::json!({ "usage": { "total_tokens": 123 } });
        assert_eq!(parse_usage(Provider::OpenAi, &openai), Some(123));

        let anthropic =
            serde_json::json!({ "usage": { "input_tokens": 100, "output_tokens": 23 } });
        assert_eq!(parse_usage(Provider::Anthropic, &anthropic), Some(123));

        assert_eq!(parse_usage(Provider::Mock, &openai), None);
        assert_eq!(parse_usage(Provider::OpenAi, &serde_json::json!({})), None);
    }
}
