//! Model-facing git worktree tools.
//!
//! `worktree_create/list/remove` expose the existing
//! [`crate::workspace_isolation`] machinery. Worktrees are detached from the
//! primary checkout's `HEAD` and live under the configured session worktree
//! root. The tools are write-class; `worktree_remove` is destructive
//! (`delete` subclass) and is additionally confined to the configured root.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::destructive_policy::SideEffectSubclass;
use crate::tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolErrorKind, ToolFuture,
    ToolResult,
};
use crate::workspace_isolation::{
    WorkspaceIsolationConfig, WorktreeWorkspace, lexical_normalize, list_worktrees,
    remove_worktree, sanitize_worktree_name,
};

/// Which worktree operation a tool implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeToolKind {
    Create,
    List,
    Remove,
}

/// One model-facing worktree tool.
pub(crate) struct WorktreeTool {
    kind: WorktreeToolKind,
    worktrees: Arc<Mutex<Vec<WorktreeWorkspace>>>,
    config: WorkspaceIsolationConfig,
}

impl WorktreeTool {
    fn new(
        kind: WorktreeToolKind,
        worktrees: Arc<Mutex<Vec<WorktreeWorkspace>>>,
        config: WorkspaceIsolationConfig,
    ) -> Self {
        Self { kind, worktrees, config }
    }

    fn definition_for(kind: WorktreeToolKind) -> ToolDefinition {
        match kind {
            WorktreeToolKind::Create => ToolDefinition::new(
                "worktree_create",
                "Create a detached git worktree from the primary checkout's HEAD under the session worktree root. Use it for isolated experiments that must not touch the primary checkout.",
            )
            .input_schema(serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false,
            }))
            .side_effect_class(SideEffectClass::Write),
            WorktreeToolKind::List => ToolDefinition::new(
                "worktree_list",
                "List git worktrees for the active workspace, including the primary checkout. Results are bounded and contain paths and branch/detached state only.",
            )
            .input_schema(serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }))
            .side_effect_class(SideEffectClass::Read),
            WorktreeToolKind::Remove => ToolDefinition::new(
                "worktree_remove",
                "Remove one dedicated worktree created by worktree_create. The path must stay inside the session worktree root. This is destructive and requires approval.",
            )
            .input_schema(serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false,
            }))
            .side_effect_class(SideEffectClass::Write)
            .side_effect_subclass(SideEffectSubclass::Delete),
        }
    }

    fn primary_root(context: &ToolCallContext) -> Result<PathBuf, String> {
        context
            .scope
            .as_ref()
            .and_then(|scope| scope.allowed_roots.first().cloned())
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| String::from("workspace root is unavailable"))
    }
}

impl ServerTool for WorktreeTool {
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
        let worktrees = self.worktrees.clone();
        let config = self.config.clone();
        Box::pin(async move {
            if !config.enabled {
                return ToolResult::failure(
                    ToolErrorKind::PermissionDenied,
                    "worktree isolation is disabled for this session",
                );
            }
            match kind {
                WorktreeToolKind::List => {
                    let primary = match Self::primary_root(&context) {
                        Ok(primary) => primary,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    match list_worktrees(&primary) {
                        Ok(entries) => {
                            let count = entries.len();
                            let value = serde_json::to_value(&entries)
                                .unwrap_or_else(|_| serde_json::json!({ "worktrees": [] }));
                            ToolResult::success_structured(format!("{count} worktree(s)"), value)
                        }
                        Err(error) => {
                            ToolResult::failure(ToolErrorKind::Backend, error.to_string())
                        }
                    }
                }
                WorktreeToolKind::Create => {
                    let name = match required_name(&arguments) {
                        Ok(name) => name,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    let primary = match Self::primary_root(&context) {
                        Ok(primary) => primary,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    let root = config.root_for(&primary);
                    match WorktreeWorkspace::create(&primary, &root, name) {
                        Ok(worktree) => {
                            let path = worktree.path().to_path_buf();
                            let name = worktree.name().to_string();
                            worktrees.lock().expect("worktrees poisoned").push(worktree);
                            ToolResult::success_structured(
                                format!("worktree created: {}", path.display()),
                                serde_json::json!({
                                    "name": name,
                                    "path": path,
                                    "primary_root": primary,
                                }),
                            )
                        }
                        Err(error) => {
                            ToolResult::failure(ToolErrorKind::Backend, error.to_string())
                        }
                    }
                }
                WorktreeToolKind::Remove => {
                    let name = match required_name(&arguments) {
                        Ok(name) => name,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    let sanitized = sanitize_worktree_name(name);
                    if sanitized.is_empty() {
                        return ToolResult::failure(
                            ToolErrorKind::InvalidArguments,
                            "worktree name must contain at least one safe character",
                        );
                    }
                    let primary = match Self::primary_root(&context) {
                        Ok(primary) => primary,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    let root = lexical_normalize(&config.root_for(&primary));
                    let path = lexical_normalize(&root.join(&sanitized));
                    if !path.starts_with(&root) {
                        return ToolResult::failure(
                            ToolErrorKind::PermissionDenied,
                            "worktree path escapes the configured worktree root",
                        );
                    }
                    worktrees
                        .lock()
                        .expect("worktrees poisoned")
                        .retain(|worktree| worktree.name() != sanitized);
                    match remove_worktree(&primary, &path) {
                        Ok(()) => ToolResult::success_structured(
                            format!("worktree removed: {}", path.display()),
                            serde_json::json!({ "name": sanitized, "path": path }),
                        ),
                        Err(error) => {
                            ToolResult::failure(ToolErrorKind::Backend, error.to_string())
                        }
                    }
                }
            }
        })
    }
}

fn required_name(arguments: &serde_json::Value) -> Result<&str, String> {
    arguments
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| String::from("name must be a non-empty string"))
}

/// All worktree tools, bound to the runtime's retained-worktree store.
pub(crate) fn worktree_tools(
    worktrees: Arc<Mutex<Vec<WorktreeWorkspace>>>,
    config: WorkspaceIsolationConfig,
) -> Vec<Arc<dyn ServerTool>> {
    [WorktreeToolKind::Create, WorktreeToolKind::List, WorktreeToolKind::Remove]
        .into_iter()
        .map(|kind| {
            Arc::new(WorktreeTool::new(kind, worktrees.clone(), config.clone()))
                as Arc<dyn ServerTool>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ee_agent_acp_server::ClientBridge;
    use serde_json::json;

    use super::*;
    use crate::tools::{TerminalWaitLimits, ToolCallContext};
    use crate::workspace_isolation::git_command;
    use crate::workspace_scope::WorkspaceScope;

    fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        git_command(dir, args).output().expect("git runs")
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(git(temp.path(), &["init", "--quiet"]).status.success());
        assert!(git(temp.path(), &["config", "user.name", "ee"]).status.success());
        assert!(git(temp.path(), &["config", "user.email", "ee@example.com"]).status.success());
        std::fs::write(temp.path().join("README.md"), "hello").expect("write");
        assert!(git(temp.path(), &["add", "."]).status.success());
        assert!(git(temp.path(), &["commit", "--quiet", "-m", "init"]).status.success());
        temp
    }

    fn context(root: &std::path::Path) -> ToolCallContext {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let _client = ClientBridge::new_for_test(Duration::from_secs(5), tx);
        ToolCallContext {
            task: crate::tasks::TaskNode::new(crate::tasks::TaskId::new("t"), "t", "t"),
            transcript: Vec::new(),
            events: crate::events::EventRecorder::new(),
            scope: Some(WorkspaceScope::new(vec![root.to_path_buf()], Vec::new())),
            model_id: None,
            terminal_wait: TerminalWaitLimits::default(),
            auto_approved: false,
        }
    }

    fn client() -> ClientBridge {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ClientBridge::new_for_test(Duration::from_secs(5), tx)
    }

    #[tokio::test]
    async fn create_list_and_remove_worktree() {
        let temp = repo();
        let worktrees = Arc::new(Mutex::new(Vec::new()));
        let config = WorkspaceIsolationConfig {
            enabled: true,
            worktree_root: Some(temp.path().join("wt")),
            keep_worktrees: true,
        };
        let tools = worktree_tools(worktrees.clone(), config);
        let create =
            tools.iter().find(|tool| tool.definition().name == "worktree_create").expect("create");
        let list =
            tools.iter().find(|tool| tool.definition().name == "worktree_list").expect("list");
        let remove =
            tools.iter().find(|tool| tool.definition().name == "worktree_remove").expect("remove");
        let context = context(temp.path());
        let client = client();

        let created = create
            .execute(
                json!({ "name": "feature" }),
                client.clone(),
                tokio::sync::watch::channel(false).1,
                context.clone(),
            )
            .await;
        assert!(created.success, "{}", created.text_output);
        let path = temp.path().join("wt").join("feature");
        assert!(path.is_dir(), "worktree created at {}", path.display());

        let listed = list
            .execute(
                json!({}),
                client.clone(),
                tokio::sync::watch::channel(false).1,
                context.clone(),
            )
            .await;
        assert!(listed.success, "{}", listed.text_output);
        assert!(listed.text_output.contains("worktree"), "{}", listed.text_output);

        let removed = remove
            .execute(
                json!({ "name": "feature" }),
                client,
                tokio::sync::watch::channel(false).1,
                context,
            )
            .await;
        assert!(removed.success, "{}", removed.text_output);
        assert!(!path.exists(), "worktree removed");
        assert!(worktrees.lock().expect("worktrees").is_empty());
    }

    #[tokio::test]
    async fn disabled_isolation_fails_closed() {
        let temp = repo();
        let tools =
            worktree_tools(Arc::new(Mutex::new(Vec::new())), WorkspaceIsolationConfig::default());
        let create =
            tools.iter().find(|tool| tool.definition().name == "worktree_create").expect("create");
        let result = create
            .execute(
                json!({ "name": "feature" }),
                client(),
                tokio::sync::watch::channel(false).1,
                context(temp.path()),
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
    }

    #[test]
    fn remove_is_destructive_and_create_is_write() {
        assert_eq!(
            WorktreeTool::definition_for(WorktreeToolKind::Create).side_effect_class,
            SideEffectClass::Write
        );
        assert_eq!(
            WorktreeTool::definition_for(WorktreeToolKind::Remove).side_effect_subclass,
            Some(SideEffectSubclass::Delete)
        );
        assert_eq!(
            WorktreeTool::definition_for(WorktreeToolKind::List).side_effect_class,
            SideEffectClass::Read
        );
    }
}
