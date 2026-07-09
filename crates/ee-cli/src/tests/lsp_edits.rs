use std::sync::mpsc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use xi_core_lib::plugin_rpc::{CodeActionDescriptor, SymbolItem};
use xi_core_lib::rpc::LineReplacement;

use crate::app::App;
use crate::backend::{
    BackendEvent, CompletionSuggestion, NavigationTarget, coalesce_backend_events,
    format_location_message, parse_notification,
};
use crate::buffer::BufferManager;
use crate::picker::PickerKind;
use crate::tests::helpers::*;

#[test]
fn request_completion_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_completion(Some(2)).expect("completion request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_completion");
    assert_eq!(value["params"]["params"]["index"], 2);
}

#[test]
fn request_definition_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_definition().expect("definition request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_definition");
}

#[test]
fn request_declaration_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_declaration().expect("declaration request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_declaration");
}

#[test]
fn request_type_definition_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_type_definition().expect("type definition request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_type_definition");
}

#[test]
fn request_implementation_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_implementation().expect("implementation request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_implementation");
}

#[test]
fn request_hover_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_hover(Some((3, 7))).expect("hover request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_hover");
    assert_eq!(value["params"]["params"]["position"]["line"], 3);
    assert_eq!(value["params"]["params"]["position"]["column"], 7);
}

#[test]
fn request_code_actions_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_code_actions(Some(2)).expect("code action request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_code_actions");
    assert_eq!(value["params"]["params"]["index"], 2);
}

#[test]
fn request_rename_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_rename("renamed_symbol").expect("rename request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_rename");
    assert_eq!(value["params"]["params"]["new_name"], "renamed_symbol");
}

#[test]
fn delete_line_range_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.delete_line_range(3, 5).expect("line delete should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "delete_line_range");
    assert_eq!(value["params"]["params"]["start_line"], 3);
    assert_eq!(value["params"]["params"]["end_line"], 5);
}

#[test]
fn goto_column_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.goto_column(2, true).expect("goto column should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 2);
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn join_selections_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.join_selections(true).expect("join selections should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "join_selections");
    assert_eq!(value["params"]["params"]["select_space"], true);
}

#[test]
fn extend_line_below_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.extend_line_below(3).expect("extend line below should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "extend_line_below");
    assert_eq!(value["params"]["params"]["count"], 3);
}

#[test]
fn move_word_start_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.move_word_start(true, true, false).expect("move word start should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_word_start");
    assert_eq!(value["params"]["params"]["forward"], true);
    assert_eq!(value["params"]["params"]["long_word"], true);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn move_word_end_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.move_word_end(false, true).expect("move word end should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_word_end");
    assert_eq!(value["params"]["params"]["long_word"], false);
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn find_char_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.find_char('x', false, true, true).expect("find char should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "find_char");
    assert_eq!(value["params"]["params"]["target"], "x");
    assert_eq!(value["params"]["params"]["forward"], false);
    assert_eq!(value["params"]["params"]["inclusive"], true);
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn move_to_matching_bracket_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.move_to_matching_bracket(true).expect("matching bracket move should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_to_matching_bracket");
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn extend_to_line_bounds_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.extend_to_line_bounds().expect("extend to line bounds should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "extend_to_line_bounds");
}

#[test]
fn shrink_to_line_bounds_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.shrink_to_line_bounds().expect("shrink to line bounds should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "shrink_to_line_bounds");
}

#[test]
fn add_newline_above_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.add_newline_above().expect("add newline above should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "add_newline_above");
}

#[test]
fn add_newline_below_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.add_newline_below().expect("add newline below should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "add_newline_below");
}

#[test]
fn replay_block_insert_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.replay_block_insert(2, 4, 6, "abc", true).expect("block insert replay should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "replay_block_insert");
    assert_eq!(value["params"]["params"]["start_line"], 2);
    assert_eq!(value["params"]["params"]["end_line"], 4);
    assert_eq!(value["params"]["params"]["column"], 6);
    assert_eq!(value["params"]["params"]["text"], "abc");
    assert_eq!(value["params"]["params"]["append"], true);
}

#[test]
fn paste_register_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.paste_register("hello", false).expect("register paste should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "paste_register");
    assert_eq!(value["params"]["params"]["chars"], "hello");
    assert_eq!(value["params"]["params"]["before"], false);
}

#[test]
fn apply_line_replacements_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client
        .apply_line_replacements(&[LineReplacement { line: 2, text: String::from("beta") }])
        .expect("line replacements should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "apply_line_replacements");
    assert_eq!(value["params"]["params"]["replacements"][0]["line"], 2);
    assert_eq!(value["params"]["params"]["replacements"][0]["text"], "beta");
}

#[test]
fn definition_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 'd', 'e', 'f'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_definition");
}

#[test]
fn commit_undo_checkpoint_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "commit_undo_checkpoint");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "commit_undo_checkpoint");
}

#[test]
fn goto_lsp_commands_use_backend_edit() {
    let commands = [
        ("goto_declaration", "request_declaration"),
        ("goto_definition", "request_definition"),
        ("goto_type_definition", "request_type_definition"),
        ("goto_reference", "request_references"),
        ("goto_implementation", "request_implementation"),
    ];
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for (command, expected) in commands {
        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], expected);
    }
}

#[test]
fn codeaction_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":codeaction 3".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_code_actions");
    assert_eq!(value["params"]["params"]["index"], 3);
}

#[test]
fn code_action_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":code_action".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_code_actions");
    assert!(value["params"]["params"]["index"].is_null());
}

#[test]
fn complete_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":complete".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_completion");
    assert!(value["params"]["params"]["index"].is_null());
}

#[test]
fn completion_command_alias_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "completion");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_completion");
    assert!(value["params"]["params"]["index"].is_null());
}

#[test]
fn increment_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":increment".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "increase_number");
}

#[test]
fn rename_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":rename fresh_name".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_rename");
    assert_eq!(value["params"]["params"]["new_name"], "fresh_name");
}

#[test]
fn pending_completion_notification_opens_picker() {
    let mut app = App::from_path(None).unwrap();
    app.backend.pending_ui_actions.push(crate::backend::PendingUiAction::Completions {
        view_id: app.backend.view_id.clone(),
        items: vec![CompletionSuggestion {
            label: String::from("println!"),
            detail: Some(String::from("macro")),
            insert_text: None,
        }],
    });

    app.handle_pending_ui_actions();

    assert_eq!(app.picker.as_ref().map(|picker| picker.kind), Some(PickerKind::Completions));
}

#[test]
fn pending_code_actions_notification_opens_picker() {
    let mut app = App::from_path(None).unwrap();
    app.backend.pending_ui_actions.push(crate::backend::PendingUiAction::CodeActions {
        view_id: app.backend.view_id.clone(),
        actions: vec![CodeActionDescriptor { title: String::from("Extract variable") }],
    });

    app.handle_pending_ui_actions();

    assert_eq!(app.picker.as_ref().map(|picker| picker.kind), Some(PickerKind::CodeActions));
}

#[test]
fn pending_hover_notification_opens_popup() {
    let mut app = App::from_path(None).unwrap();
    app.backend.pending_ui_actions.push(crate::backend::PendingUiAction::Hover {
        view_id: app.backend.view_id.clone(),
        content: String::from("hover text"),
    });

    app.handle_pending_ui_actions();

    assert_eq!(app.hover_popup.as_ref().map(|popup| popup.content.as_str()), Some("hover text"));
}

#[test]
fn plugin_started_and_stopped_notifications_are_not_shown_in_status_line() {
    let started = json!({
        "view_id": "view-id-1",
        "plugin": "xi-lsp-plugin"
    });
    let stopped = json!({
        "view_id": "view-id-1",
        "plugin": "xi-lsp-plugin"
    });

    assert!(parse_notification("plugin_started", started).is_none());
    assert!(parse_notification("plugin_stopped", stopped).is_none());
}

#[test]
fn plugin_terminated_notification_updates_status_message() {
    let params = json!({
        "view_id": "view-id-1",
        "plugin": "rust-analyzer",
        "reason": {
            "kind": "rpc_timed_out",
            "limit_ms": 250,
            "method": "update"
        }
    });

    let event =
        parse_notification("plugin_terminated", params).expect("plugin_terminated should parse");
    match event {
        BackendEvent::Alert(message) => {
            assert_eq!(
                message,
                "plugin rust-analyzer terminated: rpc update timed out after 250 ms"
            );
        }
        other => panic!("unexpected backend event: {other:?}"),
    }
}

#[test]
fn ee_cli_sources_do_not_use_raw_lsp_or_plugin_routes() {
    let app_src = include_str!("../app/mod.rs");
    let buffer_src = include_str!("../buffer.rs");
    let backend_src = include_str!("../backend.rs");

    assert!(!app_src.contains("xi-lsp-plugin"));
    assert!(!app_src.contains("lsp."));
    assert!(!app_src.contains("line_cache"));

    assert!(!buffer_src.contains("xi-lsp-plugin"));
    assert!(!buffer_src.contains("lsp."));

    assert!(!backend_src.contains("show_hover"));
    assert!(!backend_src.contains("show_completions"));
    assert!(!backend_src.contains("show_locations"));
}

#[test]
fn format_location_message_formats_empty_and_single_results() {
    assert_eq!(format_location_message("definition", &[]), "definition: no locations");
    assert_eq!(
        format_location_message(
            "definition",
            &[NavigationTarget {
                path: String::from("/tmp/main.rs"),
                line: 2,
                column: 4,
                end_line: 2,
                end_column: 7,
            }],
        ),
        "definition: /tmp/main.rs:3:5"
    );
}

#[test]
fn completion_suggestion_deserializes_optional_fields() {
    let item: CompletionSuggestion = serde_json::from_value(json!({
        "label": "println!"
    }))
    .unwrap();
    assert_eq!(item.label, "println!");
    assert_eq!(item.detail, None);
    assert_eq!(item.insert_text, None);
}

#[test]
fn parse_notification_handles_symbols() {
    let event = parse_notification(
        "symbols",
        json!({
            "view_id": "view-id-1",
            "title": "Document Symbols",
            "symbols": [{
                "name": "my_func",
                "kind": "function",
                "path": "/src/lib.rs",
                "line": 10,
                "column": 0
            }]
        }),
    )
    .expect("symbols notification should parse");

    match event {
        BackendEvent::Symbols { view_id, title, symbols } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(title, "Document Symbols");
            assert_eq!(symbols.len(), 1);
            assert_eq!(symbols[0].name, "my_func");
            assert_eq!(symbols[0].kind, "function");
            assert_eq!(symbols[0].line, 10);
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn request_document_symbols_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_document_symbols().expect("document symbols request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_document_symbols");
}

#[test]
fn request_workspace_symbols_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_workspace_symbols("Foo").expect("workspace symbols request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_workspace_symbols");
    assert_eq!(value["params"]["params"]["query"], "Foo");
}

#[test]
fn plugin_lifecycle_helpers_emit_plugin_notifications() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.restart_plugin("plugin-name").unwrap();
    let restart: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(restart["method"], "plugin");
    assert_eq!(restart["params"]["command"], "restart");

    client.stop_plugin("plugin-name").unwrap();
    let stop: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(stop["params"]["command"], "stop");
}

#[test]
fn symbols_command_sends_document_symbols_request() {
    let mut app = App::from_path(None).unwrap();
    // Drain initial events so tests start clean.
    let _ = app.backend.drain_events();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "symbols".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    // Executing the command should not panic; LSP may not be active in test env.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
}

#[test]
fn symbols_notification_populates_picker() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::Symbols {
            view_id: String::from("view-id-1"),
            title: String::from("Document Symbols"),
            symbols: vec![SymbolItem {
                name: String::from("do_thing"),
                kind: String::from("function"),
                path: String::from("/src/lib.rs"),
                line: 5,
                column: 0,
            }],
        })
        .expect("send should succeed");

    mgr.drain_events().expect("drain should not fail");

    let pending = mgr.drain_pending_symbols();
    assert_eq!(pending.len(), 1);
    let (vid, title, syms) = &pending[0];
    assert_eq!(vid, "view-id-1");
    assert_eq!(title, "Document Symbols");
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "do_thing");

    // Verify the rx channel is empty (no RPC was emitted by the notification).
    assert!(rx.try_recv().is_err(), "no RPC should be emitted for symbols notification");
}

#[test]
fn vlf_chunks_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "generation": 42,
        "line_start": 10,
        "lines": ["hello", "world"],
        "syntax_spans": [[{ "start_byte": 0, "end_byte": 5, "scope": "keyword.control" }], []],
        "approximate_line_count": 500,
        "line_count_exact": false,
        "index_progress": 0.42,
    });
    let event = parse_notification("vlf_chunks", params).expect("should parse vlf_chunks");
    match event {
        BackendEvent::VlfChunks {
            view_id,
            generation,
            line_start,
            lines,
            syntax_spans,
            approximate_line_count,
            line_count_exact,
            index_progress,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(generation, 42);
            assert_eq!(line_start, 10);
            assert_eq!(lines, vec!["hello", "world"]);
            assert_eq!(syntax_spans.len(), 2);
            assert_eq!(syntax_spans[0][0].scope, "keyword.control");
            assert_eq!(approximate_line_count, 500);
            assert!(!line_count_exact);
            assert!((index_progress - 0.42).abs() < 1e-9);
        }
        other => panic!("expected VlfChunks, got {:?}", other),
    }
}

#[test]
fn coalesce_backend_events_keeps_latest_noisy_view_events() {
    let events = vec![
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 10,
            total_bytes: 100,
            complete: false,
            stored_match_count: 1,
            ranges: Vec::new(),
        },
        BackendEvent::VlfChunks {
            view_id: String::from("view-1"),
            generation: 1,
            line_start: 0,
            lines: vec![String::from("old")],
            syntax_spans: Vec::new(),
            approximate_line_count: 100,
            line_count_exact: false,
            index_progress: 0.0,
        },
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 50,
            total_bytes: 100,
            complete: false,
            stored_match_count: 1,
            ranges: Vec::new(),
        },
        BackendEvent::VlfChunks {
            view_id: String::from("view-1"),
            generation: 2,
            line_start: 5,
            lines: vec![String::from("new")],
            syntax_spans: Vec::new(),
            approximate_line_count: 100,
            line_count_exact: false,
            index_progress: 0.5,
        },
        BackendEvent::Update {
            view_id: String::from("view-1"),
            update: crate::backend::CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![],
            },
        },
    ];
    let coalesced = coalesce_backend_events(events);
    assert_eq!(coalesced.len(), 3);
    match &coalesced[0] {
        BackendEvent::VlfSearchStatus { scanned_bytes, .. } => {
            assert_eq!(*scanned_bytes, 50);
        }
        other => panic!("expected latest vlf search status, got {other:?}"),
    }
    match &coalesced[1] {
        BackendEvent::VlfChunks { generation, line_start, lines, .. } => {
            assert_eq!(*generation, 2);
            assert_eq!(*line_start, 5);
            assert_eq!(lines, &vec![String::from("new")]);
        }
        other => panic!("expected latest vlf chunks, got {other:?}"),
    }
}

#[test]
fn vlf_search_status_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "query": "needle",
        "scanned_bytes": 1024,
        "total_bytes": 4096,
        "complete": false,
        "stored_match_count": 2,
        "ranges": [
            { "line": 3, "start_col": 2, "end_col": 8 },
            { "line": 7, "start_col": 0, "end_col": 6 }
        ]
    });
    let event =
        parse_notification("vlf_search_status", params).expect("should parse vlf search status");
    match event {
        BackendEvent::VlfSearchStatus {
            view_id,
            query,
            scanned_bytes,
            total_bytes,
            complete,
            stored_match_count,
            ranges,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(query, "needle");
            assert_eq!(scanned_bytes, 1024);
            assert_eq!(total_bytes, 4096);
            assert!(!complete);
            assert_eq!(stored_match_count, 2);
            assert_eq!(ranges.len(), 2);
            assert_eq!(ranges[0].line, 3);
            assert_eq!(ranges[0].start_col, 2);
            assert_eq!(ranges[0].end_col, 8);
        }
        other => panic!("expected VlfSearchStatus, got {:?}", other),
    }
}
