//! Per-agent ACP protocol override reaches the real `initialize` handshake.
//!
//! `AgentManagerConfig::protocols` is the runtime seam behind
//! `[agents.servers.<id>] protocol = "v2"`; these tests prove the selected
//! version is applied to the connection options and that agents without an
//! override keep the v1 default.

use std::sync::{Arc, Mutex};

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport};
use ee_agent_host::{
    AgentManager, AgentManagerConfig, AgentProcessConfig, DenyAllHandler, FakeTransportFactory,
};
use ee_agent_protocol::ProtocolVersion;
use serde_json::json;
use tokio::sync::mpsc;

/// One-shot fake transport factory that keeps the spawned agent handle for
/// request assertions.
#[derive(Clone)]
struct OneFake {
    script: FakeAgentScript,
    handle: Arc<Mutex<Option<FakeAgent>>>,
}

impl OneFake {
    fn new(script: FakeAgentScript) -> Self {
        Self { script, handle: Arc::new(Mutex::new(None)) }
    }

    fn handle(&self) -> FakeAgent {
        self.handle.lock().expect("fake handle poisoned").clone().expect("fake agent spawned")
    }
}

impl FakeTransportFactory for OneFake {
    fn build(&self) -> FakeAgentTransport {
        let (agent, transport) = FakeAgent::spawn(self.script.clone());
        *self.handle.lock().expect("fake handle poisoned") = Some(agent);
        transport
    }
}

fn v1_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
}

fn v2_script() -> FakeAgentScript {
    FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 2,
        "info": { "name": "fake-agent-v2", "version": "1.0.0" },
        "capabilities": { "session": {} },
        "authMethods": [],
    }))
}

#[tokio::test]
async fn per_agent_protocol_override_selects_v2_initialize() {
    let v2 = OneFake::new(v2_script());
    let v1 = OneFake::new(v1_script());
    let mut config = AgentManagerConfig::default();
    config.agents.insert("v2-agent".into(), AgentProcessConfig::new("unused-v2"));
    config.agents.insert("v1-agent".into(), AgentProcessConfig::new("unused-v1"));
    config.protocols.insert("v2-agent".into(), ProtocolVersion::V2);
    config.fake_transports.insert("v2-agent".into(), Arc::new(v2.clone()));
    config.fake_transports.insert("v1-agent".into(), Arc::new(v1.clone()));
    let (events, _rx) = mpsc::unbounded_channel();
    let manager = AgentManager::new(config, Arc::new(DenyAllHandler), events);

    let v2_connection = manager.connection("v2-agent").await.expect("v2 agent connects");
    v2_connection.wait_ready().await.expect("v2 handshake succeeds");
    let v1_connection = manager.connection("v1-agent").await.expect("v1 agent connects");
    v1_connection.wait_ready().await.expect("v1 handshake succeeds");

    let v2_fake = v2.handle();
    let v2_initialize = v2_fake.requests_by_method("initialize").pop().expect("v2 initialize");
    assert_eq!(v2_initialize["params"]["protocolVersion"], 2);
    assert!(v2_initialize["params"].get("clientCapabilities").is_none());

    let v1_fake = v1.handle();
    let v1_initialize = v1_fake.requests_by_method("initialize").pop().expect("v1 initialize");
    assert_eq!(v1_initialize["params"]["protocolVersion"], 1);
    assert!(
        v1_initialize["params"].get("clientCapabilities").is_some(),
        "v1 must keep the legacy client capability shape"
    );

    v2_connection.close().await;
    v1_connection.close().await;
}

#[test]
fn manager_config_defaults_to_no_protocol_overrides() {
    let config = AgentManagerConfig::default();
    assert!(config.protocols.is_empty());
}
