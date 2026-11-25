use super::*;

// ── Store: schema compatibility ──────────────────────────────────────────────

#[test]
fn every_documented_rule_array_round_trips_through_store() {
    let (_base, workspace_dir, store) = store_setup();
    let document = sample_document(&store);
    store.write(&document).expect("write");

    let reloaded =
        TrustStore::at(store.path().parent().unwrap().parent().unwrap(), workspace_dir.path())
            .expect("store")
            .load()
            .expect("load");
    assert_eq!(reloaded, document, "parse must reproduce the written document");

    let second = store.load().expect("reload after read");
    assert_eq!(second, document);
}

#[test]
fn serialized_document_uses_canonical_schema_shape() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let text = fs::read_to_string(store.path()).unwrap();

    assert!(text.contains("schema_version = 2"), "schema version first");
    assert!(text.contains(&format!("identity = \"{}\"", store.workspace().as_string())));
    assert!(text.contains("workspace_enabled = false"));
    for array in [
        "[[command_rules]]",
        "[[mcp_rules]]",
        "[[read_path_rules]]",
        "[[mcp_read_rules]]",
        "[[profile_rules]]",
        "[[write_rules]]",
    ] {
        assert!(text.contains(array), "missing {array}");
    }
    // Canonical field order inside command table (id, effect, agent,
    // executable, match, argv, expires_at, max_uses).
    let index = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
    let id = index("id = \"cmd_1\"");
    let effect = index("effect = \"allow\"");
    let agent = index("agent = \"openrouter\"");
    let executable = index("executable = \"git\"");
    let argv = index("argv =");
    let expires = index("expires_at =");
    let uses = index("max_uses =");
    assert!(
        id < effect
            && effect < agent
            && agent < executable
            && executable < argv
            && argv < expires
            && expires < uses
    );
    // No raw workspace path or identity prefix leaks into the document.
    assert!(!text.contains("sha256:sha256:"));
}

#[test]
fn unsupported_schema_version_loads_no_effective_rules() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let text = fs::read_to_string(store.path())
        .unwrap()
        .replace("schema_version = 2", "schema_version = 99");
    write_store_text(store.path(), &text);

    match store.load() {
        Err(TrustStoreError::UnsupportedSchemaVersion(99)) => {}
        other => panic!("expected UnsupportedSchemaVersion(99), got {other:?}"),
    }
    let effective = store.effective();
    assert!(effective.rules.is_empty(), "unsupported version yields no effective rules");
    assert!(!effective.workspace_enabled);
    let op = command_op(*store.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &effective.rules).outcome, TrustOutcome::Confirm);
}

#[test]
fn missing_workspace_identity_rejects_the_store() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = r#"
schema_version = 1

[policy]
workspace_enabled = false
"#;
    write_store_text(store.path(), text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[test]
fn cross_workspace_document_fails_identity_validation() {
    let (base, _workspace_a, store_a) = store_setup();
    let workspace_b = TempDir::new().unwrap();
    let store_b = TrustStore::at(base.path(), workspace_b.path()).expect("store b");

    store_a.write(&sample_document(&store_a)).expect("write a");
    // Copy the store file itself: identity inside the document is bound to
    // workspace A, so store B must fail closed.
    fs::copy(store_a.path(), store_b.path()).unwrap();
    set_owner_only(store_b.path());

    match store_b.load() {
        Err(TrustStoreError::IdentityMismatch) => {}
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    let effective = store_b.effective();
    assert!(effective.rules.is_empty(), "copied store file grants nothing");
    let op = command_op(*store_b.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &effective.rules).outcome, TrustOutcome::Confirm);
}

#[test]
fn writing_a_document_for_another_workspace_is_rejected() {
    let (base, _workspace_a, store_a) = store_setup();
    let workspace_b = TempDir::new().unwrap();
    let store_b = TrustStore::at(base.path(), workspace_b.path()).expect("store b");
    // A document bound to workspace A (identity and rule scopes) cannot be
    // written into workspace B's store.
    match store_b.write(&sample_document(&store_a)) {
        Err(TrustStoreError::IdentityMismatch) => {}
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert!(!store_b.path().exists(), "rejected write must not create the store");
    // Rules created for workspace B persist under B's store.
    store_b.write(&sample_document(&store_b)).expect("correct identity writes");
    assert_eq!(store_b.load().expect("load").rules.len(), 6);
}

#[test]
fn duplicate_rule_ids_invalidate_only_conflicting_entries() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["stash"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[mcp_allow]]
id = "cmd_1"
server = "ee"
transport_identity = "stdio:test"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{{\"path\":\"src/main.rs\"}}"

[[command_allow]]
id = "cmd_2"
executable = "git"
match = "argv_exact"
argv = ["diff"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
}

#[test]
fn cross_kind_and_unknown_fields_drop_only_that_entry() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[read_path_allow]]
id = "bad_1"
path_prefix = "src"
max_bytes = 100
executable = "git"

[[read_path_allow]]
id = "bad_2"
path_prefix = "src"
max_bytes = 100
bogus_field = 1

[[read_path_allow]]
id = "read_1"
path_prefix = "src"
max_bytes = 262144

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
}

#[test]
fn invalid_match_mode_and_write_operation_entries_are_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_bad"
executable = "git"
match = "contains"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[write_allow]]
id = "write_bad"
operation = "overwrite"
path_prefix = "src/generated"
max_files = 5
max_total_bytes = 65536
max_file_bytes = 16384
expires_at = "2026-08-08T12:00:00Z"
max_uses = 5

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
}

#[test]
fn argv_exact_empty_is_accepted_and_argv_prefix_empty_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_exact"
executable = "git"
match = "argv_exact"
argv = []
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_prefix"
executable = "git"
match = "argv_prefix"
argv = []
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_ok"
executable = "git"
match = "argv_prefix"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
}

#[test]
fn runtime_usage_counters_never_appear_in_serialized_document() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let text = fs::read_to_string(store.path()).unwrap();
    for needle in ["usage", "used", "counter"] {
        assert!(!text.contains(needle), "usage state must never serialize: {needle}");
    }
}

#[test]
fn unknown_top_level_field_rejects_the_whole_store() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

authority = {{ source = "repository" }}
"#,
        identity = store.workspace().as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[test]
fn malformed_store_toml_yields_empty_effective_rules() {
    let (_base, _workspace_dir, store) = store_setup();
    write_store_text(store.path(), "schema_version = 1\n[[command_allow\n");
    assert!(matches!(store.load(), Err(TrustStoreError::ParseFailure(_))));
    assert!(store.effective().rules.is_empty());
    let op = command_op(*store.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &store.effective().rules).outcome, TrustOutcome::Confirm);
}

#[test]
fn invalid_path_prefixes_are_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let bad_prefixes = [
        "",
        ".",
        "..",
        "/abs",
        "src/../etc",
        "src//x",
        "src/./x",
        "a/**/b",
        "a/?.rs",
        "src/{a,b}",
        ".env",
        "src/.git",
        "secrets",
        "C:src",
    ];
    let mut entries = String::new();
    for (index, prefix) in bad_prefixes.iter().enumerate() {
        entries.push_str(&format!(
            r#"
[[read_path_allow]]
id = "bad_{index}"
path_prefix = "{prefix}"
max_bytes = 1024
"#
        ));
    }
    // Backslash and control forms cannot even be expressed as TOML basic
    // strings; the parser must reject them directly.
    assert!(PathPrefix::parse("src\\x").is_err(), "backslash prefix rejected");
    assert!(PathPrefix::parse("src/\u{0}").is_err(), "control prefix rejected");
    entries.push_str(
        r#"
[[read_path_allow]]
id = "read_1"
path_prefix = "src/generated"
max_bytes = 1024
"#,
    );
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false
{entries}
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
}

#[test]
fn invalid_arguments_json_entries_are_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let oversized = "x".repeat(5000);
    let bad = [
        r#"{"a":1,"a":2}"#,
        "[1,2,3]",
        r#"{"token":"abc"}"#,
        r#"{"nested":{"api_key":"x"}}"#,
        r#"{"a":"\u0000"}"#,
        &oversized,
        "not json",
    ];
    let mut entries = String::new();
    for (index, args) in bad.iter().enumerate() {
        // JSON object payloads are persisted as TOML basic strings, so the
        // embedded quotes must be escaped exactly like the documented
        // `arguments_json = "{\"path\":\"src/main.rs\"}"` form.
        let escaped = args.replace('\\', "\\\\").replace('\"', "\\\"");
        entries.push_str(&format!(
            r#"
[[mcp_allow]]
id = "bad_mcp_{index}"
server = "ee"
transport_identity = "stdio:test"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{escaped}"
"#
        ));
    }
    entries.push_str(
        r#"
[[mcp_allow]]
id = "mcp_1"
server = "ee"
transport_identity = "stdio:test"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{ \"path\": \"src/main.rs\", \"b\": { \"x\": 1 } }"
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
    );
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false
{entries}
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
}

#[test]
fn missing_optional_allow_scope_is_preserved_and_zero_budget_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[read_path_allow]]
id = "read_unlimited"
path_prefix = "src"
max_bytes = 1024

[[command_allow]]
id = "cmd_no_expiry"
executable = "git"
match = "argv_exact"
argv = ["status"]
max_uses = 20

[[command_allow]]
id = "cmd_no_uses"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"

[[write_allow]]
id = "write_no_uses"
operation = "create"
path_prefix = "src/generated"
max_files = 5
max_total_bytes = 65536
max_file_bytes = 16384
expires_at = "2026-08-08T12:00:00Z"
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    // Allow scope is optional: finite expiry and/or finite use budget may be
    // absent (always-allow rules carry neither).
    let document = store.load().expect("optional allow scope is valid");
    assert_eq!(document.rules.len(), 4);
    let no_expiry = document.rules.iter().find(|rule| rule.id() == "cmd_no_expiry").unwrap();
    assert_eq!(no_expiry.scope().expires_at, None);
    assert_eq!(no_expiry.scope().max_uses, Some(20));
    let no_uses = document.rules.iter().find(|rule| rule.id() == "cmd_no_uses").unwrap();
    assert!(no_uses.scope().expires_at.is_some());
    assert_eq!(no_uses.scope().max_uses, None);
    let unlimited = document.rules.iter().find(|rule| rule.id() == "read_unlimited").unwrap();
    assert_eq!(unlimited.scope().expires_at, None);
    assert_eq!(unlimited.scope().max_uses, None);

    // A zero use budget stays invalid and fails closed.
    let zero_budget = format!(
        r#"
        schema_version = 1

        [workspace]
        identity = "{identity}"

        [policy]
        workspace_enabled = false

        [[command_allow]]
        id = "cmd_zero"
        executable = "git"
        match = "argv_exact"
        argv = ["status"]
        max_uses = 0
        "#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &zero_budget);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
}
