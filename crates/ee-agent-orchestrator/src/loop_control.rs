//! Loop termination and recovery policy.
//!
//! The loop engine owns the mechanics, but the *policy* lives here so it can be
//! unit-tested without a model or a tool registry:
//!
//! - [`MAX_STEPS_PROMPT`] is the text injected when the iteration budget is
//!   exhausted. The reference agent makes one final text-only call with tools
//!   disabled instead of failing the turn, so the user always receives a
//!   summary of completed and remaining work.
//! - [`RetryKind`] names *what* a retry may reuse. A request that never reached
//!   the provider is safe to send again from scratch; a request that already
//!   produced output can only be resumed.
//! - [`validate_finish_reason`] rejects provider stop reasons the loop cannot
//!   interpret instead of silently treating them as a normal completion.
//!
//! Reference: `packages/core/src/session/runner/{max-steps,step}.ts`.

use crate::error::OrchestratorError;

/// Instruction injected for the final text-only call when the step limit is
/// reached. Verbatim from the reference agent so behaviour matches.
pub const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.";

/// How much of a failed attempt a retry may reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// The request never reached the provider (or produced no output), so the
    /// whole request is replayed and a fresh assistant message is started.
    Fresh,
    /// Output already streamed; only the remaining work may be resumed.
    Resume,
    /// The credential was rejected; rotate before replaying the full request.
    ///
    /// Reserved: no model registry exposes alternate credentials yet, so the
    /// loop never emits this kind today.
    Rotate,
}

/// What one loop step decided before the next iteration.
///
/// The loop engine currently implements the transitions inline; the enum keeps
/// the vocabulary explicit and gives tests one place to assert on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step finished; `needs_continuation` mirrors the provider signal.
    Completed {
        /// Whether the model asked for another step.
        needs_continuation: bool,
    },
    /// The output bound cut the reply; continue with the bounded notice.
    OutputLimit {
        /// Whether tool work remains for a following step.
        needs_continuation: bool,
    },
    /// The prompt overflowed; the transcript was compacted and the step retried
    /// with a fresh assistant message.
    Compacted,
    /// A transport failure before any output; replay the full request once.
    RecoverFull,
    /// The step failed after output started; retry according to `kind`.
    Retry {
        /// What the retry may reuse.
        kind: RetryKind,
    },
}

/// Finish reasons the loop understands.
///
/// Dialect codecs normalize provider stop reasons into a small canonical set;
/// anything else is an unhandled provider state. Values are matched
/// case-insensitively.
const KNOWN_FINISH_REASONS: &[&str] = &[
    // Canonical stop signals.
    "stop",
    "end_turn",
    "length",
    "max_tokens",
    "max_output_tokens",
    "tool_calls",
    "tool_use",
    "function_call",
    // Provider-specific terminal reasons the codecs pass through.
    "content_filter",
    "content-filter",
    "stop_sequence",
    "refusal",
    "pause_turn",
    "incomplete",
    "failed",
    "cancelled",
    "canceled",
    "expired",
    "error",
    "guardrail",
    "safety",
    "rejected",
    "model_context_window_exceeded",
];

/// Whether `reason` is a stop reason the loop can act on.
#[must_use]
pub fn is_known_finish_reason(reason: &str) -> bool {
    let reason = reason.trim().to_ascii_lowercase();
    !reason.is_empty() && KNOWN_FINISH_REASONS.contains(&reason.as_str())
}

/// Rejects a provider stop reason the loop cannot interpret.
///
/// A missing reason is acceptable: adapters that do not report one keep the
/// historical "treat as natural end" behavior. An explicit unknown reason is a
/// contract violation and must not be counted as success.
///
/// # Errors
///
/// Returns [`OrchestratorError::InvalidProviderOutput`] naming the model when
/// the reason is present but unrecognized.
pub fn validate_finish_reason(
    model_id: Option<&str>,
    reason: Option<&str>,
) -> Result<(), OrchestratorError> {
    let Some(reason) = reason else {
        return Ok(());
    };
    if is_known_finish_reason(reason) {
        return Ok(());
    }
    Err(OrchestratorError::InvalidProviderOutput(format!(
        "model {} ended with an unknown finish reason {reason:?}",
        model_id.unwrap_or("unknown")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_finish_reasons_are_accepted() {
        for reason in ["stop", "LENGTH", "tool_calls", "end_turn", "max_tokens"] {
            assert!(validate_finish_reason(Some("m"), Some(reason)).is_ok(), "{reason}");
        }
    }

    #[test]
    fn missing_finish_reason_is_allowed() {
        assert!(validate_finish_reason(Some("m"), None).is_ok());
    }

    #[test]
    fn unknown_finish_reason_names_the_model() {
        let error = validate_finish_reason(Some("gpt-9"), Some("wat")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("gpt-9"), "{message}");
        assert!(message.contains("wat"), "{message}");
        assert!(matches!(error, OrchestratorError::InvalidProviderOutput(_)));
    }

    #[test]
    fn empty_finish_reason_is_rejected() {
        assert!(validate_finish_reason(Some("m"), Some("  ")).is_err());
    }

    #[test]
    fn step_outcomes_are_copy_and_explicit() {
        let outcome = StepOutcome::Retry { kind: RetryKind::Fresh };
        let copied = outcome;
        assert_eq!(outcome, copied);
        assert_eq!(StepOutcome::Compacted, StepOutcome::Compacted);
    }
}
