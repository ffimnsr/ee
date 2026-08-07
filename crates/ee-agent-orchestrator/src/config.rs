//! Orchestrator configuration.
//!
//! [`OrchestratorConfig`] carries the loop, tool, subagent, timeout, and
//! memory knobs that later phases wire into the loop engine, tool executor,
//! subagent manager, and memory store.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::compaction::CompactionConfig;
use crate::reflection::ReflectionConfig;
use crate::stuck::StuckConfig;

/// Default maximum loop iterations per turn.
pub const DEFAULT_MAX_LOOP_ITERATIONS: usize = 16;
/// Default maximum tool calls per turn.
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: usize = 180;
/// Default maximum subagent nesting depth.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: usize = 2;
/// Default maximum concurrently running subagents.
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 4;
/// Default per-turn wall-clock limit: 300 seconds.
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(300);
/// Default per-tool-call wall-clock limit: 120 seconds.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Default per-subagent wall-clock limit: 300 seconds.
pub const DEFAULT_SUBAGENT_TIMEOUT: Duration = Duration::from_secs(300);
/// Default memory byte limit: 1 MiB.
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 1024 * 1024;
/// Default maximum model adapter calls per turn.
pub const DEFAULT_MAX_MODEL_CALLS: usize = 16;
/// Default maximum subagent spawns per turn across all depths.
pub const DEFAULT_MAX_SUBAGENTS_PER_TURN: usize = 8;
/// Default maximum accumulated model output bytes per turn: 1 MiB.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Default maximum concurrently running independent read-only tools.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;

/// Shared configuration for one [`crate::OrchestratorRuntime`] instance.
///
/// Exhaustive so callers can use field updates off [`Default`]; construct
/// with [`OrchestratorConfig::default`] and override the knobs needed.
/// `PartialEq` is implemented (no `Eq`: the compaction config holds a
/// floating-point pressure knob).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Maximum model-call iterations per turn; the loop stops with a
    /// budget-exceeded error before exceeding this.
    pub max_loop_iterations: usize,
    /// Maximum tool calls per turn across all iterations.
    pub max_tool_calls_per_turn: usize,
    /// Maximum subagent nesting depth (subagent phase).
    pub max_subagent_depth: usize,
    /// Maximum concurrently running subagents (subagent phase).
    pub max_parallel_subagents: usize,
    /// Per-turn wall-clock limit.
    pub turn_timeout: Duration,
    /// Per-tool-call wall-clock limit.
    pub tool_timeout: Duration,
    /// Per-subagent wall-clock limit (subagent phase).
    pub subagent_timeout: Duration,
    /// Memory store byte limit.
    pub memory_limit_bytes: usize,
    /// Maximum model adapter invocations per turn; the loop stops with a
    /// budget-exceeded error before the next call.
    pub max_model_calls: usize,
    /// Maximum subagent spawns per turn across all depths.
    pub max_subagents: usize,
    /// Maximum accumulated model output bytes (text + reasoning) per turn.
    pub max_output_bytes: usize,
    /// Maximum concurrently running independent read-only tools in one
    /// planned batch; write/execute tools always run serially.
    pub max_parallel_tools: usize,
    /// Optional per-turn input-token cap; unknown provider usage fails closed.
    pub max_input_tokens: Option<usize>,
    /// Optional per-turn output-token cap; unknown provider usage fails closed.
    pub max_output_tokens: Option<usize>,
    /// Stuck-detection thresholds for the loop engine.
    pub stuck: StuckConfig,
    /// Bounded self-review behavior after tool/edit loops.
    pub reflection: ReflectionConfig,
    /// LLM compaction knobs (Phase 12): deterministic memory compaction
    /// settings plus the compaction-context input bound.
    pub compaction: CompactionConfig,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            max_loop_iterations: DEFAULT_MAX_LOOP_ITERATIONS,
            max_tool_calls_per_turn: DEFAULT_MAX_TOOL_CALLS_PER_TURN,
            max_subagent_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_parallel_subagents: DEFAULT_MAX_PARALLEL_SUBAGENTS,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            subagent_timeout: DEFAULT_SUBAGENT_TIMEOUT,
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            max_model_calls: DEFAULT_MAX_MODEL_CALLS,
            max_subagents: DEFAULT_MAX_SUBAGENTS_PER_TURN,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            max_input_tokens: None,
            max_output_tokens: None,
            stuck: StuckConfig::default(),
            reflection: ReflectionConfig::default(),
            compaction: CompactionConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_expected_knobs() {
        let config = OrchestratorConfig::default();
        assert_eq!(config.max_loop_iterations, 16);
        assert_eq!(config.max_tool_calls_per_turn, 180);
        assert_eq!(config.max_subagent_depth, 2);
        assert_eq!(config.max_parallel_subagents, 4);
        assert_eq!(config.turn_timeout, Duration::from_secs(300));
        assert_eq!(config.tool_timeout, Duration::from_secs(120));
        assert_eq!(config.subagent_timeout, Duration::from_secs(300));
        assert_eq!(config.memory_limit_bytes, 1024 * 1024);
        assert_eq!(config.max_model_calls, 16);
        assert_eq!(config.max_subagents, 8);
        assert_eq!(config.max_output_bytes, 1024 * 1024);
        assert_eq!(config.max_parallel_tools, 4);
        assert_eq!(config.max_input_tokens, None);
        assert_eq!(config.max_output_tokens, None);
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
        assert_eq!(config.compaction, CompactionConfig::default());
    }

    #[test]
    fn serde_roundtrip_preserves_config() {
        let config = OrchestratorConfig {
            max_loop_iterations: 4,
            max_tool_calls_per_turn: 8,
            max_subagent_depth: 1,
            max_parallel_subagents: 2,
            turn_timeout: Duration::from_secs(10),
            tool_timeout: Duration::from_secs(5),
            subagent_timeout: Duration::from_secs(20),
            memory_limit_bytes: 4096,
            max_model_calls: 6,
            max_subagents: 3,
            max_output_bytes: 2048,
            max_parallel_tools: 3,
            max_input_tokens: Some(1000),
            max_output_tokens: Some(2000),
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
            compaction: CompactionConfig { max_input_bytes: 2048, ..CompactionConfig::default() },
        };
        let json = serde_json::to_string(&config).expect("config serializes");
        let restored: OrchestratorConfig = serde_json::from_str(&json).expect("config parses");
        assert_eq!(restored.max_loop_iterations, 4);
        assert_eq!(restored.max_tool_calls_per_turn, 8);
        assert_eq!(restored.max_subagent_depth, 1);
        assert_eq!(restored.max_parallel_subagents, 2);
        assert_eq!(restored.turn_timeout, Duration::from_secs(10));
        assert_eq!(restored.tool_timeout, Duration::from_secs(5));
        assert_eq!(restored.subagent_timeout, Duration::from_secs(20));
        assert_eq!(restored.memory_limit_bytes, 4096);
        assert_eq!(restored.max_model_calls, 6);
        assert_eq!(restored.max_subagents, 3);
        assert_eq!(restored.max_output_bytes, 2048);
        assert_eq!(restored.max_parallel_tools, 3);
        assert_eq!(restored.max_input_tokens, Some(1000));
        assert_eq!(restored.max_output_tokens, Some(2000));
        assert_eq!(restored.stuck.max_repeated_model_responses, 2);
        assert_eq!(restored.stuck.max_no_progress_iterations, 2);
        assert!(restored.reflection.enabled);
        assert_eq!(restored.reflection.max_review_iterations, 2);
        assert_eq!(restored.reflection.max_fix_iterations, 1);
        assert_eq!(restored.compaction.max_input_bytes, 2048);
    }
}
