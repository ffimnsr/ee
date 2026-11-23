//! Prompt-bundle assembly: stable prompt, runtime snapshot, and instruction
//! planes for one provider turn.

use super::*;

/// Inputs for one prompt-bundle build.
pub(super) struct PromptBundleInputs<'a> {
    pub(super) cwd: &'a std::path::Path,
    pub(super) workspace_roots: &'a [std::path::PathBuf],
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) mode: &'a SessionModeId,
    pub(super) policy: &'a ToolPolicy,
    pub(super) advertised_tools: &'a [String],
    pub(super) mcp_servers: &'a [String],
    /// Test seam for trusted user instructions and user skill catalogs;
    /// `None` disables both user-global planes. Production resolves the config
    /// root from the environment and passes it explicitly.
    pub(super) global_config_root: Option<&'a std::path::Path>,
    /// Rendered background terminal job status, when any job is tracked.
    pub(super) background_jobs: Option<&'a str>,
    /// Configured remote instructions, already fetched for this session.
    pub(super) remote_instructions: &'a [crate::prompt::WorkspaceMessage],
    /// Selected user-defined primary agent, when one is active. Its body is
    /// injected through the trust plane that matches its scope.
    pub(super) user_agent: Option<&'a crate::user_agents::UserAgentDefinition>,
    /// Where editable harness components resolve from.
    pub(super) harness_source: crate::harness_source::HarnessSource,
}

/// Builds the stable prompt and runtime snapshot for one session mode.
pub(super) fn prompt_bundle(inputs: PromptBundleInputs<'_>) -> Result<PromptBundle, ProviderError> {
    let (harness_cwd, harness_roots) =
        inputs.harness_source.discovery_scope(inputs.cwd, inputs.workspace_roots);
    let (guidance_overrides, overridden_guidance_tools) =
        crate::prompt::tool_guidance_override::resolve(
            harness_cwd,
            &harness_roots,
            inputs.global_config_root,
            inputs.advertised_tools,
        );
    let mut bundle = crate::prompt::build(&PromptInputs {
        mode: inputs.mode,
        provider: inputs.provider,
        model: inputs.model,
        cwd: inputs.cwd,
        workspace_roots: inputs.workspace_roots,
        sandbox: SandboxMode::from_policy(inputs.policy),
        allow_execute: inputs.policy.allow_execute,
        approval: ApprovalMode::from_policy(inputs.policy),
        allow_delegate: inputs.policy.allow_delegate,
        agent: AgentIdentity {
            name: "root",
            depth: 0,
            max_depth: Some(inputs.policy.max_delegate_depth),
        },
        turn_guidance: None,
        advertised_tools: inputs.advertised_tools,
        mcp_servers: inputs.mcp_servers,
        overridden_guidance_tools: &overridden_guidance_tools,
        preset: PromptPreset::Standard,
    })
    .map_err(|error| ProviderError::BackendFailure(format!("prompt assembly failed: {error}")))?;
    // Background job status is derived per turn from live terminal state and
    // participates in the runtime-snapshot delta hash like every other section.
    if let Some(text) = inputs.background_jobs
        && let Some(runtime) = bundle.runtime.as_mut()
    {
        let slot = crate::prompt::sections::BACKGROUND_JOBS;
        runtime
            .push_section(crate::prompt::context::RuntimeSection::new(slot.name, slot.order, text));
    }
    // Recompute every turn: instruction files are attack surface, so a stale
    // snapshot is never carried in history. `TurnSystemContext` replays only
    // the current snapshot ahead of the current prompt.
    bundle.global = match inputs.global_config_root {
        Some(root) => crate::prompt::global::discover_at(root),
        None => Vec::new(),
    };
    bundle.workspace = crate::prompt::workspace::discover(inputs.cwd, inputs.workspace_roots);
    // Guardrails are always-on constraints from the repository (untrusted) or
    // the host user (trusted). The workspace file wins when both exist, and the
    // chosen body is placed ahead of the catalogs on its plane.
    if let Some(guardrails) =
        crate::prompt::guardrails::discover(harness_cwd, &harness_roots, inputs.global_config_root)
    {
        match guardrails.scope {
            crate::prompt::guardrails::GuardrailsScope::User => {
                bundle.global.insert(0, guardrails.message);
            }
            crate::prompt::guardrails::GuardrailsScope::Workspace => {
                bundle.workspace.insert(0, guardrails.message);
            }
        }
    }
    // File-level tool-guidance overrides replace the matching builtin rows in
    // the stable prompt and ride their scope's plane with a trust label.
    for overrides in guidance_overrides {
        match overrides.scope {
            crate::prompt::tool_guidance_override::ToolGuidanceScope::User => {
                bundle.global.push(overrides.message);
            }
            crate::prompt::tool_guidance_override::ToolGuidanceScope::Workspace => {
                bundle.workspace.push(overrides.message);
            }
        }
    }
    // Configured remote sources are explicit operator requests and ride the
    // same untrusted workspace plane; they are fetched once per session.
    bundle.workspace.extend(inputs.remote_instructions.iter().cloned());
    // Skill catalogs ride in the workspace plane: repository skills are
    // untrusted, user-global skills are trusted relative to workspace text.
    bundle.workspace.extend(crate::prompt::skills::catalog_messages(
        harness_cwd,
        &harness_roots,
        inputs.global_config_root,
    ));
    // Memory catalogs ride the same planes as skills: repository entries are
    // untrusted, user-global entries are trusted relative to workspace text.
    bundle.workspace.extend(crate::user_memories::catalog_messages(
        inputs.cwd,
        inputs.workspace_roots,
        inputs.global_config_root,
    ));
    // Script catalogs are inert without an execution surface, so they render
    // only when a terminal tool is advertised. Running a script still goes
    // through that tool's existing approval gate.
    bundle.workspace.extend(crate::prompt::scripts::catalog_messages(
        harness_cwd,
        &harness_roots,
        inputs.global_config_root,
        inputs.advertised_tools,
    ));
    // Experience catalogs carry cue-level summaries only; `load_experience`
    // returns one entry on demand.
    bundle.workspace.extend(crate::user_experience::catalog_messages(
        inputs.cwd,
        inputs.workspace_roots,
        inputs.global_config_root,
        None,
        None,
    ));
    // A selected primary agent's body is the session's role prompt. Personal
    // definitions are trusted relative to workspace content; project files are
    // attacker-controlled in shared workspaces and ride the untrusted plane.
    if let Some(agent) = inputs.user_agent {
        use crate::trust::TrustLevel;
        let trust = match agent.scope {
            crate::user_agents::UserAgentScope::User => TrustLevel::UserInstructionTrusted,
            crate::user_agents::UserAgentScope::Workspace => {
                TrustLevel::WorkspaceInstructionUntrusted
            }
        };
        let message = ModelMessage::text(
            ModelRole::User,
            format!("User-defined agent `{}` instructions:\n\n{}", agent.name, agent.prompt),
        )
        .with_trust(trust)
        .with_metadata([
            ("ee_prompt".to_string(), "user_agent_prompt".to_string()),
            ("ee_agent".to_string(), agent.name.clone()),
        ]);
        let message = WorkspaceMessage { path: agent.path.display().to_string(), message };
        match agent.scope {
            crate::user_agents::UserAgentScope::User => bundle.global.push(message),
            crate::user_agents::UserAgentScope::Workspace => bundle.workspace.push(message),
        }
    }
    Ok(bundle)
}

/// Inputs for [`runtime_prompt_bundle`].
#[derive(Clone, Copy)]
pub(super) struct RuntimePromptInputs<'a> {
    pub(super) cwd: &'a std::path::Path,
    pub(super) workspace_roots: &'a [std::path::PathBuf],
    pub(super) mode: &'a SessionModeId,
    pub(super) mcp_servers: &'a [String],
    pub(super) global_config_root: Option<&'a std::path::Path>,
    pub(super) background_jobs: Option<&'a str>,
    pub(super) remote_instructions: &'a [crate::prompt::WorkspaceMessage],
}

/// Builds the prompt planes for the runtime's current selection: the
/// mid-session model route and the active user-defined primary agent.
pub(super) fn runtime_prompt_bundle(
    runtime: &OrchestratorRuntime,
    models: &crate::model_registry::ModelRegistry,
    inputs: RuntimePromptInputs<'_>,
) -> Result<PromptBundle, ProviderError> {
    let (provider, model) = runtime
        .selected_model()
        .and_then(|id| models.info(&id).map(|info| (info.identity.provider_id.clone(), id)))
        .unwrap_or_else(|| super::conversation::default_model_labels(models));
    let policy = runtime.policy();
    let user_agent = runtime.active_user_agent();
    prompt_bundle(PromptBundleInputs {
        cwd: inputs.cwd,
        workspace_roots: inputs.workspace_roots,
        provider: &provider,
        model: &model,
        mode: inputs.mode,
        policy: policy.policy(),
        advertised_tools: &runtime.advertised_tool_names(),
        mcp_servers: inputs.mcp_servers,
        global_config_root: inputs.global_config_root,
        background_jobs: inputs.background_jobs,
        remote_instructions: inputs.remote_instructions,
        user_agent: user_agent.as_ref(),
        harness_source: runtime.config().harness_source.clone(),
    })
}
