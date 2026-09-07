//! Manifest, schema, and argument-cap contract tests.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_list_exposes_ee_namespaced_tools() {
    let backend: Arc<dyn EeProxyBackend> = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend).await;

    let tools = tokio::time::timeout(REQUEST_TIMEOUT, client.list_all_tools())
        .await
        .expect("list tools timed out")
        .expect("list tools failed");

    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, crate::STABLE_TOOL_NAMES);
    assert!(names.contains(&"ee_turn_evidence_summary"));
    assert!(tools.iter().all(|tool| tool.name.starts_with("ee_")));
    assert!(tools.iter().all(|tool| !tool.name.contains('.')));
    assert!(tools.iter().all(|tool| tool.input_schema.contains_key("properties")));
    assert!(tools.iter().any(|tool| tool.name == "ee_tools_manifest"));
    assert!(tools.iter().find(|tool| tool.name == "ee_list_directory").is_some_and(|tool| {
        tool.description
            .as_ref()
            .is_some_and(|description| description.contains("host default cap"))
    }));
    assert!(tools.iter().find(|tool| tool.name == "ee_search_text_regex").is_some_and(|tool| {
        tool.description.as_ref().is_some_and(|description| description.contains("safety-limited"))
    }));

    shutdown(&client, &server);
}

#[test]
fn supported_tool_profile_filters_discovery_but_keeps_manifest() {
    let proxy = EeMcpProxy::with_supported_tools(
        Arc::new(ScriptedBackend::default()),
        vec![String::from("ee_workspace_roots")],
    );
    let tools = proxy.tools();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, vec!["ee_workspace_roots", "ee_tools_manifest"]);
}

#[test]
fn tools_manifest_is_versioned_complete_and_cache_safe() {
    let proxy = EeMcpProxy::new(Arc::new(ScriptedBackend::default()));
    let manifest = proxy.tools_manifest();
    assert_eq!(manifest.manifest_version, crate::EE_TOOL_SCHEMA_VERSION);
    assert!(manifest.tools.iter().all(|entry| entry.name.starts_with("ee_")));
    assert!(
        manifest.tools.iter().all(|entry| entry.schema_version == crate::EE_TOOL_SCHEMA_VERSION)
    );
    assert!(manifest.tools.iter().all(|entry| {
        !entry.approval.is_empty()
            && !entry.transport_availability.is_empty()
            && !entry.output_caps.is_empty()
            && !entry.redaction_rules.is_empty()
            && !entry.error_classes.is_empty()
    }));
    let advertised: Vec<&str> = manifest.tools.iter().map(|entry| entry.name.as_str()).collect();
    assert!(crate::STABLE_TOOL_NAMES.iter().all(|name| advertised.contains(name)));
    assert!(advertised.contains(&"ee_turn_evidence_summary"));
    for entry in &manifest.tools {
        assert_schema_example_is_valid(&entry.input_schema, &entry.example);
    }
    assert!(manifest.tools.iter().any(|entry| entry.name == "ee_tools_manifest"));
}

#[test]
fn workspace_memory_manifest_uses_exact_bounded_flat_inputs() {
    let manifest = EeMcpProxy::new(Arc::new(ScriptedBackend::default())).tools_manifest();
    for (name, required, properties) in [
        (
            "ee_remember_workspace_fact",
            json!(["key", "value"]),
            json!({
                "key": { "type": "string", "minLength": 1, "maxLength": 128 },
                "value": { "type": "string", "minLength": 1, "maxLength": 4096 }
            }),
        ),
        (
            "ee_recall_workspace_facts",
            json!(["query"]),
            json!({
                "query": { "type": "string", "minLength": 1, "maxLength": 1024 }
            }),
        ),
        (
            "ee_read_workspace_fact",
            json!(["key"]),
            json!({
                "key": { "type": "string", "minLength": 1, "maxLength": 128 }
            }),
        ),
        (
            "ee_forget_workspace_fact",
            json!(["key"]),
            json!({
                "key": { "type": "string", "minLength": 1, "maxLength": 128 }
            }),
        ),
        (
            "ee_list_workspace_facts",
            json!(["limit"]),
            json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 256 }
            }),
        ),
        (
            "ee_retract_workspace_fact",
            json!(["key"]),
            json!({
                "key": { "type": "string", "minLength": 1, "maxLength": 128 }
            }),
        ),
        (
            "ee_export_workspace_memory",
            json!(["include_values"]),
            json!({
                "include_values": { "type": "boolean" }
            }),
        ),
        (
            "ee_import_workspace_memory",
            json!(["export_json"]),
            json!({
                "export_json": { "type": "string", "minLength": 2, "maxLength": 61440 }
            }),
        ),
    ] {
        let entry = manifest.tools.iter().find(|entry| entry.name == name).expect("tool");
        assert_eq!(entry.input_schema["required"], required, "{name}");
        assert_eq!(entry.input_schema["properties"], properties, "{name}");
        assert_eq!(entry.input_schema["additionalProperties"], json!(false), "{name}");
    }
    let clear = manifest
        .tools
        .iter()
        .find(|entry| entry.name == "ee_clear_workspace_memory")
        .expect("clear tool");
    assert_eq!(clear.input_schema["properties"], json!({}));
    assert!(clear.input_schema.get("required").is_none());
    assert_eq!(clear.input_schema["additionalProperties"], json!(false));
}

#[test]
fn web_tools_manifest_requires_exact_flat_network_inputs() {
    let proxy = EeMcpProxy::new(Arc::new(ScriptedBackend::default()));
    let manifest = proxy.tools_manifest();

    for (name, input_schema) in [
        (
            "ee_web_search",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string", "minLength": 1 } },
                "required": ["query"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_fetch_url",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string", "minLength": 1 } },
                "required": ["url"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_browser_run_content",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string", "minLength": 1 } },
                "required": ["url"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_browser_run_screenshot",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string", "minLength": 1 } },
                "required": ["url"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_browser_run_markdown",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string", "minLength": 1 } },
                "required": ["url"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_browser_run_scrape",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "minLength": 1 },
                    "selector": { "type": "string", "minLength": 1 },
                },
                "required": ["url", "selector"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_browser_run_json",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "minLength": 1 },
                    "prompt": { "type": "string", "minLength": 1 },
                },
                "required": ["url", "prompt"],
                "additionalProperties": false,
            }),
        ),
        (
            "ee_browser_run_links",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string", "minLength": 1 } },
                "required": ["url"],
                "additionalProperties": false,
            }),
        ),
    ] {
        let entry =
            manifest.tools.iter().find(|entry| entry.name == name).expect("web tool is advertised");
        assert_eq!(entry.side_effect, "read", "{name}");
        assert_eq!(entry.approval, "required", "{name}");
        assert_eq!(entry.input_schema, input_schema, "{name} must retain its fail-closed schema");
        assert!(entry.transport_availability.contains(&String::from("stdio")), "{name}");
        assert!(entry.transport_availability.contains(&String::from("acp")), "{name}");
    }
}

#[test]
fn manifest_matches_discovery_governance_and_policy_classification() {
    let proxy = EeMcpProxy::new(Arc::new(ScriptedBackend::default()));
    let tools = proxy.tools();
    let manifest = proxy.tools_manifest();
    assert_eq!(tools.len(), manifest.tools.len());

    for (tool, entry) in tools.iter().zip(&manifest.tools) {
        let governance =
            crate::governance(tool.name.as_ref()).expect("every advertised tool has governance");
        assert_eq!(entry.name, tool.name.as_ref());
        assert_eq!(entry.input_schema, serde_json::Value::Object((*tool.input_schema).clone()));
        assert_eq!(entry.side_effect, governance.side_effect.as_str());
        assert_eq!(entry.approval, governance.approval);
        assert_eq!(
            entry.transport_availability,
            governance
                .transports
                .iter()
                .map(|transport| transport.as_str().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            entry.required_capabilities,
            governance
                .required_capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(entry.output_caps[0].kind, governance.output_cap_kind);
        assert_eq!(entry.output_caps[0].max, governance.output_cap);
        if matches!(governance.side_effect, crate::classify::SideEffectClass::Read) {
            assert_eq!(
                tool.annotations.as_ref().and_then(|annotations| annotations.read_only_hint),
                Some(true),
                "{} must advertise readOnlyHint",
                tool.name
            );
        } else {
            assert!(tool.annotations.is_none(), "{} must not advertise readOnlyHint", tool.name);
        }
    }
}

#[test]
fn manifest_snapshot_matches_versioned_contract() {
    let proxy = EeMcpProxy::new(Arc::new(ScriptedBackend::default()));
    let actual =
        canonical_json(serde_json::to_value(proxy.tools_manifest()).expect("manifest serializes"));
    let expected = canonical_json(
        serde_json::from_str(include_str!("../../../tests/fixtures/ee_tools_manifest-v6.json"))
            .expect("manifest fixture parses"),
    );
    assert_eq!(actual, expected);
}

#[test]
#[ignore = "run explicitly when intentionally updating the versioned manifest fixture"]
fn regenerate_manifest_snapshot() {
    let proxy = EeMcpProxy::new(Arc::new(ScriptedBackend::default()));
    let value =
        canonical_json(serde_json::to_value(proxy.tools_manifest()).expect("manifest serializes"));
    let snapshot = serde_json::to_string_pretty(&value).expect("canonical manifest serializes");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ee_tools_manifest-v6.json");
    std::fs::write(path, format!("{snapshot}\n")).expect("write manifest fixture");
}

#[test]
fn turn_evidence_summary_requires_owned_session_for_specified_turn_and_preserves_typed_unavailable()
{
    let backend = Arc::new(ScriptedBackend::default());
    let proxy = EeMcpProxy::new(backend.clone());

    let missing_session = CallToolRequestParams::new("ee_turn_evidence_summary")
        .with_arguments(arguments(json!({ "turn_id": 1 })));
    assert!(proxy.dispatch_tool(&missing_session).is_err());
    assert!(backend.calls().is_empty());

    let current = proxy
        .dispatch_tool(&CallToolRequestParams::new("ee_turn_evidence_summary"))
        .expect("current host summary is a tool response");
    assert!(format!("{current:?}").contains("turn:1:evidence:1"));

    let unavailable = proxy
        .dispatch_tool(
            &CallToolRequestParams::new("ee_turn_evidence_summary")
                .with_arguments(arguments(json!({ "session_id": "foreign", "turn_id": 1 }))),
        )
        .expect("unavailable evidence is a tool-level response");
    assert!(format!("{unavailable:?}").contains("evidence_unavailable"));
}

#[test]
fn argument_cap_rejects_oversized_input_before_backend_dispatch() {
    let backend = Arc::new(ScriptedBackend::default());
    let proxy = EeMcpProxy::new(backend.clone());
    let oversized = "x".repeat(MAX_TOOL_ARGUMENT_BYTES);
    let request = CallToolRequestParams::new("ee_save_note").with_arguments(arguments(json!({
        "key": "note",
        "content": oversized,
    })));
    assert!(proxy.dispatch_tool(&request).is_err());
    assert!(backend.calls().is_empty(), "oversized arguments never reach the backend");
}

#[test]
fn argument_cap_accepts_exact_boundary_and_rejects_one_byte_over() {
    let empty = arguments(json!({ "path": "/abs/work/a", "content": "" }));
    let overhead = serde_json::to_vec(&empty).expect("arguments serialize").len();
    let content_len = MAX_TOOL_ARGUMENT_BYTES.checked_sub(overhead).expect("cap exceeds overhead");

    let exact_backend = Arc::new(ScriptedBackend::default());
    let exact = CallToolRequestParams::new("ee_write_text_file").with_arguments(arguments(json!({
        "path": "/abs/work/a",
        "content": "x".repeat(content_len),
    })));
    assert!(EeMcpProxy::new(exact_backend.clone()).dispatch_tool(&exact).is_ok());
    assert_eq!(exact_backend.calls().len(), 1, "exact boundary dispatches");

    let over_backend = Arc::new(ScriptedBackend::default());
    let over = CallToolRequestParams::new("ee_write_text_file").with_arguments(arguments(json!({
        "path": "/abs/work/a",
        "content": "x".repeat(content_len + 1),
    })));
    assert!(EeMcpProxy::new(over_backend.clone()).dispatch_tool(&over).is_err());
    assert!(over_backend.calls().is_empty(), "over-boundary input never dispatches");
}

#[test]
fn malformed_arguments_fail_closed_before_backend_dispatch() {
    let cases = [
        ("ee_list_directory", json!({})),
        ("ee_list_directory", json!({ "path": 1 })),
        ("ee_read_buffer_lines", json!({ "path": "/abs/work/a.rs", "line": -1, "limit": 1 })),
        ("ee_apply_patch", json!({ "path": "/abs/work/a.rs", "edits": "not-an-array" })),
        ("ee_terminal_create", json!({ "command": 1 })),
        ("ee_terminal_create", json!({ "command": "pwd", "args": [1] })),
        ("ee_symbol_dependency_map", json!({ "path": "relative.rs", "line": 1, "character": 0 })),
        (
            "ee_symbol_dependency_map",
            json!({ "path": "/abs/work/a.rs", "line": 0, "character": 0 }),
        ),
        (
            "ee_symbol_dependency_map",
            json!({ "path": "/abs/work/a.rs", "line": 1, "character": 0, "extra": true }),
        ),
        ("ee_web_search", json!({ "query": "" })),
        ("ee_web_search", json!({ "query": "rust", "extra": true })),
        ("ee_fetch_url", json!({ "url": "" })),
        ("ee_fetch_url", json!({ "url": "https://example.com", "extra": true })),
        ("ee_browser_run_content", json!({ "url": "" })),
        ("ee_browser_run_screenshot", json!({ "url": "https://example.com", "extra": true })),
        ("ee_browser_run_scrape", json!({ "url": "https://example.com" })),
        (
            "ee_browser_run_scrape",
            json!({ "url": "https://example.com", "selector": "", "extra": true }),
        ),
        ("ee_browser_run_json", json!({ "url": "https://example.com" })),
        (
            "ee_browser_run_json",
            json!({ "url": "https://example.com", "prompt": "", "extra": true }),
        ),
    ];
    for (name, value) in cases {
        let backend = Arc::new(ScriptedBackend::default());
        let request = CallToolRequestParams::new(name).with_arguments(arguments(value));
        assert!(EeMcpProxy::new(backend.clone()).dispatch_tool(&request).is_err(), "{name}");
        assert!(backend.calls().is_empty(), "{name} must not reach backend");
    }
}

#[test]
fn workspace_fact_dtos_serialize_complete_camel_case_contract() {
    let fact = workspace_fact("architecture.parser", Some("exact_key"));
    let value = serde_json::to_value(&fact).expect("fact serializes");
    assert_eq!(
        value,
        json!({
            "id": 7,
            "namespace": "project",
            "key": "architecture.parser",
            "value": "Tree-sitter owns parsing",
            "kind": "architecture",
            "authority": "user_asserted",
            "freshness": "current",
            "state": "active",
            "provenance": {
                "sourceKind": "user",
                "sourceId": "turn-1",
                "revision": "rev-1",
                "fingerprint": "sha256:source",
                "verifiedAt": "2026-09-01T00:00:00Z"
            },
            "selectionReason": "exact_key",
            "createdAt": "2026-09-01T00:00:00Z",
            "updatedAt": "2026-09-01T00:00:01Z",
            "expiresAt": null,
            "contentHash": "sha256:fact",
            "schemaVersion": 1
        })
    );
    assert_eq!(
        serde_json::to_value(WorkspaceFactsResult {
            facts: vec![fact.clone()],
            total: 3,
            omitted: 2,
            truncated: true,
        })
        .expect("recall serializes")["omitted"],
        json!(2)
    );
    assert_eq!(
        serde_json::to_value(WorkspaceFactMutationResult {
            operation: String::from("remembered"),
            key: fact.key.clone(),
            affected: 1,
            fact: Some(fact),
        })
        .expect("mutation serializes")["operation"],
        json!("remembered")
    );
}

#[test]
fn workspace_memory_tools_dispatch_strict_bounded_arguments_and_structured_results() {
    let backend = Arc::new(ScriptedBackend::default());
    let proxy = EeMcpProxy::new(backend.clone());
    for (name, args, expected_call) in [
        (
            "ee_remember_workspace_fact",
            json!({ "key": "architecture.parser", "value": "Tree-sitter owns parsing" }),
            "remember_workspace_fact:architecture.parser:Tree-sitter owns parsing",
        ),
        (
            "ee_recall_workspace_facts",
            json!({ "query": "parser" }),
            "recall_workspace_facts:parser",
        ),
        (
            "ee_read_workspace_fact",
            json!({ "key": "architecture.parser" }),
            "read_workspace_fact:architecture.parser",
        ),
        (
            "ee_forget_workspace_fact",
            json!({ "key": "architecture.parser" }),
            "forget_workspace_fact:architecture.parser",
        ),
        ("ee_list_workspace_facts", json!({ "limit": 8 }), "list_workspace_facts:8"),
        (
            "ee_retract_workspace_fact",
            json!({ "key": "architecture.parser" }),
            "retract_workspace_fact:architecture.parser",
        ),
        (
            "ee_export_workspace_memory",
            json!({ "include_values": false }),
            "export_workspace_memory:false",
        ),
        ("ee_import_workspace_memory", json!({ "export_json": "{}" }), "import_workspace_memory:2"),
    ] {
        proxy
            .dispatch_tool(&CallToolRequestParams::new(name).with_arguments(arguments(args)))
            .expect("workspace-memory dispatch");
        assert_eq!(backend.calls().last().map(String::as_str), Some(expected_call));
    }
    proxy
        .dispatch_tool(&CallToolRequestParams::new("ee_clear_workspace_memory"))
        .expect("clear dispatch");
    assert_eq!(backend.calls().last().map(String::as_str), Some("clear_workspace_memory"));

    for (name, args) in [
        ("ee_remember_workspace_fact", json!({ "key": "key", "value": "value", "extra": true })),
        ("ee_remember_workspace_fact", json!({ "key": "k".repeat(129), "value": "value" })),
        ("ee_remember_workspace_fact", json!({ "key": "key", "value": "v".repeat(4097) })),
        ("ee_recall_workspace_facts", json!({ "query": "q".repeat(1025) })),
        ("ee_read_workspace_fact", json!({ "key": "" })),
        ("ee_forget_workspace_fact", json!({ "key": "key", "extra": true })),
        ("ee_list_workspace_facts", json!({ "limit": 257 })),
        ("ee_list_workspace_facts", json!({ "limit": 1, "extra": true })),
        ("ee_retract_workspace_fact", json!({ "key": "" })),
        ("ee_export_workspace_memory", json!({ "include_values": "yes" })),
        ("ee_import_workspace_memory", json!({ "export_json": "x" })),
        (
            "ee_import_workspace_memory",
            json!({ "export_json": "x".repeat(MAX_WORKSPACE_MEMORY_IMPORT_BYTES + 1) }),
        ),
        ("ee_clear_workspace_memory", json!({ "extra": true })),
    ] {
        let before = backend.calls().len();
        let request = CallToolRequestParams::new(name).with_arguments(arguments(args));
        assert!(proxy.dispatch_tool(&request).is_err(), "{name}");
        assert_eq!(backend.calls().len(), before, "{name} must fail before dispatch");
    }
}

#[test]
fn default_workspace_memory_backend_methods_fail_closed() {
    let backend = DenyWriteBackend;
    assert!(backend.remember_workspace_fact(String::from("key"), String::from("value")).is_err());
    assert!(backend.recall_workspace_facts(String::from("query")).is_err());
    assert!(backend.read_workspace_fact(String::from("key")).is_err());
    assert!(backend.forget_workspace_fact(String::from("key")).is_err());
    assert!(backend.list_workspace_facts(1).is_err());
    assert!(backend.retract_workspace_fact(String::from("key")).is_err());
    assert!(backend.export_workspace_memory(false).is_err());
    assert!(backend.import_workspace_memory(String::from("{}")).is_err());
    assert!(backend.clear_workspace_memory().is_err());
}

#[test]
fn filesystem_tools_validate_absolute_paths_and_dispatch_structured_results() {
    let backend = Arc::new(ScriptedBackend::default());
    let proxy = EeMcpProxy::new(backend.clone());
    for (name, args, expected_call) in [
        (
            "ee_create_directory",
            json!({ "path": "/abs/work/new" }),
            "create_directory:/abs/work/new",
        ),
        ("ee_delete_path", json!({ "path": "/abs/work/old" }), "delete_path:/abs/work/old"),
        (
            "ee_copy_path",
            json!({ "source_path": "/abs/work/a", "destination_path": "/abs/work/b" }),
            "copy_path:/abs/work/a:/abs/work/b",
        ),
        (
            "ee_move_path",
            json!({ "source_path": "/abs/work/b", "destination_path": "/abs/work/c" }),
            "move_path:/abs/work/b:/abs/work/c",
        ),
    ] {
        let request = CallToolRequestParams::new(name).with_arguments(arguments(args));
        proxy.dispatch_tool(&request).expect("filesystem dispatch");
        assert_eq!(backend.calls().last().map(String::as_str), Some(expected_call));
    }

    for (name, args) in [
        ("ee_create_directory", json!({ "path": "relative" })),
        ("ee_delete_path", json!({ "path": "relative" })),
        ("ee_copy_path", json!({ "source_path": "/abs/source", "destination_path": "relative" })),
        (
            "ee_move_path",
            json!({ "source_path": "relative", "destination_path": "/abs/destination" }),
        ),
    ] {
        let before = backend.calls().len();
        let request = CallToolRequestParams::new(name).with_arguments(arguments(args));
        assert!(proxy.dispatch_tool(&request).is_err(), "{name}");
        assert_eq!(backend.calls().len(), before, "{name} must fail before dispatch");
    }
}

#[test]
fn unavailable_symbol_index_uses_typed_error_code() {
    let error = ScriptedBackend::default()
        .symbol_dependency_map(String::from("/abs/work/a.rs"), 1, 0)
        .expect_err("default backend has no symbol index");
    assert!(error.message.starts_with("dependency_index_unavailable: "));
}

#[test]
fn manifest_rejects_arguments_and_disabled_tools_fail_at_tool_level() {
    let proxy = EeMcpProxy::with_supported_tools(
        Arc::new(ScriptedBackend::default()),
        vec![String::from("ee_workspace_roots")],
    );
    let manifest_with_arguments = CallToolRequestParams::new("ee_tools_manifest")
        .with_arguments(arguments(json!({ "unexpected": true })));
    assert!(proxy.dispatch_tool(&manifest_with_arguments).is_err());

    let _disabled = proxy
        .dispatch_tool(&CallToolRequestParams::new("ee_read_text_file"))
        .expect("known disabled tool returns a tool-level result, not a protocol error");
}
