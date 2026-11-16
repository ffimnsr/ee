//! Orchestrator configuration.
//!
//! [`OrchestratorConfig`] carries the loop, tool, subagent, timeout, and
//! memory knobs that later phases wire into the loop engine, tool executor,
//! subagent manager, and memory store.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::compaction::CompactionConfig;
use crate::error::OrchestratorError;
use crate::reflection::ReflectionConfig;
use crate::repair::RepairConfig;
use crate::rubber_duck_config::RubberDuckConfig;
use crate::rubber_duck_trigger::RubberDuckTriggerConfig;
use crate::session_log::SessionLogConfig;
use crate::stop_gate::StopGateConfig;
use crate::stuck::StuckConfig;
use crate::tools::SideEffectClass;
use crate::workspace_isolation::WorkspaceIsolationConfig;

/// Default maximum subagent nesting depth.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: usize = 2;
/// Default maximum concurrently running subagents.
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 4;
/// Default wall-clock limit for fast local reads: 15 seconds.
pub const DEFAULT_FAST_LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Default wall-clock limit for search and external MCP reads: 30 seconds.
pub const DEFAULT_SEARCH_AND_MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Default wall-clock limit for writes and edit application: 60 seconds.
pub const DEFAULT_WRITE_AND_EDIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default wall-clock limit for terminal tools and validation commands: 120 seconds.
pub const DEFAULT_TERMINAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard ceiling for an explicit terminal `timeout_ms`: 300 seconds.
pub const DEFAULT_TERMINAL_MAX_TIMEOUT: Duration = Duration::from_secs(300);
/// Default per-subagent wall-clock limit: 300 seconds.
pub const DEFAULT_SUBAGENT_TIMEOUT: Duration = Duration::from_secs(300);
/// Default maximum idle interval for a running subagent: 60 seconds.
pub const DEFAULT_SUBAGENT_STALL_TIMEOUT: Duration = Duration::from_secs(60);

fn default_subagent_stall_timeout() -> Duration {
    DEFAULT_SUBAGENT_STALL_TIMEOUT
}
/// Default memory byte limit: 1 MiB.
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 1024 * 1024;
/// Default maximum subagent spawns per turn across all depths.
pub const DEFAULT_MAX_SUBAGENTS_PER_TURN: usize = 8;
/// Default maximum concurrently running independent read-only tools.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 10;
/// Default model context-window size in tokens, reported as the ACP
/// `usage_update` context-window denominator.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
/// Recovery knobs for resumable turns (feature-gated by [`RecoveryConfig::enabled`]).
///
/// When disabled (the default), turns keep today's fail-fast behavior: a
/// deadline or transient failure is an ordinary error.  When enabled, the
/// runtime captures durable checkpoints at turn milestones, deadline and
/// timeout interruptions become `TurnOutcome::Interrupted`, and a resumed
/// turn restarts only the wall-clock slice while retaining cumulative budget
/// counters, completed tool results, and the task/memory stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryConfig {
    /// Master switch: recoverable turn outcomes and durable checkpoints.
    pub enabled: bool,
    /// Directory for durable checkpoints; `None` keeps checkpoints in memory
    /// only (same-process resume, no crash restore).
    pub checkpoint_dir: Option<PathBuf>,
    /// Checkpoints retained per session before the oldest is pruned.
    pub max_checkpoints_per_session: usize,
    /// Retention window; expired checkpoints are pruned on access.
    pub checkpoint_ttl: Duration,
    /// Serialized checkpoint byte cap; captures above it fail closed.
    pub max_checkpoint_bytes: usize,
    /// Automatic resumes per turn before manual confirmation is required.
    /// Ambiguous-side-effect interruptions are never auto-resumed.
    pub auto_resume_max: u32,
    /// Cumulative session wall-clock cap across slices; `None` means the
    /// turn may resume indefinitely.
    pub session_timeout: Option<Duration>,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            checkpoint_dir: None,
            max_checkpoints_per_session: DEFAULT_MAX_CHECKPOINTS_PER_SESSION,
            checkpoint_ttl: DEFAULT_CHECKPOINT_TTL,
            max_checkpoint_bytes: DEFAULT_MAX_CHECKPOINT_BYTES,
            auto_resume_max: DEFAULT_AUTO_RESUME_MAX,
            session_timeout: None,
        }
    }
}

impl RecoveryConfig {
    /// Enables same-process recovery only. Checkpoints are never written to
    /// disk, so a provider restart cannot resume this state.
    #[must_use]
    pub fn memory_only() -> Self {
        Self { enabled: true, checkpoint_dir: None, ..Self::default() }
    }

    /// Enables crash-recoverable checkpoints under the explicit local directory.
    #[must_use]
    pub fn durable(checkpoint_dir: std::path::PathBuf) -> Self {
        Self { enabled: true, checkpoint_dir: Some(checkpoint_dir), ..Self::default() }
    }

    /// Whether checkpoints survive provider process restart.
    #[must_use]
    pub fn is_durable(&self) -> bool {
        self.enabled && self.checkpoint_dir.is_some()
    }
}
/// Default checkpoints retained per session before the oldest is pruned.
pub const DEFAULT_MAX_CHECKPOINTS_PER_SESSION: usize = 8;
/// Default checkpoint retention: 7 days.
pub const DEFAULT_CHECKPOINT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Default serialized checkpoint byte cap: 4 MiB.
pub const DEFAULT_MAX_CHECKPOINT_BYTES: usize = 4 * 1024 * 1024;
/// Default automatic resumes per turn before manual resume is required.
pub const DEFAULT_AUTO_RESUME_MAX: u32 = 1;

/// Per-category tool wall-clock limits enforced by [`crate::tools::ToolExecutor`].
///
/// Categories are resolved deterministically from the tool name and its
/// side-effect class; a tool requesting a bounded per-call `timeout_ms` is
/// clamped to `terminal_max` instead of the flat category ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTimeoutPolicy {
    /// Fast local reads (`read_file`, `list_files`, outlines, diagnostics).
    pub fast_local_read: Duration,
    /// Search and external MCP reads.
    pub search_and_mcp: Duration,
    /// Writes, edit application, formatting, and rename.
    pub write_and_edit: Duration,
    /// Terminal tools and validation commands without an explicit override.
    pub terminal_default: Duration,
    /// Upper bound for an explicit per-call `timeout_ms`.
    pub terminal_max: Duration,
}

impl Default for ToolTimeoutPolicy {
    fn default() -> Self {
        Self {
            fast_local_read: DEFAULT_FAST_LOCAL_READ_TIMEOUT,
            search_and_mcp: DEFAULT_SEARCH_AND_MCP_TIMEOUT,
            write_and_edit: DEFAULT_WRITE_AND_EDIT_TIMEOUT,
            terminal_default: DEFAULT_TERMINAL_TIMEOUT,
            terminal_max: DEFAULT_TERMINAL_MAX_TIMEOUT,
        }
    }
}

impl ToolTimeoutPolicy {
    /// Resolves the category ceiling for one tool invocation.
    #[must_use]
    pub fn timeout_for_tool(&self, name: &str, class: SideEffectClass) -> Duration {
        if terminal_tool_name(name) {
            return self.terminal_default;
        }
        if fast_local_read_tool(name) {
            return self.fast_local_read;
        }
        if write_and_edit_tool(name) || class == SideEffectClass::Write {
            return self.write_and_edit;
        }
        match class {
            SideEffectClass::Read => self.search_and_mcp,
            SideEffectClass::Execute => self.terminal_default,
            SideEffectClass::Write => self.write_and_edit,
            SideEffectClass::Delegate => self.terminal_default,
        }
    }

    /// Resolves the outer executor ceiling for one invocation, honoring a
    /// bounded explicit `timeout_ms` on terminal waits.
    #[must_use]
    pub fn effective_timeout(
        &self,
        name: &str,
        class: SideEffectClass,
        requested_ms: Option<u64>,
    ) -> Duration {
        let category = self.timeout_for_tool(name, class);
        if !terminal_tool_name(name) {
            return category;
        }
        match requested_ms {
            Some(requested_ms) => {
                let bounded = Duration::from_millis(requested_ms)
                    .clamp(EXPLICIT_TIMEOUT_FLOOR, self.terminal_max);
                bounded + TERMINAL_WAIT_SLACK
            }
            None => category,
        }
    }

    /// Caps every tier at `limit`, used for reduced subagent configurations.
    #[must_use]
    pub fn capped(mut self, limit: Duration) -> Self {
        self.fast_local_read = self.fast_local_read.min(limit);
        self.search_and_mcp = self.search_and_mcp.min(limit);
        self.write_and_edit = self.write_and_edit.min(limit);
        self.terminal_default = self.terminal_default.min(limit);
        self.terminal_max = self.terminal_max.min(limit);
        self
    }
}

/// Lower bound for an explicit terminal `timeout_ms`.
pub(crate) const EXPLICIT_TIMEOUT_FLOOR: Duration = Duration::from_secs(1);
/// Extra outer-loop allowance so the tool can clean up a timed-out terminal.
pub(crate) const TERMINAL_WAIT_SLACK: Duration = Duration::from_secs(5);

fn terminal_tool_name(name: &str) -> bool {
    matches!(
        name,
        "create_terminal"
            | "terminal_output"
            | "wait_for_terminal_exit"
            | "kill_terminal"
            | "release_terminal"
            | "cargo_check"
    ) || name.contains("terminal")
}

fn fast_local_read_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "list_files" | "ee_file_outline" | "ee_diagnostics" | "ee_workspace_roots"
    )
}

fn write_and_edit_tool(name: &str) -> bool {
    matches!(name, "write_file" | "ee_edit_apply" | "ee_format_buffer" | "ee_lsp_rename")
}

/// Shared configuration for one [`crate::OrchestratorRuntime`] instance.
///
/// Exhaustive so callers can use field updates off [`Default`]; construct
/// with [`OrchestratorConfig::default`] and override the knobs needed.
/// `PartialEq` is implemented (no `Eq`: the compaction config holds a
/// floating-point pressure knob).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Maximum model-call iterations per turn; `None` is unlimited.
    pub max_loop_iterations: Option<usize>,
    /// Maximum tool calls per turn across all iterations; `None` is unlimited.
    pub max_tool_calls_per_turn: Option<usize>,
    /// Maximum subagent nesting depth (subagent phase).
    pub max_subagent_depth: usize,
    /// Maximum concurrently running subagents (subagent phase).
    pub max_parallel_subagents: usize,
    /// Per-turn wall-clock limit; `None` is unlimited.
    pub turn_timeout: Option<Duration>,
    /// Per-tool-call wall-clock limits, resolved by tool category.
    #[serde(default)]
    pub tool_timeouts: ToolTimeoutPolicy,
    /// Per-subagent wall-clock limit (subagent phase).
    pub subagent_timeout: Duration,
    /// Maximum interval without model, tool, or task progress before a running
    /// subagent is cancelled as stalled.
    #[serde(default = "default_subagent_stall_timeout")]
    pub subagent_stall_timeout: Duration,
    /// Memory store byte limit.
    pub memory_limit_bytes: usize,
    /// Maximum model adapter invocations per turn; `None` is unlimited.
    pub max_model_calls: Option<usize>,
    /// Maximum subagent spawns per turn across all depths.
    pub max_subagents: usize,
    /// Maximum accumulated model output bytes (text + reasoning) per turn;
    /// `None` is unlimited.
    pub max_output_bytes: Option<usize>,
    /// Maximum concurrently running independent read-only tools in one
    /// planned batch; write/execute tools always run serially.
    pub max_parallel_tools: usize,
    /// Optional per-turn input-token cap; unknown provider usage fails closed.
    pub max_input_tokens: Option<usize>,
    /// Optional per-turn output-token cap; unknown provider usage fails closed.
    pub max_output_tokens: Option<usize>,
    /// Maximum automatic continuations after a length-truncated reply in one
    /// turn. `0` disables continuation.
    #[serde(default = "default_output_continuations")]
    pub max_output_continuations: usize,
    /// Whether the routed provider accepts a tool list that changes between
    /// steps. Providers advertise this as `tools.updates`; the default `false`
    /// fails closed instead of sending a change the route cannot honour.
    #[serde(default)]
    pub tool_list_updates: bool,
    /// Model context-window size in tokens; the ACP `usage_update.size`.
    pub context_window_tokens: u64,
    /// Stuck-detection thresholds for the loop engine.
    pub stuck: StuckConfig,
    /// Bounded self-review behavior after tool/edit loops.
    pub reflection: ReflectionConfig,
    /// Deterministic bound for automatic repairs after current validation fails.
    pub repair: RepairConfig,
    /// LLM compaction knobs (Phase 12): deterministic memory compaction
    /// settings plus the compaction-context input bound.
    pub compaction: CompactionConfig,
    /// Resumable-turn recovery knobs (feature-gated; disabled by default).
    pub recovery: RecoveryConfig,
    /// Resolved frontend-owned rubber-duck mode, route, and resource bounds.
    #[serde(default)]
    pub rubber_duck: RubberDuckConfig,
    /// Deterministic automatic trigger policy; manual-only until replay enables it.
    #[serde(default)]
    pub rubber_duck_triggers: RubberDuckTriggerConfig,
    /// Append-only session log: content-addressed message bodies and ordered
    /// model-request records. Memory-only when `dir` is unset.
    #[serde(default)]
    pub session_log: SessionLogConfig,
    /// Evidence gate applied before a changed turn may claim completion.
    #[serde(default)]
    pub stop_gate: StopGateConfig,
    /// Opt-in git-worktree isolation for write subagents.
    #[serde(default)]
    pub workspace_isolation: WorkspaceIsolationConfig,
    /// Opt-in risk-tiered approval classifier; disabled by default.
    #[serde(default)]
    pub approval_classifier: crate::approval_classifier::ApprovalClassifierConfig,
}

impl OrchestratorConfig {
    /// Rejects zero-valued finite caps, which would silently stop a turn before
    /// its first model or tool call.
    pub fn validate(&self) -> Result<(), OrchestratorError> {
        let zero_counts = [
            (self.max_loop_iterations, "max_loop_iterations"),
            (self.max_model_calls, "max_model_calls"),
            (self.max_tool_calls_per_turn, "max_tool_calls_per_turn"),
            (self.max_output_bytes, "max_output_bytes"),
        ];
        for (value, name) in zero_counts {
            if value == Some(0) {
                return Err(OrchestratorError::InvalidState(format!(
                    "{name} must be positive when set; omit it for unlimited"
                )));
            }
        }
        if self.turn_timeout.is_some_and(|limit| limit.is_zero()) {
            return Err(OrchestratorError::InvalidState(
                "turn_timeout must be positive when set; omit it for unlimited".into(),
            ));
        }
        for (value, name) in [
            (self.tool_timeouts.fast_local_read, "tool_timeouts.fast_local_read"),
            (self.tool_timeouts.search_and_mcp, "tool_timeouts.search_and_mcp"),
            (self.tool_timeouts.write_and_edit, "tool_timeouts.write_and_edit"),
            (self.tool_timeouts.terminal_default, "tool_timeouts.terminal_default"),
            (self.tool_timeouts.terminal_max, "tool_timeouts.terminal_max"),
        ] {
            if value.is_zero() {
                return Err(OrchestratorError::InvalidState(format!("{name} must be positive")));
            }
        }
        if self.tool_timeouts.terminal_max < self.tool_timeouts.terminal_default {
            return Err(OrchestratorError::InvalidState(
                "tool_timeouts.terminal_max must not be below terminal_default".into(),
            ));
        }
        self.compaction.validate()?;
        self.session_log.validate()?;
        self.approval_classifier.validate().map_err(OrchestratorError::InvalidState)?;
        Ok(())
    }
}

/// Maximum automatic continuations after a length-truncated reply.
pub const DEFAULT_MAX_OUTPUT_CONTINUATIONS: usize = 2;

fn default_output_continuations() -> usize {
    DEFAULT_MAX_OUTPUT_CONTINUATIONS
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            max_loop_iterations: None,
            max_tool_calls_per_turn: None,
            max_subagent_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_parallel_subagents: DEFAULT_MAX_PARALLEL_SUBAGENTS,
            turn_timeout: None,
            tool_timeouts: ToolTimeoutPolicy::default(),
            subagent_timeout: DEFAULT_SUBAGENT_TIMEOUT,
            subagent_stall_timeout: DEFAULT_SUBAGENT_STALL_TIMEOUT,
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            max_model_calls: None,
            max_subagents: DEFAULT_MAX_SUBAGENTS_PER_TURN,
            max_output_bytes: None,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            max_input_tokens: None,
            max_output_tokens: None,
            max_output_continuations: DEFAULT_MAX_OUTPUT_CONTINUATIONS,
            tool_list_updates: false,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            stuck: StuckConfig::default(),
            reflection: ReflectionConfig::default(),
            repair: RepairConfig::default(),
            compaction: CompactionConfig::default(),
            recovery: RecoveryConfig::default(),
            rubber_duck: RubberDuckConfig::default(),
            rubber_duck_triggers: RubberDuckTriggerConfig::default(),
            session_log: SessionLogConfig::default(),
            stop_gate: StopGateConfig::default(),
            workspace_isolation: WorkspaceIsolationConfig::default(),
            approval_classifier: crate::approval_classifier::ApprovalClassifierConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_expected_knobs() {
        let config = OrchestratorConfig::default();
        assert_eq!(config.max_loop_iterations, None);
        assert_eq!(config.max_tool_calls_per_turn, None);
        assert_eq!(config.max_subagent_depth, 2);
        assert_eq!(config.max_parallel_subagents, 4);
        assert_eq!(config.turn_timeout, None);
        assert_eq!(config.tool_timeouts, ToolTimeoutPolicy::default());
        assert_eq!(config.tool_timeouts.fast_local_read, Duration::from_secs(15));
        assert_eq!(config.tool_timeouts.search_and_mcp, Duration::from_secs(30));
        assert_eq!(config.tool_timeouts.write_and_edit, Duration::from_secs(60));
        assert_eq!(config.tool_timeouts.terminal_default, Duration::from_secs(120));
        assert_eq!(config.tool_timeouts.terminal_max, Duration::from_secs(300));
        assert_eq!(config.subagent_timeout, Duration::from_secs(300));
        assert_eq!(config.subagent_stall_timeout, Duration::from_secs(60));
        assert_eq!(config.memory_limit_bytes, 1024 * 1024);
        assert_eq!(config.max_model_calls, None);
        assert_eq!(config.max_subagents, 8);
        assert_eq!(config.max_output_bytes, None);
        assert_eq!(config.max_parallel_tools, 10);
        assert_eq!(config.max_input_tokens, None);
        assert_eq!(config.max_output_tokens, None);
        assert_eq!(config.context_window_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(
            config.stuck,
            StuckConfig {
                max_repeated_model_responses: 4,
                max_repeated_tool_calls: 4,
                max_failed_edit_attempts: 3,
                max_no_progress_iterations: 4,
            }
        );
        assert_eq!(
            config.reflection,
            ReflectionConfig { enabled: false, max_review_iterations: 1, max_fix_iterations: 1 }
        );
        assert_eq!(config.repair, RepairConfig::default());
        assert_eq!(config.compaction, CompactionConfig::default());
        assert_eq!(config.recovery, RecoveryConfig::default());
        assert!(!config.recovery.enabled);
        assert_eq!(config.rubber_duck, RubberDuckConfig::default());
        assert_eq!(config.rubber_duck_triggers, RubberDuckTriggerConfig::default());
        assert_eq!(config.session_log, SessionLogConfig::default());
        assert_eq!(config.stop_gate, StopGateConfig::default());
        assert_eq!(config.workspace_isolation, WorkspaceIsolationConfig::default());
    }

    #[test]
    fn tool_timeout_categories_resolve_and_clamp() {
        let policy = ToolTimeoutPolicy::default();
        assert_eq!(
            policy.timeout_for_tool("read_file", SideEffectClass::Read),
            Duration::from_secs(15)
        );
        assert_eq!(
            policy.timeout_for_tool("ee_file_outline", SideEffectClass::Read),
            Duration::from_secs(15)
        );
        assert_eq!(
            policy.timeout_for_tool("search_text", SideEffectClass::Read),
            Duration::from_secs(30)
        );
        assert_eq!(
            policy.timeout_for_tool("write_file", SideEffectClass::Write),
            Duration::from_secs(60)
        );
        assert_eq!(
            policy.timeout_for_tool("wait_for_terminal_exit", SideEffectClass::Execute),
            Duration::from_secs(120)
        );
        assert_eq!(
            policy.effective_timeout("ee_terminal_wait", SideEffectClass::Execute, Some(600_000)),
            Duration::from_secs(300) + TERMINAL_WAIT_SLACK
        );
        assert_eq!(
            policy.effective_timeout("ee_terminal_wait", SideEffectClass::Execute, None),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn zero_finite_caps_are_rejected() {
        for config in [
            OrchestratorConfig { max_loop_iterations: Some(0), ..Default::default() },
            OrchestratorConfig { max_model_calls: Some(0), ..Default::default() },
            OrchestratorConfig { max_tool_calls_per_turn: Some(0), ..Default::default() },
            OrchestratorConfig { max_output_bytes: Some(0), ..Default::default() },
            OrchestratorConfig { turn_timeout: Some(Duration::ZERO), ..Default::default() },
            OrchestratorConfig {
                tool_timeouts: ToolTimeoutPolicy {
                    fast_local_read: Duration::ZERO,
                    ..Default::default()
                },
                ..Default::default()
            },
            OrchestratorConfig {
                tool_timeouts: ToolTimeoutPolicy {
                    terminal_max: Duration::from_secs(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        ] {
            assert!(config.validate().is_err(), "zero finite caps must fail validation");
        }
        OrchestratorConfig::default().validate().expect("unlimited defaults are valid");
    }

    #[test]
    fn serde_roundtrip_preserves_config() {
        let config = OrchestratorConfig {
            max_loop_iterations: Some(4),
            max_tool_calls_per_turn: Some(8),
            max_subagent_depth: 1,
            max_parallel_subagents: 2,
            turn_timeout: Some(Duration::from_secs(10)),
            tool_timeouts: ToolTimeoutPolicy {
                fast_local_read: Duration::from_secs(2),
                search_and_mcp: Duration::from_secs(3),
                write_and_edit: Duration::from_secs(4),
                terminal_default: Duration::from_secs(5),
                terminal_max: Duration::from_secs(6),
            },
            subagent_timeout: Duration::from_secs(20),
            subagent_stall_timeout: Duration::from_secs(7),
            memory_limit_bytes: 4096,
            max_model_calls: Some(6),
            max_subagents: 3,
            max_output_bytes: Some(2048),
            max_parallel_tools: 3,
            max_input_tokens: Some(1000),
            max_output_tokens: Some(2000),
            max_output_continuations: 3,
            tool_list_updates: true,
            context_window_tokens: 250_000,
            stuck: StuckConfig {
                max_repeated_model_responses: 2,
                max_repeated_tool_calls: 2,
                max_failed_edit_attempts: 1,
                max_no_progress_iterations: 2,
            },
            reflection: ReflectionConfig {
                enabled: true,
                max_review_iterations: 2,
                max_fix_iterations: 1,
            },
            repair: RepairConfig { max_attempts: 2 },
            compaction: CompactionConfig { max_input_bytes: 2048, ..CompactionConfig::default() },
            recovery: RecoveryConfig { enabled: true, ..RecoveryConfig::default() },
            rubber_duck: RubberDuckConfig::default(),
            rubber_duck_triggers: RubberDuckTriggerConfig {
                mode: crate::rubber_duck_trigger::RubberDuckTriggerMode::Automatic,
            },
            session_log: SessionLogConfig::default(),
            stop_gate: StopGateConfig::default(),
            workspace_isolation: WorkspaceIsolationConfig::default(),
            approval_classifier: crate::approval_classifier::ApprovalClassifierConfig::auto(),
        };
        let json = serde_json::to_string(&config).expect("config serializes");
        let restored: OrchestratorConfig = serde_json::from_str(&json).expect("config parses");
        assert_eq!(restored.max_loop_iterations, Some(4));
        assert_eq!(restored.max_tool_calls_per_turn, Some(8));
        assert_eq!(restored.max_subagent_depth, 1);
        assert_eq!(restored.max_parallel_subagents, 2);
        assert_eq!(restored.turn_timeout, Some(Duration::from_secs(10)));
        assert_eq!(restored.tool_timeouts.fast_local_read, Duration::from_secs(2));
        assert_eq!(restored.tool_timeouts.search_and_mcp, Duration::from_secs(3));
        assert_eq!(restored.tool_timeouts.write_and_edit, Duration::from_secs(4));
        assert_eq!(restored.tool_timeouts.terminal_default, Duration::from_secs(5));
        assert_eq!(restored.tool_timeouts.terminal_max, Duration::from_secs(6));
        assert_eq!(restored.subagent_timeout, Duration::from_secs(20));
        assert_eq!(restored.subagent_stall_timeout, Duration::from_secs(7));
        assert_eq!(restored.memory_limit_bytes, 4096);
        assert_eq!(restored.max_model_calls, Some(6));
        assert_eq!(restored.max_subagents, 3);
        assert_eq!(restored.max_output_bytes, Some(2048));
        assert_eq!(restored.max_parallel_tools, 3);
        assert_eq!(restored.max_input_tokens, Some(1000));
        assert_eq!(restored.max_output_tokens, Some(2000));
        assert_eq!(restored.max_output_continuations, 3);
        assert_eq!(restored.context_window_tokens, 250_000);
        assert_eq!(restored.stuck.max_repeated_model_responses, 2);
        assert_eq!(restored.stuck.max_no_progress_iterations, 2);
        assert!(restored.reflection.enabled);
        assert_eq!(restored.reflection.max_review_iterations, 2);
        assert_eq!(restored.reflection.max_fix_iterations, 1);
        assert_eq!(restored.repair, RepairConfig { max_attempts: 2 });
        assert_eq!(restored.compaction.max_input_bytes, 2048);
        assert!(restored.recovery.enabled);
    }
}
