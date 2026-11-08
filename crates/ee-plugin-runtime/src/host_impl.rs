//! In-memory host registries used by tests and by hosts that do not yet inject
//! their own command or contribution registry.
//!
//! Registration maps are `Arc`-backed so a returned [`Disposer`] can remove its
//! entry without borrowing the registry or using `unsafe`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ee_plugin_api::context::{CommandRegistry, ContributionSink};
use ee_plugin_api::contribution::Contribution;
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::effect::{CommandHandler, CommandRequest, CommandSpec, Effect};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::{CommandId, ContributionId};

use crate::contributions::contribution_key;

type CommandMap = BTreeMap<CommandId, Arc<dyn CommandHandler>>;
type ContributionMap = BTreeMap<(ContributionId, &'static str), Contribution>;

/// Command registry that rejects duplicate ids and removes handlers on dispose.
#[derive(Clone, Default)]
pub struct MemoryCommandRegistry {
    commands: Arc<Mutex<CommandMap>>,
}

impl MemoryCommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: &CommandId) -> bool {
        self.commands.lock().expect("command registry lock poisoned").contains_key(id)
    }

    pub fn ids(&self) -> Vec<CommandId> {
        self.commands.lock().expect("command registry lock poisoned").keys().cloned().collect()
    }

    /// Invoke a registered handler; used by tests and simple hosts.
    pub fn invoke(
        &self,
        id: &CommandId,
        request: CommandRequest,
    ) -> Result<Vec<Effect>, PluginError> {
        let handler = self
            .commands
            .lock()
            .expect("command registry lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| PluginError::Runtime {
                message: format!("command `{id}` is not registered"),
            })?;
        handler.invoke(request)
    }
}

impl CommandRegistry for MemoryCommandRegistry {
    fn register_command(
        &self,
        spec: CommandSpec,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<Disposer, PluginError> {
        let id = spec.id;
        let mut commands = self.commands.lock().expect("command registry lock poisoned");
        if commands.contains_key(&id) {
            return Err(PluginError::Contribution(
                ee_plugin_api::contribution::ContributionError::EmptyField { field: "command.id" },
            ));
        }
        commands.insert(id.clone(), handler);
        drop(commands);

        let handle = Arc::clone(&self.commands);
        Ok(Disposer::new(move || {
            if let Ok(mut commands) = handle.lock() {
                commands.remove(&id);
            }
        }))
    }
}

/// Contribution sink that stores one entry per `(id, kind)` pair.
#[derive(Clone, Default)]
pub struct MemoryContributionSink {
    entries: Arc<Mutex<ContributionMap>>,
}

impl MemoryContributionSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: &ContributionId) -> bool {
        self.entries
            .lock()
            .expect("contribution sink lock poisoned")
            .keys()
            .any(|(entry_id, _)| entry_id == id)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("contribution sink lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn list(&self) -> Vec<Contribution> {
        self.entries.lock().expect("contribution sink lock poisoned").values().cloned().collect()
    }

    pub fn get(&self, id: &ContributionId, kind: &str) -> Option<Contribution> {
        self.entries
            .lock()
            .expect("contribution sink lock poisoned")
            .iter()
            .find(|((entry_id, entry_kind), _)| entry_id == id && *entry_kind == kind)
            .map(|(_, contribution)| contribution.clone())
    }
}

impl ContributionSink for MemoryContributionSink {
    fn contribute(&self, contribution: Contribution) -> Result<Disposer, PluginError> {
        contribution.validate()?;
        let key = contribution_key(&contribution);
        let mut entries = self.entries.lock().expect("contribution sink lock poisoned");
        if entries.contains_key(&key) {
            return Err(PluginError::Contribution(
                ee_plugin_api::contribution::ContributionError::EmptyField { field: "id" },
            ));
        }
        entries.insert(key.clone(), contribution);
        drop(entries);

        let handle = Arc::clone(&self.entries);
        Ok(Disposer::new(move || {
            if let Ok(mut entries) = handle.lock() {
                entries.remove(&key);
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Handler;

    impl CommandHandler for Handler {
        fn invoke(&self, _request: CommandRequest) -> Result<Vec<Effect>, PluginError> {
            Ok(vec![Effect::Noop])
        }
    }

    fn spec(name: &str) -> CommandSpec {
        CommandSpec {
            id: CommandId::new(name).unwrap(),
            title: name.to_owned(),
            description: String::new(),
            category: None,
            mutates_document: false,
            args_schema: None,
        }
    }

    #[test]
    fn command_disposer_unregisters() {
        let registry = MemoryCommandRegistry::new();
        let id = CommandId::new("demo.run").unwrap();
        let disposer = registry.register_command(spec("demo.run"), Arc::new(Handler)).unwrap();
        assert!(registry.contains(&id));
        disposer.dispose();
        assert!(!registry.contains(&id));
    }

    #[test]
    fn contribution_kind_prevents_command_keybinding_collision() {
        let sink = MemoryContributionSink::new();
        let command = Contribution::Command(ee_plugin_api::contribution::CommandContribution {
            id: CommandId::new("demo.run").unwrap(),
            title: "Run".to_owned(),
            description: String::new(),
            category: None,
            args_schema: None,
        });
        let keybinding =
            Contribution::Keybinding(ee_plugin_api::contribution::KeybindingContribution {
                command: CommandId::new("demo.run").unwrap(),
                key: "ctrl+r".to_owned(),
                when: None,
            });
        let first = sink.contribute(command).unwrap();
        let second = sink.contribute(keybinding).unwrap();
        assert_eq!(sink.len(), 2);
        second.dispose();
        assert_eq!(sink.len(), 1);
        first.dispose();
        assert!(sink.is_empty());
    }
}
