//! Hermetic parity contract for the recursive-harness closure.
//!
//! These tests use a deterministic in-process channel (`FakeAgentTransport`
//! equivalent) and never touch the network. They cover the approval write path,
//! full-library context, stream retention, and forward-only learning.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ee_agent_harnessing::{
    Channel, ChannelFuture, ExperienceEntry, HarnessConfig, HarnessError, HarnessTask, HarnessTree,
    ImproverReply, ImproverRequest, ImproverRole, ReflectionSource, ReflectionWrite,
    ReflectionWriter, RunRecord, RunRequest, run_stream, run_task, save_entry_at,
};
use ee_agent_orchestrator::rollout_view::RolloutView;

struct FakeAgentTransport {
    improves: Arc<Mutex<Vec<ImproverRequest>>>,
}

impl Channel for FakeAgentTransport {
    fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
        let label = request.label;
        Box::pin(async move {
            Ok(RunRecord {
                label,
                delivered: Some(ee_agent_harnessing::Deliverable {
                    text: String::from("delivered"),
                    assets: Vec::new(),
                }),
                rollout: RolloutView::default(),
                tool_records: Vec::new(),
                workspace_diff: Some(String::from("- added out.txt")),
                sessions: 1,
                rollouts: 1,
                usage_tokens: None,
            })
        })
    }

    fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
        self.improves.lock().expect("improves").push(request.clone());
        Box::pin(async move {
            let tree = match request.role {
                ImproverRole::Inv | ImproverRole::Dom | ImproverRole::Composer => {
                    HarnessTree::load_components(&request.base_harness.join(".agents"))
                        .unwrap_or_default()
                }
                ImproverRole::Selector => HarnessTree::default(),
                ImproverRole::Reflector => HarnessTree::default(),
            };
            let notes = if request.role == ImproverRole::Reflector {
                String::from(
                    "<edits>\ncomponent: guardrails.md\ncandidates: probe-0-inv\n\
                     change: kept the rule\nobserved: probe delivered\nverdict: helped\n</edits>\n\
                     <experience>\ncue: keep the rule\nlesson: keep the rule\n\
                     evidence: ref-0 and probe-0-inv delivered\n</experience>",
                )
            } else {
                String::from("kept -> guardrails.md -> probe delivered")
            };
            Ok(ImproverReply { tree, notes })
        })
    }
}

struct RecordingWriter {
    library: PathBuf,
    calls: Arc<Mutex<usize>>,
}

impl ReflectionWriter for RecordingWriter {
    fn write(
        &self,
        entry: &ExperienceEntry,
        harness: &HarnessTree,
    ) -> Result<PathBuf, HarnessError> {
        *self.calls.lock().expect("calls") += 1;
        save_entry_at(&self.library, entry, Some(harness)).map_err(HarnessError::State)
    }
}

fn workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join(".agents")).expect("agents");
    std::fs::write(workspace.join(".agents/guardrails.md"), "base rule\n").expect("rule");
    workspace
}

fn config() -> HarnessConfig {
    HarnessConfig { k: 1, resume: false, ..HarnessConfig::default() }
}

fn snapshot(directory: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    collect(directory, directory, &mut files);
    files.sort();
    files
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            files.push((path.strip_prefix(root).unwrap_or(&path).display().to_string(), content));
        }
    }
}

fn entry_count(library: &Path) -> usize {
    std::fs::read_dir(library.join("rust/entries"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

#[tokio::test]
async fn three_task_stream_learns_forward_without_leaking_backwards() {
    let root = tempfile::tempdir().expect("root");
    let workspace = workspace(root.path());
    let library = root.path().join("library");
    let writer = RecordingWriter { library: library.clone(), calls: Arc::new(Mutex::new(0)) };
    let channel = FakeAgentTransport { improves: Arc::new(Mutex::new(Vec::new())) };
    let tasks = ["a", "b", "c"]
        .into_iter()
        .map(|id| HarnessTask::new(id, format!("task {id}"), "rust", &workspace))
        .map(|task| task.with_library_root(&library))
        .collect::<Vec<_>>();
    let run_root = root.path().join("stream");
    let runs =
        run_stream(&channel, &tasks, &config(), &run_root, Some(&writer)).await.expect("stream");
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|run| run.result.is_ok()));

    let first = &runs[0].task_id;
    let second = &runs[1].task_id;
    let third = &runs[2].task_id;
    let context = |task: &str| run_root.join("tasks/rust").join(task).join("context/step-0-inv");
    assert!(!context(first).join("library").exists(), "the first task sees no prior entry");
    assert!(
        context(second).join(format!("library/rust/1-{first}/harness/guardrails.md")).is_file(),
        "the second task sees the first task's full harness tree"
    );
    assert!(
        context(third).join(format!("library/rust/1-{first}/harness/guardrails.md")).is_file(),
        "the third task sees the first entry"
    );
    assert!(
        context(third).join(format!("library/rust/2-{second}/harness/guardrails.md")).is_file(),
        "the third task sees the second entry"
    );
    assert!(
        !context(third).join(format!("library/rust/3-{third}")).exists(),
        "a task never sees its own entry"
    );
    assert_eq!(entry_count(&library), 3);
}

#[tokio::test]
async fn approval_outcomes_are_recorded_and_never_touch_active_agents() {
    let root = tempfile::tempdir().expect("root");
    let workspace = workspace(root.path());
    let before = snapshot(&workspace.join(".agents"));
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0usize));
    let writer = RecordingWriter { library: library.clone(), calls: calls.clone() };
    let channel = FakeAgentTransport { improves: Arc::new(Mutex::new(Vec::new())) };
    let task = HarnessTask::new("t1", "do work", "rust", &workspace).with_library_root(&library);

    let accepted = run_task(&channel, &task, &config(), &root.path().join("accept"), Some(&writer))
        .await
        .expect("accept");
    assert_eq!(accepted.reflection_write, ReflectionWrite::Written);
    assert_eq!(accepted.reflection_source, ReflectionSource::Invocation);
    assert_eq!(*calls.lock().expect("calls"), 1);

    let mut declined_config = config();
    declined_config.reflection_source = ReflectionSource::Flag;
    let declined = run_task(&channel, &task, &declined_config, &root.path().join("decline"), None)
        .await
        .expect("decline");
    assert_eq!(declined.reflection_write, ReflectionWrite::Declined);
    assert_eq!(declined.reflection_source, ReflectionSource::Flag);

    let mut disabled_config = config();
    disabled_config.reflect = false;
    let disabled =
        run_task(&channel, &task, &disabled_config, &root.path().join("disabled"), Some(&writer))
            .await
            .expect("disabled");
    assert_eq!(disabled.reflection_write, ReflectionWrite::Disabled);
    assert_eq!(disabled.reflection_source, ReflectionSource::None);

    assert_eq!(entry_count(&library), 1, "only the accepted run wrote an entry");
    assert_eq!(snapshot(&workspace.join(".agents")), before, "active .agents untouched");
}

#[test]
fn harness_dir_is_the_loader_anchor() {
    let root = tempfile::tempdir().expect("root");
    let library = root.path().join("library");
    let entry = ExperienceEntry {
        seq: 1,
        domain: String::from("rust"),
        task: String::from("t"),
        confidence: ee_agent_orchestrator::user_experience::ExperienceConfidence::Medium,
        edits: vec![ee_agent_orchestrator::user_experience::EditRow {
            component: String::from("guardrails.md"),
            candidates: Vec::new(),
            change: String::from("kept"),
            observed: String::from("probe delivered"),
            verdict: ee_agent_orchestrator::user_experience::EditVerdict::Helped,
        }],
        cue: Some(String::from("cue")),
        lesson: Some(String::from("lesson")),
        evidence: Some(String::from("evidence")),
        content: String::new(),
        scope: ee_agent_orchestrator::user_experience::ExperienceScope::Workspace,
        path: PathBuf::new(),
        display_path: String::new(),
    };
    let harness =
        HarnessTree::new(BTreeMap::from([(String::from("guardrails.md"), String::from("rule\n"))]));
    save_entry_at(&library, &entry, Some(&harness)).expect("entry");
    let registry =
        ee_agent_orchestrator::user_experience::ExperienceRegistry::discover_at(&library);
    let loaded = registry.iter().next().expect("entry");
    let harness_dir = loaded.harness_dir().expect("harness dir");
    assert_eq!(
        harness_dir,
        loaded.path.parent().expect("parent").join("harness"),
        "HARNESS_DIR is the production reader anchor"
    );
    assert!(harness_dir.join("guardrails.md").is_file());
}
