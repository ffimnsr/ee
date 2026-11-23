//! Where prompt assembly reads editable harness components.
//!
//! Sessions normally resolve guardrails, tool-guidance overrides, scripts, and
//! skills from the workspace. A probe session mounts a candidate harness
//! directory instead, so a rollout can be re-run without mutating the
//! workspace. The override directory is workspace-shaped: it contains an
//! `.agents/` tree with the component files.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Source of editable harness components for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSource {
    /// Resolve components from the session workspace roots.
    #[default]
    Workspace,
    /// Resolve components from a mounted candidate root.
    Override(PathBuf),
}

impl HarnessSource {
    /// Workspace-shaped root used for component discovery.
    ///
    /// The returned slice is either the session's own roots or a single
    /// override root; callers pass both results to the existing discovery
    /// functions unchanged.
    #[must_use]
    pub fn discovery_scope<'a>(
        &'a self,
        cwd: &'a Path,
        workspace_roots: &'a [PathBuf],
    ) -> (&'a Path, Vec<PathBuf>) {
        match self {
            Self::Workspace => (cwd, workspace_roots.to_vec()),
            Self::Override(root) => (root.as_path(), vec![root.clone()]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_source_keeps_the_session_roots() {
        let cwd = Path::new("/work");
        let roots = vec![PathBuf::from("/work"), PathBuf::from("/shared")];
        let (resolved_cwd, resolved_roots) = HarnessSource::Workspace.discovery_scope(cwd, &roots);
        assert_eq!(resolved_cwd, cwd);
        assert_eq!(resolved_roots, roots);
    }

    #[test]
    fn override_source_replaces_the_roots() {
        let source = HarnessSource::Override(PathBuf::from("/candidate"));
        let roots = [PathBuf::from("/work")];
        let (resolved_cwd, resolved_roots) = source.discovery_scope(Path::new("/work"), &roots);
        assert_eq!(resolved_cwd, Path::new("/candidate"));
        assert_eq!(resolved_roots, vec![PathBuf::from("/candidate")]);
    }
}
