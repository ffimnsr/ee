use super::*;

use super::session_servers::mcp_servers_to_v2;

type PromptTask = Pin<Box<dyn Future<Output = PromptTaskCompletion> + Send>>;
type LifecycleTask = Pin<Box<dyn Future<Output = ()> + Send>>;

struct PendingPrompt {
    request: PromptRequest,
    cancel: watch::Receiver<bool>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
}

struct PromptTaskCompletion {
    session_id: SessionId,
    request_id: RequestId,
    result: Result<PromptResponse, AgentError>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
}

/// Executes connection commands against the SDK connection.
///
/// Prompt requests are tracked separately and polled concurrently, keyed by
/// session and JSON-RPC request id. Independent create/load/resume requests are
/// also polled concurrently; SDK request ids preserve response attribution.
/// Connection-wide and mutating session controls remain FIFO. Shutdown closes
/// intake, resolves bounded tasks, then rejects queued commands.
#[allow(clippy::too_many_arguments)]
pub(super) async fn driver_loop(
    connection: ConnectionTo<AgentRole>,
    mut rx: mpsc::UnboundedReceiver<ConnectionCommand>,
    mut terminate: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    request_timeout: Duration,
    max_concurrent_prompts: usize,
    inner: Arc<AgentConnectionInner>,
    is_v2: bool,
) {
    let agent_id = inner.agent_id.clone();
    let (prompt_shutdown_tx, prompt_shutdown_rx) = watch::channel(false);
    let (lifecycle_shutdown_tx, lifecycle_shutdown_rx) = watch::channel(false);
    let mut prompt_tasks = FuturesUnordered::<PromptTask>::new();
    let mut steering_tasks = FuturesUnordered::<PromptTask>::new();
    let mut lifecycle_tasks = FuturesUnordered::<LifecycleTask>::new();
    let mut active_prompts = HashMap::<SessionId, RequestId>::new();
    let mut queued_prompts = VecDeque::<PendingPrompt>::new();
    let mut queued_sessions = HashSet::<SessionId>::new();
    loop {
        tokio::select! {
            _ = terminate.changed() => break,
            Some(completion) = prompt_tasks.next(), if !prompt_tasks.is_empty() => {
                finish_prompt_task(completion, &mut active_prompts);
                dispatch_queued_prompts(
                    &connection,
                    &mut prompt_tasks,
                    &mut active_prompts,
                    &mut queued_prompts,
                    &mut queued_sessions,
                    max_concurrent_prompts,
                    &prompt_shutdown_rx,
                    &state_rx,
                    &agent_id,
                    &inner,
                    is_v2,
                );
            }
            Some(completion) = steering_tasks.next(), if !steering_tasks.is_empty() => {
                // Steer/queue deliveries never occupy the per-session turn
                // reservation, so only the caller's oneshot is resolved here.
                let _ = completion.tx.send(completion.result);
            }
            Some(()) = lifecycle_tasks.next(), if !lifecycle_tasks.is_empty() => {}
            command = rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    ConnectionCommand::NewSession { request, tx } => {
                        let connection = connection.clone();
                        let shutdown = lifecycle_shutdown_rx.clone();
                        let agent_id = agent_id.clone();
                        // v2 wire: mcpServers carry a `type` discriminator;
                        // the v1-typed request is converted before sending and
                        // the v2 response is rounded back to the v1 shape the
                        // host API exposes.
                        let v2_request = is_v2.then(|| v2_new_session_request(&request));
                        lifecycle_tasks.push(Box::pin(async move {
                            let result = tokio::select! {
                                () = wait_for_true(shutdown) => {
                                    Err(AgentError::ConnectionClosed { agent_id })
                                }
                                result = async {
                                    if let Some(v2_request) = &v2_request {
                                        let response: v2::NewSessionResponse =
                                            request_with_timeout(
                                                &connection,
                                                v2_request.clone(),
                                                request_timeout,
                                                "session/new",
                                            )
                                            .await?;
                                        let mut converted = ee_agent_protocol::NewSessionResponse::new(
                                            response.session_id.0,
                                        );
                                        let options =
                                            crate::v2_updates::config_options_to_v1(
                                                response.config_options,
                                            );
                                        if !options.is_empty() {
                                            converted = converted.config_options(options);
                                        }
                                        Ok::<_, AgentError>(converted)
                                    } else {
                                        request_with_timeout(
                                            &connection,
                                            request,
                                            request_timeout,
                                            "session/new",
                                        )
                                        .await
                                    }
                                } => result,
                            };
                            let _ = tx.send(result);
                        }));
                    }
                    ConnectionCommand::LoadSession { request, tx } => {
                        if is_v2 {
                            // v2 removed `session/load`; `session/resume` with
                            // optional `replayFrom` replaces it.
                            let _ = tx.send(Err(AgentError::CapabilityUnsupported {
                                method: "session/load".into(),
                            }));
                            continue;
                        }
                        let connection = connection.clone();
                        let shutdown = lifecycle_shutdown_rx.clone();
                        let agent_id = agent_id.clone();
                        lifecycle_tasks.push(Box::pin(async move {
                            let result = tokio::select! {
                                () = wait_for_true(shutdown) => {
                                    Err(AgentError::ConnectionClosed { agent_id })
                                }
                                result = request_with_timeout(
                                    &connection,
                                    request,
                                    request_timeout,
                                    "session/load",
                                ) => result,
                            };
                            let _ = tx.send(result);
                        }));
                    }
                    ConnectionCommand::ListSessions { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/list").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::DeleteSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/delete").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::ResumeSession { request, tx } => {
                        let connection = connection.clone();
                        let shutdown = lifecycle_shutdown_rx.clone();
                        let agent_id = agent_id.clone();
                        // v2 wire: same conversion as session/new; `replayFrom`
                        // stays unset (no replay), which equals the v1 resume
                        // behavior.
                        let v2_request = is_v2.then(|| v2_resume_session_request(&request));
                        lifecycle_tasks.push(Box::pin(async move {
                            let result = tokio::select! {
                                () = wait_for_true(shutdown) => {
                                    Err(AgentError::ConnectionClosed { agent_id })
                                }
                                result = async {
                                    if let Some(v2_request) = &v2_request {
                                        let response: v2::ResumeSessionResponse =
                                            request_with_timeout(
                                                &connection,
                                                v2_request.clone(),
                                                request_timeout,
                                                "session/resume",
                                            )
                                            .await?;
                                        let mut converted = ee_agent_protocol::ResumeSessionResponse::new();
                                        let options =
                                            crate::v2_updates::config_options_to_v1(
                                                response.config_options,
                                            );
                                        if !options.is_empty() {
                                            converted = converted.config_options(options);
                                        }
                                        Ok::<_, AgentError>(converted)
                                    } else {
                                        request_with_timeout(
                                            &connection,
                                            request,
                                            request_timeout,
                                            "session/resume",
                                        )
                                        .await
                                    }
                                } => result,
                            };
                            let _ = tx.send(result);
                        }));
                    }
                    ConnectionCommand::CloseSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/close").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetMode { request, tx } => {
                        if is_v2 {
                            // v2 removed `session/set_mode`; modes are config
                            // options (see `AgentThread::set_mode`).
                            let _ = tx.send(Err(AgentError::CapabilityUnsupported {
                                method: "session/set_mode".into(),
                            }));
                            continue;
                        }
                        let result = request_with_timeout(&connection, request, request_timeout, "session/set_mode").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetConfigOption { request, tx } => {
                        let result = if is_v2 {
                            // v2 keeps `session/set_config_option` (modes are
                            // config options now); the v1-typed request and
                            // response convert to the v2 types (identical wire
                            // shapes).
                            match v2_set_config_option_request(&request) {
                                Err(error) => Err(error),
                                Ok(v2_request) => {
                                    match request_with_timeout(
                                        &connection,
                                        v2_request,
                                        request_timeout,
                                        "session/set_config_option",
                                    )
                                    .await
                                    {
                                        Err(error) => Err(error),
                                        Ok(response) => serde_json::to_value(response)
                                            .map_err(|error| {
                                                AgentError::UnexpectedResponse(error.to_string())
                                            })
                                            .and_then(|value| {
                                                serde_json::from_value(value).map_err(|error| {
                                                    AgentError::UnexpectedResponse(error.to_string())
                                                })
                                            }),
                                    }
                                }
                            }
                        } else {
                            request_with_timeout(
                                &connection,
                                request,
                                request_timeout,
                                "session/set_config_option",
                            )
                            .await
                        };
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetModel { request, tx } => {
                        let result = request_with_timeout(
                            &connection,
                            request,
                            request_timeout,
                            "session/set_model",
                        )
                        .await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::Authenticate { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "authenticate").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::Logout { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "logout").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::Prompt { request, cancel, tx } => {
                        let session_id = request.session_id.clone();
                        if active_prompts.contains_key(&session_id)
                            || queued_sessions.contains(&session_id)
                        {
                            let _ = tx.send(Err(AgentError::TurnAlreadyRunning));
                            continue;
                        }
                        if active_prompts.len() >= max_concurrent_prompts {
                            queued_sessions.insert(session_id.clone());
                            queued_prompts.push_back(PendingPrompt { request, cancel, tx });
                            let _ = inner.events.send(AgentEvent::TurnQueued {
                                session_id,
                                position: queued_prompts.len(),
                            });
                            continue;
                        }
                        start_prompt_task(
                            &connection,
                            &mut prompt_tasks,
                            &mut active_prompts,
                            PendingPrompt { request, cancel, tx },
                            &prompt_shutdown_rx,
                            &state_rx,
                            &agent_id,
                            is_v2,
                            &inner,
                        );
                    }
                    ConnectionCommand::SteerPrompt { request, tx } => {
                        start_steer_task(
                            &connection,
                            &mut steering_tasks,
                            PendingPrompt { request, cancel: watch::channel(false).1, tx },
                            &prompt_shutdown_rx,
                            &state_rx,
                            &agent_id,
                            is_v2,
                        );
                    }
                    ConnectionCommand::CancelSession { session_id } => {
                        if let Some(position) = queued_prompts
                            .iter()
                            .position(|pending| pending.request.session_id == session_id)
                        {
                            if let Some(pending) = queued_prompts.remove(position) {
                                queued_sessions.remove(&session_id);
                                let _ = pending.tx.send(Err(AgentError::Cancelled));

                            }
                            continue;
                        }
                        let _ = connection.send_notification(CancelNotification::new(session_id));
                        // Turn cancel closes every logical MCP connection on
                        // this connection.
                        inner.mcp.close_all();
                    }
                    ConnectionCommand::Close => break,
                }
            }
        }
    }

    rx.close();
    let _ = prompt_shutdown_tx.send(true);
    let _ = lifecycle_shutdown_tx.send(true);
    while let Some(pending) = queued_prompts.pop_front() {
        let _ = pending.tx.send(Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() }));
    }
    queued_sessions.clear();

    while let Some(completion) = prompt_tasks.next().await {
        finish_prompt_task(completion, &mut active_prompts);
    }
    while let Some(completion) = steering_tasks.next().await {
        let _ = completion.tx.send(completion.result);
    }
    while lifecycle_tasks.next().await.is_some() {}
    while let Some(command) = rx.recv().await {
        reject_command_after_shutdown(command, &agent_id);
    }
}

/*
 * Prompt tasks.  v1: the prompt response ends the turn and carries the stop
 * reason.  v2: the response is an insertion acknowledgment; the turn ends when
 * the idle `state_update` arrives (registered through `inner.v2_turn_ends`),
 * and the stop reason is taken from that update.
 */

#[allow(clippy::too_many_arguments)]
fn start_prompt_task(
    connection: &ConnectionTo<AgentRole>,
    prompt_tasks: &mut FuturesUnordered<PromptTask>,
    active_prompts: &mut HashMap<SessionId, RequestId>,
    pending: PendingPrompt,
    prompt_shutdown: &watch::Receiver<bool>,
    state_rx: &watch::Receiver<AgentConnectionState>,
    agent_id: &str,
    is_v2: bool,
    inner: &Arc<AgentConnectionInner>,
) {
    let session_id = pending.request.session_id.clone();
    if is_v2 {
        let v2_request = v2_prompt_request(&pending.request);
        // The turn-end oneshot is registered *before* the request goes out:
        // the SDK may dispatch follow-up notifications (chunks, the idle
        // `state_update`) before the prompt response itself is delivered to
        // the blocked task, so waiting for the ack first would miss a fast
        // idle and hang the turn.
        let (end_tx, end_rx) = oneshot::channel();
        let replace_session = session_id.clone();
        if let Some(previous) = inner
            .v2_turn_ends
            .lock()
            .expect("v2 turn ends poisoned")
            .insert(replace_session, end_tx)
        {
            // One active prompt per session is enforced by the driver; a
            // duplicate registration means a bug.
            drop(previous);
            tracing::warn!(%session_id, "duplicate v2 turn-end registration for one session");
        }
        let sent = connection.send_request(v2_request);
        let request_id = sent.id().clone();
        active_prompts.insert(session_id.clone(), request_id.clone());
        prompt_tasks.push(Box::pin(run_prompt_task_v2(
            sent,
            session_id,
            request_id,
            pending.cancel,
            prompt_shutdown.clone(),
            state_rx.clone(),
            agent_id.to_string(),
            connection.clone(),
            inner.clone(),
            end_rx,
            pending.tx,
        )));
        return;
    }
    let sent = connection.send_request(pending.request);
    let request_id = sent.id().clone();
    active_prompts.insert(session_id.clone(), request_id.clone());
    prompt_tasks.push(Box::pin(run_prompt_task(
        sent,
        session_id,
        request_id,
        pending.cancel,
        prompt_shutdown.clone(),
        state_rx.clone(),
        agent_id.to_string(),
        connection.clone(),
        pending.tx,
    )));
}

#[allow(clippy::too_many_arguments)]
fn start_steer_task(
    connection: &ConnectionTo<AgentRole>,
    steering_tasks: &mut FuturesUnordered<PromptTask>,
    pending: PendingPrompt,
    prompt_shutdown: &watch::Receiver<bool>,
    state_rx: &watch::Receiver<AgentConnectionState>,
    agent_id: &str,
    is_v2: bool,
) {
    let session_id = pending.request.session_id.clone();
    if is_v2 {
        // A delivery prompt is acknowledged immediately on v2; the joined
        // turn's completion arrives through `session/update`, so waiting for
        // the session's single turn-end waiter would steal the main turn's
        // signal.  The acknowledgment is the delivery receipt.
        let request = v2_prompt_request(&pending.request);
        let sent = connection.send_request(request);
        let request_id = sent.id().clone();
        steering_tasks.push(Box::pin(run_steer_task_v2(
            sent,
            session_id,
            request_id,
            prompt_shutdown.clone(),
            state_rx.clone(),
            agent_id.to_string(),
            pending.tx,
        )));
        return;
    }
    let sent = connection.send_request(pending.request);
    let request_id = sent.id().clone();
    // A steer/queue delivery is not cancellable from the host's turn
    // reservation; `session/cancel` on the agent closes it.
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    steering_tasks.push(Box::pin(run_prompt_task(
        sent,
        session_id,
        request_id,
        cancel_rx,
        prompt_shutdown.clone(),
        state_rx.clone(),
        agent_id.to_string(),
        connection.clone(),
        pending.tx,
    )));
}

/// v2 delivery prompt: the ack is the terminal result for the caller; the
/// joined turn keeps streaming through the regular event path.
async fn run_steer_task_v2(
    sent: ee_agent_protocol::SentRequest<v2::PromptResponse>,
    session_id: SessionId,
    request_id: RequestId,
    prompt_shutdown: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    agent_id: String,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
) -> PromptTaskCompletion {
    let shutdown = wait_for_true(prompt_shutdown);
    let result = tokio::select! {
        biased;
        () = shutdown => Err(AgentError::ConnectionClosed { agent_id }),
        response = sent.block_task() => match response {
            Ok(_ack) => Ok(PromptResponse::new(StopReason::EndTurn)),
            Err(_error) if matches!(*state_rx.borrow(), AgentConnectionState::Closed(_)) => {
                Err(AgentError::ConnectionClosed { agent_id })
            }
            Err(error) => Err(AgentError::Rpc(error)),
        },
    };
    PromptTaskCompletion { session_id, request_id, result, tx }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_queued_prompts(
    connection: &ConnectionTo<AgentRole>,
    prompt_tasks: &mut FuturesUnordered<PromptTask>,
    active_prompts: &mut HashMap<SessionId, RequestId>,
    queued_prompts: &mut VecDeque<PendingPrompt>,
    queued_sessions: &mut HashSet<SessionId>,
    max_concurrent_prompts: usize,
    prompt_shutdown: &watch::Receiver<bool>,
    state_rx: &watch::Receiver<AgentConnectionState>,
    agent_id: &str,
    inner: &Arc<AgentConnectionInner>,
    is_v2: bool,
) {
    while active_prompts.len() < max_concurrent_prompts {
        let Some(pending) = queued_prompts.pop_front() else {
            break;
        };
        queued_sessions.remove(&pending.request.session_id);
        let _ = inner
            .events
            .send(AgentEvent::TurnDispatched { session_id: pending.request.session_id.clone() });
        start_prompt_task(
            connection,
            prompt_tasks,
            active_prompts,
            pending,
            prompt_shutdown,
            state_rx,
            agent_id,
            is_v2,
            inner,
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_prompt_task(
    sent: ee_agent_protocol::SentRequest<PromptResponse>,
    session_id: SessionId,
    request_id: RequestId,
    cancel: watch::Receiver<bool>,
    prompt_shutdown: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    agent_id: String,
    connection: ConnectionTo<AgentRole>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
) -> PromptTaskCompletion {
    let cancelled = wait_for_true(cancel);
    let shutdown = wait_for_true(prompt_shutdown);
    let result = tokio::select! {
        biased;
        () = shutdown => {
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
        }
        () = cancelled => {
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::Cancelled)
        }
        response = sent.block_task() => {
            match response {
                Ok(response) => Ok(response),
                Err(_error) if matches!(*state_rx.borrow(), AgentConnectionState::Closed(_)) => {
                    Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                }
                Err(error) => {
                    tracing::warn!(agent_id, ?error, "prompt block_task failed");
                    Err(AgentError::Rpc(error))
                }
            }
        }
    };
    PromptTaskCompletion { session_id, request_id, result, tx }
}

/// v2 prompt: the acknowledgment response is not the end of the turn.  Once
/// the ack arrives, the turn stays open — and `active_prompts` stays
/// occupied — until the idle `state_update` resolves `inner.v2_turn_ends`
/// with the agent's stop reason.
///
/// The response and notifications share one transport and are processed in
/// stream order, so the ack always resolves before the idle update that
/// follows it in-band; a defensive debug log covers the impossible case where
/// a stop reason raced ahead of registration.
#[allow(clippy::too_many_arguments)]
async fn run_prompt_task_v2(
    sent: ee_agent_protocol::SentRequest<v2::PromptResponse>,
    session_id: SessionId,
    request_id: RequestId,
    cancel: watch::Receiver<bool>,
    prompt_shutdown: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    agent_id: String,
    connection: ConnectionTo<AgentRole>,
    inner: Arc<AgentConnectionInner>,
    // Resolves with the turn's stop reason when the idle `state_update`
    // arrives; created in `start_prompt_task` before the request is sent /
    // notifications can be dispatched.
    end_rx: oneshot::Receiver<StopReason>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
) -> PromptTaskCompletion {
    let cancelled = wait_for_true(cancel.clone());
    let shutdown = wait_for_true(prompt_shutdown.clone());
    // Fresh futures for the post-ack wait: the outer select consumed its own
    // copies above.
    let stop_cancelled = wait_for_true(cancel);
    let stop_shutdown = wait_for_true(prompt_shutdown);
    let result = tokio::select! {
        biased;
        () = shutdown => {
            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
        }
        () = cancelled => {
            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::Cancelled)
        }
        response = sent.block_task() => {
            match response {
                Ok(_ack) => {
                    // Turn accepted; the turn ends when the inbound idle
                    // `state_update` fires the oneshot registered before the
                    // request was sent.
                    let stop = tokio::select! {
                        () = stop_shutdown => {
                            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
                            let _ = connection.send_cancel_request(request_id.clone());
                            Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                        }
                        () = stop_cancelled => {
                            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
                            Err(AgentError::Cancelled)
                        }
                        reason = end_rx => {
                            match reason {
                                Ok(stop_reason) => {
                                    // v1 stop reasons and v2 stop reasons share
                                    // their variant set; the host API stays on
                                    // the v1-typed PromptResponse.
                                    Ok(PromptResponse::new(stop_reason))
                                }
                                Err(_) => {
                                    tracing::warn!(%session_id, "v2 idle state update never arrived");
                                    Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                                }
                            }
                        }
                    };
                    stop
                }
                Err(error) => {
                    inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
                    if matches!(*state_rx.borrow(), AgentConnectionState::Closed(_)) {
                        Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                    } else {
                        tracing::warn!(agent_id, ?error, "v2 prompt block_task failed");
                        Err(AgentError::Rpc(error))
                    }
                }
            }
        }
    };
    PromptTaskCompletion { session_id, request_id, result, tx }
}

/// v1 session/new request → v2 wire shape (mcp servers gain the `type`
/// discriminator).
fn v2_new_session_request(request: &NewSessionRequest) -> v2::NewSessionRequest {
    let mut converted = v2::NewSessionRequest::new(v2::AbsolutePath::new(request.cwd.clone()))
        .additional_directories(request.additional_directories.clone());
    if !request.mcp_servers.is_empty() {
        converted = converted.mcp_servers(mcp_servers_to_v2(&request.mcp_servers));
    }
    match &request.meta {
        Some(meta) => converted.meta(meta.clone()),
        None => converted,
    }
}

/// v1 session/resume request → v2 wire shape (`replayFrom` stays unset:
/// no replay, matching the v1 resume behavior).
fn v2_resume_session_request(request: &ResumeSessionRequest) -> v2::ResumeSessionRequest {
    let mut converted = v2::ResumeSessionRequest::new(
        v2::SessionId::new(request.session_id.0.clone()),
        v2::AbsolutePath::new(request.cwd.clone()),
    )
    .additional_directories(request.additional_directories.clone());
    if !request.mcp_servers.is_empty() {
        converted = converted.mcp_servers(mcp_servers_to_v2(&request.mcp_servers));
    }
    match &request.meta {
        Some(meta) => converted.meta(meta.clone()),
        None => converted,
    }
}

/// v1 prompt request → v2 typed request (identical wire shape; the response
/// type differs between versions, so the typed request must too).
fn v2_prompt_request(request: &PromptRequest) -> v2::PromptRequest {
    let prompt = request
        .prompt
        .iter()
        .filter_map(|block| serde_json::from_value(serde_json::to_value(block).ok()?).ok())
        .collect::<Vec<v2::ContentBlock>>();
    if prompt.len() != request.prompt.len() {
        tracing::warn!("dropped v1 prompt blocks with no v2 representation");
    }
    let converted =
        v2::PromptRequest::new(v2::SessionId::new(request.session_id.0.clone()), prompt);
    match &request.meta {
        Some(meta) => converted.meta(meta.clone()),
        None => converted,
    }
}

/// v1 `session/set_config_option` request → v2 typed request (identical wire
/// shape; the v1 `ValueId` maps to the v2 `Id` value).  Fails closed for v1
/// values without a v2 representation.
fn v2_set_config_option_request(
    request: &SetSessionConfigOptionRequest,
) -> Result<v2::SetSessionConfigOptionRequest, AgentError> {
    let value = match &request.value {
        ee_agent_protocol::SessionConfigOptionValue::ValueId { value } => {
            v2::SessionConfigOptionValue::Id {
                value: v2::SessionConfigValueId::new(value.0.clone()),
            }
        }
        ee_agent_protocol::SessionConfigOptionValue::Boolean { value } => {
            v2::SessionConfigOptionValue::Boolean { value: *value }
        }
        other => {
            return Err(AgentError::invalid_params(format!(
                "config option value has no v2 representation: {other:?}"
            )));
        }
    };
    Ok(v2::SetSessionConfigOptionRequest::new(
        v2::SessionId::new(request.session_id.0.clone()),
        request.config_id.0.clone(),
        value,
    ))
}

/// v2 `Implementation` → the v1-typed implementation the host stores.  The
/// field set is identical across versions.
pub(super) fn implementation_to_v1(implementation: &v2::Implementation) -> Implementation {
    let mut converted =
        Implementation::new(implementation.name.clone(), implementation.version.clone());
    if let Some(title) = &implementation.title {
        converted = converted.title(title.clone());
    }
    converted
}

/// Maps the v2 stop reason onto the v1-typed value the host API uses.
pub(super) fn stop_reason_to_v1(reason: v2::StopReason) -> StopReason {
    match reason {
        v2::StopReason::EndTurn => StopReason::EndTurn,
        v2::StopReason::MaxTokens => StopReason::MaxTokens,
        v2::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        v2::StopReason::Refusal => StopReason::Refusal,
        v2::StopReason::Cancelled => StopReason::Cancelled,
        other => {
            tracing::warn!(?other, "unknown v2 stop reason mapped to refusal");
            StopReason::Refusal
        }
    }
}

async fn wait_for_true(mut signal: watch::Receiver<bool>) {
    if *signal.borrow() {
        return;
    }
    loop {
        match signal.changed().await {
            Ok(()) if *signal.borrow() => return,
            Ok(()) => {}
            Err(_) => std::future::pending().await,
        }
    }
}

fn finish_prompt_task(
    completion: PromptTaskCompletion,
    active_prompts: &mut HashMap<SessionId, RequestId>,
) {
    if active_prompts.get(&completion.session_id) == Some(&completion.request_id) {
        active_prompts.remove(&completion.session_id);
    }
    let _ = completion.tx.send(completion.result);
}

fn reject_command_after_shutdown(command: ConnectionCommand, agent_id: &str) {
    let error = || AgentError::ConnectionClosed { agent_id: agent_id.to_string() };
    match command {
        ConnectionCommand::NewSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::LoadSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::ListSessions { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::DeleteSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::ResumeSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::CloseSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::SetMode { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::SetConfigOption { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::SetModel { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::Authenticate { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::Logout { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::Prompt { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::SteerPrompt { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::CancelSession { .. } | ConnectionCommand::Close => {}
    }
}

/// Runs one non-prompt ACP request with a bounded timeout.
async fn request_with_timeout<T: ee_agent_protocol::JsonRpcRequest>(
    connection: &ConnectionTo<AgentRole>,
    request: T,
    timeout: Duration,
    method: &str,
) -> Result<T::Response, AgentError> {
    let sent = connection.send_request(request);
    tokio::time::timeout(timeout, sent.block_task())
        .await
        .map_err(|_| AgentError::RequestTimeout { method: method.to_string() })?
        .map_err(AgentError::Rpc)
}
