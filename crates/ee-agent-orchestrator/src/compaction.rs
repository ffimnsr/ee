//! LLM-assisted session compaction (Phase 12).
//!
//! `/compact` turns (agent-advertised slash commands arriving as ordinary
//! `session/prompt` text) run a single model call over a provenance-rich,
//! byte-bounded context and store the result as model-derived session
//! memory.  Deterministic `compact_memory` runs first and stays the only
//! mechanism that removes structured memory; the LLM summary is purely
//! additive and can never delete decisions, constraints, or validation
//! results.  The compaction model call is made without tools and is bounded
//! by the configured input bytes and the per-turn timeout, with
//! cancellation observed before and after the call.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSnapshot;
use crate::memory::MemoryStore;
use crate::memory_compaction::{MemoryCompactionConfig, is_protected_key};
use crate::sensitive_data::SensitiveDataGuard;
use crate::tasks::{TaskGraph, TaskStatus};

/// Default bound on the serialized compaction context sent to the model.
pub const DEFAULT_COMPACT_MAX_INPUT_BYTES: usize = 64 * 1024;

/// The memory key under which the model-derived summary is stored.
pub const SESSION_SUMMARY_KEY: &str = "summary:session";

/// Compaction knobs for `/compact` turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Deterministic memory compaction settings (duplicate merging and
    /// pressure decay; protected keys are never touched).
    pub memory: MemoryCompactionConfig,
    /// Maximum serialized bytes of the compaction context (task graph,
    /// memory, validation facts, budget) sent to the model.
    pub max_input_bytes: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            memory: MemoryCompactionConfig::default(),
            max_input_bytes: DEFAULT_COMPACT_MAX_INPUT_BYTES,
        }
    }
}

/// What one `/compact` turn did, surfaced as a redacted status message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompactTurnReport {
    /// Duplicate facts merged away by the deterministic pass.
    pub merged_duplicates: usize,
    /// Low-value observations decayed by the deterministic pass.
    pub decayed_observations: usize,
    /// Protected items present before compaction (all preserved).
    pub preserved_protected: usize,
    /// Bytes of the stored model-derived summary item.
    pub summary_bytes: usize,
    /// Bytes of the compaction context retained in the model request.
    pub retained_context_bytes: usize,
}

impl CompactTurnReport {
    /// Redacted user-visible status text for the completion message.
    #[must_use]
    pub fn to_status_text(&self) -> String {
        SensitiveDataGuard::new().redact(&format!(
            "Session compacted: merged {} duplicate facts, decayed {} low-value observations, preserved {} protected items; stored {} summary bytes; retained {} context bytes",
            self.merged_duplicates,
            self.decayed_observations,
            self.preserved_protected,
            self.summary_bytes,
            self.retained_context_bytes,
        ))
    }
}

/// Builds the compaction prompt asking the model for the sections a
/// continuation summary must preserve; optional user instructions are
/// appended verbatim.
#[must_use]
pub fn build_compaction_prompt(instructions: Option<&str>) -> String {
    let mut prompt = String::from(
        "Write a compact continuation summary of this agent session. Cover:\n\
         - the user goal\n\
         - completed work\n\
         - the current state\n\
         - important files and symbols\n\
         - decisions and constraints\n\
         - pending work\n\
         - validation status\n\
         - risks and errors\n\
         Keep the summary high-signal and provenance-aware: preserve facts a continuation needs.",
    );
    if let Some(instructions) = instructions.filter(|text| !text.trim().is_empty()) {
        prompt.push_str("\nAdditional instructions: ");
        prompt.push_str(instructions.trim());
        prompt.push('\n');
    }
    prompt
}

/// Builds the provenance-rich compaction context: the task graph, memory
/// facts with source/trust provenance, explicit validation facts, and the
/// budget snapshot.  The output is bounded to `max_bytes` deterministically:
/// oldest memory lines drop first, then validation lines, then the whole
/// text is truncated at a char boundary.
#[must_use]
pub fn build_compaction_context(
    tasks: &TaskGraph,
    memory: &MemoryStore,
    budget: &BudgetSnapshot,
    max_bytes: usize,
) -> String {
    let task_section = {
        let mut lines = Vec::new();
        for task in tasks.list() {
            let status = match task.status {
                TaskStatus::Pending => "pending",
                TaskStatus::Running => "running",
                TaskStatus::Completed => "completed",
                TaskStatus::Failed => "failed",
                TaskStatus::Blocked => "blocked",
                TaskStatus::Cancelled => "cancelled",
            };
            lines.push(format!("- {}: \"{}\" [{}]", task.id.as_str(), task.title, status));
        }
        if lines.is_empty() {
            lines.push(String::from("- (no tasks yet)"));
        }
        format!("Task graph:\n{}", lines.join("\n"))
    };
    let budget_section = format!("Budget:\n{budget}");

    let mut memory_lines = Vec::new();
    for item in memory.items() {
        let source = item
            .source_task
            .as_ref()
            .map_or_else(|| "session".to_string(), |task| task.as_str().to_string());
        memory_lines.push(format!(
            "- {}: {} (source: {}, trust: {})",
            item.key,
            item.value,
            source,
            item.trust.label()
        ));
    }
    if memory_lines.is_empty() {
        memory_lines.push(String::from("- (no memory facts)"));
    }

    let mut validation_lines = Vec::new();
    for item in memory.items() {
        if item.key.starts_with("validation:") && is_protected_key(&item.key) {
            validation_lines.push(format!("- {}: {}", item.key, item.value));
        }
    }
    if validation_lines.is_empty() {
        validation_lines.push(String::from("- (none)"));
    }

    let assemble = |memory_lines: &[String], validation_lines: &[String]| {
        let sections = [
            task_section.clone(),
            format!("Memory facts:\n{}", memory_lines.join("\n")),
            format!("Validation facts:\n{}", validation_lines.join("\n")),
            budget_section.clone(),
        ];
        sections.join("\n\n")
    };

    let mut text = assemble(&memory_lines, &validation_lines);
    // Oldest memory lines drop first, then validation lines, then the whole
    // text is truncated at a char boundary.
    while text.len() > max_bytes && memory_lines.len() > 1 {
        memory_lines.remove(0);
        text = assemble(&memory_lines, &validation_lines);
    }
    while text.len() > max_bytes && validation_lines.len() > 1 {
        validation_lines.remove(0);
        text = assemble(&memory_lines, &validation_lines);
    }
    if text.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryItem;
    use crate::memory_compaction::{PROTECTED_MEMORY_PREFIXES, compact_memory};
    use crate::tasks::TaskId;
    use crate::trust::TrustLevel;

    fn store_with_items() -> MemoryStore {
        let mut store = MemoryStore::new(4_096);
        store.insert(MemoryItem::new("decision:api", "use v2")).expect("inserts");
        store
            .insert(MemoryItem::from_task("obs:file", "read a.rs", TaskId::new("task-1")))
            .expect("inserts");
        store.insert(MemoryItem::new("constraint:no-network", "offline")).expect("inserts");
        store.insert(MemoryItem::new("validation:tests", "all pass")).expect("inserts");
        store
            .insert(MemoryItem::new("cwd", "/work").with_trust(TrustLevel::UserPrompt))
            .expect("inserts");
        store
    }

    fn budget_snapshot() -> BudgetSnapshot {
        BudgetSnapshot {
            iterations_used: 3,
            iterations_max: 16,
            model_calls_used: 3,
            model_calls_max: 16,
            tool_calls_used: 1,
            tool_calls_max: 32,
            subagents_used: 0,
            subagents_max: 8,
            output_bytes_used: 42,
            output_bytes_max: 1024,
            input_tokens_used: None,
            input_tokens_max: None,
            output_tokens_used: None,
            output_tokens_max: None,
        }
    }

    #[test]
    fn compaction_prompt_covers_required_sections() {
        let prompt = build_compaction_prompt(None);
        for section in [
            "user goal",
            "completed work",
            "current state",
            "important files and symbols",
            "decisions and constraints",
            "pending work",
            "validation status",
            "risks and errors",
        ] {
            assert!(prompt.contains(section), "prompt must cover {section:?}");
        }
    }

    #[test]
    fn compaction_prompt_appends_instructions_verbatim() {
        let prompt = build_compaction_prompt(Some("  keep API v2  "));
        assert!(prompt.contains("keep API v2"), "{prompt}");
        assert!(prompt.contains("Additional instructions"), "{prompt}");
        assert!(!build_compaction_prompt(None).contains("Additional instructions"));
    }

    #[test]
    fn context_includes_provenance_validation_and_budget() {
        let mut tasks = TaskGraph::new();
        tasks.create_root("fix parser", "make the parser correct");
        let store = store_with_items();
        let context = build_compaction_context(&tasks, &store, &budget_snapshot(), 4_096);

        assert!(context.contains("Task graph:"), "{context}");
        assert!(context.contains("fix parser"), "{context}");
        assert!(context.contains("Memory facts:"), "{context}");
        assert!(
            context.contains("obs:file: read a.rs (source: task-1, trust: tool_output"),
            "{context}"
        );
        assert!(context.contains("decision:api"), "{context}");
        assert!(context.contains("Validation facts:"), "{context}");
        assert!(context.contains("validation:tests: all pass"), "{context}");
        assert!(context.contains("Budget:"), "{context}");
        assert!(context.contains("iterations 3/16"), "{context}");
    }

    #[test]
    fn context_is_bounded_by_dropping_oldest_lines_first() {
        let mut store = MemoryStore::new(4_096);
        for index in 0..50 {
            store.insert(MemoryItem::new(format!("obs:{index}"), "v".repeat(20))).expect("inserts");
        }
        store.insert(MemoryItem::new("decision:keep", "yes")).expect("inserts");
        let tasks = TaskGraph::new();
        let context = build_compaction_context(&tasks, &store, &budget_snapshot(), 700);
        assert!(context.len() <= 700, "bounded: {} bytes", context.len());
        // The newest memory line survives; the oldest dropped first.
        assert!(context.contains("obs:49"), "{context}");
        assert!(!context.contains("obs:0"), "{context}");
        assert!(context.contains("decision:keep"), "protected items survive bounding");
        assert!(context.contains("Task graph:"), "task section never dropped");
    }

    #[test]
    fn report_status_is_redacted_and_covers_all_fields() {
        let report = CompactTurnReport {
            merged_duplicates: 2,
            decayed_observations: 1,
            preserved_protected: 3,
            summary_bytes: 120,
            retained_context_bytes: 4_000,
        };
        let text = report.to_status_text();
        assert!(text.contains("merged 2 duplicate facts"), "{text}");
        assert!(text.contains("decayed 1 low-value observations"), "{text}");
        assert!(text.contains("preserved 3 protected items"), "{text}");
        assert!(text.contains("stored 120 summary bytes"), "{text}");
        assert!(text.contains("retained 4000 context bytes"), "{text}");
        assert_eq!(SensitiveDataGuard::new().redact(&text), text, "already redacted");
    }

    #[test]
    fn summary_key_is_never_protected() {
        assert!(
            !PROTECTED_MEMORY_PREFIXES.iter().any(|prefix| SESSION_SUMMARY_KEY.starts_with(prefix)),
            "summary key must not collide with protected prefixes"
        );
        let item = MemoryItem::new(SESSION_SUMMARY_KEY, "summary text");
        assert_eq!(item.key, SESSION_SUMMARY_KEY);
    }

    #[test]
    fn deterministic_compaction_preserves_protected_keys() {
        let mut store = store_with_items();
        // Duplicate the observation so the deterministic pass merges it.
        store
            .insert(MemoryItem::from_task("obs:file", "read b.rs", TaskId::new("task-1")))
            .expect("inserts");
        let report = compact_memory(&mut store, &MemoryCompactionConfig::default());
        assert_eq!(report.merged_duplicates, 1);
        assert!(store.query("decision:api").is_some(), "decision preserved");
        assert!(store.query("constraint:no-network").is_some(), "constraint preserved");
        assert!(store.query("validation:tests").is_some(), "validation preserved");
        assert!(store.query("obs:file").is_some(), "newest observation kept");
    }
}
