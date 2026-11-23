//! Run state, resume rules, and improver session cache.
//!
//! A completed run writes a per-task summary only after the run is observed to
//! return. A rerun resumes only when the task inputs and the adopted harness
//! fingerprints are unchanged and the adopted artifact is still present;
//! otherwise it re-runs with a logged reason. Improver replies are cached
//! content-addressed by label, mission, system, and context digest, with an
//! explicit workspace-present check before a hit is used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ee_agent_orchestrator::harness_validation::HarnessTree;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::channel::ImproverReply;
use crate::error::{HarnessError, HarnessResult};
use crate::task::HarnessTask;

const IGNORED_COMPONENTS: &[&str] = &["target", "node_modules", ".git", "dist", "build", ".atlas"];

/// Content fingerprint (hex SHA-256).
pub type Fingerprint = String;

/// Persisted per-task run summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    /// Schema version.
    pub schema_version: u32,
    /// Task id.
    pub task_id: String,
    /// Run mode.
    pub mode: String,
    /// Recursion depth.
    pub k: usize,
    /// Final internal label chosen by selection, when any.
    pub selected_label: Option<String>,
    /// Harness fingerprint observed at completion.
    pub harness_fingerprint: Fingerprint,
    /// Task-input fingerprint observed at completion.
    pub task_fingerprint: Fingerprint,
    /// Whether the run was observed to return and the summary is complete.
    pub completed: bool,
    /// Recorded terminal state (`delivered`, `base-failed`, ...).
    pub terminal_state: String,
}

/// Resume decision for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// Inputs and harness are unchanged; reuse the recorded result.
    Resume,
    /// Re-run with a logged reason.
    Rerun(String),
}

/// Fingerprints a workspace-shaped root's `.agents` component tree.
pub fn fingerprint_harness(root: &Path) -> HarnessResult<Fingerprint> {
    let tree =
        HarnessTree::load_components(&root.join(".agents")).map_err(HarnessError::Validation)?;
    Ok(fingerprint_tree(&tree))
}

/// Fingerprints an already-loaded tree with sorted paths.
#[must_use]
pub fn fingerprint_tree(tree: &HarnessTree) -> Fingerprint {
    let mut hasher = Sha256::new();
    for (path, content) in &tree.files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(content.as_bytes());
        hasher.update([0xff]);
    }
    hex(hasher.finalize())
}

/// Fingerprints task inputs: prompt, domain, and source file contents.
#[must_use]
pub fn fingerprint_task(task: &HarnessTask) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update(task.id.as_bytes());
    hasher.update(task.prompt.as_bytes());
    hasher.update(task.domain.as_bytes());
    match task.library_before_seq {
        Some(seq) => {
            hasher.update([1]);
            hasher.update(seq.to_le_bytes());
        }
        None => hasher.update([0]),
    }
    for seq in &task.excluded_experience_seqs {
        hasher.update(seq.to_le_bytes());
    }
    hasher.update(
        task.library_root.as_deref().map(Path::to_string_lossy).unwrap_or_default().as_bytes(),
    );
    for file in task.source_files() {
        hasher.update(file.name.as_bytes());
        hasher.update([0]);
        hasher.update(file.content.as_bytes());
        hasher.update([0xff]);
    }
    hex(hasher.finalize())
}

/// Path of one run summary inside a run root.
#[must_use]
pub fn summary_path(run_root: &Path) -> PathBuf {
    run_root.join("run-summary.json")
}

/// Loads a run summary, or `None` when absent or malformed.
#[must_use]
pub fn load_summary(run_root: &Path) -> Option<RunSummary> {
    let text = std::fs::read_to_string(summary_path(run_root)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes a completed summary. Only call this after the run returned.
pub fn save_summary(run_root: &Path, summary: &RunSummary) -> HarnessResult<()> {
    if !summary.completed {
        return Err(HarnessError::State(
            "refusing to write a completion summary before the run completes".into(),
        ));
    }
    std::fs::create_dir_all(run_root)?;
    let text = serde_json::to_string_pretty(summary)
        .map_err(|error| HarnessError::State(error.to_string()))?;
    std::fs::write(summary_path(run_root), text)?;
    Ok(())
}

/// Decides whether a summary may be resumed.
#[must_use]
pub fn resume_decision(
    summary: &RunSummary,
    harness: &HarnessTree,
    task: &HarnessTask,
    adopted_artifacts_present: bool,
) -> ResumeDecision {
    if !summary.completed {
        return ResumeDecision::Rerun(String::from("prior run has no completion marker"));
    }
    if !adopted_artifacts_present {
        return ResumeDecision::Rerun(String::from("adopted artifacts are missing"));
    }
    if fingerprint_tree(harness) != summary.harness_fingerprint {
        return ResumeDecision::Rerun(String::from("adopted harness tree changed"));
    }
    if fingerprint_task(task) != summary.task_fingerprint {
        return ResumeDecision::Rerun(String::from("task inputs changed"));
    }
    ResumeDecision::Resume
}

/// Content-addressed improver reply cache.
#[derive(Debug)]
pub struct ImproverCache {
    root: PathBuf,
    audit: std::sync::Mutex<Vec<CacheAudit>>,
}

/// One cache audit row, recorded for every call whether hit or miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheAudit {
    /// Cache key.
    pub key: String,
    /// Whether a reply was served from the cache.
    pub hit: bool,
}

impl ImproverCache {
    /// Creates a cache rooted under the run directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), audit: std::sync::Mutex::new(Vec::new()) }
    }

    /// Cache key from label, mission, system contract, and context digest.
    #[must_use]
    pub fn key(label: &str, mission: &str, system: &str, context_digest: &str) -> String {
        let mut hasher = Sha256::new();
        for part in [label, mission, system, context_digest] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hex(hasher.finalize())
    }

    /// Reads one reply when the cached tree still passes the presence check.
    #[must_use]
    pub fn get(
        &self,
        key: &str,
        still_present: impl Fn(&HarnessTree) -> bool,
    ) -> Option<ImproverReply> {
        let path = self.root.join(format!("{key}.json"));
        let hit = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<CachedReply>(&text).ok())
            .and_then(|cached| {
                let tree = HarnessTree::new(cached.files.into_iter().collect());
                still_present(&tree).then_some(ImproverReply { tree, notes: cached.notes })
            });
        self.audit
            .lock()
            .expect("cache audit poisoned")
            .push(CacheAudit { key: key.to_string(), hit: hit.is_some() });
        hit
    }

    /// Stores one reply.
    pub fn store(&self, key: &str, reply: &ImproverReply) -> HarnessResult<()> {
        std::fs::create_dir_all(&self.root)?;
        let cached = CachedReply {
            files: reply.tree.files.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            notes: reply.notes.clone(),
        };
        let text = serde_json::to_string(&cached)
            .map_err(|error| HarnessError::State(error.to_string()))?;
        std::fs::write(self.root.join(format!("{key}.json")), text)?;
        Ok(())
    }

    /// Audit rows in call order.
    #[must_use]
    pub fn audit(&self) -> Vec<CacheAudit> {
        self.audit.lock().expect("cache audit poisoned").clone()
    }
}

#[derive(Serialize, Deserialize)]
struct CachedReply {
    files: BTreeMap<String, String>,
    notes: String,
}

/// Hashes one context directory's files for the cache key.
#[must_use]
pub fn context_digest(directory: &Path) -> String {
    let mut hasher = Sha256::new();
    let mut files = Vec::new();
    collect_files(directory, 0, &mut files);
    files.sort();
    for file in files {
        let relative = file.strip_prefix(directory).unwrap_or(&file).to_string_lossy();
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        if let Ok(bytes) = std::fs::read(&file) {
            hasher.update(&bytes);
        }
        hasher.update([0xff]);
    }
    hex(hasher.finalize())
}

fn collect_files(directory: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 4 || files.len() > 256 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if IGNORED_COMPONENTS.contains(&name) {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, depth + 1, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for byte in bytes.as_ref() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(workspace: &Path) -> HarnessTask {
        HarnessTask::new("t1", "do work", "rust", workspace)
    }

    #[test]
    fn harness_fingerprint_ignores_byproducts() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join(".agents")).expect("agents");
        std::fs::write(workspace.path().join(".agents/guardrails.md"), "rule\n").expect("write");
        std::fs::create_dir_all(workspace.path().join("target")).expect("target");
        std::fs::write(workspace.path().join("target/junk"), "junk").expect("junk");
        let first = fingerprint_harness(workspace.path()).expect("fingerprint");
        let second = fingerprint_harness(workspace.path()).expect("fingerprint");
        assert_eq!(first, second);
        std::fs::write(workspace.path().join(".agents/guardrails.md"), "changed\n").expect("write");
        assert_ne!(first, fingerprint_harness(workspace.path()).expect("fingerprint"));
    }

    #[test]
    fn resume_requires_completed_matching_inputs_and_present_artifacts() {
        let workspace = tempfile::tempdir().expect("workspace");
        let task = task(workspace.path());
        let tree = HarnessTree::default();
        let summary = RunSummary {
            schema_version: 1,
            task_id: task.id.clone(),
            mode: "parallel".into(),
            k: 1,
            selected_label: Some("ref-0".into()),
            harness_fingerprint: fingerprint_tree(&tree),
            task_fingerprint: fingerprint_task(&task),
            completed: true,
            terminal_state: "delivered".into(),
        };
        assert_eq!(resume_decision(&summary, &tree, &task, true), ResumeDecision::Resume);
        assert!(matches!(resume_decision(&summary, &tree, &task, false), ResumeDecision::Rerun(_)));

        let mut incomplete = summary.clone();
        incomplete.completed = false;
        assert!(matches!(
            resume_decision(&incomplete, &tree, &task, true),
            ResumeDecision::Rerun(_)
        ));

        let changed = HarnessTask::new("t1", "different", "rust", workspace.path());
        assert!(matches!(
            resume_decision(&summary, &tree, &changed, true),
            ResumeDecision::Rerun(_)
        ));
    }

    #[test]
    fn summary_write_requires_a_completion_marker() {
        let root = tempfile::tempdir().expect("root");
        let mut summary = RunSummary {
            schema_version: 1,
            task_id: "t".into(),
            mode: "parallel".into(),
            k: 1,
            selected_label: None,
            harness_fingerprint: String::new(),
            task_fingerprint: String::new(),
            completed: false,
            terminal_state: "running".into(),
        };
        assert!(save_summary(root.path(), &summary).is_err());
        summary.completed = true;
        save_summary(root.path(), &summary).expect("save");
        assert_eq!(load_summary(root.path()).expect("load").task_id, "t");
    }

    #[test]
    fn cache_hits_only_when_the_tree_is_present_and_audits_every_call() {
        let root = tempfile::tempdir().expect("root");
        let cache = ImproverCache::new(root.path());
        let key = ImproverCache::key("inv-0", "mission", "system", "digest");
        assert!(cache.get(&key, |_| true).is_none());
        let mut files = BTreeMap::new();
        files.insert("guardrails.md".to_string(), "rule\n".to_string());
        let reply = ImproverReply { tree: HarnessTree::new(files), notes: "notes".into() };
        cache.store(&key, &reply).expect("store");
        assert!(cache.get(&key, |_| false).is_none());
        let hit = cache.get(&key, |tree| tree.files.contains_key("guardrails.md")).expect("hit");
        assert_eq!(hit.notes, "notes");
        let audit = cache.audit();
        assert_eq!(audit.len(), 3);
        assert!(!audit[0].hit);
        assert!(!audit[1].hit);
        assert!(audit[2].hit);
    }

    #[test]
    fn context_digest_is_stable_and_content_sensitive() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("a.md"), "one").expect("write");
        let first = context_digest(root.path());
        assert_eq!(first, context_digest(root.path()));
        std::fs::write(root.path().join("a.md"), "two").expect("write");
        assert_ne!(first, context_digest(root.path()));
    }
}
