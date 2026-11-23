//! Hermetic driver, selection, resume, and stream tests.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ee_agent_harnessing::{
    Channel, ChannelFuture, HarnessConfig, HarnessError, HarnessTask, ImproverReply,
    ImproverRequest, ImproverRole, RunRecord, RunRequest, run_stream, run_task,
};
use ee_agent_orchestrator::harness_reflection::ReflectionToolRecord;
use ee_agent_orchestrator::harness_validation::HarnessTree;
use ee_agent_orchestrator::rollout_view::{
    RolloutDiffFile, RolloutToolRecord, RolloutTurn, RolloutView,
};

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

fn standard_improver(calls: Arc<Mutex<Vec<ImproverRequest>>>) -> ImproveFn {
    Arc::new(move |request| {
        calls.lock().expect("calls").push(request.clone());
        let reply = match request.role {
            ImproverRole::Inv => ImproverReply { tree: inv_tree(), notes: "kept".into() },
            ImproverRole::Dom => ImproverReply { tree: dom_tree(), notes: "kept".into() },
            ImproverRole::Composer => ImproverReply {
                tree: composed_tree(),
                notes: "kept -> guardrails.md -> probe-0-inv delivered".into(),
            },
            ImproverRole::Selector => {
                panic!("k=1 must bypass the selector session")
            }
            ImproverRole::Reflector => {
                ImproverReply { tree: HarnessTree::default(), notes: reflection_notes() }
            }
        };
        Ok(reply)
    })
}

fn task(workspace: &Path) -> HarnessTask {
    HarnessTask::new("t1", "do the work", "rust", workspace)
}

fn config(k: usize) -> HarnessConfig {
    HarnessConfig { k, ..HarnessConfig::default() }
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
