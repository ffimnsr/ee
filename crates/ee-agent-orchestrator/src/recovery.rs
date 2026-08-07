//! Recoverable turn outcomes and interruption building.
//!
//! With recovery enabled ([`RecoveryConfig::enabled`](crate::config::RecoveryConfig)),
//! a deadline or timeout stop becomes a [`TurnOutcome::Interrupted`] carrying
//! a [`RecoverableInterruption`] instead of a fatal error: completed work is
//! durable in a checkpoint and the same session may resume.  Interruptions
//! map onto the wire type
//! [`RecoverableError`](ee_agent_protocol::RecoverableError) so hosts can
//! offer Resume/Discard without parsing error strings.

use std::time::Duration;

use ee_acp_agent_server::PromptResult;
use ee_agent_protocol::{RecoverableError, RecoverableFault};

use crate::checkpoint::{OrchestratorCheckpoint, current_unix_millis};
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;

/// Outcome of one recoverable turn run.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// The turn completed with an ACP prompt result.
    Completed(PromptResult),
    /// The turn stopped on a recoverable interruption; a checkpoint (when
    /// persisted) allows resuming on the same session.
    Interrupted(RecoverableInterruption),
}

/// A recoverable stop: why the turn stopped, whether resuming is safe, and
/// where the durable checkpoint lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableInterruption {
    /// Why the turn stopped.
    pub fault: RecoverableFault,
    /// Human-readable summary.
    pub detail: String,
    /// Underlying cause, when known.
    pub cause: Option<String>,
    /// Whether the checkpoint is resumable without user confirmation.
    pub safe_resume: bool,
    /// Server-provided retry hint, when known.
    pub retry_after: Option<Duration>,
    /// Durable checkpoint identity, when one was persisted.
    pub checkpoint_id: Option<String>,
    /// Tool calls whose results are durable in the checkpoint.
    pub completed_tool_calls: u64,
    /// How many times this turn has already been resumed.
    pub resumed_count: u32,
}

impl RecoverableInterruption {
    /// Builds an interruption from a persisted checkpoint, deriving the
    /// safe-resume flag from the checkpoint's in-flight marker and the fault
    /// class.  Without a checkpoint, resuming is never safe (nothing is
    /// durable).
    #[must_use]
    pub fn from_checkpoint(
        fault: RecoverableFault,
        detail: impl Into<String>,
        cause: Option<String>,
        retry_after: Option<Duration>,
        checkpoint: Option<(&str, &OrchestratorCheckpoint)>,
    ) -> Self {
        let (completed_tool_calls, resumed_count, safe_resume) = match checkpoint {
            Some((_id, checkpoint)) => checkpoint.resume.as_ref().map_or((0, 0, false), |resume| {
                let safe = fault.is_safe_to_resume()
                    && resume.in_flight.is_none()
                    && !resume.transcript.is_empty();
                (resume.completed_tools.len() as u64, resume.resumed_count, safe)
            }),
            None => (0, 0, false),
        };
        Self {
            fault,
            detail: detail.into(),
            cause,
            safe_resume,
            retry_after,
            checkpoint_id: checkpoint.map(|(id, _)| id.to_string()),
            completed_tool_calls,
            resumed_count,
        }
    }

    /// Converts into the wire payload hosts receive in JSON-RPC error `data`.
    #[must_use]
    pub fn into_wire(self) -> RecoverableError {
        RecoverableError::new(self.fault, self.detail)
            .with_cause_if(self.cause)
            .with_safe_resume(self.safe_resume)
            .with_retry_after_if(self.retry_after)
            .with_checkpoint_id_if(self.checkpoint_id)
            .with_counts(self.completed_tool_calls, self.resumed_count)
    }
}

/// Extensions used to assemble interruption payloads without conditionals at
/// call sites.
trait RecoverableErrorExt {
    fn with_cause_if(self, cause: Option<String>) -> Self;
    fn with_retry_after_if(self, retry_after: Option<Duration>) -> Self;
    fn with_checkpoint_id_if(self, id: Option<String>) -> Self;
}

impl RecoverableErrorExt for RecoverableError {
    fn with_cause_if(self, cause: Option<String>) -> Self {
        match cause {
            Some(cause) => self.with_cause(cause),
            None => self,
        }
    }

    fn with_retry_after_if(self, retry_after: Option<Duration>) -> Self {
        match retry_after {
            Some(duration) => self.with_retry_after(duration.as_millis() as u64),
            None => self,
        }
    }

    fn with_checkpoint_id_if(self, id: Option<String>) -> Self {
        match id {
            Some(id) => self.with_checkpoint_id(id),
            None => self,
        }
    }
}

/// Whether the cumulative session cap (`session_timeout`) has expired for a
/// checkpoint captured at `captured_at_millis`.
#[must_use]
pub fn session_timeout_expired(
    config: &OrchestratorConfig,
    first_started_at_millis: u64,
    now_millis: u64,
) -> bool {
    config.recovery.session_timeout.is_some_and(|limit| {
        let elapsed = now_millis.saturating_sub(first_started_at_millis);
        elapsed >= limit.as_millis() as u64
    })
}

/// Current wall-clock millis (delegates to [`current_unix_millis`]).
#[must_use]
pub fn now_millis() -> u64 {
    current_unix_millis()
}

impl From<RecoverableInterruption> for OrchestratorError {
    fn from(interruption: RecoverableInterruption) -> Self {
        OrchestratorError::DeadlineExceeded(interruption.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::TranscriptSummary;
    use crate::memory::MemoryStore;
    use crate::tasks::TaskGraph;

    fn checkpoint_with(resume: Option<crate::checkpoint::ResumeState>) -> OrchestratorCheckpoint {
        let config = OrchestratorConfig::default();
        let mut tasks = TaskGraph::new();
        let _root = tasks.create_root("plan", "plan");
        let memory = MemoryStore::new(config.memory_limit_bytes);
        let transcript = vec![];
        OrchestratorCheckpoint::with_recovery(
            "test",
            config,
            "s-1",
            tasks,
            memory,
            TranscriptSummary::from_transcript(&transcript),
            crate::budget::BudgetSnapshot {
                iterations_used: 0,
                iterations_max: 16,
                model_calls_used: 0,
                model_calls_max: 16,
                tool_calls_used: 0,
                tool_calls_max: 180,
                subagents_used: 0,
                subagents_max: 8,
                output_bytes_used: 0,
                output_bytes_max: 1024 * 1024,
                input_tokens_used: None,
                input_tokens_max: None,
                output_tokens_used: None,
                output_tokens_max: None,
            },
            crate::checkpoint::SubagentTreeState::new(),
            crate::checkpoint::IdGeneratorState::new(),
            "provider",
            1,
            resume,
        )
        .expect("checkpoint valid")
    }

    fn resume_state(
        completed: usize,
        in_flight: bool,
        task_id: &str,
    ) -> crate::checkpoint::ResumeState {
        use crate::checkpoint::{CompletedToolCall, InFlightOperation};
        use crate::model::{ModelMessage, ModelRole};
        let mut completed_tools = Vec::new();
        for index in 0..completed {
            completed_tools.push(CompletedToolCall {
                tool_call_id: format!("tc-{index}"),
                tool_name: "read_file".into(),
                arguments: serde_json::json!({ "path": "/work/a.txt" }),
                success: true,
                summary: "a.txt: content".into(),
                side_effect_class: crate::tools::SideEffectClass::Read,
            });
        }
        crate::checkpoint::ResumeState {
            transcript: vec![ModelMessage::text(ModelRole::User, "hello")],
            active_task_id: task_id.to_string(),
            completed_tools,
            in_flight: in_flight.then(|| InFlightOperation {
                tool_call_id: "tc-99".into(),
                tool_name: "write_file".into(),
                started_at_millis: 1,
            }),
            resumed_count: 1,
            first_started_at_millis: 1_699_000_000_000,
        }
    }

    #[test]
    fn interruption_without_checkpoint_is_never_safe() {
        let interruption = RecoverableInterruption::from_checkpoint(
            RecoverableFault::Deadline,
            "paused",
            None,
            None,
            None,
        );
        assert!(!interruption.safe_resume);
        assert_eq!(interruption.checkpoint_id, None);
        assert_eq!(interruption.completed_tool_calls, 0);
    }

    #[test]
    fn interruption_derives_safety_from_checkpoint_and_fault() {
        let base = checkpoint_with(None);
        let checkpoint =
            checkpoint_with(Some(resume_state(4, false, base.tasks.list()[0].id.as_str())));
        let interruption = RecoverableInterruption::from_checkpoint(
            RecoverableFault::Deadline,
            "paused",
            None,
            None,
            Some(("ck-1", &checkpoint)),
        );
        assert!(interruption.safe_resume);
        assert_eq!(interruption.checkpoint_id.as_deref(), Some("ck-1"));
        assert_eq!(interruption.completed_tool_calls, 4);
        assert_eq!(interruption.resumed_count, 1);
    }

    #[test]
    fn in_flight_operation_blocks_safe_resume() {
        let base = checkpoint_with(None);
        let checkpoint =
            checkpoint_with(Some(resume_state(2, true, base.tasks.list()[0].id.as_str())));
        let interruption = RecoverableInterruption::from_checkpoint(
            RecoverableFault::Deadline,
            "paused",
            None,
            None,
            Some(("ck-1", &checkpoint)),
        );
        assert!(!interruption.safe_resume, "ambiguous in-flight tool blocks auto-resume");
    }

    #[test]
    fn permanent_faults_are_never_safe_even_with_checkpoint() {
        let base = checkpoint_with(None);
        let checkpoint =
            checkpoint_with(Some(resume_state(0, false, base.tasks.list()[0].id.as_str())));
        for fault in
            [RecoverableFault::Auth, RecoverableFault::Policy, RecoverableFault::InvalidRequest]
        {
            let interruption = RecoverableInterruption::from_checkpoint(
                fault,
                "stopped",
                None,
                None,
                Some(("ck-1", &checkpoint)),
            );
            assert!(!interruption.safe_resume, "{fault:?} must never auto-resume");
        }
    }

    #[test]
    fn wire_payload_roundtrips_all_fields() {
        let interruption = RecoverableInterruption {
            fault: RecoverableFault::Deadline,
            detail: "paused after 300s".into(),
            cause: Some("wall clock".into()),
            safe_resume: true,
            retry_after: Some(Duration::from_secs(5)),
            checkpoint_id: Some("ck-9".into()),
            completed_tool_calls: 3,
            resumed_count: 2,
        };
        let wire = interruption.into_wire();
        assert_eq!(wire.fault, RecoverableFault::Deadline);
        assert_eq!(wire.retry_after, Some(5_000));
        assert_eq!(wire.checkpoint_id.as_deref(), Some("ck-9"));
        assert_eq!(wire.completed_tool_calls, 3);
        let json = serde_json::to_string(&wire).expect("serializes");
        let parsed: RecoverableError = serde_json::from_str(&json).expect("parses");
        assert_eq!(parsed, wire);
    }

    #[test]
    fn session_timeout_cap_is_enforced() {
        let config = OrchestratorConfig {
            recovery: crate::config::RecoveryConfig {
                session_timeout: Some(Duration::from_secs(600)),
                ..crate::config::RecoveryConfig::default()
            },
            ..OrchestratorConfig::default()
        };
        assert!(!session_timeout_expired(&config, 1_000, 1_000 + 599_000));
        assert!(session_timeout_expired(&config, 1_000, 1_000 + 601_000));
        let no_cap = OrchestratorConfig::default();
        assert!(!session_timeout_expired(&no_cap, 0, u64::MAX));
    }
}
