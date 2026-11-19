//! `/primary-agent` command: list or select a user-defined primary agent.
//!
//! Selection changes the next turn's prompt, model, step budget, and
//! permission overlay. It is rejected while a turn is running by the ACP
//! dispatcher before this handler runs, so the transcript and provider route
//! stay consistent.

use super::conversation::persist_session_snapshot;
use super::*;

/// Inputs for one `/primary-agent` invocation.
pub(super) struct AgentCommand<'a> {
    pub(super) runtime: &'a OrchestratorRuntime,
    pub(super) sessions: &'a Arc<Mutex<HashMap<String, SessionRuntime>>>,
    pub(super) session_store: &'a SessionStateStore,
    pub(super) implementation_name: &'a str,
    pub(super) models: &'a crate::model_registry::ModelRegistry,
    pub(super) sink: &'a UpdateSink,
    pub(super) session_id: &'a SessionId,
    /// Agent name; `None` lists the available primary agents.
    pub(super) instructions: Option<&'a str>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

pub(super) fn handle_agent_command(
    command: AgentCommand<'_>,
) -> Result<PromptResult, ProviderError> {
    let AgentCommand {
        runtime,
        sessions,
        session_store,
        implementation_name,
        models,
        sink,
        session_id,
        instructions,
        telemetry,
        telemetry_turn,
        telemetry_started_at,
        telemetry_event_start,
    } = command;
    let registry = runtime.user_agents_snapshot();
    let selection = instructions.map(str::trim).filter(|name| !name.is_empty());
    let message = match selection {
        None => list_agents(&registry, runtime.active_user_agent_name().as_deref()),
        Some(name) => match runtime.select_user_agent(Some(name)) {
            Ok(()) => {
                sync_model_label(runtime, sessions, models, session_id);
                match runtime.active_user_agent() {
                    Some(agent) => {
                        let model = runtime
                            .selected_model()
                            .unwrap_or_else(|| "session default".to_string());
                        format!("Selected agent `{}` (model: {model}).", agent.name)
                    }
                    None => "Agent selection cleared.".to_string(),
                }
            }
            Err(reason) => {
                let available = registry
                    .selectable_primary()
                    .map(|agent| agent.name.as_str())
                    .collect::<Vec<_>>();
                let available =
                    if available.is_empty() { "none".to_string() } else { available.join(", ") };
                format!(
                    "Cannot select agent `{name}`: {reason}\nAvailable primary agents: {available}"
                )
            }
        },
    };
    sink.agent_message_chunk("agent", message).map_err(|error| {
        ProviderError::BackendFailure(format!("failed to report agent selection: {error}"))
    })?;
    let events = runtime.event_snapshot();
    finish_provider_telemetry(
        telemetry,
        ProviderTelemetryTurn {
            turn_id: telemetry_turn,
            started_at: telemetry_started_at,
            events: &events[telemetry_event_start..],
            outcome: TelemetryTurnOutcome::Succeeded,
            terminal_state: None,
            evidence_ids: Vec::new(),
            usage: None,
        },
    );
    persist_session_snapshot(session_store, implementation_name, session_id, sessions)?;
    Ok(PromptResult::new(StopReason::EndTurn))
}

fn list_agents(registry: &crate::user_agents::UserAgentRegistry, active: Option<&str>) -> String {
    let mut lines = vec!["Available primary agents:".to_string()];
    let mut listed = 0;
    for agent in registry.selectable_primary() {
        listed += 1;
        let marker = if active == Some(agent.name.as_str()) { "*" } else { "-" };
        lines.push(format!("{marker} {} — {}", agent.name, agent.description));
    }
    if listed == 0 {
        lines.push(
            "  (none discovered under <workspace>/.agents/agents or the ee config root)"
                .to_string(),
        );
    }
    lines.push(
        "Use `/primary-agent <name>` to select, or `/primary-agent default` to clear.".to_string(),
    );
    lines.join("\n")
}

/// Keeps the session's deployment-persona labels aligned with a model change.
pub(super) fn sync_model_label(
    runtime: &OrchestratorRuntime,
    sessions: &Arc<Mutex<HashMap<String, SessionRuntime>>>,
    models: &crate::model_registry::ModelRegistry,
    session_id: &SessionId,
) {
    let Some(model_id) = runtime.selected_model() else { return };
    let Some(info) = models.info(&model_id) else { return };
    let mut sessions = sessions.lock().expect("adapter sessions poisoned");
    if let Some(session) = sessions.get_mut(&session_id.to_string()) {
        session.model_label = model_id;
        session.provider_label = info.identity.provider_id.clone();
    }
}
