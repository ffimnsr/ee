//! Budget ledger for one improvement task.
//!
//! The paper's ledger asserts an identity per task: R = 4k rollouts and
//! P = 3k improvement sessions. Selection and reflection sessions are tracked
//! outside that identity, matching the revised plan. Events are keyed by stable
//! label, so concurrent pools record the same sequence as a serial run.

use std::collections::BTreeMap;

use crate::error::{HarnessError, HarnessResult};

/// Maximum k accepted by one task.
pub const MAX_K: usize = 16;
/// Rollouts per step in the identity.
pub const ROLLOUTS_PER_STEP: u64 = 4;
/// Improver sessions per step in the identity.
pub const SESSIONS_PER_STEP: u64 = 3;
/// Hard cap on identity rollouts per task.
pub const MAX_ROLLOUTS_PER_TASK: u64 = ROLLOUTS_PER_STEP * MAX_K as u64;
/// Hard cap on identity improvement sessions per task.
pub const MAX_SESSIONS_PER_TASK: u64 = SESSIONS_PER_STEP * MAX_K as u64;

/// What one ledger event consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LedgerKind {
    /// One rollout (reference or probe).
    Rollout,
    /// One INV/DOM/composer session inside the identity.
    ImprovementSession,
    /// One validator retry, tracked outside the identity.
    ImprovementRetry,
    /// One selection session, tracked outside the identity.
    SelectionSession,
    /// One reflection session, tracked outside the identity.
    ReflectionSession,
}

impl LedgerKind {
    /// Stable lowercase label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rollout => "rollout",
            Self::ImprovementSession => "improvement-session",
            Self::ImprovementRetry => "improvement-retry",
            Self::SelectionSession => "selection-session",
            Self::ReflectionSession => "reflection-session",
        }
    }
}

/// One recorded ledger event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEvent {
    /// Stable label that makes the event idempotent and order-independent.
    pub label: String,
    /// What was consumed.
    pub kind: LedgerKind,
    /// Bounded detail.
    pub detail: String,
}

/// One dead rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadRollout {
    /// Rollout label.
    pub label: String,
    /// Why the rollout was recorded but not used.
    pub reason: String,
}

/// Mutable per-task ledger.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    events: BTreeMap<String, LedgerEvent>,
    dead_rollouts: Vec<DeadRollout>,
    base_failed: bool,
}

impl Ledger {
    /// Creates an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one event under its stable label.
    pub fn record(
        &mut self,
        kind: LedgerKind,
        label: impl Into<String>,
        detail: impl Into<String>,
    ) -> HarnessResult<()> {
        let label = label.into();
        if self.events.contains_key(&label) {
            return Err(HarnessError::Budget(format!("duplicate ledger label `{label}`")));
        }
        self.events.insert(label.clone(), LedgerEvent { label, kind, detail: detail.into() });
        self.enforce_caps()
    }

    /// Records a rollout that returned but is unsafe to use.
    pub fn dead_rollout(
        &mut self,
        label: impl Into<String>,
        reason: impl Into<String>,
    ) -> HarnessResult<()> {
        let label = label.into();
        let reason = reason.into();
        self.record(LedgerKind::Rollout, label.clone(), format!("dead: {reason}"))?;
        self.dead_rollouts.push(DeadRollout { label, reason });
        Ok(())
    }

    /// Merges the events of one concurrently-run phase ledger.
    ///
    /// Labels are stable and unique per stage, so a concurrent phase records
    /// exactly the events a serial run would. Dead rollouts are kept in label
    /// order so the report stays deterministic regardless of completion order.
    pub(crate) fn merge(&mut self, other: Ledger) -> HarnessResult<()> {
        for (label, event) in other.events {
            if self.events.insert(label.clone(), event).is_some() {
                return Err(HarnessError::Budget(format!("duplicate ledger label `{label}`")));
            }
        }
        self.base_failed |= other.base_failed;
        self.dead_rollouts.extend(other.dead_rollouts);
        self.dead_rollouts.sort_by(|a, b| a.label.cmp(&b.label));
        self.enforce_caps()
    }

    /// Marks the base task failed before any candidate could be probed.
    pub fn record_base_failed(&mut self) {
        self.base_failed = true;
    }

    /// Whether the base harness delivered no reference run.
    #[must_use]
    pub fn base_failed(&self) -> bool {
        self.base_failed
    }

    /// Identity rollouts recorded.
    #[must_use]
    pub fn rollouts(&self) -> u64 {
        self.count(LedgerKind::Rollout)
    }

    /// Identity improvement sessions recorded.
    #[must_use]
    pub fn sessions(&self) -> u64 {
        self.count(LedgerKind::ImprovementSession)
    }

    /// Selection sessions recorded outside the identity.
    #[must_use]
    pub fn selection_sessions(&self) -> u64 {
        self.count(LedgerKind::SelectionSession)
    }

    /// Validator retries recorded outside the identity.
    #[must_use]
    pub fn retry_sessions(&self) -> u64 {
        self.count(LedgerKind::ImprovementRetry)
    }

    /// Reflection sessions recorded outside the identity.
    #[must_use]
    pub fn reflection_sessions(&self) -> u64 {
        self.count(LedgerKind::ReflectionSession)
    }

    /// Asserts R = 4k and P = 3k for the completed task.
    pub fn assert_identity(&self, k: usize) -> HarnessResult<LedgerReport> {
        if k == 0 || k > MAX_K {
            return Err(HarnessError::Budget(format!("k must be 1-{MAX_K}")));
        }
        let expected_rollouts = ROLLOUTS_PER_STEP * k as u64;
        let expected_sessions = SESSIONS_PER_STEP * k as u64;
        if self.rollouts() != expected_rollouts {
            return Err(HarnessError::Budget(format!(
                "ledger identity failed: {} rollouts recorded, expected {expected_rollouts} (4k)",
                self.rollouts()
            )));
        }
        if self.sessions() != expected_sessions {
            return Err(HarnessError::Budget(format!(
                "ledger identity failed: {} improvement sessions recorded, expected {expected_sessions} (3k)",
                self.sessions()
            )));
        }
        self.enforce_caps()?;
        Ok(self.report())
    }

    /// Immutable report with events in stable label order.
    #[must_use]
    pub fn report(&self) -> LedgerReport {
        LedgerReport {
            rollouts: self.rollouts(),
            sessions: self.sessions(),
            selection_sessions: self.selection_sessions(),
            retry_sessions: self.retry_sessions(),
            reflection_sessions: self.reflection_sessions(),
            dead_rollouts: self.dead_rollouts.clone(),
            base_failed: self.base_failed,
            events: self.events.values().cloned().collect(),
        }
    }

    fn count(&self, kind: LedgerKind) -> u64 {
        self.events.values().filter(|event| event.kind == kind).count() as u64
    }

    fn enforce_caps(&self) -> HarnessResult<()> {
        if self.rollouts() > MAX_ROLLOUTS_PER_TASK {
            return Err(HarnessError::Budget(format!(
                "rollout cap {MAX_ROLLOUTS_PER_TASK} exceeded"
            )));
        }
        if self.sessions() > MAX_SESSIONS_PER_TASK {
            return Err(HarnessError::Budget(format!(
                "improvement session cap {MAX_SESSIONS_PER_TASK} exceeded"
            )));
        }
        let outside =
            self.selection_sessions() + self.reflection_sessions() + self.retry_sessions();
        let outside_cap = 2 * MAX_K as u64 + 2 + MAX_K as u64;
        if outside > outside_cap {
            return Err(HarnessError::Budget(format!(
                "selection/reflection session cap {outside_cap} exceeded"
            )));
        }
        Ok(())
    }
}

/// Stable ledger report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerReport {
    /// Identity rollouts.
    pub rollouts: u64,
    /// Identity improvement sessions.
    pub sessions: u64,
    /// Selection sessions outside the identity.
    pub selection_sessions: u64,
    /// Validator retries outside the identity.
    pub retry_sessions: u64,
    /// Reflection sessions outside the identity.
    pub reflection_sessions: u64,
    /// Dead rollouts with reasons.
    pub dead_rollouts: Vec<DeadRollout>,
    /// Whether the base failed before improvement.
    pub base_failed: bool,
    /// Events in stable label order.
    pub events: Vec<LedgerEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollouts(ledger: &mut Ledger, k: usize) {
        for step in 0..k {
            for index in 0..ROLLOUTS_PER_STEP {
                ledger
                    .record(LedgerKind::Rollout, format!("r-{step}-{index}"), "probe")
                    .expect("records");
            }
        }
    }

    fn sessions(ledger: &mut Ledger, k: usize) {
        for step in 0..k {
            for index in 0..SESSIONS_PER_STEP {
                ledger
                    .record(LedgerKind::ImprovementSession, format!("s-{step}-{index}"), "improve")
                    .expect("records");
            }
        }
    }

    #[test]
    fn identity_holds_for_single_and_multi_step() {
        for k in [1usize, 2, 4] {
            let mut ledger = Ledger::new();
            rollouts(&mut ledger, k);
            sessions(&mut ledger, k);
            ledger.record(LedgerKind::SelectionSession, "select", "choice").expect("selection");
            ledger.record(LedgerKind::ReflectionSession, "reflect", "lesson").expect("reflection");
            let report = ledger.assert_identity(k).expect("identity");
            assert_eq!(report.rollouts, 4 * k as u64);
            assert_eq!(report.sessions, 3 * k as u64);
            assert_eq!(report.selection_sessions, 1);
            assert_eq!(report.reflection_sessions, 1);
            let labels: Vec<&str> =
                report.events.iter().map(|event| event.label.as_str()).collect();
            let mut sorted = labels.clone();
            sorted.sort_unstable();
            assert_eq!(labels, sorted, "events are stable label order");
        }
    }

    #[test]
    fn identity_failures_are_typed_and_duplicates_rejected() {
        let mut ledger = Ledger::new();
        rollouts(&mut ledger, 1);
        let error = ledger.assert_identity(1).expect_err("missing sessions");
        assert!(matches!(error, HarnessError::Budget(_)));

        ledger.record(LedgerKind::Rollout, "dup", "first").expect("records");
        assert!(ledger.record(LedgerKind::Rollout, "dup", "second").is_err());
    }

    #[test]
    fn merged_phase_ledgers_keep_the_identity_and_reject_duplicates() {
        let mut left = Ledger::new();
        let mut right = Ledger::new();
        left.record(LedgerKind::Rollout, "probe-0-inv", "delivered").expect("left");
        left.record(LedgerKind::ImprovementSession, "step-0-inv", "improved").expect("left");
        right.record(LedgerKind::Rollout, "probe-0-dom", "delivered").expect("right");
        right.record(LedgerKind::ImprovementSession, "step-0-dom", "improved").expect("right");
        right.dead_rollout("composed-0", "no deliverable").expect("dead");

        let mut merged = Ledger::new();
        merged.merge(left).expect("merge left");
        merged.merge(right).expect("merge right");
        assert_eq!(merged.rollouts(), 3);
        assert_eq!(merged.sessions(), 2);
        assert_eq!(merged.report().dead_rollouts[0].label, "composed-0");

        let mut duplicate = Ledger::new();
        duplicate
            .record(LedgerKind::Rollout, "probe-0-inv", "delivered")
            .expect("duplicate source");
        assert!(merged.merge(duplicate).is_err(), "labels stay unique across phases");
    }

    #[test]
    fn caps_and_dead_rollouts_are_recorded() {
        let mut ledger = Ledger::new();
        for index in 0..MAX_ROLLOUTS_PER_TASK {
            ledger.record(LedgerKind::Rollout, format!("r-{index}"), "probe").expect("within cap");
        }
        assert!(ledger.record(LedgerKind::Rollout, "over", "probe").is_err(), "hard cap enforced");

        let mut ledger = Ledger::new();
        ledger.dead_rollout("dead-1", "provider timeout").expect("dead");
        assert_eq!(ledger.rollouts(), 1);
        assert_eq!(ledger.report().dead_rollouts.len(), 1);
    }
}
