//! Ordered, lossless-before-lossy compaction pipeline.
//!
//! Stages run in this order: request projection, lossless pruning, windowing,
//! deterministic memory compaction, and prompt/context assembly. The LLM
//! summary call stays in the runtime so this module remains synchronous and
//! testable.
//!
//! Lossless pruning is deliberately narrow: only tool results explicitly
//! marked as read-only are considered, and only their compaction-view copy is
//! rewritten. Durable transcripts, checkpoints, and evidence are untouched.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::memory::MemoryStore;
use crate::memory_compaction::{CompactionReport, MemoryCompactionConfig, compact_memory};
use crate::model::{
    ModelContent, ModelMessage, ModelRole, TOOL_PATH_METADATA_KEY, TOOL_SIDE_EFFECT_METADATA_KEY,
};
use crate::tasks::TaskGraph;

use super::{MAX_RECENT_MESSAGES, build_compaction_context, build_compaction_prompt};

/// Marker substituted for a duplicate or superseded read-only tool result.
pub const DUPLICATE_TOOL_RESULT_MARKER: &str = "[... duplicate tool result elided ...]";

/// Default number of most recent user turns whose multimodal payloads stay
/// intact before compaction offloads older binary blocks.
pub const DEFAULT_KEEP_RECENT_MULTIMODAL_TURNS: usize = 2;

/// Inputs for one synchronous compaction plan.
#[derive(Debug, Clone, Copy)]
pub struct CompactionInput<'a> {
    /// Current task graph.
    pub tasks: &'a TaskGraph,
    /// Live memory store. The pipeline clones it before any mutation.
    pub memory: &'a MemoryStore,
    /// Deterministic memory compaction settings and protected-key policy.
    pub memory_config: &'a MemoryCompactionConfig,
    /// Provider-owned recent conversation.
    pub recent_messages: &'a [ModelMessage],
    /// Recent runtime events.
    pub recent_events: &'a [crate::events::OrchestratorEvent],
    /// Budget snapshot included in the evidence context.
    pub budget: &'a crate::budget::BudgetSnapshot,
    /// Maximum serialized context bytes.
    pub max_input_bytes: usize,
    /// Optional user instructions appended to the compaction prompt.
    pub instructions: Option<&'a str>,
    /// Number of most recent user turns whose multimodal blocks stay intact.
    pub keep_recent_multimodal_turns: usize,
}

/// Per-stage accounting for one compaction plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionStageCounts {
    /// Messages supplied before projection.
    pub requested_messages: usize,
    /// Read-only results rewritten by lossless pruning.
    pub pruned_tool_results: usize,
    /// Rewrites caused by byte-identical duplicate results.
    pub deduped_messages: usize,
    /// Binary multimodal blocks replaced by deterministic text placeholders.
    pub offloaded_multimodal: usize,
    /// Oldest messages dropped by the window stage.
    pub dropped_messages: usize,
    /// Messages retained for the compaction context.
    pub retained_messages: usize,
    /// Bytes of the bounded evidence context.
    pub retained_context_bytes: usize,
}

/// Fully planned compaction work. Memory is staged, never live-mutated.
#[derive(Debug, Clone)]
pub struct CompactPlan {
    /// Deterministically compacted memory to commit after a successful call.
    pub staged_memory: MemoryStore,
    /// What the deterministic stage changed.
    pub deterministic: CompactionReport,
    /// Projected, pruned, windowed messages used for evidence assembly.
    pub messages: Vec<ModelMessage>,
    /// Bounded evidence context.
    pub context: String,
    /// Compaction instruction prompt.
    pub prompt: String,
    /// Per-stage counts.
    pub counts: CompactionStageCounts,
}

/// Pluggable compaction planner.
///
/// Implementations must be synchronous and side-effect free: the runtime owns
/// the model call and the staged commit. Only host-approved strategies are
/// selectable by name; untrusted plugin strategies never load implicitly.
pub trait CompactionStrategy {
    /// Stable strategy name used in configuration.
    fn name(&self) -> &'static str;

    /// Plans projection, pruning, windowing, memory, and context assembly.
    fn plan(&self, input: &CompactionInput<'_>) -> CompactPlan;
}

/// The default strategy: preserve current behavior unless a read result is a
/// duplicate or was superseded by a later read of the same path.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCompactionStrategy;

impl CompactionStrategy for DefaultCompactionStrategy {
    fn name(&self) -> &'static str {
        super::DEFAULT_COMPACTION_STRATEGY
    }

    fn plan(&self, input: &CompactionInput<'_>) -> CompactPlan {
        let requested_messages = input.recent_messages.len();
        // Stage 1: request projection without mutating durable messages.
        let projected: Vec<ModelMessage> = input
            .recent_messages
            .iter()
            .map(|message| message.with_pruned_tool_result().unwrap_or_else(|| message.clone()))
            .collect();
        // Stage 2: lossless pruning of read-only tool results.
        let (pruned, pruning) = prune_read_only_results(projected);
        // Stage 2b: offload stale binary multimodal payloads so the summary
        // request and retained window keep their context budget. The durable
        // session log still holds the original bytes for replay and audit.
        let (offloaded, offloaded_count) =
            offload_stale_multimodal(pruned, input.keep_recent_multimodal_turns);
        // Stage 3: deterministic window for the retained evidence tail.
        let dropped = offloaded.len().saturating_sub(MAX_RECENT_MESSAGES);
        let windowed: Vec<ModelMessage> =
            offloaded.iter().skip(dropped).take(MAX_RECENT_MESSAGES).cloned().collect();
        let retained_messages = windowed.len();
        // Stage 4: deterministic memory compaction on a clone.
        let mut staged_memory = input.memory.clone();
        let deterministic = compact_memory(&mut staged_memory, input.memory_config);
        // Stage 5: bounded evidence context and LLM instruction prompt. The
        // context receives the full pruned slice so turns that fall outside the
        // retained window are folded into the summary instead of silently
        // disappearing; byte bounding drops the oldest lines first.
        let context = build_compaction_context(
            input.tasks,
            &staged_memory,
            &offloaded,
            input.recent_events,
            input.budget,
            input.max_input_bytes,
        );
        let prompt = build_compaction_prompt(input.instructions);
        CompactPlan {
            staged_memory,
            deterministic,
            messages: windowed,
            counts: CompactionStageCounts {
                requested_messages,
                pruned_tool_results: pruning.pruned,
                deduped_messages: pruning.deduped,
                offloaded_multimodal: offloaded_count,
                dropped_messages: dropped,
                retained_messages,
                retained_context_bytes: context.len(),
            },
            context,
            prompt,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PruningCounts {
    pruned: usize,
    deduped: usize,
}

/// Losslessly elides duplicate or superseded read-only tool results from one
/// request projection. The durable transcript is untouched; only the copy sent
/// to the model is rewritten.
#[must_use]
pub fn prune_redundant_readonly_tool_outputs(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    let projected: Vec<ModelMessage> = messages
        .into_iter()
        .map(|message| message.with_pruned_tool_result().unwrap_or(message))
        .collect();
    prune_read_only_results(projected).0
}

/// Replaces binary multimodal blocks older than the most recent
/// `keep_recent_user_turns` user turns with deterministic text placeholders.
///
/// The durable session log keeps the original bytes, so replay and audit stay
/// lossless; only the compaction request projection is rewritten. Size is the
/// decoded payload length, and the short hash is the first 8 SHA-256 bytes of
/// the encoded payload.
#[must_use]
pub fn offload_stale_multimodal(
    messages: Vec<ModelMessage>,
    keep_recent_user_turns: usize,
) -> (Vec<ModelMessage>, usize) {
    let cutoff = multimodal_retention_start(&messages, keep_recent_user_turns);
    let mut offloaded = 0usize;
    let rewritten = messages
        .into_iter()
        .enumerate()
        .map(|(index, mut message)| {
            if index >= cutoff {
                return message;
            }
            let content = std::mem::take(&mut message.content);
            message.content = content
                .into_iter()
                .map(|block| match offload_block(&block) {
                    Some(placeholder) => {
                        offloaded += 1;
                        ModelContent::Text(placeholder)
                    }
                    None => block,
                })
                .collect();
            message
        })
        .collect();
    (rewritten, offloaded)
}

/// Index of the first message belonging to the most recent `keep` user turns.
///
/// Messages before it are candidates for multimodal offload. Zero retention
/// keeps nothing intact; a transcript with fewer user turns keeps everything.
fn multimodal_retention_start(messages: &[ModelMessage], keep: usize) -> usize {
    if keep == 0 {
        return messages.len();
    }
    let mut seen = 0usize;
    for (index, message) in messages.iter().enumerate().rev() {
        if is_user_turn(message) {
            seen += 1;
            if seen == keep {
                return index;
            }
        }
    }
    0
}

/// A user turn opens with a user/subagent message that carries text (the
/// prompt). Standalone media blocks do not open a new turn on their own.
fn is_user_turn(message: &ModelMessage) -> bool {
    matches!(message.role, ModelRole::User | ModelRole::Subagent)
        && message.content.iter().any(|block| matches!(block, ModelContent::Text(_)))
}

/// Returns a deterministic placeholder for one binary block, or `None` for
/// blocks that never carry a binary payload.
fn offload_block(block: &ModelContent) -> Option<String> {
    match block {
        ModelContent::Image { data, mime_type, .. } => {
            Some(multimodal_placeholder("image", mime_type, decoded_base64_len(data), data))
        }
        ModelContent::Audio { data, mime_type } => {
            Some(multimodal_placeholder("audio", mime_type, decoded_base64_len(data), data))
        }
        ModelContent::EmbeddedResource { uri, mime_type, blob: Some(blob), .. } => Some(format!(
            "[resource offloaded: uri={uri}, mime={}, size={}B, sha256={}]",
            mime_type.as_deref().unwrap_or("application/octet-stream"),
            decoded_base64_len(blob),
            short_sha256(blob),
        )),
        _ => None,
    }
}

fn multimodal_placeholder(kind: &str, mime: &str, size: usize, payload: &str) -> String {
    format!("[{kind} offloaded: mime={mime}, size={size}B, sha256={}]", short_sha256(payload))
}

/// First 8 SHA-256 bytes rendered as lowercase hex.
fn short_sha256(payload: &str) -> String {
    let digest = Sha256::digest(payload.as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// Decoded byte length of a base64 payload without allocating.
fn decoded_base64_len(encoded: &str) -> usize {
    if encoded.is_empty() {
        return 0;
    }
    let padding = encoded.bytes().rev().take_while(|byte| *byte == b'=').count().min(2);
    (encoded.len() / 4).saturating_mul(3).saturating_sub(padding)
}

/// Rewrites only read-only tool results that are exact duplicates or were
/// superseded by a later read of the same path.
fn prune_read_only_results(messages: Vec<ModelMessage>) -> (Vec<ModelMessage>, PruningCounts) {
    let mut latest_by_path: HashMap<String, usize> = HashMap::new();
    let mut latest_by_content: HashMap<String, usize> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if !is_read_only_result(message) {
            continue;
        }
        if let Some(path) = message.metadata.get(TOOL_PATH_METADATA_KEY) {
            latest_by_path.insert(path.clone(), index);
        }
        latest_by_content.insert(result_key(message), index);
    }

    let mut counts = PruningCounts::default();
    let mut output = Vec::with_capacity(messages.len());
    for (index, message) in messages.into_iter().enumerate() {
        if !is_read_only_result(&message) {
            output.push(message);
            continue;
        }
        let superseded_path = message
            .metadata
            .get(TOOL_PATH_METADATA_KEY)
            .is_some_and(|path| latest_by_path.get(path).is_some_and(|last| *last > index));
        let superseded_content =
            latest_by_content.get(&result_key(&message)).is_some_and(|last| *last > index);
        if superseded_path || superseded_content {
            counts.pruned += 1;
            if superseded_content {
                counts.deduped += 1;
            }
            output.push(elide_result(message));
        } else {
            output.push(message);
        }
    }
    (output, counts)
}

fn is_read_only_result(message: &ModelMessage) -> bool {
    message.role == crate::model::ModelRole::Tool
        && message.metadata.get(TOOL_SIDE_EFFECT_METADATA_KEY).map(String::as_str) == Some("read")
        && message.content.iter().any(|block| matches!(block, ModelContent::ToolResult { .. }))
}

fn result_key(message: &ModelMessage) -> String {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ModelContent::ToolResult { result, .. } => {
                Some(serde_json::to_string(result).unwrap_or_else(|_| result.text_output.clone()))
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn elide_result(mut message: ModelMessage) -> ModelMessage {
    for block in &mut message.content {
        if let ModelContent::ToolResult { result, .. } = block {
            result.text_output = DUPLICATE_TOOL_RESULT_MARKER.to_string();
            result.structured_output = None;
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetSnapshot;
    use crate::events::OrchestratorEvent;
    use crate::memory::MemoryItem;
    use crate::model::{ModelMessage, ModelRole, TOOL_NAME_METADATA_KEY};
    use crate::tasks::TaskId;
    use crate::tools::ToolResult;

    fn budget() -> BudgetSnapshot {
        BudgetSnapshot {
            iterations_used: 1,
            iterations_max: None,
            model_calls_used: 1,
            model_calls_max: None,
            tool_calls_used: 0,
            tool_calls_max: None,
            subagents_used: 0,
            subagents_max: 8,
            output_bytes_used: 0,
            output_bytes_max: None,
            input_tokens_used: None,
            input_tokens_max: None,
            output_tokens_used: None,
            output_tokens_max: None,
        }
    }

    fn tool_message(
        call: &str,
        name: &str,
        class: &str,
        path: Option<&str>,
        text: &str,
    ) -> ModelMessage {
        let mut message =
            ModelMessage::tool_result(call, ToolResult::success(text)).with_metadata([
                (TOOL_NAME_METADATA_KEY.to_string(), name.to_string()),
                (TOOL_SIDE_EFFECT_METADATA_KEY.to_string(), class.to_string()),
            ]);
        if let Some(path) = path {
            message.metadata.insert(TOOL_PATH_METADATA_KEY.to_string(), path.to_string());
        }
        message
    }

    fn plan(messages: &[ModelMessage], memory: &MemoryStore) -> CompactPlan {
        let tasks = TaskGraph::new();
        let events =
            [OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "t-1".into() }];
        DefaultCompactionStrategy.plan(&CompactionInput {
            tasks: &tasks,
            memory,
            memory_config: &MemoryCompactionConfig::default(),
            recent_messages: messages,
            recent_events: &events,
            budget: &budget(),
            max_input_bytes: 64 * 1024,
            instructions: None,
            keep_recent_multimodal_turns: 2,
        })
    }

    #[test]
    fn exact_duplicate_read_result_is_pruned_once() {
        let memory = MemoryStore::new(4_096);
        let messages = vec![
            tool_message("tc-1", "read_file", "read", Some("a.rs"), "same"),
            ModelMessage::text(ModelRole::Assistant, "between"),
            tool_message("tc-2", "read_file", "read", Some("b.rs"), "same"),
        ];
        let plan = plan(&messages, &memory);
        assert_eq!(plan.counts.pruned_tool_results, 1);
        assert_eq!(plan.counts.deduped_messages, 1);
        assert_eq!(
            plan.messages[0].content[0],
            ModelContent::ToolResult {
                tool_call_id: "tc-1".into(),
                result: ToolResult {
                    success: true,
                    text_output: DUPLICATE_TOOL_RESULT_MARKER.into(),
                    structured_output: None,
                    error_kind: None,
                },
            }
        );
        assert!(matches!(
            &plan.messages[2].content[0],
            ModelContent::ToolResult { result, .. } if result.text_output == "same"
        ));
    }

    #[test]
    fn superseded_read_is_pruned_and_later_read_survives() {
        let memory = MemoryStore::new(4_096);
        let messages = vec![
            tool_message("tc-1", "read_file", "read", Some("a.rs"), "old"),
            tool_message("tc-2", "read_file", "read", Some("a.rs"), "new"),
        ];
        let plan = plan(&messages, &memory);
        assert_eq!(plan.counts.pruned_tool_results, 1);
        assert_eq!(plan.counts.deduped_messages, 0);
        assert!(matches!(
            &plan.messages[0].content[0],
            ModelContent::ToolResult { result, .. }
                if result.text_output == DUPLICATE_TOOL_RESULT_MARKER
        ));
        assert!(matches!(
            &plan.messages[1].content[0],
            ModelContent::ToolResult { result, .. } if result.text_output == "new"
        ));
    }

    #[test]
    fn context_includes_messages_outside_the_retained_window() {
        let memory = MemoryStore::new(4_096);
        let messages: Vec<ModelMessage> = (0..MAX_RECENT_MESSAGES + 10)
            .map(|index| ModelMessage::text(ModelRole::User, format!("turn {index}")))
            .collect();

        let plan = plan(&messages, &memory);

        assert_eq!(plan.counts.dropped_messages, 10);
        assert!(
            plan.context.contains("turn 0") && plan.context.contains("turn 29"),
            "older turns reach the summary context: {}",
            plan.context
        );
    }

    #[test]
    fn public_pruning_helper_elides_duplicate_reads_losslessly() {
        let messages = vec![
            tool_message("tc-1", "read_file", "read", Some("a.rs"), "same"),
            tool_message("tc-2", "read_file", "read", Some("b.rs"), "same"),
        ];

        let pruned = prune_redundant_readonly_tool_outputs(messages);

        assert!(matches!(
            &pruned[0].content[0],
            ModelContent::ToolResult { result, .. }
                if result.text_output == DUPLICATE_TOOL_RESULT_MARKER
        ));
        assert!(matches!(
            &pruned[1].content[0],
            ModelContent::ToolResult { result, .. } if result.text_output == "same"
        ));
    }

    #[test]
    fn stale_multimodal_blocks_are_offloaded_with_deterministic_placeholders() {
        let memory = MemoryStore::new(4_096);
        let image = |id: &str, payload: &str| {
            ModelMessage::new(ModelRole::User).with_content(vec![ModelContent::Image {
                data: payload.into(),
                mime_type: "image/png".into(),
                uri: Some(id.into()),
            }])
        };
        let messages = vec![
            image("old-1", "AAAA"),
            ModelMessage::text(ModelRole::Assistant, "looked at it"),
            image("old-2", "BBBB"),
            ModelMessage::text(ModelRole::User, "second request"),
            image("recent-1", "CCCC"),
            ModelMessage::text(ModelRole::User, "latest request"),
            image("recent-2", "DDDD"),
        ];

        let plan = plan(&messages, &memory);

        assert_eq!(plan.counts.offloaded_multimodal, 2, "only stale turns are offloaded");
        let placeholder = |message: &ModelMessage| match &message.content[0] {
            ModelContent::Text(text) => text.clone(),
            other => panic!("expected text placeholder, got {other:?}"),
        };
        let first = placeholder(&plan.messages[0]);
        assert_eq!(first, "[image offloaded: mime=image/png, size=3B, sha256=63c1dd951ffedf6f]");
        assert!(matches!(plan.messages[4].content[0], ModelContent::Image { .. }));
        assert!(matches!(plan.messages[6].content[0], ModelContent::Image { .. }));
    }

    #[test]
    fn zero_multimodal_retention_offloads_every_binary_block() {
        let messages = vec![
            ModelMessage::new(ModelRole::User).with_content(vec![ModelContent::Audio {
                data: "QUJD".into(),
                mime_type: "audio/wav".into(),
            }]),
            ModelMessage::text(ModelRole::User, "latest"),
        ];

        let (offloaded, count) = offload_stale_multimodal(messages, 0);

        assert_eq!(count, 1);
        assert!(
            matches!(&offloaded[0].content[0], ModelContent::Text(text) if text.starts_with("[audio offloaded: mime=audio/wav, size=3B"))
        );
    }

    #[test]
    fn write_execute_and_delegate_results_are_never_pruned() {
        let memory = MemoryStore::new(4_096);
        let messages = vec![
            tool_message("tc-1", "write_file", "write", Some("a.rs"), "same"),
            tool_message("tc-2", "terminal", "execute", Some("a.rs"), "same"),
            tool_message("tc-3", "delegate_task", "delegate", None, "same"),
        ];
        let plan = plan(&messages, &memory);
        assert_eq!(plan.counts.pruned_tool_results, 0);
        assert_eq!(plan.messages.len(), 3);
    }

    #[test]
    fn unclassified_tool_results_are_never_pruned() {
        let memory = MemoryStore::new(4_096);
        let messages = vec![
            ModelMessage::tool_result("tc-1", ToolResult::success("same")),
            ModelMessage::tool_result("tc-2", ToolResult::success("same")),
        ];
        let plan = plan(&messages, &memory);
        assert_eq!(plan.counts.pruned_tool_results, 0);
    }

    #[test]
    fn protected_memory_survives_the_deterministic_stage() {
        let mut memory = MemoryStore::new(4_096);
        memory.insert(MemoryItem::new("decision:api", "use v2")).expect("inserts");
        memory.insert(MemoryItem::new("constraint:offline", "no network")).expect("inserts");
        memory.insert(MemoryItem::new("validation:tests", "pass")).expect("inserts");
        memory
            .insert(MemoryItem::from_task("obs:file", "old", TaskId::new("task-1")))
            .expect("inserts");
        memory
            .insert(MemoryItem::from_task("obs:file", "new", TaskId::new("task-1")))
            .expect("inserts");

        let plan = plan(&[], &memory);
        assert_eq!(plan.deterministic.merged_duplicates, 1);
        assert!(plan.staged_memory.query("decision:api").is_some());
        assert!(plan.staged_memory.query("constraint:offline").is_some());
        assert!(plan.staged_memory.query("validation:tests").is_some());
        assert_eq!(
            plan.staged_memory.query("obs:file").map(|item| item.value.clone()),
            Some("new".to_string())
        );
        assert!(memory.query("obs:file").is_some(), "live memory stays untouched");
    }

    #[test]
    fn default_strategy_is_byte_identical_when_no_prune_applies() {
        let mut memory = MemoryStore::new(4_096);
        memory.insert(MemoryItem::new("decision:api", "use v2")).expect("inserts");
        let tasks = TaskGraph::new();
        let events =
            [OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "t-1".into() }];
        let messages = vec![
            ModelMessage::text(ModelRole::User, "earlier request"),
            ModelMessage::text(ModelRole::Assistant, "answer"),
        ];
        let expected =
            build_compaction_context(&tasks, &memory, &messages, &events, &budget(), 4_096);
        let plan = DefaultCompactionStrategy.plan(&CompactionInput {
            tasks: &tasks,
            memory: &memory,
            memory_config: &MemoryCompactionConfig::default(),
            recent_messages: &messages,
            recent_events: &events,
            budget: &budget(),
            max_input_bytes: 4_096,
            instructions: None,
            keep_recent_multimodal_turns: 2,
        });
        assert_eq!(plan.counts.pruned_tool_results, 0);
        assert_eq!(plan.counts.deduped_messages, 0);
        assert_eq!(plan.context, expected);
    }

    #[test]
    fn window_drops_oldest_messages_and_counts_them() {
        let memory = MemoryStore::new(4_096);
        let messages: Vec<ModelMessage> = (0..MAX_RECENT_MESSAGES + 3)
            .map(|index| ModelMessage::text(ModelRole::User, format!("m{index}")))
            .collect();
        let plan = plan(&messages, &memory);
        assert_eq!(plan.counts.dropped_messages, 3);
        assert_eq!(plan.messages.len(), MAX_RECENT_MESSAGES);
        assert_eq!(plan.messages[0].text_content(), format!("m{}", 3));
    }

    #[test]
    fn funnel_prunes_before_window_drops_messages() {
        let memory = MemoryStore::new(4_096);
        let mut messages = vec![tool_message("tc-0", "read_file", "read", Some("a.rs"), "same")];
        for index in 0..MAX_RECENT_MESSAGES - 1 {
            messages.push(ModelMessage::text(ModelRole::Assistant, format!("m{index}")));
        }
        messages.push(tool_message("tc-1", "read_file", "read", Some("b.rs"), "same"));
        let plan = plan(&messages, &memory);
        assert_eq!(plan.counts.dropped_messages, 1, "window drops the oldest message");
        assert_eq!(plan.counts.pruned_tool_results, 1, "prune runs before the window");
        assert_eq!(plan.counts.deduped_messages, 1);
    }
}
