//! Deterministic demo channel and full-loop runner.
//!
//! Runs the real driver — references, propose, probe, compose, select, reflect —
//! with no model credentials. The channel is deterministic: a harness that
//! contains both the demo guardrails and the demo script produces the complete
//! deliverable, so the loop has a real contrast to select from.

use std::path::{Path, PathBuf};

use ee_agent_orchestrator::harness_reflection::ReflectionToolRecord;
use ee_agent_orchestrator::harness_validation::HarnessTree;
use ee_agent_orchestrator::rollout_view::{
    RolloutDiffFile, RolloutToolRecord, RolloutTurn, RolloutView,
};
use ee_agent_orchestrator::user_experience::ExperienceEntry;

use crate::channel::{
    Channel, ChannelFuture, Deliverable, DeliverableAsset, ImproverReply, ImproverRequest,
    ImproverRole, RunRecord, RunRequest,
};
use crate::driver::{HarnessConfig, HarnessMode, HarnessRun, ReflectionWriter, run_task};
use crate::error::{HarnessError, HarnessResult};
use crate::task::HarnessTask;

const DEMO_GUARDRAILS: &str =
    "demo: improved\nAlways write the demo deliverable file before finishing.\n";
const DEMO_SCRIPT: &str = "#!/bin/sh\nprintf 'demo deliverable\\n'\n";

/// Deterministic channel used by the demo runner and its tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoChannel;

impl Channel for DemoChannel {
    fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
        Box::pin(async move {
            let tree = HarnessTree::load_components(&request.harness_root.join(".agents"))
                .map_err(HarnessError::Validation)?;
            let improved = tree
                .files
                .get("guardrails.md")
                .is_some_and(|content| content.contains("demo: improved"));
            let scripted = tree.files.contains_key("scripts/demo.sh");
            let text = match (improved, scripted) {
                (true, true) => {
                    "DEMO: complete deliverable. Wrote the demo deliverable file with guardrails and the script in effect."
                }
                _ => {
                    "DEMO: partial deliverable. The demo deliverable file is missing its full shape."
                }
            };
            let asset = DeliverableAsset {
                name: String::from("demo.txt"),
                bytes: 16,
                content: Some(String::from("demo deliverable\n")),
            };
            let deliverable = Deliverable { text: text.to_string(), assets: vec![asset] };
            let rollout = RolloutView {
                session_id: format!("demo-{}", request.label),
                turns: vec![RolloutTurn {
                    turn: 1,
                    stop_reason: Some(String::from("end_turn")),
                    tools: vec![RolloutToolRecord {
                        turn: 1,
                        iteration: 1,
                        tool: String::from("write_file"),
                        success: true,
                        error_kind: None,
                        duration_ms: 3,
                    }],
                }],
                diff: vec![RolloutDiffFile { path: String::from("demo.txt"), change: "added" }],
                final_deliverable: Some(text.to_string()),
            };
            Ok(RunRecord {
                label: request.label,
                delivered: Some(deliverable),
                rollout,
                tool_records: vec![ReflectionToolRecord {
                    turn: 1,
                    iteration: 1,
                    tool: String::from("write_file"),
                    success: true,
                    summary: String::from("wrote demo.txt"),
                }],
                workspace_diff: Some(String::from("- added demo.txt")),
                sessions: 1,
                rollouts: 1,
                usage_tokens: None,
            })
        })
    }

    fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
        Box::pin(async move {
            match request.role {
                ImproverRole::Inv | ImproverRole::Dom | ImproverRole::Composer => {
                    let base = HarnessTree::load_components(&request.base_harness.join(".agents"))
                        .map_err(HarnessError::Validation)?;
                    let mut files = base.files;
                    let notes = match request.role {
                        ImproverRole::Inv => {
                            files
                                .insert(String::from("guardrails.md"), DEMO_GUARDRAILS.to_string());
                            String::from("kept -> guardrails.md -> probe-0-inv delivered")
                        }
                        ImproverRole::Dom => {
                            files.insert(String::from("scripts/demo.sh"), DEMO_SCRIPT.to_string());
                            String::from("kept -> scripts/demo.sh -> probe-0-dom delivered")
                        }
                        _ => {
                            files
                                .insert(String::from("guardrails.md"), DEMO_GUARDRAILS.to_string());
                            files.insert(String::from("scripts/demo.sh"), DEMO_SCRIPT.to_string());
                            String::from(
                                "kept -> guardrails.md -> probe-0-inv delivered (4 tool calls)\n\
                                 kept -> scripts/demo.sh -> probe-0-dom delivered (4 tool calls)",
                            )
                        }
                    };
                    Ok(ImproverReply { tree: HarnessTree::new(files), notes })
                }
                ImproverRole::Selector => {
                    let anon = pick_demo_choice(&request.context_dir)?;
                    let notes = format!(
                        "```choice\n{anon}\n```\n<why>\ndelivery criterion: the complete demo deliverable\n\
                         runner-up costs a missing deliverable file\n</why>"
                    );
                    Ok(ImproverReply { tree: HarnessTree::default(), notes })
                }
                ImproverRole::Reflector => Ok(ImproverReply {
                    tree: HarnessTree::default(),
                    notes: String::from(
                        "<edits>\ncomponent: guardrails.md\ncandidates: probe-0-inv\n\
                         change: added the demo delivery guardrail\n\
                         observed: the composed probe delivered the complete file\nverdict: helped\n\n\
                         component: scripts/demo.sh\ncandidates: probe-0-dom\n\
                         change: added the demo script\n\
                         observed: the composed probe kept the same delivery shape\n\
                         verdict: no-observed-effect\n</edits>\n\
                         <experience>\ncue: a demo task needs its named deliverable file\n\
                         lesson: write the named deliverable file before finishing\n\
                         evidence: ref-0 delivered a partial result while composed-0 delivered the file\n\
                         </experience>",
                    ),
                }),
            }
        })
    }
}

fn pick_demo_choice(context_dir: &Path) -> HarnessResult<String> {
    let selector_dir = context_dir.join("selector");
    let mut names = std::fs::read_dir(&selector_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".md"))
        .collect::<Vec<_>>();
    names.sort();
    for name in &names {
        let text = std::fs::read_to_string(selector_dir.join(name))?;
        if text.contains("DEMO: complete deliverable") {
            return Ok(name.trim_end_matches(".md").to_string());
        }
    }
    names
        .first()
        .map(|name| name.trim_end_matches(".md").to_string())
        .ok_or_else(|| HarnessError::State(String::from("demo selector had no candidates")))
}

/// Reflection writer used by the demo: always approved, same write path.
struct DemoWriter {
    workspace: PathBuf,
}

impl ReflectionWriter for DemoWriter {
    fn write(&self, entry: &ExperienceEntry, harness: &HarnessTree) -> HarnessResult<PathBuf> {
        ee_agent_orchestrator::user_experience::save_entry(&self.workspace, entry, Some(harness))
            .map_err(HarnessError::State)
    }
}

/// Runs the deterministic demo end to end.
pub async fn run_demo(root: &Path) -> HarnessResult<HarnessRun> {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join(".agents"))?;
    std::fs::write(
        workspace.join("README.md"),
        "# Demo workspace\n\nWrite the demo deliverable file.\n",
    )?;
    let task = HarnessTask::new(
        "demo",
        "Write the demo deliverable file and report what you wrote.",
        "demo",
        &workspace,
    );
    let config = HarnessConfig {
        k: 2,
        mode: HarnessMode::Parallel,
        dry_run: false,
        reflect: true,
        reflection_source: crate::driver::ReflectionSource::Invocation,
        improver: crate::channel::ImproverModel {
            model_id: None,
            effort: None,
            max_steps: 4,
            timeout_secs: 60,
        },
        max_workers: 1,
        resume: false,
        progress: false,
    };
    let writer = DemoWriter { workspace };
    run_task(&DemoChannel, &task, &config, &root.join("run"), Some(&writer)).await
}

/// Renders the demo plan without running it.
#[must_use]
pub fn demo_plan(root: &Path) -> crate::driver::DryRunPlan {
    let workspace = root.join("workspace");
    let task = HarnessTask::new("demo", "Write the demo deliverable file.", "demo", &workspace);
    crate::driver::dry_run_plan(&task, &HarnessConfig { k: 2, ..HarnessConfig::default() })
}

/// Convenience for CLI callers that only need the demo summary.
#[must_use]
pub fn demo_summary(run: &HarnessRun) -> String {
    let chosen = run.selection.chosen.as_deref().unwrap_or("none");
    let edits = run.reflection.as_ref().map_or(0, |output| output.edits.len());
    format!(
        "harness demo: selected={chosen} k={} rollouts={} sessions={} edits={edits}\n",
        run.k, run.ledger.rollouts, run.ledger.sessions
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn demo_runs_the_full_loop_hermetically() {
        let root = tempfile::tempdir().expect("root");
        let run = run_demo(root.path()).await.expect("demo run");
        assert_eq!(run.k, 2);
        assert!(!run.resumed);
        let chosen = run.selection.chosen.as_deref().expect("chosen candidate");
        assert!(chosen.starts_with("composed-"), "chosen {chosen}");
        assert!(!run.selection.fallback);
        assert_eq!(run.final_labels.len(), 2);
        assert_eq!(run.ledger.rollouts, 8, "R = 4k");
        assert_eq!(run.ledger.sessions, 6, "P = 3k");
        assert_eq!(run.ledger.selection_sessions, 1);
        assert_eq!(run.ledger.reflection_sessions, 1);
        assert!(run.adopted_path.as_ref().is_some_and(|path| path.join(".agents").is_dir()));
        let reflection = run.reflection.expect("reflection output");
        assert_eq!(reflection.edits.len(), 2);
        assert!(reflection.experience.is_some());
        assert!(run.reflected_path.is_some());
        assert!(
            root.path().join("workspace/.agents/experience/demo").is_dir(),
            "experience entry written through the approval path"
        );
    }

    #[tokio::test]
    async fn demo_plan_lists_the_loop_without_model_calls() {
        let root = tempfile::tempdir().expect("root");
        let plan = demo_plan(root.path());
        assert_eq!(plan.references, 2);
        assert_eq!(plan.editor_sessions, 4);
        assert_eq!(plan.composer_sessions, 2);
        assert!(plan.selection);
        assert!(plan.reflection);
    }
}
