//! Irssi-style agents pane: transcript scrollback, status footer, and
//! composer input (Phase 3).
//!
//! The pane is frontend-owned: all agent state arrives as deterministic
//! [`AgentEvent`]s from `ee-agent-host` and is rendered from the local
//! transcript model.  This module never crafts ACP JSON; prompt, permission,
//! and elicitation responses go through host APIs only.
//!
//! The host bridge runs on a dedicated worker thread over a single-threaded
//! tokio runtime so the TUI loop never blocks on subprocess or session I/O.
//! Everything here is gated by the `agents` cargo feature; without it the
//! pane state is absent and `:agents` reports the compile-time disabled
//! message.
//!
//! Module layout (this module stays below the 1K LOC threshold; the pane is
//! split into focused submodules):
//! - `constants.rs`  — pane geometry and transcript limits
//! - `thread_ui.rs`  — thread transcript model: `AgentThreadUi`, `TranscriptItem`
//! - `state.rs`      — `AgentPaneState`, layout, prompt structs, session lifecycle types
//! - `elicitation.rs`— elicitation prompts and field schemas
//! - `host.rs`       — host worker bridge and command enum
//! - `format.rs`     — display formatting and slash-command tables
//! - `pump.rs`       — main-loop bridge/event/reply pump (pre-existing)
//! - `app_*.rs`      — `impl App` blocks grouped by concern
//!
//! Privacy note: items moved from this single module into the submodules
//! below keep `pub(crate)`/`pub(super)` visibility tailored to where they are
//! consumed.  Submodules reach each other via `use super::<submodule>::item;`
//! exactly as they used to via `use super::*`.  Tests live beside the code
//! they exercise.

use super::*;

mod app_commands;
mod app_context;
mod app_events;
mod app_host;
mod app_keys;
mod app_mode;
mod app_persist;
mod app_prompt;
mod app_replies;
mod app_sessions;
mod app_workflow;
mod constants;
mod elicitation;
mod format;
mod host;
mod pump;
mod state;
mod thread_ui;

// Re-exports consumed by `crate::app` (see `app/mod.rs`) and by `state.rs`
// (`crate::app::agent_pane::AgentPaneState`).  `unused_imports` is allowed
// here on purpose: `cargo fix` would otherwise strip names that are only
// referenced through `crate::app` re-exports or from test code.
#[allow(unused_imports)]
pub(crate) use constants::{
    AGENTS_NICK_COL_WIDTH, AGENTS_PANE_BOTTOM_HEIGHT, AGENTS_PANE_RIGHT_WIDTH,
};
#[allow(unused_imports)]
pub(crate) use format::{format_duration, turn_metrics_label};
#[allow(unused_imports)]
pub(crate) use state::{AgentPaneLayout, AgentPaneState};
#[allow(unused_imports)]
pub(crate) use thread_ui::{AgentThreadUi, MessageRenderKind, ThreadUiState, TranscriptItem};
