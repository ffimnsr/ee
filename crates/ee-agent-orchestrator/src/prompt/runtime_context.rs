//! Per-turn runtime context plane.
//!
//! Runtime facts are delivered as one user-role snapshot instead of being
//! concatenated onto the stable system prompt. The stable prompt stays
//! byte-identical across turns until a real deployment change, while cwd,
//! policy, approval, delegation, and turn guidance can change every turn.
//!
//! Section names and orders mirror DeepSeek Harness `CONTEXT_ORDERS`. Text is
//! `ee`-owned and derived from typed host state, never from free-form strings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::budget::BudgetSnapshot;
use crate::policy::ToolPolicy;
use crate::prompt::context::RuntimeSnapshot;
use crate::prompt::delegation::depth_line;
use crate::prompt::error::PromptError;
use crate::prompt::render::{Section, render_sections};
use crate::prompt::sections::{
    APPROVAL_POLICY, EE_MODE_POLICY, EE_TURN_GUIDANCE, SANDBOX_POLICY, SUBAGENT_DELEGATION,
    WORKING_DIRECTORY,
};
use crate::prompt::{AgentIdentity, ApprovalMode, ModePolicy, SandboxMode};

/// Typed facts that produce the runtime snapshot.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeContextInputs<'a> {
    /// Session working directory.
    pub cwd: &'a Path,
    /// Sorted, deduplicated workspace roots including `cwd`.
    pub workspace_roots: &'a [PathBuf],
    /// File policy resolved from `ToolPolicy`.
    pub sandbox: SandboxMode,
    /// Whether terminal execution is policy-allowed.
    pub allow_execute: bool,
    /// Approval capability resolved from host settings.
    pub approval: ApprovalMode,
    /// Whether `delegate_task` is policy-allowed for this turn.
    pub allow_delegate: bool,
    /// Root or subagent identity.
    pub agent: AgentIdentity<'a>,
    /// Per-turn strategy text, when the runtime produced any.
    pub turn_guidance: Option<&'a str>,
    /// Session mode policy; plan mode is expressed by the stable slot instead.
    pub mode_policy: ModePolicy,
}

impl SandboxMode {
    /// Derives the prompt-visible file policy from the effective tool policy.
    #[must_use]
    pub(crate) fn from_policy(policy: &ToolPolicy) -> Self {
        if !policy.allow_write {
            Self::ReadOnly
        } else if policy.scope.is_some() {
            Self::WorkspaceWrite
        } else {
            Self::WorkspaceWriteUnscoped
        }
    }
}

impl ApprovalMode {
    /// Derives approval visibility from the effective tool policy.
    ///
    /// Modes that disable host-approved side effects cannot answer an approval
    /// prompt, so the model is told those actions are denied rather than
    /// promised an escalation path that does not exist.
    #[must_use]
    pub(crate) fn from_policy(policy: &ToolPolicy) -> Self {
        if policy.allow_host_approved_side_effects { Self::Ask } else { Self::Never }
    }
}

/// Renders the runtime snapshot for one turn.
pub(crate) fn render(inputs: &RuntimeContextInputs<'_>) -> Result<RuntimeSnapshot, PromptError> {
    let fallback = [inputs.cwd.to_path_buf()];
    let roots = if inputs.workspace_roots.is_empty() {
        fallback.as_slice()
    } else {
        inputs.workspace_roots
    };
    let mut sections = vec![
        Section::for_slot(&WORKING_DIRECTORY, working_directory_text(inputs.cwd, roots))?,
        Section::for_slot(&SANDBOX_POLICY, sandbox_text(inputs.sandbox, inputs.allow_execute))?,
        Section::for_slot(&APPROVAL_POLICY, approval_text(inputs.approval))?,
    ];
    if let Some(text) = delegation_text(inputs.agent, inputs.allow_delegate) {
        sections.push(Section::for_slot(&SUBAGENT_DELEGATION, text)?);
    }
    if let Some(text) = inputs.mode_policy.runtime_text() {
        sections.push(Section::for_slot(&EE_MODE_POLICY, text)?);
    }
    if let Some(guidance) = inputs.turn_guidance.filter(|text| !text.trim().is_empty()) {
        sections.push(Section::for_slot(&EE_TURN_GUIDANCE, guidance)?);
    }
    let text = render_sections(&sections, &BTreeMap::new(), None)?;
    Ok(RuntimeSnapshot { text })
}

/// Appends the strategy guidance and any finite-budget pressure notice.
///
/// The guidance block always uses the `EE_TURN_GUIDANCE` slot; it is inserted
/// last because it is the highest emitted runtime order in this phase. The
/// pressure notice is advisory and appears only for opt-in finite caps.
pub(crate) fn append_turn_guidance(
    snapshot: &mut RuntimeSnapshot,
    guidance: Option<&str>,
    budget: Option<&BudgetSnapshot>,
) {
    let mut blocks = Vec::new();
    if let Some(guidance) = guidance.filter(|text| !text.trim().is_empty()) {
        blocks.push(guidance.trim());
    }
    let notice = budget.and_then(crate::prompt::budget_notice::pressure_notice);
    if let Some(notice) = notice.as_deref() {
        blocks.push(notice);
    }
    if !blocks.is_empty() {
        snapshot.push_block(&blocks.join("\n\n"));
    }
}

fn working_directory_text(cwd: &Path, roots: &[PathBuf]) -> String {
    let mut text = format!("Current working directory: {}.", quoted_path(cwd));
    text.push_str("\n\nWorkspace roots:");
    for root in roots {
        text.push_str("\n- ");
        text.push_str(&quoted_path(root));
    }
    text.push_str(
        "\n\nBuilt-in file and terminal tools require absolute paths. Resolve relative paths against the current working directory before calling any tool. Paths must stay inside the configured workspace roots.",
    );
    text
}

fn sandbox_text(mode: SandboxMode, allow_execute: bool) -> String {
    let mut text = match mode {
        SandboxMode::ReadOnly => concat!(
            "Current file policy: read-only. Reads are allowed; file writes and ",
            "terminal execution are denied by host policy. Do not attempt them."
        )
        .to_string(),
        SandboxMode::WorkspaceWrite => concat!(
            "Current file policy: workspace-write. Writes are allowed under the session ",
            "workspace roots listed above; paths outside them are denied."
        )
        .to_string(),
        SandboxMode::WorkspaceWriteUnscoped => concat!(
            "Current file policy: workspace-write. Writes are allowed for paths accepted ",
            "by the host, including editor-approved buffers."
        )
        .to_string(),
    };
    if allow_execute {
        text.push_str(
            "\n\nTerminal execution is allowed only through the advertised terminal tools.",
        );
    } else if mode != SandboxMode::ReadOnly {
        text.push_str("\n\nTerminal execution is denied by host policy.");
    }
    text
}

fn approval_text(mode: ApprovalMode) -> String {
    match mode {
        ApprovalMode::Ask => concat!(
            "Approval prompts are enabled for writes and execution. Without an available ",
            "answerer, the request fails closed."
        )
        .to_string(),
        ApprovalMode::Never => concat!(
            "Approval prompts are not available for this mode. Actions that would need ",
            "approval are denied; do not request escalation or retry with elevated options."
        )
        .to_string(),
    }
}

fn delegation_text(agent: AgentIdentity<'_>, allow_delegate: bool) -> Option<String> {
    let mut parts = Vec::new();
    if agent.depth > 0 {
        parts.push(format!(
            "You are a subagent named {} at delegation depth {}.",
            serde_json::to_string(agent.name).unwrap_or_else(|_| "\"subagent\"".into()),
            agent.depth
        ));
        if let Some(max_depth) = agent.max_depth
            && let Some(line) = depth_line(agent.depth, max_depth)
        {
            parts.push(line);
        }
    }
    if !allow_delegate {
        parts.push("Subagent delegation is denied for this mode.".to_string());
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// JSON-escapes a path so a quote in a path cannot break the sentence.
fn quoted_path(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy())
        .unwrap_or_else(|_| "\"<invalid path>\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    fn inputs<'a>(
        cwd: &'a Path,
        roots: &'a [PathBuf],
        sandbox: SandboxMode,
        approval: ApprovalMode,
    ) -> RuntimeContextInputs<'a> {
        RuntimeContextInputs {
            cwd,
            workspace_roots: roots,
            sandbox,
            approval,
            allow_execute: false,
            allow_delegate: false,
            agent: AgentIdentity { name: "root", depth: 0, max_depth: None },
            turn_guidance: None,
            mode_policy: ModePolicy::Write,
        }
    }

    #[test]
    fn working_directory_lists_sorted_roots_and_path_contract() {
        let cwd = Path::new("/work/project");
        let roots = [root("/shared/lib"), root("/work/project")];
        let snapshot = render(&inputs(cwd, &roots, SandboxMode::WorkspaceWrite, ApprovalMode::Ask))
            .expect("renders");
        assert!(snapshot.text.contains("Current working directory: \"/work/project\"."));
        assert!(snapshot.text.contains("- \"/shared/lib\""));
        assert!(snapshot.text.contains("- \"/work/project\""));
        assert!(snapshot.text.contains("require absolute paths"));
        assert!(snapshot.text.contains("Paths must stay inside the configured workspace roots."));
    }

    #[test]
    fn quote_in_path_is_json_escaped() {
        let cwd = Path::new("/work/weird\"name");
        let roots = [root("/work/weird\"name")];
        let snapshot = render(&inputs(cwd, &roots, SandboxMode::WorkspaceWrite, ApprovalMode::Ask))
            .expect("renders");
        assert!(snapshot.text.contains("Current working directory: \"/work/weird\\\"name\"."));
    }

    #[test]
    fn read_only_policy_denies_writes_and_execution() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let snapshot = render(&inputs(cwd, &roots, SandboxMode::ReadOnly, ApprovalMode::Never))
            .expect("renders");
        assert!(snapshot.text.contains("Current file policy: read-only."));
        assert!(snapshot.text.contains("terminal execution are denied by host policy"));
        assert!(snapshot.text.contains("Approval prompts are not available for this mode."));
        assert!(!snapshot.text.contains("workspace-write"));
    }

    #[test]
    fn scoped_and_unscoped_workspace_write_differ() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let scoped = render(&inputs(cwd, &roots, SandboxMode::WorkspaceWrite, ApprovalMode::Ask))
            .expect("renders");
        assert!(scoped.text.contains("paths outside them are denied"));
        let unscoped =
            render(&inputs(cwd, &roots, SandboxMode::WorkspaceWriteUnscoped, ApprovalMode::Ask))
                .expect("renders");
        assert!(unscoped.text.contains("editor-approved buffers"));
    }

    #[test]
    fn execution_availability_is_explicit() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let with_exec = RuntimeContextInputs {
            allow_execute: true,
            ..inputs(cwd, &roots, SandboxMode::WorkspaceWrite, ApprovalMode::Ask)
        };
        let text = render(&with_exec).expect("renders").text;
        assert!(
            text.contains(
                "Terminal execution is allowed only through the advertised terminal tools."
            )
        );

        let without_exec = RuntimeContextInputs {
            allow_execute: false,
            ..inputs(cwd, &roots, SandboxMode::WorkspaceWrite, ApprovalMode::Ask)
        };
        let text = render(&without_exec).expect("renders").text;
        assert!(text.contains("Terminal execution is denied by host policy."));
    }

    #[test]
    fn denied_delegation_is_reported() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let snapshot = render(&inputs(cwd, &roots, SandboxMode::ReadOnly, ApprovalMode::Never))
            .expect("renders");
        assert!(snapshot.text.contains("Subagent delegation is denied for this mode."));
    }

    #[test]
    fn subagent_depth_line_appears_only_at_cap() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let at_cap = RuntimeContextInputs {
            allow_delegate: true,
            agent: AgentIdentity { name: "reviewer", depth: 2, max_depth: Some(2) },
            ..inputs(cwd, &roots, SandboxMode::ReadOnly, ApprovalMode::Never)
        };
        let snapshot = render(&at_cap).expect("renders");
        assert!(
            snapshot.text.contains("You are a subagent named \"reviewer\" at delegation depth 2.")
        );
        assert!(snapshot.text.contains("remaining levels 0"));

        let below = RuntimeContextInputs {
            agent: AgentIdentity { name: "reviewer", depth: 1, max_depth: Some(2) },
            ..at_cap
        };
        let snapshot = render(&below).expect("renders");
        assert!(!snapshot.text.contains("remaining levels 0"));
    }

    #[test]
    fn mode_policy_text_is_rendered_before_turn_guidance() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let ask = RuntimeContextInputs {
            mode_policy: ModePolicy::Ask,
            turn_guidance: Some("Turn guidance: strategy=Direct"),
            ..inputs(cwd, &roots, SandboxMode::ReadOnly, ApprovalMode::Never)
        };
        let snapshot = render(&ask).expect("renders");
        let mode_at = snapshot.text.find("Current session mode: ask.").expect("mode text");
        let guidance_at = snapshot.text.find("Turn guidance: strategy=Direct").expect("guidance");
        assert!(mode_at < guidance_at, "{}", snapshot.text);

        let plan = RuntimeContextInputs {
            mode_policy: ModePolicy::Plan,
            ..inputs(cwd, &roots, SandboxMode::ReadOnly, ApprovalMode::Never)
        };
        assert!(!render(&plan).expect("renders").text.contains("Current session mode: plan."));
    }

    #[test]
    fn turn_guidance_is_rendered_into_its_slot() {
        let cwd = Path::new("/work/project");
        let roots = [root("/work/project")];
        let with_guidance = RuntimeContextInputs {
            turn_guidance: Some("Turn guidance: strategy=Direct; reason=simple"),
            ..inputs(cwd, &roots, SandboxMode::WorkspaceWrite, ApprovalMode::Ask)
        };
        let snapshot = render(&with_guidance).expect("renders");
        assert!(snapshot.text.ends_with("Turn guidance: strategy=Direct; reason=simple"));
    }

    #[test]
    fn append_guidance_and_pressure_notice() {
        let mut snapshot = RuntimeSnapshot { text: "base".into() };
        let budget = BudgetSnapshot {
            iterations_used: 15,
            iterations_max: Some(16),
            model_calls_used: 15,
            model_calls_max: Some(16),
            tool_calls_used: 0,
            tool_calls_max: None,
            subagents_used: 0,
            subagents_max: 8,
            output_bytes_used: 0,
            output_bytes_max: None,
            input_tokens_used: None,
            input_tokens_max: None,
            output_tokens_used: None,
            output_tokens_max: None,
        };
        append_turn_guidance(&mut snapshot, Some("Turn guidance: x"), Some(&budget));
        assert!(snapshot.text.starts_with("base\n\nTurn guidance: x\n\nTurn budget:"));
        assert!(snapshot.text.contains("1 model call(s)"));
    }

    #[test]
    fn append_without_guidance_or_pressure_is_a_no_op() {
        let mut snapshot = RuntimeSnapshot { text: "base".into() };
        append_turn_guidance(&mut snapshot, None, None);
        assert_eq!(snapshot.text, "base");
    }
}
