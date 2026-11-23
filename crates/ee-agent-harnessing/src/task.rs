//! Harness task contract.
//!
//! One task adapts a session-shaped input to the improvement loop: a prompt, a
//! domain, a workspace-shaped base harness, bounded source files, and the rule
//! for what counts as delivered.

use std::path::{Path, PathBuf};

use ee_agent_orchestrator::harness_validation::HarnessTree;

use crate::channel::{Deliverable, DeliverableAsset, RunRecord};
use crate::error::{HarnessError, HarnessResult};

/// Maximum source files copied into one context directory.
pub const MAX_SOURCE_FILES: usize = 16;
/// Maximum bytes copied from one source file.
pub const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024;

/// One bounded source file copied into an improver context directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Repo-relative display name.
    pub name: String,
    /// Text content, bounded by [`MAX_SOURCE_FILE_BYTES`].
    pub content: String,
}

/// One improvable task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessTask {
    /// Stable task id used by run state and seeding.
    pub id: String,
    /// Task prompt the agent receives.
    pub prompt: String,
    /// Experience-library domain.
    pub domain: String,
    /// Workspace-shaped root whose `.agents` holds the base harness and whose
    /// files are the task source.
    pub workspace: PathBuf,
    /// Additional source roots, searched in order after the workspace.
    pub source_roots: Vec<PathBuf>,
    /// Explicit attachments, treated as additional source files.
    pub attachments: Vec<PathBuf>,
    /// Streaming-retention cutoff: only library entries with a strictly
    /// smaller sequence are visible. `None` exposes every completed entry.
    pub library_before_seq: Option<u64>,
    /// Experience-library root. `None` uses the task workspace; measurement
    /// runs set a scratch root so they never contaminate the daily library.
    pub library_root: Option<PathBuf>,
}

impl HarnessTask {
    /// Creates a single-workspace task.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        prompt: impl Into<String>,
        domain: impl Into<String>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            domain: domain.into(),
            workspace: workspace.into(),
            source_roots: Vec::new(),
            attachments: Vec::new(),
            library_before_seq: None,
            library_root: None,
        }
    }

    /// Scopes library reads and writes to a measurement root.
    #[must_use]
    pub fn with_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.library_root = Some(root.into());
        self
    }

    /// Root experience discovery should use for this task.
    #[must_use]
    pub fn library_root(&self) -> &Path {
        self.library_root.as_deref().unwrap_or(&self.workspace)
    }

    /// Excludes the task's own experience entry (and every later entry) from
    /// the online library cues. Used only by the offline leave-one-out eval.
    #[must_use]
    pub fn with_library_before_seq(mut self, seq: u64) -> Self {
        self.library_before_seq = Some(seq);
        self
    }

    /// Validates the contract before any model call.
    pub fn validate(&self) -> HarnessResult<()> {
        if self.id.trim().is_empty() {
            return Err(HarnessError::State("task id must not be empty".into()));
        }
        if self.prompt.trim().is_empty() {
            return Err(HarnessError::State("task prompt must not be empty".into()));
        }
        if !valid_domain(&self.domain) {
            return Err(HarnessError::State(format!(
                "task domain {:?} must be a lowercase identifier",
                self.domain
            )));
        }
        if !self.workspace.is_dir() {
            return Err(HarnessError::State(format!(
                "task workspace is not a directory: {}",
                self.workspace.display()
            )));
        }
        Ok(())
    }

    /// Base harness loaded from the workspace.
    pub fn base_harness(&self) -> HarnessResult<HarnessTree> {
        HarnessTree::load_components(&self.workspace.join(".agents"))
            .map_err(HarnessError::Validation)
    }

    /// Whether a run delivered the task result.
    ///
    /// Delivered means a non-empty final deliverable was observed.
    #[must_use]
    pub fn delivered(&self, run: &RunRecord) -> bool {
        run.delivered.as_ref().is_some_and(|deliverable| {
            !deliverable.text.trim().is_empty() || !deliverable.assets.is_empty()
        })
    }

    /// Deliverable text, or an empty string when the run delivered nothing.
    #[must_use]
    pub fn deliverable_text(&self, run: &RunRecord) -> String {
        run.delivered.as_ref().map(|deliverable| deliverable.text.clone()).unwrap_or_default()
    }

    /// Deliverable assets, or an empty list when the run delivered nothing.
    #[must_use]
    pub fn deliverable_assets<'a>(&self, run: &'a RunRecord) -> &'a [DeliverableAsset] {
        run.delivered.as_ref().map(|deliverable| deliverable.assets.as_slice()).unwrap_or(&[])
    }

    /// Domain used for library scoping; explicit domain wins.
    #[must_use]
    pub fn domain_of(&self) -> &str {
        &self.domain
    }

    /// Bounded text source files for the context directory.
    #[must_use]
    pub fn source_files(&self) -> Vec<SourceFile> {
        let roots = std::iter::once(self.workspace.clone())
            .chain(self.source_roots.iter().cloned())
            .collect::<Vec<_>>();
        let mut files = Vec::new();
        for root in roots {
            collect_source_files(&root, &root, 0, &mut files);
            if files.len() >= MAX_SOURCE_FILES {
                break;
            }
        }
        for attachment in &self.attachments {
            if files.len() >= MAX_SOURCE_FILES {
                break;
            }
            if let Some(file) = read_source_file(attachment, attachment) {
                files.push(file);
            }
        }
        files.truncate(MAX_SOURCE_FILES);
        files
    }
}

/// Renders a deliverable for selector/reflector evidence.
///
/// Oversized bodies switch to names-only mode so a candidate can never flood
/// the context with a whole run directory.
#[must_use]
pub fn render_deliverable(deliverable: &Deliverable, max_bytes: usize) -> String {
    if deliverable.text.len() <= max_bytes {
        return deliverable.text.clone();
    }
    names_only(deliverable)
}

/// Names-only deliverable rendering.
#[must_use]
pub fn names_only(deliverable: &Deliverable) -> String {
    let mut text = String::from("[names-only deliverable]\n");
    if !deliverable.assets.is_empty() {
        for asset in &deliverable.assets {
            text.push_str(&format!("- {} ({} bytes)\n", asset.name, asset.bytes));
        }
    }
    if !deliverable.text.trim().is_empty() {
        let first: String = deliverable.text.chars().take(240).collect();
        text.push_str(&format!("\nexcerpt: {first}…\n"));
    }
    text
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.chars().count() <= 64
        && domain
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && domain.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
        })
}

fn collect_source_files(root: &Path, directory: &Path, depth: usize, files: &mut Vec<SourceFile>) {
    const IGNORED: &[&str] =
        &["target", "node_modules", ".git", ".atlas", "dist", "build", ".agents"];
    if depth > 2 || files.len() >= MAX_SOURCE_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if files.len() >= MAX_SOURCE_FILES {
            return;
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if IGNORED.contains(&name) || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_source_files(root, &path, depth + 1, files);
        } else if let Some(file) = read_source_file(&path, root) {
            files.push(file);
        }
    }
}

fn read_source_file(path: &Path, root: &Path) -> Option<SourceFile> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_FILE_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let content = String::from_utf8(bytes).ok()?;
    let name = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    Some(SourceFile { name, content })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Deliverable;

    #[test]
    fn task_validation_and_base_harness() {
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t1", "do work", "rust", workspace.path());
        task.validate().expect("valid");
        assert_eq!(task.domain_of(), "rust");
        assert!(task.base_harness().expect("tree").is_empty());

        let invalid = HarnessTask::new("", "p", "Rust!", workspace.path());
        assert!(invalid.validate().is_err());
        let missing = HarnessTask::new("t", "p", "rust", workspace.path().join("missing"));
        assert!(missing.validate().is_err());
    }

    #[test]
    fn source_files_are_bounded_and_skip_byproducts() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("src")).expect("src");
        std::fs::create_dir_all(workspace.path().join("target")).expect("target");
        std::fs::write(workspace.path().join("src/lib.rs"), "pub fn a() {}\n").expect("write");
        std::fs::write(workspace.path().join("target/huge.rs"), "ignored\n").expect("write");
        let task = HarnessTask::new("t", "p", "rust", workspace.path());
        let files = task.source_files();
        assert!(files.iter().any(|file| file.name == "src/lib.rs"));
        assert!(!files.iter().any(|file| file.name.contains("target")));
        assert!(files.len() <= MAX_SOURCE_FILES);
    }

    #[test]
    fn library_root_defaults_to_the_workspace_and_can_be_scoped() {
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path());
        assert_eq!(task.library_root(), workspace.path());
        let scratch = tempfile::tempdir().expect("scratch");
        let scoped = task.with_library_root(scratch.path());
        assert_eq!(scoped.library_root(), scratch.path());
        assert_eq!(scoped.workspace, workspace.path());
    }

    #[test]
    fn delivered_requires_a_non_empty_final_result() {
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path());
        let mut run = RunRecord {
            label: String::from("ref-0"),
            delivered: None,
            rollout: ee_agent_orchestrator::rollout_view::RolloutView::default(),
            tool_records: Vec::new(),
            workspace_diff: None,
            sessions: 1,
            rollouts: 1,
            usage_tokens: None,
        };
        assert!(!task.delivered(&run));
        run.delivered = Some(Deliverable { text: String::new(), assets: Vec::new() });
        assert!(!task.delivered(&run), "an empty body with no assets did not deliver");
        run.delivered = Some(Deliverable {
            text: String::new(),
            assets: vec![DeliverableAsset {
                name: String::from("out.txt"),
                bytes: 3,
                content: None,
            }],
        });
        assert!(task.delivered(&run), "a named asset counts as delivered");
        run.delivered = Some(Deliverable { text: String::from("done"), assets: Vec::new() });
        assert!(task.delivered(&run));
    }

    #[test]
    fn deliverable_rendering_switches_to_names_only() {
        let deliverable = Deliverable {
            text: "x".repeat(100),
            assets: vec![DeliverableAsset { name: "out.txt".into(), bytes: 100, content: None }],
        };
        let text = render_deliverable(&deliverable, 10);
        assert!(text.contains("names-only"));
        assert!(text.contains("out.txt"));
    }
}
