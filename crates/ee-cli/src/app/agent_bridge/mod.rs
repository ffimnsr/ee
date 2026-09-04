//! Phase 4 bridge: ACP `fs/*` and `terminal/*` client methods against editor
//! buffers, the existing save pipeline, and tracked terminal processes.
//!
//! The host's [`BridgeUiHandler`] forwards file and terminal-create requests
//! to the agents pane (approval UI, buffer reads/writes, terminal spawning
//! all live in `ee-cli`); terminal output/wait/kill/release operate on a
//! shared [`AgentTerminals`] registry from the host worker thread.  Every
//! buffer read/write is recorded in the action log for future
//! checkpoint/restore.
//!
//! Policy invariants (fail closed):
//! - ACP paths must be absolute; reads resolve against open buffers first
//!   (unsaved text wins over disk) and fall back to workspace-scoped disk
//!   reads.  Paths outside the workspace are rejected.
//! - Writes always route through buffer open → edit → save semantics; the
//!   diff is recomputed against the latest snapshot and a conflict error is
//!   returned when the buffer cannot converge.
//! - VLF buffers reject unbounded reads and all writes.
//! - Terminals run with an explicit command/args (never a shell string),
//!   inherit no secret-like environment variables, and their output is
//!   bounded by both the ACP request limit and an editor-side hard cap.
//!
//! Module layout (this file stays below the 1K LOC threshold; the bridge is
//! split into focused submodules):
//! - `terminal.rs`   — bounded terminal registry, output capture, process env/command rules
//! - `bridge_ui.rs`  — ACP `ClientRequestHandler` forwarding layer
//! - `approval.rs`   — approval schema: kinds, choices, fingerprints
//! - `prompt.rs`     — `ApprovalPrompt` construction and option helpers
//! - `write.rs`      — revision/diff helpers, agent payloads, action log
//! - `read.rs`       — bounded text-read helpers and the `impl App` read path
//! - `pump.rs`       — request pump, workspace-memory operations, proxy dispatch
//! - `app_*.rs`      — `impl App` blocks grouped by concern
//!
//! Privacy note: items moved from this single module into the submodules
//! below keep `pub(crate)`/`pub(super)` visibility tailored to where they are
//! consumed.  Submodules reach each other via `use super::<submodule>::item;`
//! exactly as they used to via `use super::*`.  Tests live beside the code
//! they exercise.

#[cfg(test)]
pub(super) mod test_hooks;

mod app_approval;
mod app_decision;
mod app_proxy;
mod app_proxy_ops;
mod app_search;
mod app_test_hooks;
mod app_trust;
mod app_validation;
mod app_web;
mod app_write;
mod approval;
mod bridge_ui;
mod prompt;
mod pump;
mod read;
mod terminal;
mod write;

#[cfg(test)]
static WEB_DISPATCH_TEST_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// Re-exports consumed by `crate::app` (see `app/mod.rs` and `agent_pane.rs`).
// `unused_imports` is allowed here on purpose: several re-exported names are
// only referenced from test code or `crate::tests`, so `cargo fix` would
// otherwise strip them from the non-test build.
#[allow(unused_imports)]
pub(crate) use approval::{
    ApprovalChoice, ApprovalPolicy, PERSISTENT_TERMINAL_OPTION_LABEL, PreparedWrite,
    ToolApprovalMode, WriteExpectation, WriteReplyKind,
};
pub(crate) use bridge_ui::{BridgeUiHandler, BridgeUiMessage};
pub(crate) use prompt::ApprovalPrompt;
pub(crate) use terminal::{AgentTerminals, OwnedTerminalStop, OwnedTerminalSummary, TerminalOwner};
pub(crate) use write::ActionLogEntry;
