//! Provider-adapter tests: session.
use super::conversation::initial_commands;
use super::conversation::record_agent_message;
use super::conversation::record_user_message;
use super::*;
use ee_agent_acp_server::server::OutboundEvent;

#[tokio::test]
async fn reasoning_effort_config_option_round_trips_through_the_provider() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let config = OrchestratorProviderConfig {
        reasoning_effort: Some(ReasoningEffortControl::new(
            vec![String::from("low"), String::from("high")],
            None,
        )),
        ..OrchestratorProviderConfig::default()
    };
    let provider = OrchestratorProvider::with_policy(config, model, PolicyEngine::default());

    let init = provider.new_session(NewSessionContext::new("/work")).await.expect("session starts");
    let advertised = init.config_options.expect("effort is advertised");
    assert_eq!(advertised.len(), 1);
    assert_eq!(advertised[0].id.0.as_ref(), "effort");

    let updated = provider
        .set_config_option(SetConfigOptionContext::new(
            init.session_id.clone(),
            "effort",
            ee_agent_protocol::SessionConfigOptionValue::value_id("high"),
        ))
        .await
        .expect("declared effort applies");
    let ee_agent_protocol::SessionConfigKind::Select(select) = &updated[0].kind else {
        panic!("effort stays a select option");
    };
    assert_eq!(select.current_value.0.as_ref(), "high");

    let error = provider
        .set_config_option(SetConfigOptionContext::new(
            init.session_id.clone(),
            "effort",
            ee_agent_protocol::SessionConfigOptionValue::value_id("ultra"),
        ))
        .await
        .expect_err("undeclared effort is rejected");
    assert!(error.to_string().contains("unsupported reasoning effort"), "{error}");
}

#[test]
fn orchestrated_sessions_advertise_rubber_duck_without_starting_models() {
    let commands = initial_commands(false);
    assert_eq!(
        commands.iter().map(|command| command.name.as_str()).collect::<Vec<_>>(),
        vec![
            COMPACT_COMMAND_NAME,
            PRIMARY_AGENT_COMMAND_NAME,
            RUBBER_DUCK_COMMAND_NAME,
            REFLECT_COMMAND_NAME,
            ee_agent_protocol::HARNESS_COMMAND_NAME,
            ADOPT_COMMAND_NAME,
            BRANCH_COMMAND_NAME,
            REVERT_COMMAND_NAME,
            DIFF_COMMAND_NAME,
            EXPORT_SESSION_COMMAND_NAME,
            IMPORT_SESSION_COMMAND_NAME,
        ]
    );
    let recovery_commands = initial_commands(true);
    assert_eq!(
        recovery_commands.iter().map(|command| command.name.as_str()).collect::<Vec<_>>(),
        vec![
            COMPACT_COMMAND_NAME,
            PRIMARY_AGENT_COMMAND_NAME,
            RUBBER_DUCK_COMMAND_NAME,
            REFLECT_COMMAND_NAME,
            ee_agent_protocol::HARNESS_COMMAND_NAME,
            ADOPT_COMMAND_NAME,
            BRANCH_COMMAND_NAME,
            REVERT_COMMAND_NAME,
            DIFF_COMMAND_NAME,
            EXPORT_SESSION_COMMAND_NAME,
            IMPORT_SESSION_COMMAND_NAME,
            DISCARD_COMMAND_NAME,
            ee_agent_protocol::RESUME_COMMAND_NAME,
        ]
    );
}

#[test]
fn provider_registry_requires_default_adapter() {
    let result = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        crate::model_registry::ModelRegistry::new(),
        PolicyEngine::default(),
    );
    assert!(matches!(
        result,
        Err(ProviderError::BackendFailure(reason)) if reason.contains("no default adapter")
    ));
}

fn switched_model_plumbing() -> (
    UpdateSink,
    ClientBridge,
    tokio::sync::mpsc::UnboundedReceiver<ee_agent_acp_server::server::OutboundEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
        ClientBridge::new_for_test(std::time::Duration::from_secs(5), tx),
        rx,
    )
}

#[tokio::test]
async fn mid_session_model_switch_keeps_the_transcript_and_routes_the_next_turn() {
    let default =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("first answer").completed()]));
    let switched =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("second answer").completed()]));
    let mut registry = crate::model_registry::ModelRegistry::new();
    registry
        .register(crate::model_registry::DEFAULT_MODEL_ID, default.clone())
        .expect("default registered");
    registry.register("model-b", switched.clone()).expect("model-b registered");
    let provider = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        registry,
        PolicyEngine::default(),
    )
    .expect("provider builds");
    let workspace = tempfile::tempdir().expect("temp workspace");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");
    let session_id = init.session_id.clone();

    let (sink, client, _rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("hello"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("first turn completes");
    assert_eq!(default.call_count(), 1);

    provider
        .set_model(SetModelContext::new(session_id.clone(), "model-b"))
        .await
        .expect("idle session switches models");

    let (sink, client, _rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("again"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("second turn completes");

    assert_eq!(default.call_count(), 1, "the default route is no longer used");
    assert_eq!(switched.call_count(), 1, "the next turn runs on the new route");
    let request = switched.requests().remove(0);
    assert!(
        request.transcript.iter().any(|message| {
            message.role == ModelRole::User
                && message.content.iter().any(|content| {
                    matches!(content, crate::model::ModelContent::Text(text) if text == "hello")
                })
        }),
        "the prior user turn survives the switch"
    );
    assert!(
        request.transcript.iter().any(|message| {
            message.role == ModelRole::Assistant
                && message.content.iter().any(|content| {
                    matches!(content, crate::model::ModelContent::Text(text) if text == "first answer")
                })
        }),
        "the prior assistant turn survives the switch"
    );
}

#[tokio::test]
async fn agent_command_selects_a_user_defined_primary_agent_without_a_model_call() {
    let default = Arc::new(FakeModel::new(Vec::new()));
    let switched = Arc::new(FakeModel::new(Vec::new()));
    let mut registry = crate::model_registry::ModelRegistry::new();
    registry
        .register(crate::model_registry::DEFAULT_MODEL_ID, default.clone())
        .expect("default registered");
    registry.register("model-b", switched.clone()).expect("model-b registered");
    let provider = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        registry,
        PolicyEngine::default(),
    )
    .expect("provider builds");
    let workspace = tempfile::tempdir().expect("temp workspace");
    let agent_path = workspace.path().join(".agents/agents/reviewer.md");
    std::fs::create_dir_all(agent_path.parent().expect("agent parent")).expect("agent dir");
    std::fs::write(
        &agent_path,
        "---\ndescription: Reviewer\nmode: primary\nmodel: model-b\nsteps: 2\n---\nReview the change.\n",
    )
    .expect("agent file");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");
    let session_id = init.session_id.clone();

    let (sink, client, _rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/primary-agent reviewer"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("/agent completes");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(default.call_count(), 0, "commands never call the model");
    assert_eq!(switched.call_count(), 0);

    let sessions = provider.sessions.lock().expect("sessions");
    let session = sessions.get(&session_id.to_string()).expect("session exists");
    assert_eq!(session.runtime.active_user_agent_name().as_deref(), Some("reviewer"));
    assert_eq!(session.runtime.selected_model().as_deref(), Some("model-b"));
    assert_eq!(session.model_label, "model-b");
}

#[tokio::test]
async fn user_command_expands_template_and_advertises_completion() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let workspace = tempfile::tempdir().expect("temp workspace");
    let command_path = workspace.path().join(".agents/commands/release.md");
    std::fs::create_dir_all(command_path.parent().expect("command parent")).expect("mkdir");
    std::fs::write(
        &command_path,
        "---\ndescription: Prepare a release\n---\nRelease $1 now: $ARGUMENTS\n",
    )
    .expect("command file");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");
    let session_id = init.session_id.clone();

    let (sink, client, mut rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/release v1"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("command turn completes");

    let request = model.requests().remove(0);
    let user_text = request
        .transcript
        .iter()
        .filter(|message| message.role == ModelRole::User)
        .map(|message| message.text_content())
        .find(|text| text.contains("Release"))
        .expect("expanded template reaches the model");
    assert!(user_text.contains("Release v1 now: v1"));

    let mut advertised = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let OutboundEvent::Update { update, .. } = event
            && let SessionUpdate::AvailableCommandsUpdate(update) = *update
        {
            advertised =
                update.available_commands.iter().map(|command| command.name.clone()).collect();
        }
    }
    assert!(advertised.iter().any(|name| name == "release"), "completion list: {advertised:?}");
}

#[tokio::test]
async fn subtask_user_command_runs_through_the_subagent_manager() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text(crate::test_support::DELEGATED_HANDOFF_OUTPUT).completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let workspace = tempfile::tempdir().expect("temp workspace");
    let agent_path = workspace.path().join(".agents/agents/auditor.md");
    let command_path = workspace.path().join(".agents/commands/audit.md");
    std::fs::create_dir_all(agent_path.parent().expect("agent parent")).expect("mkdir");
    std::fs::create_dir_all(command_path.parent().expect("command parent")).expect("mkdir");
    std::fs::write(
        &agent_path,
        "---\nmode: subagent\ndescription: Auditor\n---\nAudit the assigned scope.\n",
    )
    .expect("agent file");
    std::fs::write(&command_path, "---\nsubtask: true\nagent: auditor\n---\nAudit $ARGUMENTS\n")
        .expect("command file");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");
    let session_id = init.session_id.clone();

    let (sink, client, mut rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = provider
        .prompt(
            PromptContext::new(
                session_id,
                vec![ContentBlock::Text(TextContent::new("/audit src"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("subtask command completes");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 1, "only the child calls the model");
    let child = model.requests().remove(0);
    assert!(
        child.transcript.iter().any(|message| {
            message.role == ModelRole::System
                && message.text_content().contains("Audit the assigned scope.")
        }),
        "user-defined agent body becomes the child system prompt"
    );
    assert!(child.transcript.iter().any(|message| {
        message.role == ModelRole::User && message.text_content().contains("Audit src")
    }));
    let mut reported = false;
    while let Ok(event) = rx.try_recv() {
        if let OutboundEvent::Update { update, .. } = event
            && let SessionUpdate::AgentMessageChunk(chunk) = *update
            && let ContentBlock::Text(text) = &chunk.content
            && text.text.contains("/audit")
        {
            reported = true;
        }
    }
    assert!(reported, "subtask result is reported back to the parent session");
}

#[tokio::test]
async fn user_command_applies_agent_and_model_before_prompting() {
    let default = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let alternate = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let mut registry = crate::model_registry::ModelRegistry::new();
    registry
        .register(crate::model_registry::DEFAULT_MODEL_ID, default.clone())
        .expect("default registered");
    registry.register("model-b", alternate.clone()).expect("model-b registered");
    let provider = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        registry,
        PolicyEngine::default(),
    )
    .expect("provider builds");
    let workspace = tempfile::tempdir().expect("temp workspace");
    let agent_path = workspace.path().join(".agents/agents/reviewer.md");
    let command_path = workspace.path().join(".agents/commands/release.md");
    std::fs::create_dir_all(agent_path.parent().expect("agent parent")).expect("mkdir");
    std::fs::create_dir_all(command_path.parent().expect("command parent")).expect("mkdir");
    std::fs::write(
        &agent_path,
        "---\nmode: primary\ndescription: Reviewer\n---\nReview the release notes.\n",
    )
    .expect("agent file");
    std::fs::write(
        &command_path,
        "---\nagent: reviewer\nmodel: model-b\n---\nRelease $ARGUMENTS\n",
    )
    .expect("command file");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");
    let session_id = init.session_id.clone();

    let (sink, client, _rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/release v1"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("command turn completes");

    assert_eq!(default.call_count(), 0, "the command switched models before the call");
    let request = alternate.requests().remove(0);
    assert!(
        request.transcript.iter().any(|message| {
            message.role == ModelRole::User
                && message.text_content().contains("Review the release notes.")
        }),
        "the selected agent prompt is in the same turn"
    );
    assert!(request.transcript.iter().any(|message| {
        message.role == ModelRole::User && message.text_content().contains("Release v1")
    }));
    let sessions = provider.sessions.lock().expect("sessions");
    let session = sessions.get(&session_id.to_string()).expect("session exists");
    assert_eq!(session.runtime.active_user_agent_name().as_deref(), Some("reviewer"));
    assert_eq!(session.runtime.selected_model().as_deref(), Some("model-b"));
}

#[tokio::test]
async fn prompt_attachment_image_is_downscaled_before_the_model_call() {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use image::ImageFormat;

    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("seen").completed()]));
    let provider = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig {
            orchestrator: crate::config::OrchestratorConfig {
                prompt_attachments: crate::prompt_attachments::PromptAttachmentConfig {
                    max_image_dimension: 64,
                    ..crate::prompt_attachments::PromptAttachmentConfig::default()
                },
                ..crate::config::OrchestratorConfig::default()
            },
            ..OrchestratorProviderConfig::default()
        },
        {
            let mut registry = crate::model_registry::ModelRegistry::new();
            registry
                .register(crate::model_registry::DEFAULT_MODEL_ID, model.clone())
                .expect("default registered");
            registry
        },
        PolicyEngine::default(),
    )
    .expect("provider builds");
    let workspace = tempfile::tempdir().expect("temp workspace");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");
    let session_id = init.session_id.clone();

    let image = image::RgbaImage::from_pixel(200, 100, image::Rgba([1, 2, 3, 255]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .expect("encode image");
    let mut ctx =
        PromptContext::new(session_id, vec![ContentBlock::Text(TextContent::new("look at this"))]);
    ctx.metadata = Some(
        serde_json::json!({
            "attachments": [{ "dataUrl": format!("data:image/png;base64,{}", BASE64.encode(&bytes)) }]
        })
        .as_object()
        .expect("map")
        .clone(),
    );
    let (sink, client, _rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    provider.prompt(ctx, sink, client, cancel_rx).await.expect("turn completes");

    let request = model.requests().remove(0);
    let crate::model::ModelContent::Image { data, .. } = request
        .transcript
        .iter()
        .flat_map(|message| message.content.iter())
        .find(|content| matches!(content, crate::model::ModelContent::Image { .. }))
        .expect("image reached the model")
    else {
        panic!("expected image content");
    };
    let decoded = image::load_from_memory(&BASE64.decode(data).expect("base64")).expect("png");
    assert!(decoded.width() <= 64 && decoded.height() <= 64, "bounded image");
}

#[tokio::test]
async fn unknown_model_switch_fails_closed_with_the_target_named() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let mut registry = crate::model_registry::ModelRegistry::new();
    registry.register(crate::model_registry::DEFAULT_MODEL_ID, model).expect("default registered");
    let provider = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        registry,
        PolicyEngine::default(),
    )
    .expect("provider builds");
    let workspace = tempfile::tempdir().expect("temp workspace");
    let init = provider
        .new_session(NewSessionContext::new(workspace.path()))
        .await
        .expect("session creates");

    let error = provider
        .set_model(SetModelContext::new(init.session_id, "missing-model"))
        .await
        .expect_err("unknown target is rejected");
    assert!(
        matches!(&error, ProviderError::InvalidRequest(reason) if reason.contains("missing-model")),
        "error names the target: {error:?}"
    );
}
#[test]
fn streamed_agent_chunks_replay_as_one_message_per_turn() {
    let conversation = Arc::new(Mutex::new(Vec::new()));
    record_user_message(&conversation, "hello");
    record_agent_message(&conversation, "Hel");
    record_agent_message(&conversation, "lo");
    record_user_message(&conversation, "next");
    record_agent_message(&conversation, "Done");

    assert_eq!(
        *conversation.lock().expect("conversation"),
        vec![
            ConversationMessage { role: ConversationRole::User, text: "hello".to_string() },
            ConversationMessage { role: ConversationRole::Agent, text: "Hello".to_string() },
            ConversationMessage { role: ConversationRole::User, text: "next".to_string() },
            ConversationMessage { role: ConversationRole::Agent, text: "Done".to_string() },
        ]
    );
}
#[tokio::test]
async fn provider_adapter_surfaces_recoverable_interruption_and_manual_resume() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model, 0);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // Prompt 1: the first model call hangs past the slice; the provider
    // answers with a JSON-RPC error carrying the recoverable payload.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle).await;
    let Response::Error { error, .. } = unwrap_response(frame.clone()) else {
        panic!("expected an error response, got {frame:?}");
    };
    let recoverable = &error.data.as_ref().expect("recoverable error carries data")["recoverable"];
    assert_eq!(recoverable["fault"], "deadline");
    assert_eq!(recoverable["safe_resume"], true);
    assert_eq!(recoverable["resumed_count"], 0);
    assert!(
        recoverable["checkpoint_id"].as_str().is_some(),
        "checkpoint id on the wire: {recoverable}"
    );

    // Prompt 2 with the same prompt resumes from the checkpoint and
    // completes; the pending checkpoint is cleared.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    let (frame, updates) = next_response_with_updates(&handle).await;
    let result = request_result(frame);
    assert_eq!(result["stopReason"], "end_turn", "resumed turn completes: {result}");
    let final_reports = updates
        .iter()
        .filter(|update| {
            update["update"]["messageId"]
                .as_str()
                .is_some_and(|id| id.starts_with("ee-final-response-"))
        })
        .collect::<Vec<_>>();
    assert_eq!(final_reports.len(), 1, "completed resume emits one final report");
    assert!(
        final_reports[0]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("completion: unverified"))
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_auto_resumes_once_and_completes() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model, 1);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // One prompt: the first slice times out, the safe single auto-resume
    // continues from the checkpoint and completes.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let (frame, updates) = next_response_with_updates(&handle).await;
    let result = request_result(frame);
    assert_eq!(result["stopReason"], "end_turn", "auto-resume completes: {result}");
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["update"]["messageId"]
                .as_str()
                .is_some_and(|id| id.starts_with("ee-final-response-")))
            .count(),
        1,
        "auto-resume emits one final report only after terminal completion"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_discard_command_clears_checkpoint() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model, 0);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // Prompt 1 times out with a recoverable error (not asserted here;
    // the discard flow below is the point).
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let _ = next_response_frame(&handle).await;

    // `/discard` drops the pending checkpoint and answers end_turn.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "/discard")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn", "discard answers end_turn: {result}");

    // A later prompt is a fresh turn, not a resume: the full script
    // replays inside the fresh slice.
    handle.send(request(4, "session/prompt", prompt_params(&session_id, "again")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn", "fresh prompt after discard: {result}");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_resume_command_requires_fresh_prompt_after_durable_redaction() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model.clone(), 0);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // Prompt 1 times out and leaves a pending checkpoint.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };

    // `/resume` carries no user content. Durable checkpoints omit the
    // original transcript, so recovery fails closed until caller supplies
    // a fresh prompt or explicitly abandons with `/discard`.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "/resume")));
    let error = request_error(next_response_frame(&handle).await);
    assert!(error.message.contains("omits transcript content"), "{error:?}");
    assert!(model.inner.requests().is_empty(), "resume must not call model blindly");

    handle.shutdown(task).await;
}
#[tokio::test]
async fn provider_adapter_resume_command_without_pending_is_ordinary_prompt() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]));
    let provider = recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*model).clone(),
        )),
        0,
    );
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "/resume")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn", "ordinary turn runs: {result}");
    let requests = model.requests();
    assert!(
        requests.iter().any(|request| {
            request.transcript.iter().any(|message| message.text_content() == "/resume")
        }),
        "without a pending checkpoint /resume reaches the model as text"
    );

    handle.shutdown(task).await;
}
#[tokio::test]
async fn provider_adapter_session_resume_restores_pending_turn_without_replay() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    // Provider A pauses the turn and persists a durable checkpoint.
    let provider_a = durable_recovery_provider(model.clone(), dir.path());
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle_a).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };
    handle_a.shutdown(task_a).await;

    // Provider B (fresh process) restores via session/resume: no replay
    // updates precede its mode advertisement response.
    let provider_b = durable_recovery_provider(model, dir.path());
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(
        1,
        "session/resume",
        json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
    ));
    let result = request_result(handle_b.next_frame().await);
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID, "resume mode: {result}");
    assert!(handle_b.outbound().is_empty(), "no replay updates before the resume response");

    // The next prompt continues the paused turn from the checkpoint.
    handle_b.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_b).await);
    assert_eq!(result["stopReason"], "end_turn", "paused turn resumes after session/resume");

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_session_resume_without_pending_state_is_rejected() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    // Provider A completes the turn: the checkpoint is cleared.
    let provider_a = durable_recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*model).clone(),
        )),
        dir.path(),
    );
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_a).await);
    assert_eq!(result["stopReason"], "end_turn");
    handle_a.shutdown(task_a).await;

    // Provider B has no in-memory state and no pending checkpoint.
    let provider_b = durable_recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*model).clone(),
        )),
        dir.path(),
    );
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(
        1,
        "session/resume",
        json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
    ));
    let error = request_error(handle_b.next_frame().await);
    assert!(
        error.message.contains("no pending checkpoint"),
        "resume without pending state is rejected: {error:?}"
    );

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_load_after_crash_replays_checkpoint_transcript() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    // Provider A pauses the turn; the checkpoint holds the transcript
    // tail (the user message).
    let provider_a = durable_recovery_provider(model.clone(), dir.path());
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle_a).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };
    handle_a.shutdown(task_a).await;

    // Provider B loads from the checkpoint store: the pending turn's
    // transcript tail is replayed as the crash-restore conversation.
    let provider_b = durable_recovery_provider(model, dir.path());
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(
        1,
        "session/load",
        json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
    ));
    let mut replayed_user_texts = Vec::new();
    let mut commands_seen = false;
    let mut response = None;
    for _ in 0..10 {
        let frame = handle_b.next_frame_real().await;
        match &frame {
            RawJsonRpcMessage::Notification(update) => {
                let params = raw_params_to_value(update.params.clone());
                match params["update"]["sessionUpdate"].as_str() {
                    Some("user_message_chunk") => {
                        replayed_user_texts.push(
                            params["update"]["content"]["text"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                    Some("available_commands_update") => commands_seen = true,
                    _ => {}
                }
            }
            RawJsonRpcMessage::Response(_) => {
                response = Some(frame);
                break;
            }
            _ => {}
        }
    }
    assert!(
        replayed_user_texts.is_empty(),
        "crash restore must not replay durable transcript text: {replayed_user_texts:?}"
    );
    assert!(commands_seen, "loaded providers re-advertise their commands");
    let result = request_result(response.expect("load response arrives after replay"));
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID, "load mode: {result}");

    // The loaded session resumes the paused turn on the next prompt.
    handle_b.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_b).await);
    assert_eq!(result["stopReason"], "end_turn", "paused turn resumes after crash load");

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_initialize_advertises_resume_capability_only_with_durable_recovery() {
    let plain = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), plain.clone());
    let (handle, task) = spawn_server(provider);
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        Value::Null,
        "no resume advertisement without recovery"
    );
    handle.shutdown(task).await;

    let recovered = Arc::new(FakeModel::new(Vec::new()));
    let provider = recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*recovered).clone(),
        )),
        0,
    );
    let (handle, task) = spawn_server(provider);
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        Value::Null,
        "memory-only recovery never implies crash-resumable ACP state"
    );
    handle.shutdown(task).await;

    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let provider = durable_recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            FakeModel::new(Vec::new()),
        )),
        dir.path(),
    );
    let (handle, task) = spawn_server(provider);
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        json!({}),
        "durable recovery advertises session/resume"
    );
    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_new_session_avoids_durable_checkpoint_collision() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    // Provider A creates `session-1` and pauses it (durable checkpoint).
    let provider_a = durable_recovery_provider(model.clone(), dir.path());
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    assert_eq!(session_id, "session-1");
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle_a).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };
    handle_a.shutdown(task_a).await;

    // Provider B (fresh process) must not shadow the reconnected
    // `session-1` when creating a new session.
    let provider_b = durable_recovery_provider(model, dir.path());
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(1, "session/new", session_new_params("/work")));
    let result = request_result(handle_b.next_frame().await);
    assert_eq!(result["sessionId"], "session-2", "fresh id skips durable ids: {result}");
    // Subsequent allocations stay monotonic past the durable base (drain
    // the first session's command advertisement first).
    let frame = handle_b.next_frame().await;
    let RawJsonRpcMessage::Notification(update) = &frame else {
        panic!("expected the available_commands_update, got {frame:?}");
    };
    assert_eq!(
        raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    handle_b.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle_b.next_frame().await);
    assert_eq!(result["sessionId"], "session-3", "ids stay monotonic: {result}");

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_load_after_process_restart_restores_durable_session() {
    let state_dir = tempfile::TempDir::new().expect("session state dir");
    let workspace = tempfile::TempDir::new().expect("workspace");
    let workspace = workspace.path().to_string_lossy().into_owned();
    let provider_a = OrchestratorProvider::new(
        OrchestratorProviderConfig {
            session_state_dir: Some(state_dir.path().to_path_buf()),
            ..OrchestratorProviderConfig::default()
        },
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()])),
    );
    let (handle_a, task_a) = spawn_server(provider_a);
    handle_a.send(request(1, "session/new", session_new_params(&workspace)));
    let session_id = request_result(handle_a.next_frame().await)["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let _ = handle_a.next_frame().await; // available commands update
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_a).await);
    assert_eq!(result["stopReason"], "end_turn");
    // EOF drops provider A without ACP `session/close`, mirroring
    // `/quit_full` stopping its child agent process.
    handle_a.shutdown(task_a).await;

    let provider_b = OrchestratorProvider::new(
        OrchestratorProviderConfig {
            session_state_dir: Some(state_dir.path().to_path_buf()),
            ..OrchestratorProviderConfig::default()
        },
        Arc::new(FakeModel::new(Vec::new())),
    );
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(1, "session/new", session_new_params(&workspace)));
    let new_session = request_result(handle_b.next_frame().await);
    assert_eq!(new_session["sessionId"], "session-2");
    let _ = handle_b.next_frame().await; // available commands update
    handle_b.send(request(
        2,
        "session/load",
        json!({ "sessionId": session_id, "cwd": workspace, "mcpServers": [] }),
    ));
    let mut replayed = Vec::new();
    let mut response = None;
    for _ in 0..10 {
        let frame = handle_b.next_frame_real().await;
        match &frame {
            RawJsonRpcMessage::Notification(update) => {
                let params = raw_params_to_value(update.params.clone());
                let kind = params["update"]["sessionUpdate"].as_str();
                if matches!(kind, Some("user_message_chunk") | Some("agent_message_chunk")) {
                    replayed.push(params["update"]["content"]["text"].clone());
                }
            }
            RawJsonRpcMessage::Response(_) => {
                response = Some(frame);
                break;
            }
            _ => {}
        }
    }
    let result = request_result(response.expect("load response"));
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID);
    assert!(replayed.is_empty(), "durable session load must not replay transcript: {replayed:?}");
    handle_b.shutdown(task_b).await;
}

#[derive(Clone)]
struct MutatingModel {
    root: std::path::PathBuf,
}

impl ModelAdapter for MutatingModel {
    fn complete(
        &self,
        _request: ModelRequest,
        _cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let root = self.root.clone();
        Box::pin(async move {
            for name in ["a.rs", "b.rs", "c.rs"] {
                std::fs::write(root.join(name), format!("fn {name} changed\n"))
                    .expect("mutate workspace");
                // Give each file a distinct mtime-independent content change.
                std::fs::write(root.join(name), format!("changed-{name}\n"))
                    .expect("mutate workspace");
            }
            Ok(ModelResponse::new().text("done").completed())
        })
    }
}

#[tokio::test]
async fn provider_turn_diff_reports_exactly_the_files_the_turn_changed() {
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_path_buf();
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(root.join(name), format!("original-{name}\n")).expect("seed");
    }
    let mut registry = crate::model_registry::ModelRegistry::new();
    registry
        .register(
            crate::model_registry::DEFAULT_MODEL_ID,
            Arc::new(MutatingModel { root: root.clone() }),
        )
        .expect("default registered");
    let provider = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        registry,
        PolicyEngine::default(),
    )
    .expect("provider builds");
    let init = provider.new_session(NewSessionContext::new(&root)).await.expect("session creates");
    let session_id = init.session_id.clone();
    let (sink, client, _rx) = switched_model_plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("edit"))],
            ),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("turn completes");

    let session = provider
        .sessions
        .lock()
        .expect("adapter sessions poisoned")
        .get(&session_id.to_string())
        .expect("session runtime")
        .runtime
        .clone();
    let diff = session
        .workspace_snapshots()
        .turn_diff(&session_id.to_string(), 1, &root)
        .expect("diff reads")
        .expect("turn 1 has a diff");
    let paths: Vec<_> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, vec!["a.rs", "b.rs", "c.rs"]);
    assert!(
        diff.files
            .iter()
            .all(|file| file.change == crate::workspace_snapshot::DiffChange::Modified)
    );
}
