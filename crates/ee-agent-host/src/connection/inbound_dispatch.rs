use super::*;

use super::driver::stop_reason_to_v1;

pub(super) fn handle_session_notification(
    notification: SessionNotification,
    inner: &AgentConnectionInner,
) {
    let Some(thread) =
        inner.threads.lock().expect("threads poisoned").get(&notification.session_id).cloned()
    else {
        tracing::warn!(
            session_id = %notification.session_id.0,
            "session/update for unknown session"
        );
        return;
    };
    thread.apply_update(notification.update);
}

pub(super) fn handle_elicitation_complete(
    notification: CompleteElicitationNotification,
    inner: &AgentConnectionInner,
) {
    let elicitation_id = notification.elicitation_id.0.to_string();
    if let Some(session_id) = inner.complete_url_elicitation(&elicitation_id) {
        let _ = inner.events.send(AgentEvent::ElicitationCompleted {
            agent_id: inner.agent_id.clone(),
            session_id,
            elicitation_id: notification.elicitation_id,
        });
    } else {
        tracing::debug!(
            agent_id = %inner.agent_id,
            elicitation_id,
            "ignored stale or unknown elicitation completion"
        );
    }
}

pub(super) fn handle_permission_request(
    request: RequestPermissionRequest,
    responder: ee_agent_protocol::Responder<RequestPermissionResponse>,
    cx: &ConnectionTo<AgentRole>,
    inner: &Arc<AgentConnectionInner>,
) -> Result<(), RpcError> {
    let session_id = request.session_id.clone();
    // Declarative allow/deny rules settle the request before any client
    // round-trip; only an `ask` verdict reaches the UI.
    if let Some(outcome) = inner.broker.preflight(&request.tool_call, &request.options) {
        let _ = inner.events.send(AgentEvent::PermissionAutoResolved {
            session_id: session_id.clone(),
            tool_call: request.tool_call.clone(),
            outcome: outcome.clone(),
        });
        let _ = responder.respond(RequestPermissionResponse::new(outcome));
        return Ok(());
    }
    let (request_id, info, rx) =
        inner.broker.request(session_id.clone(), request.tool_call, request.options);
    let _ = inner.events.send(AgentEvent::PermissionRequested {
        session_id: session_id.clone(),
        request: Box::new(info),
    });
    let events = inner.events.clone();
    let spawned = cx.spawn(async move {
        let outcome = match rx.await {
            Ok(outcome) => outcome,
            // Broker dropped the sender (cancel or connection close): answer
            // cancelled so the agent never hangs on an unanswered approval.
            Err(_) => RequestPermissionOutcome::Cancelled,
        };
        let _ = events.send(AgentEvent::PermissionResolved {
            session_id: session_id.clone(),
            request_id,
            outcome: outcome.clone(),
        });
        responder.respond(RequestPermissionResponse::new(outcome))
    });
    if spawned.is_err() {
        // Connection is shutting down; the responder is dropped with the
        // task and the agent is gone anyway.
        tracing::debug!(agent_id = %inner.agent_id, "permission request dropped: connection closing");
    }
    Ok(())
}

/// Routes a v2 `session/request_permission` (required `title`, optional
/// `subject`) through the same permission broker the v1 surface uses.
///
/// The broker is v1-shaped (it holds a [`ToolCallUpdate`]); the v2 tool-call
/// subject becomes that value, a command subject renders with the command
/// text in the title and command/cwd in `rawInput`, and an absent or unknown
/// subject becomes a title-only placeholder, so the UI has something
/// deterministic to show.
pub(super) fn handle_permission_request_v2(
    request: v2::RequestPermissionRequest,
    responder: ee_agent_protocol::Responder<v2::RequestPermissionResponse>,
    cx: &ConnectionTo<AgentRole>,
    inner: &Arc<AgentConnectionInner>,
) -> Result<(), RpcError> {
    use crate::v2_updates::permission_subject_tool_call;
    let session_id = ee_agent_protocol::SessionId::new(request.session_id.0.clone());
    let command = match request.subject.as_ref() {
        Some(v2::RequestPermissionSubject::Command(command)) => {
            Some(crate::events::PermissionCommand {
                command: command.command.clone(),
                cwd: command.cwd.0.clone(),
            })
        }
        _ => None,
    };
    let tool_call = permission_subject_tool_call(
        &request.title,
        request.subject.as_ref(),
        &format!("permission-{}", request.session_id.0),
    );
    let options: Vec<ee_agent_protocol::PermissionOption> = request
        .options
        .iter()
        .filter_map(|option| serde_json::from_value(serde_json::to_value(option).ok()?).ok())
        .collect();
    if let Some(outcome) = inner.broker.preflight(&tool_call, &options) {
        let _ = inner.events.send(AgentEvent::PermissionAutoResolved {
            session_id: session_id.clone(),
            tool_call: tool_call.clone(),
            outcome: outcome.clone(),
        });
        let v2_outcome: v2::RequestPermissionOutcome =
            serde_json::from_value(serde_json::to_value(outcome).expect("outcome"))
                .expect("permission outcome maps between versions");
        let _ = responder.respond(v2::RequestPermissionResponse::new(v2_outcome));
        return Ok(());
    }
    let (request_id, info, rx) =
        inner.broker.request_with_command(session_id.clone(), tool_call, options, command);
    let _ = inner.events.send(AgentEvent::PermissionRequested {
        session_id: session_id.clone(),
        request: Box::new(info),
    });
    let events = inner.events.clone();
    let spawned = cx.spawn(async move {
        let outcome = match rx.await {
            Ok(outcome) => serde_json::from_value(serde_json::to_value(outcome).expect("outcome"))
                .expect("permission outcome maps between versions"),
            // Broker dropped the sender (cancel or connection close): answer
            // cancelled so the agent never hangs on an unanswered approval.
            Err(_) => ee_agent_protocol::RequestPermissionOutcome::Cancelled,
        };
        let _ = events.send(AgentEvent::PermissionResolved {
            session_id: session_id.clone(),
            request_id,
            outcome: outcome.clone(),
        });
        // The v1-typed broker outcome must be serialized back onto the v2 wire.
        let v2_outcome: v2::RequestPermissionOutcome =
            serde_json::from_value(serde_json::to_value(outcome).expect("outcome"))
                .expect("permission outcome maps between versions");
        responder.respond(v2::RequestPermissionResponse::new(v2_outcome))
    });
    if spawned.is_err() {
        tracing::debug!(agent_id = %inner.agent_id, "v2 permission request dropped: connection closing");
    }
    Ok(())
}

/// Routes one v2 `session/update` notification: the idle `state_update`
/// resolves the pending turn's stop reason (see `run_prompt_task_v2`); every
/// other representable update is translated to the v1-typed shape and
/// reduced by the session thread.
pub(super) fn handle_v2_session_notification(
    notification: v2::UpdateSessionNotification,
    inner: &AgentConnectionInner,
) {
    let session_id = ee_agent_protocol::SessionId::new(notification.session_id.0.clone());
    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = &notification.update {
        if let Some(end_tx) =
            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id)
        {
            let stop_reason =
                idle.stop_reason.clone().map(stop_reason_to_v1).unwrap_or(StopReason::EndTurn);
            let _ = end_tx.send(stop_reason);
        }
        // Idle without a pending turn (background work, or a turn the host
        // already resolved locally) is expected and ignored.
        return;
    }
    let Some(thread) = inner.threads.lock().expect("threads poisoned").get(&session_id).cloned()
    else {
        tracing::debug!(
            session_id = %session_id.0,
            "v2 session/update for unknown session"
        );
        return;
    };
    crate::v2_updates::apply_v2_update(&thread, &notification.update);
}

pub(super) fn dispatch_client_request(
    inner: &Arc<AgentConnectionInner>,
    request: ClientRequest,
    responder: ee_agent_protocol::Responder<serde_json::Value>,
    cx: &ConnectionTo<AgentRole>,
) -> Result<(), RpcError> {
    let method = request.method().to_string();
    let session_id = request.session_id().cloned();
    let target = ClientRequest::client_request_target(&request);
    // Fail closed: never invoke a handler for a capability we did not
    // advertise during initialize.
    if !inner.handler_capabilities.supports_request(&request) {
        let error = match &request {
            ClientRequest::CreateElicitation(_) => {
                AgentError::invalid_params("elicitation mode was not advertised by the client")
            }
            _ => AgentError::CapabilityUnsupported { method: method.clone() },
        };
        return responder.respond_with_error(error.into_rpc());
    }
    if let ClientRequest::CreateElicitation(request) = &request
        && let ee_agent_protocol::ElicitationMode::Url(mode) = &request.mode
    {
        inner.register_url_elicitation(mode.elicitation_id.0.as_ref(), session_id.clone());
    }
    let _ = inner.events.send(AgentEvent::ClientRequestDispatched {
        session_id: session_id.clone(),
        method: method.clone(),
        target,
    });
    let request_id = responder.id().clone();
    let handler = inner.handler.clone();
    let cancellation = responder.cancellation();
    let mut cancel_rx = inner.register_client_request(&request_id);
    let inner_for_spawn = inner.clone();
    let spawned = cx.spawn(async move {
        let url_elicitation_id = match &request {
            ClientRequest::CreateElicitation(request) => match &request.mode {
                ee_agent_protocol::ElicitationMode::Url(mode) => {
                    Some(mode.elicitation_id.0.to_string())
                }
                _ => None,
            },
            _ => None,
        };
        let result = tokio::select! {
            _ = cancellation.cancelled() => responder.respond_with_error(RpcError::request_cancelled()),
            changed = cancel_rx.changed() => match changed {
                Ok(()) if *cancel_rx.borrow() => responder.respond_with_error(RpcError::request_cancelled()),
                Ok(()) | Err(_) => responder.respond_with_error(RpcError::request_cancelled()),
            },
            result = handler.handle(request) => match result {
                Ok(response) => {
                    let value = response.into_value().map_err(|error| {
                        AgentError::HandlerError(format!(
                            "handler response serialization failed: {error}"
                        ))
                        .into_rpc()
                    })?;
                    responder.respond_with_result(Ok(value))
                }
                Err(error) => responder.respond_with_error(error.into_rpc()),
            },
        };
        inner_for_spawn.finish_client_request(&request_id);
        if let Some(elicitation_id) = url_elicitation_id {
            inner_for_spawn.finish_url_elicitation(&elicitation_id);
        }
        result
    });
    if spawned.is_err() {
        // Connection is shutting down; the responder is dropped with the
        // task and the agent is gone anyway.
        tracing::debug!(agent_id = %inner.agent_id, "client request dropped: connection closing");
    }
    Ok(())
}
