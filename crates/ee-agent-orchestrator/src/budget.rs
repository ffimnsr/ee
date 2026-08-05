//! Budget tracking for the loop engine.
//!
//! [`BudgetTracker`] centralizes the per-turn resource controls: loop
//! iterations, model calls, tool calls, subagent spawns, accumulated output
//! bytes, optional input/output token caps, and a wall-clock deadline.  Every
//! allowance is checked *before* the operation starts, so budget-denied work
//! is never invoked; usage is recorded *after* the operation completes.
//! Missing provider token usage is treated as unknown, not zero, so a
//! configured token budget fails closed when the adapter reports nothing.
//!
//! Every counter change can be surfaced as an [`OrchestratorEvent::BudgetUpdated`]
//! through [`BudgetTracker::emit`], keeping budget decisions observable for
//! tests and future UI display.

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};

/// Per-turn budget limits for one [`BudgetTracker`].
///
/// Constructed from [`OrchestratorConfig`] via [`BudgetConfig::from_config`],
/// which derives the wall-clock deadline from the configured turn timeout.
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    /// Maximum loop iterations per turn.
    pub max_iterations: usize,
    /// Maximum model adapter invocations per turn.
    pub max_model_calls: usize,
    /// Maximum tool calls per turn.
    pub max_tool_calls: usize,
    /// Maximum subagent spawns per turn across all depths.
    pub max_subagents: usize,
    /// Maximum accumulated model output bytes (text + reasoning) per turn.
    pub max_output_bytes: usize,
    /// Optional per-turn input-token cap; unknown usage fails closed.
    pub max_input_tokens: Option<usize>,
    /// Optional per-turn output-token cap; unknown usage fails closed.
    pub max_output_tokens: Option<usize>,
    /// Wall-clock deadline; reservations after it are denied.
    pub deadline: Option<Instant>,
}

impl BudgetConfig {
    /// Derives budget limits from an orchestrator config, anchoring the
    /// wall-clock deadline to the configured per-turn timeout.
    #[must_use]
    pub fn from_config(config: &OrchestratorConfig) -> Self {
        Self {
            max_iterations: config.max_loop_iterations,
            max_model_calls: config.max_model_calls,
            max_tool_calls: config.max_tool_calls_per_turn,
            max_subagents: config.max_subagents,
            max_output_bytes: config.max_output_bytes,
            max_input_tokens: config.max_input_tokens,
            max_output_tokens: config.max_output_tokens,
            deadline: Some(Instant::now() + config.turn_timeout),
        }
    }
}

/// Immutable snapshot of budget state included in model requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BudgetSnapshot {
    /// Loop iterations consumed so far.
    pub iterations_used: usize,
    /// Loop iteration cap.
    pub iterations_max: usize,
    /// Model calls consumed so far.
    pub model_calls_used: usize,
    /// Model-call cap per turn.
    pub model_calls_max: usize,
    /// Tool calls consumed so far.
    pub tool_calls_used: usize,
    /// Tool-call cap per turn.
    pub tool_calls_max: usize,
    /// Subagent spawns consumed so far.
    pub subagents_used: usize,
    /// Subagent-spawn cap per turn.
    pub subagents_max: usize,
    /// Accumulated model output bytes.
    pub output_bytes_used: usize,
    /// Accumulated output-byte cap.
    pub output_bytes_max: usize,
    /// Accumulated input tokens; `None` while unknown.
    pub input_tokens_used: Option<usize>,
    /// Optional per-turn input-token cap.
    pub input_tokens_max: Option<usize>,
    /// Accumulated output tokens; `None` while unknown.
    pub output_tokens_used: Option<usize>,
    /// Optional per-turn output-token cap.
    pub output_tokens_max: Option<usize>,
}

impl std::fmt::Display for BudgetSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let opt = |v: &Option<usize>| v.map_or_else(|| "?".to_string(), |n| n.to_string());
        write!(
            f,
            "iterations {}/{} model {}/{} tools {}/{} subagents {}/{} output {}B/{}B input_tokens {}/{} output_tokens {}/{}",
            self.iterations_used,
            self.iterations_max,
            self.model_calls_used,
            self.model_calls_max,
            self.tool_calls_used,
            self.tool_calls_max,
            self.subagents_used,
            self.subagents_max,
            self.output_bytes_used,
            self.output_bytes_max,
            opt(&self.input_tokens_used),
            opt(&self.input_tokens_max),
            opt(&self.output_tokens_used),
            opt(&self.output_tokens_max),
        )
    }
}

/// Mutable budget state; one instance per runtime (children get their own).
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    iterations_used: usize,
    model_calls_used: usize,
    tool_calls_used: usize,
    subagents_used: usize,
    output_bytes_used: usize,
    input_tokens_used: Option<usize>,
    output_tokens_used: Option<usize>,
    max_iterations: usize,
    max_model_calls: usize,
    max_tool_calls: usize,
    max_subagents: usize,
    max_output_bytes: usize,
    max_input_tokens: Option<usize>,
    max_output_tokens: Option<usize>,
    deadline: Option<Instant>,
}

impl BudgetTracker {
    /// Creates a tracker from the configured limits; the wall-clock deadline
    /// is anchored to the configured turn timeout.
    #[must_use]
    pub fn new(config: &OrchestratorConfig) -> Self {
        Self::with_config(BudgetConfig::from_config(config))
    }

    /// Creates a tracker from explicit budget limits.
    #[must_use]
    pub fn with_config(config: BudgetConfig) -> Self {
        Self {
            iterations_used: 0,
            model_calls_used: 0,
            tool_calls_used: 0,
            subagents_used: 0,
            output_bytes_used: 0,
            input_tokens_used: None,
            output_tokens_used: None,
            max_iterations: config.max_iterations,
            max_model_calls: config.max_model_calls,
            max_tool_calls: config.max_tool_calls,
            max_subagents: config.max_subagents,
            max_output_bytes: config.max_output_bytes,
            max_input_tokens: config.max_input_tokens,
            max_output_tokens: config.max_output_tokens,
            deadline: config.deadline,
        }
    }

    /// Records a [`OrchestratorEvent::BudgetUpdated`] with the current
    /// counters into `events`, keeping budget decisions observable.
    pub fn emit(&self, events: &EventRecorder) {
        let snapshot = self.snapshot();
        events.record(OrchestratorEvent::BudgetUpdated {
            iterations_used: snapshot.iterations_used,
            model_calls_used: snapshot.model_calls_used,
            tool_calls_used: snapshot.tool_calls_used,
            subagents_used: snapshot.subagents_used,
            output_bytes_used: snapshot.output_bytes_used,
        });
    }

    /// Reserves one loop iteration; fails when the iteration budget is spent
    /// or the wall-clock deadline has passed.
    pub fn try_reserve_iteration(&mut self) -> Result<(), OrchestratorError> {
        self.check_deadline()?;
        if self.iterations_used >= self.max_iterations {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "max loop iterations exceeded ({})",
                self.max_iterations
            )));
        }
        self.iterations_used += 1;
        Ok(())
    }

    /// Reserves one model call; fails when the model-call budget is spent or
    /// the wall-clock deadline has passed.  Called before every adapter
    /// invocation, so denied calls are never made.
    pub fn try_reserve_model_call(&mut self) -> Result<(), OrchestratorError> {
        self.check_deadline()?;
        if self.model_calls_used >= self.max_model_calls {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "max model calls exceeded ({})",
                self.max_model_calls
            )));
        }
        self.model_calls_used += 1;
        Ok(())
    }

    /// Reserves one tool call; fails when the per-turn tool budget is spent
    /// or the wall-clock deadline has passed.
    pub fn try_reserve_tool_call(&mut self) -> Result<(), OrchestratorError> {
        self.check_deadline()?;
        if self.tool_calls_used >= self.max_tool_calls {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "max tool calls per turn exceeded ({})",
                self.max_tool_calls
            )));
        }
        self.tool_calls_used += 1;
        Ok(())
    }

    /// Reserves one subagent spawn; fails when the per-turn subagent budget
    /// is spent or the wall-clock deadline has passed.  Called before the
    /// child task node is created, so denied spawns are never started.
    pub fn try_reserve_subagent(&mut self) -> Result<(), OrchestratorError> {
        self.check_deadline()?;
        if self.subagents_used >= self.max_subagents {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "max subagents per turn exceeded ({})",
                self.max_subagents
            )));
        }
        self.subagents_used += 1;
        Ok(())
    }

    /// Fails when no output-byte allowance remains; used before an operation
    /// that will produce output.  The actual bytes are recorded by
    /// [`BudgetTracker::record_model_usage`].
    pub fn check_output_allowance(&self) -> Result<(), OrchestratorError> {
        if self.output_bytes_used >= self.max_output_bytes {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "max output bytes exceeded ({})",
                self.max_output_bytes
            )));
        }
        Ok(())
    }

    /// Records one model completion's output bytes and (when reported) token
    /// usage.  Output bytes count against the per-turn byte cap; configured
    /// token caps fail closed when the provider reports no usage (`None` is
    /// unknown, not zero).  Unknown-but-unconfigured token usage is kept as
    /// informational state only.
    pub fn record_model_usage(
        &mut self,
        output_bytes: usize,
        input_tokens: Option<usize>,
        output_tokens: Option<usize>,
    ) -> Result<(), OrchestratorError> {
        self.output_bytes_used += output_bytes;
        if self.output_bytes_used > self.max_output_bytes {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "max output bytes exceeded (used {}, max {})",
                self.output_bytes_used, self.max_output_bytes
            )));
        }
        if let Some(max) = self.max_input_tokens {
            match input_tokens {
                Some(actual) => {
                    self.input_tokens_used = Some(self.input_tokens_used.unwrap_or(0) + actual);
                    if self.input_tokens_used.unwrap_or(0) > max {
                        return Err(OrchestratorError::BudgetExceeded(format!(
                            "max input tokens exceeded (used {}, max {max})",
                            self.input_tokens_used.unwrap_or(0)
                        )));
                    }
                }
                None => {
                    return Err(OrchestratorError::BudgetExceeded(
                        "input token usage unknown; cannot enforce max input tokens".into(),
                    ));
                }
            }
        } else if let Some(actual) = input_tokens {
            self.input_tokens_used = Some(self.input_tokens_used.unwrap_or(0) + actual);
        }
        if let Some(max) = self.max_output_tokens {
            match output_tokens {
                Some(actual) => {
                    self.output_tokens_used = Some(self.output_tokens_used.unwrap_or(0) + actual);
                    if self.output_tokens_used.unwrap_or(0) > max {
                        return Err(OrchestratorError::BudgetExceeded(format!(
                            "max output tokens exceeded (used {}, max {max})",
                            self.output_tokens_used.unwrap_or(0)
                        )));
                    }
                }
                None => {
                    return Err(OrchestratorError::BudgetExceeded(
                        "output token usage unknown; cannot enforce max output tokens".into(),
                    ));
                }
            }
        } else if let Some(actual) = output_tokens {
            self.output_tokens_used = Some(self.output_tokens_used.unwrap_or(0) + actual);
        }
        Ok(())
    }

    /// Fails when the wall-clock deadline has passed.
    pub fn check_deadline(&self) -> Result<(), OrchestratorError> {
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(OrchestratorError::BudgetExceeded("wall-clock deadline exceeded".into()));
        }
        Ok(())
    }

    /// Restores usage counters from a snapshot (checkpoint restore).
    ///
    /// Fails closed when the snapshot's caps differ from this tracker's
    /// configured caps or when any used counter exceeds its cap, so a
    /// checkpoint built under different limits is never resumed silently.
    pub fn restore_used(&mut self, used: &BudgetSnapshot) -> Result<(), OrchestratorError> {
        let caps = [
            (used.iterations_max, self.max_iterations, "iterations"),
            (used.model_calls_max, self.max_model_calls, "model calls"),
            (used.tool_calls_max, self.max_tool_calls, "tool calls"),
            (used.subagents_max, self.max_subagents, "subagents"),
            (used.output_bytes_max, self.max_output_bytes, "output bytes"),
        ];
        for (snapshot_max, tracker_max, name) in caps {
            if snapshot_max != tracker_max {
                return Err(OrchestratorError::InvalidState(format!(
                    "budget snapshot {name} cap {snapshot_max:?} differs from configured {tracker_max:?}"
                )));
            }
        }
        if used.input_tokens_max != self.max_input_tokens
            || used.output_tokens_max != self.max_output_tokens
        {
            return Err(OrchestratorError::InvalidState(
                "budget snapshot token caps differ from configured caps".into(),
            ));
        }
        let used_values = [
            (used.iterations_used, self.max_iterations, "iterations"),
            (used.model_calls_used, self.max_model_calls, "model calls"),
            (used.tool_calls_used, self.max_tool_calls, "tool calls"),
            (used.subagents_used, self.max_subagents, "subagents"),
            (used.output_bytes_used, self.max_output_bytes, "output bytes"),
        ];
        for (used_value, max_value, name) in used_values {
            if used_value > max_value {
                return Err(OrchestratorError::InvalidState(format!(
                    "budget snapshot {name} used {used_value} exceeds cap {max_value}"
                )));
            }
        }
        if let Some(used_value) = used.input_tokens_used
            && used_value > self.max_input_tokens.unwrap_or(usize::MAX)
        {
            return Err(OrchestratorError::InvalidState(format!(
                "budget snapshot input tokens used {used_value} exceeds cap"
            )));
        }
        if let Some(used_value) = used.output_tokens_used
            && used_value > self.max_output_tokens.unwrap_or(usize::MAX)
        {
            return Err(OrchestratorError::InvalidState(format!(
                "budget snapshot output tokens used {used_value} exceeds cap"
            )));
        }
        self.iterations_used = used.iterations_used;
        self.model_calls_used = used.model_calls_used;
        self.tool_calls_used = used.tool_calls_used;
        self.subagents_used = used.subagents_used;
        self.output_bytes_used = used.output_bytes_used;
        self.input_tokens_used = used.input_tokens_used;
        self.output_tokens_used = used.output_tokens_used;
        Ok(())
    }

    /// Snapshot of the current budget state.
    #[must_use]
    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            iterations_used: self.iterations_used,
            iterations_max: self.max_iterations,
            model_calls_used: self.model_calls_used,
            model_calls_max: self.max_model_calls,
            tool_calls_used: self.tool_calls_used,
            tool_calls_max: self.max_tool_calls,
            subagents_used: self.subagents_used,
            subagents_max: self.max_subagents,
            output_bytes_used: self.output_bytes_used,
            output_bytes_max: self.max_output_bytes,
            input_tokens_used: self.input_tokens_used,
            input_tokens_max: self.max_input_tokens,
            output_tokens_used: self.output_tokens_used,
            output_tokens_max: self.max_output_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

    use super::*;
    use crate::events::EventRecorder;

    fn config_with(iterations: usize, tools: usize) -> OrchestratorConfig {
        OrchestratorConfig {
            max_loop_iterations: iterations,
            max_tool_calls_per_turn: tools,
            ..OrchestratorConfig::default()
        }
    }

    #[test]
    fn iteration_budget_is_enforced() {
        let mut tracker = BudgetTracker::new(&config_with(2, 32));
        tracker.try_reserve_iteration().expect("first");
        tracker.try_reserve_iteration().expect("second");
        let error = tracker.try_reserve_iteration().expect_err("spent");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref r) if r.contains("max loop iterations"))
        );
        assert_eq!(tracker.snapshot().iterations_used, 2);
    }

    #[test]
    fn tool_call_budget_is_enforced() {
        let mut tracker = BudgetTracker::new(&config_with(16, 2));
        tracker.try_reserve_tool_call().expect("first");
        tracker.try_reserve_tool_call().expect("second");
        let error = tracker.try_reserve_tool_call().expect_err("spent");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref r) if r.contains("max tool calls"))
        );
        assert_eq!(tracker.snapshot().tool_calls_used, 2);
    }

    #[test]
    fn model_call_budget_is_enforced() {
        let config = OrchestratorConfig { max_model_calls: 2, ..OrchestratorConfig::default() };
        let mut tracker = BudgetTracker::new(&config);
        tracker.try_reserve_model_call().expect("first");
        tracker.try_reserve_model_call().expect("second");
        let error = tracker.try_reserve_model_call().expect_err("spent");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref r) if r.contains("max model calls"))
        );
        assert_eq!(tracker.snapshot().model_calls_used, 2);
        assert_eq!(tracker.snapshot().model_calls_max, 2);
    }

    #[test]
    fn subagent_budget_is_enforced() {
        let config = OrchestratorConfig { max_subagents: 2, ..OrchestratorConfig::default() };
        let mut tracker = BudgetTracker::new(&config);
        tracker.try_reserve_subagent().expect("first");
        tracker.try_reserve_subagent().expect("second");
        let error = tracker.try_reserve_subagent().expect_err("spent");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref r) if r.contains("max subagents"))
        );
        assert_eq!(tracker.snapshot().subagents_used, 2);
        assert_eq!(tracker.snapshot().subagents_max, 2);
    }

    #[test]
    fn output_byte_budget_is_enforced_on_record() {
        let config = OrchestratorConfig { max_output_bytes: 50, ..OrchestratorConfig::default() };
        let mut tracker = BudgetTracker::new(&config);
        tracker.check_output_allowance().expect("allowance remains");
        tracker.record_model_usage(30, None, None).expect("within budget");
        tracker.check_output_allowance().expect("allowance remains");
        let error = tracker.record_model_usage(30, None, None).expect_err("over budget");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref r) if r.contains("max output bytes"))
        );
        assert_eq!(tracker.snapshot().output_bytes_used, 60);
        assert_eq!(tracker.snapshot().output_bytes_max, 50);
        assert!(
            tracker.check_output_allowance().is_err(),
            "no allowance remains once the cap is consumed"
        );
    }

    #[test]
    fn token_budget_fails_closed_on_unknown_usage() {
        let config = OrchestratorConfig {
            max_input_tokens: Some(100),
            max_output_tokens: Some(200),
            ..OrchestratorConfig::default()
        };
        let mut tracker = BudgetTracker::new(&config);
        let error = tracker.record_model_usage(10, None, None).expect_err("unknown usage");
        assert!(error.to_string().contains("unknown"), "{error}");
        assert_eq!(tracker.snapshot().input_tokens_used, None, "unknown stays unknown");
    }

    #[test]
    fn token_budget_tracks_reported_usage_and_denies_overage() {
        let config = OrchestratorConfig {
            max_input_tokens: Some(100),
            max_output_tokens: Some(200),
            ..OrchestratorConfig::default()
        };
        let mut tracker = BudgetTracker::new(&config);
        tracker.record_model_usage(10, Some(50), Some(100)).expect("within token budgets");
        tracker.record_model_usage(10, Some(50), Some(100)).expect("exactly at caps");
        let error = tracker.record_model_usage(10, Some(10), Some(10)).expect_err("over input cap");
        assert!(error.to_string().contains("max input tokens"), "{error}");
        assert_eq!(tracker.snapshot().input_tokens_used, Some(110));
        // The third record aborts at the input cap, so the extra output
        // tokens are not accumulated.
        assert_eq!(tracker.snapshot().output_tokens_used, Some(200));
    }

    #[test]
    fn unknown_tokens_without_budget_are_informational_only() {
        let mut tracker = BudgetTracker::new(&OrchestratorConfig::default());
        tracker.record_model_usage(10, Some(5), Some(7)).expect("no token caps configured");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.input_tokens_used, Some(5));
        assert_eq!(snapshot.output_tokens_used, Some(7));
        assert_eq!(snapshot.input_tokens_max, None);
        assert_eq!(snapshot.output_tokens_max, None);
    }

    #[tokio::test(start_paused = true)]
    async fn wall_clock_deadline_denies_reservations_after_it_passes() {
        let config = BudgetConfig {
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            ..BudgetConfig::from_config(&OrchestratorConfig::default())
        };
        let mut tracker = BudgetTracker::with_config(config);
        tracker.try_reserve_model_call().expect("before deadline");
        tokio::time::advance(Duration::from_secs(11)).await;
        let error = tracker.try_reserve_model_call().expect_err("deadline passed");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref r) if r.contains("deadline"))
        );
        assert!(tracker.try_reserve_tool_call().is_err(), "all reservations share the deadline");
        assert!(tracker.try_reserve_subagent().is_err(), "all reservations share the deadline");
    }

    #[tokio::test(start_paused = true)]
    async fn from_config_anchors_deadline_to_turn_timeout() {
        let config = OrchestratorConfig {
            turn_timeout: Duration::from_secs(5),
            ..OrchestratorConfig::default()
        };
        let mut tracker = BudgetTracker::new(&config);
        tracker.try_reserve_model_call().expect("before deadline");
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(tracker.try_reserve_model_call().is_err(), "deadline derived from turn_timeout");
    }

    #[test]
    fn snapshot_reflects_usage_and_caps() {
        let mut tracker = BudgetTracker::new(&config_with(16, 32));
        tracker.try_reserve_iteration().expect("reserves");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.iterations_used, 1);
        assert_eq!(snapshot.iterations_max, 16);
        assert_eq!(snapshot.tool_calls_used, 0);
        assert_eq!(snapshot.tool_calls_max, 32);
    }

    #[test]
    fn emit_records_budget_updated_event() {
        let mut tracker = BudgetTracker::new(&config_with(16, 32));
        let events = EventRecorder::new();
        tracker.try_reserve_iteration().expect("reserves");
        tracker.try_reserve_model_call().expect("reserves");
        tracker.emit(&events);
        let recorded = events.events();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0],
            OrchestratorEvent::BudgetUpdated {
                iterations_used: 1,
                model_calls_used: 1,
                tool_calls_used: 0,
                subagents_used: 0,
                output_bytes_used: 0,
            }
        );
    }
}
