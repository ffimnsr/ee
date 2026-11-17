//! Tool types, the tool registry, the execution pipeline, and the
//! server-side tool trait.
//!
//! The model adapter returns [`ToolIntent`] values; the [`ToolExecutor`]
//! resolves them against the [`ToolRegistry`], validates argument shape,
//! gates them through the policy engine, executes them (with timeouts and
//! cancellation), emits the tool-call update lifecycle, and returns
//! normalized [`ToolResult`] values to the transcript.  Editor/client
//! operations are performed through the framework's `ClientBridge`;
//! server-side tools implement [`ServerTool`].

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_acp_server::{ClientBridge, ProviderError, UpdateSink, UpdateSinkError};
use ee_agent_protocol::{
    ContentBlock, CreateElicitationRequest, CreateTerminalRequest, ElicitationFormMode,
    ElicitationMode, ElicitationSchema, ElicitationScope, ElicitationSessionScope,
    KillTerminalRequest, ReadTextFileRequest, ReleaseTerminalRequest, SessionId, TerminalId,
    TerminalOutputRequest, TextContent, ToolCallContent, ToolKind, WaitForTerminalExitRequest,
    WriteTextFileRequest,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::decision_log::DecisionLog;
use crate::destructive_policy::SideEffectSubclass;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::model::ModelMessage;
use crate::policy::{PolicyContext, PolicyEngine};
use crate::tasks::TaskNode;
use crate::tool_cache::{ToolResultCache, affected_paths, cache_key, path_argument};
use crate::tool_dependencies::{ToolDataClass, ToolDependency};
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

/// A server-side tool the orchestrator can execute.
///
/// Editor/client operations must go through the supplied [`ClientBridge`];
/// `cancel` flips when the turn is cancelled.
pub trait ServerTool: Send + Sync + 'static {
    /// Static definition advertised to the model adapter.
    fn definition(&self) -> ToolDefinition;

    /// Executes the tool with validated, policy-approved arguments.
    fn execute(
        &self,
        arguments: serde_json::Value,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult>;
}

/// Registry of server-side tools, keyed by unique name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn ServerTool>>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool; rejects empty and duplicate names.
    pub fn register(&mut self, tool: Arc<dyn ServerTool>) -> Result<(), OrchestratorError> {
        let definition = tool.definition();
        if definition.name.is_empty() {
            return Err(OrchestratorError::InvalidState("tool name must not be empty".into()));
        }
        if self.tools.contains_key(&definition.name) {
            return Err(OrchestratorError::InvalidState(format!(
                "duplicate tool name: {}",
                definition.name
            )));
        }
        self.tools.insert(definition.name, tool);
        Ok(())
    }

    /// Looks up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ServerTool>> {
        self.tools.get(name).cloned()
    }

    /// Removes a tool by name (per-prompt MCP tools deregister at prompt
    /// end); returns the removed tool, if any.
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn ServerTool>> {
        self.tools.remove(name)
    }

    /// Definitions of all registered tools, sorted by name for deterministic
    /// model requests.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> =
            self.tools.values().map(|tool| tool.definition()).collect();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }

    /// Names of all registered tools, sorted (tests and diagnostics).
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Registers the built-in client-bridge tools bound to `session_id`:
    /// `read_file`, `write_file`, the terminal lifecycle tools, and
    /// `ask_user`.
    ///
    /// Fails closed on name conflicts with already-registered tools.
    pub fn register_builtins(&mut self, session_id: &SessionId) -> Result<(), OrchestratorError> {
        for tool in builtin_tools(session_id) {
            self.register(tool)?;
        }
        Ok(())
    }
}

/// Which framework `ClientBridge` method a built-in tool wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeCall {
    ReadTextFile,
    WriteTextFile,
    CreateTerminal,
    TerminalOutput,
    WaitForTerminalExit,
    KillTerminal,
    ReleaseTerminal,
    CreateElicitation,
}

/// A built-in tool performing editor/client operations through the
/// framework's [`ClientBridge`].
#[derive(Clone)]
pub(crate) struct ClientBridgeTool {
    session_id: SessionId,
    call: BridgeCall,
    definition: ToolDefinition,
}

impl ClientBridgeTool {
    fn new(call: BridgeCall, session_id: &SessionId, definition: ToolDefinition) -> Self {
        Self { session_id: session_id.clone(), call, definition }
    }

    async fn run(
        call: BridgeCall,
        session_id: SessionId,
        arguments: serde_json::Value,
        client: &ClientBridge,
        terminal_wait: TerminalWaitLimits,
    ) -> Result<ToolResult, ToolResult> {
        let outcome = match call {
            BridgeCall::ReadTextFile => {
                let path = string_arg(&arguments, "path")?;
                let response = client
                    .read_text_file(ReadTextFileRequest::new(session_id, PathBuf::from(path)))
                    .await;
                match response {
                    Ok(response) => ToolResult::success(response.content),
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::WriteTextFile => {
                let path = string_arg(&arguments, "path")?;
                let content = string_arg(&arguments, "content")?;
                let response = client
                    .write_text_file(WriteTextFileRequest::new(
                        session_id,
                        PathBuf::from(path),
                        content,
                    ))
                    .await;
                match response {
                    Ok(_) => ToolResult::success("file written"),
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::CreateTerminal => {
                let command = string_arg(&arguments, "command")?;
                let mut request = CreateTerminalRequest::new(session_id, command);
                if let Some(cwd) = optional_string_arg(&arguments, "cwd") {
                    request = request.cwd(PathBuf::from(cwd));
                }
                match client.create_terminal(request).await {
                    Ok(response) => {
                        let terminal_id = response.terminal_id.0.as_ref();
                        ToolResult::success_structured(
                            format!("terminal created: {terminal_id}"),
                            serde_json::json!({ "terminal_id": terminal_id }),
                        )
                    }
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::TerminalOutput => {
                let terminal_id = terminal_arg(&arguments)?;
                match client
                    .terminal_output(TerminalOutputRequest::new(session_id, terminal_id))
                    .await
                {
                    Ok(response) => ToolResult::success_structured(
                        response.output.clone(),
                        serde_json::json!({ "output": response.output, "truncated": response.truncated }),
                    ),
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::WaitForTerminalExit => {
                let terminal_id = terminal_arg(&arguments)?;
                let timeout = terminal_wait_timeout(&arguments, terminal_wait)?;
                let wait = client.wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                ));
                tokio::pin!(wait);

                tokio::select! {
                    response = &mut wait => match response {
                        Ok(response) => ToolResult::success_structured(
                            "terminal exited",
                            serde_json::to_value(response.exit_status)
                                .unwrap_or_else(|_| serde_json::json!(null)),
                        ),
                        Err(error) => bridge_error(error),
                    },
                    () = tokio::time::sleep(timeout) => {
                        let timeout_ms = timeout.as_millis() as u64;
                        if let Err(error) = client
                            .kill_terminal(KillTerminalRequest::new(session_id.clone(), terminal_id.clone()))
                            .await
                        {
                            // ACP release also kills a still-running terminal. Attempt it even
                            // when the explicit kill failed so timeout cleanup cannot leak work.
                            let _ = client
                                .release_terminal(ReleaseTerminalRequest::new(session_id, terminal_id))
                                .await;
                            return Ok(bridge_error(error));
                        }

                        let output = client
                            .terminal_output(TerminalOutputRequest::new(session_id.clone(), terminal_id.clone()))
                            .await;
                        let released = client
                            .release_terminal(ReleaseTerminalRequest::new(session_id, terminal_id))
                            .await;

                        match (output, released) {
                            (Ok(output), Ok(_)) => ToolResult::success_structured(
                                format!("terminal timed out after {timeout_ms}ms and was terminated"),
                                serde_json::json!({
                                    "timed_out": true,
                                    "timeout_ms": timeout_ms,
                                    "output": output.output,
                                    "truncated": output.truncated,
                                    "exit_status": output.exit_status,
                                    "released": true,
                                }),
                            ),
                            (Err(error), _) => bridge_error(error),
                            (_, Err(error)) => bridge_error(error),
                        }
                    }
                }
            }
            BridgeCall::KillTerminal => {
                let terminal_id = terminal_arg(&arguments)?;
                let response =
                    client.kill_terminal(KillTerminalRequest::new(session_id, terminal_id)).await;
                match response {
                    Ok(_) => ToolResult::success("terminal killed"),
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::ReleaseTerminal => {
                let terminal_id = terminal_arg(&arguments)?;
                let response = client
                    .release_terminal(ReleaseTerminalRequest::new(session_id, terminal_id))
                    .await;
                match response {
                    Ok(_) => ToolResult::success("terminal released"),
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::CreateElicitation => {
                let message = string_arg(&arguments, "message")?;
                let scope = ElicitationScope::Session(ElicitationSessionScope::new(session_id));
                let mode = ElicitationMode::Form(ElicitationFormMode::new(
                    scope,
                    ElicitationSchema::new(),
                ));
                let response =
                    client.create_elicitation(CreateElicitationRequest::new(mode, message)).await;
                match response {
                    Ok(_) => ToolResult::success("elicitation created"),
                    Err(error) => bridge_error(error),
                }
            }
        };
        Ok(outcome)
    }
}

impl ServerTool for ClientBridgeTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let call = self.call;
        let session_id = self.session_id.clone();
        Box::pin(async move {
            Self::run(call, session_id, arguments, &client, context.terminal_wait)
                .await
                .unwrap_or_else(|result| result)
        })
    }
}

/// Fixed validation command routed through the existing approved terminal
/// lifecycle. The command is code-owned; callers cannot replace its argv.
#[derive(Clone)]
struct CargoCheckTool {
    session_id: SessionId,
}

impl ServerTool for CargoCheckTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("cargo_check", "Runs focused cargo check through editor terminal")
            .side_effect_class(SideEffectClass::Execute)
            .input_schema(serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": [],
            }))
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let session_id = self.session_id.clone();
        let wait_timeout = context.terminal_wait.default;
        Box::pin(async move {
            if *cancel.borrow() {
                return ToolResult::failure(ToolErrorKind::Cancelled, "validation cancelled");
            }
            let mut request = CreateTerminalRequest::new(session_id.clone(), "cargo check --quiet");
            if let Some(path) = optional_string_arg(&arguments, "path")
                && let Some(parent) =
                    Path::new(&path).parent().filter(|parent| parent.is_absolute())
            {
                request = request.cwd(parent.to_path_buf());
            }
            let terminal = match client.create_terminal(request).await {
                Ok(response) => response.terminal_id,
                Err(error) => return bridge_error(error),
            };
            let wait = client.wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                terminal.clone(),
            ));
            let mut cancel_wait = cancel.clone();
            let waited = tokio::select! {
                result = tokio::time::timeout(wait_timeout, wait) => result,
                changed = cancel_wait.changed() => {
                    let _ = changed;
                    let _ = client.kill_terminal(KillTerminalRequest::new(session_id.clone(), terminal.clone())).await;
                    let _ = client.release_terminal(ReleaseTerminalRequest::new(session_id, terminal)).await;
                    return ToolResult::failure(ToolErrorKind::Cancelled, "validation cancelled");
                }
            };
            let wait = match waited {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return bridge_error(error),
                Err(_) => {
                    let _ = client
                        .kill_terminal(KillTerminalRequest::new(
                            session_id.clone(),
                            terminal.clone(),
                        ))
                        .await;
                    let _ = client
                        .release_terminal(ReleaseTerminalRequest::new(session_id, terminal))
                        .await;
                    return ToolResult::failure(ToolErrorKind::Timeout, "cargo check timed out");
                }
            };
            let output = client
                .terminal_output(TerminalOutputRequest::new(session_id.clone(), terminal.clone()))
                .await;
            let _ =
                client.release_terminal(ReleaseTerminalRequest::new(session_id, terminal)).await;
            let output = match output {
                Ok(output) => output,
                Err(error) => return bridge_error(error),
            };
            let exit_code = serde_json::to_value(wait.exit_status)
                .ok()
                .and_then(|value| {
                    value
                        .get("exitCode")
                        .or_else(|| value.get("exit_code"))
                        .and_then(serde_json::Value::as_u64)
                })
                .and_then(|code| i32::try_from(code).ok());
            if exit_code == Some(0) {
                ToolResult::success_structured(
                    "cargo check passed",
                    serde_json::json!({
                        "exit_status": exit_code,
                        "output": output.output,
                        "truncated": output.truncated,
                    }),
                )
            } else {
                ToolResult::failure(
                    ToolErrorKind::Backend,
                    format!(
                        "cargo check failed (exit: {}): {}",
                        exit_code.map_or_else(|| "unknown".into(), |code| code.to_string()),
                        output.output
                    ),
                )
            }
        })
    }
}

/// Read-only skill manifest loader.
///
/// The prompt advertises a bounded skill catalog; the model reads a body only
/// when it needs one. Paths are resolved against the active workspace scope
/// and canonicalized so symlink escapes are rejected.
#[derive(Clone)]
struct LoadSkillTool;

impl ServerTool for LoadSkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "load_skill",
            "Loads the full SKILL.md for a skill listed in the prompt catalog",
        )
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "path": { "type": "string" },
            },
            "required": [],
        }))
        .dependency(ToolDependency::new().produces(vec![ToolDataClass::FileText]))
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        Box::pin(async move {
            let name = optional_string_arg(&arguments, "name");
            let path = optional_string_arg(&arguments, "path");
            if name.as_deref().is_none_or(str::is_empty)
                && path.as_deref().is_none_or(str::is_empty)
            {
                return invalid_args(String::from(
                    "load_skill requires a skill name or path argument",
                ));
            }
            let roots = context
                .scope
                .as_ref()
                .map(|scope| scope.allowed_roots.clone())
                .filter(|roots| !roots.is_empty())
                .or_else(|| std::env::current_dir().ok().map(|cwd| vec![cwd]))
                .unwrap_or_default();
            let cwd = roots.first().cloned().unwrap_or_default();
            let config_root = crate::prompt::skills::user_config_root_from_env();
            match crate::prompt::skills::load_skill(
                name.as_deref(),
                path.as_deref(),
                &cwd,
                &roots,
                config_root.as_deref(),
            ) {
                Ok(skill) => {
                    let bytes = skill.content.len();
                    ToolResult::success_structured(
                        skill.content,
                        serde_json::json!({
                            "name": skill.name,
                            "path": skill.path.display().to_string(),
                            "scope": skill.scope.label(),
                            "bytes": bytes,
                        }),
                    )
                }
                Err(reason) => ToolResult::failure(ToolErrorKind::InvalidArguments, reason),
            }
        })
    }
}

/// All built-in tools, bound to the given session.
fn builtin_tools(session_id: &SessionId) -> Vec<Arc<dyn ServerTool>> {
    let schema = |properties: &[(&str, &str)], required: &[&str]| {
        serde_json::json!({
            "type": "object",
            "properties": properties.iter().map(|(name, kind)| (name, serde_json::json!({ "type": kind }))).collect::<HashMap<_, _>>(),
            "required": required,
        })
    };
    let make = |call: BridgeCall,
                name: &'static str,
                description: &'static str,
                class: SideEffectClass,
                subclass: Option<SideEffectSubclass>,
                dependency: ToolDependency,
                input_schema: serde_json::Value| {
        Arc::new(ClientBridgeTool::new(
            call,
            session_id,
            ToolDefinition {
                name: name.into(),
                description: description.into(),
                input_schema,
                side_effect_class: class,
                side_effect_subclass: subclass,
                host_approval: false,
                required_capabilities: Vec::new(),
                strict: false,
                format: None,
                dependency,
            },
        )) as Arc<dyn ServerTool>
    };
    let none = ToolDependency::new();
    vec![
        Arc::new(CargoCheckTool { session_id: session_id.clone() }) as Arc<dyn ServerTool>,
        Arc::new(LoadSkillTool) as Arc<dyn ServerTool>,
        make(
            BridgeCall::ReadTextFile,
            "read_file",
            "Reads a text file through the editor",
            SideEffectClass::Read,
            None,
            none.clone().produces(vec![ToolDataClass::FileText]),
            schema(&[("path", "string")], &["path"]),
        ),
        make(
            BridgeCall::WriteTextFile,
            "write_file",
            "Writes a text file through the editor",
            SideEffectClass::Write,
            Some(SideEffectSubclass::Overwrite),
            none.clone().produces(vec![ToolDataClass::FileText]),
            schema(&[("path", "string"), ("content", "string")], &["path", "content"]),
        ),
        make(
            BridgeCall::CreateTerminal,
            "create_terminal",
            "Creates a terminal running a command; background jobs are polled between model steps and report exit status automatically.",
            SideEffectClass::Execute,
            None,
            none.clone().produces(vec![ToolDataClass::TerminalHandle]),
            schema(
                &[("command", "string"), ("cwd", "string"), ("background", "boolean")],
                &["command"],
            ),
        ),
        make(
            BridgeCall::TerminalOutput,
            "terminal_output",
            "Fetches a terminal's current output",
            SideEffectClass::Execute,
            None,
            none.clone()
                .requires(vec![ToolDataClass::TerminalHandle])
                .produces(vec![ToolDataClass::TerminalOutput]),
            schema(&[("terminal_id", "string")], &["terminal_id"]),
        ),
        make(
            BridgeCall::WaitForTerminalExit,
            "wait_for_terminal_exit",
            "Waits for a terminal command to exit; timeout_ms triggers kill, output capture, and release.",
            SideEffectClass::Execute,
            None,
            none.clone()
                .requires(vec![ToolDataClass::TerminalHandle])
                .produces(vec![ToolDataClass::TerminalExit, ToolDataClass::TerminalOutput]),
            schema(&[("terminal_id", "string"), ("timeout_ms", "integer")], &["terminal_id"]),
        ),
        make(
            BridgeCall::KillTerminal,
            "kill_terminal",
            "Kills a terminal without releasing it",
            SideEffectClass::Execute,
            Some(SideEffectSubclass::TerminalKill),
            none.clone().requires(vec![ToolDataClass::TerminalHandle]),
            schema(&[("terminal_id", "string")], &["terminal_id"]),
        ),
        make(
            BridgeCall::ReleaseTerminal,
            "release_terminal",
            "Releases a terminal and its resources",
            SideEffectClass::Execute,
            None,
            none.clone().requires(vec![ToolDataClass::TerminalHandle]),
            schema(&[("terminal_id", "string")], &["terminal_id"]),
        ),
        make(
            BridgeCall::CreateElicitation,
            "ask_user",
            "Asks the user for input through the editor",
            SideEffectClass::Read,
            None,
            none.produces(vec![ToolDataClass::UserInput]),
            schema(&[("message", "string")], &["message"]),
        ),
    ]
}

fn string_arg(arguments: &serde_json::Value, name: &str) -> Result<String, ToolResult> {
    arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_args(format!("tool argument {name} must be a string")))
}

fn optional_string_arg(arguments: &serde_json::Value, name: &str) -> Option<String> {
    arguments.get(name).and_then(serde_json::Value::as_str).map(str::to_string)
}

fn terminal_arg(arguments: &serde_json::Value) -> Result<TerminalId, ToolResult> {
    string_arg(arguments, "terminal_id").map(TerminalId::new)
}

fn terminal_wait_timeout(
    arguments: &serde_json::Value,
    limits: TerminalWaitLimits,
) -> Result<Duration, ToolResult> {
    let Some(timeout_ms) = arguments.get("timeout_ms") else {
        return Ok(limits.default);
    };
    let timeout_ms = timeout_ms.as_u64().filter(|timeout_ms| *timeout_ms > 0).ok_or_else(|| {
        invalid_args(String::from("tool argument timeout_ms must be a positive integer"))
    })?;
    let max_ms = limits.max.as_millis() as u64;
    if timeout_ms > max_ms {
        return Err(invalid_args(format!("tool argument timeout_ms must not exceed {max_ms}")));
    }
    Ok(Duration::from_millis(timeout_ms))
}

fn invalid_args(message: String) -> ToolResult {
    ToolResult::failure(ToolErrorKind::InvalidArguments, message)
}

/// Maps a `ClientBridge` failure onto a normalized tool result.
fn bridge_error(error: ProviderError) -> ToolResult {
    let kind = match &error {
        ProviderError::PermissionDenied(_) => ToolErrorKind::PermissionDenied,
        ProviderError::InvalidRequest(_) => ToolErrorKind::InvalidArguments,
        ProviderError::Cancellation => ToolErrorKind::Cancelled,
        ProviderError::BackendFailure(_) | ProviderError::ClientRequestFailure(_) => {
            ToolErrorKind::Backend
        }
        // Recoverable interruptions, rate limits, and transient failures
        // are backend-class tool failures from the bridge's perspective.
        ProviderError::Recoverable(_)
        | ProviderError::RateLimited { .. }
        | ProviderError::Transient { .. }
        | ProviderError::QuotaExceeded { .. }
        | ProviderError::Transport { .. }
        | ProviderError::ContextOverflow(_)
        | ProviderError::PayloadTooLarge(_) => ToolErrorKind::Backend,
        ProviderError::ContentPolicy(_) => ToolErrorKind::InvalidArguments,
        ProviderError::StaleContinuation(_) => ToolErrorKind::Backend,
    };
    ToolResult::failure(kind, error.to_string())
}

/// Executes one tool intent end to end: lookup, argument validation, policy
/// gate, budget reservation, update lifecycle, bounded run, and outcome
/// update.  Returns the normalized [`ToolResult`] for the transcript, or a
/// loop-stopping error on cancellation and budget exhaustion.
#[derive(Clone)]
pub struct ToolExecutor {
    config: OrchestratorConfig,
    tools: Arc<Mutex<ToolRegistry>>,
    budget: Arc<Mutex<BudgetTracker>>,
    policy: PolicyEngine,
    depth: usize,
    events: EventRecorder,
    cache: Option<Arc<Mutex<ToolResultCache>>>,
    model_id: Option<String>,
    background_jobs: Option<crate::background_jobs::SharedBackgroundJobs>,
    workspace_remap: Option<Arc<crate::workspace_isolation::WorkspacePathRemap>>,
    approval_classifier: Option<Arc<crate::approval_classifier::ApprovalClassifier>>,
    decisions: Option<Arc<Mutex<DecisionLog>>>,
}

impl ToolExecutor {
    /// Creates an executor sharing the runtime's stores, evaluating policy at
    /// the given subagent depth and recording tool lifecycle events.
    #[must_use]
    pub fn new(
        config: OrchestratorConfig,
        tools: Arc<Mutex<ToolRegistry>>,
        budget: Arc<Mutex<BudgetTracker>>,
        policy: PolicyEngine,
        depth: usize,
        events: EventRecorder,
    ) -> Self {
        Self {
            config,
            tools,
            budget,
            policy,
            depth,
            events,
            cache: None,
            model_id: None,
            background_jobs: None,
            workspace_remap: None,
            approval_classifier: None,
            decisions: None,
        }
    }

    /// Sets the registry model id of the adapter the loop runs on; passed to
    /// tools so delegation can fall back to the parent adapter.
    #[must_use]
    pub fn with_model_id(mut self, model_id: Option<String>) -> Self {
        self.model_id = model_id;
        self
    }

    /// Enables a turn-scoped read-only result cache; write successes
    /// invalidate overlapping path-scoped entries.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<Mutex<ToolResultCache>>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Registers created terminals as background jobs and clears a job when
    /// the model waits on it in the foreground.
    #[must_use]
    pub fn with_background_jobs(
        mut self,
        jobs: Option<crate::background_jobs::SharedBackgroundJobs>,
    ) -> Self {
        self.background_jobs = jobs;
        self
    }

    /// Remaps primary-checkout paths into an isolated worktree before policy
    /// evaluation and execution.
    #[must_use]
    pub fn with_workspace_remap(
        mut self,
        remap: Option<Arc<crate::workspace_isolation::WorkspacePathRemap>>,
    ) -> Self {
        self.workspace_remap = remap;
        self
    }

    /// Enables the opt-in risk classifier before host approval dispatch.
    #[must_use]
    pub fn with_approval_classifier(
        mut self,
        classifier: Option<Arc<crate::approval_classifier::ApprovalClassifier>>,
    ) -> Self {
        self.approval_classifier = classifier;
        self
    }

    /// Shares the runtime decision log for classifier audit entries.
    #[must_use]
    pub fn with_decisions(mut self, decisions: Option<Arc<Mutex<DecisionLog>>>) -> Self {
        self.decisions = decisions;
        self
    }

    /// Executes one tool intent, emitting the pending → in-progress →
    /// completed/failed update lifecycle.
    pub async fn execute(
        &self,
        intent: &ToolIntent,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &TaskNode,
        transcript: &[ModelMessage],
    ) -> Result<ToolResult, OrchestratorError> {
        // Isolated children see primary-checkout paths in prompts and scope
        // rules but must operate on their worktree; remap before validation,
        // policy, cache keys, and execution so every layer agrees.
        let mut remapped;
        let intent = match &self.workspace_remap {
            Some(remap) => {
                remapped = intent.clone();
                remap.remap_arguments(&mut remapped.arguments);
                &remapped
            }
            None => intent,
        };
        let tool = self.tools.lock().expect("tool registry poisoned").get(&intent.name);
        let Some(tool) = tool else {
            let result = ToolResult::failure(
                ToolErrorKind::Backend,
                format!("unknown tool: {}", intent.name),
            );
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        };
        let definition = tool.definition();

        if let Some(reason) = &intent.argument_error {
            // The model's own malformed JSON is a replayable tool error: return
            // the parse failure so it can correct the call on the next step.
            let result = ToolResult::failure(ToolErrorKind::InvalidArguments, reason.clone());
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        }

        if let Err(reason) = definition.validate_arguments(&intent.arguments) {
            let result = ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        }

        let decision = self.policy.check_with_arguments(
            &definition,
            PolicyContext { subagent_depth: self.depth, active_delegates: 0 },
            &intent.arguments,
        );
        if !decision.allow {
            let reason = decision.reason.unwrap_or_else(|| "tool denied by policy".to_string());
            let result = ToolResult::failure(ToolErrorKind::PermissionDenied, reason);
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        }
        if definition.host_approval {
            if let Some(classifier) = &self.approval_classifier {
                let candidate = crate::approval_classifier::ApprovalCandidate::new(
                    &definition.name,
                    definition.side_effect_class,
                    definition.side_effect_subclass,
                    &intent.arguments,
                );
                let outcome = classifier.classify(&candidate).await;
                match &self.decisions {
                    Some(decisions) => classifier.record(
                        &candidate,
                        &outcome,
                        &mut decisions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                        &self.events,
                    ),
                    None => {
                        let mut decisions = DecisionLog::default();
                        classifier.record(&candidate, &outcome, &mut decisions, &self.events);
                    }
                }
                match &outcome {
                    crate::approval_classifier::ApprovalClassification::Deny { reason } => {
                        let result =
                            ToolResult::failure(ToolErrorKind::PermissionDenied, reason.clone());
                        emit_failed_tool(sink, intent, &result)?;
                        return Ok(result);
                    }
                    // A non-low classification keeps the interactive host prompt,
                    // so the approval waterfall stage must still observe it.
                    crate::approval_classifier::ApprovalClassification::NeedsApproval {
                        ..
                    } => {
                        self.events.record(OrchestratorEvent::ApprovalRequested {
                            tool_call_id: intent.tool_call_id.clone(),
                            tool_name: definition.name.clone(),
                        });
                    }
                    crate::approval_classifier::ApprovalClassification::AutoApprove { .. } => {}
                }
            } else {
                self.events.record(OrchestratorEvent::ApprovalRequested {
                    tool_call_id: intent.tool_call_id.clone(),
                    tool_name: definition.name.clone(),
                });
            }
        }
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_tool_call()?;
            budget.emit(&self.events);
        }

        // Read-only result cache: on a hit the tool is not executed, but the
        // update lifecycle is still streamed from the cached result and the
        // tool call still counts against the per-turn budget.
        let session_id = sink.session_id().to_string();
        // Plugin tool gates run after mandatory policy, budget reservation, and
        // the write-transaction/ownership checks above. A plugin may deny an
        // otherwise allowed call; it can never allow a mandatory denial.
        if let Some(gate) = self.policy.plugin_gate() {
            let decision = gate.check_tool(
                &definition,
                PolicyContext { subagent_depth: self.depth, active_delegates: 0 },
                &intent.arguments,
            );
            if !decision.allow {
                let reason =
                    decision.reason.unwrap_or_else(|| "tool denied by plugin gate".to_string());
                let result = ToolResult::failure(ToolErrorKind::PermissionDenied, reason);
                emit_failed_tool(sink, intent, &result)?;
                return Ok(result);
            }
        }
        if let Some(cache) = &self.cache {
            let key = cache_key(
                &definition.name,
                &intent.arguments,
                &session_id,
                path_argument(&intent.arguments),
            );
            if let Some(cached) = cache.lock().expect("tool cache poisoned").get(&key) {
                sink.tool_call_pending(
                    &intent.tool_call_id,
                    &definition.name,
                    tool_kind(&definition),
                )
                .map_err(map_update_error)?;
                sink.tool_call_in_progress(&intent.tool_call_id, &definition.name, Vec::new())
                    .map_err(map_update_error)?;
                emit_tool_outcome(sink, intent, &cached)?;
                return Ok(cached);
            }
        }

        sink.tool_call_pending(&intent.tool_call_id, &definition.name, tool_kind(&definition))
            .map_err(map_update_error)?;
        sink.tool_call_in_progress(&intent.tool_call_id, &definition.name, Vec::new())
            .map_err(map_update_error)?;

        let arguments = intent.arguments.clone();
        let terminal_wait = TerminalWaitLimits {
            default: self.config.tool_timeouts.terminal_default,
            max: self.config.tool_timeouts.terminal_max,
        };
        let context = ToolCallContext {
            task: task.clone(),
            transcript: transcript.to_vec(),
            events: self.events.clone(),
            scope: self.policy.policy().scope.clone(),
            model_id: self.model_id.clone(),
            terminal_wait,
        };
        let requested_ms = intent.arguments.get("timeout_ms").and_then(serde_json::Value::as_u64);
        let tool_timeout = self.config.tool_timeouts.effective_timeout(
            &definition.name,
            definition.side_effect_class,
            requested_ms,
        );
        let result = tokio::time::timeout(
            tool_timeout,
            tool.execute(arguments, client.clone(), cancel.clone(), context),
        )
        .await
        .unwrap_or_else(|_| {
            ToolResult::failure(
                ToolErrorKind::Timeout,
                format!("tool {} timed out after {tool_timeout:?}", definition.name),
            )
        });

        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        self.track_background_job(&definition, intent, &result);
        if let Some(cache) = &self.cache {
            if result.success && definition.side_effect_class == SideEffectClass::Read {
                let key = cache_key(
                    &definition.name,
                    &intent.arguments,
                    &session_id,
                    path_argument(&intent.arguments),
                );
                cache.lock().expect("tool cache poisoned").insert(key, result.clone());
            }
            if result.success && definition.side_effect_class == SideEffectClass::Write {
                let mut cache = cache.lock().expect("tool cache poisoned");
                for path in affected_paths(&definition, &intent.arguments) {
                    cache.invalidate_path(&session_id, &path);
                }
            }
        }
        emit_tool_outcome(sink, intent, &result)?;
        Ok(result)
    }

    /// Records or clears background terminal jobs from terminal lifecycle
    /// outcomes. Only successful calls change registry state.
    fn track_background_job(
        &self,
        definition: &ToolDefinition,
        intent: &ToolIntent,
        result: &ToolResult,
    ) {
        let Some(jobs) = &self.background_jobs else {
            return;
        };
        if !result.success {
            return;
        }
        match definition.name.as_str() {
            "create_terminal" => {
                let background = intent
                    .arguments
                    .get("background")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                if !background {
                    return;
                }
                let Some(terminal_id) = result
                    .structured_output
                    .as_ref()
                    .and_then(|value| value.get("terminal_id"))
                    .and_then(serde_json::Value::as_str)
                else {
                    return;
                };
                let command = intent
                    .arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("terminal command");
                jobs.register(terminal_id.to_string(), command.to_string());
            }
            "wait_for_terminal_exit" => {
                if let Some(terminal_id) =
                    intent.arguments.get("terminal_id").and_then(serde_json::Value::as_str)
                {
                    jobs.note_foreground_wait(terminal_id);
                }
            }
            _ => {}
        }
    }
}

/// Maps an SDK-side-effect class onto the ACP `ToolKind` update vocabulary.
fn tool_kind(definition: &ToolDefinition) -> ToolKind {
    match definition.side_effect_class {
        SideEffectClass::Read => ToolKind::Read,
        SideEffectClass::Write => ToolKind::Edit,
        SideEffectClass::Execute => ToolKind::Execute,
        SideEffectClass::Delegate => ToolKind::Other,
    }
}

/// Emits the completed/failed tool-call update for a finished tool.
fn emit_tool_outcome(
    sink: &UpdateSink,
    intent: &ToolIntent,
    result: &ToolResult,
) -> Result<(), OrchestratorError> {
    if result.success {
        let content = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            result.summary_text(),
        )))];
        sink.tool_call_completed(&intent.tool_call_id, &intent.name, content)
            .map_err(map_update_error)
    } else {
        emit_failed_tool(sink, intent, result)
    }
}

/// Emits a failed tool-call update carrying the error summary as content.
fn emit_failed_tool(
    sink: &UpdateSink,
    intent: &ToolIntent,
    result: &ToolResult,
) -> Result<(), OrchestratorError> {
    sink.tool_call_failed(&intent.tool_call_id, &intent.name, result.summary_text())
        .map_err(map_update_error)
}

fn map_update_error(error: UpdateSinkError) -> OrchestratorError {
    OrchestratorError::InvalidState(format!("update emission failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ee_agent_acp_server::server::OutboundEvent;
    use ee_agent_acp_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::{
        RawJsonRpcMessage, RequestId, Response, SessionId, SessionUpdate, ToolCallStatus,
    };
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::budget::BudgetTracker;
    use crate::config::OrchestratorConfig;
    use crate::policy::PolicyEngine;
    use crate::tasks::TaskId;
    use crate::test_support::FakeTool;

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    fn harness(
        config: OrchestratorConfig,
    ) -> (Arc<Mutex<ToolRegistry>>, Arc<Mutex<BudgetTracker>>, ToolExecutor) {
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
        let executor = ToolExecutor::new(
            config,
            tools.clone(),
            budget.clone(),
            PolicyEngine::default(),
            0,
            EventRecorder::new(),
        );
        (tools, budget, executor)
    }

    fn task_fixture() -> TaskNode {
        TaskNode::new(TaskId::new("task-1"), "t", "d")
    }

    /// Drains the next `session/update` notification from the outbound
    /// channel, panicking on any other event.
    async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }

    async fn next_client_request(
        rx: &mut mpsc::UnboundedReceiver<OutboundEvent>,
    ) -> RawJsonRpcMessage {
        match rx.recv().await.expect("client request queued") {
            OutboundEvent::ClientRequest { frame } => frame,
            other => panic!("expected client request, got {other:?}"),
        }
    }

    /// A tool that returns after a fixed delay, for timeout tests.
    struct SlowTool {
        definition: ToolDefinition,
        delay: Duration,
    }

    impl ServerTool for SlowTool {
        fn definition(&self) -> ToolDefinition {
            self.definition.clone()
        }

        fn execute(
            &self,
            _arguments: Value,
            _client: ClientBridge,
            _cancel: watch::Receiver<bool>,
            _context: ToolCallContext,
        ) -> ToolFuture<ToolResult> {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                ToolResult::success("slow done")
            })
        }
    }

    /// A tool that blocks until cancellation, for cancellation tests.
    struct CancelObservingTool {
        definition: ToolDefinition,
    }

    impl ServerTool for CancelObservingTool {
        fn definition(&self) -> ToolDefinition {
            self.definition.clone()
        }

        fn execute(
            &self,
            _arguments: Value,
            _client: ClientBridge,
            mut cancel: watch::Receiver<bool>,
            _context: ToolCallContext,
        ) -> ToolFuture<ToolResult> {
            Box::pin(async move {
                let _ = cancel.changed().await;
                ToolResult::failure(ToolErrorKind::Cancelled, "stopped by caller")
            })
        }
    }

    #[test]
    fn register_and_lookup_roundtrip() {
        let mut registry = ToolRegistry::new();
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("echo", "echoes")
                .input_schema(serde_json::json!({ "type": "object" })),
            ToolResult::success("echo"),
        ));
        registry.register(tool.clone()).expect("registers");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn duplicate_tool_name_is_rejected() {
        let mut registry = ToolRegistry::new();
        let tool =
            Arc::new(FakeTool::new(ToolDefinition::new("echo", "one"), ToolResult::success("a")));
        registry.register(tool.clone()).expect("first registers");
        let second =
            Arc::new(FakeTool::new(ToolDefinition::new("echo", "two"), ToolResult::success("b")));
        let error = registry.register(second).expect_err("duplicate rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref r) if r.contains("duplicate tool name: echo"))
        );
    }

    #[test]
    fn empty_tool_name_is_rejected() {
        let mut registry = ToolRegistry::new();
        let tool =
            Arc::new(FakeTool::new(ToolDefinition::new("", "no name"), ToolResult::success("x")));
        let error = registry.register(tool).expect_err("empty name rejected");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn definitions_are_sorted_by_name() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(FakeTool::new(
                ToolDefinition::new("zeta", "z"),
                ToolResult::success("z"),
            )))
            .expect("registers");
        registry
            .register(Arc::new(FakeTool::new(
                ToolDefinition::new("alpha", "a"),
                ToolResult::success("a"),
            )))
            .expect("registers");
        let names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn tool_result_shapes() {
        let ok = ToolResult::success("done");
        assert!(ok.success);
        assert_eq!(ok.summary_text(), "done");

        let failed = ToolResult::failure(ToolErrorKind::Timeout, "slow");
        assert!(!failed.success);
        assert_eq!(failed.error_kind, Some(ToolErrorKind::Timeout));
        assert_eq!(failed.summary_text(), "timeout: slow");
    }

    #[test]
    fn builtin_tools_register_under_session() {
        let mut registry = ToolRegistry::new();
        registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
        let names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "ask_user",
                "cargo_check",
                "create_terminal",
                "kill_terminal",
                "load_skill",
                "read_file",
                "release_terminal",
                "terminal_output",
                "wait_for_terminal_exit",
                "write_file"
            ]
        );
        assert_eq!(registry.len(), 10);
    }

    #[tokio::test]
    async fn load_skill_reads_a_scoped_manifest_and_rejects_traversal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let skill_dir = temp.path().join(".agents/skills/atlas");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: atlas\ndescription: Graph queries\n---\n\nUse atlas.\n",
        )
        .expect("write skill");
        std::fs::write(temp.path().join("outside.md"), "no").expect("outside file");
        let mut registry = ToolRegistry::new();
        registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
        let tool = registry.get("load_skill").expect("load_skill registered");
        let (sink, client, _rx) = plumbing();
        drop(sink);
        let context = ToolCallContext {
            task: task_fixture(),
            transcript: Vec::new(),
            events: EventRecorder::new(),
            scope: Some(WorkspaceScope::new(vec![temp.path().to_path_buf()], Vec::new())),
            model_id: None,
            terminal_wait: TerminalWaitLimits::default(),
        };

        let loaded = tool
            .execute(
                json!({ "name": "atlas" }),
                client.clone(),
                watch::channel(false).1,
                context.clone(),
            )
            .await;
        assert!(loaded.success, "{}", loaded.text_output);
        assert!(loaded.text_output.contains("Use atlas."));
        assert_eq!(loaded.structured_output.expect("structured")["scope"], "workspace");

        let traversal = tool
            .execute(json!({ "path": "../outside.md" }), client, watch::channel(false).1, context)
            .await;
        assert!(!traversal.success);
        assert_eq!(traversal.error_kind, Some(ToolErrorKind::InvalidArguments));
    }

    #[test]
    fn register_builtins_conflicts_with_custom_tool() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(FakeTool::new(
                ToolDefinition::new("read_file", "custom"),
                ToolResult::success("custom"),
            )))
            .expect("custom read_file registers");
        let error =
            registry.register_builtins(&SessionId::new("s-1")).expect_err("conflict rejected");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn validate_arguments_rejects_non_object_and_missing_required() {
        let definition = ToolDefinition::new("read_file", "reads")
            .input_schema(json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }));
        assert!(definition.validate_arguments(&json!([1, 2])).is_err());
        assert!(definition.validate_arguments(&json!({})).is_err());
        assert_eq!(
            definition.validate_arguments(&json!({})).unwrap_err(),
            "missing required argument: path"
        );
        assert!(definition.validate_arguments(&json!({ "path": "/a" })).is_ok());
        assert_eq!(
            definition.validate_arguments(&json!({ "path": 42 })).unwrap_err(),
            "argument path must be string"
        );
    }

    #[tokio::test]
    async fn tool_executor_builtin_read_file_uses_client_bridge() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "read_file", json!({ "path": "/a" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let task_bridge = bridge.clone();
        let task = tokio::spawn(async move {
            executor.execute(&intent, &sink, &task_bridge, cancel_rx, &task_fixture(), &[]).await
        });

        // Lifecycle: pending → in-progress → client request.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
            panic!("expected request frame");
        };
        assert_eq!(request.method.as_ref(), "fs/read_text_file");
        assert_eq!(request.params.clone().expect("params").into_value()["path"], "/a");

        bridge.handle_response(Response::Result {
            id: RequestId::Number(1),
            result: json!({ "content": "file contents" }),
        });

        let result = task.await.expect("task joins").expect("executes");
        assert!(result.success);
        assert_eq!(result.text_output, "file contents");
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(rx.try_recv().is_err(), "no further outbound events");
    }

    #[tokio::test]
    async fn tool_executor_builtin_ask_user_uses_elicitation() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "ask_user", json!({ "message": "pick one" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let task_bridge = bridge.clone();
        let task = tokio::spawn(async move {
            executor.execute(&intent, &sink, &task_bridge, cancel_rx, &task_fixture(), &[]).await
        });

        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
            panic!("expected request frame");
        };
        assert_eq!(request.method.as_ref(), "elicitation/create");

        bridge.handle_response(Response::Result {
            id: RequestId::Number(1),
            result: json!({ "action": "cancel" }),
        });

        let result = task.await.expect("task joins").expect("executes");
        assert!(result.success);
        assert_eq!(result.text_output, "elicitation created");
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_wait_timeout_kills_captures_output_and_releases() {
        let (_sink, bridge, mut rx) = plumbing();
        let task_bridge = bridge.clone();
        let task = tokio::spawn(async move {
            ClientBridgeTool::run(
                BridgeCall::WaitForTerminalExit,
                SessionId::new("s-1"),
                json!({ "terminal_id": "term-1", "timeout_ms": 1 }),
                &task_bridge,
                TerminalWaitLimits::default(),
            )
            .await
        });

        let RawJsonRpcMessage::Request(wait) = next_client_request(&mut rx).await else {
            panic!("expected terminal wait request");
        };
        assert_eq!(wait.method.as_ref(), "terminal/wait_for_exit");

        tokio::time::advance(Duration::from_millis(1)).await;
        let RawJsonRpcMessage::Request(kill) = next_client_request(&mut rx).await else {
            panic!("expected terminal kill request");
        };
        assert_eq!(kill.method.as_ref(), "terminal/kill");
        bridge.handle_response(Response::Result { id: kill.id, result: json!({}) });

        let RawJsonRpcMessage::Request(output) = next_client_request(&mut rx).await else {
            panic!("expected terminal output request");
        };
        assert_eq!(output.method.as_ref(), "terminal/output");
        bridge.handle_response(Response::Result {
            id: output.id,
            result: json!({
                "output": "partial output",
                "truncated": false,
                "exitStatus": { "exitCode": null, "signal": "9" },
            }),
        });

        let RawJsonRpcMessage::Request(release) = next_client_request(&mut rx).await else {
            panic!("expected terminal release request");
        };
        assert_eq!(release.method.as_ref(), "terminal/release");
        bridge.handle_response(Response::Result { id: release.id, result: json!({}) });

        let result = task.await.expect("task joins").expect("timeout workflow succeeds");
        assert!(result.success);
        assert_eq!(
            result.structured_output.as_ref().expect("structured output")["timed_out"],
            true
        );
        assert_eq!(
            result.structured_output.as_ref().expect("structured output")["output"],
            "partial output"
        );
        assert_eq!(result.structured_output.as_ref().expect("structured output")["released"], true);
    }

    #[tokio::test]
    async fn tool_executor_rejects_missing_arguments_without_execution() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "read_file", json!({}));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
        assert!(result.text_output.contains("missing required argument: path"));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        assert!(rx.try_recv().is_err(), "no client request may be sent for invalid arguments");
    }

    #[tokio::test]
    async fn tool_executor_rejects_non_object_arguments_without_execution() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "read_file", serde_json::Value::Null);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
        assert_eq!(result.text_output, "tool arguments must be a JSON object");
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        assert!(rx.try_recv().is_err(), "no client request may be sent for invalid arguments");
    }

    #[tokio::test]
    async fn tool_executor_denies_write_tool_by_default_policy() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "write_file", json!({ "path": "/a", "content": "x" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        let denied_reason_carried = failed
            .fields
            .content
            .as_ref()
            .expect("content")
            .iter()
            .any(|block| {
                matches!(block, ToolCallContent::Content(content) if matches!(&content.content, ContentBlock::Text(text) if text.text.contains("explicit policy allowance")))
            });
        assert!(denied_reason_carried);
        assert!(rx.try_recv().is_err(), "denied write tool must not reach the bridge");
    }

    #[tokio::test]
    async fn tool_executor_denies_execute_tool_by_default_policy() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "create_terminal", json!({ "command": "ls" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        assert!(rx.try_recv().is_err(), "denied execute tool must not reach the bridge");
    }

    #[tokio::test]
    async fn tool_executor_runs_custom_tool_with_structured_output() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("stats", "counts things"),
            ToolResult::success_structured("3 items", json!({ "count": 3 })),
        ));
        tools.lock().expect("registry").register(tool.clone()).expect("registers");

        let intent = ToolIntent::new("tc-1", "stats", json!({}));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(tool.call_count(), 1);
        assert!(result.success);
        assert_eq!(result.structured_output, Some(json!({ "count": 3 })));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(rx.try_recv().is_err());
    }

    #[derive(Debug)]
    struct FixedRisk(crate::approval_classifier::RiskLevel);

    impl crate::approval_classifier::ApprovalRiskClassifier for FixedRisk {
        fn model_id(&self) -> &str {
            "test-fast"
        }

        fn classify(
            &self,
            _candidate: &crate::approval_classifier::ApprovalCandidate,
        ) -> crate::model::ModelFuture<
            Result<
                crate::approval_classifier::ClassifierVerdict,
                crate::approval_classifier::ApprovalClassifierError,
            >,
        > {
            let risk = self.0;
            Box::pin(async move {
                Ok(crate::approval_classifier::ClassifierVerdict {
                    risk,
                    reason: String::from("test verdict"),
                })
            })
        }
    }

    fn classifier_executor(
        risk: crate::approval_classifier::RiskLevel,
    ) -> (
        Arc<Mutex<ToolRegistry>>,
        Arc<Mutex<crate::decision_log::DecisionLog>>,
        EventRecorder,
        ToolExecutor,
    ) {
        let config = OrchestratorConfig::default();
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
        let decisions = Arc::new(Mutex::new(crate::decision_log::DecisionLog::default()));
        let events = EventRecorder::new();
        let executor = ToolExecutor::new(
            config,
            tools.clone(),
            budget,
            PolicyEngine::default(),
            0,
            events.clone(),
        )
        .with_approval_classifier(Some(Arc::new(
            crate::approval_classifier::ApprovalClassifier::new(
                Arc::new(FixedRisk(risk)),
                Duration::from_secs(1),
            ),
        )))
        .with_decisions(Some(decisions.clone()));
        (tools, decisions, events, executor)
    }

    fn host_approval_write_tool(tools: &Arc<Mutex<ToolRegistry>>) -> Arc<FakeTool> {
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("write_file", "writes")
                .side_effect_class(SideEffectClass::Write)
                .host_approval(),
            ToolResult::success("wrote"),
        ));
        tools.lock().expect("registry").register(tool.clone()).expect("registers");
        tool
    }

    fn approval_codes(decisions: &Arc<Mutex<crate::decision_log::DecisionLog>>) -> Vec<String> {
        decisions
            .lock()
            .expect("decisions")
            .entries()
            .iter()
            .map(|entry| entry.reason_code.clone())
            .collect()
    }

    #[tokio::test]
    async fn approval_classifier_hard_block_denies_without_executing() {
        let (sink, bridge, _rx) = plumbing();
        let (tools, decisions, _events, executor) =
            classifier_executor(crate::approval_classifier::RiskLevel::Low);
        let tool = host_approval_write_tool(&tools);

        let intent = ToolIntent::new("tc-1", "write_file", json!({ "path": ".env" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        assert_eq!(tool.call_count(), 0, "hard-blocked calls never execute");
        assert_eq!(approval_codes(&decisions), vec![String::from("approval-denied")]);
    }

    #[tokio::test]
    async fn approval_classifier_low_risk_executes_and_audits() {
        let (sink, bridge, _rx) = plumbing();
        let (tools, decisions, events, executor) =
            classifier_executor(crate::approval_classifier::RiskLevel::Low);
        let tool = host_approval_write_tool(&tools);

        let intent =
            ToolIntent::new("tc-1", "write_file", json!({ "path": "src/lib.rs", "content": "x" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert!(result.success);
        assert_eq!(tool.call_count(), 1);
        assert_eq!(approval_codes(&decisions), vec![String::from("approval-auto-approved")]);
        assert!(
            !events
                .events()
                .iter()
                .any(|event| matches!(event, OrchestratorEvent::ApprovalRequested { .. })),
            "auto-approved calls skip the approval stage"
        );
    }

    #[tokio::test]
    async fn approval_classifier_medium_risk_keeps_the_host_approval_stage() {
        let (sink, bridge, _rx) = plumbing();
        let (tools, decisions, events, executor) =
            classifier_executor(crate::approval_classifier::RiskLevel::Medium);
        let tool = host_approval_write_tool(&tools);

        let intent =
            ToolIntent::new("tc-1", "write_file", json!({ "path": "src/lib.rs", "content": "x" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert!(result.success);
        assert_eq!(tool.call_count(), 1);
        assert_eq!(approval_codes(&decisions), vec![String::from("approval-needs-host")]);
        assert!(
            events
                .events()
                .iter()
                .any(|event| matches!(event, OrchestratorEvent::ApprovalRequested { .. })),
            "non-low risk must still report the approval stage"
        );
    }

    #[tokio::test]
    async fn tool_executor_unknown_tool_returns_backend_failure() {
        let (sink, bridge, mut rx) = plumbing();
        let (_tools, _budget, executor) = harness(OrchestratorConfig::default());

        let intent = ToolIntent::new("tc-1", "missing", json!({}));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::Backend));
        assert!(result.text_output.contains("unknown tool: missing"));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    }

    #[tokio::test]
    async fn background_terminal_jobs_register_and_clear() {
        let jobs = Arc::new(crate::background_jobs::BackgroundJobRegistry::new());
        let (_tools, _budget, executor) = harness(OrchestratorConfig::default());
        let executor = executor.with_background_jobs(Some(jobs.clone()));

        let create = ToolDefinition::new("create_terminal", "creates");
        let create_intent =
            ToolIntent::new("tc-1", "create_terminal", json!({ "command": "cargo build" }));
        executor.track_background_job(
            &create,
            &create_intent,
            &ToolResult::success_structured("ok", json!({ "terminal_id": "term-1" })),
        );
        assert!(jobs.snapshot_text().expect("job tracked").contains("cargo build"));

        let wait = ToolDefinition::new("wait_for_terminal_exit", "waits");
        let wait_intent =
            ToolIntent::new("tc-2", "wait_for_terminal_exit", json!({ "terminal_id": "term-1" }));
        executor.track_background_job(&wait, &wait_intent, &ToolResult::success("exited"));
        assert!(jobs.is_empty(), "foreground wait clears the background job");
    }

    #[tokio::test]
    async fn tool_executor_timeout_emits_failed_update() {
        let (sink, bridge, mut rx) = plumbing();
        let config = OrchestratorConfig {
            tool_timeouts: crate::config::ToolTimeoutPolicy {
                fast_local_read: Duration::from_millis(50),
                search_and_mcp: Duration::from_millis(50),
                write_and_edit: Duration::from_millis(50),
                terminal_default: Duration::from_millis(50),
                terminal_max: Duration::from_millis(50),
            },
            ..OrchestratorConfig::default()
        };
        let (tools, _budget, executor) = harness(config);
        tools
            .lock()
            .expect("registry")
            .register(Arc::new(SlowTool {
                definition: ToolDefinition::new("slow", "sleeps"),
                delay: Duration::from_millis(200),
            }))
            .expect("registers");

        let intent = ToolIntent::new("tc-1", "slow", json!({}));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::Timeout));
        assert!(result.text_output.contains("timed out"));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        let timeout_reason_carried = failed
            .fields
            .content
            .as_ref()
            .expect("content")
            .iter()
            .any(|block| {
                matches!(block, ToolCallContent::Content(content) if matches!(&content.content, ContentBlock::Text(text) if text.text.contains("timed out")))
            });
        assert!(timeout_reason_carried);
    }

    #[tokio::test]
    async fn tool_executor_cancelled_result_shapes_failed_update() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register(Arc::new(FakeTool::new(
                ToolDefinition::new("cancel_me", "cancels"),
                ToolResult::failure(ToolErrorKind::Cancelled, "backend refused"),
            )))
            .expect("registers");

        let intent = ToolIntent::new("tc-1", "cancel_me", json!({}));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::Cancelled));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    }

    #[tokio::test]
    async fn tool_executor_cancel_during_run_stops_loop() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register(Arc::new(CancelObservingTool {
                definition: ToolDefinition::new("blocking", "blocks until cancelled"),
            }))
            .expect("registers");

        let intent = ToolIntent::new("tc-1", "blocking", json!({}));
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            executor.execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[]).await
        });

        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        cancel_tx.send(true).expect("cancels");

        let error = task.await.expect("task joins").expect_err("cancellation stops the loop");
        assert_eq!(error, OrchestratorError::Cancellation);
        assert!(rx.try_recv().is_err(), "no outcome update after cancellation");
    }

    #[tokio::test]
    async fn tool_executor_relative_read_path_fails_without_request() {
        let (sink, bridge, mut rx) = plumbing();
        let (tools, _budget, executor) = harness(OrchestratorConfig::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "read_file", json!({ "path": "rel/file" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(rx.try_recv().is_err(), "relative path must not reach the transport");
    }
}
