//! Editor, plugin, and line-ending tests.
use super::*;

#[test]
fn begin_plugin_launch_blocks_duplicate_starts() {
    let peer = Box::new(DummyPeer);
    let mut state = CoreState::new(&peer.box_clone(), None, None);

    assert!(state.begin_plugin_launch("test-plugin"));
    assert!(!state.begin_plugin_launch("test-plugin"));
    assert!(state.launching_plugins.contains("test-plugin"));
}

#[test]
fn stderr_visibility_filters_noise() {
    assert!(stderr_is_user_visible("plugin panicked at line 7"));
    assert!(stderr_is_user_visible("ERROR: broken transport"));
    assert!(stderr_is_user_visible("request failed"));
    assert!(!stderr_is_user_visible("info: warming cache"));
}

#[test]
fn restart_delay_backs_off_for_repeated_crashes() {
    let peer = Box::new(DummyPeer);
    let mut state = CoreState::new(&peer.box_clone(), None, None);

    let first = state.next_restart_delay("test-plugin");
    let second = state.next_restart_delay("test-plugin");
    let third = state.next_restart_delay("test-plugin");

    assert!(first < second);
    assert!(second <= third);
    assert!(third <= Duration::from_millis(PLUGIN_RESTART_MAX_DELAY_MS));
}

#[test]
fn test_deserialize_view_id() {
    let de = json!("view-id-1");
    assert_eq!(ViewId::deserialize(&de).unwrap(), ViewId(1));

    let de = json!("not-a-view-id");
    assert!(ViewId::deserialize(&de).unwrap_err().is_data());
}

#[test]
fn large_file_line_ending_detection_is_deferred_after_open() {
    let peer = Box::new(DummyPeer);
    let ctx = RpcCtx::new(peer.box_clone());
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    for _ in 0..10_000 {
        tmp.write_all(b"  item\r\n").unwrap();
    }
    tmp.flush().unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
    let initial = core.inner().config_manager.get_buffer_config(buffer_id).items.clone();
    assert!(initial.translate_tabs_to_spaces);
    assert_eq!(initial.tab_size, 2);
    assert_eq!(initial.line_ending, "\n");

    core.inner().handle_idle(VERIFY_LINE_ENDINGS_IDLE_TOKEN);

    let verified = core.inner().config_manager.get_buffer_config(buffer_id).items.clone();
    assert_eq!(verified.line_ending, "\r\n");
}

#[test]
fn normalize_line_endings_edit_updates_buffer_config() {
    let peer = Box::new(DummyPeer);
    let ctx = RpcCtx::new(peer.box_clone());
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"alpha\r\nbeta\r\n").unwrap();
    tmp.flush().unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
    let initial = core.inner().config_manager.get_buffer_config(buffer_id).items.clone();
    assert_eq!(initial.line_ending, "\r\n");

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Edit(crate::rpc::EditCommand {
            view_id,
            cmd: crate::rpc::EditNotification::NormalizeLineEndings {
                line_ending: "\n".to_owned(),
            },
        }),
    );

    let updated = core.inner().config_manager.get_buffer_config(buffer_id).items.clone();
    assert_eq!(updated.line_ending, "\n");
}

#[test]
fn save_notification_reports_clear_vlf_status() {
    let peer = RecordingPeer::default();
    let ctx = RpcCtx::new(Box::new(peer.clone()));
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    core.inner().file_manager.set_open_policy(OpenPolicy::new(OpenThresholds {
        normal_bytes: 1,
        normal_lines: 1,
        vlf_bytes: 2,
        vlf_lines: 1,
        confirm_local_bytes: u64::MAX,
        confirm_remote_bytes: u64::MAX,
        confirm_web_bytes: u64::MAX,
    }));

    let tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.as_file().set_len(8).unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
    assert!(core.inner().editors.get(&buffer_id).unwrap().borrow().is_vlf());
    let read_only_store = crate::vlf::store::VlfStore::open(tmp.path()).unwrap();
    *core.inner().editors.get(&buffer_id).unwrap().borrow_mut() =
        crate::editor::Editor::with_vlf_store(read_only_store);

    peer.take_notifications();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let notifications = peer.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected VLF save alert");
    assert_eq!(
        params["msg"].as_str(),
        Some(
            "save disabled in VLF: VLF mode is read-only; copy, search, and navigation remain available"
        )
    );
}

#[test]
fn save_notification_saves_editable_vlf_buffer() {
    let peer = RecordingPeer::default();
    let ctx = RpcCtx::new(Box::new(peer.clone()));
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    core.inner().file_manager.set_open_policy(OpenPolicy::new(OpenThresholds {
        normal_bytes: 1,
        normal_lines: 1,
        vlf_bytes: 2,
        vlf_lines: 1,
        confirm_local_bytes: u64::MAX,
        confirm_remote_bytes: u64::MAX,
        confirm_web_bytes: u64::MAX,
    }));

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"alpha").unwrap();
    tmp.flush().unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
    {
        let inner = core.inner();
        let editor_cell = inner.editors.get(&buffer_id).unwrap();
        let mut editor = editor_cell.borrow_mut();
        assert!(editor.is_vlf());
        assert!(editor.enable_vlf_editing());
        let overlay_ctx =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(5, "\n", overlay_ctx).unwrap();
        editor.commit_vlf_overlay_revision(overlay_ctx.revision_id);
        assert!(!editor.is_pristine());
    }

    peer.take_notifications();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        drive_save_idle(&mut core, view_id);
        if std::fs::read_to_string(tmp.path()).unwrap() == "alpha\n"
            && core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine()
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("editable VLF async save did not complete within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine());
    let (saved_path, saved_mod_time, saved_has_changed) = {
        let inner = core.inner();
        let info = inner.file_manager.get_info(buffer_id).unwrap();
        (info.path.clone(), info.mod_time, info.has_changed)
    };
    assert_eq!(saved_path, tmp.path().to_path_buf());
    assert!(saved_mod_time.is_some());
    assert!(!saved_has_changed);
    let notifications = peer.take_notifications();
    assert!(notifications.iter().any(|(method, _)| method == "save_progress"));
    let expected = super::save_complete_alert(tmp.path());
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected save-complete alert");
    assert_eq!(params["msg"].as_str(), Some(expected.as_str()));
}

#[test]
fn edit_notification_vlf_replace_range_updates_search_and_viewport() {
    let peer = RecordingPeer::default();
    let ctx = RpcCtx::new(Box::new(peer.clone()));
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    core.inner().file_manager.set_open_policy(OpenPolicy::new(OpenThresholds {
        normal_bytes: 1,
        normal_lines: 1,
        vlf_bytes: 2,
        vlf_lines: 1,
        confirm_local_bytes: u64::MAX,
        confirm_remote_bytes: u64::MAX,
        confirm_web_bytes: u64::MAX,
    }));

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"alpha\nbeta\ngamma\n").unwrap();
    tmp.flush().unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Edit(crate::rpc::EditCommand {
            view_id,
            cmd: crate::rpc::EditNotification::VlfReplaceRange {
                start_line: 1,
                start_col: 4,
                end_line: 1,
                end_col: 4,
                text: String::from(" needle"),
            },
        }),
    );

    assert_eq!(vlf_text(&core, buffer_id), "alpha\nbeta needle\ngamma\n");

    let notifications = peer.take_notifications();
    let (_, scroll) = notifications
        .iter()
        .find(|(method, _)| method == "scroll_to")
        .expect("expected scroll_to after VLF edit");
    assert_eq!(scroll["line"], 1u64);
    assert_eq!(scroll["col"], 11u64);

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Edit(crate::rpc::EditCommand {
            view_id,
            cmd: crate::rpc::EditNotification::Find {
                chars: String::from("needle"),
                case_sensitive: true,
                regex: false,
                whole_words: false,
            },
        }),
    );

    let notifications = peer.take_notifications();
    let (_, status) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_search_status")
        .expect("expected vlf_search_status after VLF edit");
    assert_eq!(status["stored_match_count"], 1u64);
    assert_eq!(status["ranges"][0]["line"], 1u64);
    assert_eq!(status["ranges"][0]["start_col"], 5u64);
    assert_eq!(status["ranges"][0]["end_col"], 11u64);

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Edit(crate::rpc::EditCommand {
            view_id,
            cmd: crate::rpc::EditNotification::VlfViewport {
                line_start: 1,
                line_end: 1,
                generation: 17,
            },
        }),
    );

    let notifications = peer.take_notifications();
    let (_, viewport) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_chunks")
        .expect("expected vlf_chunks after VLF edit");
    assert_eq!(viewport["generation"], 17u64);
    let lines = viewport["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].as_str(), Some("beta needle"));
}
