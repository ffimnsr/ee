//! Reasoning-effort and reasoning-variant shaping, dialect by dialect.
//!
//! `OPENCODE_MODEL` may carry a `#variant` suffix (`gpt-5.5#high`) and
//! `OPENCODE_REASONING_EFFORT` is a shorthand for the matching variant. Both
//! resolve against the catalog row's derived variants before any request
//! exists.
//!
//! A variant becomes exactly one documented request field:
//!
//! | Dialect | Effort variant | Disabled variant | Budget variant |
//! |---|---|---|---|
//! | OpenAI Responses | `reasoning.effort` | `reasoning.effort: "none"` | n/a |
//! | OpenAI-compatible Chat Completions | `reasoning_effort` | `reasoning_effort: "none"` | n/a |
//! | Anthropic Messages | none (budget protocol) | `thinking.type: "disabled"` | `thinking.budget_tokens` |
//!
//! Nothing here can choose an endpoint, a dialect, or a model.

use std::str::FromStr;

use ee_agent_orchestrator::ReasoningVariant;
use serde_json::{Value, json};

use crate::routes::OpenCodeDialect;

/// Supported reasoning-effort levels.
///
/// The set matches the values models.dev publishes for the routed providers, so
/// every catalog-declared effort is expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Reasoning disabled.
    None,
    /// Minimal reasoning budget.
    Minimal,
    /// Low reasoning budget.
    Low,
    /// Balanced reasoning budget.
    Medium,
    /// High reasoning budget.
    High,
    /// Extra-high reasoning budget.
    Xhigh,
    /// Maximum reasoning budget.
    Max,
}

impl ReasoningEffort {
    /// Wire value sent to the provider.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
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
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "unsupported reasoning effort {other:?}: expected \"none\", \"minimal\", \
                 \"low\", \"medium\", \"high\", \"xhigh\", or \"max\""
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

/// Request-body fields carrying one derived reasoning variant.
///
/// Variants come from the catalog row's declared support, so a field is emitted
/// only for a capability the model documents. `None` means the variant cannot be
/// expressed on this dialect.
#[must_use]
pub fn variant_extensions(dialect: OpenCodeDialect, variant: ReasoningVariant) -> Option<Value> {
    if variant.disabled {
        return match dialect {
            OpenCodeDialect::OpenAiResponses => Some(json!({ "reasoning": { "effort": "none" } })),
            OpenCodeDialect::OpenAiChatCompletions => Some(json!({ "reasoning_effort": "none" })),
            OpenCodeDialect::AnthropicMessages => {
                Some(json!({ "thinking": { "type": "disabled" } }))
            }
        };
    }
    if let Some(budget) = variant.budget_tokens {
        return match dialect {
            OpenCodeDialect::AnthropicMessages => Some(json!({
                "thinking": { "type": "enabled", "budget_tokens": budget }
            })),
            _ => None,
        };
    }
    let effort = variant.effort?;
    match dialect {
        OpenCodeDialect::OpenAiResponses => Some(json!({ "reasoning": { "effort": effort } })),
        OpenCodeDialect::OpenAiChatCompletions => Some(json!({ "reasoning_effort": effort })),
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
