//! Workspace scope policy.
//!
//! [`WorkspaceScope`] restricts file-tool paths to allowed absolute roots and
//! optional file globs.  When a scope is active, tool intents whose path
//! (or terminal `cwd`) resolves outside the scope are rejected before any
//! client-bridge call.  Subagent scopes are narrowed from parent scopes via
//! [`WorkspaceScope::narrow`]: children never widen roots or globs.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::tools::SideEffectClass;

/// Allowed workspace roots and file globs for tool paths.
///
/// Paths are absolute; a path is inside the scope when it starts with one of
/// the allowed roots (component-wise) and, when globs are configured, matches
/// at least one glob.  An empty scope (no roots) matches nothing — fail
/// closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceScope {
    /// Absolute allowed roots; a path must start with one of these.
    pub allowed_roots: Vec<PathBuf>,
    /// Optional file glob patterns (`**/*.rs`); empty means any file under
    /// the roots.
    pub allowed_globs: Vec<String>,
}

impl WorkspaceScope {
    /// Creates a scope with the given roots and globs.
    #[must_use]
    pub fn new(allowed_roots: Vec<PathBuf>, allowed_globs: Vec<String>) -> Self {
        Self { allowed_roots, allowed_globs }
    }

    /// Whether `path` lies inside the scope.
    ///
    /// Relative paths never match (they are evaluated elsewhere and rejected
    /// by the client bridge); a scope with no roots matches nothing.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        if !path.is_absolute() {
            return false;
        }
        if !self.allowed_roots.iter().any(|root| path.starts_with(root)) {
            return false;
        }
        if self.allowed_globs.is_empty() {
            return true;
        }
        match build_globset(&self.allowed_globs) {
            Some(set) => set.is_match(path.to_string_lossy().as_ref()),
            None => false, // unbuildable patterns fail closed
        }
    }

    /// Narrowed child scope: roots are inherited unchanged and never widened;
    /// globs become the intersection of the parent globs and `globs` (or the
    /// provided globs when the parent has none; or the parent globs when
    /// nothing is provided).
    #[must_use]
    pub fn narrow(&self, globs: &[String]) -> WorkspaceScope {
        let child_globs = if globs.is_empty() {
            self.allowed_globs.clone()
        } else if self.allowed_globs.is_empty() {
            globs.to_vec()
        } else {
            globs.iter().filter(|glob| self.allowed_globs.contains(glob)).cloned().collect()
        };
        WorkspaceScope { allowed_roots: self.allowed_roots.clone(), allowed_globs: child_globs }
    }

    /// Whether `other` is a narrowing of this scope (roots subset, globs
    /// subset).
    #[must_use]
    pub fn is_narrower_or_equal(&self, other: &WorkspaceScope) -> bool {
        other
            .allowed_roots
            .iter()
            .all(|root| self.allowed_roots.iter().any(|parent| parent == root))
            && other.allowed_globs.iter().all(|glob| self.allowed_globs.contains(glob))
    }

    /// Checks a tool's path-bearing arguments against the scope.  Read/write
    /// tools carry `path`; execute tools may carry `cwd`.  Relative paths and
    /// tools without path arguments pass through (relative paths are rejected
    /// later by the client bridge); absolute paths outside the scope fail
    /// closed with a reason.
    pub fn check_arguments(
        &self,
        class: SideEffectClass,
        arguments: &serde_json::Value,
    ) -> Result<(), String> {
        let candidates: Vec<&str> = match class {
            SideEffectClass::Read | SideEffectClass::Write => {
                arguments.get("path").and_then(serde_json::Value::as_str).into_iter().collect()
            }
            SideEffectClass::Execute => {
                arguments.get("cwd").and_then(serde_json::Value::as_str).into_iter().collect()
            }
            SideEffectClass::Delegate => Vec::new(),
        };
        for path in candidates {
            let path = Path::new(path);
            if path.is_absolute() && !self.contains(path) {
                return Err(format!(
                    "path {} is outside the active workspace scope",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

/// Builds a glob set; `None` when any pattern is invalid (callers fail
/// closed).
fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).ok()?;
        builder.add(glob);
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::tools::{ToolExecutor, ToolIntent, ToolRegistry};

    fn scope(roots: &[&str], globs: &[&str]) -> WorkspaceScope {
        WorkspaceScope::new(
            roots.iter().map(PathBuf::from).collect(),
            globs.iter().map(|g| (*g).to_string()).collect(),
        )
    }

    #[test]
    fn paths_inside_roots_are_allowed() {
        let s = scope(&["/work"], &[]);
        assert!(s.contains(Path::new("/work")));
        assert!(s.contains(Path::new("/work/a/b.rs")));
        assert!(!s.contains(Path::new("/etc/passwd")));
        assert!(!s.contains(Path::new("/workplace/x.rs")), "component-wise prefix only");
        assert!(!s.contains(Path::new("relative/path")), "relative paths never match");
    }

    #[test]
    fn multiple_roots_are_allowed() {
        let s = scope(&["/work", "/data"], &[]);
        assert!(s.contains(Path::new("/data/in.txt")));
        assert!(s.contains(Path::new("/work/in.txt")));
        assert!(!s.contains(Path::new("/tmp/x")));
    }

    #[test]
    fn empty_scope_matches_nothing() {
        let s = scope(&[], &[]);
        assert!(!s.contains(Path::new("/work/x.rs")));
    }

    #[test]
    fn globs_restrict_which_files_are_readable() {
        let s = scope(&["/work"], &["**/*.rs", "**/*.toml"]);
        assert!(s.contains(Path::new("/work/src/lib.rs")));
        assert!(s.contains(Path::new("/work/Cargo.toml")));
        assert!(!s.contains(Path::new("/work/README.md")));
        assert!(!s.contains(Path::new("/work/src/lib.txt")));
    }

    #[test]
    fn unbuildable_globs_fail_closed() {
        let s = scope(&["/work"], &["[invalid"]);
        assert!(!s.contains(Path::new("/work/a.rs")));
    }

    #[test]
    fn narrow_intersects_globs_and_never_widens_roots() {
        let parent = scope(&["/work"], &["**/*.rs", "**/*.toml"]);
        let child = parent.narrow(&["**/*.toml".to_string()]);
        assert_eq!(child.allowed_roots, parent.allowed_roots);
        assert_eq!(child.allowed_globs, vec!["**/*.toml"]);
        assert!(parent.is_narrower_or_equal(&child));

        let inherits = parent.narrow(&[]);
        assert_eq!(inherits, parent, "no provided globs means inheritance");

        let from_wide = scope(&["/work"], &[]).narrow(&["src/**".to_string()]);
        assert_eq!(from_wide.allowed_globs, vec!["src/**"], "root-wide parent narrows to globs");

        let disjoint = parent.narrow(&["**/*.go".to_string()]);
        assert!(disjoint.allowed_globs.is_empty(), "disjoint globs narrow to nothing");
    }

    #[test]
    fn check_arguments_rejects_root_escape_before_execution() {
        let s = scope(&["/work"], &[]);
        assert!(s.check_arguments(SideEffectClass::Read, &json!({ "path": "/work/a.rs" })).is_ok());
        assert!(
            s.check_arguments(SideEffectClass::Read, &json!({ "path": "/etc/passwd" })).is_err()
        );
        assert!(
            s.check_arguments(SideEffectClass::Read, &json!({ "path": "rel/a.rs" })).is_ok(),
            "relative paths pass the scope check (bridge rejects them)"
        );
        assert!(s.check_arguments(SideEffectClass::Write, &json!({ "path": "/etc/x" })).is_err());
        assert!(s.check_arguments(SideEffectClass::Execute, &json!({ "cwd": "/work" })).is_ok());
        assert!(s.check_arguments(SideEffectClass::Execute, &json!({ "cwd": "/etc" })).is_err());
        assert!(s.check_arguments(SideEffectClass::Delegate, &json!({ "prompt": "x" })).is_ok());
    }

    #[test]
    fn scope_roundtrips_through_json() {
        let s = scope(&["/work"], &["**/*.rs"]);
        let json = serde_json::to_string(&s).expect("serializes");
        let restored: WorkspaceScope = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, s);
    }

    // ── Executor enforcement ─────────────────────────────────────────────

    fn harness(
        policy: PolicyEngine,
    ) -> (std::sync::Arc<std::sync::Mutex<ToolRegistry>>, ToolExecutor) {
        use crate::budget::BudgetTracker;
        use crate::config::OrchestratorConfig;
        use crate::events::EventRecorder;
        use std::sync::{Arc, Mutex};
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let budget = Arc::new(Mutex::new(BudgetTracker::new(&OrchestratorConfig::default())));
        let executor = ToolExecutor::new(
            OrchestratorConfig::default(),
            tools.clone(),
            budget,
            policy,
            0,
            EventRecorder::new(),
        );
        (tools, executor)
    }

    fn plumbing() -> (
        ee_acp_agent_server::UpdateSink,
        ee_acp_agent_server::ClientBridge,
        tokio::sync::mpsc::UnboundedReceiver<ee_acp_agent_server::server::OutboundEvent>,
    ) {
        use ee_acp_agent_server::{ClientBridge, UpdateSink};
        use std::time::Duration;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(ee_agent_protocol::SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    #[tokio::test]
    async fn read_outside_scope_is_denied_before_bridge_call() {
        use crate::tools::ToolErrorKind;
        use ee_acp_agent_server::server::OutboundEvent;
        use ee_agent_protocol::{SessionId, SessionUpdate};
        use tokio::sync::watch;

        let (sink, bridge, mut rx) = plumbing();
        let policy = PolicyEngine::new(ToolPolicy::default().with_scope(scope(&["/work"], &[])));
        let (tools, executor) = harness(policy);
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "read_file", json!({ "path": "/etc/passwd" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let task = crate::tasks::TaskNode::new(crate::tasks::TaskId::new("task-1"), "t", "d");
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task, &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        assert!(result.text_output.contains("outside the active workspace scope"));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ee_agent_protocol::ToolCallStatus::Failed));
        let after: Vec<OutboundEvent> = rx.try_recv().into_iter().collect();
        assert!(
            after.iter().all(|event| !matches!(event, OutboundEvent::ClientRequest { .. })),
            "scope-denied read must not reach the bridge"
        );
    }

    async fn next_update(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ee_acp_agent_server::server::OutboundEvent>,
    ) -> ee_agent_protocol::SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            ee_acp_agent_server::server::OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }
}
