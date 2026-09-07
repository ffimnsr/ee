//! Command surface tests.
use super::App;
use crate::app::Mode;

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
