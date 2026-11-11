//! Evidence-gated stop control for changed turns.
//!
//! [`crate::completion::derive_completion`] already derives a terminal state
//! from tool-observed evidence, and [`crate::final_response::FinalResponse`]
//! exposes it as `can_finish`. The loop, however, used to return `EndTurn`
//! whenever the model signalled completion. [`StopGate`] installs that check at
//! the completion call site.
//!
//! Gate scope is deliberately narrow:
//!
//! - answer-only turns (no successful write-class execution) bypass the gate;
//! - verified evidence allows the stop;
//! - missing or failing evidence may run a bounded repair turn, then blocks;
//! - model prose is never evidence.
//!
//! The gate is deterministic and replay-safe: it reads only the execution log
//! and its own repair counter.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::completion::{
    CompletionEvidence, CompletionEvidenceItem, CompletionReport, CompletionState,
    derive_completion,
};
use crate::final_response::{
    ChangedFile, ValidationOutcome, ValidationRecord, changed_files_from_log,
};
use crate::strategy::is_validation_tool_name;
use crate::tasks::TaskId;
use crate::tools::ToolExecutionLogEntry;

/// Default bounded repair turns allowed after a blocked completion signal.
pub const DEFAULT_MAX_STOP_GATE_REPAIRS: usize = 1;
/// Cap for configured stop-gate repair turns.
pub const MAX_STOP_GATE_REPAIRS: usize = 4;
/// Maximum characters of blocker text embedded in a repair instruction.
pub const MAX_REPAIR_INSTRUCTION_CHARS: usize = 512;
/// Revision label assigned to loop-observed evidence.
pub const LOOP_EVIDENCE_REVISION: &str = "execution-log";

/// Stop-gate configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StopGateConfig {
    /// Whether changed turns must present verified evidence before stopping.
    pub enabled: bool,
    /// Bounded repair turns attempted before the gate blocks.
    pub max_repairs: usize,
}

impl Default for StopGateConfig {
    fn default() -> Self {
        Self { enabled: true, max_repairs: DEFAULT_MAX_STOP_GATE_REPAIRS }
    }
}

impl StopGateConfig {
    /// Clamps the repair cap to the hard limit.
    #[must_use]
    pub fn bounded(self) -> Self {
        Self { max_repairs: self.max_repairs.min(MAX_STOP_GATE_REPAIRS), ..self }
    }
}

/// Deterministic stop decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopGateOutcome {
    /// The loop may stop normally.
    Allow,
    /// The loop should run one more bounded iteration with repair guidance.
    Repair,
    /// The loop must stop without claiming success.
    Blocked,
}

impl StopGateOutcome {
    /// Stable lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Repair => "repair",
            Self::Blocked => "blocked",
        }
    }
}

/// Gate decision plus the evidence report that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StopGateDecision {
    /// Selected outcome.
    pub outcome: StopGateOutcome,
    /// Evidence-derived completion report.
    pub report: CompletionReport,
    /// Successful write-class executions observed this turn.
    pub changed_file_count: usize,
    /// Bounded repair instruction for [`StopGateOutcome::Repair`].
    pub repair_instruction: Option<String>,
    /// Why the gate blocked an otherwise allowed stop (plugin veto).
    pub veto_reason: Option<String>,
}

/// Optional plugin/host veto. Returning `true` from the callback blocks a stop
/// the evidence gate would otherwise allow; it never upgrades a blocked stop.
#[derive(Clone)]
pub struct StopVeto(Arc<dyn Fn(&StopGateDecision) -> bool + Send + Sync>);

impl StopVeto {
    /// Wraps a veto callback.
    #[must_use]
    pub fn new(veto: impl Fn(&StopGateDecision) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(veto))
    }

    /// Whether this veto denies `decision`.
    #[must_use]
    pub fn vetoed(&self, decision: &StopGateDecision) -> bool {
        (self.0)(decision)
    }
}

impl std::fmt::Debug for StopVeto {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("StopVeto").finish_non_exhaustive()
    }
}

/// Evidence gate evaluated before the loop returns `EndTurn`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StopGate;

impl StopGate {
    /// Evaluates one completion signal.
    ///
    /// `repairs_used` is the number of bounded repair turns already run for
    /// this turn, and `veto` may only deny an allowed stop.
    #[must_use]
    pub fn evaluate(
        &self,
        config: StopGateConfig,
        task_id: &TaskId,
        log: &[ToolExecutionLogEntry],
        repairs_used: usize,
        veto: Option<&StopVeto>,
    ) -> StopGateDecision {
        let changed_files = changed_files_from_log(log, task_id);
        if changed_files.is_empty() {
            return StopGateDecision {
                outcome: StopGateOutcome::Allow,
                report: bypassed_report(),
                changed_file_count: 0,
                repair_instruction: None,
                veto_reason: None,
            };
        }
        let evidence = completion_evidence_from_log(log, &changed_files);
        let validation = validation_records_from_log(log);
        let report = derive_completion(&changed_files, &evidence, &validation);
        let changed_file_count = changed_files.len();
        if report.is_verified() {
            let decision = StopGateDecision {
                outcome: StopGateOutcome::Allow,
                report,
                changed_file_count,
                repair_instruction: None,
                veto_reason: None,
            };
            if veto.is_some_and(|veto| veto.vetoed(&decision)) {
                return StopGateDecision {
                    outcome: StopGateOutcome::Blocked,
                    report: CompletionReport {
                        state: CompletionState::Blocked,
                        blocker: Some("plugin stop veto".into()),
                        safe_follow_up: Some(
                            "resolve the vetoing plugin condition before completing".into(),
                        ),
                        evidence_ids: decision.report.evidence_ids.clone(),
                    },
                    changed_file_count,
                    repair_instruction: None,
                    veto_reason: Some("plugin stop veto".into()),
                };
            }
            return decision;
        }
        if config.max_repairs > repairs_used {
            return StopGateDecision {
                outcome: StopGateOutcome::Repair,
                repair_instruction: Some(repair_instruction(&report)),
                report,
                changed_file_count,
                veto_reason: None,
            };
        }
        StopGateDecision {
            outcome: StopGateOutcome::Blocked,
            report,
            changed_file_count,
            repair_instruction: None,
            veto_reason: None,
        }
    }
}

/// Builds loop-observed evidence from successful write-class entries.
#[must_use]
pub fn completion_evidence_from_log(
    log: &[ToolExecutionLogEntry],
    changed_files: &[ChangedFile],
) -> CompletionEvidence {
    let mut evidence = CompletionEvidence::new(LOOP_EVIDENCE_REVISION);
    evidence.record_changed_file_inventory(
        CompletionEvidenceItem::passed(
            "inventory-log",
            "tool execution log",
            LOOP_EVIDENCE_REVISION,
        ),
        changed_files.iter().map(|file| file.path.clone()),
    );
    if let Some(entry) =
        log.iter().find(|entry| entry.success && is_diagnostics_tool_name(&entry.tool_name))
    {
        evidence.record_post_write_diagnostics(CompletionEvidenceItem::passed(
            "diagnostics-log",
            entry.tool_name.clone(),
            LOOP_EVIDENCE_REVISION,
        ));
    }
    if let Some(entry) =
        log.iter().find(|entry| entry.success && is_diff_review_tool_name(&entry.tool_name))
    {
        evidence.record_final_diff_review(CompletionEvidenceItem::passed(
            "diff-log",
            entry.tool_name.clone(),
            LOOP_EVIDENCE_REVISION,
        ));
    }
    evidence
}

/// Builds selected validation records from loop-observed validation tools.
#[must_use]
pub fn validation_records_from_log(log: &[ToolExecutionLogEntry]) -> Vec<ValidationRecord> {
    log.iter()
        .enumerate()
        .filter(|(_, entry)| is_validation_tool_name(&entry.tool_name))
        .map(|(index, entry)| {
            ValidationRecord::evidence(
                format!("validation-log-{index}"),
                entry.tool_name.clone(),
                if entry.success { ValidationOutcome::Passed } else { ValidationOutcome::Failed },
                Some(entry.tool_name.clone()),
                None,
                None,
                Vec::new(),
                0,
                entry.summary.len() > MAX_REPAIR_INSTRUCTION_CHARS,
                None,
                Some(LOOP_EVIDENCE_REVISION.into()),
                true,
                false,
                entry.success.then(|| entry.summary.clone()),
            )
        })
        .collect()
}

/// Whether the tool name identifies a diagnostics-producing tool.
#[must_use]
pub fn is_diagnostics_tool_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("diagnostic")
}

/// Whether the tool name identifies a diff-review tool.
#[must_use]
pub fn is_diff_review_tool_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("diff")
}

fn bypassed_report() -> CompletionReport {
    CompletionReport {
        state: CompletionState::Unverified,
        blocker: None,
        safe_follow_up: None,
        evidence_ids: Vec::new(),
    }
}

fn repair_instruction(report: &CompletionReport) -> String {
    let blocker = report.blocker.as_deref().unwrap_or("completion evidence is missing");
    let follow_up = report
        .safe_follow_up
        .as_deref()
        .unwrap_or("collect the missing tool evidence and report completion again");
    let mut instruction = format!(
        "Completion is blocked: {blocker}. Required next step: {follow_up}. Run the missing diagnostics or validation tools, then report completion again."
    );
    if instruction.chars().count() > MAX_REPAIR_INSTRUCTION_CHARS {
        instruction = instruction.chars().take(MAX_REPAIR_INSTRUCTION_CHARS).collect();
    }
    instruction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::SideEffectClass;

    fn entry(name: &str, class: SideEffectClass, success: bool) -> ToolExecutionLogEntry {
        ToolExecutionLogEntry {
            tool_call_id: format!("tc-{name}"),
            tool_name: name.into(),
            side_effect_class: Some(class),
            arguments: serde_json::json!({"path": "/work/src/lib.rs"}),
            success,
            summary: "done".into(),
        }
    }

    fn task() -> TaskId {
        TaskId::new("task-1")
    }

    #[test]
    fn answer_only_turn_bypasses_the_gate() {
        let log = vec![entry("read_file", SideEffectClass::Read, true)];
        let decision = StopGate.evaluate(StopGateConfig::default(), &task(), &log, 0, None);
        assert_eq!(decision.outcome, StopGateOutcome::Allow);
        assert_eq!(decision.changed_file_count, 0);
    }

    #[test]
    fn changed_turn_without_evidence_requests_bounded_repair() {
        let log = vec![entry("write_file", SideEffectClass::Write, true)];
        let decision = StopGate.evaluate(StopGateConfig::default(), &task(), &log, 0, None);
        assert_eq!(decision.outcome, StopGateOutcome::Repair);
        assert!(decision.repair_instruction.expect("instruction").contains("blocked"));
    }

    #[test]
    fn changed_turn_blocks_after_repair_budget() {
        let log = vec![entry("write_file", SideEffectClass::Write, true)];
        let config = StopGateConfig { max_repairs: 1, ..StopGateConfig::default() };
        let decision = StopGate.evaluate(config, &task(), &log, 1, None);
        assert_eq!(decision.outcome, StopGateOutcome::Blocked);
        assert!(decision.report.blocker.is_some());
    }

    #[test]
    fn verified_evidence_allows_the_stop() {
        let log = vec![
            entry("write_file", SideEffectClass::Write, true),
            entry("ee_diagnostics", SideEffectClass::Read, true),
            entry("ee_diff", SideEffectClass::Read, true),
            entry("cargo_test", SideEffectClass::Execute, true),
        ];
        let decision = StopGate.evaluate(StopGateConfig::default(), &task(), &log, 0, None);
        assert_eq!(decision.outcome, StopGateOutcome::Allow);
        assert!(decision.report.is_verified());
        assert_eq!(decision.changed_file_count, 1);
    }

    #[test]
    fn failed_validation_blocks_and_repairs() {
        let log = vec![
            entry("write_file", SideEffectClass::Write, true),
            entry("ee_diagnostics", SideEffectClass::Read, true),
            entry("ee_diff", SideEffectClass::Read, true),
            entry("cargo_test", SideEffectClass::Execute, false),
        ];
        let decision = StopGate.evaluate(StopGateConfig::default(), &task(), &log, 0, None);
        assert_eq!(decision.outcome, StopGateOutcome::Repair);
        assert!(decision.report.blocker.expect("blocker").contains("validation failed"));
    }

    #[test]
    fn plugin_veto_blocks_an_allowed_stop() {
        let log = vec![
            entry("write_file", SideEffectClass::Write, true),
            entry("ee_diagnostics", SideEffectClass::Read, true),
            entry("ee_diff", SideEffectClass::Read, true),
            entry("cargo_test", SideEffectClass::Execute, true),
        ];
        let veto = StopVeto::new(|_| true);
        let decision = StopGate.evaluate(StopGateConfig::default(), &task(), &log, 0, Some(&veto));
        assert_eq!(decision.outcome, StopGateOutcome::Blocked);
        assert_eq!(decision.veto_reason.as_deref(), Some("plugin stop veto"));
    }

    #[test]
    fn veto_never_upgrades_a_mandatory_denial() {
        let log = vec![entry("write_file", SideEffectClass::Write, true)];
        let veto = StopVeto::new(|_| false);
        let config = StopGateConfig { max_repairs: 0, ..StopGateConfig::default() };
        let decision = StopGate.evaluate(config, &task(), &log, 0, Some(&veto));
        assert_eq!(decision.outcome, StopGateOutcome::Blocked);
    }

    #[test]
    fn failed_write_is_not_a_changed_file() {
        let log = vec![entry("write_file", SideEffectClass::Write, false)];
        let decision = StopGate.evaluate(StopGateConfig::default(), &task(), &log, 0, None);
        assert_eq!(decision.outcome, StopGateOutcome::Allow);
    }
}
