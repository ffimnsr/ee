//! Deterministic automatic rubber-duck trigger policy and bounded deduplication.
//!
//! Policy consumes typed, host/runtime-observed facts only. It never inspects
//! critic prose, invokes models, or mutates workspace state. Automatic mode is
//! disabled by default until replay evidence explicitly enables it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::critique::CritiqueTarget;
use crate::strategy::{StrategyReason, TurnStrategy};

/// Trigger-policy contract version. Increment when decision or key semantics change.
pub const RUBBER_DUCK_TRIGGER_POLICY_VERSION: u32 = 1;
/// Maximum automatic trigger attempts retained per runtime.
pub const MAX_RUBBER_DUCK_TRIGGER_RECORDS: usize = 128;

/// Whether deterministic automatic critique boundaries are enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckTriggerMode {
    /// Explicit `/rubber-duck` requests only.
    #[default]
    ManualOnly,
    /// Enable deterministic high-leverage boundaries.
    Automatic,
}

/// Automatic trigger configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubberDuckTriggerConfig {
    pub mode: RubberDuckTriggerMode,
}

/// High-leverage boundary evaluated by orchestrator runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckTrigger {
    Plan,
    Implementation,
    Failure,
    Tests,
}

/// Typed work-impact facts supplied by trusted planning/host surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkImpact {
    PublicApi,
    Security,
    Persistence,
    Migration,
    Destructive,
    HighCoupling,
    Behavioral,
    FormattingOnly,
    Mechanical,
}

/// Stable reason for running automatic critique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckTriggerReason {
    MultiFilePlan,
    MaterialPlanImpact,
    NonTrivialChangedScope,
    DiagnosticsPresent,
    ValidationIncomplete,
    RecoveryOccurred,
    RepeatedFailure,
    BehavioralChangeWithoutTests,
}

/// Stable reason automatic critique did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckTriggerSkipReason {
    ManualOnly,
    Cancelled,
    PureQuestion,
    FormattingOnly,
    TrivialMechanicalEdit,
    AlreadyEvaluated,
    MissingRevision,
    NoMaterialSignal,
}

/// Terminal disposition retained for one automatic key. Every terminal outcome
/// remains claimed so equivalent failures cannot create hidden retry loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckTriggerDisposition {
    Running,
    Completed,
    Unavailable,
    Quarantined,
    Cancelled,
    Failed,
}

/// Deterministic automatic invocation identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RubberDuckTriggerKey {
    pub session_id: String,
    pub trigger: RubberDuckTrigger,
    pub target: CritiqueTarget,
    pub revision: String,
    pub material_fingerprint: String,
    pub policy_version: u32,
}

/// Typed facts evaluated at one boundary.
#[derive(Debug, Clone)]
pub struct RubberDuckTriggerFacts<'a> {
    pub session_id: &'a str,
    pub revision: &'a str,
    pub material_fingerprint: &'a str,
    pub strategy: TurnStrategy,
    pub strategy_reason: StrategyReason,
    pub impacts: &'a BTreeSet<WorkImpact>,
    pub planned_file_count: usize,
    pub changed_file_count: usize,
    pub diagnostics_present: bool,
    pub validation_passed: bool,
    pub validation_partial_or_skipped: bool,
    pub recovery_occurred: bool,
    pub repeated_failure_count: usize,
    pub selected_adjacent_tests: bool,
    pub cancelled: bool,
}

/// Pure policy result before deduplication claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RubberDuckTriggerDecision {
    Run { key: RubberDuckTriggerKey, reason: RubberDuckTriggerReason },
    Skip(RubberDuckTriggerSkipReason),
}

/// Pure deterministic trigger policy.
#[derive(Debug, Clone, Copy)]
pub struct RubberDuckTriggerPolicy {
    config: RubberDuckTriggerConfig,
}

impl RubberDuckTriggerPolicy {
    #[must_use]
    pub fn new(config: RubberDuckTriggerConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn evaluate(
        self,
        trigger: RubberDuckTrigger,
        facts: &RubberDuckTriggerFacts<'_>,
    ) -> RubberDuckTriggerDecision {
        if self.config.mode == RubberDuckTriggerMode::ManualOnly {
            return RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::ManualOnly);
        }
        if facts.cancelled {
            return RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::Cancelled);
        }
        if facts.revision.trim().is_empty() || facts.material_fingerprint.trim().is_empty() {
            return RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::MissingRevision);
        }
        if facts.strategy == TurnStrategy::SimpleAnswer {
            return RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::PureQuestion);
        }
        if facts.impacts.contains(&WorkImpact::FormattingOnly) {
            return RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::FormattingOnly);
        }

        let selection = match trigger {
            RubberDuckTrigger::Plan => {
                self.plan_reason(facts).map(|reason| (CritiqueTarget::Plan, reason))
            }
            RubberDuckTrigger::Implementation => self
                .implementation_reason(facts)
                .map(|reason| (CritiqueTarget::Implementation, reason)),
            RubberDuckTrigger::Failure => (facts.repeated_failure_count >= 2).then_some((
                CritiqueTarget::FailureAnalysis,
                RubberDuckTriggerReason::RepeatedFailure,
            )),
            RubberDuckTrigger::Tests => (facts.impacts.contains(&WorkImpact::Behavioral)
                && facts.changed_file_count > 0
                && !facts.selected_adjacent_tests)
                .then_some((
                    CritiqueTarget::Tests,
                    RubberDuckTriggerReason::BehavioralChangeWithoutTests,
                )),
        };
        let Some((target, reason)) = selection else {
            return RubberDuckTriggerDecision::Skip(
                if trigger == RubberDuckTrigger::Implementation && trivial_mechanical(facts) {
                    RubberDuckTriggerSkipReason::TrivialMechanicalEdit
                } else {
                    RubberDuckTriggerSkipReason::NoMaterialSignal
                },
            );
        };
        RubberDuckTriggerDecision::Run {
            key: RubberDuckTriggerKey {
                session_id: facts.session_id.to_string(),
                trigger,
                target,
                revision: facts.revision.to_string(),
                material_fingerprint: facts.material_fingerprint.to_string(),
                policy_version: RUBBER_DUCK_TRIGGER_POLICY_VERSION,
            },
            reason,
        }
    }

    fn plan_reason(self, facts: &RubberDuckTriggerFacts<'_>) -> Option<RubberDuckTriggerReason> {
        if facts.strategy != TurnStrategy::PlanThenExecute {
            return None;
        }
        if facts.strategy_reason == StrategyReason::MultiFileImplementation
            || facts.planned_file_count >= 2
        {
            return Some(RubberDuckTriggerReason::MultiFilePlan);
        }
        has_material_impact(facts.impacts).then_some(RubberDuckTriggerReason::MaterialPlanImpact)
    }

    fn implementation_reason(
        self,
        facts: &RubberDuckTriggerFacts<'_>,
    ) -> Option<RubberDuckTriggerReason> {
        if facts.changed_file_count == 0 || trivial_mechanical(facts) {
            return None;
        }
        if facts.changed_file_count >= 2 || has_material_impact(facts.impacts) {
            return Some(RubberDuckTriggerReason::NonTrivialChangedScope);
        }
        if facts.diagnostics_present {
            return Some(RubberDuckTriggerReason::DiagnosticsPresent);
        }
        if !facts.validation_passed || facts.validation_partial_or_skipped {
            return Some(RubberDuckTriggerReason::ValidationIncomplete);
        }
        facts.recovery_occurred.then_some(RubberDuckTriggerReason::RecoveryOccurred)
    }
}

fn has_material_impact(impacts: &BTreeSet<WorkImpact>) -> bool {
    impacts.iter().any(|impact| {
        matches!(
            impact,
            WorkImpact::PublicApi
                | WorkImpact::Security
                | WorkImpact::Persistence
                | WorkImpact::Migration
                | WorkImpact::Destructive
                | WorkImpact::HighCoupling
        )
    })
}

fn trivial_mechanical(facts: &RubberDuckTriggerFacts<'_>) -> bool {
    facts.changed_file_count == 1
        && facts.impacts.contains(&WorkImpact::Mechanical)
        && !facts.impacts.contains(&WorkImpact::Behavioral)
        && !has_material_impact(facts.impacts)
        && !facts.diagnostics_present
        && facts.validation_passed
        && !facts.validation_partial_or_skipped
        && !facts.recovery_occurred
}

/// Bounded automatic-attempt ledger. Keys are claimed before model dispatch.
#[derive(Debug, Default)]
pub struct RubberDuckTriggerController {
    records: BTreeMap<RubberDuckTriggerKey, RubberDuckTriggerDisposition>,
}

impl RubberDuckTriggerController {
    #[must_use]
    pub fn claim(&mut self, decision: RubberDuckTriggerDecision) -> RubberDuckTriggerDecision {
        let RubberDuckTriggerDecision::Run { key, reason } = decision else {
            return decision;
        };
        if self.records.contains_key(&key) {
            return RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::AlreadyEvaluated);
        }
        self.records.insert(key.clone(), RubberDuckTriggerDisposition::Running);
        self.evict_if_needed();
        RubberDuckTriggerDecision::Run { key, reason }
    }

    pub fn finish(
        &mut self,
        key: &RubberDuckTriggerKey,
        disposition: RubberDuckTriggerDisposition,
    ) {
        if let Some(record) = self.records.get_mut(key) {
            *record = disposition;
        }
    }

    #[must_use]
    pub fn disposition(&self, key: &RubberDuckTriggerKey) -> Option<RubberDuckTriggerDisposition> {
        self.records.get(key).copied()
    }

    fn evict_if_needed(&mut self) {
        while self.records.len() > MAX_RUBBER_DUCK_TRIGGER_RECORDS {
            if let Some(key) = self.records.keys().next().cloned() {
                self.records.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(impacts: &'a BTreeSet<WorkImpact>) -> RubberDuckTriggerFacts<'a> {
        RubberDuckTriggerFacts {
            session_id: "session-1",
            revision: "workspace-r1",
            material_fingerprint: "plan-a",
            strategy: TurnStrategy::PlanThenExecute,
            strategy_reason: StrategyReason::MultiFileImplementation,
            impacts,
            planned_file_count: 2,
            changed_file_count: 0,
            diagnostics_present: false,
            validation_passed: false,
            validation_partial_or_skipped: false,
            recovery_occurred: false,
            repeated_failure_count: 0,
            selected_adjacent_tests: false,
            cancelled: false,
        }
    }

    #[test]
    fn manual_only_is_default_and_skips_every_boundary() {
        let impacts = BTreeSet::from([WorkImpact::Security, WorkImpact::Behavioral]);
        let mut input = facts(&impacts);
        input.changed_file_count = 3;
        input.repeated_failure_count = 4;
        let policy = RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig::default());
        for trigger in [
            RubberDuckTrigger::Plan,
            RubberDuckTrigger::Implementation,
            RubberDuckTrigger::Failure,
            RubberDuckTrigger::Tests,
        ] {
            assert_eq!(
                policy.evaluate(trigger, &input),
                RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::ManualOnly)
            );
        }
    }

    #[test]
    fn pre_write_plan_claim_precedes_and_deduplicates_dispatch() {
        let impacts = BTreeSet::new();
        let decision = RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
            mode: RubberDuckTriggerMode::Automatic,
        })
        .evaluate(RubberDuckTrigger::Plan, &facts(&impacts));
        let mut controller = RubberDuckTriggerController::default();
        let claimed = controller.claim(decision.clone());
        let RubberDuckTriggerDecision::Run { key, reason } = claimed else {
            panic!("multi-file plan runs")
        };
        assert_eq!(reason, RubberDuckTriggerReason::MultiFilePlan);
        assert_eq!(controller.disposition(&key), Some(RubberDuckTriggerDisposition::Running));
        assert_eq!(
            controller.claim(decision),
            RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::AlreadyEvaluated)
        );
    }

    #[test]
    fn post_write_key_binds_revision_and_material_fingerprint() {
        let impacts = BTreeSet::from([WorkImpact::PublicApi]);
        let mut first = facts(&impacts);
        first.changed_file_count = 2;
        let policy = RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
            mode: RubberDuckTriggerMode::Automatic,
        });
        let mut controller = RubberDuckTriggerController::default();
        let one = controller.claim(policy.evaluate(RubberDuckTrigger::Implementation, &first));
        let RubberDuckTriggerDecision::Run { key: first_key, .. } = one else {
            panic!("first revision runs")
        };
        controller.finish(&first_key, RubberDuckTriggerDisposition::Completed);
        assert!(matches!(
            controller.claim(policy.evaluate(RubberDuckTrigger::Implementation, &first)),
            RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::AlreadyEvaluated)
        ));
        first.revision = "workspace-r2";
        first.material_fingerprint = "diff-b";
        assert!(matches!(
            controller.claim(policy.evaluate(RubberDuckTrigger::Implementation, &first)),
            RubberDuckTriggerDecision::Run { .. }
        ));
    }

    #[test]
    fn repeated_failure_invokes_once_without_retry_loop() {
        let impacts = BTreeSet::new();
        let mut input = facts(&impacts);
        input.repeated_failure_count = 2;
        let policy = RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
            mode: RubberDuckTriggerMode::Automatic,
        });
        let decision = policy.evaluate(RubberDuckTrigger::Failure, &input);
        let mut controller = RubberDuckTriggerController::default();
        let RubberDuckTriggerDecision::Run { key, .. } = controller.claim(decision.clone()) else {
            panic!("repeated failure runs")
        };
        controller.finish(&key, RubberDuckTriggerDisposition::Failed);
        assert_eq!(
            controller.claim(decision),
            RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::AlreadyEvaluated)
        );
    }

    #[test]
    fn trivial_mechanical_edit_with_focused_validation_skips() {
        let impacts = BTreeSet::from([WorkImpact::Mechanical]);
        let mut input = facts(&impacts);
        input.strategy = TurnStrategy::ToolLoop;
        input.strategy_reason = StrategyReason::FileInspectionRequested;
        input.planned_file_count = 0;
        input.changed_file_count = 1;
        input.validation_passed = true;
        let decision = RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
            mode: RubberDuckTriggerMode::Automatic,
        })
        .evaluate(RubberDuckTrigger::Implementation, &input);
        assert_eq!(
            decision,
            RubberDuckTriggerDecision::Skip(RubberDuckTriggerSkipReason::TrivialMechanicalEdit)
        );
    }

    #[test]
    fn one_target_revision_key_cannot_form_unlimited_loop() {
        let impacts = BTreeSet::from([WorkImpact::Behavioral]);
        let mut input = facts(&impacts);
        input.changed_file_count = 1;
        let policy = RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
            mode: RubberDuckTriggerMode::Automatic,
        });
        let decision = policy.evaluate(RubberDuckTrigger::Tests, &input);
        let mut controller = RubberDuckTriggerController::default();
        let mut runs = 0;
        for _ in 0..100 {
            if let RubberDuckTriggerDecision::Run { key, .. } = controller.claim(decision.clone()) {
                runs += 1;
                controller.finish(&key, RubberDuckTriggerDisposition::Unavailable);
            }
        }
        assert_eq!(runs, 1);
    }
}
