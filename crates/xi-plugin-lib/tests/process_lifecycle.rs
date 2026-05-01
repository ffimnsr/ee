use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::json;
use tempfile::tempdir;

fn valid_config() -> serde_json::Value {
    json!({
        "line_ending": "\n",
        "tab_size": 4,
        "translate_tabs_to_spaces": false,
        "use_tab_stops": true,
        "font_face": "monospace",
        "font_size": 14.0,
        "auto_indent": true,
        "scroll_past_end": false,
        "wrap_width": 0,
        "word_wrap": false,
        "autodetect_whitespace": true,
        "surrounding_pairs": [["(", ")"]],
        "save_with_newline": true
    })
}

fn multi_view_buffer_info() -> serde_json::Value {
    json!({
        "buffer_id": 1,
        "views": ["view-id-1", "view-id-2"],
        "rev": 1,
        "buf_size": 12,
        "nb_lines": 1,
        "path": null,
        "syntax": "plain_text",
        "config": valid_config(),
    })
}

fn write_message(stdin: &mut impl Write, value: serde_json::Value) {
    writeln!(stdin, "{}", value).expect("stdin write should succeed");
}

#[test]
fn real_plugin_process_handles_multiview_lifecycle() {
    let temp = tempdir().expect("tempdir should exist");
    let log_path = temp.path().join("plugin-events.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_test-plugin-process"))
        .env("XI_PLUGIN_EVENT_LOG", &log_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plugin child should spawn");

    let mut stdin = child.stdin.take().expect("stdin should be piped");
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "plugin_id": 9,
                "buffer_info": [multi_view_buffer_info()],
                "protocol_version": 1,
                "core_capabilities": ["core_capability_negotiation", "graceful_shutdown", "restart_backoff"]
            }
        }),
    );
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "config_changed",
            "params": {
                "view_id": "view-id-1",
                "changes": { "tab_size": 8 }
            }
        }),
    );
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "did_close",
            "params": { "view_id": "view-id-1" }
        }),
    );
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "did_close",
            "params": { "view_id": "view-id-2" }
        }),
    );
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "shutdown",
            "params": {}
        }),
    );
    drop(stdin);

    let status = child.wait().expect("plugin child should exit");
    assert!(status.success(), "plugin child should exit cleanly: {status:?}");

    let log = fs::read_to_string(&log_path).expect("event log should exist");
    assert!(log.contains("initialize"));
    assert!(log.contains("new_view:view-id-1"));
    assert!(log.contains("new_view:view-id-2"));
    assert!(log.contains("config_changed:view-id-1"));
    assert!(log.contains("did_close:view-id-1"));
    assert!(log.contains("did_close:view-id-2"));
    assert!(log.contains("shutdown"));
}

#[test]
fn real_plugin_process_surfaces_crash_handling() {
    let temp = tempdir().expect("tempdir should exist");
    let log_path = temp.path().join("plugin-crash.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_test-plugin-process"))
        .env("XI_PLUGIN_EVENT_LOG", &log_path)
        .env("XI_PLUGIN_CRASH_ON_CONFIG", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plugin child should spawn");

    let mut stdin = child.stdin.take().expect("stdin should be piped");
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "plugin_id": 9,
                "buffer_info": [multi_view_buffer_info()],
                "protocol_version": 1,
                "core_capabilities": ["core_capability_negotiation", "graceful_shutdown", "restart_backoff"]
            }
        }),
    );
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "config_changed",
            "params": {
                "view_id": "view-id-1",
                "changes": { "tab_size": 8 }
            }
        }),
    );
    drop(stdin);

    let status = child.wait().expect("plugin child should exit");
    assert!(!status.success(), "plugin child should fail on requested crash: {status:?}");

    let log = fs::read_to_string(&log_path).expect("event log should exist");
    assert!(log.contains("initialize"));
    assert!(log.contains("config_changed:view-id-1"));
}
