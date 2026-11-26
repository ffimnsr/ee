//! Model-facing session management tools: rename, move, and model listing.
//!
//! `session_rename` records a title that the loop emits as an ACP
//! `session_info_update` right after the call succeeds. `session_move` changes
//! the session's working directory used for workspace instruction discovery;
//! it only accepts destinations that are already inside an allowed workspace
//! root (moving outside the configured roots would need an approval surface
//! this build does not have). `models` is a read-only view of the registry's
//! public metadata — never credentials or provider clients.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::config::OrchestratorConfig;
use crate::model_registry::ModelRegistry;
use crate::tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolErrorKind, ToolFuture,
    ToolResult,
};

/// Maximum models returned by the `models` tool.
const MAX_MODELS: usize = 100;
/// Maximum characters accepted for a session title.
const MAX_TITLE_CHARS: usize = 200;

/// Shared runtime handles the session tools mutate.
pub(crate) struct SessionToolContext {
    pub(crate) models: Arc<ModelRegistry>,
    pub(crate) config: OrchestratorConfig,
    pub(crate) workspace_scope: crate::workspace_scope::WorkspaceScopeSlot,
    pub(crate) session_move_target: Arc<Mutex<Option<PathBuf>>>,
    pub(crate) pending_title: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionToolKind {
    Rename,
    Move,
    Models,
}

struct SessionTool {
    kind: SessionToolKind,
    context: Arc<SessionToolContext>,
}

impl SessionTool {
    fn definition_for(kind: SessionToolKind) -> ToolDefinition {
        let definition = match kind {
            SessionToolKind::Rename => ToolDefinition::new(
                "session_rename",
                "Rename the current session with a short, specific title that summarizes the work. The title is shown to the user immediately.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"],
                "additionalProperties": false,
            })),
            SessionToolKind::Move => ToolDefinition::new(
                "session_move",
                "Move the session's working directory to another directory inside an allowed workspace root. The move applies to following prompts and workspace instruction discovery. External destinations fail closed.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": { "directory": { "type": "string" } },
                "required": ["directory"],
                "additionalProperties": false,
            })),
            SessionToolKind::Models => ToolDefinition::new(
                "models",
                "List the models registered for this session with their public provider, model id, capabilities, roles, tier, enabled state, and the configured context/token limits. Never returns credentials.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            })),
        };
        // Session-state tools are read-class: they do not touch workspace files.
        definition.side_effect_class(SideEffectClass::Read)
    }
}

impl ServerTool for SessionTool {
    fn definition(&self) -> ToolDefinition {
        Self::definition_for(self.kind)
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _client: ee_agent_acp_server::ClientBridge,
        _cancel: tokio::sync::watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let kind = self.kind;
        let ctx = self.context.clone();
        Box::pin(async move {
            match kind {
                SessionToolKind::Rename => {
                    let title = match required_str(&arguments, "title") {
                        Ok(title) => title.trim().to_string(),
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    if title.chars().count() > MAX_TITLE_CHARS {
                        return ToolResult::failure(
                            ToolErrorKind::InvalidArguments,
                            format!("session title must be at most {MAX_TITLE_CHARS} characters"),
                        );
                    }
                    *ctx.pending_title.lock().expect("pending session title poisoned") =
                        Some(title.clone());
                    ToolResult::success_structured(
                        format!("session renamed: {title}"),
                        json!({ "title": title }),
                    )
                }
                SessionToolKind::Move => match move_session(&ctx, &context, &arguments) {
                    Ok((directory, roots)) => ToolResult::success_structured(
                        format!("session moved to {}", directory.display()),
                        json!({ "directory": directory, "workspace_roots": roots }),
                    ),
                    Err((kind, reason)) => ToolResult::failure(kind, reason),
                },
                SessionToolKind::Models => {
                    let models: Vec<serde_json::Value> = ctx
                        .models
                        .advertised()
                        .into_iter()
                        .take(MAX_MODELS)
                        .map(|info| {
                            let capabilities: Vec<String> = info
                                .identity
                                .capabilities
                                .iter()
                                .filter_map(|capability| {
                                    serde_json::to_value(capability)
                                        .ok()
                                        .and_then(|value| value.as_str().map(str::to_string))
                                })
                                .collect();
                            let tier = serde_json::to_value(info.tier)
                                .ok()
                                .and_then(|value| value.as_str().map(str::to_string))
                                .unwrap_or_default();
                            json!({
                                "id": info.id,
                                "provider": info.identity.provider_id,
                                "model": info.identity.model_id,
                                "display_name": info.identity.display_name,
                                "capabilities": capabilities,
                                "roles": info.roles,
                                "tier": tier,
                                "enabled": info.enabled,
                            })
                        })
                        .collect();
                    let count = models.len();
                    ToolResult::success_structured(
                        format!("{count} model(s)"),
                        json!({
                            "models": models,
                            "context_window_tokens": ctx.config.context_window_tokens,
                            "max_input_tokens": ctx.config.max_input_tokens,
                            "max_output_tokens": ctx.config.max_output_tokens,
                        }),
                    )
                }
            }
        })
    }
}

/// Resolves and applies one `session_move` call.
///
/// The destination must be an existing absolute directory inside an allowed
/// workspace root (when a scope is configured); the resolved canonical path
/// becomes the session working directory so later prompts and instruction
/// discovery follow it.
fn move_session(
    context: &SessionToolContext,
    call: &ToolCallContext,
    arguments: &serde_json::Value,
) -> Result<(PathBuf, Vec<PathBuf>), (ToolErrorKind, String)> {
    let requested = required_str(arguments, "directory")
        .map_err(|reason| (ToolErrorKind::InvalidArguments, reason))?;
    let requested = PathBuf::from(requested);
    if !requested.is_absolute() {
        return Err((
            ToolErrorKind::InvalidArguments,
            "session_move requires an absolute directory".into(),
        ));
    }
    let directory = std::fs::canonicalize(&requested).map_err(|error| {
        (
            ToolErrorKind::InvalidArguments,
            format!("session_move destination is not accessible: {}: {error}", requested.display()),
        )
    })?;
    if !directory.is_dir() {
        return Err((
            ToolErrorKind::InvalidArguments,
            format!("session_move destination is not a directory: {}", directory.display()),
        ));
    }
    if let Some(scope) = call.scope.as_ref()
        && !scope.allowed_roots.iter().any(|root| {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            directory.starts_with(root)
        })
    {
        return Err((
            ToolErrorKind::PermissionDenied,
            "session_move destination must stay inside an allowed workspace root".into(),
        ));
    }

    let roots = {
        let mut scope = context.workspace_scope.lock().expect("workspace scope poisoned");
        let roots = match scope.as_ref() {
            Some((_, roots)) if !roots.is_empty() => roots.clone(),
            _ => call.scope.as_ref().map_or_else(Vec::new, |scope| scope.allowed_roots.clone()),
        };
        *scope = Some((directory.clone(), roots.clone()));
        roots
    };
    *context.session_move_target.lock().expect("session move target poisoned") =
        Some(directory.clone());
    Ok((directory, roots))
}

fn required_str<'a>(arguments: &'a serde_json::Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} must be a non-empty string"))
}

/// All session management tools bound to the runtime's shared state.
pub(crate) fn session_tools(context: Arc<SessionToolContext>) -> Vec<Arc<dyn ServerTool>> {
    [SessionToolKind::Rename, SessionToolKind::Move, SessionToolKind::Models]
        .into_iter()
        .map(|kind| Arc::new(SessionTool { kind, context: context.clone() }) as Arc<dyn ServerTool>)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ee_agent_acp_server::ClientBridge;

    use super::*;
    use crate::model::ModelResponse;
    use crate::model_registry::{ModelCapability, ModelFamily, ModelIdentity, ModelRegistration};
    use crate::test_support::FakeModel;
    use crate::tools::TerminalWaitLimits;
    use crate::workspace_scope::WorkspaceScope;

    fn context(
        root: &std::path::Path,
    ) -> (Arc<SessionToolContext>, ToolCallContext, tempfile::TempDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = ModelIdentity::new(
            "test-model",
            "test-provider",
            ModelFamily::Other("test".into()),
            "Test Model",
            [ModelCapability::ChatCompletion],
        )
        .expect("identity");
        let mut registry = ModelRegistry::new();
        registry
            .register_model(
                "test-model",
                Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()])),
                ModelRegistration::new(identity),
            )
            .expect("registers model");
        let tool_context = Arc::new(SessionToolContext {
            models: Arc::new(registry),
            config: OrchestratorConfig::default(),
            workspace_scope: Arc::new(Mutex::new(None)),
            session_move_target: Arc::new(Mutex::new(None)),
            pending_title: Arc::new(Mutex::new(None)),
        });
        let call_context = ToolCallContext {
            task: crate::tasks::TaskNode::new(crate::tasks::TaskId::new("t"), "t", "t"),
            transcript: Vec::new(),
            events: crate::events::EventRecorder::new(),
            scope: Some(WorkspaceScope::new(
                vec![root.to_path_buf(), temp.path().to_path_buf()],
                Vec::new(),
            )),
            model_id: None,
            terminal_wait: TerminalWaitLimits::default(),
            auto_approved: false,
        };
        (tool_context, call_context, temp)
    }

    fn client() -> ClientBridge {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ClientBridge::new_for_test(Duration::from_secs(5), tx)
    }

    fn tool(context: &Arc<SessionToolContext>, name: &str) -> Arc<dyn ServerTool> {
        session_tools(context.clone())
            .into_iter()
            .find(|tool| tool.definition().name == name)
            .expect("session tool registered")
    }

    #[tokio::test]
    async fn rename_records_a_pending_title() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (context, call_context, _keep) = context(temp.path());
        let result = tool(&context, "session_rename")
            .execute(
                json!({ "title": "Audit run" }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context,
            )
            .await;
        assert!(result.success, "{}", result.text_output);
        assert_eq!(context.pending_title.lock().expect("title").as_deref(), Some("Audit run"));
    }

    #[tokio::test]
    async fn rename_rejects_blank_and_oversized_titles() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (context, call_context, _keep) = context(temp.path());
        let rename = tool(&context, "session_rename");
        let blank = rename
            .execute(
                json!({ "title": "   " }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context.clone(),
            )
            .await;
        assert!(!blank.success);
        assert_eq!(blank.error_kind, Some(ToolErrorKind::InvalidArguments));

        let oversized = rename
            .execute(
                json!({ "title": "x".repeat(MAX_TITLE_CHARS + 1) }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context,
            )
            .await;
        assert!(!oversized.success);
        assert_eq!(oversized.error_kind, Some(ToolErrorKind::InvalidArguments));
    }

    #[tokio::test]
    async fn move_requires_an_allowed_directory() {
        let repository = tempfile::tempdir().expect("repo");
        let (context, call_context, temp) = context(repository.path());
        let move_tool = tool(&context, "session_move");
        let allowed = move_tool
            .execute(
                json!({ "directory": temp.path() }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context.clone(),
            )
            .await;
        assert!(allowed.success, "{}", allowed.text_output);
        let moved = std::fs::canonicalize(temp.path()).expect("canonical");
        assert_eq!(context.session_move_target.lock().expect("target").as_ref(), Some(&moved));

        let outside = move_tool
            .execute(
                json!({ "directory": "/" }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context,
            )
            .await;
        assert!(!outside.success);
        assert_eq!(outside.error_kind, Some(ToolErrorKind::PermissionDenied));
    }

    #[tokio::test]
    async fn move_rejects_relative_and_missing_directories() {
        let repository = tempfile::tempdir().expect("repo");
        let (context, call_context, temp) = context(repository.path());
        let move_tool = tool(&context, "session_move");
        let relative = move_tool
            .execute(
                json!({ "directory": "relative/dir" }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context.clone(),
            )
            .await;
        assert!(!relative.success);
        assert_eq!(relative.error_kind, Some(ToolErrorKind::InvalidArguments));

        let missing = move_tool
            .execute(
                json!({ "directory": temp.path().join("does-not-exist") }),
                client(),
                tokio::sync::watch::channel(false).1,
                call_context,
            )
            .await;
        assert!(!missing.success);
        assert_eq!(missing.error_kind, Some(ToolErrorKind::InvalidArguments));
    }

    #[tokio::test]
    async fn models_lists_public_metadata_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (context, call_context, _keep) = context(temp.path());
        let result = tool(&context, "models")
            .execute(json!({}), client(), tokio::sync::watch::channel(false).1, call_context)
            .await;
        assert!(result.success, "{}", result.text_output);
        let value = result.structured_output.expect("structured");
        assert_eq!(value["models"][0]["provider"], "test-provider");
        assert_eq!(value["models"][0]["capabilities"][0], "chat_completion");
        assert!(!value.to_string().contains("api_key"));
    }
}
