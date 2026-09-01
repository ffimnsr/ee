use std::sync::Arc;

use ee_agent_host::{
    AgentManager, AgentManagerConfig, DenyAllHandler, WorkspaceMemoryAvailability,
    WorkspaceMemoryHostConfig, WorkspaceMemoryHostErrorCode, WorkspaceMemoryMutationApproval,
};
use tokio::sync::mpsc;

fn manager(config: WorkspaceMemoryHostConfig) -> AgentManager {
    let (events, _receiver) = mpsc::unbounded_channel();
    AgentManager::new(
        AgentManagerConfig { workspace_memory: config, ..AgentManagerConfig::default() },
        Arc::new(DenyAllHandler),
        events,
    )
}

#[test]
fn disabled_and_unavailable_management_statuses_fail_closed_without_paths() {
    let disabled = manager(WorkspaceMemoryHostConfig::default());
    let status = disabled.workspace_memory_status();
    assert_eq!(status.availability, WorkspaceMemoryAvailability::Disabled);
    assert!(!status.enabled);
    assert!(status.primary_workspace_id.is_none());
    assert!(!serde_json::to_string(&status).expect("status serializes").contains("database"));
    assert_eq!(
        disabled.workspace_memory_read("missing").expect_err("disabled read").code,
        WorkspaceMemoryHostErrorCode::Disabled
    );

    let temp = tempfile::tempdir().expect("tempdir");
    let unavailable = manager(WorkspaceMemoryHostConfig {
        enabled: true,
        trusted_roots: vec![temp.path().join("missing")],
        database_path: Some(temp.path().join("memory.sqlite3")),
        ..Default::default()
    });
    let status = unavailable.workspace_memory_status();
    assert!(status.enabled);
    assert_eq!(status.availability, WorkspaceMemoryAvailability::Unavailable);
    let error = unavailable
        .workspace_memory_clear_approved(WorkspaceMemoryMutationApproval::Approved)
        .expect_err("unavailable clear");
    assert_eq!(error.code, WorkspaceMemoryHostErrorCode::Unavailable);
    assert!(!error.to_string().contains(temp.path().to_string_lossy().as_ref()));
}

#[test]
fn manager_management_api_covers_bounded_lifecycle_export_and_import() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace root");
    let manager = manager(WorkspaceMemoryHostConfig {
        enabled: true,
        trusted_roots: vec![root],
        database_path: Some(temp.path().join("memory.sqlite3")),
        ..Default::default()
    });
    let approved = WorkspaceMemoryMutationApproval::Approved;

    for (key, value) in [
        ("architecture.parser", "Tree-sitter is backend-owned"),
        ("convention.tests", "Run quiet targeted tests"),
    ] {
        let result = manager
            .workspace_memory_remember_approved(key, value, approved)
            .expect("remember approved");
        assert_eq!(result.affected, 1);
        let fact = result.fact.expect("remembered fact");
        assert_eq!(fact.authority, "user_asserted");
        assert_eq!(fact.state, "active");
        assert_eq!(fact.provenance.source_kind, "frontend_user_approved");
        assert!(fact.provenance.source_id.starts_with("frontend:"));
        assert!(!fact.provenance.source_id.contains(value));
    }

    let status = manager.workspace_memory_status();
    assert_eq!(status.availability, WorkspaceMemoryAvailability::Available);
    assert_eq!(status.active_facts, 2);
    assert_eq!(status.trusted_root_count, 1);
    assert!(status.primary_workspace_id.as_deref().is_some_and(|id| id.starts_with("sha256:")));

    let listed = manager.workspace_memory_list(1).expect("bounded list");
    assert_eq!(listed.facts.len(), 1);
    assert_eq!(listed.total, 2);
    assert_eq!(listed.omitted, 1);
    assert!(listed.truncated);

    let recalled = manager.workspace_memory_recall("parser", 1).expect("bounded recall");
    assert_eq!(recalled.facts.len(), 1);
    assert_eq!(recalled.facts[0].key, "architecture.parser");
    assert_eq!(recalled.facts[0].selection_reason.as_deref(), Some("full_text"));
    assert_eq!(
        manager.workspace_memory_read("architecture.parser").expect("exact read").value,
        "Tree-sitter is backend-owned"
    );

    manager
        .workspace_memory_retract_approved("convention.tests", approved)
        .expect("retract approved");
    assert_eq!(
        manager.workspace_memory_read("convention.tests").expect_err("retracted hidden").code,
        WorkspaceMemoryHostErrorCode::NotFound
    );

    let redacted =
        manager.workspace_memory_export_approved(false, approved).expect("redacted export");
    assert!(redacted.redacted);
    assert!(redacted.facts.iter().all(|fact| fact.value.is_none()));
    assert_eq!(
        manager
            .workspace_memory_import_approved(redacted, approved)
            .expect_err("redacted import rejected")
            .code,
        WorkspaceMemoryHostErrorCode::InvalidExport
    );

    let export = manager.workspace_memory_export_approved(true, approved).expect("valued export");
    assert!(!export.redacted);
    assert!(export.facts.iter().all(|fact| fact.value.is_some()));
    assert_eq!(
        manager.workspace_memory_clear_approved(approved).expect("clear approved").affected,
        2
    );
    assert_eq!(manager.workspace_memory_status().active_facts, 0);
    assert_eq!(
        manager
            .workspace_memory_import_approved(export, approved)
            .expect("import approved")
            .affected,
        1
    );
    assert_eq!(manager.workspace_memory_status().active_facts, 1);
    assert_eq!(
        manager
            .workspace_memory_forget_approved("architecture.parser", approved)
            .expect("forget approved")
            .affected,
        1
    );
    assert_eq!(manager.workspace_memory_status().active_facts, 0);
}
