//! The normalized request handed to a model adapter.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSnapshot;
use crate::model_options::{ResponseFormat, ToolCallControl};
use crate::tasks::TaskNode;
use crate::tools::ToolDefinition;

use super::ModelMessage;

/// Normalized request handed to `ModelAdapter::complete`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRequest {
    /// The conversation so far (prompt, assistant turns, tool observations).
    pub transcript: Vec<ModelMessage>,
    /// The tool schemas available to the model this turn.
    pub tools: Vec<ToolDefinition>,
    /// A snapshot of the budget state before the call.
    pub budget: BudgetSnapshot,
    /// The current task state.
    pub task: TaskNode,
    /// The advertised model list the delegating model can select for
    /// subagents (ids plus display name/capability hints); empty when no
    /// registry is wired.
    #[serde(default)]
    pub available_models: Vec<crate::model_registry::ModelInfo>,
    /// The registry id of the adapter this call runs on (diagnostic-only; the
    /// child's selection is recorded here too).
    #[serde(default)]
    pub model_id: Option<String>,
    /// Prompt size measured by the provider on the previous call, when known.
    /// Adapters fit their output bound against this value; `None` falls back to
    /// `estimate_prompt_tokens`.
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    /// Tool-call control for this request; the default keeps provider behavior.
    #[serde(default)]
    pub tool_control: ToolCallControl,
    /// Structured-output request; `None` sends nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Whether the routed provider accepts a tool list that changes between
    /// steps. The default is `false`: a route that cannot honour the change
    /// fails closed instead of silently sending it.
    #[serde(default)]
    pub allow_tool_list_updates: bool,
    /// Sampling knobs for this call; `None` keeps the provider session default.
    ///
    /// A configured value wins knob by knob over the session default, and an
    /// unset knob emits no body field at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<crate::generation::GenerationOptions>,
    /// Runtime-selected reasoning effort for this call.
    ///
    /// `None` keeps whatever the provider profile configured at startup, so a
    /// session that never selected an effort sends byte-identical requests.
    /// Adapters that document a reasoning field lower this value; adapters that
    /// do not ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Provider-specific request-body extensions for this call.
    ///
    /// The owning adapter builds this overlay (for example an OpenRouter
    /// `reasoning` object) and codecs merge it over the profile extensions.
    /// `None` emits nothing, so providers stay in charge of their own shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extensions: Option<serde_json::Value>,
    /// Session affinity for provider cache routing and per-session limits.
    /// `None` emits no session headers and falls back to the profile cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::session_context::SessionContext>,
    /// Provider-side response continuation for stateful routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
}

impl ModelRequest {
    /// Creates a request from its parts; the advertised model list and the
    /// diagnostic model id are empty/unset until set with the builders.
    #[must_use]
    pub fn new(
        transcript: Vec<ModelMessage>,
        tools: Vec<ToolDefinition>,
        budget: BudgetSnapshot,
        task: TaskNode,
    ) -> Self {
        Self {
            transcript,
            tools,
            budget,
            task,
            available_models: Vec::new(),
            model_id: None,
            prompt_tokens: None,
            tool_control: ToolCallControl::auto(),
            response_format: None,
            allow_tool_list_updates: false,
            generation: None,
            reasoning_effort: None,
            provider_extensions: None,
            session: None,
            continuation: None,
        }
    }

    /// Sets the sampling knobs for this request.
    ///
    /// Per-request values win over the provider's session defaults.
    #[must_use]
    pub fn with_generation(
        mut self,
        generation: Option<crate::generation::GenerationOptions>,
    ) -> Self {
        self.generation = generation;
        self
    }

    /// Sets the tool-call control for this request.
    #[must_use]
    pub fn with_tool_control(mut self, tool_control: ToolCallControl) -> Self {
        self.tool_control = tool_control;
        self
    }

    /// Sets the runtime reasoning-effort selection for this request.
    #[must_use]
    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<String>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    /// Sets the provider-specific request-body extension overlay.
    #[must_use]
    pub fn with_provider_extensions(
        mut self,
        provider_extensions: Option<serde_json::Value>,
    ) -> Self {
        self.provider_extensions = provider_extensions;
        self
    }

    /// Requests structured output.
    #[must_use]
    pub fn with_response_format(mut self, format: Option<ResponseFormat>) -> Self {
        self.response_format = format;
        self
    }

    /// Declares whether the route accepts a mid-conversation tool-list change.
    #[must_use]
    pub fn with_tool_list_updates(mut self, allowed: bool) -> Self {
        self.allow_tool_list_updates = allowed;
        self
    }

    /// Sets the prompt size measured by the provider on the previous call.
    #[must_use]
    pub fn with_prompt_tokens(mut self, prompt_tokens: Option<u64>) -> Self {
        self.prompt_tokens = prompt_tokens;
        self
    }

    /// Sets the advertised model list the delegating model can pick from.
    #[must_use]
    pub fn with_available_models(mut self, models: Vec<crate::model_registry::ModelInfo>) -> Self {
        self.available_models = models;
        self
    }

    /// Sets the diagnostic model id of the adapter this call runs on.
    #[must_use]
    pub fn with_model_id(mut self, model_id: Option<String>) -> Self {
        self.model_id = model_id;
        self
    }

    /// Sets the session affinity carried by this request.
    #[must_use]
    pub fn with_session(mut self, session: Option<crate::session_context::SessionContext>) -> Self {
        self.session = session;
        self
    }

    /// Sets the provider-side continuation anchor for this request.
    #[must_use]
    pub fn with_continuation(mut self, continuation: Option<Continuation>) -> Self {
        self.continuation = continuation;
        self
    }
}

pub use crate::continuation::Continuation;
pub use crate::usage::ModelUsage;

/// Builds an ACP prompt result, attaching reported token usage when known.
pub(crate) fn prompt_result_with_usage(
    stop_reason: ee_agent_protocol::StopReason,
    usage: ModelUsage,
) -> ee_agent_acp_server::PromptResult {
    let mut result = ee_agent_acp_server::PromptResult::new(stop_reason);
    if let Some(usage) = usage.acp_usage() {
        result = result.usage(usage);
    }
    result
}
