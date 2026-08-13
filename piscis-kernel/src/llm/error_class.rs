//! Central classification of LLM/tool failures into a small, stable set of
//! machine-readable codes. Shared by:
//! - `agent::loop_::is_fallback_eligible_error` (decide whether to try the
//!   next fallback model), and
//! - `AgentEvent::Error.code` (surfaced to the frontend so it can render a
//!   category-specific message instead of a raw upstream string).
//!
//! Classification is heuristic — it works on the already-flattened error
//! `String` (produced by `anyhow`/`format!` call sites across the client
//! implementations), not on structured HTTP responses. This keeps it usable
//! from any call site without threading status codes through every error
//! path, at the cost of being best-effort rather than exhaustive.

use serde::{Deserialize, Serialize};

/// Stable machine-readable error codes shared across piscis-kernel,
/// DimWork and DimRouter/DimSDK (see plan doc "错误契约").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// The requested model/deployment is not supported by the configured
    /// account/group, or does not exist in the provider's or DimRouter's
    /// catalogue. Eligible for automatic fallback to another model.
    ModelUnavailable,
    /// A tool call's `arguments` JSON was missing, truncated, or otherwise
    /// failed to parse. Must never be silently treated as `{}`.
    ToolArgsInvalid,
    /// The upstream response violated the expected protocol shape (empty
    /// `choices`, stream ended without `[DONE]`/`finish_reason`, undecodable
    /// body, etc.) rather than reporting a normal error.
    ProtocolError,
    /// Provider-side rate limiting. Eligible for automatic fallback.
    RateLimited,
    /// Authentication/authorization failure (bad/expired API key, etc.).
    /// Not eligible for fallback — switching models won't help.
    AuthFailed,
    /// Transient upstream failure (timeout, connection reset, 5xx,
    /// mid-stream disconnects). Should be retried with backoff on the same
    /// model, not switched away from.
    UpstreamTransient,
    /// Doesn't match any known category.
    Unknown,
}

impl ErrorClass {
    /// The stable string code used in `AgentEvent::Error.code`, DimWork's
    /// `formatChatError`, and platform error envelopes.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ModelUnavailable => "model_unavailable",
            Self::ToolArgsInvalid => "tool_args_invalid",
            Self::ProtocolError => "protocol_error",
            Self::RateLimited => "rate_limited",
            Self::AuthFailed => "auth_failed",
            Self::UpstreamTransient => "upstream_transient",
            Self::Unknown => "unknown",
        }
    }

    /// True if a caller should try the next configured fallback model
    /// instead of retrying the same model or giving up outright.
    ///
    /// Intentionally excludes `UpstreamTransient` ("overloaded", timeouts,
    /// 502/503/529, mid-stream disconnects): those are retried with
    /// exponential backoff on the *same* model by the caller, since they
    /// say nothing about whether the model itself is usable.
    pub fn is_fallback_eligible(&self) -> bool {
        matches!(self, Self::ModelUnavailable | Self::RateLimited)
    }
}

/// Best-effort extraction of a leading HTTP status code from error strings
/// shaped like `"OpenAI API error 404 Not Found: {...}"` (see
/// `llm::openai::OpenAiClient`) or `"... error 429: ..."`.
fn extract_http_status(lower_msg: &str) -> Option<u16> {
    let idx = lower_msg.find("error ")?;
    let rest = &lower_msg[idx + "error ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

/// Classify a flattened error message string into an [`ErrorClass`].
///
/// This is intentionally conservative about `ModelUnavailable`: a bare 404
/// is not enough (many providers 404 for unrelated reasons), so we require
/// either an explicit model-not-found phrase or a 404 whose body also
/// mentions "model".
pub fn classify_error(msg: &str) -> ErrorClass {
    let lower = msg.to_lowercase();
    let status = extract_http_status(&lower);

    if lower.contains("tool_args_invalid") {
        return ErrorClass::ToolArgsInvalid;
    }

    if lower.contains("model_not_found")
        || lower.contains("model not found")
        || lower.contains("model_unavailable")
        || lower.contains("does not exist")
        || lower.contains("not supported by any configured account")
        || lower.contains("not in the catalogue")
        || lower.contains("not in the catalog")
        || (status == Some(404) && lower.contains("model"))
    {
        return ErrorClass::ModelUnavailable;
    }

    if lower.contains("rate_limit") || lower.contains("rate limit") || status == Some(429) {
        return ErrorClass::RateLimited;
    }

    if lower.contains("auth_failed")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("incorrect api key")
        || status == Some(401)
        || status == Some(403)
    {
        return ErrorClass::AuthFailed;
    }

    if lower.contains("protocol_error")
        || lower.contains("missing 'choices'")
        || lower.contains("returned empty choices")
        || lower.contains("json decode error")
        || lower.contains("response json decode error")
    {
        return ErrorClass::ProtocolError;
    }

    if lower.contains("timeout")
        || lower.contains("connection")
        || lower.contains("overloaded")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("529")
        || lower.contains("error decoding response body")
        || lower.contains("incomplete message")
        || lower.contains("unexpected eof")
        || lower.contains("broken pipe")
    {
        return ErrorClass::UpstreamTransient;
    }

    ErrorClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_group_404_as_model_unavailable() {
        // Real-world sample reported by a user via a compatible-API proxy group.
        let msg = r#"OpenAI API error 404 Not Found: {"error":{"message":"Model \"gpt-5.2\" is not supported by any configured account in this group","type":"model_not_found","param":null,"code":null}}"#;
        assert_eq!(classify_error(msg), ErrorClass::ModelUnavailable);
        assert!(classify_error(msg).is_fallback_eligible());
    }

    #[test]
    fn classifies_explicit_model_not_found_phrase() {
        assert_eq!(
            classify_error("upstream said: model_not_found"),
            ErrorClass::ModelUnavailable
        );
        assert_eq!(
            classify_error("Error: model not found for this account"),
            ErrorClass::ModelUnavailable
        );
    }

    #[test]
    fn classifies_does_not_exist_as_model_unavailable() {
        assert_eq!(
            classify_error("The model `gpt-9` does not exist"),
            ErrorClass::ModelUnavailable
        );
    }

    #[test]
    fn classifies_router_catalogue_miss() {
        assert_eq!(
            classify_error("requested model is not in the catalogue"),
            ErrorClass::ModelUnavailable
        );
    }

    #[test]
    fn bare_404_without_model_mention_is_not_model_unavailable() {
        // A 404 that has nothing to do with the model (e.g. wrong path) must
        // not be misclassified — otherwise callers would burn fallback
        // attempts for unrelated failures.
        let msg = "OpenAI API error 404 Not Found: {\"error\":\"route not found\"}";
        assert_ne!(classify_error(msg), ErrorClass::ModelUnavailable);
    }

    #[test]
    fn classifies_rate_limit_variants() {
        assert_eq!(classify_error("rate_limit_exceeded"), ErrorClass::RateLimited);
        assert_eq!(
            classify_error("You have hit the rate limit, slow down"),
            ErrorClass::RateLimited
        );
        assert_eq!(
            classify_error("OpenAI API error 429 Too Many Requests: {}"),
            ErrorClass::RateLimited
        );
        assert!(classify_error("rate_limit_exceeded").is_fallback_eligible());
    }

    #[test]
    fn classifies_auth_failures_as_not_fallback_eligible() {
        let class = classify_error("OpenAI API error 401 Unauthorized: invalid api key provided");
        assert_eq!(class, ErrorClass::AuthFailed);
        assert!(!class.is_fallback_eligible());
    }

    #[test]
    fn classifies_transient_upstream_errors() {
        for msg in [
            "request timeout after 120s",
            "OpenAI API error 503 Service Unavailable: overloaded",
            "error decoding response body: unexpected eof",
            "connection reset by peer",
        ] {
            let class = classify_error(msg);
            assert_eq!(class, ErrorClass::UpstreamTransient, "msg={msg}");
            assert!(!class.is_fallback_eligible(), "msg={msg}");
        }
    }

    #[test]
    fn classifies_protocol_errors() {
        assert_eq!(
            classify_error("OpenAI response returned empty choices"),
            ErrorClass::ProtocolError
        );
        assert_eq!(
            classify_error("OpenAI response missing 'choices' field"),
            ErrorClass::ProtocolError
        );
    }

    #[test]
    fn unknown_falls_through() {
        assert_eq!(classify_error("something totally unexpected"), ErrorClass::Unknown);
        assert!(!classify_error("something totally unexpected").is_fallback_eligible());
    }

    #[test]
    fn tool_args_invalid_takes_priority() {
        // Constructed by the tool-json handling (p0-tool-json); make sure the
        // marker itself round-trips even if the message also mentions other
        // keywords incidentally.
        assert_eq!(
            classify_error("tool_args_invalid: failed to parse arguments for file_write"),
            ErrorClass::ToolArgsInvalid
        );
    }
}
