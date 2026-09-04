use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use xi_rpc::{Callback, Error, Peer, RequestId, RpcPeer};

use super::manifest::{PluginLaunchConfig, PluginScope};
use super::{Plugin, PluginController, PluginDescription, PluginId, PluginRuntime, build_plugin};
use crate::core::dummy_weak_core;

struct TestPeer;

impl Peer for TestPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(Self)
    }

    fn send_rpc_notification(&self, _method: &str, _params: &Value) {}

    fn send_rpc_request_async(
        &self,
        _method: &str,
        _params: &Value,
        _callback: Box<dyn Callback>,
    ) -> RequestId {
        RequestId::Number(0)
    }

    fn send_rpc_request(&self, _method: &str, _params: &Value) -> Result<Value, Error> {
        Ok(Value::Null)
    }

    fn send_rpc_request_timeout(
        &self,
        _method: &str,
        _params: &Value,
        _timeout: Duration,
    ) -> Result<Value, Error> {
        Ok(Value::Null)
    }

    fn cancel_rpc_request(&self, _id: RequestId) -> bool {
        false
    }

    fn request_is_pending(&self) -> bool {
        false
    }

    fn schedule_idle(&self, _token: usize) {}

    fn schedule_timer(&self, _after: Instant, _token: usize) {}

    fn cancel_timer(&self, _token: usize) -> bool {
        false
    }

    fn request_shutdown(&self) {}
}

struct TestController;

impl PluginController for TestController {
    fn has_exited(&self) -> io::Result<bool> {
        Ok(false)
    }

    fn terminate(&self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn test_plugin(name: &str) -> Plugin {
    let peer: RpcPeer = Box::new(TestPeer);
    let description = Arc::new(PluginDescription {
        name: name.to_string(),
        version: "0.0.0".into(),
        requires: Vec::new(),
        scope: PluginScope::Global,
        runtime: PluginRuntime::Native,
        capabilities: Vec::new(),
        launch: PluginLaunchConfig::default(),
        max_rss_bytes: None,
        max_cpu_seconds: None,
        rpc_timeout_ms: None,
        exec_path: PathBuf::from("test-plugin"),
        activations: Vec::new(),
        commands: Vec::new(),
        languages: Vec::new(),
    });

    build_plugin(
        peer,
        Arc::new(TestController),
        description,
        PluginId::default(),
        dummy_weak_core(),
    )
}
