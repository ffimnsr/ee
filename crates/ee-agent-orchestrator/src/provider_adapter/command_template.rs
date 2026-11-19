//! User-defined slash-command templates: rendering, agent/model selection,
//! and `subtask` execution through the shared subagent manager.

use super::*;

/// Inputs for one user-defined command invocation.
pub(super) struct CommandTemplate<'a> {
    pub(super) runtime: &'a OrchestratorRuntime,
    pub(super) sessions: &'a Arc<Mutex<HashMap<String, SessionRuntime>>>,
    pub(super) models: &'a crate::model_registry::ModelRegistry,
    pub(super) sink: &'a UpdateSink,
    pub(super) client: &'a ClientBridge,
    pub(super) cancel: &'a watch::Receiver<bool>,
    pub(super) ctx: &'a mut PromptContext,
    pub(super) command: &'a crate::user_commands::UserCommandDefinition,
    pub(super) instructions: Option<&'a str>,
    pub(super) transcript: &'a Arc<Mutex<Vec<ModelMessage>>>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Result of command handling: either an expanded prompt for the next turn,
/// or a fully handled turn (subtask or a reported command error).
pub(super) enum CommandTemplateOutcome {
    Prompt(String),
    Completed(PromptResult),
}

pub(super) async fn handle_command_template(
    command: CommandTemplate<'_>,
) -> Result<CommandTemplateOutcome, ProviderError> {
    let CommandTemplate {
        runtime,
        sessions,
        models,
        sink,
        client,
        cancel,
        ctx,
        command,
        instructions,
        transcript,
        telemetry,
        telemetry_turn,
        telemetry_started_at,
        telemetry_event_start,
    } = command;
    let arguments = match crate::user_commands::parse_arguments(instructions.unwrap_or_default()) {
        Ok(arguments) => arguments,
        Err(reason) => {
            return Ok(fail(
                sink,
                runtime,
                telemetry,
                telemetry_turn,
                telemetry_started_at,
                telemetry_event_start,
                format!("Cannot run `/{name}`: {reason}", name = command.name),
            ));
        }
    };
    let rendered = match command.render(&arguments) {
        Ok(rendered) => rendered,
        Err(reason) => {
            return Ok(fail(
                sink,
                runtime,
                telemetry,
                telemetry_turn,
                telemetry_started_at,
                telemetry_event_start,
                format!("Cannot run `/{name}`: {reason}", name = command.name),
            ));
        }
    };
    if !command.subtask {
        if let Some(agent) = &command.agent {
            if let Err(reason) = runtime.select_user_agent(Some(agent)) {
                return Ok(fail(
                    sink,
                    runtime,
                    telemetry,
                    telemetry_turn,
                    telemetry_started_at,
                    telemetry_event_start,
                    format!(
                        "Cannot apply agent `{agent}` for `/{name}`: {reason}",
                        name = command.name
                    ),
                ));
            }
            super::agent_command::sync_model_label(runtime, sessions, models, &ctx.session_id);
        }
        if let Some(model) = &command.model {
            let valid = models.info(model).is_some_and(|info| {
                info.enabled
                    && info
                        .identity
                        .capabilities
                        .contains(&crate::model_registry::ModelCapability::ChatCompletion)
            });
            if !valid {
                let available = models
                    .advertised()
                    .into_iter()
                    .map(|model| model.id)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(fail(
                    sink,
                    runtime,
                    telemetry,
                    telemetry_turn,
                    telemetry_started_at,
                    telemetry_event_start,
                    format!(
                        "Cannot apply model `{model}` for `/{name}`: not registered or not chat-capable; available models: {available}",
                        name = command.name
                    ),
                ));
            }
            runtime.set_selected_model(Some(model.clone()));
            super::agent_command::sync_model_label(runtime, sessions, models, &ctx.session_id);
        }
        ctx.prompt = vec![ContentBlock::Text(TextContent::new(rendered.clone()))];
        return Ok(CommandTemplateOutcome::Prompt(rendered));
    }

    let mut role = match runtime.command_role(command.agent.as_deref()) {
        Ok(role) => role,
        Err(error) => {
            return Ok(fail(
                sink,
                runtime,
                telemetry,
                telemetry_turn,
                telemetry_started_at,
                telemetry_event_start,
                format!("Cannot run `/{name}` as a subtask: {error}", name = command.name),
            ));
        }
    };
    if let Some(model) = &command.model {
        role.model = Some(model.clone());
    }
    let history = transcript.lock().expect("model transcript poisoned").clone();
    let result = runtime
        .run_user_command_subtask(
            role,
            rendered,
            runtime.selected_model(),
            history,
            client.clone(),
            cancel.clone(),
        )
        .await;
    let (outcome, message) = match result {
        Ok(result) => {
            let status = result.handoff.status;
            let summary = if result.handoff.summary.trim().is_empty() {
                result.error_summary.unwrap_or_default()
            } else {
                result.handoff.summary
            };
            (
                if status == crate::subagents::SubagentStatus::Completed {
                    TelemetryTurnOutcome::Succeeded
                } else {
                    TelemetryTurnOutcome::Failed
                },
                format!("`/{name}` subtask {status:?}:\n{summary}", name = command.name),
            )
        }
        Err(error) => (
            TelemetryTurnOutcome::Failed,
            format!("`/{name}` subtask failed: {error}", name = command.name),
        ),
    };
    sink.agent_message_chunk("command", message).map_err(|error| {
        ProviderError::BackendFailure(format!("failed to report command result: {error}"))
    })?;
    finish_provider_telemetry(
        telemetry,
        ProviderTelemetryTurn {
            turn_id: telemetry_turn,
            started_at: telemetry_started_at,
            events: &runtime.event_snapshot()[telemetry_event_start..],
            outcome,
            terminal_state: None,
            evidence_ids: Vec::new(),
            usage: None,
        },
    );
    Ok(CommandTemplateOutcome::Completed(PromptResult::new(StopReason::EndTurn)))
}

fn fail(
    sink: &UpdateSink,
    runtime: &OrchestratorRuntime,
    telemetry: &Arc<Mutex<TelemetryRecorder>>,
    telemetry_turn: &str,
    telemetry_started_at: Instant,
    telemetry_event_start: usize,
    message: String,
) -> CommandTemplateOutcome {
    let _ = sink.agent_message_chunk("command", message);
    finish_provider_telemetry(
        telemetry,
        ProviderTelemetryTurn {
            turn_id: telemetry_turn,
            started_at: telemetry_started_at,
            events: &runtime.event_snapshot()[telemetry_event_start..],
            outcome: TelemetryTurnOutcome::Failed,
            terminal_state: None,
            evidence_ids: Vec::new(),
            usage: None,
        },
    );
    CommandTemplateOutcome::Completed(PromptResult::new(StopReason::EndTurn))
}
