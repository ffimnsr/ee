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
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{ClientBridge, ProviderError, UpdateSink, UpdateSinkError};
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
use crate::destructive_policy::SideEffectSubclass;
use crate::error::OrchestratorError;
use crate::events::EventRecorder;
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
    /// Capability flags the client must advertise for this tool.
    pub required_capabilities: Vec<String>,
    /// Dependency metadata for planned-batch ordering and cache invalidation.
    #[serde(default)]
    pub dependency: ToolDependency,
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
            required_capabilities: Vec::new(),
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

    /// Sets the required capability flags.
    #[must_use]
    pub fn required_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.required_capabilities = capabilities;
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
}

impl ToolIntent {
    /// Creates a tool intent.
    #[must_use]
    pub fn new(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self { tool_call_id: tool_call_id.into(), name: name.into(), arguments }
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
                let response = client
                    .wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                        session_id,
                        terminal_id,
                    ))
                    .await;
                match response {
                    Ok(response) => ToolResult::success_structured(
                        "terminal exited",
                        serde_json::to_value(response.exit_status)
                            .unwrap_or_else(|_| serde_json::json!(null)),
                    ),
                    Err(error) => bridge_error(error),
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
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let call = self.call;
        let session_id = self.session_id.clone();
        Box::pin(async move {
            Self::run(call, session_id, arguments, &client).await.unwrap_or_else(|result| result)
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
                required_capabilities: Vec::new(),
                dependency,
            },
        )) as Arc<dyn ServerTool>
    };
    let none = ToolDependency::new();
    vec![
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
            "Creates a terminal running a command",
            SideEffectClass::Execute,
            None,
            none.clone().produces(vec![ToolDataClass::TerminalHandle]),
            schema(&[("command", "string"), ("cwd", "string")], &["command"]),
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
            "Waits for a terminal command to exit",
            SideEffectClass::Execute,
            None,
            none.clone()
                .requires(vec![ToolDataClass::TerminalHandle])
                .produces(vec![ToolDataClass::TerminalExit]),
            schema(&[("terminal_id", "string")], &["terminal_id"]),
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
        Self { config, tools, budget, policy, depth, events, cache: None, model_id: None }
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
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_tool_call()?;
            budget.emit(&self.events);
        }

        // Read-only result cache: on a hit the tool is not executed, but the
        // update lifecycle is still streamed from the cached result and the
        // tool call still counts against the per-turn budget.
        let session_id = sink.session_id().to_string();
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
        let context = ToolCallContext {
            task: task.clone(),
            transcript: transcript.to_vec(),
            events: self.events.clone(),
            scope: self.policy.policy().scope.clone(),
            model_id: self.model_id.clone(),
        };
        let tool_timeout = self.config.tool_timeout;
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

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
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
                "create_terminal",
                "kill_terminal",
                "read_file",
                "release_terminal",
                "terminal_output",
                "wait_for_terminal_exit",
                "write_file"
            ]
        );
        assert_eq!(registry.len(), 8);
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
    async fn tool_executor_timeout_emits_failed_update() {
        let (sink, bridge, mut rx) = plumbing();
        let config = OrchestratorConfig {
            tool_timeout: Duration::from_millis(50),
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
