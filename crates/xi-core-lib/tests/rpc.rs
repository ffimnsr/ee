// Copyright 2017 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::Builder;
use xi_core_lib::XiCore;
use xi_core_lib::test_helpers;
use xi_rpc::test_utils::{make_reader, test_channel};
use xi_rpc::{NewlineWriter, ReadError, RpcLoop};

#[test]
/// Tests that the handler responds to a standard startup sequence as expected.
fn test_startup() {
    let mut state = XiCore::new();
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let json = make_reader(r#"{"method":"client_started","params":{}}"#);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());
    rx.expect_rpc("available_languages");

    let json = make_reader(r#"{"id":0,"method":"new_view","params":{}}"#);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());
    assert_eq!(rx.expect_response(), Ok(json!("view-id-1")));
    rx.expect_rpc("available_plugins");
    rx.expect_rpc("language_changed");
    rx.expect_rpc("document_mode");
    rx.expect_rpc("update");
    rx.expect_rpc("scroll_to");
    rx.expect_nothing();
}

#[test]
/// Tests that the handler creates and destroys views and buffers
fn test_state() {
    let mut state = XiCore::new();

    let write = NewlineWriter::new(io::sink());
    let json = make_reader(
        r#"{"method":"client_started","params":{}}
{"id":0,"method":"new_view","params":{"file_path":"../Cargo.toml"}}"#,
    );
    let mut rpc_looper = RpcLoop::new(write);
    rpc_looper.mainloop(|| json, &mut state).unwrap();

    {
        let state = state.inner();
        assert_eq!(state._test_open_editors(), vec![test_helpers::new_buffer_id(2)]);
        assert_eq!(state._test_open_views(), vec![test_helpers::new_view_id(1)]);
    }

    let json = make_reader(r#"{"method":"close_view","params":{"view_id":"view-id-1"}}"#);
    rpc_looper.mainloop(|| json, &mut state).unwrap();
    {
        let state = state.inner();
        assert_eq!(state._test_open_views(), Vec::new());
        assert_eq!(state._test_open_editors(), Vec::new());
    }

    let json = make_reader(
        r#"{"id":1,"method":"new_view","params":{}}
{"id":2,"method":"new_view","params":{}}
{"id":3,"method":"new_view","params":{}}"#,
    );

    rpc_looper.mainloop(|| json, &mut state).unwrap();
    {
        let state = state.inner();
        assert_eq!(state._test_open_editors().len(), 3);
    }
}

/// Test whether xi-core invalidates cache lines upon a cursor motion.
#[test]
fn test_invalidate() {
    let mut state = XiCore::new();
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let json = make_reader(
        r#"{"method":"client_started","params":{}}
{"id":0,"method":"new_view","params":{}}
"#,
    );
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let mut edit_cmds = String::new();

    for i in 1..20 {
        // add lines "line 1", "line 2",...
        edit_cmds.push_str(r#"{"method":"edit","params":{"view_id":"view-id-1","method":"insert","params":{"chars":"line "#);
        edit_cmds.push_str(&i.to_string());
        edit_cmds.push_str(
            r#""}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"insert_newline","params":[]}}
"#,
        );
    }

    let json = make_reader(edit_cmds);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    // jump to line 1, then jump to line 18
    const MOVEMENTS: &str = r#"{"method":"edit","params":{"view_id":"view-id-1","method":"goto_line","params":{"line":1}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_line","params":{"line":18}}}"#;

    let json = make_reader(MOVEMENTS);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let mut last_ops = Vec::new();

    while let Some(Ok(resp)) = rx.next_timeout(std::time::Duration::from_millis(1000)) {
        if !resp.is_response() && resp.get_method().unwrap() == "update" {
            let ops = resp.0.as_object().unwrap()["params"].as_object().unwrap()["update"]
                .as_object()
                .unwrap()["ops"]
                .as_array()
                .unwrap();
            last_ops = ops.clone();

            // Verify that the "invalidate" ops can only go first or last.
            if ops.len() > 2 {
                debug_assert!(
                    !ops.iter()
                        // step over leading "invalidate" and "skip"
                        .skip_while(|op| op["op"].as_str().unwrap() == "invalidate"
                            || op["op"].as_str().unwrap() == "skip")
                        // current op (ins/copy/update) adds lines;
                        // wait for another invalidate/skip
                        .skip_while(|op| op["op"].as_str().unwrap() != "invalidate")
                        .any(|op| {
                            op["op"].as_str().unwrap() != "invalidate"
                                && op["op"].as_str().unwrap() != "skip"
                        }),
                    "bad update: {}",
                    ops.iter()
                        .map(|op| format!(
                            "{} {}",
                            op["op"].as_str().unwrap(),
                            op["n"].as_u64().unwrap()
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }

    // Dump the last vector of ops.
    // Verify that there is an "update" op in case of a cursor motion.
    assert_eq!(
        last_ops
            .iter()
            .map(|op| {
                let op_in = op.as_object().unwrap();
                (op_in["op"].as_str().unwrap(), op_in["n"].as_u64().unwrap())
            })
            .collect::<Vec<_>>(),
        [("copy", 1), ("update", 1), ("copy", 5), ("copy", 11), ("update", 2)]
    );
}

#[test]
/// Tests that the runloop exits with the correct error when receiving
/// malformed json.
fn test_malformed_json() {
    let mut state = XiCore::new();
    let write = NewlineWriter::new(io::sink());
    let mut rpc_looper = RpcLoop::new(write);
    // malformed json: method should be in quotes.
    let read = make_reader(
        r#"{"method":"client_started","params":{}}
{"id":0,method:"new_view","params":{}}"#,
    );
    match rpc_looper.mainloop(|| read, &mut state).expect_err("malformed json exits with error") {
        ReadError::Json(_) => (), // expected
        err => panic!("Unexpected error: {:?}", err),
    }
    // read should have ended after first item
    {
        let state = state.inner();
        assert_eq!(state._test_open_editors().len(), 0);
    }
}

#[test]
/// Sends all of the cursor movement-related commands, and verifies that
/// they are handled.
///
///
/// Note: this is a test of message parsing, not of editor behaviour.
fn test_movement_cmds() {
    let mut state = XiCore::new();
    let write = NewlineWriter::new(io::sink());
    let mut rpc_looper = RpcLoop::new(write);
    // init a new view
    let json = make_reader(
        r#"{"method":"client_started","params":{}}
{"id":0,"method":"new_view","params":{}}"#,
    );
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let json = make_reader(MOVEMENT_RPCS);
    rpc_looper.mainloop(|| json, &mut state).unwrap();
}

#[test]
/// Sends all the commands which modify the buffer, and verifies that they
/// are handled.
fn test_text_commands() {
    let mut state = XiCore::new();
    let write = NewlineWriter::new(io::sink());
    let mut rpc_looper = RpcLoop::new(write);
    // init a new view
    let json = make_reader(
        r#"{"method":"client_started","params":{}}
{"id":0,"method":"new_view","params":{}}"#,
    );
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let json = make_reader(TEXT_EDIT_RPCS);
    rpc_looper.mainloop(|| json, &mut state).unwrap();
}

#[test]
fn test_other_edit_commands() {
    let mut state = XiCore::new();
    let write = NewlineWriter::new(io::sink());
    let mut rpc_looper = RpcLoop::new(write);
    // init a new view
    let json = make_reader(
        r#"{"method":"client_started","params":{}}
{"id":0,"method":"new_view","params":{}}"#,
    );
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let json = make_reader(OTHER_EDIT_RPCS);
    rpc_looper.mainloop(|| json, &mut state).unwrap();
}

#[test]
fn move_parent_node_start_rpc_updates_selection_annotation() {
    let mut state = XiCore::new();
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let file = Builder::new().suffix(".rs").tempfile().unwrap();
    let path = serde_json::to_string(&file.path().to_string_lossy().to_string()).unwrap();
    let startup = format!(
        "{{\"method\":\"client_started\",\"params\":{{}}}}\n{{\"id\":0,\"method\":\"new_view\",\"params\":{{\"file_path\":{path}}}}}"
    );
    let json = make_reader(&startup);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());
    rx.expect_rpc("available_languages");
    assert_eq!(rx.expect_response(), Ok(json!("view-id-1")));
    rx.expect_rpc("available_plugins");
    rx.expect_rpc("language_changed");
    rx.expect_rpc("document_mode");
    rx.expect_rpc("update");
    rx.expect_rpc("scroll_to");
    rx.expect_nothing();

    let source = "fn main() { foo(bar); }";
    let bar = source.find("bar").unwrap();
    let parent_start = source.rfind('(').unwrap() as u64;
    let bar_end = (bar + "bar".len()) as u64;
    let cmds = format!(
        "{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"insert\",\"params\":{{\"chars\":\"{}\"}}}}}}\n{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"gesture\",\"params\":{{\"line\":0,\"col\":{},\"ty\":\"point_select\"}}}}}}\n{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"gesture\",\"params\":{{\"line\":0,\"col\":{},\"ty\":\"range_select\"}}}}}}\n{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"move_parent_node_start\",\"params\":[]}}}}",
        source, bar, bar_end
    );
    let json = make_reader(&cmds);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let mut last_selection = None;
    while let Some(Ok(resp)) = rx.next_timeout(Duration::from_millis(100)) {
        if !resp.is_response() && resp.get_method().unwrap() == "update" {
            last_selection = selection_ranges_from_update(&resp.0);
        }
    }

    assert_eq!(last_selection, Some(vec![[0, parent_start, 0, parent_start]]));
}

#[test]
fn move_parent_node_end_rpc_updates_selection_annotation() {
    let mut state = XiCore::new();
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let file = Builder::new().suffix(".rs").tempfile().unwrap();
    let path = serde_json::to_string(&file.path().to_string_lossy().to_string()).unwrap();
    let startup = format!(
        "{{\"method\":\"client_started\",\"params\":{{}}}}\n{{\"id\":0,\"method\":\"new_view\",\"params\":{{\"file_path\":{path}}}}}"
    );
    let json = make_reader(&startup);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());
    rx.expect_rpc("available_languages");
    assert_eq!(rx.expect_response(), Ok(json!("view-id-1")));
    rx.expect_rpc("available_plugins");
    rx.expect_rpc("language_changed");
    rx.expect_rpc("document_mode");
    rx.expect_rpc("update");
    rx.expect_rpc("scroll_to");
    rx.expect_nothing();

    let source = "fn main() { foo(bar); }";
    let bar = source.find("bar").unwrap();
    let parent_end = (source.rfind(')').unwrap() + 1) as u64;
    let bar_end = (bar + "bar".len()) as u64;
    let cmds = format!(
        "{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"insert\",\"params\":{{\"chars\":\"{}\"}}}}}}\n{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"gesture\",\"params\":{{\"line\":0,\"col\":{},\"ty\":\"point_select\"}}}}}}\n{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"gesture\",\"params\":{{\"line\":0,\"col\":{},\"ty\":\"range_select\"}}}}}}\n{{\"method\":\"edit\",\"params\":{{\"view_id\":\"view-id-1\",\"method\":\"move_parent_node_end\",\"params\":[]}}}}",
        source, bar, bar_end
    );
    let json = make_reader(&cmds);
    assert!(rpc_looper.mainloop(|| json, &mut state).is_ok());

    let mut last_selection = None;
    while let Some(Ok(resp)) = rx.next_timeout(Duration::from_millis(100)) {
        if !resp.is_response() && resp.get_method().unwrap() == "update" {
            last_selection = selection_ranges_from_update(&resp.0);
        }
    }

    assert_eq!(last_selection, Some(vec![[0, parent_end, 0, parent_end]]));
}

fn selection_ranges_from_update(message: &Value) -> Option<Vec<[u64; 4]>> {
    let annotations = message.get("params")?.get("update")?.get("annotations")?.as_array()?;
    let selection = annotations.iter().find(|annotation| {
        annotation.get("type") == Some(&Value::String(String::from("selection")))
    })?;
    let ranges = selection.get("ranges")?.as_array()?;
    Some(
        ranges
            .iter()
            .map(|range| {
                let items = range.as_array().unwrap();
                [
                    items[0].as_u64().unwrap(),
                    items[1].as_u64().unwrap(),
                    items[2].as_u64().unwrap(),
                    items[3].as_u64().unwrap(),
                ]
            })
            .collect(),
    )
}

//TODO: test saving rpc
//TODO: test plugin rpc

const MOVEMENT_RPCS: &str = r#"{"method":"edit","params":{"view_id":"view-id-1","method":"move_up","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_down","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_up_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_down_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_left","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_backward","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_right","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_forward","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_left_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_right_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_word_left","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_word_right","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_word_left_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_word_right_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_beginning_of_paragraph","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_end_of_paragraph","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_left_end_of_line","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_left_end_of_line_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_right_end_of_line","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_right_end_of_line_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_beginning_of_document","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_beginning_of_document_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_end_of_document","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_to_end_of_document_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"scroll_page_up","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"scroll_page_down","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"page_up_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"page_down_and_modify_selection","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"select_all","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"add_selection_above","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"add_selection_below","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"collapse_selections","params":[]}}"#;

const TEXT_EDIT_RPCS: &str = r#"{"method":"edit","params":{"view_id":"view-id-1","method":"insert","params":{"chars":"a"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"delete_backward","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"delete_forward","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"delete_word_forward","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"delete_word_backward","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"delete_to_end_of_paragraph","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"insert_newline","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"insert_tab","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"yank","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"undo","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"redo","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"transpose","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"uppercase","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"lowercase","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"indent","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"outdent","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"duplicate_line","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"replace_next","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"replace_all","params":[]}}"#;

const OTHER_EDIT_RPCS: &str = r#"{"method":"edit","params":{"view_id":"view-id-1","method":"scroll","params":[0,1]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_line","params":{"line":1}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"request_lines","params":[0,1]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"drag","params":[17,15,0]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "toggle_sel"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "point_select"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "range_select"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "line_select"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "word_select"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "multi_line_select"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"gesture","params":{"line": 1, "col": 2, "ty": "multi_word_select"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"find","params":{"case_sensitive":false,"chars":"m"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"multi_find","params":{"queries": [{"case_sensitive":false,"chars":"m"}]}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"find_next","params":{"wrap_around":true}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"find_previous","params":{"wrap_around":true}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"find_all","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"highlight_find","params":{"visible":true}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"selection_for_find","params":{"case_sensitive":true}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"replace","params":{"chars":"a"}}}
{"method":"edit","params":{"view_id":"view-id-1","method":"selection_for_replace","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_next_function","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_prev_function","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_next_class","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_prev_class","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_next_parameter","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_prev_parameter","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_next_comment","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_prev_comment","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_next_test","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_prev_test","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_next_paragraph","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"goto_prev_paragraph","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_parent_node_start","params":[]}}
{"method":"edit","params":{"view_id":"view-id-1","method":"move_parent_node_end","params":[]}}"#;
