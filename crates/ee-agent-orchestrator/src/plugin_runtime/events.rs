//! Typed `agent.*` event catalog.
//!
//! Event names are part of the external plugin contract (manifest
//! `activation.on_event` and native `plugin/event` payloads). Renaming one is a
//! breaking change; see `docs/upgrades/plugin-system-cordis.md` §7.2.
//!
//! Payloads are bounded: identifiers, counters, classifications, and redacted
//! argument trees only. Prompts, transcripts, and tool output never travel
//! through these events.

use ee_plugin_api::event::Event;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model_router::TaskKind;
use crate::strategy::{StrategyReason, TurnStrategy};
use crate::tools::{SideEffectClass, ToolDefinition};

/// A turn is about to start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTurnBefore {
    pub session_id: String,
    pub task_id: String,
    /// Prompt length in bytes; prompt text never leaves the host.
    pub prompt_bytes: usize,
}

impl Event for AgentTurnBefore {
    const NAME: &'static str = "agent.turn.before";
}

/// A turn reached a terminal stop reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTurnAfter {
    pub session_id: String,
    pub task_id: String,
    pub stop_reason: String,
}

impl Event for AgentTurnAfter {
    const NAME: &'static str = "agent.turn.after";
}

/// A model call is about to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelBefore {
    pub session_id: String,
    pub model_id: Option<String>,
    pub task_kind: Option<TaskKind>,
    pub role: Option<String>,
    pub iteration: usize,
}

impl Event for AgentModelBefore {
    const NAME: &'static str = "agent.model.before";
}

/// A model call returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelAfter {
    pub session_id: String,
    pub model_id: Option<String>,
    pub task_kind: Option<TaskKind>,
    pub iteration: usize,
}

impl Event for AgentModelAfter {
    const NAME: &'static str = "agent.model.after";
}

/// A tool is about to run.
///
/// Mandatory policy (workspace trust, destructive safeguards, budget, write
/// transactions) is evaluated before plugin bail listeners see this event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolBefore {
    pub session_id: String,
    pub tool_name: String,
    pub side_effect_class: SideEffectClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect_subclass: Option<String>,
    pub subagent_depth: usize,
    pub active_delegates: usize,
    #[serde(default)]
    pub arguments: Value,
}

impl Event for AgentToolBefore {
    const NAME: &'static str = "agent.tool.before";
}

/// A tool finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToolAfter {
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub success: bool,
}

impl Event for AgentToolAfter {
    const NAME: &'static str = "agent.tool.after";
}

/// A host approval decision is requested.
///
/// Mandatory host approval and workspace trust are evaluated before plugin
/// listeners; plugins can deny but never bypass a mandatory denial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentApprovalBefore {
    pub session_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    pub host_approved: bool,
}

impl Event for AgentApprovalBefore {
    const NAME: &'static str = "agent.approval.before";
}

/// Turn context is being collected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContextCollect {
    pub session_id: String,
    pub turn: u64,
    pub max_items: usize,
    pub max_bytes: usize,
}

impl Event for AgentContextCollect {
    const NAME: &'static str = "agent.context.collect";
}

/// The host is selecting a turn strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStrategySelect {
    pub session_id: String,
    pub base_strategy: TurnStrategy,
    pub base_reason: StrategyReason,
    #[serde(default)]
    pub available_capabilities: Vec<String>,
}

impl Event for AgentStrategySelect {
    const NAME: &'static str = "agent.strategy.select";
}

/// A subagent started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSubagentStarted {
    pub session_id: String,
    pub subagent_id: String,
    pub role: String,
    pub model_id: Option<String>,
}

impl Event for AgentSubagentStarted {
    const NAME: &'static str = "agent.subagent.started";
}

/// A subagent finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSubagentFinished {
    pub session_id: String,
    pub subagent_id: String,
    pub success: bool,
}

impl Event for AgentSubagentFinished {
    const NAME: &'static str = "agent.subagent.finished";
}

/// Validation completed for one command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentValidationCompleted {
    pub session_id: String,
    pub command: String,
    pub status: String,
}

impl Event for AgentValidationCompleted {
    const NAME: &'static str = "agent.validation.completed";
}

/// A plugin session scope was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionCreated {
    pub session_id: String,
}

impl Event for AgentSessionCreated {
    const NAME: &'static str = "agent.session.created";
}

/// A plugin session scope was closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionClosed {
    pub session_id: String,
    pub reason: String,
}

impl Event for AgentSessionClosed {
    const NAME: &'static str = "agent.session.closed";
}

/// Builds the tool-before event from a definition and its invocation context.
pub(crate) fn tool_before(
    session_id: &str,
    definition: &ToolDefinition,
    depth: usize,
    active_delegates: usize,
    arguments: &Value,
) -> AgentToolBefore {
    AgentToolBefore {
        session_id: session_id.to_owned(),
        tool_name: definition.name.clone(),
        side_effect_class: definition.side_effect_class,
        side_effect_subclass: definition
            .side_effect_subclass
            .map(|subclass| subclass.as_str().to_owned()),
        subagent_depth: depth,
        active_delegates,
        arguments: arguments.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_stable() {
        assert_eq!(AgentTurnBefore::NAME, "agent.turn.before");
        assert_eq!(AgentTurnAfter::NAME, "agent.turn.after");
        assert_eq!(AgentModelBefore::NAME, "agent.model.before");
        assert_eq!(AgentModelAfter::NAME, "agent.model.after");
        assert_eq!(AgentToolBefore::NAME, "agent.tool.before");
        assert_eq!(AgentToolAfter::NAME, "agent.tool.after");
        assert_eq!(AgentApprovalBefore::NAME, "agent.approval.before");
        assert_eq!(AgentContextCollect::NAME, "agent.context.collect");
        assert_eq!(AgentStrategySelect::NAME, "agent.strategy.select");
        assert_eq!(AgentSubagentStarted::NAME, "agent.subagent.started");
        assert_eq!(AgentSubagentFinished::NAME, "agent.subagent.finished");
        assert_eq!(AgentValidationCompleted::NAME, "agent.validation.completed");
        assert_eq!(AgentSessionCreated::NAME, "agent.session.created");
        assert_eq!(AgentSessionClosed::NAME, "agent.session.closed");
    }

    #[test]
    fn tool_before_includes_classification_but_not_prompt_text() {
        let definition =
            ToolDefinition::new("read_file", "reads").input_schema(serde_json::json!({}));
        let event =
            tool_before("session-1", &definition, 1, 2, &serde_json::json!({"path": "src/lib.rs"}));
        assert_eq!(event.side_effect_class, SideEffectClass::Read);
        assert_eq!(event.arguments["path"], "src/lib.rs");
        let json = serde_json::to_value(&event).expect("serializes");
        assert_eq!(json["tool_name"], "read_file");
    }
}
