use super::super::*;

// The process cwd is process-global; lock it while mutating.
// The ancestor cannot override with, or cause launch of, a reference.
// Even from an ancestor layer, substrings are never treated as refs.
// Built-in defaults keep agents disabled even with no config layers.
// User layer defines an agent server but never enables agents mode.
// Project-local `.ee.toml` enables agents and refines the server.
// Agent `env` documentation exposes the `secret://<name>` reference
// syntax and its user-global-only resolution boundary (phase 5)
// without changing the config shape.
// Serialize back through the full document shape: the resolved
// document carries the proxy flag.
#[test]
fn workspace_memory_defaults_disabled_and_merges_field_by_field() {
    let mut settings = EditorSettings::default();
    let defaults = settings.agents.workspace_memory.clone();
    assert!(!defaults.enabled);

    let first: EeToml = toml::from_str(
            "[agents.workspace_memory]\nenabled = true\nmax_value_bytes = 8192\nmax_recall_results = 12\ndefault_expiry_days = 180\ncandidate_retention_days = 5\n",
        )
        .unwrap();
    settings.merge_toml(&first, ConfigLayerKind::System);
    let second: EeToml = toml::from_str(
            "[agents.workspace_memory]\nmax_active_facts = 512\nbusy_timeout_ms = 3500\nstale_retention_days = 45\nsuperseded_retention_days = 120\n",
        )
        .unwrap();
    settings.merge_toml(&second, ConfigLayerKind::Ancestor);

    let memory = &settings.agents.workspace_memory;
    assert!(memory.enabled);
    assert_eq!(memory.max_value_bytes, 8192);
    assert_eq!(memory.max_active_facts, 512);
    assert_eq!(memory.max_active_bytes, defaults.max_active_bytes);
    assert_eq!(memory.max_recall_results, 12);
    assert_eq!(memory.busy_timeout_ms, 3500);
    assert_eq!(memory.default_expiry_days, 180);
    assert_eq!(memory.candidate_retention_days, 5);
    assert_eq!(memory.stale_retention_days, 45);
    assert_eq!(memory.superseded_retention_days, 120);
}
#[test]
fn workspace_memory_validation_rejects_out_of_range_values() {
    let parsed: EeToml =
        toml::from_str("[agents.workspace_memory]\nenabled = true\nmax_recall_results = 0\n")
            .unwrap();
    let error = validate_agents_mcp_config(&parsed).unwrap_err();
    assert!(error.contains("agents.workspace_memory.max_recall_results"));

    let parsed: EeToml =
        toml::from_str("[agents.workspace_memory]\ncandidate_retention_days = 0\n").unwrap();
    let error = validate_agents_mcp_config(&parsed).unwrap_err();
    assert!(error.contains("agents.workspace_memory.candidate_retention_days"));

    let parsed: EeToml =
        toml::from_str("[agents.workspace_memory]\ndefault_expiry_days = 0\n").unwrap();
    validate_agents_mcp_config(&parsed).unwrap();
}
#[test]
fn workspace_memory_invalid_values_keep_safe_defaults() {
    let mut resolved = WorkspaceMemorySettings::default();
    let defaults = resolved.clone();
    merge_workspace_memory(
        &mut resolved,
        &WorkspaceMemoryToml {
            max_value_bytes: Some(0),
            max_active_facts: Some(MAX_WORKSPACE_MEMORY_ACTIVE_FACTS + 1),
            max_active_bytes: Some(0),
            max_recall_results: Some(MAX_WORKSPACE_MEMORY_RECALL_RESULTS + 1),
            busy_timeout_ms: Some(0),
            default_expiry_days: Some(MAX_WORKSPACE_MEMORY_RETENTION_DAYS + 1),
            candidate_retention_days: Some(0),
            stale_retention_days: Some(MAX_WORKSPACE_MEMORY_RETENTION_DAYS + 1),
            superseded_retention_days: Some(0),
            ..WorkspaceMemoryToml::default()
        },
    );
    assert_eq!(resolved, defaults);
}
#[test]
fn workspace_memory_switch_persistence_preserves_unrelated_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    let original = "# keep this comment\r\n[agents]\r\nenabled = true\r\n\r\n[agents.workspace_memory]\r\nenabled = false # explicit switch\r\ncandidate_retention_days = 11\r\n\r\n[agents.servers.helper]\r\ncommand = \"helper-agent\"\r\n";
    std::fs::write(&path, original).unwrap();

    persist_workspace_memory_enabled(&path, true).unwrap();

    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("# keep this comment\r\n"));
    assert!(updated.contains("enabled = true # explicit switch\r\n"));
    assert!(updated.contains("candidate_retention_days = 11\r\n"));
    assert!(updated.contains("[agents.servers.helper]\r\ncommand = \"helper-agent\"\r\n"));
    let parsed: EeToml = toml::from_str(&updated).unwrap();
    assert_eq!(parsed.agents.unwrap().workspace_memory.unwrap().enabled, Some(true));
}
#[test]
fn workspace_memory_switch_persistence_adds_explicit_table() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(&path, "[agents]\nenabled = true\n").unwrap();

    persist_workspace_memory_enabled(&path, false).unwrap();

    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("[agents]\nenabled = true\n"));
    assert!(updated.contains("[agents.workspace_memory]\nenabled = false\n"));
}
#[test]
fn workspace_memory_switch_persistence_handles_header_without_final_newline() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(&path, "[agents.workspace_memory]").unwrap();

    persist_workspace_memory_enabled(&path, true).unwrap();

    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "[agents.workspace_memory]\nenabled = true\n"
    );
}
#[test]
fn workspace_memory_switch_persistence_refuses_invalid_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    let invalid = "[agents.workspace_memory\nenabled = true\n";
    std::fs::write(&path, invalid).unwrap();

    let error = persist_workspace_memory_enabled(&path, false).unwrap_err();

    assert!(error.contains("refusing to modify invalid config"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), invalid);
}
