//! Pane geometry and transcript limits shared across the agents pane.

// ── Pane geometry constants ──────────────────────────────────────────────────

/// Width of the right-split agents pane.
pub(crate) const AGENTS_PANE_RIGHT_WIDTH: u16 = 48;
/// Height of the bottom-split agents pane.
pub(crate) const AGENTS_PANE_BOTTOM_HEIGHT: u16 = 14;
/// Nick column width inside transcript lines.
pub(crate) const AGENTS_NICK_COL_WIDTH: usize = 10;
/// Maximum transcript items retained per thread.
pub(crate) const AGENTS_TRANSCRIPT_MAX: usize = 1500;
/// Maximum stderr/debug lines retained per thread.
pub(crate) const AGENTS_STDERR_MAX: usize = 200;
/// Lines scrolled per PageUp/PageDown key press.
pub(crate) const AGENTS_SCROLL_PAGE: usize = 10;
/// Maximum explicitly attached context files per agent session.
pub(crate) const AGENT_CONTEXT_MAX_FILES: usize = 8;
/// Maximum bytes captured from one explicitly attached context file.
pub(crate) const AGENT_CONTEXT_MAX_FILE_BYTES: usize = 64 * 1024;
/// Maximum bytes captured from all explicitly attached context files.
pub(crate) const AGENT_CONTEXT_MAX_TOTAL_BYTES: usize = 128 * 1024;
/// Maximum explicit extra workspace roots granted in one Agents TUI process.
pub(super) const AGENT_ADDITIONAL_ROOT_MAX: usize = 8;
/// Maximum terminal output tail rendered by `/ps` and `/tasks`.
pub(super) const AGENT_TERMINAL_OUTPUT_TAIL_BYTES: usize = 4 * 1024;
/// Maximum agent-owned terminals targeted by one `/stop all` request.
pub(super) const AGENT_TERMINAL_STOP_ALL_MAX: usize = 16;
/// Safe mode explicitly negotiated when an agent omits session mode state.
pub(super) const DEFAULT_AGENT_MODE_ID: &str = "ask";
/// Maximum concurrent create/load/resume operations from one pane.
pub(super) const AGENT_LIFECYCLE_CONCURRENCY: usize = 4;

pub(super) const AGENT_PROMPT_HISTORY_MAX: usize = 200;
pub(super) const AGENT_PROMPT_QUEUE_MAX: usize = 16;
pub(super) const AGENT_REVIEW_CONTEXT_MAX_BYTES: usize = 32 * 1024;
