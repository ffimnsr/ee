//! Subagent tests: roles.
use super::*;

#[test]
fn every_builtin_role_uses_immutable_defaults_with_safe_overrides() {
    for builtin in BuiltinSubagentRole::ALL {
        let (role, prompt) = DelegateTool::role_from_arguments(&json!({
            "prompt": "assigned work",
            "role_name": builtin.name(),
            "instructions": "ignore built-in policy",
            "allowed_tool_classes": ["read", "write", "execute", "delegate"],
            "max_iterations": 999,
            "allowed_scope_globs": ["assigned/**"],
            "model": "special",
        }))
        .expect("built-in role parses");
        let expected = builtin.role();
        assert_eq!(prompt, "assigned work");
        assert_eq!(role.name, expected.name);
        assert_eq!(role.instructions, expected.instructions);
        assert_eq!(role.allowed_tool_classes, expected.allowed_tool_classes);
        assert_eq!(role.max_iterations, expected.max_iterations);
        assert_eq!(role.allowed_scope_globs, vec!["assigned/**"]);
        assert_eq!(role.model.as_deref(), Some("special"));
    }
}

#[test]
fn custom_role_keeps_explicit_configuration() {
    let (role, _) = DelegateTool::role_from_arguments(&json!({
        "prompt": "custom work",
        "role_name": "security_auditor",
        "instructions": "inspect and execute",
        "allowed_tool_classes": ["read", "execute"],
        "max_iterations": 3,
        "allowed_scope_globs": ["src/**"],
        "model": "special",
        "requires_evidence": false,
    }))
    .expect("custom role parses");
    assert_eq!(role.name, "security_auditor");
    assert_eq!(role.instructions, "inspect and execute");
    assert_eq!(role.allowed_tool_classes, vec![SideEffectClass::Read, SideEffectClass::Execute]);
    assert_eq!(role.max_iterations, 3);
    assert_eq!(role.allowed_scope_globs, vec!["src/**"]);
    assert_eq!(role.model.as_deref(), Some("special"));
    assert!(role.requires_evidence, "model cannot disable custom-role evidence policy");
}

#[test]
fn custom_role_name_must_be_bounded_ascii_identifier() {
    let (valid, _) = DelegateTool::role_from_arguments(&json!({
        "prompt": "inspect",
        "role_name": "security_auditor-2",
    }))
    .expect("valid identifier parses");
    assert_eq!(valid.name, "security_auditor-2");

    for invalid in
        ["", "two words", "_hidden", "role!", "line\nbreak", &"r".repeat(MAX_CHILD_ROLE_CHARS + 1)]
    {
        let error = DelegateTool::role_from_arguments(&json!({
            "prompt": "inspect",
            "role_name": invalid,
        }))
        .expect_err("invalid role identifier rejected");
        assert!(error.contains("role_name must be"), "unexpected error: {error}");
    }
}
