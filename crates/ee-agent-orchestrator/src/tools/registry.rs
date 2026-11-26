//! The server-side tool trait, the tool registry, and built-in tools.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_acp_server::{ClientBridge, ProviderError};
use ee_agent_protocol::approval_meta::mark_classifier_auto_approved;
use ee_agent_protocol::{
    CreateElicitationRequest, CreateTerminalRequest, ElicitationAction, ElicitationFormMode,
    ElicitationMode, ElicitationSchema, ElicitationScope, ElicitationSessionScope,
    KillTerminalRequest, ReadTextFileRequest, ReleaseTerminalRequest, SessionId, TerminalId,
    TerminalOutputRequest, WaitForTerminalExitRequest, WriteTextFileRequest,
};
use tokio::sync::watch;

use crate::destructive_policy::SideEffectSubclass;
use crate::error::OrchestratorError;
use crate::todo_plan::{TodoItem, TodoPlan, TodoStatus};
use crate::tool_dependencies::{ToolDataClass, ToolDependency};

use super::definition::{
    SideEffectClass, TerminalWaitLimits, ToolCallContext, ToolDefinition, ToolErrorKind,
    ToolFuture, ToolResult,
};

/// Removes duplicate bridge file tools once the ee proxy advertises the
/// canonical text-file tools.
///
/// The bridge builtins and the ee proxy both reach the same editor
/// `fs/read_text_file` / `fs/write_text_file` path, so advertising both names
/// forces the model to choose arbitrarily. When `ee_read_text_file` or
/// `ee_write_text_file` is registered, the matching bridge builtin is hidden.
/// An explicit `visible` set still wins, so a critic that names `read_file`
/// keeps it.
pub(crate) fn canonical_tool_definitions(
    definitions: Vec<ToolDefinition>,
    visible: Option<&HashSet<String>>,
) -> Vec<ToolDefinition> {
    let has_ee_read = definitions.iter().any(|tool| tool.name == "ee_read_text_file");
    let has_ee_write = definitions.iter().any(|tool| tool.name == "ee_write_text_file");
    definitions
        .into_iter()
        .filter(|tool| {
            let explicitly_visible = visible.is_some_and(|allowed| allowed.contains(&tool.name));
            if !explicitly_visible {
                if has_ee_read && tool.name == "read_file" {
                    return false;
                }
                if has_ee_write && tool.name == "write_file" {
                    return false;
                }
            }
            visible.is_none_or(|allowed| allowed.contains(&tool.name))
        })
        .collect()
}

/// Keeps only read-class tool definitions.
///
/// Plan mode advertises read-class tools only so the model never sees a
/// mutation it cannot execute; execution policy still denies writes.
pub(crate) fn read_only_definitions(definitions: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    definitions
        .into_iter()
        .filter(|definition| definition.side_effect_class == SideEffectClass::Read)
        .collect()
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

    /// Registers the model-facing `todowrite` tool backed by the session plan.
    ///
    /// Kept separate from [`Self::register_builtins`] because the plan store is
    /// owned by the runtime, not the client bridge.
    pub fn register_todo_tool(
        &mut self,
        plan: Arc<Mutex<TodoPlan>>,
    ) -> Result<(), OrchestratorError> {
        self.register(Arc::new(TodowriteTool::new(plan)))
    }
}

/// Which framework `ClientBridge` method a built-in tool wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeCall {
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

    pub(crate) async fn run(
        call: BridgeCall,
        session_id: SessionId,
        arguments: serde_json::Value,
        client: &ClientBridge,
        terminal_wait: TerminalWaitLimits,
        auto_approved: bool,
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
                let mut request =
                    WriteTextFileRequest::new(session_id, PathBuf::from(path), content);
                if auto_approved {
                    request.meta = Some(mark_classifier_auto_approved(request.meta.take()));
                }
                let response = client.write_text_file(request).await;
                match response {
                    Ok(_) => ToolResult::success("file written"),
                    Err(error) => bridge_error(error),
                }
            }
            BridgeCall::CreateTerminal => {
                let command = string_arg(&arguments, "command")?;
                let mut request = CreateTerminalRequest::new(session_id, command);
                if auto_approved {
                    request.meta = Some(mark_classifier_auto_approved(request.meta.take()));
                }
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
            Self::run(
                call,
                session_id,
                arguments,
                &client,
                context.terminal_wait,
                context.auto_approved,
            )
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
            .classifier_eligible()
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
        let auto_approved = context.auto_approved;
        Box::pin(async move {
            if *cancel.borrow() {
                return ToolResult::failure(ToolErrorKind::Cancelled, "validation cancelled");
            }
            let mut request = CreateTerminalRequest::new(session_id.clone(), "cargo check --quiet");
            if auto_approved {
                request.meta = Some(mark_classifier_auto_approved(request.meta.take()));
            }
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

/// Read-only memory entry loader and search.
///
/// The prompt advertises a bounded catalog of `.agents/memories` entries; the
/// model loads one by name or searches by text and tags only when it needs the
/// content. Entry text is repository- or user-authored data, never policy.
#[derive(Clone)]
struct LoadMemoryTool;

impl ServerTool for LoadMemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "load_memory",
            "Loads or searches memory entries listed in the prompt catalog",
        )
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "query": { "type": "string" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                },
                "limit": { "type": "integer" },
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
            let query = optional_string_arg(&arguments, "query");
            let tags = match string_array_arg(&arguments, "tags") {
                Ok(tags) => tags,
                Err(result) => return result,
            };
            let limit = arguments
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(crate::user_memories::DEFAULT_MEMORY_RECALL_LIMIT);
            let roots = context
                .scope
                .as_ref()
                .map(|scope| scope.allowed_roots.clone())
                .filter(|roots| !roots.is_empty())
                .or_else(|| std::env::current_dir().ok().map(|cwd| vec![cwd]))
                .unwrap_or_default();
            let cwd = roots.first().cloned().unwrap_or_default();
            let config_root = crate::prompt::skills::user_config_root_from_env();
            let registry = crate::user_memories::UserMemoryRegistry::discover(
                &cwd,
                &roots,
                config_root.as_deref(),
            );
            if let Some(name) = name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
                let Some(entry) = registry.get(name) else {
                    return ToolResult::failure(
                        ToolErrorKind::InvalidArguments,
                        format!("unknown memory entry {name:?}"),
                    );
                };
                let (content, truncated) = crate::user_memories::render_entries(
                    &[entry],
                    crate::user_memories::MAX_MEMORY_TOOL_BYTES,
                );
                return ToolResult::success_structured(
                    content,
                    serde_json::json!({
                        "name": entry.name,
                        "description": entry.description,
                        "tags": entry.tags,
                        "importance": entry.importance.as_str(),
                        "scope": entry.scope.as_str(),
                        "path": entry.path.display().to_string(),
                        "bytes": entry.content.len(),
                        "truncated": truncated,
                    }),
                );
            }
            let hits = registry.search(query.as_deref(), &tags, limit);
            if hits.is_empty() {
                return ToolResult::success("No memory entries matched.");
            }
            let (content, truncated) = crate::user_memories::render_entries(
                &hits,
                crate::user_memories::MAX_MEMORY_TOOL_BYTES,
            );
            let entries: Vec<serde_json::Value> = hits
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "name": entry.name,
                        "description": entry.description,
                        "tags": entry.tags,
                        "importance": entry.importance.as_str(),
                        "scope": entry.scope.as_str(),
                        "path": entry.path.display().to_string(),
                    })
                })
                .collect();
            ToolResult::success_structured(
                content,
                serde_json::json!({
                    "count": entries.len(),
                    "truncated": truncated,
                    "entries": entries,
                }),
            )
        })
    }
}

/// Loads one harness experience entry by domain and sequence.
///
/// Entry text is repository- or user-authored data, never policy.
#[derive(Clone)]
struct LoadExperienceTool;

impl ServerTool for LoadExperienceTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "load_experience",
            "Loads one harness experience entry from the prompt catalog",
        )
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "domain": { "type": "string" },
                "seq": { "type": "integer" },
            },
            "required": ["domain"],
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
            let Some(domain) = optional_string_arg(&arguments, "domain")
                .map(|domain| domain.trim().to_string())
                .filter(|domain| !domain.is_empty())
            else {
                return ToolResult::failure(
                    ToolErrorKind::InvalidArguments,
                    "load_experience requires a domain argument",
                );
            };
            let seq = arguments.get("seq").and_then(serde_json::Value::as_u64);
            let roots = context
                .scope
                .as_ref()
                .map(|scope| scope.allowed_roots.clone())
                .filter(|roots| !roots.is_empty())
                .or_else(|| std::env::current_dir().ok().map(|cwd| vec![cwd]))
                .unwrap_or_default();
            let cwd = roots.first().cloned().unwrap_or_default();
            let config_root = crate::prompt::skills::user_config_root_from_env();
            match crate::user_experience::load_experience(
                &domain,
                seq,
                &cwd,
                &roots,
                config_root.as_deref(),
            ) {
                Ok(loaded) => {
                    let entry = &loaded.entry;
                    ToolResult::success_structured(
                        loaded.text,
                        serde_json::json!({
                            "domain": entry.domain,
                            "seq": entry.seq,
                            "task": entry.task,
                            "confidence": entry.confidence.as_str(),
                            "scope": entry.scope.as_str(),
                            "path": entry.path.display().to_string(),
                            "no_entry": entry.is_no_entry(),
                            "edits": entry.edits,
                            "truncated": loaded.truncated,
                        }),
                    )
                }
                Err(message) => ToolResult::failure(ToolErrorKind::InvalidArguments, message),
            }
        })
    }
}

/// Creates or replaces one workspace memory entry after user confirmation.
#[derive(Clone)]
struct SaveMemoryTool {
    session_id: SessionId,
}

impl ServerTool for SaveMemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "save_memory",
            "Creates or updates one workspace memory entry under .agents/memories after user confirmation",
        )
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "content": { "type": "string" },
                "description": { "type": "string" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                },
                "importance": {
                    "type": "string",
                    "enum": ["low", "normal", "high", "critical"],
                },
            },
            "required": ["name", "content"],
        }))
        .side_effect_class(SideEffectClass::Write)
        .side_effect_subclass(SideEffectSubclass::Overwrite)
        .host_approval()
        .dependency(ToolDependency::new().produces(vec![ToolDataClass::FileText]))
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let session_id = self.session_id.clone();
        Box::pin(async move {
            let name = match string_arg(&arguments, "name") {
                Ok(name) => name,
                Err(result) => return result,
            };
            let content = match string_arg(&arguments, "content") {
                Ok(content) => content,
                Err(result) => return result,
            };
            let description = optional_string_arg(&arguments, "description");
            let tags_present = arguments.get("tags").is_some();
            let tags = match string_array_arg(&arguments, "tags") {
                Ok(tags) => tags,
                Err(result) => return result,
            };
            let importance = match optional_string_arg(&arguments, "importance") {
                Some(value) => match value.parse::<crate::user_memories::MemoryImportance>() {
                    Ok(importance) => Some(importance),
                    Err(reason) => return invalid_args(reason),
                },
                None => None,
            };
            let roots = memory_workspace_roots(&context);
            let cwd = roots.first().cloned().unwrap_or_default();
            let request = crate::user_memory_write::MemorySaveRequest {
                name: &name,
                description: description.as_deref(),
                tags: if tags_present { Some(tags.as_slice()) } else { None },
                importance,
                content: &content,
            };
            if let Err(result) = confirm_memory_mutation(
                &client,
                &session_id,
                format!("Save memory entry `{name}` under .agents/memories?"),
            )
            .await
            {
                return result;
            }
            if *cancel.borrow() {
                return ToolResult::failure(ToolErrorKind::Cancelled, "memory save cancelled");
            }
            match crate::user_memory_write::save(&cwd, &roots, request) {
                Ok(outcome) => ToolResult::success_structured(
                    if outcome.created {
                        format!("created memory entry {name}")
                    } else {
                        format!("updated memory entry {name}")
                    },
                    serde_json::json!({
                        "name": outcome.entry.name,
                        "description": outcome.entry.description,
                        "tags": outcome.entry.tags,
                        "importance": outcome.entry.importance.as_str(),
                        "scope": outcome.entry.scope.as_str(),
                        "path": outcome.entry.path.display().to_string(),
                        "created": outcome.created,
                    }),
                ),
                Err(reason) => ToolResult::failure(ToolErrorKind::InvalidArguments, reason),
            }
        })
    }
}

/// Deletes one workspace memory entry after user confirmation.
#[derive(Clone)]
struct DeleteMemoryTool {
    session_id: SessionId,
}

impl ServerTool for DeleteMemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "delete_memory",
            "Deletes one workspace memory entry under .agents/memories after user confirmation",
        )
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
            },
            "required": ["name"],
        }))
        .side_effect_class(SideEffectClass::Write)
        .side_effect_subclass(SideEffectSubclass::Delete)
        .host_approval()
        .dependency(ToolDependency::new())
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let session_id = self.session_id.clone();
        Box::pin(async move {
            let name = match string_arg(&arguments, "name") {
                Ok(name) => name,
                Err(result) => return result,
            };
            let roots = memory_workspace_roots(&context);
            let cwd = roots.first().cloned().unwrap_or_default();
            if let Err(result) = confirm_memory_mutation(
                &client,
                &session_id,
                format!("Delete memory entry `{name}` under .agents/memories?"),
            )
            .await
            {
                return result;
            }
            if *cancel.borrow() {
                return ToolResult::failure(ToolErrorKind::Cancelled, "memory deletion cancelled");
            }
            match crate::user_memory_write::delete(&cwd, &roots, &name) {
                Ok(entry) => ToolResult::success_structured(
                    format!("deleted memory entry {name}"),
                    serde_json::json!({
                        "name": entry.name,
                        "deleted": true,
                        "path": entry.path.display().to_string(),
                    }),
                ),
                Err(reason) => ToolResult::failure(ToolErrorKind::InvalidArguments, reason),
            }
        })
    }
}

fn memory_workspace_roots(context: &ToolCallContext) -> Vec<std::path::PathBuf> {
    context
        .scope
        .as_ref()
        .map(|scope| scope.allowed_roots.clone())
        .filter(|roots| !roots.is_empty())
        .or_else(|| std::env::current_dir().ok().map(|cwd| vec![cwd]))
        .unwrap_or_default()
}

async fn confirm_memory_mutation(
    client: &ClientBridge,
    session_id: &SessionId,
    message: String,
) -> Result<(), ToolResult> {
    let scope = ElicitationScope::Session(ElicitationSessionScope::new(session_id.clone()));
    let mode = ElicitationMode::Form(ElicitationFormMode::new(scope, ElicitationSchema::new()));
    match client.create_elicitation(CreateElicitationRequest::new(mode, message)).await {
        Ok(response) => match response.action {
            ElicitationAction::Accept(_) => Ok(()),
            ElicitationAction::Decline => Err(ToolResult::failure(
                ToolErrorKind::PermissionDenied,
                "memory mutation declined",
            )),
            _ => Err(ToolResult::failure(ToolErrorKind::Cancelled, "memory mutation cancelled")),
        },
        Err(error) => Err(bridge_error(error)),
    }
}

/// Model-facing session todo list.
///
/// Mutates only ephemeral session state, never the workspace, so it stays
/// read-class and needs no host approval. The loop emits an ACP plan update
/// from the stored plan after every successful call.
#[derive(Clone)]
struct TodowriteTool {
    plan: Arc<Mutex<TodoPlan>>,
}

impl TodowriteTool {
    fn new(plan: Arc<Mutex<TodoPlan>>) -> Self {
        Self { plan }
    }
}

impl ServerTool for TodowriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "todowrite",
            "Create and update the session todo list shown to the user. Send the full ordered list on every call; it replaces the previous list. Use it when a task has three or more distinct steps, and update statuses as work progresses.",
        )
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                            },
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["todos"],
            "additionalProperties": false,
        }))
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let plan = self.plan.clone();
        Box::pin(async move {
            let items = match parse_todo_items(&arguments) {
                Ok(items) => items,
                Err(reason) => return invalid_args(reason),
            };
            let mut plan = plan.lock().expect("todo plan poisoned");
            if let Err(error) = plan.replace(&items) {
                return invalid_args(error.to_string());
            }
            let summary = plan.summary();
            let todos = plan.items();
            ToolResult::success_structured(summary, serde_json::json!({ "todos": todos }))
        })
    }
}

fn parse_todo_items(arguments: &serde_json::Value) -> Result<Vec<TodoItem>, String> {
    let todos = arguments
        .get("todos")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "todowrite requires a todos array".to_string())?;
    let mut items = Vec::with_capacity(todos.len());
    for (index, value) in todos.iter().enumerate() {
        let content = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("todos[{index}].content must be a string"))?;
        let raw_status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("todos[{index}].status must be a string"))?;
        let status = match raw_status {
            "pending" => TodoStatus::Pending,
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            "cancelled" => TodoStatus::Cancelled,
            other => {
                return Err(format!(
                    "todos[{index}].status must be pending, in_progress, completed, or cancelled; got {other}"
                ));
            }
        };
        items.push(TodoItem::new(content, status));
    }
    Ok(items)
}

/// All built-in tools, bound to the given session.
pub(crate) fn builtin_tools(session_id: &SessionId) -> Vec<Arc<dyn ServerTool>> {
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
                // Native write/terminal bridge calls are eligible for the opt-in
                // risk classifier; policy semantics stay unchanged.
                classifier_eligible: matches!(
                    call,
                    BridgeCall::WriteTextFile | BridgeCall::CreateTerminal
                ),
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
        Arc::new(LoadMemoryTool) as Arc<dyn ServerTool>,
        Arc::new(LoadExperienceTool) as Arc<dyn ServerTool>,
        Arc::new(SaveMemoryTool { session_id: session_id.clone() }) as Arc<dyn ServerTool>,
        Arc::new(DeleteMemoryTool { session_id: session_id.clone() }) as Arc<dyn ServerTool>,
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

fn string_array_arg(arguments: &serde_json::Value, name: &str) -> Result<Vec<String>, ToolResult> {
    let Some(value) = arguments.get(name) else { return Ok(Vec::new()) };
    let Some(values) = value.as_array() else {
        return Err(invalid_args(format!("tool argument {name} must be an array of strings")));
    };
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                invalid_args(format!("tool argument {name} must be an array of strings"))
            })
        })
        .collect()
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
        ProviderError::UnsupportedCompaction(_) => ToolErrorKind::Backend,
    };
    ToolResult::failure(kind, error.to_string())
}
