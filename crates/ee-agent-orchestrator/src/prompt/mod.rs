//! Ordered prompt assembly for agent turns.
//!
//! Phase 0 defines the contract: DSH-compatible slot registry, strict
//! renderer, and the transcript context type. The builder is intentionally
//! not wired into the turn loop yet; Phase 1 replaces the flat context strings.
#![allow(dead_code)] // Phase 0 contracts are consumed by Phase 1; remove after wiring.

pub(crate) mod budget_notice;
pub(crate) mod context;
pub(crate) mod delegation;
pub(crate) mod error;
pub(crate) mod render;
pub(crate) mod repeat_guard;
pub(crate) mod sections;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ee_agent_protocol::SessionModeId;

use crate::prompt::context::{RuntimeSnapshot, WorkspaceMessage};
use crate::prompt::error::PromptError;
use crate::prompt::render::{Section, render_sections};
use crate::prompt::sections::{
    DEPLOYMENT_PERSONA_PREFIX, DEPLOYMENT_PERSONA_SUFFIX, HARNESS_IDENTITY,
};
use crate::tools::ToolDefinition;

/// Which section set the builder emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptPreset {
    /// Identity, deployment persona, mode policy, and advertised tool guidance.
    Standard,
    /// Identity only, for deterministic tests of `complete` semantics.
    Minimal,
}

/// File policy resolved by the adapter, never parsed from free-form text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxMode {
    /// Reads only; writes and execution are denied.
    ReadOnly,
    /// Writes under the workspace roots.
    WorkspaceWrite,
}

/// Approval capability visible to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalMode {
    /// The host can answer approval prompts.
    Ask,
    /// Approval is unavailable; actions that need it are denied.
    Never,
}

/// Agent identity and delegation position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentIdentity<'a> {
    /// Role or agent name.
    pub name: &'a str,
    /// Current delegation depth; root is zero.
    pub depth: usize,
    /// Absolute delegation cap, when one is configured.
    pub max_depth: Option<usize>,
}

/// Host fact about one configured MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct McpServerFact<'a> {
    /// Configured server name.
    pub name: &'a str,
}

/// Explicit inputs for one prompt assembly.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptInputs<'a> {
    /// Active ACP session mode.
    pub mode: &'a SessionModeId,
    /// Resolved provider id.
    pub provider: &'a str,
    /// Resolved model id.
    pub model: &'a str,
    /// Session working directory.
    pub cwd: &'a Path,
    /// Sorted workspace roots, including cwd.
    pub workspace_roots: &'a [PathBuf],
    /// File policy derived from `ToolPolicy`.
    pub sandbox: SandboxMode,
    /// Approval capability derived from host settings.
    pub approval: ApprovalMode,
    /// Root or subagent identity.
    pub agent: AgentIdentity<'a>,
    /// Tools advertised for this turn.
    pub tools: &'a [ToolDefinition],
    /// Configured MCP servers.
    pub mcp_servers: &'a [McpServerFact<'a>],
    /// Per-turn strategy text, when the runtime produced any.
    pub turn_guidance: Option<&'a str>,
    /// Section set to emit.
    pub preset: PromptPreset,
}

/// Rendered prompt planes for one turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PromptBundle {
    /// Stable system prompt, empty when nothing is emitted.
    pub stable: String,
    /// Runtime snapshot, when the builder produced one.
    pub runtime: Option<RuntimeSnapshot>,
    /// Workspace instruction messages, when discovery produced any.
    pub workspace: Vec<WorkspaceMessage>,
}

/// Builds the stable prompt and runtime snapshot.
pub(crate) fn build(inputs: &PromptInputs<'_>) -> Result<PromptBundle, PromptError> {
    let sections = match inputs.preset {
        PromptPreset::Minimal => vec![
            Section::for_slot(&HARNESS_IDENTITY, "You are an AI agent powered by ee.")?
                .with_complete(true),
        ],
        PromptPreset::Standard => vec![
            Section::for_slot(&HARNESS_IDENTITY, "You are an AI agent powered by ee.")?,
            Section::for_slot(
                &DEPLOYMENT_PERSONA_PREFIX,
                "You are a coding agent powered by the {{provider}}/{{model}} model.",
            )?,
            Section::for_slot(
                &DEPLOYMENT_PERSONA_SUFFIX,
                "Host policy and host approval remain authoritative. Never claim a file, command, or test succeeded without a tool result that proves it.",
            )?,
        ],
    };

    let variables = BTreeMap::from([
        ("provider".to_string(), inputs.provider.to_string()),
        ("model".to_string(), inputs.model.to_string()),
    ]);
    let stable = render_sections(&sections, &variables, None)?;
    Ok(PromptBundle { stable, runtime: None, workspace: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(provider: &'a str, model: &'a str) -> PromptInputs<'a> {
        let mode: &'static SessionModeId = Box::leak(Box::new(SessionModeId::new("write")));
        PromptInputs {
            mode,
            provider,
            model,
            cwd: Path::new("/workspace"),
            workspace_roots: &[],
            sandbox: SandboxMode::WorkspaceWrite,
            approval: ApprovalMode::Ask,
            agent: AgentIdentity { name: "root", depth: 0, max_depth: None },
            tools: &[],
            mcp_servers: &[],
            turn_guidance: None,
            preset: PromptPreset::Standard,
        }
    }

    #[test]
    fn standard_prompt_has_identity_and_persona() {
        let bundle = build(&inputs("test", "test-model")).expect("builds");
        assert_eq!(
            bundle.stable,
            "You are an AI agent powered by ee.\n\nYou are a coding agent powered by the test/test-model model.\n\nHost policy and host approval remain authoritative. Never claim a file, command, or test succeeded without a tool result that proves it."
        );
        assert!(bundle.runtime.is_none());
        assert!(bundle.workspace.is_empty());
    }

    #[test]
    fn minimal_prompt_uses_complete_identity() {
        let mut inputs = inputs("test", "test-model");
        inputs.preset = PromptPreset::Minimal;
        let bundle = build(&inputs).expect("builds");
        assert_eq!(bundle.stable, "You are an AI agent powered by ee.");
    }

    #[test]
    fn empty_provider_or_model_fails_instead_of_printing_placeholders() {
        assert!(matches!(
            build(&inputs("", "test-model")),
            Err(PromptError::UndefinedVariable { .. })
        ));
        assert!(matches!(build(&inputs("test", "")), Err(PromptError::UndefinedVariable { .. })));
    }
}
