use super::super::*;

use serde_json::Value;
#[test]
fn ee_toml_parses_indent_style() {
    let toml = r#"indent_style = "tabs"
indent_size = 2
tab_width = 4
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();
    let mut s = EditorSettings::default();
    s.merge_toml(&raw, ConfigLayerKind::UserXdg);
    assert_eq!(s.indent_style, IndentStyle::Tabs);
    assert_eq!(s.indent_size, 2);
    assert_eq!(s.tab_width, 4);
}
#[test]
fn ee_toml_defaults_unchanged_when_field_absent() {
    let toml = r#"trim_trailing_whitespace = true"#;
    let raw: EeToml = toml::from_str(toml).unwrap();
    let mut s = EditorSettings::default();
    s.merge_toml(&raw, ConfigLayerKind::UserXdg);
    assert_eq!(s.indent_style, IndentStyle::Spaces); // unchanged
    assert!(s.trim_trailing_whitespace);
}
#[test]
fn editorconfig_star_section_applies() {
    let ec = "[*]\nindent_style = tab\nindent_size = 2\n";
    let target = std::path::Path::new("/foo/bar.rs");
    let mut s = EditorSettings::default();
    apply_editorconfig_text(&mut s, ec, target);
    assert_eq!(s.indent_style, IndentStyle::Tabs);
    assert_eq!(s.indent_size, 2);
}
#[test]
fn editorconfig_extension_section_matches() {
    let ec = "[*.rs]\nindent_size = 2\n[*.toml]\nindent_size = 4\n";
    let target = std::path::Path::new("/foo/main.rs");
    let mut s = EditorSettings::default();
    apply_editorconfig_text(&mut s, ec, target);
    assert_eq!(s.indent_size, 2);
}
#[test]
fn editorconfig_brace_group_matches() {
    let ec = "[*.{rs,toml}]\ninsert_final_newline = true\n";
    let target = std::path::Path::new("/foo/Cargo.toml");
    let mut s = EditorSettings::default();
    apply_editorconfig_text(&mut s, ec, target);
    assert!(s.insert_final_newline);
}
#[test]
fn editorconfig_non_matching_section_skipped() {
    let ec = "[*.py]\nindent_size = 2\n";
    let target = std::path::Path::new("/foo/main.rs");
    let mut s = EditorSettings::default(); // indent_size = 4
    apply_editorconfig_text(&mut s, ec, target);
    assert_eq!(s.indent_size, 4); // unchanged
}
#[test]
fn glob_match_star_basic() {
    assert!(glob_match("*.rs", "main.rs"));
    assert!(!glob_match("*.rs", "main.toml"));
}
#[test]
fn glob_match_double_star() {
    assert!(glob_match("**/*.rs", "src/main.rs"));
    assert!(glob_match("**/*.rs", "a/b/c/lib.rs"));
}
#[test]
fn xi_config_tables_split_global_and_file_overrides() {
    // The process cwd is process-global; lock it while mutating.
    let _cwd_lock = test_cwd_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("main.rs");
    let editorconfig = temp.path().join(".editorconfig");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    std::fs::write(&editorconfig, "[*]\nindent_style = tab\nindent_size = 2\n").unwrap();

    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();

    let (_, general, overrides) = xi_config_tables_for_file(Some(&file));

    std::env::set_current_dir(cwd).unwrap();

    assert_eq!(general.get("tab_size").and_then(Value::as_u64), Some(4));
    assert_eq!(overrides.get("tab_size").and_then(Value::as_u64), Some(2));
    assert_eq!(overrides.get("translate_tabs_to_spaces").and_then(Value::as_bool), Some(false));
}
#[test]
fn ee_toml_parses_keymap_overrides() {
    let toml = r#"
[keymap]
inherit_defaults = false

[[keymap.bindings]]
mode = "normal"
key = "H"
action = "request_hover"

[[keymap.unbind]]
mode = "normal"
key = "K"
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();
    let mut settings = EditorSettings::default();
    settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

    assert!(!settings.keymap.inherit_defaults);
    assert_eq!(settings.keymap.operations.len(), 2);
}
#[test]
fn ee_toml_parses_key_sequence_overrides() {
    let toml = r#"
[keymap]
inherit_defaults = true
sequence_timeout_ms = 250

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "file_picker"
description = "find files"
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();
    let mut settings = EditorSettings::default();
    settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

    assert_eq!(settings.keymap.sequence_bindings.len(), 1);
    assert_eq!(settings.keymap.sequence_timeout_ms, 250);
    assert_eq!(settings.keymap.sequence_bindings[0].description, "find files");
    assert_eq!(settings.keymap.sequence_bindings[0].sequence.len(), 3);
}
#[test]
fn xi_config_table_uses_configured_auto_and_smart_indent() {
    let raw: EeToml = toml::from_str("auto_indent = false\nsmart_indent = false\n").unwrap();
    let mut settings = EditorSettings::default();
    settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

    let table = settings.to_xi_config_table();

    assert_eq!(table.get("auto_indent").and_then(Value::as_bool), Some(false));
    assert_eq!(table.get("smart_indent").and_then(Value::as_bool), Some(false));
}
