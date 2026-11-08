//! Effect application tests.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ee_plugin_api::effect::{
    AgentToolResult, DecorationPatch, DecorationSpan, Effect, ScrollRequest, SelectionRequest,
    StatusItem, StatusItemKey, StyleRole, TaskRequest,
};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::model::{BufferId, EditRequest, TextEdit, ViewId};
use serde_json::json;

use crate::plugin_runtime::core_api::{EditorAccess, EffectSink};
use crate::plugin_runtime::effects::apply_effects;
use crate::plugin_runtime::services::tasks::{TaskHandler, TaskService};
use crate::plugin_runtime::services::ui::UiService;
use crate::plugin_runtime::test_support::MemoryEditor;

fn editor() -> MemoryEditor {
    let editor = MemoryEditor::new();
    editor.set_document(BufferId(1), ViewId(1), "hello");
    editor
}

struct Marker;

impl TaskHandler for Marker {
    fn run(
        &self,
        payload: &serde_json::Value,
        _cancel: &AtomicBool,
    ) -> Result<serde_json::Value, PluginError> {
        Ok(payload.clone())
    }
}

#[test]
fn effects_apply_in_order_and_record_owner() {
    let mut editor = editor();
    let tasks = TaskService::new();
    tasks.register("marker", Arc::new(Marker));
    let ui = UiService::new();
    let owner = PluginId::new("demo").unwrap();

    let effects = vec![
        Effect::SetStatus(StatusItem {
            key: StatusItemKey::new("demo.status"),
            text: "ok".to_owned(),
            alignment: ee_plugin_api::contribution::StatusAlignment::Right,
            priority: 1,
        }),
        Effect::ApplyEdit(EditRequest {
            view_id: ViewId(1),
            rev: 0,
            edits: vec![TextEdit {
                range: ee_plugin_api::model::Range { start: 0, end: 0 },
                new_text: "X".to_owned(),
            }],
            priority: 0,
            after_cursor: false,
            undo_group: Some(5),
            author: "demo".to_owned(),
        }),
        Effect::SetDecorations(DecorationPatch {
            view_id: ViewId(1),
            rev: 1,
            decoration_kind: "diagnostic".to_owned(),
            spans: vec![DecorationSpan {
                start: 0,
                end: 1,
                style: StyleRole::new("syntax.keyword"),
                text: None,
            }],
        }),
        Effect::RunTask(TaskRequest {
            task_kind: "marker".to_owned(),
            payload: json!({"ok": true}),
            timeout_ms: 1000,
        }),
        Effect::EmitAgentToolResult(AgentToolResult {
            call_id: "call-1".to_owned(),
            is_error: false,
            content: json!({"done": true}),
        }),
        Effect::Noop,
    ];

    let report = apply_effects(&mut editor, Some(&owner), effects, Some(&ui), Some(&tasks));
    assert!(report.is_ok(), "unexpected errors: {:?}", report.errors);
    assert_eq!(report.applied, 6);
    assert_eq!(editor.statuses()[0].0.as_ref().unwrap().as_str(), "demo");
    assert_eq!(editor.decorations()[0].1.rev, 1);
    assert_eq!(editor.agent_results()[0].0, "call-1");
    assert_eq!(editor.read_document(BufferId(1), 10).unwrap(), "Xhello");
}

#[test]
fn effect_panic_does_not_stop_later_effects() {
    let mut editor = PanicSink::default();
    let effects = vec![
        Effect::SetStatus(StatusItem {
            key: StatusItemKey::new("demo.status"),
            text: "ok".to_owned(),
            alignment: ee_plugin_api::contribution::StatusAlignment::Left,
            priority: 0,
        }),
        Effect::Noop,
        Effect::ScrollTo(ScrollRequest { view_id: ViewId(1), line: 3, column: 0 }),
    ];
    let report = apply_effects(&mut editor, None, effects, None, None);
    assert_eq!(report.applied, 2);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].1.to_string().contains("panicked"));
    assert_eq!(editor.scrolls, 1);
}

#[test]
fn status_removal_is_scoped_to_owner() {
    let mut editor = editor();
    let first = PluginId::new("first").unwrap();
    let second = PluginId::new("second").unwrap();
    let item = StatusItem {
        key: StatusItemKey::new("shared"),
        text: "ok".to_owned(),
        alignment: ee_plugin_api::contribution::StatusAlignment::Left,
        priority: 0,
    };
    let _ =
        apply_effects(&mut editor, Some(&first), vec![Effect::SetStatus(item.clone())], None, None);
    let _ = apply_effects(&mut editor, Some(&second), vec![Effect::SetStatus(item)], None, None);
    let _ = apply_effects(
        &mut editor,
        Some(&first),
        vec![Effect::RemoveStatus { key: StatusItemKey::new("shared") }],
        None,
        None,
    );
    assert_eq!(editor.statuses().len(), 1);
    assert_eq!(editor.statuses()[0].0.as_ref().unwrap().as_str(), "second");
}

#[derive(Default)]
struct PanicSink {
    scrolls: usize,
}

impl EffectSink for PanicSink {
    fn effect_apply_edit(
        &mut self,
        _request: &EditRequest,
    ) -> Result<ee_plugin_api::model::EditAck, PluginError> {
        Ok(ee_plugin_api::model::EditAck { applied: true, rev: 0, reason: None })
    }

    fn effect_set_selection(&mut self, _request: &SelectionRequest) -> Result<(), PluginError> {
        Ok(())
    }

    fn effect_scroll_to(&mut self, _request: &ScrollRequest) -> Result<(), PluginError> {
        self.scrolls += 1;
        Ok(())
    }

    fn effect_set_status(
        &mut self,
        _owner: Option<&PluginId>,
        _item: &StatusItem,
    ) -> Result<(), PluginError> {
        panic!("status panic")
    }

    fn effect_remove_status(
        &mut self,
        _owner: Option<&PluginId>,
        _key: &str,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn effect_set_decorations(
        &mut self,
        _owner: Option<&PluginId>,
        _patch: &DecorationPatch,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn effect_emit_agent_tool_result(
        &mut self,
        _call_id: &str,
        _is_error: bool,
        _content: &serde_json::Value,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}
