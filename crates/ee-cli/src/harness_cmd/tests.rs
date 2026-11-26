use super::*;
use ee_agent_harnessing::save_entry_at;

#[tokio::test]
async fn host_channel_binds_a_fresh_host_event_tap() {
    let host = RemoteHost::deny_all(ee_agent_remote::RemoteHostConfig::default());
    let channel = bind_host_channel(&host, String::from("agent"), Duration::from_secs(1));
    assert!(channel.is_ok(), "fresh host must expose the rollout stream");
    assert!(host.take_events().is_none(), "the pump owns the receiver");
}

// Regression: the harness used to start the host pump before claiming the
// stream, so `take_events` returned `None` and every `/harness` invocation
// failed with "agent host did not expose its event stream".
#[tokio::test]
async fn host_channel_rejects_a_host_whose_pump_already_owns_events() {
    let host = RemoteHost::deny_all(ee_agent_remote::RemoteHostConfig::default());
    host.start().expect("pump starts");
    let error = bind_host_channel(&host, String::from("agent"), Duration::from_secs(1))
        .err()
        .expect("already-started host cannot serve rollouts");
    assert!(error.to_string().contains("cannot watch rollouts"), "error: {error}");
}
use ee_agent_orchestrator::user_experience::{
    EditRow, EditVerdict, ExperienceConfidence, ExperienceScope,
};

#[test]
fn delivery_combines_final_text_and_added_files() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("out.txt"), "done\n").expect("asset");
    let added = vec![PathBuf::from("out.txt")];
    let deliverable = delivery("wrote it", &added, root.path()).expect("delivered");
    assert_eq!(deliverable.text, "wrote it");
    assert_eq!(deliverable.assets.len(), 1);
    assert_eq!(deliverable.assets[0].name, "out.txt");
    assert_eq!(deliverable.assets[0].bytes, 5);
    assert_eq!(deliverable.assets[0].content.as_deref(), Some("done\n"));
    assert!(delivery("", &[], root.path()).is_none());
    assert!(delivery("", &added, root.path()).is_some());
}

#[test]
fn task_id_and_domain_are_stable() {
    let workspace = Path::new("/work/My Project");
    assert_eq!(derive_domain(workspace), "my-project");
    let first = task_id("my-project", "do the task");
    assert_eq!(first, task_id("my-project", "do the task"));
    assert_ne!(first, task_id("my-project", "a different task"));
    assert!(first.starts_with("cli-my-project-"));
}

#[test]
fn rollout_workspace_mounts_the_candidate_and_copies_sources() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).expect("workspace");
    std::fs::write(workspace.join("src/lib.rs"), "pub fn one() {}\n").expect("source");
    let harness = root.path().join("harness");
    std::fs::create_dir_all(harness.join(".agents")).expect("harness");
    std::fs::write(harness.join(".agents/guardrails.md"), "candidate rule\n").expect("rule");
    let request = RunRequest {
        label: String::from("ref-0"),
        task: HarnessTask::new("t", "do work", "rust", &workspace),
        harness_root: harness,
        scratch_root: root.path().join("scratch"),
    };
    let prepared = prepare_rollout_workspace(&request).expect("prepared");
    assert_eq!(
        std::fs::read_to_string(prepared.join("src/lib.rs")).expect("source"),
        "pub fn one() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(prepared.join(".agents/guardrails.md")).expect("rule"),
        "candidate rule\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".agents/guardrails.md")).unwrap_or_default(),
        "",
        "the task workspace harness is never mutated"
    );
}

fn sample_entry() -> ExperienceEntry {
    ExperienceEntry {
        seq: 1,
        domain: String::from("rust"),
        task: String::from("t"),
        confidence: ExperienceConfidence::Medium,
        edits: vec![EditRow {
            component: String::from("guardrails.md"),
            candidates: vec![String::from("probe-0-inv")],
            change: String::from("added a rule"),
            observed: String::from("probe delivered"),
            verdict: EditVerdict::Helped,
        }],
        cue: Some(String::from("cue")),
        lesson: Some(String::from("lesson")),
        evidence: Some(String::from("evidence")),
        content: String::new(),
        scope: ExperienceScope::Workspace,
        path: PathBuf::new(),
        display_path: String::new(),
    }
}

#[test]
fn cli_writer_writes_to_the_shared_library_root_without_prompting() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".agents")).expect("workspace");
    let scratch = root.path().join("library");
    let location = LibraryLocation::resolve(LibraryLocation::workspace(&workspace), Some(&scratch));
    let task = HarnessTask::new("t", "p", "rust", &workspace).with_library_root(location.root());
    let writer = CliWriter { library_root: location.root().to_path_buf() };
    assert_eq!(task.library_root(), writer.library_root, "reader root == writer root");
    let harness =
        HarnessTree::new(BTreeMap::from([(String::from("guardrails.md"), String::from("rule\n"))]));
    let path = writer.write(&sample_entry(), &harness).expect("write");
    assert!(path.join("EXPERIENCE.md").is_file());
    assert!(path.join("harness/guardrails.md").is_file());
    assert!(
        !workspace.join(".agents/experience").exists(),
        "a scratch library root never contaminates the workspace library"
    );
    assert!(save_entry_at(location.root(), &sample_entry(), Some(&harness)).is_ok());
}
