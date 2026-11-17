//! Provider-adapter tests: `/branch` checkpoint surface.
use std::time::Duration;

use ee_agent_acp_server::server::OutboundEvent;
use ee_agent_acp_server::{ClientBridge, UpdateSink};
use ee_agent_protocol::{ContentBlock, SessionId, TextContent};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::checkpoint::{
    IdGeneratorState, OrchestratorCheckpoint, SubagentTreeState, TranscriptSummary,
};

fn plumbing(
    session_id: &SessionId,
) -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        UpdateSink::new_for_test(session_id.clone(), tx.clone()),
        ClientBridge::new_for_test(Duration::from_secs(5), tx),
        rx,
    )
}

fn save_checkpoint(provider: &OrchestratorProvider, session_id: &str) {
    let runtime = provider.session_runtime(session_id).expect("live runtime");
    let checkpoint = OrchestratorCheckpoint::new(
        DEFAULT_IMPLEMENTATION_NAME,
        runtime.config().clone(),
        session_id,
        runtime.tasks(),
        runtime.memory(),
        TranscriptSummary::from_transcript(&[]),
        runtime.budget_snapshot(),
        SubagentTreeState::new(),
        IdGeneratorState::new(),
    )
    .expect("checkpoint builds");
    runtime.checkpoint_store().save(session_id, &checkpoint).expect("checkpoint saves");
}

fn branch_session_id(source: &str, seq: u64) -> String {
    format!("{source}-branch-{seq}")
}

#[tokio::test]
async fn branch_command_forks_session_and_reports_unreplayed_conversation() {
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
    );
    let init = provider.new_session(NewSessionContext::new("/work")).await.expect("session");
    let session_id = init.session_id;
    save_checkpoint(&provider, &session_id.to_string());

    let (sink, client, mut rx) = plumbing(&session_id);
    let (_cancel_tx, cancel) = watch::channel(false);
    let result = provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/branch"))],
            ),
            sink,
            client,
            cancel,
        )
        .await
        .expect("branch succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    let branch_id = branch_session_id(&session_id.to_string(), 1);
    assert!(provider.session_state(&branch_id).is_some(), "branched session is registered");
    let mut summary = None;
    while let Ok(OutboundEvent::Update { update, .. }) = rx.try_recv() {
        if let SessionUpdate::AgentMessageChunk(chunk) = *update
            && chunk.message_id.as_ref().is_some_and(|id| id.0.as_ref() == "branch")
            && let ContentBlock::Text(text) = &chunk.content
        {
            summary = Some(text.text.clone());
        }
    }
    let summary = summary.expect("branch summary emitted");
    assert!(summary.contains(&branch_id), "{summary}");
    assert!(summary.contains("\"conversation_unreplayed\":true"), "{summary}");
}

#[tokio::test]
async fn prompt_retains_normalized_transcript_without_prompt_planes() {
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig::default(),
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("hi").completed()])),
    );
    let init = provider.new_session(NewSessionContext::new("/work")).await.expect("session");
    let session_id = init.session_id;

    let (sink, client, _rx) = plumbing(&session_id);
    let (_cancel_tx, cancel) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("hello"))],
            ),
            sink,
            client,
            cancel,
        )
        .await
        .expect("prompt completes");

    let transcript = provider.session_transcript(&session_id.to_string()).expect("transcript");
    assert!(
        transcript
            .iter()
            .any(|message| message.role == ModelRole::User
                && message.text_content().contains("hello"))
    );
    assert!(transcript.iter().any(|message| message.role == ModelRole::Assistant));
    assert!(
        transcript.iter().all(|message| message.role != ModelRole::System),
        "rebuilt prompt planes stay out of retained history"
    );
}

#[tokio::test]
async fn branch_command_rejects_missing_checkpoint_without_mutating_session() {
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
    );
    let init = provider.new_session(NewSessionContext::new("/work")).await.expect("session");
    let session_id = init.session_id;

    let (sink, client, _rx) = plumbing(&session_id);
    let (_cancel_tx, cancel) = watch::channel(false);
    let error = provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/branch 9"))],
            ),
            sink,
            client,
            cancel,
        )
        .await
        .expect_err("missing checkpoint is rejected");
    assert!(error.to_string().contains("not found"), "{error}");
    assert!(provider.session_state(&session_id.to_string()).is_some());
    assert!(
        provider.session_state(&branch_session_id(&session_id.to_string(), 9)).is_none(),
        "no branch session was registered"
    );
}

#[tokio::test]
async fn branch_command_rejects_malformed_sequence() {
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
    );
    let init = provider.new_session(NewSessionContext::new("/work")).await.expect("session");
    let session_id = init.session_id;

    let (sink, client, _rx) = plumbing(&session_id);
    let (_cancel_tx, cancel) = watch::channel(false);
    let error = provider
        .prompt(
            PromptContext::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/branch nope"))],
            ),
            sink,
            client,
            cancel,
        )
        .await
        .expect_err("malformed sequence is rejected");
    assert!(error.to_string().contains("invalid checkpoint sequence"), "{error}");
}

#[tokio::test]
async fn branch_inherits_parent_session_affinity() {
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
    );
    let init = provider.new_session(NewSessionContext::new("/work")).await.expect("session");
    let source = init.session_id.to_string();
    save_checkpoint(&provider, &source);

    let (sink, client, _rx) = plumbing(&init.session_id);
    let (_cancel_tx, cancel) = watch::channel(false);
    provider
        .prompt(
            PromptContext::new(
                init.session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("/branch"))],
            ),
            sink,
            client,
            cancel,
        )
        .await
        .expect("branch succeeds");

    let branch_id = branch_session_id(&source, 1);
    let context = {
        let sessions = provider.sessions.lock().expect("sessions");
        sessions
            .get(&branch_id)
            .expect("branch session is registered")
            .runtime
            .session_context_snapshot()
    };
    assert_eq!(context.session_id, branch_id);
    assert_eq!(context.parent_session_id.as_deref(), Some(source.as_str()));
    assert_eq!(context.affinity(), source, "the branch shares its parent's cache lineage");
    assert!(context.project_id.is_some(), "the project id is derived at construction");
}
