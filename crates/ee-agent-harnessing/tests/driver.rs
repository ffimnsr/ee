//! Hermetic driver, selection, resume, and stream tests.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ee_agent_harnessing::{
    Channel, ChannelFuture, HarnessConfig, HarnessError, HarnessTask, ImproverReply,
    ImproverRequest, ImproverRole, ReflectionSource, ReflectionWrite, ReflectionWriter, RunRecord,
    RunRequest, run_stream, run_task, save_entry_at,
};
use ee_agent_orchestrator::harness_reflection::ReflectionToolRecord;
use ee_agent_orchestrator::harness_validation::HarnessTree;
use ee_agent_orchestrator::rollout_view::{
    RolloutDiffFile, RolloutToolRecord, RolloutTurn, RolloutView,
};
use tokio::sync::Barrier;

type RunFn = Arc<dyn Fn(&RunRequest) -> Result<RunRecord, HarnessError> + Send + Sync>;
type ImproveFn = Arc<dyn Fn(&ImproverRequest) -> Result<ImproverReply, HarnessError> + Send + Sync>;

struct FnChannel {
    run: RunFn,
    improve: ImproveFn,
}

impl Channel for FnChannel {
    fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
        let run = self.run.clone();
        Box::pin(async move { run(&request) })
    }

    fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
        let improve = self.improve.clone();
        Box::pin(async move { improve(&request) })
    }
}

fn channel(run: RunFn, improve: ImproveFn) -> FnChannel {
    FnChannel { run, improve }
}

fn tree(files: &[(&str, &str)]) -> HarnessTree {
    HarnessTree::new(
        files
            .iter()
            .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn inv_tree() -> HarnessTree {
    tree(&[("guardrails.md", "improved inv rule\n")])
}

fn dom_tree() -> HarnessTree {
    tree(&[("guardrails.md", "base rule\n"), ("scripts/ok.sh", "#!/bin/sh\ntrue\n")])
}

fn composed_tree() -> HarnessTree {
    tree(&[("guardrails.md", "improved inv rule\n"), ("scripts/ok.sh", "#!/bin/sh\ntrue\n")])
}

fn reflection_notes() -> String {
    String::from(
        "<edits>\ncomponent: guardrails.md\ncandidates: probe-0-inv\n\
         change: added a rule\nobserved: probe delivered\nverdict: helped\n</edits>\n\
         <experience>\ncue: repeated rule\nlesson: keep the rule\n\
         evidence: ref-0 and probe-0-inv delivered different results\n</experience>",
    )
}

fn no_entry_notes() -> String {
    String::from("<edits></edits>\n<experience>\nNO_ENTRY\n</experience>")
}

fn record(label: &str, text: &str) -> RunRecord {
    RunRecord {
        label: label.to_string(),
        delivered: Some(ee_agent_harnessing::Deliverable {
            text: text.to_string(),
            assets: Vec::new(),
        }),
        rollout: RolloutView {
            session_id: format!("session-{label}"),
            turns: vec![RolloutTurn {
                turn: 1,
                stop_reason: Some(String::from("end_turn")),
                tools: vec![RolloutToolRecord {
                    turn: 1,
                    iteration: 1,
                    tool: String::from("write_file"),
                    success: true,
                    error_kind: None,
                    duration_ms: 2,
                }],
            }],
            diff: vec![RolloutDiffFile { path: String::from("out.txt"), change: "added" }],
            final_deliverable: Some(text.to_string()),
        },
        tool_records: vec![ReflectionToolRecord {
            turn: 1,
            iteration: 1,
            tool: String::from("write_file"),
            success: true,
            summary: String::from("wrote out.txt"),
        }],
        workspace_diff: Some(String::from("- added out.txt")),
        sessions: 1,
        rollouts: 1,
        usage_tokens: None,
    }
}

fn standard_reply(request: &ImproverRequest) -> ImproverReply {
    match request.role {
        ImproverRole::Inv => ImproverReply { tree: inv_tree(), notes: "kept".into() },
        ImproverRole::Dom => ImproverReply { tree: dom_tree(), notes: "kept".into() },
        ImproverRole::Composer => ImproverReply {
            tree: composed_tree(),
            notes: "kept -> guardrails.md -> probe-0-inv delivered".into(),
        },
        ImproverRole::Selector => panic!("k=1 must bypass the selector session"),
        ImproverRole::Reflector => {
            ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() }
        }
    }
}

fn standard_improver(calls: Arc<Mutex<Vec<ImproverRequest>>>) -> ImproveFn {
    Arc::new(move |request| {
        calls.lock().expect("calls").push(request.clone());
        Ok(standard_reply(request))
    })
}

fn task(workspace: &Path) -> HarnessTask {
    HarnessTask::new("t1", "do the work", "rust", workspace)
}

fn config(k: usize) -> HarnessConfig {
    HarnessConfig { k, ..HarnessConfig::default() }
}

struct RecordingWriter {
    library: PathBuf,
    calls: Arc<Mutex<usize>>,
}

impl ReflectionWriter for RecordingWriter {
    fn write(
        &self,
        entry: &ee_agent_harnessing::ExperienceEntry,
        harness: &HarnessTree,
    ) -> Result<PathBuf, HarnessError> {
        *self.calls.lock().expect("calls") += 1;
        save_entry_at(&self.library, entry, Some(harness)).map_err(HarnessError::State)
    }
}

fn prepare_workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join(".agents")).expect("agents");
    std::fs::write(workspace.join(".agents/guardrails.md"), "base rule\n").expect("harness");
    workspace
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
            let name = path.strip_prefix(root).unwrap_or(&path).display().to_string();
            files.push((name, content));
        }
    }
}

fn selector_choice(context: &Path, needle: &str) -> Result<String, HarnessError> {
    let selector = context.join("selector");
    let entries = std::fs::read_dir(&selector)
        .map_err(|error| HarnessError::State(format!("no selector directory: {error}")))?;
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    names.sort();
    for name in names {
        if let Ok(text) = std::fs::read_to_string(selector.join(&name))
            && text.contains(needle)
        {
            return Ok(format!(
                "```choice\n{}\n```\n<why>\ndelivery\nrunner-up cost\n</why>",
                name.trim_end_matches(".md")
            ));
        }
    }
    Err(HarnessError::State(format!("no selector candidate contains {needle:?}")))
}

#[tokio::test]
async fn inv_dom_and_probe_phases_run_concurrently() {
    // Both phases are pairs of independent stages. A serial driver can never
    // satisfy these barriers, so the timeout is the regression signal.
    struct PhaseBarrierChannel {
        improve: Arc<Barrier>,
        probe: Arc<Barrier>,
    }

    impl Channel for PhaseBarrierChannel {
        fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
            let barrier = self.probe.clone();
            Box::pin(async move {
                let response = record(&request.label, "delivered");
                if request.label.starts_with("probe-") {
                    barrier.wait().await;
                }
                Ok(response)
            })
        }

        fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
            let barrier = self.improve.clone();
            Box::pin(async move {
                let reply = standard_reply(&request);
                if matches!(request.role, ImproverRole::Inv | ImproverRole::Dom) {
                    barrier.wait().await;
                }
                Ok(reply)
            })
        }
    }

    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let channel = PhaseBarrierChannel {
        improve: Arc::new(Barrier::new(2)),
        probe: Arc::new(Barrier::new(2)),
    };
    let mut settings = config(1);
    settings.reflect = false;
    settings.max_workers = 2;
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_task(&channel, &task(&workspace), &settings, &root.path().join("run"), None),
    )
    .await
    .expect("phase pairs must complete concurrently")
    .expect("run");
    assert_eq!(run.ledger.rollouts, 4, "R = 4k");
    assert_eq!(run.ledger.sessions, 3, "P = 3k");
}

#[tokio::test]
async fn full_loop_scopes_improver_writes_to_the_run_root() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let before = snapshot(&workspace.join(".agents"));
    let run_root = root.path().join("run");
    let runs = Arc::new(Mutex::new(Vec::<RunRequest>::new()));
    let improves = Arc::new(Mutex::new(Vec::<ImproverRequest>::new()));
    let run_calls = runs.clone();
    let channel = channel(
        Arc::new(move |request| {
            run_calls.lock().expect("runs").push(request.clone());
            Ok(record(&request.label, "complete deliverable"))
        }),
        standard_improver(improves.clone()),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(1), &run_root, None).await.expect("run");
    assert_eq!(run.selection.chosen.as_deref(), Some("composed-0"));
    assert!(!run.selection.fallback);
    assert_eq!(run.ledger.rollouts, 4, "R = 4k");
    assert_eq!(run.ledger.sessions, 3, "P = 3k");
    assert_eq!(run.ledger.reflection_sessions, 1);
    assert!(run.reflection.is_some());
    assert!(run.reflected_path.is_none(), "no writer means no write");
    assert!(run.adopted_path.as_ref().is_some_and(|path| path.join(".agents").is_dir()));
    assert_eq!(runs.lock().expect("runs").len(), 4);
    for request in runs.lock().expect("runs").iter() {
        assert!(request.harness_root.starts_with(&run_root), "probe mounts a candidate dir");
        assert!(request.scratch_root.starts_with(&run_root));
        assert!(!request.harness_root.starts_with(workspace.join(".agents")));
    }
    assert_eq!(improves.lock().expect("improves").len(), 4, "INV + DOM + composer + reflector");
    for request in improves.lock().expect("improves").iter() {
        assert!(request.candidate_dir.starts_with(run_root.join("candidates")));
        assert!(request.base_harness.starts_with(&run_root));
        assert!(request.context_dir.starts_with(run_root.join("context")));
    }
    assert_eq!(snapshot(&workspace.join(".agents")), before, "workspace harness untouched");
}

#[tokio::test]
async fn invalid_candidate_retries_then_inherits_the_current_harness() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let attempts = Arc::new(Mutex::new(HashMap::<ImproverRole, usize>::new()));
    let attempt_counts = attempts.clone();
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        Arc::new(move |request| {
            let mut counts = attempt_counts.lock().expect("attempts");
            let attempt = counts.entry(request.role).or_insert(0);
            *attempt += 1;
            match request.role {
                ImproverRole::Inv => Ok(ImproverReply {
                    tree: tree(&[("guardrails.md", "base rule\n"), ("scripts/bad.txt", "nope\n")]),
                    notes: String::new(),
                }),
                ImproverRole::Dom => Ok(ImproverReply { tree: dom_tree(), notes: String::new() }),
                ImproverRole::Composer => {
                    Ok(ImproverReply { tree: composed_tree(), notes: "kept".into() })
                }
                ImproverRole::Reflector => {
                    Ok(ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() })
                }
                ImproverRole::Selector => unreachable!("k=1"),
            }
        }),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(1), &run_root, None).await.expect("run");
    assert_eq!(
        attempts.lock().expect("attempts").get(&ImproverRole::Inv).copied(),
        Some(2),
        "one retry"
    );
    assert_eq!(run.ledger.retry_sessions, 1);
    assert_eq!(run.ledger.sessions, 3);
    assert_eq!(run.ledger.rollouts, 4);
    let seeded = run_root.join("candidates/step-0-inv-retry/.agents/scripts/bad.txt");
    assert!(seeded.is_file(), "the retry candidate dir carries the partial tree");
    let observations =
        std::fs::read_to_string(run_root.join("context/step-0-inv-retry/observations.md"))
            .expect("observations");
    assert!(observations.contains("not allowed"), "{observations}");
    let inherited =
        std::fs::read_to_string(run_root.join("harness/step-0/probe-inv/.agents/guardrails.md"))
            .expect("inherited probe");
    assert_eq!(inherited, "base rule\n", "probe mounts the inherited base tree");
    assert!(!run_root.join("harness/step-0/probe-inv/.agents/scripts/bad.txt").exists());
}

#[tokio::test]
async fn empty_candidate_is_rejected_and_inherits() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        Arc::new(|request| {
            let reply = match request.role {
                ImproverRole::Inv => {
                    ImproverReply { tree: HarnessTree::default(), notes: String::new() }
                }
                ImproverRole::Dom => ImproverReply { tree: dom_tree(), notes: String::new() },
                ImproverRole::Composer => {
                    ImproverReply { tree: composed_tree(), notes: "kept".into() }
                }
                ImproverRole::Reflector => {
                    ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() }
                }
                ImproverRole::Selector => unreachable!("k=1"),
            };
            Ok(reply)
        }),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(1), &run_root, None).await.expect("run");
    assert_eq!(run.ledger.retry_sessions, 1);
    let observations =
        std::fs::read_to_string(run_root.join("context/step-0-inv-retry/observations.md"))
            .expect("observations");
    assert!(observations.contains("edit the files, do not delete them all"), "{observations}");
}

fn selector_channel(needle: &'static str, fallback: bool) -> FnChannel {
    channel(
        Arc::new(|request| {
            let tree = HarnessTree::load_components(&request.harness_root.join(".agents"))
                .unwrap_or_default();
            let text = if tree
                .files
                .get("guardrails.md")
                .is_some_and(|content| content.contains("improved inv rule"))
            {
                "CANDIDATE deliverable"
            } else {
                "BASE deliverable"
            };
            Ok(record(&request.label, text))
        }),
        Arc::new(move |request| {
            let reply = match request.role {
                ImproverRole::Inv => ImproverReply { tree: inv_tree(), notes: String::new() },
                ImproverRole::Dom => ImproverReply { tree: dom_tree(), notes: String::new() },
                ImproverRole::Composer => ImproverReply {
                    tree: composed_tree(),
                    notes: "kept -> guardrails.md -> probe-0-inv delivered".into(),
                },
                ImproverRole::Selector => {
                    if fallback {
                        return Ok(ImproverReply {
                            tree: HarnessTree::default(),
                            notes: "not a selector reply".into(),
                        });
                    }
                    let reply = selector_choice(&request.context_dir, needle)?;
                    return Ok(ImproverReply { tree: HarnessTree::default(), notes: reply });
                }
                ImproverRole::Reflector => {
                    ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() }
                }
            };
            Ok(reply)
        }),
    )
}

#[tokio::test]
async fn selector_can_pick_the_base_deliverable() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let channel = selector_channel("BASE deliverable", false);
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(2), &run_root, None).await.expect("run");
    assert_eq!(run.selection.chosen.as_deref(), Some("ref-0"));
    assert!(!run.selection.fallback);
    assert_eq!(run.ledger.selection_sessions, 1);
    let adopted = HarnessTree::load_components(&run_root.join("adopted/.agents")).expect("adopted");
    assert_eq!(adopted.files.get("guardrails.md").map(String::as_str), Some("base rule\n"));
}

#[tokio::test]
async fn selector_parse_failure_falls_back_to_the_base() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let channel = selector_channel("unused", true);
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(2), &run_root, None).await.expect("run");
    assert!(run.selection.fallback);
    assert_eq!(run.selection.chosen.as_deref(), Some("ref-0"));
    assert!(run.selection.why.iter().any(|line| line.contains("neutral fallback")));
}

#[tokio::test]
async fn improver_channel_failure_inherits_the_base_and_keeps_the_identity() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        Arc::new(|request| match request.role {
            ImproverRole::Inv => Err(HarnessError::Channel(String::from("editor timed out"))),
            ImproverRole::Dom => {
                Ok(ImproverReply { tree: dom_tree(), notes: String::from("kept") })
            }
            ImproverRole::Composer => Ok(ImproverReply {
                tree: composed_tree(),
                notes: String::from("kept -> guardrails.md -> probe-0-inv delivered"),
            }),
            ImproverRole::Selector => panic!("k=1 must bypass the selector session"),
            ImproverRole::Reflector => {
                Ok(ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() })
            }
        }),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(1), &run_root, None).await.expect("run");
    assert_eq!(run.ledger.sessions, 3, "the failed editor still consumes its identity slot");
    assert_eq!(run.ledger.rollouts, 4);
    assert_eq!(run.selection.chosen.as_deref(), Some("composed-0"));
}

#[tokio::test]
async fn selector_and_reflector_session_errors_do_not_fail_the_run() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delegated deliverable"))),
        Arc::new(|request| match request.role {
            ImproverRole::Inv => {
                Ok(ImproverReply { tree: inv_tree(), notes: String::from("kept") })
            }
            ImproverRole::Dom => {
                Ok(ImproverReply { tree: dom_tree(), notes: String::from("kept") })
            }
            ImproverRole::Composer => Ok(ImproverReply {
                tree: composed_tree(),
                notes: String::from("kept -> guardrails.md -> probe-0-inv delivered"),
            }),
            ImproverRole::Selector => {
                Err(HarnessError::Channel(String::from("selector timed out")))
            }
            ImproverRole::Reflector => {
                Err(HarnessError::Channel(String::from("reflector timed out")))
            }
        }),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(2), &run_root, None).await.expect("run");
    assert!(run.selection.fallback);
    assert_eq!(run.selection.chosen.as_deref(), Some("ref-0"));
    assert_eq!(run.ledger.selection_sessions, 1);
    assert_eq!(run.ledger.reflection_sessions, 1);
    assert!(run.reflection.is_none());
    assert!(run.reflected_path.is_none());
}

#[tokio::test]
async fn base_failure_short_circuits_with_a_recorded_state() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let improves = Arc::new(Mutex::new(0usize));
    let improve_calls = improves.clone();
    let channel = channel(
        Arc::new(|_| Err(HarnessError::Channel(String::from("provider down")))),
        Arc::new(move |_| {
            *improve_calls.lock().expect("calls") += 1;
            Ok(ImproverReply { tree: HarnessTree::default(), notes: String::new() })
        }),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(2), &run_root, None).await.expect("run");
    assert!(run.ledger.base_failed);
    assert_eq!(run.ledger.dead_rollouts.len(), 2);
    assert_eq!(run.selection.chosen, None);
    assert_eq!(*improves.lock().expect("calls"), 0, "no improver after base failure");
    let summary = std::fs::read_to_string(run_root.join("run-summary.json")).expect("summary");
    assert!(summary.contains("base-failed"), "{summary}");
}

#[tokio::test]
async fn dead_probe_is_recorded_and_the_run_continues() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let channel = channel(
        Arc::new(|request| {
            if request.harness_root.display().to_string().contains("probe-dom") {
                return Err(HarnessError::Channel(String::from("dom probe timed out")));
            }
            Ok(record(&request.label, "delivered"))
        }),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let task = task(&workspace);
    let run = run_task(&channel, &task, &config(1), &run_root, None).await.expect("run");
    assert_eq!(run.ledger.dead_rollouts.len(), 1);
    assert_eq!(run.ledger.dead_rollouts[0].label, "probe-0-dom");
    assert_eq!(run.ledger.rollouts, 4, "dead rollout still counts in the identity");
    assert_eq!(run.selection.chosen.as_deref(), Some("composed-0"));
}

#[tokio::test]
async fn resume_reuses_the_completed_summary_and_improvers_are_cached() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let run_root = root.path().join("run");
    let runs = Arc::new(Mutex::new(0usize));
    let improves = Arc::new(Mutex::new(0usize));
    let run_calls = runs.clone();
    let improve_calls = improves.clone();
    let channel = channel(
        Arc::new(move |request| {
            *run_calls.lock().expect("runs") += 1;
            Ok(record(&request.label, "delivered"))
        }),
        Arc::new(move |request| {
            *improve_calls.lock().expect("improves") += 1;
            let reply = match request.role {
                ImproverRole::Inv => ImproverReply { tree: inv_tree(), notes: String::new() },
                ImproverRole::Dom => ImproverReply { tree: dom_tree(), notes: String::new() },
                ImproverRole::Composer => {
                    ImproverReply { tree: composed_tree(), notes: String::from("kept") }
                }
                ImproverRole::Reflector => {
                    ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() }
                }
                ImproverRole::Selector => unreachable!("k=1"),
            };
            Ok(reply)
        }),
    );
    let task = task(&workspace);
    let mut settings = config(1);
    settings.reflect = false;
    let first = run_task(&channel, &task, &settings, &run_root, None).await.expect("first");
    assert!(!first.resumed);
    assert_eq!(*runs.lock().expect("runs"), 4);
    assert_eq!(*improves.lock().expect("improves"), 3);
    let second = run_task(&channel, &task, &settings, &run_root, None).await.expect("second");
    assert!(second.resumed);
    assert_eq!(*runs.lock().expect("runs"), 4, "no new rollouts on resume");
    assert_eq!(*improves.lock().expect("improves"), 3, "no new sessions on resume");
    settings.resume = false;
    run_task(&channel, &task, &settings, &run_root, None).await.expect("third");
    assert_eq!(*runs.lock().expect("runs"), 8, "rollouts are never cached");
    assert_eq!(*improves.lock().expect("improves"), 3, "all improver sessions hit the cache");
    let changed = HarnessTask::new("t1", "a different instruction", "rust", &workspace);
    run_task(&channel, &changed, &settings, &run_root, None).await.expect("changed task");
    assert_eq!(*improves.lock().expect("improves"), 6, "changed inputs miss the cache");
}

fn stream_workspace(root: &Path, name: &str) -> PathBuf {
    let workspace = root.join(name);
    std::fs::create_dir_all(workspace.join(".agents")).expect("agents");
    std::fs::write(workspace.join(".agents/guardrails.md"), "base rule\n").expect("harness");
    workspace
}

#[tokio::test]
async fn stream_keeps_a_frozen_order_and_rejects_stale_inputs() {
    let root = tempfile::tempdir().expect("root");
    let workspace = stream_workspace(root.path(), "workspace");
    let improve_calls = Arc::new(Mutex::new(Vec::<ImproverRequest>::new()));
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(improve_calls),
    );
    let mut tasks = Vec::new();
    for (domain, id) in [("alpha", "a1"), ("alpha", "a2"), ("beta", "b1"), ("beta", "b2")] {
        let mut task = HarnessTask::new(id, "do work", domain, &workspace);
        task.source_roots = Vec::new();
        tasks.push(task);
    }
    let mut settings = config(1);
    settings.reflect = false;
    settings.max_workers = 2;
    let stream_root = root.path().join("stream");
    let first = run_stream(&channel, &tasks, &settings, &stream_root, None).await.expect("stream");
    let first_order: Vec<(String, String)> =
        first.iter().map(|run| (run.domain.clone(), run.task_id.clone())).collect();
    assert_eq!(first.len(), 4);
    assert!(first.iter().all(|run| run.result.as_ref().is_ok_and(|run| !run.resumed)));
    assert!(ee_agent_harnessing::stream_order_path(&stream_root).is_file());
    let mut reversed = tasks.clone();
    reversed.reverse();
    let second =
        run_stream(&channel, &reversed, &settings, &stream_root, None).await.expect("stream");
    let second_order: Vec<(String, String)> =
        second.iter().map(|run| (run.domain.clone(), run.task_id.clone())).collect();
    assert_eq!(first_order, second_order, "frozen order overrides input order");
    assert!(second.iter().all(|run| run.result.as_ref().is_ok_and(|run| run.resumed)));

    let mut stale = reversed;
    stale.push(HarnessTask::new("a3", "do work", "alpha", &workspace));
    let error =
        run_stream(&channel, &stale, &settings, &stream_root, None).await.expect_err("stale order");
    assert!(error.to_string().contains("stale frozen stream order"), "{error}");
}

#[tokio::test]
async fn invocation_consent_writes_the_library_entry_and_leaves_active_agents_unchanged() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let before = snapshot(&workspace.join(".agents"));
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0usize));
    let writer = RecordingWriter { library: library.clone(), calls: calls.clone() };
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let task = task(&workspace).with_library_root(&library);
    let mut settings = config(1);
    settings.resume = false;
    let run = run_task(&channel, &task, &settings, &root.path().join("run"), Some(&writer))
        .await
        .expect("run");
    assert_eq!(run.reflection_write, ReflectionWrite::Written);
    assert_eq!(run.reflection_source, ReflectionSource::Invocation);
    assert_eq!(run.library_root, library);
    assert_eq!(*calls.lock().expect("calls"), 1);
    let entry = run.reflected_path.as_ref().expect("entry path");
    assert!(entry.join("EXPERIENCE.md").is_file());
    assert!(entry.join("harness/guardrails.md").is_file());
    assert_eq!(snapshot(&workspace.join(".agents")), before, "active .agents untouched");
    let summary =
        std::fs::read_to_string(root.path().join("run/run-summary.json")).expect("summary");
    assert!(summary.contains("\"reflection_write\": \"written\""), "{summary}");
    assert!(summary.contains("\"reflection_source\": \"invocation\""), "{summary}");
    assert!(summary.contains("library"), "{summary}");
}

#[tokio::test]
async fn progress_writes_status_file_until_completion() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let mut settings = config(1);
    settings.progress = true;
    let run_root = root.path().join("run");
    run_task(&channel, &task(&workspace), &settings, &run_root, None).await.expect("run");
    let text = std::fs::read_to_string(run_root.join("status.json")).expect("status");
    let status: ee_agent_harnessing::runstate::HarnessStatus =
        serde_json::from_str(&text).expect("status parses");
    assert_eq!(status.stage, "complete");
    assert_eq!(status.state, "completed");
    assert_eq!(status.rollouts, 4);
    assert!(!status.base_failed);
}

#[tokio::test]
async fn no_change_reflection_is_no_entry_not_failure() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0));
    let writer = RecordingWriter { library: library.clone(), calls: calls.clone() };
    let notes = no_entry_notes();
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        Arc::new(move |request| {
            if request.role == ImproverRole::Reflector {
                return Ok(ImproverReply { tree: HarnessTree::default(), notes: notes.clone() });
            }
            Ok(standard_reply(request))
        }),
    );
    let run =
        run_task(&channel, &task(&workspace), &config(1), &root.path().join("run"), Some(&writer))
            .await
            .expect("run");
    assert_eq!(run.reflection_write, ReflectionWrite::NoEntry);
    assert!(run.reflected_path.is_none());
    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!library.exists(), "NO_ENTRY must not create a library entry");
}

#[tokio::test]
async fn no_reflect_skips_the_stage_and_writes_nothing() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0usize));
    let writer = RecordingWriter { library: library.clone(), calls: calls.clone() };
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let task = task(&workspace);
    let mut settings = config(1);
    settings.reflect = false;
    settings.resume = false;
    let run = run_task(&channel, &task, &settings, &root.path().join("run"), Some(&writer))
        .await
        .expect("run");
    assert_eq!(run.reflection_write, ReflectionWrite::Disabled);
    assert_eq!(run.reflection_source, ReflectionSource::None);
    assert_eq!(run.ledger.reflection_sessions, 0);
    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!library.exists());
}

#[tokio::test]
async fn declined_reflection_runs_but_writes_nothing() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let library = root.path().join("library");
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let task = task(&workspace).with_library_root(&library);
    let mut settings = config(1);
    settings.resume = false;
    settings.reflection_source = ReflectionSource::Flag;
    let run =
        run_task(&channel, &task, &settings, &root.path().join("run"), None).await.expect("run");
    assert_eq!(run.reflection_write, ReflectionWrite::Declined);
    assert_eq!(run.reflection_source, ReflectionSource::Flag);
    assert_eq!(run.ledger.reflection_sessions, 1);
    assert!(run.reflection.is_some());
    assert!(run.reflected_path.is_none());
    assert!(!library.exists());
}

#[tokio::test]
async fn second_task_reads_the_first_tasks_full_library_tree() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0usize));
    let writer = RecordingWriter { library: library.clone(), calls };
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let first = task(&workspace).with_library_root(&library);
    let mut settings = config(1);
    settings.resume = false;
    run_task(&channel, &first, &settings, &root.path().join("run-1"), Some(&writer))
        .await
        .expect("first");

    let second =
        HarnessTask::new("t2", "do the work again", "rust", &workspace).with_library_root(&library);
    let second_run = run_task(&channel, &second, &settings, &root.path().join("run-2"), None)
        .await
        .expect("second");
    assert_eq!(second_run.library_root, library);
    let copied =
        root.path().join("run-2/context/step-0-inv/library/rust/1-t1/harness/guardrails.md");
    assert!(copied.is_file(), "prior harness tree copied into the next context");
    assert_eq!(
        std::fs::read_to_string(copied).expect("copied rule").trim_end(),
        "improved inv rule"
    );
    assert!(root.path().join("run-2/context/step-0-inv/library/rust/1-t1/EXPERIENCE.md").is_file());
    assert!(root.path().join("run-2/context/step-0-inv/library/INDEX.md").is_file());
}

#[tokio::test]
async fn resume_reuses_the_written_entry_without_duplicating_it() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0usize));
    let writer = RecordingWriter { library: library.clone(), calls };
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let task = task(&workspace).with_library_root(&library);
    let run_root = root.path().join("run");
    let first =
        run_task(&channel, &task, &config(1), &run_root, Some(&writer)).await.expect("first");
    let reflected = first.reflected_path.clone().expect("entry written");
    let second =
        run_task(&channel, &task, &config(1), &run_root, Some(&writer)).await.expect("second");
    assert!(second.resumed);
    assert_eq!(second.reflected_path.as_deref(), Some(reflected.as_path()));
    assert_eq!(second.reflection_write, ReflectionWrite::Written);
    let entries = std::fs::read_dir(library.join("rust/entries")).expect("entries");
    assert_eq!(entries.count(), 1, "resume must not duplicate the library entry");
}

#[tokio::test]
async fn stream_reports_failed_task_without_aborting_completed_domains() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let channel = channel(
        Arc::new(|request| {
            if request.task.domain == "alpha" {
                Err(HarnessError::Channel(String::from("alpha provider down")))
            } else {
                Ok(record(&request.label, "delivered"))
            }
        }),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let tasks = vec![
        HarnessTask::new("a1", "alpha task", "alpha", &workspace),
        HarnessTask::new("b1", "beta task", "beta", &workspace),
    ];
    let mut settings = config(1);
    settings.reflect = false;
    settings.resume = false;
    let runs = run_stream(&channel, &tasks, &settings, &root.path().join("stream"), None)
        .await
        .expect("stream");
    assert_eq!(runs.len(), 2);
    let alpha = runs.iter().find(|run| run.domain == "alpha").expect("alpha");
    let beta = runs.iter().find(|run| run.domain == "beta").expect("beta");
    let alpha_run = alpha.result.as_ref().expect("alpha returns a recorded failure");
    assert!(alpha_run.ledger.base_failed, "alpha has no delivered base run");
    let beta_run = beta.result.as_ref().expect("beta completed");
    assert!(beta_run.selection.chosen.is_some(), "beta delivered despite alpha failing");
}

#[tokio::test]
async fn stream_runs_domains_concurrently_and_keeps_one_domain_sequential() {
    #[derive(Clone)]
    struct BarrierChannel {
        barrier: Arc<Barrier>,
        seen: Arc<Mutex<BTreeSet<String>>>,
        active: Arc<Mutex<BTreeSet<String>>>,
        overlap: Arc<AtomicBool>,
    }

    impl Channel for BarrierChannel {
        fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
            let this = self.clone();
            let label = request.label.clone();
            let domain = request.task.domain.clone();
            Box::pin(async move {
                // Track only base rollouts: they mark the top-level task boundary.
                // Probe rollouts within one task legitimately run as a pair.
                if label.starts_with("ref-") {
                    let first = this.seen.lock().expect("seen").insert(domain.clone());
                    if !this.active.lock().expect("active").insert(domain.clone()) {
                        this.overlap.store(true, Ordering::Relaxed);
                    }
                    if first {
                        this.barrier.wait().await;
                    }
                    tokio::task::yield_now().await;
                    this.active.lock().expect("active").remove(&domain);
                }
                Ok(record(&label, "delivered"))
            })
        }

        fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
            Box::pin(async move {
                Ok(ImproverReply {
                    tree: HarnessTree::load_components(&request.base_harness.join(".agents"))
                        .unwrap_or_default(),
                    notes: String::from("kept -> base harness -> probe delivered"),
                })
            })
        }
    }

    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let channel = BarrierChannel {
        barrier: Arc::new(Barrier::new(2)),
        seen: Arc::new(Mutex::new(BTreeSet::new())),
        active: Arc::new(Mutex::new(BTreeSet::new())),
        overlap: Arc::new(AtomicBool::new(false)),
    };
    let tasks = vec![
        HarnessTask::new("a1", "alpha one", "alpha", &workspace),
        HarnessTask::new("a2", "alpha two", "alpha", &workspace),
        HarnessTask::new("b1", "beta one", "beta", &workspace),
    ];
    let mut settings = config(1);
    settings.reflect = false;
    settings.resume = false;
    settings.max_workers = 2;
    let runs = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_stream(&channel, &tasks, &settings, &root.path().join("stream"), None),
    )
    .await
    .expect("domains must reach the barrier concurrently")
    .expect("stream");
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|run| run.result.is_ok()));
    assert!(!channel.overlap.load(Ordering::Relaxed), "one domain ran two tasks at once");
}

#[tokio::test]
async fn stream_retention_exposes_prior_same_domain_and_hides_own_entries() {
    let root = tempfile::tempdir().expect("root");
    let workspace = prepare_workspace(root.path());
    let library = root.path().join("library");
    let calls = Arc::new(Mutex::new(0usize));
    let writer = RecordingWriter { library: library.clone(), calls };
    let channel = channel(
        Arc::new(|request| Ok(record(&request.label, "delivered"))),
        standard_improver(Arc::new(Mutex::new(Vec::new()))),
    );
    let tasks = vec![
        HarnessTask::new("a", "first task", "rust", &workspace).with_library_root(&library),
        HarnessTask::new("b", "second task", "rust", &workspace).with_library_root(&library),
    ];
    let mut settings = config(1);
    settings.max_workers = 2;
    settings.resume = false;
    let stream_root = root.path().join("stream");
    let runs =
        run_stream(&channel, &tasks, &settings, &stream_root, Some(&writer)).await.expect("stream");
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.result.is_ok()));
    let first_id = runs[0].task_id.clone();
    let second_id = runs[1].task_id.clone();
    assert!(
        !stream_root.join("tasks/rust").join(&first_id).join("context/step-0-inv/library").exists(),
        "the first task has no prior entries"
    );
    let b_copy = stream_root
        .join("tasks/rust")
        .join(&second_id)
        .join(format!("context/step-0-inv/library/rust/1-{first_id}/harness/guardrails.md"));
    assert!(b_copy.is_file(), "the second task sees the first task's full harness tree");

    let rerun_root = root.path().join("rerun");
    let rerun =
        run_stream(&channel, &tasks[..1], &settings, &rerun_root, None).await.expect("rerun");
    assert!(rerun[0].result.is_ok());
    let rerun_library = rerun_root.join("tasks/rust/a/context/step-0-inv/library");
    if let Ok(entries) = std::fs::read_dir(rerun_library.join("rust")) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.ends_with("-a"), "own prior entry {name} is hidden");
        }
    }
}
