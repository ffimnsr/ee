//! Manifest contribution conversion and revision-safe upsert tracking.

use ee_plugin_api::context::ContributionSink;
use ee_plugin_api::contribution::{
    AgentToolContribution, CommandContribution, ConfigSchemaContribution,
    ContextProviderContribution, Contribution, ExCommandContribution, FormatterContribution,
    KeybindingContribution, LanguageContribution, LspServerContribution, MenuItemContribution,
    ModelProviderContribution, OverlayContribution, PanelContribution, PickerContribution,
    PolicyRuleContribution, StatusItemContribution,
};
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ContributionId;
use ee_plugin_api::manifest::ManifestV2;

/// Kind discriminator used so a command and its keybinding may share an id.
pub fn contribution_kind(contribution: &Contribution) -> &'static str {
    match contribution {
        Contribution::Command(_) => "command",
        Contribution::ExCommand(_) => "ex_command",
        Contribution::Keybinding(_) => "keybinding",
        Contribution::StatusItem(_) => "status_item",
        Contribution::DecorationProvider(_) => "decoration_provider",
        Contribution::Panel(_) => "panel",
        Contribution::Overlay(_) => "overlay",
        Contribution::Picker(_) => "picker",
        Contribution::MenuItem(_) => "menu_item",
        Contribution::Language(_) => "language",
        Contribution::Formatter(_) => "formatter",
        Contribution::LspServer(_) => "lsp_server",
        Contribution::AgentTool(_) => "agent_tool",
        Contribution::ModelProvider(_) => "model_provider",
        Contribution::PolicyRule(_) => "policy_rule",
        Contribution::ContextProvider(_) => "context_provider",
        Contribution::ConfigSchema(_) => "config_schema",
    }
}

/// Composite key used by the manager and the in-memory sink.
pub fn contribution_key(contribution: &Contribution) -> (ContributionId, &'static str) {
    (contribution.id(), contribution_kind(contribution))
}

/// Convert every declarative table in a manifest into wire contributions.
pub fn collect_manifest_contributions(manifest: &ManifestV2) -> Vec<Contribution> {
    let contributes = &manifest.contributes;
    let mut items = Vec::new();

    for command in &contributes.commands {
        items.push(Contribution::Command(CommandContribution {
            id: command.id.clone(),
            title: command.title.clone(),
            description: command.description.clone(),
            category: command.category.clone(),
            args_schema: command.args_schema.clone(),
        }));
    }
    for ex_command in &contributes.ex_commands {
        items.push(Contribution::ExCommand(ExCommandContribution {
            name: ex_command.name.clone(),
            aliases: ex_command.aliases.clone(),
            command: ex_command.command.clone(),
            usage: ex_command.usage.clone(),
        }));
    }
    for keybinding in &contributes.keybindings {
        items.push(Contribution::Keybinding(KeybindingContribution {
            command: keybinding.command.clone(),
            key: keybinding.key.clone(),
            when: keybinding.when.clone(),
        }));
    }
    for menu_item in &contributes.menu_items {
        items.push(Contribution::MenuItem(MenuItemContribution {
            command: menu_item.command.clone(),
            label: menu_item.label.clone(),
            group: menu_item.group.clone(),
            order: menu_item.order,
        }));
    }
    for status_item in &contributes.ui.status_items {
        items.push(Contribution::StatusItem(StatusItemContribution {
            key: status_item.key.clone(),
            text: status_item.text.clone(),
            alignment: status_item.alignment,
            priority: status_item.priority,
        }));
    }
    for panel in &contributes.ui.panels {
        items.push(Contribution::Panel(PanelContribution {
            id: panel.id.clone(),
            title: panel.title.clone(),
            placement: panel.placement,
            render: panel.render.clone(),
            data_provider: panel.data_provider,
        }));
    }
    for overlay in &contributes.ui.overlays {
        items.push(Contribution::Overlay(OverlayContribution {
            id: overlay.id.clone(),
            anchor: overlay.anchor,
            render: overlay.render.clone(),
            transient: overlay.transient,
        }));
    }
    for picker in &contributes.ui.pickers {
        items.push(Contribution::Picker(PickerContribution {
            id: picker.id.clone(),
            title: picker.title.clone(),
            columns: picker.columns.clone(),
            filter_mode: picker.filter_mode,
            data_provider: picker.data_provider,
        }));
    }
    for language in &contributes.languages {
        items.push(Contribution::Language(LanguageContribution {
            name: language.name.clone(),
            extensions: language.extensions.clone(),
            filenames: language.filenames.clone(),
            globs: language.globs.clone(),
            first_line_match: language.first_line_match.clone(),
            scope: language.scope.clone(),
            default_config: language.default_config.clone(),
        }));
    }
    for formatter in &contributes.formatters {
        items.push(Contribution::Formatter(FormatterContribution {
            id: formatter.id.clone(),
            command: formatter.command.clone(),
            args: formatter.args.clone(),
            extensions: formatter.extensions.clone(),
        }));
    }
    for server in &contributes.lsp_servers {
        items.push(Contribution::LspServer(LspServerContribution {
            id: server.id.clone(),
            language: server.language.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            env: server.env.clone(),
            initialization_options: server.initialization_options.clone(),
        }));
    }
    for tool in &contributes.agent.tools {
        items.push(Contribution::AgentTool(AgentToolContribution {
            id: tool.id.clone(),
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters_schema: tool.parameters_schema.clone(),
            scope: tool.scope,
        }));
    }
    for provider in &contributes.agent.model_providers {
        items.push(Contribution::ModelProvider(ModelProviderContribution {
            id: provider.id.clone(),
            name: provider.name.clone(),
            capabilities: provider.capabilities.clone(),
        }));
    }
    for rule in &contributes.agent.policy_rules {
        items.push(Contribution::PolicyRule(PolicyRuleContribution {
            id: rule.id.clone(),
            gate: rule.gate,
            precedence: rule.precedence,
        }));
    }
    for provider in &contributes.agent.context_providers {
        items.push(Contribution::ContextProvider(ContextProviderContribution {
            id: provider.id.clone(),
            trust: provider.trust,
            max_bytes: provider.max_bytes,
        }));
    }
    for schema in &contributes.config_schema {
        items.push(Contribution::ConfigSchema(ConfigSchemaContribution {
            section: schema.section.clone(),
            schema: schema.schema.clone(),
            defaults: schema.defaults.clone(),
        }));
    }
    items
}

struct SessionEntry {
    id: ContributionId,
    kind: &'static str,
    disposer: Disposer,
}

/// Tracks contributions published by an external plugin.
///
/// Upserts replace the previous value for the same `(id, kind)` pair. Removing
/// an id disposes every kind registered under it.
#[derive(Default)]
pub struct ContributionSession {
    entries: Vec<SessionEntry>,
}

impl ContributionSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn upsert(
        &mut self,
        sink: &dyn ContributionSink,
        contribution: Contribution,
    ) -> Result<(), PluginError> {
        contribution.validate()?;
        let id = contribution.id();
        let kind = contribution_kind(&contribution);
        if let Some(index) =
            self.entries.iter().position(|entry| entry.id == id && entry.kind == kind)
        {
            let old = self.entries.remove(index);
            old.disposer.dispose();
        }
        let disposer = sink.contribute(contribution)?;
        self.entries.push(SessionEntry { id, kind, disposer });
        Ok(())
    }

    pub fn remove(&mut self, id: &ContributionId) {
        let mut index = 0;
        while index < self.entries.len() {
            if &self.entries[index].id == id {
                let entry = self.entries.remove(index);
                entry.disposer.dispose();
            } else {
                index += 1;
            }
        }
    }

    pub fn dispose_all(&mut self) {
        while let Some(entry) = self.entries.pop() {
            entry.disposer.dispose();
        }
    }

    pub fn ids(&self) -> Vec<ContributionId> {
        let mut ids: Vec<_> = self.entries.iter().map(|entry| entry.id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

impl Drop for ContributionSession {
    fn drop(&mut self) {
        self.dispose_all();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use ee_plugin_api::contribution::StatusAlignment;

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        items: std::sync::Arc<Mutex<BTreeMap<(ContributionId, &'static str), Contribution>>>,
    }

    impl ContributionSink for RecordingSink {
        fn contribute(&self, contribution: Contribution) -> Result<Disposer, PluginError> {
            let key = contribution_key(&contribution);
            let mut items = self.items.lock().unwrap();
            if items.insert(key.clone(), contribution).is_some() {
                return Err(PluginError::Contribution(
                    ee_plugin_api::contribution::ContributionError::EmptyField { field: "id" },
                ));
            }
            drop(items);
            let items = std::sync::Arc::clone(&self.items);
            Ok(Disposer::new(move || {
                if let Ok(mut items) = items.lock() {
                    items.remove(&key);
                }
            }))
        }
    }

    fn manifest() -> ManifestV2 {
        let mut manifest = crate::builtin::builtin_manifest(
            "demo",
            ee_plugin_api::manifest::HostTarget::Editor,
            &[],
        );
        manifest.contributes.ui.status_items.push(
            ee_plugin_api::contribution::StatusItemContribution {
                key: "demo.status".to_owned(),
                text: "ok".to_owned(),
                alignment: StatusAlignment::Left,
                priority: 0,
            },
        );
        manifest.contributes.commands.push(CommandContribution {
            id: ee_plugin_api::ids::CommandId::new("demo.run").unwrap(),
            title: "Run".to_owned(),
            description: String::new(),
            category: None,
            args_schema: None,
        });
        manifest.contributes.keybindings.push(KeybindingContribution {
            command: ee_plugin_api::ids::CommandId::new("demo.run").unwrap(),
            key: "ctrl+r".to_owned(),
            when: None,
        });
        manifest
    }

    #[test]
    fn manifest_tables_convert_in_stable_order() {
        let contributions = collect_manifest_contributions(&manifest());
        assert_eq!(contributions.len(), 3);
        assert!(matches!(contributions[0], Contribution::Command(_)));
        assert!(matches!(contributions[1], Contribution::Keybinding(_)));
        assert!(matches!(contributions[2], Contribution::StatusItem(_)));
    }

    #[test]
    fn command_and_keybinding_share_id_but_not_kind() {
        let contributions = collect_manifest_contributions(&manifest());
        assert_eq!(contributions[0].id(), contributions[1].id());
        assert_ne!(contribution_kind(&contributions[0]), contribution_kind(&contributions[1]));
    }

    #[test]
    fn session_replaces_and_removes_contributions() {
        let sink = RecordingSink::default();
        let mut session = ContributionSession::new();
        let first = Contribution::StatusItem(StatusItemContribution {
            key: "demo.status".to_owned(),
            text: "one".to_owned(),
            alignment: StatusAlignment::Left,
            priority: 0,
        });
        session.upsert(&sink, first).unwrap();
        assert_eq!(session.len(), 1);

        let second = Contribution::StatusItem(StatusItemContribution {
            key: "demo.status".to_owned(),
            text: "two".to_owned(),
            alignment: StatusAlignment::Left,
            priority: 0,
        });
        session.upsert(&sink, second).unwrap();
        assert_eq!(session.len(), 1);

        session.remove(&session.ids()[0].clone());
        assert!(session.is_empty());
    }
}
