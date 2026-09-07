//! VLF save / watcher tests.
use super::*;

#[test]
fn edit_notification_vlf_replace_range_delete_and_insert_reuse_undo_group() {
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
    tmp.write_all(b"alpha\n").unwrap();
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
                start_line: 0,
                start_col: 5,
                end_line: 0,
                end_col: 5,
                text: String::from("x"),
            },
        }),
    );
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Edit(crate::rpc::EditCommand {
            view_id,
            cmd: crate::rpc::EditNotification::VlfReplaceRange {
                start_line: 0,
                start_col: 6,
                end_line: 0,
                end_col: 6,
                text: String::from("y"),
            },
        }),
    );

    {
        let inner = core.inner();
        let editor = inner.editors.get(&buffer_id).unwrap().borrow();
        let delta = editor
            .vlf_overlay_delta_for_undo_group(1)
            .expect("expected first VLF undo group delta");
        assert_eq!(delta.ops.len(), 2);
    }
    assert_eq!(vlf_text(&core, buffer_id), "alphaxy\n");

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Edit(crate::rpc::EditCommand {
            view_id,
            cmd: crate::rpc::EditNotification::VlfReplaceRange {
                start_line: 0,
                start_col: 5,
                end_line: 0,
                end_col: 7,
                text: String::new(),
            },
        }),
    );

    assert_eq!(vlf_text(&core, buffer_id), "alpha\n");
}

#[cfg(feature = "notify")]
#[test]
fn vlf_save_does_not_mark_own_watcher_event_as_external_change() {
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
        assert!(editor.enable_vlf_editing());
        let overlay_ctx =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(5, "\n", overlay_ctx).unwrap();
        editor.commit_vlf_overlay_revision(overlay_ctx.revision_id);
    }

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

    core.inner().handle_open_file_fs_event(notify::Event {
        kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
        paths: vec![tmp.path().to_path_buf()],
        attrs: notify::event::EventAttributes::default(),
    });

    assert!(core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine());
    assert!(!core.inner().file_manager.get_info(buffer_id).unwrap().has_changed);
}

#[cfg(feature = "notify")]
#[test]
fn external_modification_after_vlf_save_is_still_detected() {
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
        assert!(editor.enable_vlf_editing());
        let overlay_ctx =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(5, "\n", overlay_ctx).unwrap();
        editor.commit_vlf_overlay_revision(overlay_ctx.revision_id);
    }

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        drive_save_idle(&mut core, view_id);
        if std::fs::read_to_string(tmp.path()).unwrap() == "alpha\n" {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("editable VLF async save did not complete within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let saved_mod_time = core.inner().file_manager.get_info(buffer_id).unwrap().mod_time;
    let rewrite_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        std::fs::write(tmp.path(), b"external\n").unwrap();
        let current_mod_time = std::fs::metadata(tmp.path()).unwrap().modified().ok();
        if current_mod_time != saved_mod_time {
            break;
        }
        if std::time::Instant::now() > rewrite_deadline {
            panic!("external rewrite did not change file metadata within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    core.inner().handle_open_file_fs_event(notify::Event {
        kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
        paths: vec![tmp.path().to_path_buf()],
        attrs: notify::event::EventAttributes::default(),
    });

    assert!(core.inner().file_manager.get_info(buffer_id).unwrap().has_changed);
}

#[test]
fn save_notification_save_as_keeps_vlf_mode_and_updates_buffer_path() {
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
    let other = tempfile::NamedTempFile::new().unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
    {
        let inner = core.inner();
        let editor_cell = inner.editors.get(&buffer_id).unwrap();
        let mut editor = editor_cell.borrow_mut();
        assert!(editor.enable_vlf_editing());
        let overlay_ctx =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(8, "!", overlay_ctx).unwrap();
        editor.commit_vlf_overlay_revision(overlay_ctx.revision_id);
    }

    peer.take_notifications();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save {
            view_id,
            file_path: other.path().display().to_string(),
        },
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        drive_save_idle(&mut core, view_id);
        if std::fs::read_to_string(other.path()).unwrap() == "\0\0\0\0\0\0\0\0!"
            && core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine()
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("VLF save-as did not complete within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(std::fs::metadata(tmp.path()).unwrap().len(), 8);
    let inner = core.inner();
    let editor = inner.editors.get(&buffer_id).unwrap().borrow();
    assert!(editor.is_vlf());
    assert!(editor.is_pristine());
    assert_eq!(inner.file_manager.get_info(buffer_id).unwrap().path, other.path().to_path_buf());
    let notifications = peer.take_notifications();
    assert!(notifications.iter().any(|(method, _)| method == "language_changed"));
    let expected = super::save_complete_alert(other.path());
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected save-complete alert");
    assert_eq!(params["msg"].as_str(), Some(expected.as_str()));
}

#[test]
fn save_notification_requires_explicit_vlf_save_as_policy() {
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
    tmp.as_file().set_len(600 * 1024 * 1024).unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
    {
        let inner = core.inner();
        let editor_cell = inner.editors.get(&buffer_id).unwrap();
        let mut editor = editor_cell.borrow_mut();
        assert!(editor.enable_vlf_editing());
        let overlay_ctx =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::Delete).unwrap();
        editor
            .vlf_store
            .as_ref()
            .unwrap()
            .apply_delete(crate::text_store::ByteRange::new(0, 70 * 1024 * 1024), overlay_ctx)
            .unwrap();
        editor.commit_vlf_overlay_revision(overlay_ctx.revision_id);
    }

    peer.take_notifications();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let notifications = peer.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected save-as-required alert");
    assert_eq!(
        params["msg"].as_str(),
        Some("save-as required for VLF: explicit destination must be chosen before saving")
    );
}

#[test]
fn vlf_second_save_after_rebase_uses_new_base_file() {
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
        assert!(editor.enable_vlf_editing());
        let first =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(5, "\n", first).unwrap();
        editor.commit_vlf_overlay_revision(first.revision_id);
    }

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let first_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        drive_save_idle(&mut core, view_id);
        if std::fs::read_to_string(tmp.path()).unwrap() == "alpha\n"
            && core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine()
        {
            break;
        }
        if std::time::Instant::now() > first_deadline {
            panic!("first VLF save did not complete within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    {
        let inner = core.inner();
        let editor_cell = inner.editors.get(&buffer_id).unwrap();
        let mut editor = editor_cell.borrow_mut();
        let second =
            editor.next_vlf_overlay_edit_context(crate::editor::EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(6, "beta\n", second).unwrap();
        editor.commit_vlf_overlay_revision(second.revision_id);
    }

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let second_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        drive_save_idle(&mut core, view_id);
        if std::fs::read_to_string(tmp.path()).unwrap() == "alpha\nbeta\n"
            && core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine()
        {
            break;
        }
        if std::time::Instant::now() > second_deadline {
            panic!("second VLF save did not complete within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(core.inner().editors.get(&buffer_id).unwrap().borrow().is_pristine());
}

#[test]
fn save_notification_completes_async_and_appends_newline() {
    let peer = RecordingPeer::default();
    let ctx = RpcCtx::new(Box::new(peer.clone()));
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"alpha").unwrap();
    tmp.flush().unwrap();

    let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
    let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::Save { view_id, file_path: tmp.path().display().to_string() },
    );

    let save_idle_token = SAVE_VIEW_IDLE_MASK | usize::from(view_id);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut notifications = Vec::new();
    loop {
        core.inner().handle_idle(save_idle_token);
        notifications.extend(peer.take_notifications());
        let saw_save_complete_alert = notifications.iter().any(|(method, params)| {
            method == "alert"
                && params["msg"].as_str() == Some(super::save_complete_alert(tmp.path()).as_str())
        });
        if fs::read_to_string(tmp.path()).unwrap() == "alpha\n" && saw_save_complete_alert {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("async save did not complete within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let expected = super::save_complete_alert(tmp.path());
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected save-complete alert");
    assert_eq!(params["msg"].as_str(), Some(expected.as_str()));
}

#[test]
fn finish_async_save_reports_cancelled_message() {
    let peer = RecordingPeer::default();
    let ctx = RpcCtx::new(Box::new(peer.clone()));
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    let path = PathBuf::from("/tmp/cancelled.txt");
    let request = crate::file::PreparedRopeSave {
        buffer_id: crate::tabs::BufferId(1),
        path: path.clone(),
        encoding: crate::file::CharacterEncoding::Utf8,
        kind: crate::file::PreparedRopeSaveKind::New,
        options: crate::file::SaveOptions::default(),
    };
    let saved_rev_id = xi_rope::engine::Engine::new(xi_rope::Rope::from("")).get_head_rev_id();

    core.inner().finish_async_save(
        ViewId(1),
        crate::whole_scan::SaveTaskResult {
            generation: 1,
            request: crate::whole_scan::CompletedSaveRequest::Rope(request),
            saved_rev_id,
            result: Err(crate::file::FileError::Io(
                std::io::Error::new(ErrorKind::Interrupted, "save cancelled"),
                path.clone(),
            )),
        },
    );

    let notifications = peer.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected save-cancelled alert");
    let expected = super::save_cancelled_alert(&path);
    assert_eq!(params["msg"].as_str(), Some(expected.as_str()));
}

#[test]
fn finish_async_save_reports_failed_message() {
    let peer = RecordingPeer::default();
    let ctx = RpcCtx::new(Box::new(peer.clone()));
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );

    let path = PathBuf::from("/tmp/failed.txt");
    let request = crate::file::PreparedRopeSave {
        buffer_id: crate::tabs::BufferId(1),
        path: path.clone(),
        encoding: crate::file::CharacterEncoding::Utf8,
        kind: crate::file::PreparedRopeSaveKind::New,
        options: crate::file::SaveOptions::default(),
    };
    let saved_rev_id = xi_rope::engine::Engine::new(xi_rope::Rope::from("")).get_head_rev_id();
    let error = crate::file::FileError::Io(
        std::io::Error::new(ErrorKind::PermissionDenied, "permission denied"),
        path.clone(),
    );

    core.inner().finish_async_save(
        ViewId(1),
        crate::whole_scan::SaveTaskResult {
            generation: 1,
            request: crate::whole_scan::CompletedSaveRequest::Rope(request),
            saved_rev_id,
            result: Err(error),
        },
    );

    let notifications = peer.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected save-failed alert");
    assert_eq!(
        params["msg"].as_str(),
        Some("save failed: permission denied. File path: /tmp/failed.txt")
    );
}
