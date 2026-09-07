//! Subagent tests: memory.
use super::*;

#[test]
fn merge_memory_items_skips_sensitive_and_rejected() {
    let mut store = MemoryStore::new(1024);
    let normal = MemoryItem::new("fact", "value");
    let sensitive = MemoryItem::new("token", "secret").as_sensitive();
    let merged = merge_memory_items(&mut store, &[normal.clone(), sensitive]);
    assert_eq!(merged, 1);
    assert_eq!(store.query("fact"), Some(normal));
    assert_eq!(store.query("token"), None, "sensitive item never merged");
}

#[test]
fn subagent_role_defaults_are_read_only() {
    let role = SubagentRole::new("worker", "do good work");
    assert_eq!(role.name, "worker");
    assert_eq!(role.allowed_tool_classes, vec![SideEffectClass::Read]);
    assert_eq!(role.max_iterations, SUBAGENT_DEFAULT_MAX_ITERATIONS);
    assert!(role.requires_evidence, "custom roles default fail closed");
    let role =
        role.with_allowed_tool_classes(vec![SideEffectClass::Execute]).with_max_iterations(4);
    assert_eq!(role.allowed_tool_classes, vec![SideEffectClass::Execute]);
    assert_eq!(role.max_iterations, 4);
}

#[test]
fn subagent_types_roundtrip_through_json() {
    let result = SubagentResult {
        subagent_id: SubagentId::new("task-3"),
        handoff: SubagentHandoff::from_completed_output(
            "worker",
            "task-3",
            &json!({
                "schema_version": 1,
                "summary": "done",
                "findings": [],
                "citations": {"files": ["/work/a.rs"], "tools": ["read_file"]},
                "unresolved": [],
                "recommended_actions": []
            })
            .to_string(),
            SubagentEvidence::default(),
        ),
        produced_memory_items: vec![MemoryItem::new("fact", "value")],
        tool_call_count: 2,
        error_summary: None,
    };
    let json = serde_json::to_string(&result).expect("serializes");
    let restored: SubagentResult = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored, result);
    assert_eq!(restored.handoff.claimed_citations, result.handoff.claimed_citations);

    let intent = SubagentIntent::new("summarize the findings");
    let json = serde_json::to_string(&intent).expect("serializes");
    let restored: SubagentIntent = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored, intent);
}
