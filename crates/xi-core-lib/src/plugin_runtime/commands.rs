//! Host command registry.
//!
//! Builtin commands and plugin commands use the same registration path: there
//! is no privileged branch for core commands. Dispatch validates argument
//! schemas, enforces the VLF mutation gate, and bounds handler runtime.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_plugin_api::contribution::ContributionError;
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::effect::{CommandHandler, CommandRequest, CommandSpec, Effect};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::CommandId;
use ee_plugin_api::model::ViewId;
use serde_json::Value;

use crate::plugin_runtime::core_api::EditorAccess;

/// Default per-command deadline.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

struct RegisteredCommand {
    spec: CommandSpec,
    handler: Arc<dyn CommandHandler>,
}

/// Command registry shared by the core, builtin commands, and plugins.
#[derive(Clone)]
pub struct EditorCommandRegistry {
    access: Arc<dyn EditorAccess>,
    commands: Arc<Mutex<BTreeMap<CommandId, RegisteredCommand>>>,
    aliases: Arc<Mutex<BTreeMap<String, CommandId>>>,
    next_request: Arc<AtomicU64>,
    cancellations: Arc<Mutex<BTreeMap<u64, Arc<AtomicBool>>>>,
}

impl EditorCommandRegistry {
    pub fn new(access: Arc<dyn EditorAccess>) -> Self {
        Self {
            access,
            commands: Arc::new(Mutex::new(BTreeMap::new())),
            aliases: Arc::new(Mutex::new(BTreeMap::new())),
            next_request: Arc::new(AtomicU64::new(1)),
            cancellations: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Register a host-owned command through the same path plugins use.
    pub fn register_builtin(
        &self,
        spec: CommandSpec,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<(), PluginError> {
        self.register(spec, handler)
    }

    /// Register an ex-command alias. Duplicate aliases fail closed.
    pub fn register_alias(&self, alias: &str, command: CommandId) -> Result<(), PluginError> {
        if alias.is_empty() {
            return Err(PluginError::Contribution(ContributionError::EmptyField {
                field: "alias",
            }));
        }
        let mut aliases = self.aliases.lock().expect("alias lock poisoned");
        if aliases.contains_key(alias) {
            return Err(PluginError::Contribution(ContributionError::EmptyField {
                field: "alias",
            }));
        }
        aliases.insert(alias.to_owned(), command);
        Ok(())
    }

    pub fn resolve_alias(&self, alias: &str) -> Option<CommandId> {
        self.aliases.lock().expect("alias lock poisoned").get(alias).cloned()
    }

    pub fn aliases(&self) -> Vec<(String, CommandId)> {
        self.aliases
            .lock()
            .expect("alias lock poisoned")
            .iter()
            .map(|(alias, id)| (alias.clone(), id.clone()))
            .collect()
    }

    fn register(
        &self,
        spec: CommandSpec,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<(), PluginError> {
        if spec.title.trim().is_empty() {
            return Err(PluginError::Contribution(ContributionError::EmptyField {
                field: "title",
            }));
        }
        let mut commands = self.commands.lock().expect("command lock poisoned");
        if commands.contains_key(&spec.id) {
            return Err(PluginError::Runtime {
                message: format!("command `{}` is already registered", spec.id),
            });
        }
        commands.insert(spec.id.clone(), RegisteredCommand { spec, handler });
        Ok(())
    }

    pub fn contains(&self, id: &CommandId) -> bool {
        self.commands.lock().expect("command lock poisoned").contains_key(id)
    }

    pub fn ids(&self) -> Vec<CommandId> {
        self.commands.lock().expect("command lock poisoned").keys().cloned().collect()
    }

    pub fn spec(&self, id: &CommandId) -> Option<CommandSpec> {
        self.commands.lock().expect("command lock poisoned").get(id).map(|entry| entry.spec.clone())
    }

    /// Flag a pending dispatch for cancellation.
    pub fn cancel(&self, request_id: u64) -> bool {
        let cancellations = self.cancellations.lock().expect("cancel lock poisoned");
        if let Some(flag) = cancellations.get(&request_id) {
            flag.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Invoke a command by id with the default deadline.
    pub fn dispatch(
        &self,
        id: &CommandId,
        view_id: ViewId,
        args: Value,
    ) -> Result<Vec<Effect>, PluginError> {
        self.dispatch_with_timeout(id, view_id, args, DEFAULT_COMMAND_TIMEOUT)
    }

    pub fn dispatch_with_timeout(
        &self,
        id: &CommandId,
        view_id: ViewId,
        args: Value,
        timeout: Duration,
    ) -> Result<Vec<Effect>, PluginError> {
        self.dispatch_cancellable(id, view_id, args, timeout, Arc::new(AtomicBool::new(false)))
    }

    /// Dispatch with an external cancellation flag.
    pub fn dispatch_cancellable(
        &self,
        id: &CommandId,
        view_id: ViewId,
        args: Value,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Effect>, PluginError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(PluginError::Cancelled);
        }
        let (spec, handler) = {
            let commands = self.commands.lock().expect("command lock poisoned");
            let entry = commands.get(id).ok_or_else(|| PluginError::Runtime {
                message: format!("command `{id}` is not registered"),
            })?;
            (entry.spec.clone(), Arc::clone(&entry.handler))
        };

        if spec.mutates_document && self.access.is_vlf(view_id) {
            return Err(PluginError::Permission {
                permission: format!("command `{id}` is disabled in VLF"),
            });
        }

        if let Some(schema) = &spec.args_schema {
            let validator = jsonschema::validator_for(schema).map_err(|error| {
                PluginError::Schema { message: format!("command `{id}` args schema: {error}") }
            })?;
            if let Err(error) = validator.validate(&args) {
                return Err(PluginError::Schema {
                    message: format!("command `{id}` args rejected: {error}"),
                });
            }
        }

        let request_id = self.next_request.fetch_add(1, Ordering::SeqCst);
        let per_request_cancel = Arc::new(AtomicBool::new(false));
        self.cancellations
            .lock()
            .expect("cancel lock poisoned")
            .insert(request_id, Arc::clone(&per_request_cancel));

        let request = CommandRequest { id: id.clone(), view_id, request_id, args };
        let started = Instant::now();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name(format!("ee-plugin-command-{id}"))
            .spawn(move || {
                let _ = sender.send(handler.invoke(request));
            })
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start command worker: {error}"),
            })?;

        let result = match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                cancel.store(true, Ordering::SeqCst);
                per_request_cancel.store(true, Ordering::SeqCst);
                Err(PluginError::Runtime {
                    message: format!(
                        "command `{id}` timed out after {} ms",
                        started.elapsed().as_millis()
                    ),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(PluginError::Runtime { message: format!("command `{id}` worker exited") })
            }
        };

        self.cancellations.lock().expect("cancel lock poisoned").remove(&request_id);
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            // Do not block the core on a timed-out handler. Cancellation-aware
            // handlers observe the flag and exit; others finish their bounded
            // work on the worker thread.
            drop(worker);
        }

        result
    }
}

impl ee_plugin_api::context::CommandRegistry for EditorCommandRegistry {
    fn register_command(
        &self,
        spec: CommandSpec,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<Disposer, PluginError> {
        self.register(spec.clone(), handler)?;
        let id = spec.id;
        let commands = Arc::clone(&self.commands);
        Ok(Disposer::new(move || {
            if let Ok(mut commands) = commands.lock() {
                commands.remove(&id);
            }
        }))
    }
}

impl ee_plugin_api::service::Service for EditorCommandRegistry {
    const ID: &'static str = "editor.commands@1";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::test_support::MemoryEditor;

    struct Noop;

    impl CommandHandler for Noop {
        fn invoke(&self, _request: CommandRequest) -> Result<Vec<Effect>, PluginError> {
            Ok(vec![Effect::Noop])
        }
    }

    struct Sleepy;

    impl CommandHandler for Sleepy {
        fn invoke(&self, _request: CommandRequest) -> Result<Vec<Effect>, PluginError> {
            std::thread::sleep(Duration::from_millis(50));
            Ok(vec![Effect::Noop])
        }
    }

    fn spec(id: &str, mutates_document: bool, schema: Option<Value>) -> CommandSpec {
        CommandSpec {
            id: CommandId::new(id).unwrap(),
            title: id.to_owned(),
            description: String::new(),
            category: None,
            mutates_document,
            args_schema: schema,
        }
    }

    fn registry() -> (EditorCommandRegistry, Arc<MemoryEditor>) {
        let editor = Arc::new(MemoryEditor::new());
        editor.set_document(
            ee_plugin_api::model::BufferId(1),
            ee_plugin_api::model::ViewId(1),
            "hello",
        );
        (EditorCommandRegistry::new(editor.clone()), editor)
    }

    #[test]
    fn duplicate_and_missing_ids_are_reported() {
        let (registry, _editor) = registry();
        registry.register_builtin(spec("demo.run", false, None), Arc::new(Noop)).unwrap();
        let error =
            registry.register_builtin(spec("demo.run", false, None), Arc::new(Noop)).unwrap_err();
        assert!(error.to_string().contains("already"));
        let error = registry
            .dispatch(&CommandId::new("demo.missing").unwrap(), ViewId(1), Value::Null)
            .unwrap_err();
        assert!(error.to_string().contains("not registered"));
    }

    #[test]
    fn args_schema_is_validated() {
        let (registry, _editor) = registry();
        let schema = serde_json::json!({
            "type": "object",
            "required": ["count"],
            "properties": {"count": {"type": "integer"}}
        });
        registry.register_builtin(spec("demo.count", false, Some(schema)), Arc::new(Noop)).unwrap();
        let error = registry
            .dispatch(&CommandId::new("demo.count").unwrap(), ViewId(1), serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Schema);
    }

    #[test]
    fn timeout_is_reported() {
        let (registry, _editor) = registry();
        registry.register_builtin(spec("demo.slow", false, None), Arc::new(Sleepy)).unwrap();
        let error = registry
            .dispatch_with_timeout(
                &CommandId::new("demo.slow").unwrap(),
                ViewId(1),
                Value::Null,
                Duration::from_millis(5),
            )
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn vlf_mutation_is_rejected_before_dispatch() {
        let (registry, editor) = registry();
        editor.set_vlf(ViewId(1), true);
        registry.register_builtin(spec("demo.edit", true, None), Arc::new(Noop)).unwrap();
        let error = registry
            .dispatch(&CommandId::new("demo.edit").unwrap(), ViewId(1), Value::Null)
            .unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Permission);
    }

    #[test]
    fn cancellation_before_dispatch_is_reported() {
        let (registry, _editor) = registry();
        registry.register_builtin(spec("demo.run", false, None), Arc::new(Noop)).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let error = registry
            .dispatch_cancellable(
                &CommandId::new("demo.run").unwrap(),
                ViewId(1),
                Value::Null,
                Duration::from_millis(10),
                cancel,
            )
            .unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Runtime);
    }

    #[test]
    fn aliases_are_unique() {
        let (registry, _editor) = registry();
        let id = CommandId::new("demo.run").unwrap();
        registry.register_alias("run", id.clone()).unwrap();
        assert_eq!(registry.resolve_alias("run"), Some(id));
        assert!(registry.register_alias("run", CommandId::new("x").unwrap()).is_err());
    }
}
