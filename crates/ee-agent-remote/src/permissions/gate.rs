//! Consumer-boundary approval gate for headless operations.
//!
//! The headless handler must gate file writes and terminal creation, but it
//! must not know how approvals reach a remote driver. Implementations of
//! [`ApprovalGate`] bridge into the remote approval registry (R5) or a test
//! script. Every implementation is fail-closed: an error, a dropped future, or
//! an expired deadline is a denial.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use ee_agent_protocol::SessionId;

/// What a headless operation is asking to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateKind {
    /// Write a file at an already-contained path.
    WriteFile {
        /// Canonical target path.
        path: PathBuf,
        /// New content size in bytes.
        bytes: usize,
    },
    /// Spawn a terminal command in an already-contained directory.
    CreateTerminal {
        /// Executable to spawn (never a shell string).
        command: String,
        /// Explicit argument list.
        args: Vec<String>,
        /// Canonical working directory.
        cwd: PathBuf,
    },
}

/// One gated operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRequest {
    /// Session that triggered the operation.
    pub session_id: SessionId,
    /// Bounded human summary (redacted before crossing any boundary).
    pub summary: String,
    /// Operation details.
    pub kind: GateKind,
}

/// Gate answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Proceed with the operation.
    Allow,
    /// Refuse the operation with a typed denial.
    Deny,
}

/// Asynchronous approval gate.
pub trait ApprovalGate: Send + Sync + 'static {
    /// Answers one gate request. Must not block the connection task.
    fn authorize(
        &self,
        request: GateRequest,
    ) -> Pin<Box<dyn Future<Output = GateDecision> + Send + '_>>;
}

/// Gate that denies everything (default, fail closed).
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllGate;

impl ApprovalGate for DenyAllGate {
    fn authorize(
        &self,
        _request: GateRequest,
    ) -> Pin<Box<dyn Future<Output = GateDecision> + Send + '_>> {
        Box::pin(async { GateDecision::Deny })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deny_all_gate_denies() {
        let decision = DenyAllGate
            .authorize(GateRequest {
                session_id: SessionId::new("s-1"),
                summary: "write".into(),
                kind: GateKind::WriteFile { path: PathBuf::from("/tmp/x"), bytes: 1 },
            })
            .await;
        assert_eq!(decision, GateDecision::Deny);
    }
}
