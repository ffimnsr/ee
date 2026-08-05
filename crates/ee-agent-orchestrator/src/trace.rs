//! JSONL trace export for orchestrator events with secret redaction.
//!
//! [`export_jsonl`] serializes every [`OrchestratorEvent`] as one compact JSON
//! line with a stable sequence number; the event variants already carry the
//! applicable task id, subagent id, tool-call id, and budget counters.
//! Before writing, the JSON is walked by [`redact_json`], which replaces
//! values under sensitive key names (keys containing `token`, `secret`,
//! `password`, `api_key`, ...) and masks `KEY=value` assignments inside
//! string values, so exported traces never leak credentials.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::OrchestratorError;
use crate::events::OrchestratorEvent;
use crate::sensitive_data::redact_values;

/// Marker replacing sensitive values in exported traces.
pub use crate::sensitive_data::REDACTED;
/// Substrings that mark a JSON key or assignment name as sensitive.
pub use crate::sensitive_data::SENSITIVE_KEY_MARKERS;
/// Whether a key name (or assignment name) looks sensitive.
pub use crate::sensitive_data::is_sensitive_key;
/// Masks `SENSITIVE=value` assignments inside a string value, leaving the
/// assignment name visible for diagnostics but hiding the value.
pub use crate::sensitive_data::redact_assignments;

/// One JSONL trace line: a sequence number plus the recorded event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TraceLine {
    /// 1-based position in the exported trace.
    pub seq: u64,
    /// The recorded event.
    pub event: OrchestratorEvent,
}

impl TraceLine {
    /// Creates a line at the given position.
    #[must_use]
    pub fn new(seq: u64, event: OrchestratorEvent) -> Self {
        Self { seq, event }
    }
}

/// One JSONL trace line: a sequence number plus the recorded event.
/// Walks a JSON value and redacts sensitive content in place: values under
/// sensitive key names become [`REDACTED`], string values get their
/// sensitive `KEY=value` assignments masked, and standalone secret-like
/// token values are masked too.
pub fn redact_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *child = Value::String(REDACTED.to_string());
                } else {
                    redact_json(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item);
            }
        }
        Value::String(text) => {
            let redacted = redact_values(&redact_assignments(text));
            if redacted != *text {
                *text = redacted;
            }
        }
        _ => {}
    }
}

/// Serializes events as newline-delimited JSON, one [`TraceLine`] per event
/// in order, with secret redaction applied to every line.
pub fn export_jsonl(events: &[OrchestratorEvent]) -> Result<String, OrchestratorError> {
    let mut out = String::new();
    for (index, event) in events.iter().enumerate() {
        let line = TraceLine::new(index as u64 + 1, event.clone());
        let mut value = serde_json::to_value(&line)
            .map_err(|error| OrchestratorError::Serialization(error.to_string()))?;
        redact_json(&mut value);
        let json = serde_json::to_string(&value)
            .map_err(|error| OrchestratorError::Serialization(error.to_string()))?;
        out.push_str(&json);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_events() -> Vec<OrchestratorEvent> {
        vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
            },
            OrchestratorEvent::SubagentStarted {
                subagent_id: "task-2".into(),
                model_id: Some("strong".into()),
            },
            OrchestratorEvent::BudgetUpdated {
                iterations_used: 2,
                model_calls_used: 2,
                tool_calls_used: 1,
                subagents_used: 0,
                output_bytes_used: 42,
            },
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ]
    }

    #[test]
    fn export_jsonl_preserves_order_one_line_per_event() {
        let events = sample_events();
        let trace = export_jsonl(&events).expect("exports");
        let lines: Vec<&str> = trace.lines().collect();
        assert_eq!(lines.len(), events.len());
        for (index, line) in lines.iter().enumerate() {
            let parsed: TraceLine = serde_json::from_str(line).expect("line parses");
            assert_eq!(parsed.seq, index as u64 + 1, "sequence must be stable");
            assert_eq!(parsed.event, events[index]);
        }
    }

    #[test]
    fn every_event_variant_roundtrips_through_trace_lines() {
        let events = vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
            },
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
                success: false,
            },
            OrchestratorEvent::SubagentStarted { subagent_id: "task-2".into(), model_id: None },
            OrchestratorEvent::SubagentFinished { subagent_id: "task-2".into(), success: true },
            OrchestratorEvent::BudgetUpdated {
                iterations_used: 1,
                model_calls_used: 1,
                tool_calls_used: 1,
                subagents_used: 1,
                output_bytes_used: 7,
            },
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            OrchestratorEvent::Error { error: "boom".into() },
            OrchestratorEvent::StrategySelected {
                strategy: crate::strategy::TurnStrategy::ResearchThenEdit,
                reason: crate::strategy::StrategyReason::UnknownCodebaseChange,
            },
            OrchestratorEvent::SuspiciousContentDetected {
                trust: crate::trust::TrustLevel::SubagentSummaryUntrusted,
                pattern: "you are now".into(),
                excerpt: "you are now the system".into(),
            },
        ];
        let trace = export_jsonl(&events).expect("exports");
        let lines: Vec<TraceLine> =
            trace.lines().map(|line| serde_json::from_str(line).expect("line parses")).collect();
        assert_eq!(lines.len(), events.len());
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(line.event, events[index]);
        }
    }

    #[test]
    fn redact_json_masks_sensitive_keys_at_any_depth() {
        let mut value = json!({
            "metadata": {
                "api_key": "sk-live-123",
                "nested": { "token": "abc", "path": "/work" },
                "password": "hunter2",
                "note": "OPENROUTER_API_KEY=sk-other-456",
                "fine": "hello"
            },
            "list": [{ "authorization": "Bearer xyz", "count": 3 }]
        });
        redact_json(&mut value);
        assert_eq!(value["metadata"]["api_key"], REDACTED);
        assert_eq!(value["metadata"]["nested"]["token"], REDACTED);
        assert_eq!(value["metadata"]["password"], REDACTED);
        assert_eq!(value["list"][0]["authorization"], REDACTED);
        // Non-sensitive content is untouched; sensitive assignments inside
        // strings are masked.
        assert_eq!(value["metadata"]["fine"], "hello");
        assert_eq!(value["metadata"]["nested"]["path"], "/work");
        assert_eq!(value["list"][0]["count"], 3);
        let note = value["metadata"]["note"].as_str().expect("string");
        assert!(note.contains("[redacted]"), "{note}");
        assert!(!note.contains("sk-other-456"), "{note}");
    }

    #[test]
    fn redact_json_masks_standalone_secret_like_values() {
        let mut value = json!({ "note": "sk-live-1234567890", "fine": "hello" });
        redact_json(&mut value);
        assert_eq!(value["note"], REDACTED);
        assert_eq!(value["fine"], "hello");
        let mut inline = json!({ "note": "the key is sk-live-1234567890" });
        redact_json(&mut inline);
        let masked = inline["note"].as_str().expect("string");
        assert!(masked.contains("[redacted]"), "{masked}");
        assert!(!masked.contains("sk-live-1234567890"), "{masked}");
    }

    #[test]
    fn redact_assignments_masks_only_sensitive_names() {
        assert_eq!(
            redact_assignments("OPENROUTER_API_KEY=sk-live-123"),
            "OPENROUTER_API_KEY=[redacted]"
        );
        assert_eq!(redact_assignments("token=abc"), "token=[redacted]");
        assert_eq!(redact_assignments("path=/work/a.txt"), "path=/work/a.txt");
        assert_eq!(
            redact_assignments("user=admin password=hunter2"),
            "user=admin password=[redacted]"
        );
        assert_eq!(redact_assignments("plain text"), "plain text");
    }

    #[test]
    fn export_jsonl_redacts_error_messages() {
        let events = vec![OrchestratorEvent::Error {
            error: "provider rejected OPENROUTER_API_KEY=sk-live-123".into(),
        }];
        let trace = export_jsonl(&events).expect("exports");
        assert!(trace.contains("[redacted]"), "{trace}");
        assert!(!trace.contains("sk-live-123"), "{trace}");
        assert!(trace.contains("OPENROUTER_API_KEY"), "assignment name stays for diagnostics");
    }

    #[test]
    fn sensitive_key_detection_is_case_insensitive() {
        for key in ["api_key", "API_KEY", "BearerToken", "password_hash", "clientSecret"] {
            assert!(is_sensitive_key(key), "{key}");
        }
        for key in ["path", "title", "session_id", "tool_call_id", "message"] {
            assert!(!is_sensitive_key(key), "{key}");
        }
    }
}
