//! Cordis-style plugin runtime for the agent host.
//!
//! Phase 5 integrates the host-agnostic `ee-plugin-runtime` loader into
//! `ee-agent-orchestrator` and exposes the `agent.*` service namespace:
//!
//! - [`host`] owns the [`ee_plugin_runtime::PluginManager`], the service
//!   registry, the [`services::sessions`] scopes, and plugin diagnostics.
//! - [`services`] wraps the existing orchestrator stores
//!   (`ToolRegistry`, `ModelRegistry`, `PolicyEngine`, context planner,
//!   strategy selector, validation planner, sessions) behind typed services.
//! - [`events`] declares the `agent.*` event catalog.
//! - [`effects`] applies structured plugin results. Agent plugins never touch
//!   editor UI; they return tool results, context updates, and session control.
//! - [`bridge`] maps orchestrator boundaries onto the event catalog and gates
//!   tool/approval decisions through mandatory-first policy evaluation.
//!
//! Host rules (see `docs/upgrades/plugin-system-cordis.md` §11):
//!
//! - Mandatory policy, approval, workspace trust, destructive-operation
//!   safeguards, budget, and write-transaction checks run before plugin
//!   listeners.
//! - Plugin tools execute through the existing `ToolExecutor` pipeline; the
//!   service only registers and removes them.
//! - Session-scoped contributions dispose at session close; turn-scoped
//!   contributions dispose at turn end.
//! - Agent plugins cannot write editor UI directly.

pub mod bridge;
pub mod effects;
pub mod events;
pub mod host;
pub mod services;

#[cfg(test)]
mod tests;

pub use bridge::{AgentBridge, ContextCollectAccumulator, StrategyProposal};
pub use effects::{
    AgentEffect, AgentEffectQueue, AgentEffectSink, EffectReport, SessionControlEffect,
    ToolResultEffect,
};
pub use events::{
    AgentApprovalBefore, AgentContextCollect, AgentModelAfter, AgentModelBefore,
    AgentSessionClosed, AgentSessionCreated, AgentStrategySelect, AgentSubagentFinished,
    AgentSubagentStarted, AgentToolAfter, AgentToolBefore, AgentTurnAfter, AgentTurnBefore,
    AgentValidationCompleted,
};
pub use host::{
    AgentHealthReport, AgentHostAdapter, AgentNotifier, AgentNotifierEvent, AgentPluginHost,
    AgentPluginHostConfig, AgentPluginStatus, NoopAgentNotifier, QueuedAgentNotifier,
};
pub use services::{
    AgentServiceRouter, ContextCollection, ContextContribution, ContextProvider, ContextService,
    ModelService, PolicyService, SessionHandle, SessionService, SessionState, StrategyCandidate,
    StrategyService, ToolScope, ToolService, TurnScope, ValidationContribution, ValidationService,
};
