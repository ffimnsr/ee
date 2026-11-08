//! Native transport integration tests against the fixture plugin binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::{ErrorClass, PluginError};
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::{CommandId, PluginId};
use ee_plugin_api::manifest::LaunchTransport;
use ee_plugin_api::wire::PluginToHost;
use ee_plugin_runtime::MemoryContributionSink;
use ee_plugin_runtime::native::{NativeClient, NativeSpec};

const FIXTURE: &str = env!("CARGO_BIN_EXE_ee-plugin-runtime-echo");

#[derive(Default)]
struct RecordingAdapter {
    effects: Mutex<Vec<Effect>>,
}

impl HostAdapter for RecordingAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Editor
    }

    fn submit_effects(
        &self,
        _plugin: Option<&PluginId>,
        effects: Vec<Effect>,
    ) -> Result<(), PluginError> {
        self.effects.lock().unwrap().extend(effects);
        Ok(())
    }

    fn handle_service_request(
        &self,
        _plugin: &PluginId,
        request: &ee_plugin_api::wire::ServiceRequestParams,
    ) -> Result<serde_json::Value, PluginError> {
        if request.service.as_str() == "editor.document@1" && request.method == "snapshot" {
            return Ok(serde_json::json!({"rev": 1, "len": 5}));
        }
        Err(PluginError::Unsupported { feature: "service method" })
    }
}

fn spec(env: BTreeMap<String, String>, rpc_timeout_ms: u64) -> NativeSpec {
    let mut spec = NativeSpec::new(PluginId::new("echo").unwrap(), FIXTURE);
    spec.env = env;
    spec.working_dir = std::env::current_dir().unwrap();
    spec.limits.rpc_timeout_ms = rpc_timeout_ms;
    spec
}

fn spawn(
    env: BTreeMap<String, String>,
    rpc_timeout_ms: u64,
) -> (NativeClient, Arc<RecordingAdapter>, Arc<MemoryContributionSink>) {
    let adapter = Arc::new(RecordingAdapter::default());
    let sink = Arc::new(MemoryContributionSink::new());
    let client =
        NativeClient::spawn(spec(env, rpc_timeout_ms), adapter.clone(), sink.clone()).unwrap();
    (client, adapter, sink)
}

#[test]
fn initialize_and_command_round_trip() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 2_000);
    let id = CommandId::new("echo.ping").unwrap();
    let result = client.invoke(&id, serde_json::json!({"x": 1})).unwrap();
    assert_eq!(result, serde_json::json!({"x": 1}));
}

#[test]
fn event_delivery_emits_effect() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 2_000);
    client.send_event("editor.document.changed", 1, serde_json::json!({"rev": 1})).unwrap();
    let message = client.poll(Duration::from_secs(2)).unwrap().unwrap();
    match message {
        PluginToHost::Effect(envelope) => assert_eq!(envelope.effects, vec![Effect::Noop]),
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn effects_contributions_and_health_are_applied() {
    let (mut client, adapter, sink) = spawn(BTreeMap::new(), 2_000);

    client.invoke(&CommandId::new("echo.effect").unwrap(), serde_json::json!({})).unwrap();
    assert_eq!(adapter.effects.lock().unwrap().len(), 1);

    client.invoke(&CommandId::new("echo.contribution").unwrap(), serde_json::json!({})).unwrap();
    assert_eq!(sink.len(), 1);

    client.invoke(&CommandId::new("echo.health").unwrap(), serde_json::json!({})).unwrap();
    assert_eq!(client.health().unwrap().state, "degraded");
}

#[test]
fn content_length_transport_round_trips() {
    let env = BTreeMap::from([("ECHO_TRANSPORT".to_owned(), "content_length".to_owned())]);
    let adapter = Arc::new(RecordingAdapter::default());
    let sink = Arc::new(MemoryContributionSink::new());
    let mut spec = spec(env, 2_000);
    spec.transport = LaunchTransport::StdioContentLength;
    let mut client = NativeClient::spawn(spec, adapter, sink).unwrap();
    let result = client
        .invoke(&CommandId::new("echo.ping").unwrap(), serde_json::json!({"frame": "length"}))
        .unwrap();
    assert_eq!(result["frame"], "length");
}

#[test]
fn service_request_is_dispatched_through_adapter() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 2_000);
    let result =
        client.invoke(&CommandId::new("echo.request").unwrap(), serde_json::json!({})).unwrap();
    assert_eq!(result, serde_json::json!({"rev": 1, "len": 5}));
}

#[test]
fn command_timeout_is_reported() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 100);
    let error =
        client.invoke(&CommandId::new("echo.timeout").unwrap(), serde_json::json!({})).unwrap_err();
    assert_eq!(error.class(), ErrorClass::Transport);
    assert!(error.to_string().contains("timed out"));
}

#[test]
fn malformed_response_is_reported() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 2_000);
    let error = client
        .invoke(&CommandId::new("echo.malformed").unwrap(), serde_json::json!({}))
        .unwrap_err();
    assert_eq!(error.class(), ErrorClass::Transport);
}

#[test]
fn child_exit_is_reported_as_transport_error() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 2_000);
    let error =
        client.invoke(&CommandId::new("echo.exit").unwrap(), serde_json::json!({})).unwrap_err();
    assert_eq!(error.class(), ErrorClass::Transport);
}

#[test]
fn shutdown_terminates_child() {
    let (mut client, _adapter, _sink) = spawn(BTreeMap::new(), 2_000);
    let pid = client.pid();
    client.shutdown();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("child process {pid} is still running after shutdown");
}
