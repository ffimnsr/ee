//! Bounded experience-library evidence for improver contexts.
//!
//! The library entry's `harness/` tree is archived data for future improvers,
//! not active `.agents` configuration. This module is its production reader:
//! it resolves the tree beside `EXPERIENCE.md`, rejects symlinks and path
//! escapes, and skips malformed entries with a recorded note. Context building
//! owns redaction, grader refusal, and the on-disk copy.

use std::path::{Component, Path};

use ee_agent_orchestrator::harness_validation::HarnessTree;
use ee_agent_orchestrator::user_experience::{
    ExperienceEntry, ExperienceRegistry, HARNESS_DIR, WORKSPACE_EXPERIENCE_DIR,
};

use crate::task::HarnessTask;

/// Maximum library entries copied into one stage context.
pub const MAX_LIBRARY_ENTRIES: usize = 16;
/// Maximum total bytes copied from the library into one stage context.
pub const MAX_LIBRARY_TOTAL_BYTES: usize = 1024 * 1024;
/// Maximum bytes copied from one library file.
pub const MAX_LIBRARY_FILE_BYTES: usize = 64 * 1024;
/// Maximum bytes used by the stage-local cue catalog.
pub const MAX_LIBRARY_CUE_BYTES: usize = 4 * 1024;
/// Maximum characters retained in one cue line.
const MAX_LIBRARY_CUE_CHARS: usize = 200;

/// One prior experience entry with its archived harness tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    /// Library domain.
    pub domain: String,
    /// Adoption sequence.
    pub seq: u64,
    /// Entry directory name (`<seq>-<slug>`).
    pub directory: String,
    /// `EXPERIENCE.md` content.
    pub experience: String,
    /// Archived adopted harness tree.
    pub harness: HarnessTree,
}

/// Bounded library evidence for one task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryEvidence {
    /// Bounded cue catalog for `library/INDEX.md`.
    pub cues: Vec<String>,
    /// Full entries copied beside the index.
    pub entries: Vec<LibraryEntry>,
    /// Entries skipped by bounds or malformed data.
    pub skipped: Vec<String>,
}

impl LibraryEvidence {
    /// Whether there is anything to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.skipped.is_empty()
    }
}

/// One invocation's experience-library location.
///
/// The root is the library directory itself, so the reader and the writer
/// receive the same path by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryLocation {
    root: std::path::PathBuf,
}

impl LibraryLocation {
    /// Uses an explicit library root.
    #[must_use]
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default single-task root: `<workspace>/.agents/experience`.
    #[must_use]
    pub fn workspace(workspace: &Path) -> Self {
        Self { root: workspace.join(WORKSPACE_EXPERIENCE_DIR) }
    }

    /// Run-scoped root for sweeps: `<run-root>/library/<mode>-k<k>`.
    #[must_use]
    pub fn run_scoped(run_root: &Path, mode: &str, k: usize) -> Self {
        Self { root: run_root.join("library").join(format!("{mode}-k{k}")) }
    }

    /// Resolves the invocation root: explicit override, then the mode default.
    #[must_use]
    pub fn resolve(default: Self, explicit: Option<&Path>) -> Self {
        explicit.map_or(default, Self::new)
    }

    /// Library root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Loads prior same-domain entries for one task, newest first.
#[must_use]
pub fn load(task: &HarnessTask) -> LibraryEvidence {
    let root = task.library_root();
    let registry = ExperienceRegistry::discover_at(&root);
    let mut evidence = LibraryEvidence::default();
    let mut total_bytes = 0usize;
    let mut cue_bytes = 0usize;
    for entry in registry.filtered(Some(task.domain_of()), task.library_before_seq) {
        if evidence.entries.len() >= MAX_LIBRARY_ENTRIES {
            break;
        }
        if task.excludes_experience(entry.seq) || task.excludes_task(&entry.task) {
            continue;
        }
        let directory = entry
            .path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        if directory.is_empty() {
            evidence
                .skipped
                .push(format!("{} #{}: entry has no directory", entry.domain, entry.seq));
            continue;
        }
        let harness = match load_harness_tree(entry) {
            Ok(harness) => harness,
            Err(reason) => {
                tracing::warn!(
                    domain = %entry.domain,
                    seq = entry.seq,
                    %reason,
                    "skipping library entry with an unusable harness tree"
                );
                evidence.skipped.push(format!("{} #{}: {reason}", entry.domain, entry.seq));
                continue;
            }
        };
        let entry_bytes =
            entry.content.len() + harness.files.values().map(String::len).sum::<usize>();
        if entry_bytes > MAX_LIBRARY_TOTAL_BYTES
            || total_bytes.saturating_add(entry_bytes) > MAX_LIBRARY_TOTAL_BYTES
        {
            evidence.skipped.push(format!(
                "{} #{}: exceeds the {} byte library cap",
                entry.domain, entry.seq, MAX_LIBRARY_TOTAL_BYTES
            ));
            continue;
        }
        total_bytes += entry_bytes;
        if let Some(line) = cue_line(entry) {
            let bytes = line.len() + 1;
            if cue_bytes.saturating_add(bytes) <= MAX_LIBRARY_CUE_BYTES {
                cue_bytes += bytes;
                evidence.cues.push(line);
            }
        }
        evidence.entries.push(LibraryEntry {
            domain: entry.domain.clone(),
            seq: entry.seq,
            directory,
            experience: entry.content.clone(),
            harness,
        });
    }
    evidence
}

fn load_harness_tree(entry: &ExperienceEntry) -> Result<HarnessTree, String> {
    let entry_dir =
        entry.path.parent().ok_or_else(|| String::from("entry file has no parent directory"))?;
    let canonical_entry = std::fs::canonicalize(entry_dir)
        .map_err(|error| format!("cannot resolve {}: {error}", entry_dir.display()))?;
    let harness_dir = canonical_entry.join(HARNESS_DIR);
    let metadata = std::fs::symlink_metadata(&harness_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("missing {HARNESS_DIR}/ tree")
        } else {
            format!("cannot stat {}: {error}", harness_dir.display())
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{HARNESS_DIR}/ tree is a symlink"));
    }
    if !metadata.is_dir() {
        return Err(format!("{HARNESS_DIR}/ is not a directory"));
    }
    let canonical_harness = std::fs::canonicalize(&harness_dir)
        .map_err(|error| format!("cannot resolve {}: {error}", harness_dir.display()))?;
    if !canonical_harness.starts_with(&canonical_entry) {
        return Err(format!("{HARNESS_DIR}/ tree escapes the entry directory"));
    }
    HarnessTree::load_components(&canonical_harness)
}

fn cue_line(entry: &ExperienceEntry) -> Option<String> {
    let summary = entry.cue.clone().unwrap_or_else(|| {
        let components: Vec<&str> =
            entry.edits.iter().map(|edit| edit.component.as_str()).collect();
        format!("edits: {}", components.join(", "))
    });
    let summary = truncate_chars(&summary, MAX_LIBRARY_CUE_CHARS);
    Some(format!("- [{} #{}] {summary}", entry.domain, entry.seq))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

/// Whether a tree-relative path is safe to join under a context directory.
#[must_use]
pub fn safe_relative_path(relative: &str) -> bool {
    let path = Path::new(relative);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ee_agent_orchestrator::user_experience::{
        EditRow, EditVerdict, ExperienceConfidence, ExperienceEntry, ExperienceScope, save_entry_at,
    };

    use super::*;

    fn entry(seq: u64, task: &str, cue: &str) -> ExperienceEntry {
        ExperienceEntry {
            seq,
            domain: String::from("rust"),
            task: task.to_string(),
            confidence: ExperienceConfidence::High,
            edits: vec![EditRow {
                component: String::from("guardrails.md"),
                candidates: vec![String::from("probe-0-inv")],
                change: String::from("added a rule"),
                observed: String::from("probe delivered"),
                verdict: EditVerdict::Helped,
            }],
            cue: Some(cue.to_string()),
            lesson: Some(String::from("keep the rule")),
            evidence: Some(String::from("contrast")),
            content: String::new(),
            scope: ExperienceScope::Workspace,
            path: std::path::PathBuf::new(),
            display_path: String::new(),
        }
    }

    fn tree(files: &[(&str, &str)]) -> HarnessTree {
        HarnessTree::new(
            files
                .iter()
                .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn round_trips_the_written_harness_tree() {
        let root = tempfile::tempdir().expect("root");
        let library = root.path().join(".agents/experience");
        save_entry_at(
            &library,
            &entry(1, "task-a", "first cue"),
            Some(&tree(&[("guardrails.md", "verify writes\n")])),
        )
        .expect("save");
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path()).with_library_root(&library);
        let evidence = load(&task);
        assert_eq!(evidence.entries.len(), 1);
        assert_eq!(evidence.entries[0].directory, "1-task-a");
        assert_eq!(
            evidence.entries[0].harness.files.get("guardrails.md").map(String::as_str),
            Some("verify writes\n")
        );
        assert!(evidence.cues[0].contains("first cue"));
    }

    #[test]
    fn missing_or_malformed_harness_is_skipped_with_a_note() {
        let root = tempfile::tempdir().expect("root");
        let library = root.path().join(".agents/experience");
        save_entry_at(&library, &entry(1, "task-a", "no tree"), None).expect("save");
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path()).with_library_root(&library);
        let evidence = load(&task);
        assert!(evidence.entries.is_empty());
        assert_eq!(evidence.skipped.len(), 1);
        assert!(evidence.skipped[0].contains("missing harness"));
    }

    #[test]
    fn own_task_and_cutoff_entries_are_invisible() {
        let root = tempfile::tempdir().expect("root");
        let library = root.path().join(".agents/experience");
        let harness = tree(&[("guardrails.md", "rule\n")]);
        save_entry_at(&library, &entry(1, "task-a", "a"), Some(&harness)).expect("a");
        save_entry_at(&library, &entry(2, "task-b", "b"), Some(&harness)).expect("b");
        save_entry_at(&library, &entry(3, "task-c", "c"), Some(&harness)).expect("c");
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("task-b", "p", "rust", workspace.path())
            .with_library_root(&library)
            .with_library_before_seq(3)
            .with_excluded_task_id("task-b");
        let evidence = load(&task);
        let seqs: Vec<u64> = evidence.entries.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, vec![1]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_harness_tree_is_skipped() {
        let root = tempfile::tempdir().expect("root");
        let library = root.path().join(".agents/experience");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("guardrails.md"), "escaped\n").expect("rule");
        save_entry_at(&library, &entry(1, "task-a", "cue"), None).expect("save");
        let entry_dir = library.join("rust/entries/1-task-a");
        std::os::unix::fs::symlink(&outside, entry_dir.join(HARNESS_DIR)).expect("symlink");
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path()).with_library_root(&library);
        let evidence = load(&task);
        assert!(evidence.entries.is_empty());
        assert!(evidence.skipped[0].contains("symlink"));
    }

    #[test]
    fn domain_scope_isolates_entries() {
        let root = tempfile::tempdir().expect("root");
        let library = root.path().join(".agents/experience");
        let harness = tree(&[("guardrails.md", "rule\n")]);
        save_entry_at(&library, &entry(1, "task-a", "rust cue"), Some(&harness)).expect("rust");
        let mut docs = entry(1, "task-b", "docs cue");
        docs.domain = String::from("docs");
        save_entry_at(&library, &docs, Some(&harness)).expect("docs");
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path()).with_library_root(&library);
        let evidence = load(&task);
        assert_eq!(evidence.entries.len(), 1);
        assert_eq!(evidence.entries[0].domain, "rust");
        assert!(evidence.cues.iter().all(|cue| cue.contains("rust")));
    }

    #[test]
    fn caps_entries_deterministically() {
        let root = tempfile::tempdir().expect("root");
        let library = root.path().join(".agents/experience");
        let harness = tree(&[("guardrails.md", "rule\n")]);
        for seq in 1..=(MAX_LIBRARY_ENTRIES as u64 + 1) {
            save_entry_at(
                &library,
                &entry(seq, &format!("task-{seq}"), &format!("cue {seq}")),
                Some(&harness),
            )
            .expect("save");
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let task = HarnessTask::new("t", "p", "rust", workspace.path()).with_library_root(&library);
        let evidence = load(&task);
        assert_eq!(evidence.entries.len(), MAX_LIBRARY_ENTRIES);
        assert_eq!(evidence.entries[0].seq, MAX_LIBRARY_ENTRIES as u64 + 1);
    }

    #[test]
    fn safe_relative_paths_reject_escapes() {
        assert!(safe_relative_path("skills/a/SKILL.md"));
        assert!(!safe_relative_path("../outside.md"));
        assert!(!safe_relative_path("/absolute.md"));
    }

    #[test]
    fn library_location_resolves_workspace_run_and_explicit_roots() {
        let workspace = Path::new("/work");
        let run_root = Path::new("/state/run");
        assert_eq!(
            LibraryLocation::resolve(LibraryLocation::workspace(workspace), None).root(),
            workspace.join(WORKSPACE_EXPERIENCE_DIR)
        );
        assert_eq!(
            LibraryLocation::resolve(LibraryLocation::run_scoped(run_root, "parallel", 2), None)
                .root(),
            run_root.join("library/parallel-k2")
        );
        assert_eq!(
            LibraryLocation::resolve(
                LibraryLocation::workspace(workspace),
                Some(Path::new("/scratch"))
            )
            .root(),
            Path::new("/scratch")
        );
    }
}
