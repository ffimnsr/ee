//! Propose → Probe → Compose → Select driver plus reflection and run state.
//!
//! The driver owns ordering, budget accounting, and run summaries. Stage work
//! lives in `stages`; stream scheduling lives in `stream`.

use std::path::{Path, PathBuf};

use ee_agent_orchestrator::harness_reflection::ReflectionOutput;
use ee_agent_orchestrator::harness_validation::HarnessTree;
use ee_agent_orchestrator::user_experience::ExperienceEntry;
use serde::{Deserialize, Serialize};

use crate::channel::{Channel, ImproverModel, ImproverRole, RunRecord};
use crate::context::StageEvidence;
use crate::error::HarnessError;
use crate::ledger::{Ledger, LedgerReport, MAX_K};
use crate::library_context::{self, LibraryEvidence};
use crate::runstate::{self, ImproverCache, RunSummary};
use crate::select::{
    Candidate, Selection, drop_non_delivering, neutral_fallback, trivial_selection,
};
use crate::task::HarnessTask;

mod stages;
mod stream;

use stages::{ImproveRequest, ReflectInputs, SelectionInputs, StageRuntime, materialize_harness};
pub use stream::{StreamTaskRun, run_stream, stream_order_path};

/// Layered editor contract: process and execution evidence.
pub const MISSION_INV: &str = concat!(
    "Improve the harness process layer from the reference and probe evidence. ",
    "Fix the earliest observed divergence first: the cheapest evidence-backed change sits ",
    "where two runs disagreed. Name the trajectory moment each change addresses. ",
    "Keeping the current harness unchanged (KEEP_BASE) is a valid answer. ",
    "Prefer a triggered skill, then a script, then a subagent, then a middleware hook, ",
    "and always-on prose last. Harness changes alter how the agent works, never the shape ",
    "of what it delivers. Write only inside this isolated workspace; edit `.agents/` and ",
    "return a complete candidate harness tree; do not delete every file."
);

/// Layered editor contract: domain and artifact knowledge.
pub const MISSION_DOM: &str = concat!(
    "Improve the harness domain layer from the reference and probe evidence: practitioner ",
    "knowledge, artifact contracts, and decision rules. Name the trajectory moment each ",
    "change addresses; KEEP_BASE is valid. Prefer a skill or script over always-on prose. ",
    "Never change the delivered artifact shape as a substitute for better work. Write only ",
    "inside this isolated workspace; edit `.agents/` and return a complete candidate harness tree."
);

/// Composer contract.
pub const MISSION_COMPOSER: &str = concat!(
    "Compose one harness tree from the full pool of references, candidates, probes, and diffs. ",
    "Merge, adopt one candidate, or keep the base (KEEP_BASE) — all are valid. ",
    "Emit one note line per decision exactly as `kept|revised|dropped -> <edit> -> ",
    "<trajectory moment with counts>`. An edit whose source rollout did not deliver is unsafe ",
    "unless the rest of the evidence is overwhelming; state that evidence in the note. ",
    "Write only inside this isolated workspace; edit `.agents/` and return the composed tree."
);

/// Selector contract.
pub const MISSION_SELECTOR: &str = concat!(
    "Choose one deliverable from the anonymized pool. Judge in this fixed order: delivery, ",
    "conformance, fidelity, internal consistency, operational fitness, finish. Length and ",
    "polish are not merits, and the plainer artifact is valid. The base deliverable is a valid ",
    "candidate. Output a fenced ```choice block with exactly one anonymous id and a <why> ",
    "block of at most six lines naming the deciding criterion and the runner-up's cost."
);

/// Reflector contract.
pub const MISSION_REFLECTOR: &str = "Reflect on the improvement run from the curated evidence. Emit exactly one <edits> block \
    with one five-line row per changed component (component, candidates, change, observed, \
    verdict), then at most one <experience> block with cue, lesson, evidence, and optional \
    confidence (low, medium, or high). When no component changed, emit an empty <edits> block \
    and NO_ENTRY; NO_ENTRY is also the valid answer when no execution contrast supports a \
    lesson. Never invent a trajectory moment, never treat selection as transfer evidence, and \
    never emit more than one lesson.";

/// Execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessMode {
    /// Independent candidates against the base harness.
    Parallel,
    /// Chained steps; each step's composition becomes the next base.
    Sequential,
}

impl HarnessMode {
    /// Stable label used for seeding, summaries, and run roots.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Sequential => "sequential",
        }
    }
}

/// Reflection persistence outcome recorded in the run summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionWrite {
    /// The experience entry was written.
    Written,
    /// Reflection produced no component changes and no lesson. This is a valid
    /// no-op, not a failure.
    NoEntry,
    /// Reflection ran but no write was requested.
    Declined,
    /// Reflection or the write failed.
    Failed,
    /// The reflector stage did not run.
    Disabled,
}

impl ReflectionWrite {
    /// Stable lowercase label used in run summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::NoEntry => "no-entry",
            Self::Declined => "declined",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
        }
    }

    /// Parses a recorded label; unknown values fall back to `Disabled`.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "written" => Self::Written,
            "no-entry" | "no_entry" => Self::NoEntry,
            "declined" => Self::Declined,
            "failed" => Self::Failed,
            _ => Self::Disabled,
        }
    }
}

/// Where reflection-write consent came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionSource {
    /// The invocation itself consented (default CLI or `/harness`).
    Invocation,
    /// An explicit `--reflection=decline` flag withheld consent.
    Flag,
    /// Reflection was disabled.
    None,
}

impl ReflectionSource {
    /// Stable lowercase label used in run summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::Flag => "flag",
            Self::None => "none",
        }
    }

    /// Parses a recorded label; unknown values fall back to `None`.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "invocation" => Self::Invocation,
            "flag" => Self::Flag,
            _ => Self::None,
        }
    }
}

/// Driver configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessConfig {
    /// Recursion depth; the paper's default is 1.
    pub k: usize,
    /// Execution mode.
    pub mode: HarnessMode,
    /// Print the plan without any model call.
    pub dry_run: bool,
    /// Run the reflector stage.
    pub reflect: bool,
    /// Consent provenance recorded when reflection writes.
    pub reflection_source: ReflectionSource,
    /// Improver policy.
    pub improver: ImproverModel,
    /// Domain concurrency for stream mode; task-level recursion uses it as the
    /// phase width, capped at two independent stages (INV/DOM or their probes).
    pub max_workers: usize,
    /// Reuse a matching completed summary.
    pub resume: bool,
    /// Emit incremental `status.json` and `ee: harness-progress` lines.
    /// CLI runs enable this; library tests and embedding callers leave it off.
    pub progress: bool,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            k: 1,
            mode: HarnessMode::Parallel,
            dry_run: false,
            reflect: true,
            reflection_source: ReflectionSource::Invocation,
            improver: ImproverModel {
                model_id: None,
                effort: None,
                max_steps: 12,
                timeout_secs: 300,
            },
            max_workers: 2,
            resume: true,
            progress: false,
        }
    }
}

impl HarnessConfig {
    /// Validates configuration and improver policy.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if self.k == 0 || self.k > MAX_K {
            return Err(HarnessError::Budget(format!("k must be 1-{MAX_K}")));
        }
        if self.max_workers == 0 {
            return Err(HarnessError::State("max_workers must be at least 1".into()));
        }
        self.improver.validate().map_err(HarnessError::State)
    }
}

/// Approval-gated reflection writer supplied by the caller.
pub trait ReflectionWriter: Send + Sync {
    /// Writes one reflection entry after its approval check.
    fn write(
        &self,
        entry: &ExperienceEntry,
        harness: &HarnessTree,
    ) -> Result<std::path::PathBuf, HarnessError>;
}

/// Completion record for one driver run.
#[derive(Debug, Clone)]
pub struct HarnessRun {
    /// Task id.
    pub task_id: String,
    /// Recursion depth.
    pub k: usize,
    /// Execution mode.
    pub mode: HarnessMode,
    /// Whether this result was served from a completed summary.
    pub resumed: bool,
    /// Adopted harness tree.
    pub final_harness: HarnessTree,
    /// Selection outcome.
    pub selection: Selection,
    /// Final pool labels.
    pub final_labels: Vec<String>,
    /// Ledger report.
    pub ledger: LedgerReport,
    /// Parsed reflection, when the stage ran.
    pub reflection: Option<ReflectionOutput>,
    /// Path of the adopted harness copy, when written.
    pub adopted_path: Option<std::path::PathBuf>,
    /// Path of the written reflection entry, when approved.
    pub reflected_path: Option<std::path::PathBuf>,
    /// Experience-library root used for both reads and writes.
    pub library_root: PathBuf,
    /// Recorded reflection persistence outcome.
    pub reflection_write: ReflectionWrite,
    /// Recorded consent provenance.
    pub reflection_source: ReflectionSource,
}

/// Plan rendered by dry-run mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunPlan {
    /// Task id.
    pub task_id: String,
    /// Recursion depth.
    pub k: usize,
    /// Execution mode.
    pub mode: HarnessMode,
    /// Reference rollouts.
    pub references: usize,
    /// Editor sessions (INV + DOM per step).
    pub editor_sessions: usize,
    /// Composer sessions.
    pub composer_sessions: usize,
    /// Candidate probe rollouts.
    pub candidate_rollouts: usize,
    /// Composed probe rollouts.
    pub composed_rollouts: usize,
    /// Whether a selector session runs.
    pub selection: bool,
    /// Whether a reflector session runs.
    pub reflection: bool,
}

impl DryRunPlan {
    /// Renders the plan without model calls.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "harness plan: task={} k={} mode={}\n\
             references={} editor_sessions={} composer_sessions={}\n\
             candidate_rollouts={} composed_rollouts={} selection={} reflection={}\n",
            self.task_id,
            self.k,
            self.mode.as_str(),
            self.references,
            self.editor_sessions,
            self.composer_sessions,
            self.candidate_rollouts,
            self.composed_rollouts,
            self.selection,
            self.reflection
        )
    }
}

/// Builds the dry-run plan for one task.
#[must_use]
pub fn dry_run_plan(task: &HarnessTask, config: &HarnessConfig) -> DryRunPlan {
    let k = config.k;
    DryRunPlan {
        task_id: task.id.clone(),
        k,
        mode: config.mode,
        references: k,
        editor_sessions: 2 * k,
        composer_sessions: k,
        candidate_rollouts: 2 * k,
        composed_rollouts: k,
        selection: !trivial_selection(k),
        reflection: config.reflect,
    }
}

/// Two proposers and two probe rollouts per step are independent. Cap the
/// phase at two regardless of `max_workers`, matching harness2's PHASE_WIDTH.
fn phase_width(config: &HarnessConfig) -> usize {
    config.max_workers.clamp(1, 2)
}

fn emit_progress(
    config: &HarnessConfig,
    task: &HarnessTask,
    run_root: &Path,
    stage: &str,
    state: &str,
    ledger: &Ledger,
) {
    if !config.progress {
        return;
    }
    let report = ledger.report();
    let status = runstate::HarnessStatus {
        schema_version: 1,
        task_id: task.id.clone(),
        stage: stage.to_string(),
        state: state.to_string(),
        rollouts: report.rollouts,
        sessions: report.sessions,
        selection_sessions: report.selection_sessions,
        retry_sessions: report.retry_sessions,
        reflection_sessions: report.reflection_sessions,
        dead_rollouts: report.dead_rollouts.len(),
        base_failed: report.base_failed,
    };
    if let Err(error) = runstate::save_status(run_root, &status) {
        tracing::warn!(%error, "harness status could not be written");
    }
    match serde_json::to_string(&status) {
        Ok(json) => println!("ee: harness-progress {json}"),
        Err(error) => tracing::warn!(%error, "harness status could not be serialized"),
    }
}

/// Runs one improvement task end to end.
pub async fn run_task(
    channel: &dyn Channel,
    task: &HarnessTask,
    config: &HarnessConfig,
    run_root: &Path,
    writer: Option<&dyn ReflectionWriter>,
) -> Result<HarnessRun, HarnessError> {
    config.validate()?;
    task.validate()?;
    if config.dry_run {
        return Ok(HarnessRun {
            task_id: task.id.clone(),
            k: config.k,
            mode: config.mode,
            resumed: false,
            final_harness: HarnessTree::default(),
            selection: Selection { chosen: None, anon: None, why: Vec::new(), fallback: false },
            final_labels: Vec::new(),
            ledger: Ledger::new().report(),
            reflection: None,
            adopted_path: None,
            reflected_path: None,
            library_root: task.library_root(),
            reflection_write: ReflectionWrite::Disabled,
            reflection_source: ReflectionSource::None,
        });
    }
    std::fs::create_dir_all(run_root)?;
    let base_tree = task.base_harness()?;
    if config.resume
        && let Some(summary) = runstate::load_summary(run_root)
    {
        if summary.k != config.k || summary.mode != config.mode.as_str() {
            tracing::info!(
                recorded_k = summary.k,
                recorded_mode = %summary.mode,
                k = config.k,
                mode = config.mode.as_str(),
                "resume rejected: invocation changed"
            );
        } else {
            let adopted_dir = run_root.join("adopted/.agents");
            let adopted =
                HarnessTree::load_components(&adopted_dir).map_err(HarnessError::Validation)?;
            let present = adopted_dir.is_dir();
            match runstate::resume_decision(&summary, &adopted, task, present) {
                runstate::ResumeDecision::Resume => {
                    return Ok(HarnessRun {
                        task_id: task.id.clone(),
                        k: summary.k,
                        mode: config.mode,
                        resumed: true,
                        final_harness: adopted,
                        selection: Selection {
                            chosen: summary.selected_label,
                            anon: None,
                            why: vec![String::from("resumed completed run")],
                            fallback: false,
                        },
                        final_labels: Vec::new(),
                        ledger: Ledger::new().report(),
                        reflection: None,
                        adopted_path: Some(run_root.join("adopted")),
                        reflected_path: summary
                            .reflected_path
                            .as_ref()
                            .map(PathBuf::from)
                            .filter(|path| path.exists()),
                        library_root: summary
                            .library_root
                            .as_ref()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| task.library_root()),
                        reflection_write: ReflectionWrite::parse(&summary.reflection_write),
                        reflection_source: ReflectionSource::parse(&summary.reflection_source),
                    });
                }
                runstate::ResumeDecision::Rerun(reason) => {
                    tracing::info!(%reason, "resume rejected; re-running the task");
                }
            }
        }
    }

    let base_dir = materialize_harness(&base_tree, &run_root.join("harness/base"))?;
    let cache = ImproverCache::new(run_root.join("improver-cache"));
    let runtime = StageRuntime { channel, task, config, run_root, cache: &cache };
    let mut ledger = Ledger::new();
    emit_progress(config, task, run_root, "starting", "running", &ledger);
    let mut references: Vec<RunRecord> = Vec::new();
    for index in 0..config.k {
        let label = format!("ref-{index}");
        let record = runtime.run_rollout(&label, &base_dir, &mut ledger).await?;
        references.push(record);
    }
    emit_progress(config, task, run_root, "reference", "completed", &ledger);
    if references.iter().all(|record| !task.delivered(record)) {
        ledger.record_base_failed();
        emit_progress(config, task, run_root, "base-failed", "failed", &ledger);
        let summary = RunSummary {
            schema_version: 1,
            task_id: task.id.clone(),
            mode: config.mode.as_str().into(),
            k: config.k,
            selected_label: None,
            harness_fingerprint: runstate::fingerprint_tree(&base_tree),
            task_fingerprint: runstate::fingerprint_task(task),
            completed: true,
            terminal_state: String::from("base-failed"),
            library_root: Some(task.library_root().display().to_string()),
            reflection_write: String::from(ReflectionWrite::Disabled.as_str()),
            reflection_source: String::from(ReflectionSource::None.as_str()),
            reflected_path: None,
        };
        runstate::save_summary(run_root, &summary)?;
        return Ok(HarnessRun {
            task_id: task.id.clone(),
            k: config.k,
            mode: config.mode,
            resumed: false,
            final_harness: base_tree,
            selection: Selection {
                chosen: None,
                anon: None,
                why: vec![String::from("base harness delivered no reference run")],
                fallback: true,
            },
            final_labels: Vec::new(),
            ledger: ledger.report(),
            reflection: None,
            adopted_path: None,
            reflected_path: None,
            library_root: task.library_root(),
            reflection_write: ReflectionWrite::Disabled,
            reflection_source: ReflectionSource::None,
        });
    }

    let library = library_context::load(task);
    let mut current_base = base_tree.clone();
    let mut finals: Vec<(Candidate, HarnessTree, RunRecord)> = Vec::new();
    let mut probe_records: Vec<(String, HarnessTree, RunRecord)> = Vec::new();
    let mut decisions: Vec<(String, String)> = Vec::new();
    for step in 0..config.k {
        let step_base = match config.mode {
            HarnessMode::Parallel => base_tree.clone(),
            HarnessMode::Sequential => current_base.clone(),
        };
        let step_base_dir =
            materialize_harness(&step_base, &run_root.join(format!("harness/step-{step}/base")))?;
        // Two proposers and two probe rollouts per step. Run each pair concurrently
        // when the worker budget allows: the stages are independent, write to
        // distinct candidate/scratch directories, and the ledger merges by label.
        let phase_width = phase_width(config);
        let inv_stage = format!("step-{step}-inv");
        let dom_stage = format!("step-{step}-dom");
        let inv_evidence = stage_evidence(
            &inv_stage,
            MISSION_INV,
            task,
            &step_base,
            &references,
            &library,
            decisions.clone(),
        );
        let dom_evidence = stage_evidence(
            &dom_stage,
            MISSION_DOM,
            task,
            &step_base,
            &references,
            &library,
            decisions.clone(),
        );
        emit_progress(config, task, run_root, &format!("step-{step}-propose"), "running", &ledger);
        let (inv_reply, dom_reply) = if phase_width >= 2 {
            let mut inv_ledger = Ledger::new();
            let mut dom_ledger = Ledger::new();
            let (inv, dom) = tokio::join!(
                runtime.improve(
                    ImproveRequest {
                        role: ImproverRole::Inv,
                        stage: inv_stage,
                        evidence: inv_evidence,
                        base: step_base.clone(),
                        base_dir: step_base_dir.clone(),
                    },
                    &mut inv_ledger,
                ),
                runtime.improve(
                    ImproveRequest {
                        role: ImproverRole::Dom,
                        stage: dom_stage,
                        evidence: dom_evidence,
                        base: step_base.clone(),
                        base_dir: step_base_dir.clone(),
                    },
                    &mut dom_ledger,
                ),
            );
            let inv = inv?;
            let dom = dom?;
            ledger.merge(inv_ledger)?;
            ledger.merge(dom_ledger)?;
            (inv, dom)
        } else {
            let inv = runtime
                .improve(
                    ImproveRequest {
                        role: ImproverRole::Inv,
                        stage: inv_stage,
                        evidence: inv_evidence,
                        base: step_base.clone(),
                        base_dir: step_base_dir.clone(),
                    },
                    &mut ledger,
                )
                .await?;
            let dom = runtime
                .improve(
                    ImproveRequest {
                        role: ImproverRole::Dom,
                        stage: dom_stage,
                        evidence: dom_evidence,
                        base: step_base.clone(),
                        base_dir: step_base_dir.clone(),
                    },
                    &mut ledger,
                )
                .await?;
            (inv, dom)
        };

        emit_progress(
            config,
            task,
            run_root,
            &format!("step-{step}-propose"),
            "completed",
            &ledger,
        );
        let mut prepared = Vec::new();
        for (role, reply) in [(ImproverRole::Inv, inv_reply), (ImproverRole::Dom, dom_reply)] {
            let label = format!("probe-{step}-{}", role.as_str());
            let tree = reply.tree;
            let harness_dir = materialize_harness(
                &tree,
                &run_root.join(format!("harness/step-{step}/probe-{}", role.as_str())),
            )?;
            prepared.push((label, tree, harness_dir));
        }
        let records = if phase_width >= 2 {
            let mut inv_ledger = Ledger::new();
            let mut dom_ledger = Ledger::new();
            let (inv, dom) = tokio::join!(
                runtime.run_rollout(&prepared[0].0, &prepared[0].2, &mut inv_ledger),
                runtime.run_rollout(&prepared[1].0, &prepared[1].2, &mut dom_ledger),
            );
            let inv = inv?;
            let dom = dom?;
            ledger.merge(inv_ledger)?;
            ledger.merge(dom_ledger)?;
            vec![inv, dom]
        } else {
            let mut serial = Vec::new();
            for (label, _, harness_dir) in &prepared {
                serial.push(runtime.run_rollout(label, harness_dir, &mut ledger).await?);
            }
            serial
        };
        emit_progress(config, task, run_root, &format!("step-{step}-probe"), "completed", &ledger);
        let mut probes: Vec<(String, HarnessTree, RunRecord)> = Vec::new();
        for ((label, tree, _), record) in prepared.into_iter().zip(records) {
            probe_records.push((label.clone(), tree.clone(), record.clone()));
            probes.push((label, tree, record));
        }
        emit_progress(config, task, run_root, &format!("step-{step}-compose"), "running", &ledger);
        let compose_stage = format!("step-{step}-compose");
        let mut probe_harnesses: Vec<(String, HarnessTree)> =
            vec![(String::from("base"), step_base.clone())];
        probe_harnesses.extend(probes.iter().map(|(label, tree, _)| (label.clone(), tree.clone())));
        let mut probe_rollouts: Vec<(String, ee_agent_orchestrator::rollout_view::RolloutView)> =
            Vec::new();
        let mut probe_deliverables = Vec::new();
        for record in references.iter().chain(probes.iter().map(|(_, _, record)| record)) {
            probe_rollouts.push((record.label.clone(), record.rollout.clone()));
            if let Some(deliverable) = &record.delivered {
                probe_deliverables.push((record.label.clone(), deliverable.clone()));
            }
        }
        let compose_evidence = StageEvidence {
            stage: compose_stage.clone(),
            mission: MISSION_COMPOSER.to_string(),
            task: task.clone(),
            harnesses: probe_harnesses,
            rollouts: probe_rollouts,
            deliverables: probe_deliverables,
            library: library.clone(),
            decisions: decisions.clone(),
            selector: Vec::new(),
            observations: None,
        };
        let composed = runtime
            .improve(
                ImproveRequest {
                    role: crate::channel::ImproverRole::Composer,
                    stage: compose_stage.clone(),
                    evidence: compose_evidence,
                    base: step_base.clone(),
                    base_dir: step_base_dir.clone(),
                },
                &mut ledger,
            )
            .await?;
        decisions.push((compose_stage.clone(), composed.notes.clone()));
        emit_progress(config, task, run_root, &compose_stage, "completed", &ledger);
        if crate::compose::parse_notes(&composed.notes).is_empty() {
            tracing::warn!(
                stage = %compose_stage,
                "composer notes do not satisfy `kept|revised|dropped -> <edit> -> <moment>`"
            );
        }
        let composed_label = format!("composed-{step}");
        let composed_dir = materialize_harness(
            &composed.tree,
            &run_root.join(format!("harness/step-{step}/composed")),
        )?;
        let composed_record =
            runtime.run_rollout(&composed_label, &composed_dir, &mut ledger).await?;
        emit_progress(
            config,
            task,
            run_root,
            &format!("step-{step}-rollout"),
            "completed",
            &ledger,
        );
        let candidate = Candidate {
            label: composed_label,
            delivered: task.delivered(&composed_record),
            deliverable: composed_record.delivered.clone(),
            is_base: false,
        };
        finals.push((candidate, composed.tree.clone(), composed_record));
        if config.mode == HarnessMode::Sequential {
            current_base = composed.tree;
        }
    }

    let base_candidate = references
        .iter()
        .find(|record| task.delivered(record))
        .map(|record| Candidate {
            label: record.label.clone(),
            delivered: true,
            deliverable: record.delivered.clone(),
            is_base: true,
        })
        .ok_or_else(|| {
            HarnessError::State(String::from("no delivered reference after the base gate"))
        })?;
    let final_candidates: Vec<Candidate> = std::iter::once(base_candidate)
        .chain(finals.iter().map(|(candidate, _, _)| candidate.clone()))
        .collect();
    let (pool, dropped) = drop_non_delivering(final_candidates);
    let (selection, adoption, raw_selection_record) = if trivial_selection(config.k) {
        let chosen = finals
            .iter()
            .find(|(candidate, _, _)| candidate.delivered)
            .map(|(candidate, _, _)| candidate.label.clone())
            .or_else(|| {
                pool.iter()
                    .find(|candidate| candidate.is_base)
                    .map(|candidate| candidate.label.clone())
            });
        let selection = if chosen.is_some() {
            Selection {
                chosen,
                anon: None,
                why: vec![String::from("k=1 bypasses the selector session")],
                fallback: false,
            }
        } else {
            neutral_fallback(&pool)
        };
        (selection, None, None)
    } else {
        let outcome = runtime
            .select(
                SelectionInputs {
                    pool: pool.clone(),
                    finals: &finals,
                    references: &references,
                    dropped: &dropped,
                    base_dir: &base_dir,
                },
                &mut ledger,
            )
            .await?;
        (outcome.selection, outcome.adoption, outcome.raw_reply)
    };

    emit_progress(config, task, run_root, "selection", "completed", &ledger);
    let chosen_tree = match selection.chosen.as_deref() {
        Some(label) if label.starts_with("ref-") => base_tree.clone(),
        Some(label) => finals
            .iter()
            .find(|(candidate, _, _)| candidate.label == label)
            .map(|(_, tree, _)| tree.clone())
            .unwrap_or_else(|| base_tree.clone()),
        None => base_tree.clone(),
    };
    ledger.assert_identity(config.k)?;

    let adopted_path = run_root.join("adopted");
    materialize_harness(&chosen_tree, &adopted_path)?;
    emit_progress(config, task, run_root, "adopted", "completed", &ledger);

    let mut reflection = None;
    let mut reflected_path = None;
    let mut reflection_write =
        if config.reflect { ReflectionWrite::Declined } else { ReflectionWrite::Disabled };
    let reflection_source =
        if config.reflect { config.reflection_source } else { ReflectionSource::None };
    if config.reflect {
        emit_progress(config, task, run_root, "reflection", "running", &ledger);
        let outcome = runtime
            .reflect(
                ReflectInputs {
                    base: &base_tree,
                    chosen: &chosen_tree,
                    finals: &finals,
                    references: &references,
                    probes: &probe_records,
                    adoption,
                    decisions: &decisions,
                    selection: &selection,
                    raw_selection_record,
                    base_dir: &base_dir,
                    writer,
                    library: &library,
                },
                &mut ledger,
            )
            .await?;
        reflection = outcome.output;
        reflected_path = outcome.path;
        reflection_write = outcome.write;
        emit_progress(config, task, run_root, "reflection", "completed", &ledger);
    }
    emit_progress(config, task, run_root, "complete", "completed", &ledger);

    let summary = RunSummary {
        schema_version: 1,
        task_id: task.id.clone(),
        mode: config.mode.as_str().into(),
        k: config.k,
        selected_label: selection.chosen.clone(),
        harness_fingerprint: runstate::fingerprint_tree(&chosen_tree),
        task_fingerprint: runstate::fingerprint_task(task),
        completed: true,
        terminal_state: if selection.chosen.is_some() {
            String::from("delivered")
        } else {
            String::from("no-deliverable")
        },
        library_root: Some(task.library_root().display().to_string()),
        reflection_write: String::from(reflection_write.as_str()),
        reflection_source: String::from(reflection_source.as_str()),
        reflected_path: reflected_path.as_ref().map(|path| path.display().to_string()),
    };
    runstate::save_summary(run_root, &summary)?;

    Ok(HarnessRun {
        task_id: task.id.clone(),
        k: config.k,
        mode: config.mode,
        resumed: false,
        final_harness: chosen_tree,
        selection,
        final_labels: finals.iter().map(|(candidate, _, _)| candidate.label.clone()).collect(),
        ledger: ledger.report(),
        reflection,
        adopted_path: Some(adopted_path),
        reflected_path,
        library_root: task.library_root(),
        reflection_write,
        reflection_source,
    })
}

fn stage_evidence(
    stage: &str,
    mission: &str,
    task: &HarnessTask,
    base: &HarnessTree,
    references: &[RunRecord],
    library: &LibraryEvidence,
    decisions: Vec<(String, String)>,
) -> StageEvidence {
    StageEvidence {
        stage: stage.to_string(),
        mission: mission.to_string(),
        task: task.clone(),
        harnesses: vec![(String::from("base"), base.clone())],
        rollouts: references
            .iter()
            .map(|record| (record.label.clone(), record.rollout.clone()))
            .collect(),
        deliverables: references
            .iter()
            .filter_map(|record| {
                record.delivered.clone().map(|deliverable| (record.label.clone(), deliverable))
            })
            .collect(),
        library: library.clone(),
        decisions,
        selector: Vec::new(),
        observations: None,
    }
}
