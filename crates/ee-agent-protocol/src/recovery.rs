//! Recoverable turn interruption wire types.
//!
//! Shared by the server side (which produces recoverable-turn outcomes),
//! orchestrator providers (which build the payloads), and hosts/TUIs (which
//! parse them out of JSON-RPC error `data`).  The payload carries the fault
//! class, a safe-resume flag, retry hints, and checkpoint identity so clients
//! can offer Resume/Discard without parsing free-form error strings.

use serde::{Deserialize, Serialize};

/// Fault class of a recoverable interruption.
///
/// Classifies *why* a turn stopped so callers can decide whether resuming is
/// safe, whether a retry is useful, and how to phrase the notice.  Never
/// derived from parsing error strings — providers must classify structurally
/// at the point of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum RecoverableFault {
    /// The wall-clock deadline of the current turn slice elapsed; completed
    /// work is durable and a fresh slice may continue.
    Deadline,
    /// A transient provider or network failure (5xx, connection reset, ...).
    Transient,
    /// The provider rate-limited the request; `retry_after` may carry the
    /// server hint.
    RateLimited,
    /// A tool may have executed with unknown completion (ambiguous
    /// side effect); automatic replay must not run.
    AmbiguousTool,
    /// A permanent configuration problem (unknown model, bad endpoint, ...).
    Configuration,
    /// An authentication or authorization problem (expired key, 401/403).
    Auth,
    /// A policy denial.
    Policy,
    /// The request itself was invalid.
    InvalidRequest,
}

impl RecoverableFault {
    /// Stable wire label for diagnostics and metrics keys.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Transient => "transient",
            Self::RateLimited => "rate_limited",
            Self::AmbiguousTool => "ambiguous_tool",
            Self::Configuration => "configuration",
            Self::Auth => "auth",
            Self::Policy => "policy",
            Self::InvalidRequest => "invalid_request",
        }
    }

    /// Whether resuming after this fault is ever safe without user
    /// confirmation.  Ambiguous side effects are never auto-resumed.
    #[must_use]
    pub fn is_safe_to_resume(&self) -> bool {
        matches!(self, Self::Deadline | Self::Transient | Self::RateLimited | Self::Configuration)
    }
}

/// Structured recoverable-turn payload carried in a JSON-RPC error `data`
/// field (key `"recoverable"`) and inside
/// `ProviderError::Recoverable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoverableError {
    /// Why the turn stopped.
    pub fault: RecoverableFault,
    /// Human-readable summary ("turn paused after 300s; 4 tools completed").
    pub detail: String,
    /// Optional underlying cause, when one is known.
    pub cause: Option<String>,
    /// Whether the checkpoint is resumable without rerunning ambiguous
    /// operations.  `false` when a write/execute tool was in flight.
    pub safe_resume: bool,
    /// Server-provided retry hint, in milliseconds.
    pub retry_after: Option<u64>,
    /// Durable checkpoint identity, when one was persisted.
    pub checkpoint_id: Option<String>,
    /// Tool calls whose results are already durable in the checkpoint.
    pub completed_tool_calls: u64,
    /// How many times this turn has already been resumed.
    pub resumed_count: u32,
}

impl RecoverableError {
    /// Builds a new recoverable payload.
    #[must_use]
    pub fn new(fault: RecoverableFault, detail: impl Into<String>) -> Self {
        Self {
            fault,
            detail: detail.into(),
            cause: None,
            safe_resume: fault.is_safe_to_resume(),
            retry_after: None,
            checkpoint_id: None,
            completed_tool_calls: 0,
            resumed_count: 0,
        }
    }

    /// Attaches the underlying cause.
    #[must_use]
    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Overrides the safe-resume flag (used when an ambiguous tool was in
    /// flight even though the fault class alone would allow resuming).
    #[must_use]
    pub fn with_safe_resume(mut self, safe_resume: bool) -> Self {
        self.safe_resume = safe_resume;
        self
    }

    /// Attaches a server retry hint in milliseconds.
    #[must_use]
    pub fn with_retry_after(mut self, millis: u64) -> Self {
        self.retry_after = Some(millis);
        self
    }

    /// Attaches the durable checkpoint identity.
    #[must_use]
    pub fn with_checkpoint_id(mut self, id: impl Into<String>) -> Self {
        self.checkpoint_id = Some(id.into());
        self
    }

    /// Attaches the completed-tool count and resume count.
    #[must_use]
    pub fn with_counts(mut self, completed_tool_calls: u64, resumed_count: u32) -> Self {
        self.completed_tool_calls = completed_tool_calls;
        self.resumed_count = resumed_count;
        self
    }
}
