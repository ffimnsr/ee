//! Editor contribution registry.
//!
//! The registry stores one entry per `(id, kind)` pair, so a command and its
//! keybinding may share an id. Every mutation advances the revision; a
//! disposer removes only its own entry.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_plugin_api::context::ContributionSink;
use ee_plugin_api::contribution::{
    Contribution, ContributionError, DecorationProviderContribution,
};
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ContributionId;

use ee_plugin_runtime::contributions::contribution_key;

/// Maximum contributions accepted from one editor host.
pub const MAX_CONTRIBUTIONS: usize = 4_096;

/// Registry of declarative contributions published by plugins.
#[derive(Clone, Default)]
pub struct EditorContributionRegistry {
    entries: Arc<Mutex<BTreeMap<(ContributionId, &'static str), Contribution>>>,
    revision: Arc<AtomicU64>,
}

impl EditorContributionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("contribution lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn list(&self) -> Vec<Contribution> {
        self.entries.lock().expect("contribution lock poisoned").values().cloned().collect()
    }

    pub fn get(&self, id: &ContributionId, kind: &str) -> Option<Contribution> {
        self.entries
            .lock()
            .expect("contribution lock poisoned")
            .iter()
            .find(|((entry_id, entry_kind), _)| entry_id == id && *entry_kind == kind)
            .map(|(_, contribution)| contribution.clone())
    }

    pub fn decorations(&self) -> Vec<DecorationProviderContribution> {
        self.entries
            .lock()
            .expect("contribution lock poisoned")
            .values()
            .filter_map(|contribution| match contribution {
                Contribution::DecorationProvider(provider) => Some(provider.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn by_kind(&self, kind: &str) -> Vec<Contribution> {
        self.entries
            .lock()
            .expect("contribution lock poisoned")
            .iter()
            .filter(|((_, entry_kind), _)| *entry_kind == kind)
            .map(|(_, contribution)| contribution.clone())
            .collect()
    }

    fn insert(
        &self,
        contribution: Contribution,
    ) -> Result<(ContributionId, &'static str), PluginError> {
        contribution.validate()?;
        let key = contribution_key(&contribution);
        let mut entries = self.entries.lock().expect("contribution lock poisoned");
        if entries.len() >= MAX_CONTRIBUTIONS {
            return Err(PluginError::Resource {
                message: format!("contribution limit of {MAX_CONTRIBUTIONS} reached"),
            });
        }
        if entries.contains_key(&key) {
            return Err(PluginError::Contribution(ContributionError::EmptyField { field: "id" }));
        }
        entries.insert(key.clone(), contribution);
        drop(entries);
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok(key)
    }
}

impl ContributionSink for EditorContributionRegistry {
    fn contribute(&self, contribution: Contribution) -> Result<Disposer, PluginError> {
        let key = self.insert(contribution)?;
        let entries = Arc::clone(&self.entries);
        let revision = Arc::clone(&self.revision);
        Ok(Disposer::new(move || {
            if let Ok(mut entries) = entries.lock() {
                entries.remove(&key);
            }
            revision.fetch_add(1, Ordering::SeqCst);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_plugin_api::contribution::{
        CommandContribution, KeybindingContribution, StatusAlignment, StatusItemContribution,
    };
    use ee_plugin_api::ids::CommandId;

    fn command(id: &str) -> Contribution {
        Contribution::Command(CommandContribution {
            id: CommandId::new(id).unwrap(),
            title: "Run".to_owned(),
            description: String::new(),
            category: None,
            args_schema: None,
        })
    }

    fn status(key: &str) -> Contribution {
        Contribution::StatusItem(StatusItemContribution {
            key: key.to_owned(),
            text: "ok".to_owned(),
            alignment: StatusAlignment::Left,
            priority: 0,
        })
    }

    #[test]
    fn upsert_and_remove_track_owner() {
        let registry = EditorContributionRegistry::new();
        let disposer = registry.contribute(status("demo.status")).unwrap();
        assert_eq!(registry.len(), 1);
        let revision = registry.revision();
        disposer.dispose();
        assert!(registry.is_empty());
        assert!(registry.revision() > revision);
    }

    #[test]
    fn duplicate_id_fails_but_kinds_may_share_an_id() {
        let registry = EditorContributionRegistry::new();
        let disposer = registry.contribute(command("demo.run")).unwrap();
        assert!(registry.contribute(command("demo.run")).is_err());
        let _ = registry
            .contribute(Contribution::Keybinding(KeybindingContribution {
                command: CommandId::new("demo.run").unwrap(),
                key: "ctrl+r".to_owned(),
                when: None,
            }))
            .unwrap();
        assert_eq!(registry.len(), 2);
        disposer.dispose();
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn invalid_contribution_is_rejected() {
        let registry = EditorContributionRegistry::new();
        let error = registry
            .contribute(Contribution::StatusItem(StatusItemContribution {
                key: String::new(),
                text: "ok".to_owned(),
                alignment: StatusAlignment::Left,
                priority: 0,
            }))
            .unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Contribution);
    }

    #[test]
    fn decorations_are_selected_by_kind() {
        let registry = EditorContributionRegistry::new();
        let _ = registry
            .contribute(Contribution::StatusItem(StatusItemContribution {
                key: "demo.status".to_owned(),
                text: "ok".to_owned(),
                alignment: StatusAlignment::Left,
                priority: 0,
            }))
            .unwrap();
        let _ = registry
            .contribute(Contribution::DecorationProvider(DecorationProviderContribution {
                id: ContributionId::new("demo.decorations").unwrap(),
                kinds: vec!["diagnostic".to_owned()],
                revision_policy: None,
            }))
            .unwrap();
        assert_eq!(registry.decorations().len(), 1);
        assert_eq!(registry.by_kind("status_item").len(), 1);
    }
}
