#![cfg(feature = "test-utils")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{
    AgentManager, AgentManagerConfig, CriticAgentBroker, CriticRevisionObserver,
    ExternalCriticConfig, ExternalCriticTrust, ExternalCriticUnavailable, ExternalCritiqueOutcome,
    ExternalCritiqueRequest, FakeTransportFactory, HandlerCapabilities, RecordingHandler,
};
use ee_agent_orchestrator::{CritiqueReport, CritiqueTarget, ReportEvidence};
use ee_agent_protocol::{ContentBlock, TextContent};
use serde_json::json;
use tokio::sync::{mpsc, watch};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

struct FixedRevision;

impl CriticRevisionObserver for FixedRevision {
    fn current_revision(&self, _worktree_roots: &[PathBuf]) -> Result<String, String> {
        Ok(String::from("rev-1"))
    }
}

fn revision_observer() -> Arc<dyn CriticRevisionObserver> {
    Arc::new(FixedRevision)
}

struct AdvancingRevision(AtomicUsize);

impl CriticRevisionObserver for AdvancingRevision {
    fn current_revision(&self, _worktree_roots: &[PathBuf]) -> Result<String, String> {
        let call = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(if call == 0 { String::from("rev-1") } else { String::from("rev-2") })
    }
}

#[derive(Clone)]
struct ScriptedFake {
    script: FakeAgentScript,
    handle: Arc<Mutex<Option<FakeAgent>>>,
}

impl ScriptedFake {
    fn new(script: FakeAgentScript) -> Self {
        Self { script, handle: Arc::new(Mutex::new(None)) }
    }

    fn spawned(&self) -> bool {
        self.handle.lock().expect("fake handle poisoned").is_some()
    }

    async fn agent(&self) -> FakeAgent {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if let Some(agent) = self.handle.lock().expect("fake handle poisoned").clone() {
                    break agent;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake agent spawned")
    }
}

impl FakeTransportFactory for ScriptedFake {
    fn build(&self) -> FakeAgentTransport {
        let (agent, transport) = FakeAgent::spawn(self.script.clone());
        *self.handle.lock().expect("fake handle poisoned") = Some(agent);
        transport
    }
}

fn root_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "root-s" }))
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "root-s",
            wire::agent_message_chunk(
                "root-m",
                "Accepted verified critic evidence; plan unchanged.",
            ),
        ))
        .respond(json!({ "stopReason": "end_turn" }))
}

fn critic_script(raw_report: String) -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "close": {} } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "critic-s" }))
        .wait_for("session/prompt")
        .emit(wire::session_update("critic-s", wire::agent_message_chunk("critic-m", &raw_report)))
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/close")
        .respond(json!({}))
}

fn manager(root: &ScriptedFake, critic: &ScriptedFake) -> (AgentManager, Arc<RecordingHandler>) {
    let mut config = AgentManagerConfig {
        agents: BTreeMap::from([
            ("root".into(), ee_agent_host::AgentProcessConfig::new("root-unused")),
            ("critic".into(), ee_agent_host::AgentProcessConfig::new("critic-unused")),
        ]),
        ee_proxy_enabled: true,
        fake_transports: BTreeMap::new(),
    };
    config.fake_transports.insert("root".into(), Arc::new(root.clone()));
    config.fake_transports.insert("critic".into(), Arc::new(critic.clone()));
    let handler = Arc::new(RecordingHandler::new(HandlerCapabilities::all()));
    let (events, _rx) = mpsc::unbounded_channel();
    (AgentManager::new(config, handler.clone(), events), handler)
}

fn request(automatic: bool) -> ExternalCritiqueRequest {
    ExternalCritiqueRequest {
        root_agent_id: "root".into(),
        target: CritiqueTarget::Implementation,
        untrusted_context: "changed file: src/lib.rs\nvalidation: cargo test passed".into(),
        observed_evidence: ReportEvidence::default(),
        worktree_roots: vec![PathBuf::from("/work"), PathBuf::from("/readonly-extra")],
        automatic,
        revision: "rev-1".into(),
    }
}

fn config(trust: ExternalCriticTrust) -> ExternalCriticConfig {
    ExternalCriticConfig { agent_id: "critic".into(), trust, require_independent_agent: true }
}

#[tokio::test]
async fn two_agents_stay_separate_and_verified_report_can_feed_root_synthesis() {
    let root = ScriptedFake::new(root_script());
    let clean =
        serde_json::to_string(&CritiqueReport::clean(CritiqueTarget::Implementation)).unwrap();
    let critic = ScriptedFake::new(critic_script(clean));
    let (manager, _handler) = manager(&root, &critic);

    let root_thread = manager
        .new_session("root", vec![PathBuf::from("/work")], Vec::new(), None)
        .await
        .expect("root session");
    assert!(!critic.spawned(), "critic stays lazy until selected");

    let broker = CriticAgentBroker::new(
        &manager,
        config(ExternalCriticTrust::HostForwardedReadOnly),
        revision_observer(),
    )
    .expect("broker");
    let preview = broker.preview();
    assert_eq!(preview.agent_id, "critic");
    assert!(preview.extra_model_call);
    assert_eq!(preview.estimated_cost_micros, None);
    assert!(preview.warning.is_some());
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut critic_request = request(false);
    critic_request.untrusted_context.push_str("\nAPI_KEY=super-secret-value");
    let outcome = broker.critique(critic_request, cancel_rx).await;
    let ExternalCritiqueOutcome::Completed(completed) = outcome else {
        panic!("expected verified report")
    };
    assert_eq!(completed.attribution.root_agent_id, "root");
    assert_eq!(completed.attribution.critic_agent_id, "critic");
    assert!(completed.attribution.warning.is_some(), "manual unsandboxed limitation stays visible");
    assert_eq!(completed.attribution.implementation_name.as_deref(), None);

    root_thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new(
            completed.report.to_json().expect("verified evidence JSON"),
        ))])
        .await
        .expect("root synthesis");
    assert!(root_thread.snapshot().messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, ContentBlock::Text(text) if text.text.contains("plan unchanged"))
        })
    }));

    let root_agent = root.agent().await;
    let critic_agent = critic.agent().await;
    assert_eq!(root_agent.requests_by_method("initialize").len(), 1);
    assert_eq!(critic_agent.requests_by_method("initialize").len(), 1);
    assert_eq!(root_agent.requests_by_method("session/prompt").len(), 1);
    assert_eq!(critic_agent.requests_by_method("session/prompt").len(), 1);
    assert_eq!(critic_agent.requests_by_method("session/close").len(), 1);
    let new_session = &critic_agent.requests_by_method("session/new")[0];
    assert_eq!(new_session["params"]["cwd"], "/work");
    assert_eq!(new_session["params"]["additionalDirectories"], json!(["/readonly-extra"]));
    let prompt = &critic_agent.requests_by_method("session/prompt")[0];
    let text = prompt["params"]["prompt"][0]["text"].as_str().expect("critic prompt text");
    assert!(text.contains("<untrusted_review_context>"));
    assert!(text.contains("<observed_evidence_allowlist>"));
    assert!(!text.contains("API_KEY"));
    assert!(!text.contains("super-secret-value"));

    manager.shutdown().await;
    root_agent.join(TEST_TIMEOUT).await;
    critic_agent.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn malformed_report_is_quarantined_and_ephemeral_session_closes() {
    let root = ScriptedFake::new(root_script());
    let critic = ScriptedFake::new(critic_script("not-json".into()));
    let (manager, _) = manager(&root, &critic);
    let broker = CriticAgentBroker::new(
        &manager,
        config(ExternalCriticTrust::SandboxEnforcedReadOnly),
        revision_observer(),
    )
    .unwrap();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = broker.critique(request(false), cancel_rx).await;
    assert!(matches!(outcome, ExternalCritiqueOutcome::Quarantined { .. }));
    let critic_agent = critic.agent().await;
    assert_eq!(critic_agent.requests_by_method("session/close").len(), 1);
    manager.shutdown().await;
    critic_agent.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn configured_output_limit_quarantines_report_and_closes_session() {
    let root = ScriptedFake::new(root_script());
    let clean =
        serde_json::to_string(&CritiqueReport::clean(CritiqueTarget::Implementation)).unwrap();
    let critic = ScriptedFake::new(critic_script(clean));
    let (manager, _) = manager(&root, &critic);
    let broker = CriticAgentBroker::new(
        &manager,
        config(ExternalCriticTrust::SandboxEnforcedReadOnly),
        revision_observer(),
    )
    .unwrap()
    .with_output_limit(8)
    .unwrap();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let outcome = broker.critique(request(false), cancel_rx).await;
    assert!(matches!(
        outcome,
        ExternalCritiqueOutcome::Quarantined { ref reason, .. }
            if reason == "external critique output exceeds 8 bytes"
    ));
    let critic_agent = critic.agent().await;
    assert_eq!(critic_agent.requests_by_method("session/close").len(), 1);
    manager.shutdown().await;
    critic_agent.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancellation_propagates_and_closes_critic_process() {
    let root = ScriptedFake::new(root_script());
    let critic = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "critic-s" }))
            .wait_for("session/prompt")
            .wait_for("session/cancel"),
    );
    let (manager, _) = manager(&root, &critic);
    let broker = CriticAgentBroker::with_timeout(
        &manager,
        config(ExternalCriticTrust::SandboxEnforcedReadOnly),
        TEST_TIMEOUT,
        revision_observer(),
    )
    .unwrap();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let run = tokio::spawn(async move { broker.critique(request(false), cancel_rx).await });
    let critic_agent = critic.agent().await;
    tokio::time::timeout(TEST_TIMEOUT, async {
        while critic_agent.requests_by_method("session/prompt").is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("critic prompt starts");
    cancel_tx.send(true).unwrap();
    assert!(matches!(run.await.unwrap(), ExternalCritiqueOutcome::Cancelled));
    assert_eq!(critic_agent.requests_by_method("session/cancel").len(), 1);
    manager.shutdown().await;
    critic_agent.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn stale_revision_and_unsafe_workspace_roots_fail_before_process_start() {
    let root = ScriptedFake::new(root_script());
    let clean =
        serde_json::to_string(&CritiqueReport::clean(CritiqueTarget::Implementation)).unwrap();
    let critic = ScriptedFake::new(critic_script(clean));
    let (manager, _) = manager(&root, &critic);
    let broker = CriticAgentBroker::new(
        &manager,
        config(ExternalCriticTrust::SandboxEnforcedReadOnly),
        revision_observer(),
    )
    .unwrap();

    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(request(true), rx).await,
        ExternalCritiqueOutcome::Unavailable(
            ExternalCriticUnavailable::AutomaticRequiresEnforcedReadOnly
        )
    ));
    assert!(!critic.spawned(), "configured trust label alone cannot enable automatic critique");

    let mut stale = request(false);
    stale.revision = "rev-0".into();
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(stale, rx).await,
        ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::InvalidRequest(_))
    ));

    let mut escaped = request(false);
    escaped.worktree_roots = vec![PathBuf::from("/work/../escape")];
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(escaped, rx).await,
        ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::InvalidRequest(_))
    ));

    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    #[cfg(unix)]
    {
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut symlinked = request(false);
        symlinked.worktree_roots = vec![link.clone()];
        let (_tx, rx) = watch::channel(false);
        assert!(matches!(
            broker.critique(symlinked, rx).await,
            ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::InvalidRequest(_))
        ));

        let mut dangling_below_symlink = request(false);
        dangling_below_symlink.worktree_roots = vec![link.join("not-created")];
        let (_tx, rx) = watch::channel(false);
        assert!(matches!(
            broker.critique(dangling_below_symlink, rx).await,
            ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::InvalidRequest(_))
        ));

        let dangling = temp.path().join("dangling");
        std::os::unix::fs::symlink(temp.path().join("missing-target"), &dangling).unwrap();
        let mut dangling_symlink = request(false);
        dangling_symlink.worktree_roots = vec![dangling];
        let (_tx, rx) = watch::channel(false);
        assert!(matches!(
            broker.critique(dangling_symlink, rx).await,
            ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::InvalidRequest(_))
        ));
    }
    assert!(!critic.spawned());
    assert!(
        broker
            .events()
            .iter()
            .all(|event| matches!(event, ee_agent_orchestrator::CriticEvent::Skipped { .. }))
    );
}

#[tokio::test]
async fn revision_change_during_critique_quarantines_verified_output() {
    let root = ScriptedFake::new(root_script());
    let clean =
        serde_json::to_string(&CritiqueReport::clean(CritiqueTarget::Implementation)).unwrap();
    let critic = ScriptedFake::new(critic_script(clean));
    let (manager, _) = manager(&root, &critic);
    let broker = CriticAgentBroker::new(
        &manager,
        config(ExternalCriticTrust::HostForwardedReadOnly),
        Arc::new(AdvancingRevision(AtomicUsize::new(0))),
    )
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(request(false), rx).await,
        ExternalCritiqueOutcome::Quarantined {
            safe_reason: ee_agent_orchestrator::CriticSafeReason::StaleRevision,
            ..
        }
    ));
    assert!(matches!(
        broker.events().last(),
        Some(ee_agent_orchestrator::CriticEvent::Quarantined {
            reason: ee_agent_orchestrator::CriticSafeReason::StaleRevision,
            ..
        })
    ));
    let critic_agent = critic.agent().await;
    manager.shutdown().await;
    critic_agent.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn timeout_is_distinct_from_cancellation_and_emits_terminal_event() {
    let root = ScriptedFake::new(root_script());
    let critic = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "critic-s" }))
            .wait_for("session/prompt")
            .wait_for("session/cancel"),
    );
    let (manager, _) = manager(&root, &critic);
    let broker = CriticAgentBroker::with_timeout(
        &manager,
        config(ExternalCriticTrust::HostForwardedReadOnly),
        Duration::from_millis(20),
        revision_observer(),
    )
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(broker.critique(request(false), rx).await, ExternalCritiqueOutcome::TimedOut));
    assert!(matches!(
        broker.events().last(),
        Some(ee_agent_orchestrator::CriticEvent::Failed {
            reason: ee_agent_orchestrator::CriticSafeReason::Timeout,
            ..
        })
    ));
    let critic_agent = critic.agent().await;
    manager.shutdown().await;
    critic_agent.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn critic_write_attempt_fails_closed_and_unsandboxed_automatic_never_starts() {
    let root = ScriptedFake::new(root_script());
    let clean =
        serde_json::to_string(&CritiqueReport::clean(CritiqueTarget::Implementation)).unwrap();
    let critic = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "critic-s" }))
            .wait_for("session/prompt")
            .emit(json!({
                "jsonrpc": "2.0",
                "id": 700,
                "method": "fs/write_text_file",
                "params": {
                    "sessionId": "critic-s",
                    "path": "/work/owned.txt",
                    "content": "mutation"
                }
            }))
            .wait_for_response(700)
            .emit(wire::session_update("critic-s", wire::agent_message_chunk("critic-m", &clean)))
            .respond(json!({ "stopReason": "end_turn" })),
    );
    let (manager, handler) = manager(&root, &critic);
    let broker = CriticAgentBroker::new(
        &manager,
        config(ExternalCriticTrust::HostForwardedReadOnly),
        revision_observer(),
    )
    .unwrap();

    let mut same_agent = request(false);
    same_agent.root_agent_id = "critic".into();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(same_agent, cancel_rx).await,
        ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::SameAgentRejected(_))
    ));
    assert!(!critic.spawned(), "independent-id rejection stays lazy");

    let mut oversized = request(false);
    oversized.untrusted_context = "x".repeat(ee_agent_host::MAX_EXTERNAL_CRITIC_CONTEXT_BYTES + 1);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(oversized, cancel_rx).await,
        ExternalCritiqueOutcome::Unavailable(ExternalCriticUnavailable::InvalidRequest(_))
    ));
    assert!(!critic.spawned(), "oversized context cannot start critic process");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let skipped = broker.critique(request(true), cancel_rx).await;
    assert!(matches!(
        skipped,
        ExternalCritiqueOutcome::Unavailable(
            ExternalCriticUnavailable::AutomaticRequiresEnforcedReadOnly
        )
    ));
    assert!(!critic.spawned(), "unsafe automatic mode cannot start critic process");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    assert!(matches!(
        broker.critique(request(false), cancel_rx).await,
        ExternalCritiqueOutcome::Completed(_)
    ));
    let critic_agent = critic.agent().await;
    let denial = critic_agent.response_with_id(700).expect("write denial response");
    assert!(denial.get("error").is_some(), "write must fail: {denial}");
    assert!(handler.seen().is_empty(), "denied mutation never reaches editor handler");
    manager.shutdown().await;
    critic_agent.join(TEST_TIMEOUT).await;
}
