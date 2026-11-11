//! Read-only prompt views over runtime state.
//!
//! Kept small and separate so the prompt boundary never reaches into registry
//! internals.

use super::*;

use crate::policy::PolicyContext;
use crate::prompt::context::RuntimeSnapshot;

impl OrchestratorRuntime {
    /// Projects one full runtime snapshot into the plane the model sees.
    ///
    /// A delta is emitted only when this session still retains the previous
    /// full snapshot. First turn, resume, compaction, truncation, or a missing
    /// retained snapshot all emit the complete snapshot.
    pub(crate) fn runtime_plane(
        &self,
        session_id: &str,
        full: &RuntimeSnapshot,
    ) -> (RuntimeSnapshot, Option<String>) {
        let previous = self
            .runtime_snapshots
            .lock()
            .expect("runtime snapshots poisoned")
            .get(session_id)
            .cloned();
        match previous {
            Some(previous) => match full.delta_against(&previous) {
                Some(delta) => (delta, Some(previous.full_text)),
                None => (full.clone(), None),
            },
            None => (full.clone(), None),
        }
    }

    /// Commits the full snapshot only after the turn reached the loop.
    pub(crate) fn commit_runtime_plane(&self, session_id: &str, full: &RuntimeSnapshot) {
        if full.sections.is_empty() {
            return;
        }
        self.runtime_snapshots
            .lock()
            .expect("runtime snapshots poisoned")
            .insert(session_id.to_string(), full.full_state());
    }

    /// Drops retained runtime state so the next turn emits a full snapshot.
    pub(crate) fn clear_runtime_plane(&self, session_id: &str) {
        self.runtime_snapshots.lock().expect("runtime snapshots poisoned").remove(session_id);
    }

    /// Names of registered tools that the active policy admits for a root turn.
    ///
    /// Guidance sections use this set so prompt prose can never describe a tool
    /// that policy hides. MCP tools appear here only after they are registered
    /// for the prompt.
    #[must_use]
    pub(crate) fn advertised_tool_names(&self) -> Vec<String> {
        let policy = self.policy.read().expect("policy poisoned");
        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        definitions
            .into_iter()
            .filter(|definition| policy.check(definition, PolicyContext::default()).allow)
            .map(|definition| definition.name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::context::{RuntimeSection, RuntimeSnapshotMode};
    use crate::test_support::FakeModel;

    fn runtime() -> OrchestratorRuntime {
        OrchestratorRuntime::with_policy(
            OrchestratorConfig::default(),
            Arc::new(FakeModel::new(Vec::new())),
            PolicyEngine::default(),
        )
    }

    fn snapshot(cwd: &str) -> crate::prompt::context::RuntimeSnapshot {
        crate::prompt::context::RuntimeSnapshot {
            text: format!("{cwd}\n\nsandbox"),
            sections: vec![
                RuntimeSection::new("WORKING_DIRECTORY", 100, cwd),
                RuntimeSection::new("SANDBOX_POLICY", 110, "sandbox"),
            ],
            ..crate::prompt::context::RuntimeSnapshot::default()
        }
    }

    #[test]
    fn identical_next_turn_emits_delta_not_full_resend() {
        let runtime = runtime();
        let full = snapshot("old cwd");
        let (first, retained) = runtime.runtime_plane("s-1", &full);
        assert_eq!(first.mode, RuntimeSnapshotMode::Full);
        assert!(retained.is_none());
        runtime.commit_runtime_plane("s-1", &full);

        let (second, retained) = runtime.runtime_plane("s-1", &full);
        assert_eq!(second.mode, RuntimeSnapshotMode::Delta);
        assert!(second.sections.is_empty());
        assert_eq!(retained.as_deref(), Some(full.text.as_str()));
        assert!(!second.text.contains("old cwd"), "{}", second.text);
    }

    #[test]
    fn changed_cwd_emits_only_working_directory() {
        let runtime = runtime();
        runtime.commit_runtime_plane("s-1", &snapshot("old cwd"));
        let (delta, _) = runtime.runtime_plane("s-1", &snapshot("new cwd"));
        assert_eq!(delta.mode, RuntimeSnapshotMode::Delta);
        assert_eq!(delta.sections.len(), 1);
        assert_eq!(delta.sections[0].name, "WORKING_DIRECTORY");
        assert!(!delta.text.contains("sandbox"), "{}", delta.text);
    }

    #[test]
    fn cleared_or_missing_state_forces_full_snapshot() {
        let runtime = runtime();
        runtime.commit_runtime_plane("s-1", &snapshot("old cwd"));
        runtime.clear_runtime_plane("s-1");
        let (full_again, retained) = runtime.runtime_plane("s-1", &snapshot("old cwd"));
        assert_eq!(full_again.mode, RuntimeSnapshotMode::Full);
        assert!(retained.is_none());
        let (other, _) = runtime.runtime_plane("s-2", &snapshot("old cwd"));
        assert_eq!(other.mode, RuntimeSnapshotMode::Full);
    }
}
