//! Optional git-worktree isolation for write subagents.
//!
//! An isolated child edits a detached worktree under the session worktree root
//! instead of the primary checkout. Tool paths are remapped before policy
//! evaluation, the child's scope is narrowed to the worktree root, and the
//! changed-file inventory plus patch are captured as handoff evidence before
//! the worktree is pruned.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;

/// Subdirectory of the isolation root holding child worktrees.
pub const WORKTREE_DIR: &str = "worktrees";
/// Maximum patch bytes retained as handoff evidence.
pub const MAX_WORKTREE_PATCH_BYTES: usize = 256 * 1024;

/// Whether a subagent edits the shared checkout or an isolated worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIsolation {
    /// Edit the primary checkout directly (default, historical behavior).
    #[default]
    Shared,
    /// Edit a detached git worktree created for this child.
    GitWorktree,
}

/// Session-level worktree isolation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceIsolationConfig {
    /// Opt-in switch. Disabled keeps shared-checkout behavior.
    pub enabled: bool,
    /// Root directory for child worktrees. `None` defaults under the primary
    /// repository root at `.ee/worktrees`.
    pub worktree_root: Option<PathBuf>,
    /// Keep worktrees after a completed child so the host can apply the patch;
    /// failed and cancelled children are always pruned.
    pub keep_worktrees: bool,
}

impl WorkspaceIsolationConfig {
    /// Whether the requested role isolation can be honored.
    #[must_use]
    pub fn allows(&self, isolation: WorkspaceIsolation) -> bool {
        self.enabled && isolation == WorkspaceIsolation::GitWorktree
    }

    /// Resolves the worktree root for a primary repository root.
    #[must_use]
    pub fn root_for(&self, primary_root: &Path) -> PathBuf {
        self.worktree_root.clone().unwrap_or_else(|| primary_root.join(".ee").join(WORKTREE_DIR))
    }
}

/// A detached worktree created for one isolated child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeWorkspace {
    name: String,
    primary_root: PathBuf,
    path: PathBuf,
}

impl WorktreeWorkspace {
    /// Creates a detached worktree at `<root>/<sanitized name>` from the
    /// primary repository `HEAD`.
    ///
    /// # Errors
    ///
    /// Fails closed when the primary root is not a git work tree, the target
    /// path already exists, or `git worktree add` returns non-zero.
    pub fn create(
        primary_root: &Path,
        worktree_root: &Path,
        name: &str,
    ) -> Result<Self, OrchestratorError> {
        let name = sanitize_worktree_name(name);
        if name.is_empty() {
            return Err(OrchestratorError::InvalidState(
                "worktree name must contain at least one safe character".into(),
            ));
        }
        if !primary_root.is_dir() {
            return Err(OrchestratorError::InvalidState(format!(
                "worktree primary root {} is not a directory",
                primary_root.display()
            )));
        }
        let path = worktree_root.join(&name);
        if path.exists() {
            return Err(OrchestratorError::InvalidState(format!(
                "worktree target {} already exists",
                path.display()
            )));
        }
        std::fs::create_dir_all(worktree_root).map_err(|error| {
            OrchestratorError::InvalidState(format!(
                "failed to create worktree root {}: {error}",
                worktree_root.display()
            ))
        })?;
        let output =
            git(primary_root, &["worktree", "add", "--detach", &path.to_string_lossy(), "HEAD"])?;
        if !output.status.success() {
            return Err(OrchestratorError::InvalidState(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(Self { name, primary_root: primary_root.to_path_buf(), path })
    }

    /// Worktree directory the child operates in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Worktree name (also its directory name).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Primary checkout this worktree was created from.
    #[must_use]
    pub fn primary_root(&self) -> &Path {
        &self.primary_root
    }

    /// Builds the path remapper for this worktree.
    #[must_use]
    pub fn remap(&self) -> WorkspacePathRemap {
        WorkspacePathRemap::new(self.primary_root.clone(), self.path.clone())
    }

    /// Captures a deterministic changed-file inventory plus a bounded patch.
    ///
    /// # Errors
    ///
    /// Fails when git cannot report the worktree status or diff.
    pub fn capture(&self) -> Result<WorktreeCapture, OrchestratorError> {
        let inventory = git(&self.path, &["status", "--porcelain=v1", "--untracked-files=all"])?;
        if !inventory.status.success() {
            return Err(OrchestratorError::InvalidState(format!(
                "git status failed in worktree {}: {}",
                self.path.display(),
                String::from_utf8_lossy(&inventory.stderr).trim()
            )));
        }
        let diff = git(&self.path, &["diff", "--binary", "HEAD"])?;
        if !diff.status.success() {
            return Err(OrchestratorError::InvalidState(format!(
                "git diff failed in worktree {}: {}",
                self.path.display(),
                String::from_utf8_lossy(&diff.stderr).trim()
            )));
        }
        let changed_files = String::from_utf8_lossy(&inventory.stdout)
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let patch = String::from_utf8_lossy(&diff.stdout).to_string();
        let patch_truncated = patch.len() > MAX_WORKTREE_PATCH_BYTES;
        let patch = if patch_truncated {
            let mut end = MAX_WORKTREE_PATCH_BYTES;
            while !patch.is_char_boundary(end) {
                end -= 1;
            }
            patch[..end].to_string()
        } else {
            patch
        };
        Ok(WorktreeCapture { worktree: self.path.clone(), changed_files, patch, patch_truncated })
    }

    /// Removes the worktree and its administrative entry.
    ///
    /// # Errors
    ///
    /// Fails only when the filesystem directory cannot be removed after a git
    /// prune attempt; a missing worktree is not an error.
    pub fn prune(&self) -> Result<(), OrchestratorError> {
        remove_worktree(&self.primary_root, &self.path)
    }
}

/// One entry reported by `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeEntry {
    /// Absolute worktree path.
    pub path: PathBuf,
    /// Checked-out branch, when not detached.
    pub branch: Option<String>,
    /// Whether HEAD is detached.
    pub detached: bool,
    /// Whether this is the bare repository entry.
    pub bare: bool,
}

/// Maximum worktree entries returned by [`list_worktrees`].
pub const MAX_WORKTREE_ENTRIES: usize = 64;

/// Lists git worktrees for `primary_root` in bounded porcelain order.
///
/// # Errors
///
/// Fails when git cannot list the repository's worktrees.
pub fn list_worktrees(primary_root: &Path) -> Result<Vec<WorktreeEntry>, OrchestratorError> {
    let output = git(primary_root, &["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        return Err(OrchestratorError::InvalidState(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in text.lines() {
        if line.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
                if entries.len() >= MAX_WORKTREE_ENTRIES {
                    break;
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                branch: None,
                detached: false,
                bare: false,
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(branch.to_string());
            } else if line == "detached" {
                entry.detached = true;
            } else if line == "bare" {
                entry.bare = true;
            }
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries.truncate(MAX_WORKTREE_ENTRIES);
    Ok(entries)
}

/// Removes one worktree and prunes its administrative entry.
///
/// Idempotent: a missing worktree is not an error. Callers must ensure `path`
/// is inside the configured worktree root.
///
/// # Errors
///
/// Fails only when the filesystem directory cannot be removed.
pub fn remove_worktree(primary_root: &Path, path: &Path) -> Result<(), OrchestratorError> {
    let _ = git(primary_root, &["worktree", "remove", "--force", &path.to_string_lossy()]);
    let _ = git(primary_root, &["worktree", "prune"]);
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|error| {
            OrchestratorError::InvalidState(format!(
                "failed to remove worktree {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

/// Captured worktree changes used as parent-visible handoff evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCapture {
    /// Worktree directory.
    pub worktree: PathBuf,
    /// `git status --porcelain` lines, sorted by git.
    pub changed_files: Vec<String>,
    /// Bounded unified binary diff.
    pub patch: String,
    /// Whether the patch was truncated to the evidence budget.
    pub patch_truncated: bool,
}

impl WorktreeCapture {
    /// Writes the bounded patch next to the worktree, returning its path so a
    /// host or root agent can apply it with `git apply`.
    ///
    /// # Errors
    ///
    /// Fails when the patch file cannot be written.
    pub fn write_patch(&self) -> Result<PathBuf, OrchestratorError> {
        let name = self
            .worktree
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("worktree"));
        let base = self.worktree.parent().unwrap_or_else(|| Path::new("."));
        let path = base.join(format!("{name}.patch"));
        std::fs::write(&path, &self.patch).map_err(|error| {
            OrchestratorError::InvalidState(format!(
                "failed to write worktree patch {}: {error}",
                path.display()
            ))
        })?;
        Ok(path)
    }

    /// Renders the bounded evidence line appended to the child handoff.
    #[must_use]
    pub fn evidence_text(&self) -> String {
        let mut text = format!(
            "Worktree isolation: {} changed file(s) under {}.",
            self.changed_files.len(),
            self.worktree.display()
        );
        for file in &self.changed_files {
            text.push_str("\n- ");
            text.push_str(file);
        }
        if self.patch_truncated {
            text.push_str("\nPatch evidence truncated at the capture budget.");
        }
        text
    }
}

/// Maps primary-checkout absolute paths into an isolated worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePathRemap {
    primary_root: PathBuf,
    isolated_root: PathBuf,
}

impl WorkspacePathRemap {
    /// Creates a remap from `primary_root` to `isolated_root`.
    #[must_use]
    pub fn new(primary_root: PathBuf, isolated_root: PathBuf) -> Self {
        Self { primary_root, isolated_root }
    }

    /// Remaps one absolute path inside the primary root; paths outside it are
    /// returned unchanged so non-workspace arguments keep their meaning.
    #[must_use]
    pub fn remap_path(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.primary_root) {
            Ok(relative) => self.isolated_root.join(relative),
            Err(_) => path.to_path_buf(),
        }
    }

    /// Remaps the path-bearing argument keys used by built-in tools.
    pub fn remap_arguments(&self, arguments: &mut serde_json::Value) {
        for key in ["path", "cwd"] {
            if let Some(value) = arguments.get_mut(key)
                && let Some(path) = value.as_str()
            {
                let mapped = self.remap_path(Path::new(path));
                if mapped.as_os_str() != path {
                    *value = serde_json::Value::String(mapped.to_string_lossy().into_owned());
                }
            }
        }
        if let Some(paths) = arguments.get_mut("paths").and_then(serde_json::Value::as_array_mut) {
            for value in paths {
                if let Some(path) = value.as_str() {
                    let mapped = self.remap_path(Path::new(path));
                    if mapped.as_os_str() != path {
                        *value = serde_json::Value::String(mapped.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
}

pub(crate) fn sanitize_worktree_name(name: &str) -> String {
    name.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn git(root: &Path, args: &[&str]) -> Result<std::process::Output, OrchestratorError> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|error| {
            OrchestratorError::InvalidState(format!(
                "failed to run git in {}: {error}",
                root.display()
            ))
        })
}

/// Normalizes a path for comparison without requiring it to exist.
#[must_use]
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_moves_only_paths_inside_the_primary_root() {
        let remap = WorkspacePathRemap::new(PathBuf::from("/work"), PathBuf::from("/tmp/wt"));
        assert_eq!(
            remap.remap_path(Path::new("/work/src/lib.rs")),
            PathBuf::from("/tmp/wt/src/lib.rs")
        );
        assert_eq!(remap.remap_path(Path::new("/other/x")), PathBuf::from("/other/x"));
        let mut arguments =
            serde_json::json!({ "path": "/work/a.rs", "cwd": "/other", "paths": ["/work/b.rs"] });
        remap.remap_arguments(&mut arguments);
        assert_eq!(arguments["path"], "/tmp/wt/a.rs");
        assert_eq!(arguments["cwd"], "/other");
        assert_eq!(arguments["paths"][0], "/tmp/wt/b.rs");
    }

    #[test]
    fn worktree_names_are_sanitized() {
        assert_eq!(sanitize_worktree_name("task-1/sub agent"), "task-1-sub-agent");
        assert_eq!(sanitize_worktree_name("---"), "");
    }

    #[test]
    fn lexical_normalize_collapses_dot_segments() {
        assert_eq!(lexical_normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    fn init_repo(root: &Path) {
        std::fs::create_dir_all(root).expect("repo dir");
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "test@example.invalid"]);
        run_git(root, &["config", "user.name", "ee test"]);
        std::fs::write(root.join("file.txt"), "base\n").expect("seed file");
        run_git(root, &["add", "file.txt"]);
        run_git(root, &["commit", "-q", "-m", "seed"]);
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn worktree_isolates_writes_and_captures_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let root = temp.path().join("worktrees");
        let worktree =
            WorktreeWorkspace::create(&repo, &root, "child-1").expect("worktree creates");

        std::fs::write(worktree.path().join("file.txt"), "isolated\n").expect("write worktree");
        std::fs::write(worktree.path().join("new.txt"), "new\n").expect("write worktree");

        assert_eq!(
            std::fs::read_to_string(repo.join("file.txt")).expect("primary file"),
            "base\n",
            "primary checkout is untouched"
        );
        let capture = worktree.capture().expect("capture");
        assert_eq!(capture.changed_files.len(), 2);
        assert!(capture.patch.contains("isolated"));
        assert!(capture.evidence_text().contains("changed file(s)"));

        worktree.prune().expect("prune");
        assert!(!worktree.path().exists());
    }

    #[test]
    fn existing_target_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let root = temp.path().join("worktrees");
        std::fs::create_dir_all(root.join("child-1")).expect("seed target");
        let error = WorktreeWorkspace::create(&repo, &root, "child-1").expect_err("must fail");
        assert!(error.to_string().contains("already exists"));
    }
}
