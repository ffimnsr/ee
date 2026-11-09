//! `agent.validation@1`: contributed validation planners and command metadata.
//!
//! Execution never happens here. Contributed declarations are merged into the
//! existing [`WorkspaceValidationConfig`] and planned through
//! [`ValidationPlanner`]; the shared `ValidationRunner` still executes them with
//! cache, retries, write-transaction, and evidence checks.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::validation::{
    DeclaredValidationCommand, DeclaredValidationTask, ValidationPlan, ValidationPlanner,
    ValidationPlanningContext, WorkspaceValidationConfig,
};

/// Maximum contributed validation commands.
pub const MAX_VALIDATION_COMMANDS: usize = 32;

/// A validation command declared by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationContribution {
    /// Stable command id; namespaced by the owning plugin at registration.
    pub id: String,
    /// Human-readable command or task name.
    pub command: String,
    /// Registered tool that runs the check.
    pub tool_name: String,
    /// Changed extensions selecting this command; empty selects every file.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Schema-valid tool arguments.
    #[serde(default = "default_arguments")]
    pub arguments: Value,
}

fn default_arguments() -> Value {
    serde_json::json!({})
}

struct ContributionEntry {
    plugin: PluginId,
    contribution: ValidationContribution,
}

/// Validation registry service.
pub struct ValidationService {
    contributions: Arc<RwLock<BTreeMap<String, ContributionEntry>>>,
    planner: ValidationPlanner,
}

impl Service for ValidationService {
    const ID: &'static str = "agent.validation@1";
}

impl Default for ValidationService {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            contributions: Arc::new(RwLock::new(BTreeMap::new())),
            planner: ValidationPlanner::new(),
        }
    }

    /// Registers one validation command contribution.
    pub fn register_contribution(
        &self,
        plugin: &PluginId,
        contribution: ValidationContribution,
    ) -> Result<Disposer, PluginError> {
        if contribution.id.is_empty() {
            return Err(PluginError::Runtime {
                message: "validation contribution id must not be empty".to_owned(),
            });
        }
        let key = format!("plugin:{plugin}:{}", contribution.id);
        let mut contributions = self.contributions.write().expect("validation lock poisoned");
        if contributions.len() >= MAX_VALIDATION_COMMANDS {
            return Err(PluginError::Resource {
                message: format!(
                    "validation contribution limit of {MAX_VALIDATION_COMMANDS} reached"
                ),
            });
        }
        if contributions.contains_key(&key) {
            return Err(PluginError::Runtime {
                message: format!("duplicate validation contribution `{key}`"),
            });
        }
        contributions
            .insert(key.clone(), ContributionEntry { plugin: plugin.clone(), contribution });
        drop(contributions);

        let handle = Arc::clone(&self.contributions);
        let plugin = plugin.clone();
        Ok(Disposer::new(move || {
            if let Ok(mut contributions) = handle.write()
                && contributions.get(&key).is_some_and(|entry| entry.plugin == plugin)
            {
                contributions.remove(&key);
            }
        }))
    }

    /// Plans validation with host rules plus contributed declarations.
    #[must_use]
    pub fn plan(
        &self,
        context: ValidationPlanningContext<'_>,
        tools: &[crate::tools::ToolDefinition],
    ) -> ValidationPlan {
        let mut workspace = context.workspace.clone();
        let contributions = self.contributions.read().expect("validation lock poisoned");
        for (key, entry) in contributions.iter() {
            workspace.declared_commands.push(DeclaredValidationCommand::new(
                DeclaredValidationTask::new(
                    entry.contribution.tool_name.clone(),
                    entry.contribution.command.clone(),
                    entry.contribution.arguments.clone(),
                )
                .for_extensions(entry.contribution.extensions.iter().cloned()),
                key.clone(),
            ));
        }
        drop(contributions);
        self.planner.plan_with_context(
            ValidationPlanningContext {
                changed_files: context.changed_files,
                changed_symbols: context.changed_symbols,
                workspace: &workspace,
            },
            tools,
        )
    }

    /// Merges contributions into an existing workspace configuration.
    #[must_use]
    pub fn workspace_config(&self, base: &WorkspaceValidationConfig) -> WorkspaceValidationConfig {
        let mut workspace = base.clone();
        let contributions = self.contributions.read().expect("validation lock poisoned");
        for (key, entry) in contributions.iter() {
            workspace.declared_commands.push(DeclaredValidationCommand::new(
                DeclaredValidationTask::new(
                    entry.contribution.tool_name.clone(),
                    entry.contribution.command.clone(),
                    entry.contribution.arguments.clone(),
                )
                .for_extensions(entry.contribution.extensions.iter().cloned()),
                key.clone(),
            ));
        }
        workspace
    }

    /// Sorted namespaced contribution ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.contributions.read().expect("validation lock poisoned").keys().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.contributions.read().expect("validation lock poisoned").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for ValidationService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidationService")
            .field("contributions", &self.ids())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use crate::final_response::ChangedFile;
    use crate::tools::{SideEffectClass, ToolDefinition};

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    fn contribution() -> ValidationContribution {
        ValidationContribution {
            id: "lint".to_owned(),
            command: "cargo clippy".to_owned(),
            tool_name: "cargo_check".to_owned(),
            extensions: vec![".rs".to_owned()],
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        }
    }

    fn tool() -> ToolDefinition {
        ToolDefinition::new("cargo_check", "check").side_effect_class(SideEffectClass::Execute)
    }

    #[test]
    fn contributed_command_routes_through_the_planner() {
        let service = ValidationService::new();
        service.register_contribution(&plugin(), contribution()).expect("registers");
        let changed = [ChangedFile { path: "src/lib.rs".to_owned(), source_task: None }];
        let workspace = WorkspaceValidationConfig::default();
        let plan = service.plan(
            ValidationPlanningContext {
                changed_files: &changed,
                changed_symbols: &[],
                workspace: &workspace,
            },
            &[tool()],
        );
        assert!(plan.entries.iter().any(|entry| entry.tool_name == "cargo_check"));
    }

    #[test]
    fn contribution_provenance_is_namespaced() {
        let service = ValidationService::new();
        service.register_contribution(&plugin(), contribution()).expect("registers");
        let changed = [ChangedFile { path: "src/lib.rs".to_owned(), source_task: None }];
        let workspace = WorkspaceValidationConfig::default();
        let plan = service.plan(
            ValidationPlanningContext {
                changed_files: &changed,
                changed_symbols: &[],
                workspace: &workspace,
            },
            &[tool()],
        );
        assert!(plan.entries.iter().any(|entry| entry.metadata.command_id == "plugin:demo:lint"));
    }

    #[test]
    fn unknown_tool_is_not_planned() {
        let service = ValidationService::new();
        service.register_contribution(&plugin(), contribution()).expect("registers");
        let changed = [ChangedFile { path: "src/lib.rs".to_owned(), source_task: None }];
        let workspace = WorkspaceValidationConfig::default();
        let plan = service.plan(
            ValidationPlanningContext {
                changed_files: &changed,
                changed_symbols: &[],
                workspace: &workspace,
            },
            &[],
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn removal_restores_host_plan() {
        let service = ValidationService::new();
        let disposer = service.register_contribution(&plugin(), contribution()).expect("registers");
        assert_eq!(service.len(), 1);
        disposer.dispose();
        assert!(service.is_empty());
    }
}
