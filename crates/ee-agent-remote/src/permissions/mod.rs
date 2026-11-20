//! Remote approval round-trip and audit trail.
//!
//! [`gate`] defines the consumer-boundary trait the headless handler uses.
//! [`approvals`] is the registry shared by ACP permission events and headless
//! gates. [`audit`] is the append-only decision trail.

pub mod approvals;
pub mod audit;
pub mod gate;

pub use approvals::{MAX_SUMMARY_CHARS, RemoteApprovals, SERVER_REQUEST_ID_BASE};
pub use audit::{AuditEntry, AuditLog, MAX_AUDIT_DETAIL_CHARS};
pub use gate::{ApprovalGate, DenyAllGate, GateDecision, GateKind, GateRequest};
