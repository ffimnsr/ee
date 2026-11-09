//! Frontend contribution registry.
//!
//! The registry is the frontend's only view of plugin UI state. It never holds
//! plugin code or plugin-owned handles. Batches are applied atomically: a
//! rejected batch leaves the previous revision and every table untouched.

use std::collections::BTreeMap;

use ee_plugin_api::contribution::{
    CommandContribution, Contribution, ExCommandContribution, KeybindingContribution,
    MenuItemContribution, OverlayContribution, PanelContribution, PickerContribution,
    StatusItemContribution,
};
use ee_plugin_api::ids::{CommandId, ContributionId};

use super::contributions::{ContributionBatch, PluginDiagnostic, contribution_kind};
use super::keybindings::{ParsedKeybinding, parse_keybinding};
use super::sanitize::escape_markdown;
use crate::keymap::{Action, BindingKey};

/// A batch application failure. Callers keep the previous registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    StaleRevision { current: u64, attempted: u64 },
    DuplicateExAlias { alias: String },
    KeybindingConflict { key: String, first: String, second: String },
    InvalidKeybinding { command: String, reason: String },
    InvalidContribution { kind: &'static str, id: String, reason: String },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleRevision { current, attempted } => {
                write!(f, "stale plugin revision {attempted} (current {current})")
            }
            Self::DuplicateExAlias { alias } => {
                write!(f, "duplicate ex command alias `{alias}`")
            }
            Self::KeybindingConflict { key, first, second } => {
                write!(f, "conflicting plugin keybinding `{key}` for `{first}` and `{second}`")
            }
            Self::InvalidKeybinding { command, reason } => {
                write!(f, "invalid keybinding for `{command}`: {reason}")
            }
            Self::InvalidContribution { kind, id, reason } => {
                write!(f, "invalid {kind} contribution `{id}`: {reason}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Outcome of one accepted batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub diagnostics: Vec<PluginDiagnostic>,
    pub removed: Vec<ContributionId>,
}

/// Every declarative contribution the terminal frontend renders or invokes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContributionRegistry {
    revision: u64,
    commands: BTreeMap<CommandId, CommandContribution>,
    ex_commands: BTreeMap<String, ExCommandContribution>,
    ex_aliases: BTreeMap<String, CommandId>,
    keybindings: BTreeMap<String, (KeybindingContribution, ParsedKeybinding)>,
    status_items: BTreeMap<String, StatusItemContribution>,
    panels: BTreeMap<ContributionId, PanelContribution>,
    overlays: BTreeMap<ContributionId, OverlayContribution>,
    pickers: BTreeMap<ContributionId, PickerContribution>,
    menu_items: BTreeMap<String, MenuItemContribution>,
    owners: BTreeMap<ContributionId, String>,
}

impl ContributionRegistry {
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn command(&self, id: &str) -> Option<&CommandContribution> {
        self.commands.get(&CommandId::new(id).ok()?)
    }

    #[must_use]
    pub fn has_command(&self, id: &str) -> bool {
        self.command(id).is_some()
    }

    pub fn commands(&self) -> impl Iterator<Item = &CommandContribution> {
        self.commands.values()
    }

    pub fn ex_commands(&self) -> impl Iterator<Item = &ExCommandContribution> {
        self.ex_commands.values()
    }

    /// Resolve an ex-command name or alias contributed by a plugin.
    #[must_use]
    pub fn resolve_ex_alias(&self, alias: &str) -> Option<&CommandId> {
        self.ex_aliases.get(alias)
    }

    pub fn status_items(&self) -> impl Iterator<Item = &StatusItemContribution> {
        self.status_items.values()
    }

    #[must_use]
    pub fn status_item(&self, key: &str) -> Option<&StatusItemContribution> {
        self.status_items.get(key)
    }

    pub fn panels(&self) -> impl Iterator<Item = &PanelContribution> {
        self.panels.values()
    }

    #[must_use]
    pub fn panel(&self, id: &ContributionId) -> Option<&PanelContribution> {
        self.panels.get(id)
    }

    pub fn overlays(&self) -> impl Iterator<Item = &OverlayContribution> {
        self.overlays.values()
    }

    #[must_use]
    pub fn overlay(&self, id: &ContributionId) -> Option<&OverlayContribution> {
        self.overlays.get(id)
    }

    pub fn pickers(&self) -> impl Iterator<Item = &PickerContribution> {
        self.pickers.values()
    }

    #[must_use]
    pub fn picker(&self, id: &ContributionId) -> Option<&PickerContribution> {
        self.pickers.get(id)
    }

    pub fn menu_items(&self) -> impl Iterator<Item = &MenuItemContribution> {
        self.menu_items.values()
    }

    /// Action bound by a plugin default for `binding`, if any.
    #[must_use]
    pub fn keybinding_action(&self, binding: &BindingKey) -> Option<Action> {
        self.keybindings.values().find(|(_contribution, parsed)| parsed.binding == *binding).map(
            |(contribution, _)| Action::PluginCommand(contribution.command.as_str().to_owned()),
        )
    }

    pub fn keybindings(&self) -> impl Iterator<Item = &KeybindingContribution> {
        self.keybindings.values().map(|(contribution, _)| contribution)
    }

    /// Owner derived from the first id segment for diagnostics and removal.
    #[must_use]
    pub fn owner_of(&self, id: &ContributionId) -> Option<&str> {
        self.owners.get(id).map(String::as_str)
    }

    /// Remove every contribution owned by `owner`.
    pub fn remove_owner(&mut self, owner: &str) -> Vec<ContributionId> {
        let ids = self
            .owners
            .iter()
            .filter(|(_, candidate)| candidate.as_str() == owner)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in &ids {
            self.remove_id(id);
        }
        ids
    }

    /// Atomically apply one contribution batch.
    pub fn apply(&mut self, batch: &ContributionBatch) -> Result<ApplyReport, RegistryError> {
        if batch.revision <= self.revision {
            return Err(RegistryError::StaleRevision {
                current: self.revision,
                attempted: batch.revision,
            });
        }

        let mut next = self.clone();
        let mut removed = Vec::new();
        for id in &batch.remove {
            if next.remove_id(id) {
                removed.push(id.clone());
            }
        }
        for contribution in &batch.upsert {
            next.upsert(contribution)?;
        }
        next.validate_keybindings()?;
        next.revision = batch.revision;

        let diagnostics = batch.diagnostics.clone();
        *self = next;
        Ok(ApplyReport { diagnostics, removed })
    }

    fn upsert(&mut self, contribution: &Contribution) -> Result<(), RegistryError> {
        let id = contribution.id();
        self.owners.insert(id.clone(), owner_for(&id));
        match contribution {
            Contribution::Command(command) => {
                self.commands.insert(command.id.clone(), command.clone());
            }
            Contribution::ExCommand(command) => {
                self.remove_ex_aliases(&command.name);
                let command_id = command.command.clone();
                let mut names = command.aliases.clone();
                names.push(command.name.clone());
                for name in names {
                    if let Some(existing) = self.ex_aliases.get(&name)
                        && existing != &command_id
                    {
                        return Err(RegistryError::DuplicateExAlias { alias: name });
                    }
                }
                self.ex_commands.insert(command.name.clone(), command.clone());
                self.ex_aliases.insert(command.name.clone(), command_id.clone());
                for alias in &command.aliases {
                    self.ex_aliases.insert(alias.clone(), command_id.clone());
                }
            }
            Contribution::Keybinding(binding) => {
                let parsed = parse_keybinding(binding).map_err(|reason| {
                    RegistryError::InvalidKeybinding {
                        command: binding.command.as_str().to_owned(),
                        reason,
                    }
                })?;
                let key = binding_key_id(binding, &parsed);
                self.keybindings.insert(key, (binding.clone(), parsed));
            }
            Contribution::StatusItem(item) => {
                self.status_items.insert(item.key.clone(), item.clone());
            }
            Contribution::Panel(panel) => {
                self.panels.insert(panel.id.clone(), panel.clone());
            }
            Contribution::Overlay(overlay) => {
                self.overlays.insert(overlay.id.clone(), overlay.clone());
            }
            Contribution::Picker(picker) => {
                self.pickers.insert(picker.id.clone(), picker.clone());
            }
            Contribution::MenuItem(item) => {
                let key = menu_item_key(item);
                self.menu_items.insert(key, item.clone());
            }
            _ => {
                return Err(RegistryError::InvalidContribution {
                    kind: contribution_kind(contribution),
                    id: id.into_string(),
                    reason: String::from("not a frontend-renderable contribution"),
                });
            }
        }
        Ok(())
    }

    fn remove_ex_aliases(&mut self, name: &str) {
        if let Some(command) = self.ex_aliases.remove(name) {
            for (alias, candidate) in std::mem::take(&mut self.ex_aliases) {
                if candidate == command {
                    self.ex_aliases.insert(alias, candidate);
                }
            }
        }
    }

    fn remove_id(&mut self, id: &ContributionId) -> bool {
        self.owners.remove(id);
        let mut removed = false;
        if let Ok(command) = CommandId::new(id.as_str().to_owned()) {
            if self.commands.remove(&command).is_some() {
                removed = true;
            }
            let before = self.keybindings.len();
            self.keybindings.retain(|_, (binding, _)| binding.command != command);
            removed |= self.keybindings.len() != before;
            let before = self.menu_items.len();
            self.menu_items.retain(|_, item| item.command != command);
            removed |= self.menu_items.len() != before;
        }
        if self.status_items.remove(id.as_str()).is_some() {
            removed = true;
        }
        if self.panels.remove(id).is_some() {
            removed = true;
        }
        if self.overlays.remove(id).is_some() {
            removed = true;
        }
        if self.pickers.remove(id).is_some() {
            removed = true;
        }
        if self.ex_commands.remove(id.as_str()).is_some() {
            removed = true;
        }
        if let Some(command) = self.ex_aliases.get(id.as_str()).cloned() {
            self.ex_aliases.retain(|_, candidate| candidate != &command);
            let _ = command;
            removed = true;
        }
        removed
    }

    fn validate_keybindings(&self) -> Result<(), RegistryError> {
        let mut seen: Vec<(BindingKey, CommandId)> = Vec::new();
        let mut entries = self.keybindings.values().collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| {
            left.command.as_str().cmp(right.command.as_str()).then_with(|| left.key.cmp(&right.key))
        });
        for (contribution, parsed) in entries {
            if let Some((_, existing)) = seen.iter().find(|(binding, _)| *binding == parsed.binding)
                && existing != &contribution.command
            {
                return Err(RegistryError::KeybindingConflict {
                    key: contribution.key.clone(),
                    first: existing.as_str().to_owned(),
                    second: contribution.command.as_str().to_owned(),
                });
            }
            seen.push((parsed.binding, contribution.command.clone()));
        }
        Ok(())
    }
}

fn owner_for(id: &ContributionId) -> String {
    let raw = id.as_str();
    raw.split('.').next().filter(|segment| !segment.is_empty()).unwrap_or(raw).to_owned()
}

fn binding_key_id(binding: &KeybindingContribution, parsed: &ParsedKeybinding) -> String {
    format!(
        "{}|{}|{}|{:?}|{:?}",
        binding.command.as_str(),
        parsed.binding.mode.label(),
        binding.key,
        parsed.binding.modifiers,
        parsed.binding.prefix
    )
}

fn menu_item_key(item: &MenuItemContribution) -> String {
    format!("{}|{}", item.command.as_str(), escape_markdown(&item.label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_plugin_api::contribution::{
        CommandContribution, StatusAlignment, StatusItemContribution,
    };

    fn command(id: &str) -> Contribution {
        Contribution::Command(CommandContribution {
            id: CommandId::new(id).unwrap(),
            title: String::from("Demo"),
            description: String::new(),
            category: None,
            args_schema: None,
        })
    }

    fn batch(revision: u64, upsert: Vec<Contribution>) -> ContributionBatch {
        ContributionBatch { revision, upsert, remove: Vec::new(), diagnostics: Vec::new() }
    }

    #[test]
    fn upsert_replaces_and_remove_drops_all_owned_entries() {
        let mut registry = ContributionRegistry::default();
        registry.apply(&batch(1, vec![command("demo.run")])).unwrap();
        assert!(registry.has_command("demo.run"));
        registry.apply(&batch(2, vec![command("demo.run")])).unwrap();
        assert_eq!(registry.commands().count(), 1);

        let mut remove = batch(3, Vec::new());
        remove.remove.push(ContributionId::new("demo.run").unwrap());
        registry.apply(&remove).unwrap();
        assert!(!registry.has_command("demo.run"));
    }

    #[test]
    fn duplicate_ids_inside_a_batch_are_rejected_before_mutation() {
        let registry = ContributionRegistry::default();
        let result = ContributionBatch::parse(&serde_json::json!({
            "revision": 1,
            "upsert": [
                {"kind": "command", "id": "demo.run", "title": "Run"},
                {"kind": "command", "id": "demo.run", "title": "Run again"}
            ]
        }));
        assert!(result.is_err());
        assert_eq!(registry.revision(), 0);
    }

    #[test]
    fn stale_revision_keeps_previous_registry() {
        let mut registry = ContributionRegistry::default();
        registry.apply(&batch(2, vec![command("demo.run")])).unwrap();
        let stale = batch(2, vec![command("demo.other")]);
        assert_eq!(
            registry.apply(&stale),
            Err(RegistryError::StaleRevision { current: 2, attempted: 2 })
        );
        assert!(registry.has_command("demo.run"));
        assert!(!registry.has_command("demo.other"));
    }

    #[test]
    fn duplicate_ex_alias_is_rejected() {
        let mut registry = ContributionRegistry::default();
        let ex = |name: &str, alias: &str, command: &str| {
            Contribution::ExCommand(ExCommandContribution {
                name: name.to_owned(),
                aliases: vec![alias.to_owned()],
                command: CommandId::new(command).unwrap(),
                usage: None,
            })
        };
        registry
            .apply(&batch(
                1,
                vec![ex("outline", "ol", "demo.run"), ex("other", "ol", "demo.other")],
            ))
            .unwrap_err();
        assert_eq!(registry.revision(), 0);
    }

    #[test]
    fn status_items_are_tracked_by_owner() {
        let mut registry = ContributionRegistry::default();
        registry
            .apply(&batch(
                1,
                vec![Contribution::StatusItem(StatusItemContribution {
                    key: String::from("demo.status"),
                    text: String::from("ok"),
                    alignment: StatusAlignment::Right,
                    priority: 1,
                })],
            ))
            .unwrap();
        assert_eq!(registry.owner_of(&ContributionId::new("demo.status").unwrap()), Some("demo"));
        assert_eq!(registry.remove_owner("demo").len(), 1);
        assert_eq!(registry.status_items().count(), 0);
    }
}
