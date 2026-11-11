//! Reasoning-effort shaping, dialect by dialect.
//!
//! `OPENCODE_REASONING_EFFORT` is one explicit, validated value that EE sends
//! only through fields the routed dialect documents:
//!
//! | Dialect | Field |
//! |---|---|
//! | OpenAI Responses | `reasoning.effort` |
//! | OpenAI-compatible Chat Completions | `reasoning_effort` |
//! | Anthropic Messages | none — the dialect takes a token budget, not an effort level |
//!
//! The Messages dialect is not guessed at: EE will not send an unverified field
//! or invent a budget mapping, so the knob is reported as not applied for those
//! routes and every other request field stays exactly as it is without the
//! setting. Nothing here can choose an endpoint, a dialect, or a model.

use std::str::FromStr;

use serde_json::{Value, json};

use crate::routes::OpenCodeDialect;

/// Supported reasoning-effort levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Minimal reasoning budget.
    Low,
    /// Balanced reasoning budget.
    Medium,
    /// Maximum reasoning budget.
    High,
}

impl ReasoningEffort {
    /// Wire value sent to the provider.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parses one effort level, case-insensitively.
    ///
    /// # Errors
    ///
    /// Returns guidance naming the accepted values; a rejected value never
    /// reaches a request.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "unsupported reasoning effort {other:?}: expected \"low\", \"medium\", or \"high\""
            )),
        }
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// Whether a dialect documents a reasoning-effort field at all.
#[must_use]
pub const fn supports_effort(dialect: OpenCodeDialect) -> bool {
    match dialect {
        OpenCodeDialect::OpenAiResponses | OpenCodeDialect::OpenAiChatCompletions => true,
        OpenCodeDialect::AnthropicMessages => false,
    }
}

/// Request-body fields carrying one effort setting for `dialect`.
///
/// Returns `None` when the dialect documents no such field, which is a
/// deliberate no-op: the request is sent unchanged rather than with a guessed
/// field.
#[must_use]
pub fn request_extensions(dialect: OpenCodeDialect, effort: ReasoningEffort) -> Option<Value> {
    match dialect {
        OpenCodeDialect::OpenAiResponses => {
            Some(json!({ "reasoning": { "effort": effort.as_str() } }))
        }
        OpenCodeDialect::OpenAiChatCompletions => {
            Some(json!({ "reasoning_effort": effort.as_str() }))
        }
        OpenCodeDialect::AnthropicMessages => None,
    }
}

/// Bounded operator note explaining why the knob is not applied to `dialect`.
#[must_use]
pub fn unsupported_note(dialect: OpenCodeDialect) -> String {
    format!(
        "reasoning effort is not applied to the OpenCode {} dialect: it takes a token budget rather \
         than an effort level, and ee never sends an unverified field. Requests stay unchanged",
        dialect.as_str()
    )
}

#[cfg(test)]
mod tests;
