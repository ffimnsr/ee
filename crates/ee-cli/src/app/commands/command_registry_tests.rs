//! Command-registry contract tests.
use super::*;
use std::collections::HashSet;

#[test]
fn command_registry_keeps_aliases_unique() {
    let aliases = App::command_registry_aliases();
    assert_eq!(aliases.len(), App::command_specs().len());
    let unique = aliases.iter().copied().collect::<HashSet<_>>();
    assert_eq!(aliases.len(), unique.len());
}

#[test]
fn command_registry_resolves_aliases_to_canonical_dispatch() {
    let completion = App::resolve_ex_command("completion").unwrap();
    assert_eq!(completion.canonical_id, "complete");
    assert_eq!(completion.dispatch, "complete");

    let terminal = App::resolve_ex_command("terminal").unwrap();
    assert_eq!(terminal.canonical_id, "term");
    assert_eq!(terminal.dispatch, "term");

    let pipe = App::resolve_ex_command("|").unwrap();
    assert_eq!(pipe.canonical_id, "pipe");
    assert_eq!(pipe.dispatch, "pipe");
}

#[test]
fn command_registry_rewrite_preserves_tail() {
    assert_eq!(
        App::rewrite_command_alias("terminal cargo test -p ee-cli").as_ref(),
        "term cargo test -p ee-cli"
    );
    assert_eq!(App::rewrite_command_alias("completion").as_ref(), "complete");
    assert_eq!(App::rewrite_command_alias("goto_next_function").as_ref(), "goto_next_function");
}

#[test]
fn command_registry_preserves_existing_completion_order_for_prefixes() {
    let aliases = App::command_registry_aliases();
    let wr = aliases.iter().copied().find(|alias| alias.starts_with("wr")).unwrap();
    assert_eq!(wr, "write!");

    let bp = aliases.iter().copied().find(|alias| alias.starts_with("bp")).unwrap();
    assert_eq!(bp, "bp");

    let ed = aliases.iter().copied().find(|alias| alias.starts_with("ed")).unwrap();
    assert_eq!(ed, "edit");
}

#[test]
fn command_help_items_document_phase_three_aliases() {
    let help = App::command_help_items().join("\n");
    assert!(help.contains(":config_reload"));
    assert!(help.contains(":bpick"));
    assert!(help.contains(":outline"));
    assert!(help.contains(":wsymbols"));
    assert!(help.contains(":set"));
}

#[test]
fn completion_now_covers_new_registry_aliases() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("conf");
    app.complete_command();
    assert_eq!(app.command_buffer, "config_reload");

    app.command_buffer = String::from("nohl");
    app.complete_command();
    assert_eq!(app.command_buffer, "nohlsearch");

    app.command_buffer = String::from("wsy");
    app.complete_command();
    assert_eq!(app.command_buffer, "wsymbol");
}

#[test]
fn completable_aliases_all_resolve_through_registry() {
    let unresolved: Vec<_> = App::command_registry_aliases()
        .into_iter()
        .filter(|alias| App::resolve_ex_command(alias).is_none())
        .collect();
    assert!(unresolved.is_empty(), "completion aliases missing from registry: {unresolved:?}");
}

#[test]
fn command_help_rows_only_claim_resolvable_aliases() {
    let unresolved: Vec<_> = App::claimed_command_aliases()
        .into_iter()
        .filter(|alias| App::resolve_ex_command(alias).is_none())
        .collect();
    assert!(unresolved.is_empty(), "help rows mention unknown aliases: {unresolved:?}");
}

#[test]
fn command_help_rows_include_category_usage_and_grouped_aliases() {
    let tab_row = App::format_command_help_item("tab_edit");
    assert!(tab_row.starts_with("[windows] "));
    assert!(tab_row.contains(":tabnew / :tabe / :tabedit [path]"));

    let grep_row = App::format_command_help_item("grep");
    assert!(grep_row.starts_with("[buffers] "));
    assert!(grep_row.contains(":grep <query>"));

    let complete_row = App::format_command_help_item("complete");
    assert!(complete_row.contains(":complete / :completion"));

    let substitute_row = App::format_command_help_item("substitute");
    assert!(substitute_row.starts_with("[editing] "));
    assert!(substitute_row.contains(":s / :substitute s/pattern/replacement/[flags]"));
}

#[test]
fn help_items_partially_reuse_registry_briefs() {
    let help = App::help_items();
    assert!(help[0].contains(":commands list ex commands and features"));
    assert!(help[0].contains(":keymap list high-value normal-mode bindings"));
    assert!(help[4].contains(":hover request LSP hover at cursor"));
    assert!(help[8].contains(":term <shell-command> run shell command and open transcript buffer"));
}

#[test]
fn help_items_command_mentions_resolve_through_registry() {
    let help = App::help_items();
    let unresolved: Vec<_> = help
        .iter()
        .flat_map(|line| {
            line.split_whitespace().filter_map(|token| {
                token
                    .trim_matches(|ch: char| ch == '|' || ch == ',')
                    .strip_prefix(':')
                    .filter(|alias| !alias.is_empty())
            })
        })
        .filter(|alias| App::resolve_ex_command(alias).is_none())
        .collect();
    assert!(unresolved.is_empty(), "general help mentions unknown aliases: {unresolved:?}");
}

#[test]
fn agents_commands_resolve_lowercase_snake_case_only() {
    for alias in [
        "agents",
        "agents_clear",
        "agents_close",
        "agents_config",
        "agents_config_set",
        "agents_config_toggle",
        "agents_layout",
        "agents_thoughts",
        "agents_mcp",
        "agents_mode_next",
        "agents_mode_prev",
        "agents_new",
        "agents_next",
        "agents_prev",
        "agents_stop",
    ] {
        let spec = App::resolve_ex_command(alias)
            .unwrap_or_else(|| panic!("agents alias `{alias}` missing from registry"));
        assert_eq!(spec.canonical_id, alias);
        assert_eq!(spec.dispatch, alias);
    }
}

#[test]
fn camel_case_agent_command_aliases_are_rejected() {
    for alias in [
        "Agents",
        "AgentClose",
        "AgentStop",
        "AgentNew",
        "AgentClear",
        "AgentNext",
        "AgentPrev",
        "AgentLayout",
        "AgentMcp",
    ] {
        assert!(
            App::resolve_ex_command(alias).is_none(),
            "CamelCase alias `{alias}` must not resolve"
        );
        assert_eq!(
            App::rewrite_command_alias(alias).as_ref(),
            alias,
            "CamelCase alias `{alias}` must not be rewritten"
        );
    }
}
