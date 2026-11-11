//! Ordered prompt assembly for agent turns.
//!
//! The stable system prompt and the per-turn runtime snapshot are separate
//! planes. Stable slots mirror DeepSeek Harness `SECTION_ORDERS`; runtime slots
//! mirror `CONTEXT_ORDERS`. `ee` tool names and policy own the text.

pub(crate) mod budget_notice;
pub(crate) mod context;
pub(crate) mod delegation;
pub(crate) mod error;
pub(crate) mod render;
pub(crate) mod repeat_guard;
pub(crate) mod runtime_context;
pub(crate) mod sections;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ee_agent_protocol::SessionModeId;

pub use crate::prompt::context::{RuntimeSnapshot, WorkspaceMessage};
use crate::prompt::error::PromptError;
use crate::prompt::render::{Section, render_sections};
use crate::prompt::sections::{
    DEPLOYMENT_PERSONA_PREFIX, DEPLOYMENT_PERSONA_SUFFIX, HARNESS_IDENTITY, PLAN_POLICY,
};

/// Which section set the builder emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptPreset {
    /// Identity, deployment persona, mode policy, and advertised tool guidance.
    Standard,
    /// Identity only, for deterministic tests of `complete` semantics.
    #[cfg(test)]
    Minimal,
}

/// File policy resolved by the adapter, never parsed from free-form text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxMode {
    /// Reads only; writes and execution are denied.
    ReadOnly,
    /// Writes under the workspace roots.
    WorkspaceWrite,
    /// Writes for host-accepted paths when no workspace scope is active.
    WorkspaceWriteUnscoped,
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
    /// Whether terminal execution is policy-allowed.
    pub allow_execute: bool,
    /// Approval capability derived from host settings.
    pub approval: ApprovalMode,
    /// Whether delegation is policy-allowed for this turn.
    pub allow_delegate: bool,
    /// Root or subagent identity.
    pub agent: AgentIdentity<'a>,
    /// Per-turn strategy text, when the runtime produced any.
    pub turn_guidance: Option<&'a str>,
    /// Section set to emit.
    pub preset: PromptPreset,
}

/// Rendered prompt planes for one turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptBundle {
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
        #[cfg(test)]
        PromptPreset::Minimal => vec![
            Section::for_slot(&HARNESS_IDENTITY, "You are an AI agent powered by ee.")?
                .with_complete(true),
        ],
        PromptPreset::Standard => {
            let mut sections = vec![
                Section::for_slot(&HARNESS_IDENTITY, "You are an AI agent powered by ee.")?,
                Section::for_slot(
                    &DEPLOYMENT_PERSONA_PREFIX,
                    "You are a coding agent powered by the {{provider}}/{{model}} model.",
                )?,
            ];
            if inputs.mode.to_string() == PLAN_MODE_ID {
                sections.push(Section::for_slot(&PLAN_POLICY, PLAN_POLICY_TEXT)?);
            }
            sections.push(Section::for_slot(
                &DEPLOYMENT_PERSONA_SUFFIX,
                "Host policy and host approval remain authoritative. Never claim a file, command, or test succeeded without a tool result that proves it.",
            )?);
            sections
        }
    };

    let variables = BTreeMap::from([
        ("provider".to_string(), inputs.provider.to_string()),
        ("model".to_string(), inputs.model.to_string()),
    ]);
    let stable = render_sections(&sections, &variables, None)?;

    let runtime = runtime_context::render(&runtime_context::RuntimeContextInputs {
        cwd: inputs.cwd,
        workspace_roots: inputs.workspace_roots,
        sandbox: inputs.sandbox,
        allow_execute: inputs.allow_execute,
        approval: inputs.approval,
        allow_delegate: inputs.allow_delegate,
        agent: inputs.agent,
        turn_guidance: inputs.turn_guidance,
    })?;
    let runtime = (!runtime.text.trim().is_empty()).then_some(runtime);

    Ok(PromptBundle { stable, runtime, workspace: Vec::new() })
}

/// Mode id that activates `PLAN_POLICY`.
const PLAN_MODE_ID: &str = "plan";

/// `ee` plan-mode contract: the existing deliverable format plus one payload.
const PLAN_POLICY_TEXT: &str = r#"You are in plan mode. Investigate with read-only tools when needed, then return a concrete implementation plan, not a general explanation or a promise to investigate later.

Format final response exactly with these sections:
## Plan
1. `<file or symbol>` — exact change, reason, dependency/order, and observable success criterion.
2. Continue one numbered item per independently actionable step.
## Validation
- Name exact tests, checks, or manual verification for the completed implementation; say why validation cannot run when none applies.
## Open questions
- List only blockers that require a user decision; otherwise write `None`.

Every plan step must name affected files or symbols when known, describe an executable change, and state how completion is verified. Do not claim implementation is complete. Do not modify files, run commands, or delegate.

After `## Open questions`, include exactly one machine-readable payload. Keep it synchronized with `## Plan` and use this exact shape:
<!-- ee-plan
[
  {
    "title": "short task title",
    "action": "exact implementation action",
    "scope": "affected file or symbol",
    "expected_result": "observable completed state",
    "verification": "specific test, check, or manual verification",
    "depends_on": ["prior task title or #index"]
  }
]
-->
The payload must contain at least one task. It is hidden from rendered Markdown and becomes the ACP task plan."#;

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
            allow_execute: true,
            approval: ApprovalMode::Ask,
            allow_delegate: true,
            agent: AgentIdentity { name: "root", depth: 0, max_depth: None },
            turn_guidance: None,
            preset: PromptPreset::Standard,
        }
    }

    #[test]
    fn standard_prompt_has_identity_persona_and_runtime_context() {
        let bundle = build(&inputs("test", "test-model")).expect("builds");
        assert_eq!(
            bundle.stable,
            "You are an AI agent powered by ee.\n\nYou are a coding agent powered by the test/test-model model.\n\nHost policy and host approval remain authoritative. Never claim a file, command, or test succeeded without a tool result that proves it."
        );
        let runtime = bundle.runtime.expect("runtime context");
        assert!(runtime.text.contains("Current working directory: \"/workspace\"."));
        assert!(runtime.text.contains("Current file policy: workspace-write."));
        assert!(bundle.workspace.is_empty());
    }

    #[test]
    fn minimal_prompt_uses_complete_identity_but_keeps_runtime_context() {
        let mut inputs = inputs("test", "test-model");
        inputs.preset = PromptPreset::Minimal;
        let bundle = build(&inputs).expect("builds");
        assert_eq!(bundle.stable, "You are an AI agent powered by ee.");
        assert!(bundle.runtime.is_some());
    }

    #[test]
    fn plan_mode_adds_plan_policy_before_persona_suffix() {
        let mode: &'static SessionModeId = Box::leak(Box::new(SessionModeId::new("plan")));
        let mut inputs = inputs("test", "test-model");
        inputs.mode = mode;
        let bundle = build(&inputs).expect("builds");
        assert!(bundle.stable.contains("You are in plan mode."));
        assert!(bundle.stable.contains("## Plan"));
        assert!(bundle.stable.contains("<!-- ee-plan"));
        assert!(
            bundle.stable.find("## Plan").expect("plan section")
                < bundle.stable.find("Host policy and host approval").expect("suffix")
        );
    }

    #[test]
    fn write_mode_omits_plan_policy() {
        let bundle = build(&inputs("test", "test-model")).expect("builds");
        assert!(!bundle.stable.contains("You are in plan mode."));
    }

    #[test]
    fn stable_prompt_ignores_cwd_while_runtime_tracks_it() {
        let first_roots = [PathBuf::from("/work/one")];
        let second_roots = [PathBuf::from("/work/two")];
        let mut first = inputs("test", "test-model");
        first.cwd = Path::new("/work/one");
        first.workspace_roots = &first_roots;
        let mut second = inputs("test", "test-model");
        second.cwd = Path::new("/work/two");
        second.workspace_roots = &second_roots;

        let first = build(&first).expect("builds");
        let second = build(&second).expect("builds");
        assert_eq!(first.stable, second.stable);
        assert_ne!(first.runtime, second.runtime);
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
