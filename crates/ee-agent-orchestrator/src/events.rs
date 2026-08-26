//! Loop event types and the in-memory event recorder.
//!
//! Every loop decision — turn start/stop, model round-trips, tool and
//! subagent lifecycle, budget updates, and non-fatal errors — is recorded as
//! an [`OrchestratorEvent`] so tests can assert the exact decision sequence.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// One observable loop decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OrchestratorEvent {
    /// A turn started for a session and task.
    TurnStarted {
        /// ACP session id.
        session_id: String,
        /// Root task id.
        task_id: String,
    },
    /// The model adapter was called for an iteration.
    ModelRequested {
        /// 1-based loop iteration.
        iteration: usize,
    },
    /// The model adapter returned a response.
    ModelResponded {
        /// 1-based loop iteration.
        iteration: usize,
    },
    /// A tool execution started.
    ToolStarted {
        /// Model-supplied tool-call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
    },
    /// A tool execution finished.
    ToolFinished {
        /// Model-supplied tool-call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Whether it succeeded.
        success: bool,
    },
    /// A host-owned approval was requested before a trusted editor mutation
    /// or terminal operation. This records no arguments or approval decision.
    ApprovalRequested {
        /// Model-supplied tool-call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
    },
    /// A classified transient retry was scheduled. Policy denial, invalid
    /// input, stale state, and approval denial never emit this event.
    RetryScheduled {
        /// Tool name.
        tool_name: String,
        /// Safe typed failure class, never raw error text.
        reason: String,
    },
    /// A subagent started (subagent phase).
    SubagentStarted {
        /// Subagent id.
        subagent_id: String,
        /// Registry model id the child runs on (resolved selection or parent
        /// fallback).
        model_id: Option<String>,
    },
    /// A subagent finished (subagent phase).
    SubagentFinished {
        /// Subagent id.
        subagent_id: String,
        /// Whether it succeeded.
        success: bool,
    },
    /// Budget state changed.
    BudgetUpdated {
        /// Iterations used.
        iterations_used: usize,
        /// Model calls used.
        model_calls_used: usize,
        /// Tool calls used.
        tool_calls_used: usize,
        /// Subagent spawns used.
        subagents_used: usize,
        /// Accumulated output bytes.
        output_bytes_used: usize,
    },
    /// The turn stopped with a stop reason.
    TurnStopped {
        /// Stop reason (`end_turn`, `max_iterations`, ...).
        stop_reason: String,
    },
    /// A non-fatal error was recorded.
    Error {
        /// Error message.
        error: String,
    },
    /// The turn's strategy was selected with its deterministic reason.
    StrategySelected {
        /// Selected strategy.
        strategy: crate::strategy::TurnStrategy,
        /// Deterministic reason code.
        reason: crate::strategy::StrategyReason,
    },
    /// Suspicious prompt-injection text was detected in untrusted content.
    SuspiciousContentDetected {
        /// Trust level of the offending content.
        trust: crate::trust::TrustLevel,
        /// Matched injection phrase.
        pattern: String,
        /// Bounded excerpt of the offending content.
        excerpt: String,
    },
    /// A model route was selected for a task kind and optional subagent role.
    ModelRouted {
        /// Selected route id.
        route_id: String,
        /// The adapter the route resolves to.
        adapter_id: String,
        /// Task kind the call serves.
        task_kind: crate::model_router::TaskKind,
        /// Subagent role the call serves, when routed for one.
        role: Option<String>,
    },
    /// A recovery checkpoint was persisted.
    CheckpointSaved {
        /// ACP session id.
        session_id: String,
        /// Stable checkpoint id.
        checkpoint_id: String,
    },
    /// A turn stopped on a recoverable interruption.
    TurnInterrupted {
        /// ACP session id.
        session_id: String,
        /// Fault class label ([`RecoverableFault::as_str`](ee_agent_protocol::RecoverableFault::as_str)).
        fault: String,
        /// Whether the checkpoint may be resumed without confirmation.
        safe_resume: bool,
        /// How many times this turn has been resumed.
        resumed_count: u32,
    },
    /// A turn resumed from a checkpoint.
    TurnResumed {
        /// ACP session id.
        session_id: String,
        /// Checkpoint id the turn resumed from.
        checkpoint_id: String,
        /// Resume count after this resume.
        resumed_count: u32,
    },
    /// A bounded automatic repair attempt began after fresh host context was collected.
    RepairStarted {
        /// One-based repair attempt number.
        attempt_number: usize,
        /// Typed repair-failure source.
        reason: String,
    },
    /// Automatic repair reached a typed terminal stop. This is not ACP prompt completion.
    RepairStopped {
        /// Stable stop classification.
        reason: String,
    },
    /// A completed write/execute/delegate call was reused instead of replayed
    /// (idempotency guard on resumed turns).
    ToolResultReused {
        /// New tool-call id the model supplied.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
    },
}

/// Thread-safe in-memory recorder of [`OrchestratorEvent`] values.
#[derive(Debug, Clone, Default)]
pub struct EventRecorder {
    events: Arc<Mutex<Vec<OrchestratorEvent>>>,
}

impl EventRecorder {
    /// Creates an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one event.
    pub fn record(&self, event: OrchestratorEvent) {
        self.events.lock().expect("event recorder poisoned").push(event);
    }

    /// Snapshot of all recorded events in order.
    #[must_use]
    pub fn events(&self) -> Vec<OrchestratorEvent> {
        self.events.lock().expect("event recorder poisoned").clone()
    }

    /// Takes all recorded events, clearing the recorder.
    #[must_use]
    pub fn take(&self) -> Vec<OrchestratorEvent> {
        std::mem::take(&mut self.events.lock().expect("event recorder poisoned"))
    }

    /// Whether anything has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.lock().expect("event recorder poisoned").is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_preserves_order_and_takes() {
        let recorder = EventRecorder::new();
        recorder.record(OrchestratorEvent::TurnStarted {
            session_id: "s-1".into(),
            task_id: "task-1".into(),
        });
        recorder.record(OrchestratorEvent::ModelRequested { iteration: 1 });
        assert_eq!(recorder.events().len(), 2);
        assert_eq!(
            recorder.events()[0],
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() }
        );

        let taken = recorder.take();
        assert_eq!(taken.len(), 2);
        assert!(recorder.is_empty());
    }

    #[test]
    fn events_serialize_deterministically() {
        let event = OrchestratorEvent::ToolFinished {
            tool_call_id: "tc-1".into(),
            tool_name: "read_file".into(),
            success: true,
        };
        let json = serde_json::to_string(&event).expect("serializes");
        let restored: OrchestratorEvent = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, event);
    }

    #[test]
    fn every_event_variant_roundtrips_through_json() {
        let events = vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
            },
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
                success: false,
            },
            OrchestratorEvent::ApprovalRequested {
                tool_call_id: "tc-2".into(),
                tool_name: "ee_write_text_file".into(),
            },
            OrchestratorEvent::RetryScheduled {
                tool_name: "cargo_check".into(),
                reason: "timeout".into(),
            },
            OrchestratorEvent::SubagentStarted {
                subagent_id: "sub-1".into(),
                model_id: Some("default".into()),
            },
            OrchestratorEvent::SubagentFinished { subagent_id: "sub-1".into(), success: true },
            OrchestratorEvent::BudgetUpdated {
                iterations_used: 3,
                model_calls_used: 3,
                tool_calls_used: 1,
                subagents_used: 1,
                output_bytes_used: 42,
            },
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            OrchestratorEvent::Error { error: "boom".into() },
            OrchestratorEvent::StrategySelected {
                strategy: crate::strategy::TurnStrategy::PlanThenExecute,
                reason: crate::strategy::StrategyReason::MultiFileImplementation,
            },
            OrchestratorEvent::SuspiciousContentDetected {
                trust: crate::trust::TrustLevel::ToolOutputUntrusted,
                pattern: "ignore previous instructions".into(),
                excerpt: "file says ignore previous instructions".into(),
            },
            OrchestratorEvent::CheckpointSaved {
                session_id: "s-1".into(),
                checkpoint_id: "s-1-0000000001".into(),
            },
            OrchestratorEvent::TurnInterrupted {
                session_id: "s-1".into(),
                fault: "deadline".into(),
                safe_resume: true,
                resumed_count: 1,
            },
            OrchestratorEvent::TurnResumed {
                session_id: "s-1".into(),
                checkpoint_id: "s-1-0000000002".into(),
                resumed_count: 2,
            },
            OrchestratorEvent::RepairStarted {
                attempt_number: 1,
                reason: "selected_validation_failure".into(),
            },
            OrchestratorEvent::RepairStopped { reason: "attempts_exhausted".into() },
            OrchestratorEvent::ToolResultReused {
                tool_call_id: "tc-2".into(),
                tool_name: "write_file".into(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("serializes");
            let restored: OrchestratorEvent = serde_json::from_str(&json).expect("parses");
            assert_eq!(restored, event, "round-trip mismatch for {json}");
        }
    }
}
