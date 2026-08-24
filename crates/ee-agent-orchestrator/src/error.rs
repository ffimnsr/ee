//! Orchestrator errors and their mapping onto the ACP provider framework.
//!
//! [`OrchestratorError`] is the single error type of the orchestrator loop;
//! the conversion into
//! [`ProviderError`] lets an
//! orchestrator-backed provider surface failures through the framework's
//! JSON-RPC error shaping (policy denials become permission errors,
//! cancellation stays cancellation, everything else is a backend failure).

use std::fmt;

use ee_acp_agent_server::ProviderError;

/// Orchestrator-level failure, distinct from framework and provider errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorError {
    /// The model adapter failed or returned an unsupported response.
    ModelFailure(String),
    /// A tool failed at the orchestration boundary.
    ToolFailure(String),
    /// The policy engine denied an operation.
    PolicyDenied(String),
    /// A configured budget (iterations, tool calls, memory, ...) was exceeded.
    BudgetExceeded(String),
    /// A wall-clock deadline (turn slice, model call, ...) was exceeded.
    /// Distinct from [`OrchestratorError::BudgetExceeded`] so the runtime can
    /// convert deadline stops into recoverable interruptions instead of
    /// budget failures.
    DeadlineExceeded(String),
    /// A wall-clock limit (turn, tool, ...) was exceeded.
    Timeout(String),
    /// The turn was cancelled.
    Cancellation,
    /// An orchestrator invariant was violated (duplicate tool, closed sink, ...).
    InvalidState(String),
    /// A subagent failed (subagent phase).
    SubagentFailure(String),
    /// Orchestrator state could not be serialized or deserialized.
    Serialization(String),
    /// The loop was judged stuck (repeated responses, repeated tool calls,
    /// repeated failed edits, or no task-graph progress).
    Stuck(String),
}

impl OrchestratorError {
    /// Whether this error represents a cancelled turn.
    #[must_use]
    pub fn is_cancellation(&self) -> bool {
        matches!(self, Self::Cancellation)
    }
}

impl fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelFailure(reason) => write!(f, "model failure: {reason}"),
            Self::ToolFailure(reason) => write!(f, "tool failure: {reason}"),
            Self::PolicyDenied(reason) => write!(f, "policy denied: {reason}"),
            Self::BudgetExceeded(reason) => write!(f, "budget exceeded: {reason}"),
            Self::DeadlineExceeded(reason) => write!(f, "deadline exceeded: {reason}"),
            Self::Timeout(reason) => write!(f, "orchestrator timeout: {reason}"),
            Self::Cancellation => f.write_str("turn cancelled"),
            Self::InvalidState(reason) => write!(f, "orchestrator state error: {reason}"),
            Self::SubagentFailure(reason) => write!(f, "subagent failure: {reason}"),
            Self::Serialization(reason) => {
                write!(f, "orchestrator serialization error: {reason}")
            }
            Self::Stuck(reason) => write!(f, "stuck: {reason}"),
        }
    }
}

impl std::error::Error for OrchestratorError {}

impl From<crate::model::ModelError> for OrchestratorError {
    fn from(error: crate::model::ModelError) -> Self {
        match error {
            crate::model::ModelError::Adapter(reason) => Self::ModelFailure(reason),
            crate::model::ModelError::InvalidResponse(reason) => {
                Self::ModelFailure(format!("invalid model response: {reason}"))
            }
            crate::model::ModelError::Cancelled => Self::Cancellation,
        }
    }
}

impl From<OrchestratorError> for ProviderError {
    fn from(error: OrchestratorError) -> Self {
        match error {
            OrchestratorError::ModelFailure(reason) => {
                ProviderError::BackendFailure(format!("model failure: {reason}"))
            }
            OrchestratorError::ToolFailure(reason) => {
                ProviderError::BackendFailure(format!("tool failure: {reason}"))
            }
            OrchestratorError::PolicyDenied(reason) => ProviderError::PermissionDenied(reason),
            OrchestratorError::BudgetExceeded(reason) => {
                ProviderError::BackendFailure(format!("budget exceeded: {reason}"))
            }
            OrchestratorError::DeadlineExceeded(reason) => {
                ProviderError::BackendFailure(format!("deadline exceeded: {reason}"))
            }
            OrchestratorError::Timeout(reason) => {
                ProviderError::BackendFailure(format!("orchestrator timeout: {reason}"))
            }
            OrchestratorError::Cancellation => ProviderError::Cancellation,
            OrchestratorError::InvalidState(reason) => {
                ProviderError::BackendFailure(format!("orchestrator state error: {reason}"))
            }
            OrchestratorError::SubagentFailure(reason) => {
                ProviderError::BackendFailure(format!("subagent failure: {reason}"))
            }
            OrchestratorError::Serialization(reason) => {
                ProviderError::BackendFailure(format!("orchestrator serialization error: {reason}"))
            }
            OrchestratorError::Stuck(reason) => {
                ProviderError::BackendFailure(format!("stuck: {reason}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<(OrchestratorError, &'static str)> {
        vec![
            (OrchestratorError::ModelFailure("boom".into()), "model failure"),
            (OrchestratorError::ToolFailure("boom".into()), "tool failure"),
            (OrchestratorError::PolicyDenied("no".into()), "policy denied"),
            (OrchestratorError::BudgetExceeded("loop".into()), "budget exceeded"),
            (OrchestratorError::DeadlineExceeded("turn".into()), "deadline exceeded"),
            (OrchestratorError::Timeout("turn".into()), "orchestrator timeout"),
            (OrchestratorError::Cancellation, "turn cancelled"),
            (OrchestratorError::InvalidState("dup".into()), "orchestrator state error"),
            (OrchestratorError::SubagentFailure("kid".into()), "subagent failure"),
            (OrchestratorError::Serialization("json".into()), "orchestrator serialization error"),
            (OrchestratorError::Stuck("repeated-model-response".into()), "stuck"),
        ]
    }

    #[test]
    fn display_is_populated_for_every_variant() {
        for (error, expected_fragment) in samples() {
            let message = error.to_string();
            assert!(message.contains(expected_fragment), "{message}");
        }
    }

    #[test]
    fn error_impl_is_satisfied() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<OrchestratorError>();
    }

    #[test]
    fn cancellation_helpers_agree() {
        assert!(OrchestratorError::Cancellation.is_cancellation());
        assert!(!OrchestratorError::ModelFailure("x".into()).is_cancellation());
    }

    #[test]
    fn deadline_is_distinct_from_budget_exceeded() {
        let deadline: OrchestratorError = OrchestratorError::DeadlineExceeded("slice".into());
        let budget: OrchestratorError = OrchestratorError::BudgetExceeded("count".into());
        assert_ne!(deadline, budget);
        assert!(deadline.to_string().contains("deadline exceeded"));
        assert!(!deadline.is_cancellation());
    }

    #[test]
    fn model_errors_map_to_orchestrator_errors() {
        use crate::model::ModelError;
        let mapped: OrchestratorError = ModelError::Adapter("net".into()).into();
        assert_eq!(mapped, OrchestratorError::ModelFailure("net".into()));
        let mapped: OrchestratorError = ModelError::InvalidResponse("shape".into()).into();
        assert!(
            matches!(mapped, OrchestratorError::ModelFailure(ref r) if r.contains("invalid model response"))
        );
        let mapped: OrchestratorError = ModelError::Cancelled.into();
        assert_eq!(mapped, OrchestratorError::Cancellation);
    }

    #[test]
    fn provider_error_mapping_preserves_kind() {
        use ee_acp_agent_server::ProviderError;
        let cases: Vec<(OrchestratorError, ProviderError)> = vec![
            (
                OrchestratorError::ModelFailure("m".into()),
                ProviderError::BackendFailure("model failure: m".into()),
            ),
            (
                OrchestratorError::ToolFailure("t".into()),
                ProviderError::BackendFailure("tool failure: t".into()),
            ),
            (
                OrchestratorError::PolicyDenied("write denied".into()),
                ProviderError::PermissionDenied("write denied".into()),
            ),
            (
                OrchestratorError::BudgetExceeded("b".into()),
                ProviderError::BackendFailure("budget exceeded: b".into()),
            ),
            (
                OrchestratorError::DeadlineExceeded("d".into()),
                ProviderError::BackendFailure("deadline exceeded: d".into()),
            ),
            (
                OrchestratorError::Timeout("to".into()),
                ProviderError::BackendFailure("orchestrator timeout: to".into()),
            ),
            (OrchestratorError::Cancellation, ProviderError::Cancellation),
            (
                OrchestratorError::InvalidState("s".into()),
                ProviderError::BackendFailure("orchestrator state error: s".into()),
            ),
            (
                OrchestratorError::SubagentFailure("k".into()),
                ProviderError::BackendFailure("subagent failure: k".into()),
            ),
            (
                OrchestratorError::Serialization("j".into()),
                ProviderError::BackendFailure("orchestrator serialization error: j".into()),
            ),
            (
                OrchestratorError::Stuck("repeated-model-response".into()),
                ProviderError::BackendFailure("stuck: repeated-model-response".into()),
            ),
        ];
        for (orchestrator, expected) in cases {
            let provider: ProviderError = orchestrator.clone().into();
            assert_eq!(format!("{provider:?}"), format!("{expected:?}"), "{orchestrator:?}");
        }
    }

    #[test]
    fn provider_error_mapping_hits_expected_jsonrpc_codes() {
        use ee_acp_agent_server::ProviderError;
        let denied: ProviderError = OrchestratorError::PolicyDenied("no".into()).into();
        assert_eq!(denied.jsonrpc_code(), -32001);
        let cancelled: ProviderError = OrchestratorError::Cancellation.into();
        assert_eq!(cancelled.jsonrpc_code(), -32800);
        let backend: ProviderError = OrchestratorError::ModelFailure("x".into()).into();
        assert_eq!(backend.jsonrpc_code(), -32603);
    }
}
