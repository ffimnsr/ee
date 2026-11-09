//! Client-call regression tests with an in-memory host caller.

use std::sync::{Arc, Mutex};

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::CommandId;
use ee_plugin_api::ids::ServiceId;
use ee_plugin_api::model::{BufferId, SymbolItem, ViewId};
use serde_json::{Value, json};

use super::*;

#[derive(Default)]
struct FakeCaller {
    calls: Mutex<Vec<(String, String, Value)>>,
    effects: Mutex<Vec<Effect>>,
}

impl FakeCaller {
    fn take_calls(&self) -> Vec<(String, String, Value)> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl ServiceCaller for FakeCaller {
    fn call_service(
        &self,
        service: ServiceId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.calls.lock().unwrap().push((service.as_str().to_owned(), method.to_owned(), params));
        Ok(match (service.as_str(), method) {
            ("editor.document@1", "snapshot") => json!({
                "buffer_id": 1,
                "rev": 3,
                "len": 5,
                "line_count": 1,
                "path": "/tmp/demo.rs",
                "language": "rust",
                "vlf": false,
            }),
            ("editor.document@1", "document_text") => json!("hello"),
            ("editor.document@1", "revision") => json!(3),
            ("editor.views@1", "view_buffer") => json!(1),
            ("editor.views@1", "active_view") => json!(1),
            ("editor.selection@1", "get") => json!([{"start": 0, "end": 2}]),
            ("editor.commands@1", "list") => json!(["demo.run"]),
            ("editor.syntax@1", "symbols") => json!([{
                "name": "main",
                "kind": "function",
                "path": "/tmp/demo.rs",
                "line": 0,
                "column": 0,
                "children": [],
            }]),
            _ => Value::Null,
        })
    }

    fn submit_effects(&self, effects: Vec<Effect>) -> Result<(), PluginError> {
        self.effects.lock().unwrap().extend(effects);
        Ok(())
    }
}

fn caller() -> (Arc<FakeCaller>, Arc<dyn ServiceCaller>) {
    let fake = Arc::new(FakeCaller::default());
    (Arc::clone(&fake), fake)
}

#[test]
fn document_client_uses_versioned_service_methods() {
    let (fake, caller) = caller();
    let client = DocumentClient::new(caller);
    let snapshot = client.snapshot(BufferId(1)).unwrap();
    assert_eq!(snapshot.rev, 3);
    assert_eq!(snapshot.path.as_deref(), Some("/tmp/demo.rs"));
    assert_eq!(client.document_text(BufferId(1), 1024).unwrap(), "hello");
    let calls = fake.take_calls();
    assert_eq!(calls[0].0, "editor.document@1");
    assert_eq!(calls[0].1, "snapshot");
    assert_eq!(calls[0].2["buffer_id"], 1);
}

#[test]
fn views_commands_and_syntax_clients_are_typed() {
    let (fake, caller) = caller();
    assert_eq!(
        ViewsClient::new(Arc::clone(&caller)).view_buffer(ViewId(1)).unwrap(),
        Some(BufferId(1))
    );
    let commands = CommandsClient::new(Arc::clone(&caller));
    assert_eq!(commands.list().unwrap(), vec![CommandId::new("demo.run").unwrap()]);
    let symbols = SyntaxClient::new(caller).symbols(ViewId(1)).unwrap();
    assert_eq!(
        symbols,
        vec![SymbolItem {
            name: "main".to_owned(),
            kind: "function".to_owned(),
            path: "/tmp/demo.rs".to_owned(),
            line: 0,
            column: 0,
            children: Vec::new(),
        }]
    );
    let calls = fake.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, "editor.views@1");
    assert_eq!(calls[1].0, "editor.commands@1");
    assert_eq!(calls[2].0, "editor.syntax@1");
}

#[test]
fn ui_client_emits_declarative_effects() {
    let (fake, caller) = caller();
    let ui = UiClient::new(caller);
    ui.set_status(StatusItem {
        key: ee_plugin_api::effect::StatusItemKey::new("lsp.status"),
        text: "ok".to_owned(),
        alignment: StatusAlignment::Left,
        priority: 0,
    })
    .unwrap();
    ui.show_picker(
        ee_plugin_api::ids::ContributionId::new("lsp.ui.completion").unwrap(),
        1,
        vec![json!({"label": "foo"})],
    )
    .unwrap();
    let effects = fake.effects.lock().unwrap();
    assert!(matches!(effects[0], Effect::SetStatus(_)));
    assert!(matches!(effects[1], Effect::ShowPicker(_)));
}

#[test]
fn editor_view_loads_snapshot_and_selection() {
    let (_fake, caller) = caller();
    let view = EditorView::load(caller, ViewId(1)).unwrap();
    assert_eq!(view.get_document().unwrap(), "hello");
    assert_eq!(view.rev(), 3);
    assert_eq!(view.get_selections().unwrap()[0].end, 2);
    assert_eq!(view.get_path().unwrap().to_string_lossy(), "/tmp/demo.rs");
    assert_eq!(view.line_count(), 1);
    assert_eq!(view.get_line(0).unwrap(), "hello");
}

#[test]
fn lsp_and_tasks_clients_forward_requests() {
    let (fake, caller) = caller();
    let lsp = LspClient::new(Arc::clone(&caller));
    let request = LspRequest { view_id: ViewId(1), line: 1, column: 2, new_name: None };
    lsp.hover(&request).unwrap();
    TasksClient::new(caller).run("demo", json!({}), 100).unwrap();
    let calls = fake.take_calls();
    assert_eq!(calls[0].0, "editor.lsp@1");
    assert_eq!(calls[0].1, "hover");
    assert_eq!(calls[0].2["line"], 1);
    assert_eq!(calls[1].0, "editor.tasks@1");
    assert_eq!(calls[1].1, "run");
}
