//! Advisory repeated-tool-call reminder.
//!
//! Mirrors DeepSeek Harness's `repeat-tool-reminder`: counts consecutive
//! identical calls, emits a user-role notice at configured thresholds, and
//! never blocks execution. The loop's stuck detector remains the hard stop.

use serde_json::Value;

use crate::model::{ModelMessage, ModelRole, Transcript};
use crate::trust::TrustLevel;

/// Default thresholds; `ee` stops at 4, so only the first fires by default.
pub(crate) const DEFAULT_REPEAT_THRESHOLDS: &[usize] = &[3, 5, 8];
/// Default maximum characters of canonical arguments quoted in a reminder.
pub(crate) const DEFAULT_ARGUMENTS_PREVIEW_CHARS: usize = 500;

const GENTLE_REMINDER: &str = "You are repeating the exact same tool call with identical arguments. Carefully analyze the previous result before calling again: if the task is not complete, try a different approach or different arguments instead of repeating the call.";

/// Repeated-call guard configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepeatGuardConfig {
    /// Consecutive counts that emit a reminder.
    pub thresholds: Vec<usize>,
    /// Tool-name patterns to track; empty means every tool.
    pub include: Vec<String>,
    /// Tool-name patterns that neither count nor reset the chain.
    pub exclude: Vec<String>,
    /// Maximum characters of canonical arguments quoted.
    pub arguments_preview_chars: usize,
}

impl Default for RepeatGuardConfig {
    fn default() -> Self {
        Self {
            thresholds: DEFAULT_REPEAT_THRESHOLDS.to_vec(),
            include: Vec::new(),
            exclude: Vec::new(),
            arguments_preview_chars: DEFAULT_ARGUMENTS_PREVIEW_CHARS,
        }
    }
}

/// Per-turn repeated-call state.
#[derive(Debug, Clone)]
pub(crate) struct RepeatGuard {
    config: RepeatGuardConfig,
    last_key: Option<String>,
    count: usize,
}

impl RepeatGuard {
    /// Creates a guard with validated thresholds.
    pub(crate) fn new(config: RepeatGuardConfig) -> Self {
        let mut thresholds = config.thresholds;
        thresholds.retain(|threshold| *threshold >= 2);
        thresholds.sort_unstable();
        thresholds.dedup();
        Self { config: RepeatGuardConfig { thresholds, ..config }, last_key: None, count: 0 }
    }

    /// Observes one executed call and returns a reminder when a threshold hits.
    pub(crate) fn observe(&mut self, tool_name: &str, arguments: &Value) -> Option<ModelMessage> {
        if !self.tracked(tool_name) {
            return None;
        }
        let canonical = canonical_arguments(arguments);
        let key = format!("{tool_name}\u{0}{canonical}");
        self.count = if self.last_key.as_deref() == Some(key.as_str()) {
            self.count.saturating_add(1)
        } else {
            1
        };
        self.last_key = Some(key);
        if !self.config.thresholds.contains(&self.count) {
            return None;
        }
        let text = if self.count == self.config.thresholds.first().copied().unwrap_or(3) {
            GENTLE_REMINDER.to_string()
        } else {
            detailed_reminder(
                tool_name,
                self.count,
                &canonical,
                self.config.arguments_preview_chars,
            )
        };
        Some(
            ModelMessage::text(ModelRole::User, text)
                .with_trust(TrustLevel::SystemPolicy)
                .with_metadata([("ee_prompt".to_string(), "repeat_reminder".to_string())]),
        )
    }

    /// Observes a call and appends the reminder, when any, to the transcript.
    pub(crate) fn observe_and_push(
        &mut self,
        transcript: &mut Transcript,
        tool_name: &str,
        arguments: &Value,
    ) {
        if let Some(notice) = self.observe(tool_name, arguments) {
            transcript.push_unlogged(notice);
        }
    }

    fn tracked(&self, tool_name: &str) -> bool {
        if !self.config.include.is_empty()
            && !self.config.include.iter().any(|pattern| wildcard_match(pattern, tool_name))
        {
            return false;
        }
        !self.config.exclude.iter().any(|pattern| wildcard_match(pattern, tool_name))
    }
}

fn detailed_reminder(
    tool_name: &str,
    count: usize,
    canonical: &str,
    preview_chars: usize,
) -> String {
    format!(
        "Repeated tool call detected:\n- tool: {tool_name}\n- consecutive_calls: {count}\n- arguments: {}\nThe repeated calls are not making progress. Do not call this tool with these exact arguments again. Inspect the latest result and choose a different action, different arguments, or finish the task if enough evidence has been gathered.",
        preview(canonical, preview_chars)
    )
}

fn preview(canonical: &str, limit: usize) -> String {
    if canonical.chars().count() <= limit {
        return canonical.to_string();
    }
    let head: String = canonical.chars().take(limit).collect();
    let omitted = canonical.chars().count().saturating_sub(limit);
    format!("{head}… (+{omitted} more chars)")
}

/// Canonical JSON: object keys sorted at every nesting level.
pub(crate) fn canonical_arguments(arguments: &Value) -> String {
    serde_json::to_string(&sort_json(arguments)).unwrap_or_else(|_| "null".to_string())
}

fn sort_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sort_json).collect()),
        Value::Object(map) => {
            let mut sorted = serde_json::Map::with_capacity(map.len());
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            for key in keys {
                sorted.insert(key.clone(), sort_json(&map[key]));
            }
            Value::Object(sorted)
        }
        other => other.clone(),
    }
}

/// Anchored `*` wildcard match used by the include/exclude filters.
pub(crate) fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pattern_index, mut text_index) = (0, 0);
    let (mut star, mut star_text) = (None, 0);
    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            star_text = text_index;
            pattern_index += 1;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_text += 1;
            text_index = star_text;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn third_identical_call_emits_gentle_reminder() {
        let mut guard = RepeatGuard::new(RepeatGuardConfig::default());
        let args = json!({ "path": "a" });
        assert!(guard.observe("read_file", &args).is_none());
        assert!(guard.observe("read_file", &args).is_none());
        let notice = guard.observe("read_file", &args).expect("third call reminds");
        assert!(notice.text_content().contains("repeating the exact same tool call"));
        assert_eq!(notice.metadata.get("ee_prompt").map(String::as_str), Some("repeat_reminder"));
        assert_eq!(notice.role, ModelRole::User);
        assert_eq!(notice.trust, TrustLevel::SystemPolicy);
    }

    #[test]
    fn changed_arguments_reset_the_chain() {
        let mut guard = RepeatGuard::new(RepeatGuardConfig::default());
        for _ in 0..2 {
            assert!(guard.observe("read_file", &json!({ "path": "a" })).is_none());
        }
        assert!(guard.observe("read_file", &json!({ "path": "b" })).is_none());
        assert!(guard.observe("read_file", &json!({ "path": "b" })).is_none());
        assert!(guard.observe("read_file", &json!({ "path": "b" })).is_some());
    }

    #[test]
    fn key_order_does_not_break_detection() {
        let mut guard = RepeatGuard::new(RepeatGuardConfig::default());
        for _ in 0..2 {
            assert!(guard.observe("x", &json!({ "a": 1, "b": 2 })).is_none());
        }
        assert!(guard.observe("x", &json!({ "b": 2, "a": 1 })).is_some());
    }

    #[test]
    fn excluded_tools_do_not_count_or_reset() {
        let config = RepeatGuardConfig { exclude: vec!["web_*".into()], ..Default::default() };
        let mut guard = RepeatGuard::new(config);
        let args = json!({ "q": "x" });
        for _ in 0..3 {
            assert!(guard.observe("web_search", &args).is_none());
        }
        // The excluded calls were transparent: the read chain still starts at 1.
        assert!(guard.observe("read_file", &args).is_none());
        assert!(guard.observe("read_file", &args).is_none());
        assert!(guard.observe("read_file", &args).is_some());
    }

    #[test]
    fn include_filter_limits_tracking() {
        let config = RepeatGuardConfig { include: vec!["read_*".into()], ..Default::default() };
        let mut guard = RepeatGuard::new(config);
        let args = json!({});
        for _ in 0..3 {
            assert!(guard.observe("write_file", &args).is_none());
        }
        for _ in 0..2 {
            assert!(guard.observe("read_file", &args).is_none());
        }
        assert!(guard.observe("read_file", &args).is_some());
    }

    #[test]
    fn later_threshold_is_detailed_and_preview_is_bounded() {
        let config = RepeatGuardConfig { thresholds: vec![2, 3], ..Default::default() };
        let mut guard = RepeatGuard::new(config);
        let args = json!({ "blob": "x".repeat(600) });
        assert!(guard.observe("write_file", &args).is_none());
        assert!(guard.observe("write_file", &args).is_some());
        let notice = guard.observe("write_file", &args).expect("detailed reminder");
        assert!(notice.text_content().contains("Repeated tool call detected"));
        assert!(notice.text_content().contains("more chars)"));
    }

    #[test]
    fn wildcard_matching_is_anchored() {
        assert!(wildcard_match("read_*", "read_file"));
        assert!(wildcard_match("*_file", "read_file"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("read_*", "write_file"));
    }
}
