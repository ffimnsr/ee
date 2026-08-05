//! Write-scope conflict detection for concurrent subagents.
//!
//! Concurrent subagents must not edit overlapping files.  The
//! [`WriteScopeConflictDetector`] tracks the absolute paths each active
//! subagent intends to write, rejects a new acquisition whose scope overlaps
//! any held scope, and releases the lock when the subagent's task completes
//! or is cancelled.  Paths are compared component-wise (a path overlaps an
//! ancestor, descendant, or equal path); relative paths are rejected because
//! they cannot be compared safely, so the detector fails closed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::subagents::SubagentId;

/// Deterministic write-scope lock table, keyed by subagent id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteScopeConflictDetector {
    held: BTreeMap<SubagentId, Vec<PathBuf>>,
}

impl WriteScopeConflictDetector {
    /// Creates an empty detector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires the intended write scope for a subagent.
    ///
    /// An empty scope (no intended writes) is a no-op and records nothing.
    /// Relative paths are rejected outright.  An overlap with any held scope
    /// fails closed with the first conflicting holder in stable id order.
    pub fn acquire(
        &mut self,
        subagent: &SubagentId,
        scope: Vec<PathBuf>,
    ) -> Result<(), OrchestratorError> {
        if scope.is_empty() {
            return Ok(());
        }
        if let Some(path) = scope.iter().find(|path| !path.is_absolute()) {
            return Err(OrchestratorError::InvalidState(format!(
                "write scope paths must be absolute; got {}",
                path.display()
            )));
        }
        if let Some((holder, held_path)) = self.find_conflict(&scope) {
            return Err(OrchestratorError::PolicyDenied(format!(
                "write scope overlap: {} conflicts with concurrent subagent {} at {}",
                subagent,
                holder,
                held_path.display()
            )));
        }
        self.held.insert(subagent.clone(), scope);
        Ok(())
    }

    /// Releases the write scope of a subagent (task completed or cancelled).
    /// Returns whether a scope was held.
    pub fn release(&mut self, subagent: &SubagentId) -> bool {
        self.held.remove(subagent).is_some()
    }

    /// The held scope of a subagent, when locked.
    #[must_use]
    pub fn held_scope(&self, subagent: &SubagentId) -> Option<&[PathBuf]> {
        self.held.get(subagent).map(Vec::as_slice)
    }

    /// The first subagent (in stable id order) whose held scope overlaps
    /// `scope`, if any.  Empty or relative-path scopes never conflict.
    #[must_use]
    pub fn conflicts_with(&self, scope: &[PathBuf]) -> Option<SubagentId> {
        self.find_conflict(scope).map(|(holder, _)| holder)
    }

    /// All subagents currently holding write scopes, in stable id order.
    #[must_use]
    pub fn holders(&self) -> Vec<SubagentId> {
        self.held.keys().cloned().collect()
    }

    /// Number of held write scopes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether no write scopes are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Releases every held scope.
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// First overlap between `scope` and any held scope, in stable holder id
    /// and path order.
    fn find_conflict(&self, scope: &[PathBuf]) -> Option<(SubagentId, PathBuf)> {
        for (holder, held) in &self.held {
            for held_path in held {
                if scope.iter().any(|path| overlaps(held_path, path)) {
                    return Some((holder.clone(), held_path.clone()));
                }
            }
        }
        None
    }
}

/// Component-wise overlap: equal paths, or one being an ancestor of the
/// other.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scope(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn disjoint_scopes_are_acquired_together() {
        let mut detector = WriteScopeConflictDetector::new();
        detector
            .acquire(&SubagentId::new("task-2"), scope(&["/work/a.rs"]))
            .expect("first acquires");
        detector
            .acquire(&SubagentId::new("task-3"), scope(&["/work/b.rs", "/work/c.rs"]))
            .expect("disjoint scope acquires");
        assert_eq!(detector.len(), 2);
        assert_eq!(
            detector.held_scope(&SubagentId::new("task-2")),
            Some(scope(&["/work/a.rs"]).as_slice())
        );
        assert_eq!(detector.holders(), vec![SubagentId::new("task-2"), SubagentId::new("task-3")]);
    }

    #[test]
    fn overlapping_scopes_are_rejected_with_the_holder() {
        let mut detector = WriteScopeConflictDetector::new();
        detector
            .acquire(&SubagentId::new("task-2"), scope(&["/work/src/a.rs"]))
            .expect("first acquires");
        let error = detector
            .acquire(&SubagentId::new("task-3"), scope(&["/work/src/a.rs"]))
            .expect_err("same file rejected");
        assert!(
            matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("task-2") && reason.contains("overlap"))
        );
        assert_eq!(
            detector.held_scope(&SubagentId::new("task-3")),
            None,
            "rejected subagent holds nothing"
        );
    }

    #[test]
    fn ancestor_and_descendant_paths_overlap() {
        let mut detector = WriteScopeConflictDetector::new();
        detector.acquire(&SubagentId::new("task-2"), scope(&["/work"])).expect("dir lock");
        let error = detector
            .acquire(&SubagentId::new("task-3"), scope(&["/work/src/lib.rs"]))
            .expect_err("descendant of held directory rejected");
        assert!(matches!(error, OrchestratorError::PolicyDenied(_)));
        assert_eq!(
            detector.conflicts_with(&scope(&["/work/src/a.rs"])),
            Some(SubagentId::new("task-2")),
            "ancestor scope conflicts both ways"
        );
    }

    #[test]
    fn relative_paths_are_rejected_before_any_lock() {
        let mut detector = WriteScopeConflictDetector::new();
        let error = detector
            .acquire(&SubagentId::new("task-2"), scope(&["relative/a.rs"]))
            .expect_err("relative paths fail closed");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("absolute"))
        );
        assert!(detector.is_empty(), "no lock recorded for a rejected scope");
    }

    #[test]
    fn empty_scope_records_nothing_and_never_conflicts() {
        let mut detector = WriteScopeConflictDetector::new();
        detector.acquire(&SubagentId::new("task-2"), Vec::new()).expect("no-op");
        assert!(detector.is_empty(), "empty scopes are not locked");
        assert_eq!(detector.conflicts_with(&[]), None);
    }

    #[test]
    fn release_frees_the_scope_for_reacquisition() {
        let mut detector = WriteScopeConflictDetector::new();
        let first = SubagentId::new("task-2");
        let second = SubagentId::new("task-3");
        detector.acquire(&first, scope(&["/work/a.rs"])).expect("acquires");
        assert!(detector.acquire(&second, scope(&["/work/a.rs"])).is_err(), "conflict while held");
        assert!(detector.release(&first), "held scope released");
        assert!(!detector.release(&first), "second release is a no-op");
        detector.acquire(&second, scope(&["/work/a.rs"])).expect("reacquires after release");
        assert_eq!(detector.held_scope(&second), Some(scope(&["/work/a.rs"]).as_slice()));
    }

    #[test]
    fn clear_releases_every_scope() {
        let mut detector = WriteScopeConflictDetector::new();
        detector.acquire(&SubagentId::new("task-2"), scope(&["/work/a.rs"])).expect("acquires");
        detector.acquire(&SubagentId::new("task-3"), scope(&["/work/b.rs"])).expect("acquires");
        detector.clear();
        assert!(detector.is_empty());
        assert_eq!(detector.holders(), Vec::<SubagentId>::new());
    }

    #[test]
    fn detector_roundtrips_through_json() {
        let mut detector = WriteScopeConflictDetector::new();
        detector.acquire(&SubagentId::new("task-2"), scope(&["/work/a.rs"])).expect("acquires");
        let json = serde_json::to_string(&detector).expect("serializes");
        let restored: WriteScopeConflictDetector = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, detector);
    }
}
