//! Command surface tests.
use super::App;
use crate::app::Mode;

#[test]
fn set_command_accepts_space_separated_value() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("set wrap_lines true");
    app.execute_command();
    assert!(app.config.wrap_lines, "wrap_lines true must enable wrapping");

    app.command_buffer = String::from("set wrap_lines false");
    app.execute_command();
    assert!(!app.config.wrap_lines, "wrap_lines false must disable wrapping");
}

#[test]
fn set_command_accepts_equals_and_bare_flags() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("set scrolloff=12");
    app.execute_command();
    assert_eq!(app.config.scroll_offset, 12);

    app.command_buffer = String::from("set nowrap");
    app.execute_command();
    assert!(!app.config.wrap_lines);

    app.command_buffer = String::from("set wrap");
    app.execute_command();
    assert!(app.config.wrap_lines);

    // Multiple options in one `:set`.
    app.command_buffer = String::from("set nu list");
    app.execute_command();
    assert_eq!(app.config.number_style, crate::config::NumberStyle::Absolute);
    assert!(app.config.show_visible_whitespace);
}

#[test]
fn set_nonumber_hides_gutter() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("set nonumber");
    app.execute_command();
    assert_eq!(app.config.number_style, crate::config::NumberStyle::None);

    app.command_buffer = String::from("set relativenumber");
    app.execute_command();
    assert_eq!(app.config.number_style, crate::config::NumberStyle::Relative);
}

#[test]
fn set_command_queries_and_summarizes() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("set wrap?");
    app.execute_command();
    assert_eq!(app.backend.status_message.as_deref(), Some("wrap=false"));

    app.command_buffer = String::from("set");
    app.execute_command();
    assert_eq!(app.backend.status_message.as_deref(), Some("set: all options at defaults"));

    app.command_buffer = String::from("set scrolloff=40");
    app.execute_command();
    app.command_buffer = String::from("set");
    app.execute_command();
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("set: scrolloff=40"),
        "summary lists only changed options"
    );

    // `no`-prefixed queries read the plain option.
    app.command_buffer = String::from("set wrap true");
    app.execute_command();
    app.command_buffer = String::from("set nowrap?");
    app.execute_command();
    assert_eq!(app.backend.status_message.as_deref(), Some("wrap=true"));
}

#[test]
fn set_command_reports_unknown_and_invalid() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("set nosuchopt");
    app.execute_command();
    // The `no` negation prefix is stripped before lookup, like vim.
    assert_eq!(app.backend.status_message.as_deref(), Some("unknown option: suchopt"));

    app.command_buffer = String::from("set wrap_lines maybe");
    app.execute_command();
    // vim parity: bool flag applied, stray token reported (E518-style).
    assert!(app.config.wrap_lines);
    assert_eq!(app.backend.status_message.as_deref(), Some("unknown option: maybe"));

    app.command_buffer = String::from("set scrolloff");
    app.execute_command();
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("set: invalid value \"true\" for scrolloff")
    );
}

#[test]
fn set_command_configures_editor_core_options() {
    let mut app = App::from_path(None).unwrap();

    app.command_buffer = String::from("set tabwidth=2 shiftwidth=8");
    app.execute_command();
    assert_eq!(app.config.tab_width, 2);
    assert_eq!(app.config.indent_size, 8);

    app.command_buffer = String::from("set indent_style tabs");
    app.execute_command();
    assert_eq!(app.config.indent_style, crate::config::IndentStyle::Tabs);

    app.command_buffer = String::from("set end_of_line crlf");
    app.execute_command();
    assert_eq!(app.config.end_of_line, crate::config::EndOfLine::CrLf);

    app.command_buffer = String::from("set colorcolumn=0");
    app.execute_command();
    assert_eq!(app.config.color_column, None);

    app.command_buffer = String::from("set statusline minimal");
    app.execute_command();
    assert_eq!(app.config.statusline_format, crate::config::StatuslineFormat::Minimal);
}

#[test]
fn set_command_pushes_runtime_config_to_backend() {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = crate::buffer::BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    // Simulate a rendered editor area so the wrap-width resize carries the
    // real text width, not the (0,0) default.
    app.last_editor_width = 100;
    app.last_editor_height = 40;
    app.last_terminal_size = (120, 40);

    app.command_buffer = String::from("set wrap_lines true");
    app.execute_command();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_word_wrap = false;
    let mut saw_resize = false;
    while std::time::Instant::now() < deadline && !(saw_word_wrap && saw_resize) {
        if let Ok(message) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
            let value: serde_json::Value = serde_json::from_str(&message).unwrap();
            if value["method"] == "set_config"
                && value["params"]["domain"] == "general"
                && value["params"]["changes"]["word_wrap"] == true
            {
                saw_word_wrap = true;
            }
            // The wrap width resize accompanies the config push.
            if value["params"]["method"] == "resize" {
                assert_eq!(value["params"]["params"]["width"], 100.0);
                saw_resize = true;
            }
        }
    }
    assert!(saw_word_wrap, "expected runtime word_wrap=true set_config notification");
    assert!(saw_resize, "expected resize edit with the editor width");
}

#[test]
fn start_privileged_save_confirm_populates_prompt_and_cancel_cleans_draft() {
    let Some(_tool) = crate::terminal::detect_elevation_tool() else {
        return;
    };

    let path = std::env::temp_dir().join(format!(
        "ee-privileged-save-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::write(&path, "seed\n").unwrap();

    let mut app = App::from_path(Some(path.clone())).unwrap();
    app.start_privileged_save_confirm().unwrap();

    assert_eq!(app.mode, Mode::PrivilegeConfirm);
    let prompt = app.backend.status_message.clone().unwrap_or_default();
    assert!(prompt.contains("cmd:"));
    assert!(prompt.contains(path.to_string_lossy().as_ref()));

    let draft_path =
        app.privileged_save_pending.as_ref().expect("pending privileged save").draft_path.clone();
    assert!(draft_path.exists());

    app.cancel_privileged_save_confirm();
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.privileged_save_pending.is_none());
    assert!(!draft_path.exists());

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn command_help_items_cover_all_ex_commands() {
    let help = App::command_help_items().join("\n");
    let missing: Vec<_> = App::ex_command_names()
        .iter()
        .copied()
        .filter(|command| !help.contains(&format!(":{command}")))
        .collect();
    assert!(missing.is_empty(), "missing commands from command palette: {missing:?}");
}

#[test]
fn documented_aliases_are_completable() {
    let completable =
        App::command_registry_aliases().into_iter().collect::<std::collections::HashSet<_>>();
    let missing: Vec<_> = App::documented_command_aliases()
        .into_iter()
        .filter(|alias| !completable.contains(alias.as_str()))
        .collect();
    assert!(missing.is_empty(), "documented aliases missing from completion: {missing:?}");
}

#[test]
fn agents_command_reports_disabled_by_default() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = false\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let cwd_restore = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    std::env::set_current_dir(cwd_restore).unwrap();
    drop(_cwd_lock);
    app.command_buffer = String::from("agents");
    app.execute_command();

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("agents mode disabled"), "unexpected status: {status}");
}

#[test]
fn agents_stop_is_noop_without_active_session() {
    let mut app = App::from_path(None).unwrap();
    app.command_buffer = String::from("agents_stop");
    app.execute_command();

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert_eq!(status, "no active agent session");
}

#[cfg(not(feature = "agents"))]
#[test]
fn agents_command_reports_disabled_without_feature() {
    let mut app = App::from_path(None).unwrap();
    app.command_buffer = String::from("agents");
    app.execute_command();

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("compiled without `agents` feature"), "status: {status}");
}

#[cfg(feature = "agents")]
#[test]
fn agents_command_opens_pane_when_runtime_config_on() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = true\n").unwrap();

    let _cwd_guard = crate::config::test_cwd_lock().lock().unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    let app = App::from_path(None);
    std::env::set_current_dir(original).unwrap();
    drop(_cwd_guard);
    let mut app = app.unwrap();
    // User-level agent servers can contain secret references. This test only
    // verifies runtime enablement opens the pane, so keep host setup hermetic.
    app.config.agents.servers.clear();
    app.config.agents.default_agent = None;

    app.command_buffer = String::from("agents");
    app.execute_command();

    // Phase 3: `:agents` opens the pane and starts the host lazily.
    assert_eq!(app.agents.layout, crate::app::AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.agents.host.is_some(), "host starts lazily for the pane");
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("agents"), "unexpected status: {status}");
}
