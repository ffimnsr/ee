//! Tool definitions, intents, results, and side-effect classification.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::destructive_policy::SideEffectSubclass;
use crate::events::EventRecorder;
use crate::model::ModelMessage;
use crate::tasks::TaskNode;
use crate::tool_dependencies::ToolDependency;
use crate::workspace_scope::WorkspaceScope;

/// Boxed future returned by [`ServerTool::execute`].
pub type ToolFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Context supplied to every tool invocation.
///
/// Tools receive the task the current loop is working on, a snapshot of the
/// normalized transcript (used by delegation tools as the child's context),
/// and the turn's event recorder so subagent work stays observable.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolCallContext {
    /// The task the current loop is working on.
    pub task: TaskNode,
    /// Snapshot of the transcript at invocation time.
    pub transcript: Vec<ModelMessage>,
    /// The turn's event recorder, shared with subagents.
    pub events: EventRecorder,
    /// The active workspace scope, when one is configured; delegation passes
    /// it down so subagent scopes narrow, never widen.
    pub scope: Option<WorkspaceScope>,
    /// Registry model id of the adapter the current loop runs on; delegation
    /// uses it as the fallback child adapter when the role selects none.
    pub model_id: Option<String>,
    /// Per-call terminal wait bounds derived from the timeout policy.
    pub terminal_wait: TerminalWaitLimits,
}

/// Default and maximum terminal wait for the built-in terminal lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalWaitLimits {
    /// Wait used when the model supplies no `timeout_ms`.
    pub default: Duration,
    /// Hard ceiling for an explicit `timeout_ms`.
    pub max: Duration,
}

impl Default for TerminalWaitLimits {
    fn default() -> Self {
        Self {
            default: crate::config::DEFAULT_TERMINAL_TIMEOUT,
            max: crate::config::DEFAULT_TERMINAL_MAX_TIMEOUT,
        }
    }
}

/// Side-effect class of a tool, used by the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SideEffectClass {
    /// Reading files or data.
    Read,
    /// Modifying files or content.
    Write,
    /// Running commands or code.
    Execute,
    /// Delegating work to a subagent.
    Delegate,
}

impl SideEffectClass {
    /// Stable lowercase name for schemas and diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Delegate => "delegate",
        }
    }
}

/// Static description of one tool, advertised to the model adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolDefinition {
    /// Unique tool name used by model tool intents.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Flat JSON schema for the tool arguments.
    pub input_schema: serde_json::Value,
    /// Side-effect class driving policy decisions.
    pub side_effect_class: SideEffectClass,
    /// Destructive side-effect subclass, when the tool deletes, overwrites,
    /// kills, or touches the network; denied by default policy.
    pub side_effect_subclass: Option<SideEffectSubclass>,
    /// Whether a trusted editor host owns mutation approval for this tool.
    ///
    /// Only ee proxy tools from the pinned manifest set this. Their side-effect
    /// class remains write or execute for scheduling and audit purposes.
    #[serde(default)]
    pub host_approval: bool,
    /// Capability flags the client must advertise for this tool.
    pub required_capabilities: Vec<String>,
    /// Whether the argument schema is enforced strictly by the provider.
    ///
    /// Only emitted to routes whose catalog advertises `tools.strict`.
    #[serde(default)]
    pub strict: bool,
    /// Free-form input format, when the tool takes raw text instead of JSON.
    ///
    /// Only emitted to routes whose catalog advertises the matching format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ToolFormat>,
    /// Dependency metadata for planned-batch ordering and cache invalidation.
    #[serde(default)]
    pub dependency: ToolDependency,
}

/// Free-form tool input format a provider advertises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToolFormat {
    /// Unconstrained text.
    Text,
    /// A Lark grammar definition.
    Lark {
        /// Grammar source.
        definition: String,
    },
    /// A regular-expression grammar.
    Regex {
        /// Pattern source.
        definition: String,
    },
}

impl ToolFormat {
    /// Stable lowercase name used by catalog capability lists.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Lark { .. } => "lark",
            Self::Regex { .. } => "regex",
        }
    }
}

impl ToolDefinition {
    /// Creates a read-class tool definition with a plain object schema.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({ "type": "object" }),
            side_effect_class: SideEffectClass::Read,
            side_effect_subclass: None,
            host_approval: false,
            required_capabilities: Vec::new(),
            strict: false,
            format: None,
            dependency: ToolDependency::new(),
        }
    }

    /// Validates flat tool arguments against this definition's schema:
    /// arguments must be a JSON object, every `required` property must be
    /// present, and declared property `type`s must match.
    pub fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), String> {
        let Some(map) = arguments.as_object() else {
            return Err("tool arguments must be a JSON object".into());
        };
        let Some(schema) = self.input_schema.as_object() else {
            return Err("tool schema must be a JSON object".into());
        };
        if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
            for name in required {
                let Some(name) = name.as_str() else { continue };
                if !map.contains_key(name) {
                    return Err(format!("missing required argument: {name}"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(serde_json::Value::as_object) {
            for (name, property) in properties {
                let Some(expected) = property.get("type").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let Some(value) = map.get(name) else { continue };
                let matches = match expected {
                    "string" => value.is_string(),
                    "boolean" => value.is_boolean(),
                    "number" => value.is_number(),
                    "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "null" => value.is_null(),
                    _ => true,
                };
                if !matches {
                    return Err(format!("argument {name} must be {expected}"));
                }
            }
        }
        Ok(())
    }

    /// Sets the argument JSON schema.
    #[must_use]
    pub fn input_schema(mut self, schema: serde_json::Value) -> Self {
        self.input_schema = schema;
        self
    }

    /// Sets the side-effect class.
    #[must_use]
    pub fn side_effect_class(mut self, class: SideEffectClass) -> Self {
        self.side_effect_class = class;
        self
    }

    /// Sets the destructive side-effect subclass.
    #[must_use]
    pub fn side_effect_subclass(mut self, subclass: SideEffectSubclass) -> Self {
        self.side_effect_subclass = Some(subclass);
        self
    }

    /// Marks this tool as requiring approval from its trusted editor host.
    #[must_use]
    pub fn host_approval(mut self) -> Self {
        self.host_approval = true;
        self
    }

    /// Sets the required capability flags.
    #[must_use]
    pub fn required_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.required_capabilities = capabilities;
        self
    }

    /// Marks this tool's argument schema as strictly enforced.
    #[must_use]
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    /// Sets the free-form input format.
    #[must_use]
    pub fn format(mut self, format: ToolFormat) -> Self {
        self.format = Some(format);
        self
    }

    /// Sets the dependency metadata (data classes and affected path scope).
    #[must_use]
    pub fn dependency(mut self, dependency: ToolDependency) -> Self {
        self.dependency = dependency;
        self
    }
}

/// One tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolIntent {
    /// The model-supplied tool-call id, used for update correlation.
    pub tool_call_id: String,
    /// The registered tool name.
    pub name: String,
    /// The tool arguments as JSON.
    pub arguments: serde_json::Value,
    /// Whether the provider executed this tool itself. Provider-executed calls
    /// are recorded in the transcript but never dispatched to the local
    /// executor.
    #[serde(default)]
    pub provider_executed: bool,
    /// Provider-supplied result for a provider-executed call, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_result: Option<serde_json::Value>,
    /// Bounded parse failure when the model emitted malformed tool arguments.
    /// The executor returns this text as the tool observation so the model can
    /// correct itself instead of seeing a generic schema error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_error: Option<String>,
}

impl ToolIntent {
    /// Creates a tool intent.
    #[must_use]
    pub fn new(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            name: name.into(),
            arguments,
            provider_executed: false,
            provider_result: None,
            argument_error: None,
        }
    }

    /// Marks the call as provider-executed.
    #[must_use]
    pub fn with_provider_executed(mut self, provider_executed: bool) -> Self {
        self.provider_executed = provider_executed;
        self
    }

    /// Attaches the provider's result payload.
    #[must_use]
    pub fn with_provider_result(mut self, result: serde_json::Value) -> Self {
        self.provider_result = Some(result);
        self
    }

    /// Attaches the bounded parse failure that explains `Null` arguments.
    #[must_use]
    pub fn with_argument_error(mut self, error: impl Into<String>) -> Self {
        self.argument_error = Some(error.into());
        self
    }
}

/// Why a tool result is a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToolErrorKind {
    /// The tool arguments were rejected.
    InvalidArguments,
    /// The tool backend failed.
    Backend,
    /// The tool exceeded its wall-clock limit.
    Timeout,
    /// The tool was cancelled.
    Cancelled,
    /// The policy engine denied execution.
    PermissionDenied,
}

impl ToolErrorKind {
    /// Stable lowercase name for diagnostics and transcript content.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::Backend => "backend",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::PermissionDenied => "permission_denied",
        }
    }
}

/// Normalized outcome of one tool execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolResult {
    /// Whether the tool completed successfully.
    pub success: bool,
    /// Human-readable text output.
    pub text_output: String,
    /// Optional structured output.
    pub structured_output: Option<serde_json::Value>,
    /// Present exactly when `success` is false.
    pub error_kind: Option<ToolErrorKind>,
}

impl ToolResult {
    /// Creates a successful result with text output.
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self { success: true, text_output: text.into(), structured_output: None, error_kind: None }
    }

    /// Builds a successful result with text and structured output.
    #[must_use]
    pub fn success_structured(
        text_output: impl Into<String>,
        structured_output: serde_json::Value,
    ) -> Self {
        Self {
            success: true,
            text_output: text_output.into(),
            structured_output: Some(structured_output),
            error_kind: None,
        }
    }

    /// Builds a failed result with an error kind and message.
    #[must_use]
    pub fn failure(kind: ToolErrorKind, text_output: impl Into<String>) -> Self {
        Self {
            success: false,
            text_output: text_output.into(),
            structured_output: None,
            error_kind: Some(kind),
        }
    }

    /// One-line summary for tool-call updates: output on success, the error
    /// kind and message on failure.
    #[must_use]
    pub fn summary_text(&self) -> String {
        match (self.success, self.error_kind) {
            (true, _) => self.text_output.clone(),
            (false, Some(kind)) => format!("{}: {}", kind.as_str(), self.text_output),
            (false, None) => format!("error: {}", self.text_output),
        }
    }
}

/// One observed tool execution during a turn, recorded for final-response
/// assembly (changed files, validation outcomes) and traceability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolExecutionLogEntry {
    /// Model-supplied tool-call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The tool's side-effect class, when the tool was registered.
    pub side_effect_class: Option<SideEffectClass>,
    /// The arguments the tool ran with.
    pub arguments: serde_json::Value,
    /// Whether the execution succeeded.
    pub success: bool,
    /// Bounded one-line outcome summary.
    pub summary: String,
}
