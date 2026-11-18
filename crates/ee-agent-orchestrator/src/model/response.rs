//! The normalized model response and its builders.

use serde::{Deserialize, Serialize};

use crate::opaque_state::OpaqueProviderState;
use crate::tools::ToolIntent;
use crate::usage::ModelUsage;

/// Normalized model response for one completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelResponse {
    /// Assistant text (empty when the model only reasoned or called tools).
    pub text: String,
    /// Reasoning text, kept separate from the answer.
    pub reasoning: Option<String>,
    /// Tool intents the model wants executed, in order.
    pub tool_intents: Vec<ToolIntent>,
    /// Whether the model signals turn completion.
    pub completed: bool,
    /// Provider stop reason as reported (for example `stop`, `length`, or
    /// `tool_calls`); `None` when the adapter could not report one.
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Opaque provider reasoning state for the next turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<OpaqueProviderState>,
    /// Decoded structured-output object, when the request asked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<serde_json::Value>,
    /// Provider response id usable as a continuation anchor, when the route
    /// stores responses server-side. `None` for stateless routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Reported token usage; unknown fields stay `None`.
    pub usage: ModelUsage,
}

impl ModelResponse {
    /// Creates an empty, incomplete response.
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            reasoning: None,
            tool_intents: Vec::new(),
            completed: false,
            finish_reason: None,
            native: None,
            structured_output: None,
            response_id: None,
            usage: ModelUsage::new(),
        }
    }

    /// Sets opaque provider reasoning state for the next request.
    #[must_use]
    pub fn with_native(mut self, native: Option<OpaqueProviderState>) -> Self {
        self.native = native;
        self
    }

    /// Sets the provider response id used for continuation.
    #[must_use]
    pub fn with_response_id(mut self, response_id: Option<String>) -> Self {
        self.response_id = response_id;
        self
    }

    /// Sets the provider stop reason.
    #[must_use]
    pub fn with_finish_reason(mut self, finish_reason: Option<String>) -> Self {
        self.finish_reason = finish_reason;
        self
    }

    /// Sets the reported token usage.
    #[must_use]
    pub fn with_usage(mut self, usage: ModelUsage) -> Self {
        self.usage = usage;
        self
    }

    /// Sets the assistant text.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Sets the reasoning text.
    #[must_use]
    pub fn reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Sets the tool intents.
    #[must_use]
    pub fn tool_intents(mut self, intents: Vec<ToolIntent>) -> Self {
        self.tool_intents = intents;
        self
    }

    /// Marks the response as completing the turn.
    #[must_use]
    pub fn completed(mut self) -> Self {
        self.completed = true;
        self
    }

    /// Whether the response carries nothing (no text, reasoning, or intents).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.reasoning.as_deref().is_none_or(str::is_empty)
            && self.tool_intents.is_empty()
    }
}

impl Default for ModelResponse {
    fn default() -> Self {
        Self::new()
    }
}
