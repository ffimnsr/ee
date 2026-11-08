//! Editor plugin host tests.

mod effects_tests;
mod host_tests;
mod service_tests;

use std::sync::{Arc, Mutex};

use ee_plugin_api::context::{Plugin, PluginContext};
use ee_plugin_api::contribution::{Contribution, StatusAlignment, StatusItemContribution};
use ee_plugin_api::effect::{CommandHandler, CommandRequest, CommandSpec, Effect};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::HostKind;
use ee_plugin_api::ids::CommandId;
use ee_plugin_api::manifest::HostTarget;
use ee_plugin_api::model::{BufferId, EditRequest, Range, TextEdit, ViewId};
use ee_plugin_api::service::Service;
use ee_plugin_runtime::builtin::{BuiltinCatalog, BuiltinDescriptor, builtin_manifest};
use ee_plugin_runtime::manager::{ManagerStatus, PluginManagerConfig, PluginStatus};
use serde_json::json;

use crate::plugin_runtime::events::DocumentChanged;
use crate::plugin_runtime::host::{EditorPluginHost, HostNotifier};
use crate::plugin_runtime::services::document::DocumentService;
use crate::plugin_runtime::services::fs::FsScope;
use crate::plugin_runtime::test_support::MemoryEditor;

/// Plugin used by host integration tests.
pub(crate) struct TestPlugin {
    pub name: String,
    pub order: Arc<Mutex<Vec<String>>>,
    pub panic_on_event: bool,
}

impl Plugin for TestPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn apply(&mut self, ctx: &PluginContext) -> Result<(), PluginError> {
        self.order.lock().unwrap().push(self.name.clone());
        let document = ctx.require::<DocumentService>()?;
        let _ = ctx.command(
            CommandSpec {
                id: CommandId::new(format!("{}.run", self.name)).unwrap(),
                title: "Run".to_owned(),
                description: String::new(),
                category: None,
                mutates_document: true,
                args_schema: None,
            },
            Arc::new(ReadCommand { document }),
        )?;
        let _ = ctx.contribute(Contribution::StatusItem(StatusItemContribution {
            key: format!("{}.status", self.name),
            text: "ready".to_owned(),
            alignment: StatusAlignment::Right,
            priority: 0,
        }))?;
        let _ = ctx.contribute(Contribution::DecorationProvider(
            ee_plugin_api::contribution::DecorationProviderContribution {
                id: ee_plugin_api::ids::ContributionId::new(format!("{}.decorations", self.name))
                    .unwrap(),
                kinds: vec!["diagnostic".to_owned()],
                revision_policy: None,
            },
        ))?;
        if self.panic_on_event {
            let _ = ctx.on::<DocumentChanged>(|_| panic!("boom"))?;
        }
        Ok(())
    }
}

struct ReadCommand {
    document: Arc<DocumentService>,
}

impl CommandHandler for ReadCommand {
    fn invoke(&self, request: CommandRequest) -> Result<Vec<Effect>, PluginError> {
        let snapshot = self.document.snapshot(BufferId(1))?;
        Ok(vec![Effect::ApplyEdit(EditRequest {
            view_id: request.view_id,
            rev: snapshot.rev,
            edits: vec![TextEdit { range: Range { start: 0, end: 0 }, new_text: "!".to_owned() }],
            priority: 0,
            after_cursor: false,
            undo_group: Some(7),
            author: request.id.as_str().to_owned(),
        })])
    }
}

#[derive(Default)]
pub(crate) struct RecordingNotifier {
    pub batches: Mutex<Vec<(u64, usize)>>,
    pub statuses: Mutex<Vec<Vec<PluginStatus>>>,
    pub health: Mutex<Vec<(String, String, Option<String>)>>,
}

impl HostNotifier for RecordingNotifier {
    fn plugin_contributions(
        &self,
        revision: u64,
        upsert: Vec<Contribution>,
        _remove: Vec<ee_plugin_api::ids::ContributionId>,
    ) {
        self.batches.lock().unwrap().push((revision, upsert.len()));
    }

    fn plugin_status(&self, status: &ManagerStatus) {
        self.statuses.lock().unwrap().push(status.plugins.clone());
    }

    fn plugin_health(&self, plugin: &str, health: &str, reason: Option<&str>) {
        self.health.lock().unwrap().push((
            plugin.to_owned(),
            health.to_owned(),
            reason.map(str::to_owned),
        ));
    }

    fn alert(&self, _message: &str) {}
}

pub(crate) fn editor() -> Arc<MemoryEditor> {
    let editor = Arc::new(MemoryEditor::new());
    editor.set_document(BufferId(1), ViewId(1), "hello");
    editor
}

pub(crate) type TestPluginSpec = (String, bool, Arc<Mutex<Vec<String>>>);

pub(crate) fn host(
    named: Vec<TestPluginSpec>,
    editor: Arc<MemoryEditor>,
    notifier: Arc<RecordingNotifier>,
) -> (tempfile::TempDir, EditorPluginHost) {
    let dir = tempfile::tempdir().unwrap();
    let mut builtins = BuiltinCatalog::new();
    for (name, panic_on_event, order) in named {
        let manifest = builtin_manifest(&name, HostTarget::Editor, &[]);
        let plugin_name = name.clone();
        builtins
            .insert(BuiltinDescriptor::new(
                manifest,
                Arc::new(move || {
                    Box::new(TestPlugin {
                        name: plugin_name.clone(),
                        order: Arc::clone(&order),
                        panic_on_event,
                    })
                }),
            ))
            .unwrap();
    }
    let config = PluginManagerConfig::new(HostKind::Editor, dir.path().join("config"), dir.path())
        .with_host_config(json!({"theme": {"name": "dark"}}));
    let host = EditorPluginHost::new(
        editor,
        config,
        builtins,
        FsScope { read: true, write: true, allow: Vec::new() },
        notifier,
    )
    .unwrap();
    (dir, host)
}

pub(crate) fn single_host() -> (tempfile::TempDir, EditorPluginHost, Arc<RecordingNotifier>) {
    let notifier = Arc::new(RecordingNotifier::default());
    let order = Arc::new(Mutex::new(Vec::new()));
    let (dir, host) =
        host(vec![("fake".to_owned(), false, order)], editor(), Arc::clone(&notifier));
    (dir, host, notifier)
}

pub(crate) fn two_hosts() -> (tempfile::TempDir, EditorPluginHost, Arc<Mutex<Vec<String>>>) {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (dir, host) = host(
        vec![
            ("alpha".to_owned(), false, Arc::clone(&order)),
            ("beta".to_owned(), false, Arc::clone(&order)),
        ],
        editor(),
        Arc::new(RecordingNotifier::default()),
    );
    (dir, host, order)
}

pub(crate) fn event(view: ViewId) -> DocumentChanged {
    DocumentChanged {
        view_id: view,
        buffer_id: BufferId(1),
        rev: 1,
        new_len: 5,
        new_line_count: 1,
        undo_group: Some(1),
        author: "core".to_owned(),
        edit_type: "insert".to_owned(),
        delta: None,
    }
}

#[allow(dead_code)]
fn service_id<T: Service>() -> &'static str {
    T::ID
}
