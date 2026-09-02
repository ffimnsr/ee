#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

#[cfg(target_os = "linux")]
use super::manifest::{PluginLaunchConfig, PluginScope};
use super::rpc::PluginUpdate;
#[cfg(target_os = "linux")]
use super::{
    PluginCapability, PluginDescription, PluginRuntime, linux_denied_syscalls,
    should_apply_linux_plugin_sandbox,
};
use super::{PluginController, PluginId, PluginTerminationHandle, drive_plugin_update};
use xi_rpc::{Callback, Error as RpcError, Peer, RequestId};

/// Minimal mock peer that captures the most-recently registered callback so
/// tests can trigger it manually.
struct MockPeer {
    call_count: Arc<AtomicUsize>,
    pending_cb: Arc<Mutex<Option<Box<dyn Callback>>>>,
}

impl MockPeer {
    fn new(
        call_count: Arc<AtomicUsize>,
        pending_cb: Arc<Mutex<Option<Box<dyn Callback>>>>,
    ) -> Self {
        Self { call_count, pending_cb }
    }
}

impl Peer for MockPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(MockPeer {
            call_count: Arc::clone(&self.call_count),
            pending_cb: Arc::clone(&self.pending_cb),
        })
    }

    fn send_rpc_notification(&self, _method: &str, _params: &Value) {}

    fn send_rpc_request_async(
        &self,
        _method: &str,
        _params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        self.call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.pending_cb.lock().unwrap() = Some(f);
        RequestId::Number(0)
    }

    fn send_rpc_request(&self, _method: &str, _params: &Value) -> Result<Value, RpcError> {
        Ok(Value::Null)
    }

    fn send_rpc_request_timeout(
        &self,
        _method: &str,
        _params: &Value,
        _timeout: Duration,
    ) -> Result<Value, RpcError> {
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

struct MockController;

impl PluginController for MockController {
    fn has_exited(&self) -> std::io::Result<bool> {
        Ok(false)
    }

    fn terminate(&self) -> std::io::Result<()> {
        Ok(())
    }
}

fn test_termination_handle() -> Arc<PluginTerminationHandle> {
    Arc::new(PluginTerminationHandle::new(
        PluginId::default(),
        Arc::new(MockController),
        crate::core::dummy_weak_core(),
    ))
}

fn dummy_update(rev: u64) -> PluginUpdate {
    use crate::tabs::ViewId;
    PluginUpdate::new(ViewId::from(0usize), rev, None, 0, 1, None, "edit".into(), "test".into())
}

/// When `drive_plugin_update` is called it immediately sends via the peer.
#[test]
fn coalesce_first_update_goes_directly_to_peer() {
    use crate::tabs::ViewId;
    let call_count = Arc::new(AtomicUsize::new(0));
    let pending_cb: Arc<Mutex<Option<Box<dyn Callback>>>> = Arc::new(Mutex::new(None));
    let peer = MockPeer::new(Arc::clone(&call_count), Arc::clone(&pending_cb));

    let in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let coalesced: Arc<Mutex<Option<PluginUpdate>>> = Arc::new(Mutex::new(None));
    let weak_core = crate::core::dummy_weak_core();

    drive_plugin_update(
        &peer,
        dummy_update(1),
        Arc::clone(&coalesced),
        Arc::clone(&in_flight),
        None,
        test_termination_handle(),
        weak_core,
        PluginId::default(),
        ViewId::from(0usize),
    );

    assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(pending_cb.lock().unwrap().is_some());
    // still in-flight while awaiting the response
    assert!(in_flight.load(std::sync::atomic::Ordering::Acquire));
}

/// When a response arrives with no coalesced update, `in_flight` is cleared.
#[test]
fn coalesce_clears_in_flight_on_response_when_no_coalesced() {
    use crate::tabs::ViewId;
    let call_count = Arc::new(AtomicUsize::new(0));
    let pending_cb: Arc<Mutex<Option<Box<dyn Callback>>>> = Arc::new(Mutex::new(None));
    let peer = MockPeer::new(Arc::clone(&call_count), Arc::clone(&pending_cb));

    let in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let coalesced: Arc<Mutex<Option<PluginUpdate>>> = Arc::new(Mutex::new(None));
    let weak_core = crate::core::dummy_weak_core();

    drive_plugin_update(
        &peer,
        dummy_update(1),
        Arc::clone(&coalesced),
        Arc::clone(&in_flight),
        None,
        test_termination_handle(),
        weak_core,
        PluginId::default(),
        ViewId::from(0usize),
    );

    // Simulate the RPC response arriving.
    let cb = pending_cb.lock().unwrap().take().expect("callback registered");
    cb.call(Ok(Value::Null));

    assert!(!in_flight.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sandbox_denies_process_escape_syscalls() {
    let denied = linux_denied_syscalls();

    assert!(denied.contains(&libc::SYS_fork));
    assert!(denied.contains(&libc::SYS_vfork));
    assert!(denied.contains(&libc::SYS_ptrace));
    assert!(denied.contains(&libc::SYS_socket));
    assert!(denied.contains(&libc::SYS_open));
    assert!(denied.contains(&libc::SYS_openat));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sandbox_skips_plugins_with_filesystem_or_network_capabilities() {
    let base = PluginDescription {
        name: "sandbox-test".into(),
        version: "0.1.0".into(),
        requires: Vec::new(),
        scope: PluginScope::Global,
        runtime: PluginRuntime::Native,
        capabilities: Vec::new(),
        launch: PluginLaunchConfig::default(),
        max_rss_bytes: None,
        max_cpu_seconds: None,
        rpc_timeout_ms: None,
        exec_path: PathBuf::from("plugin"),
        activations: Vec::new(),
        commands: Vec::new(),
        languages: Vec::new(),
    };

    assert!(should_apply_linux_plugin_sandbox(&base));

    let mut filesystem = base.clone();
    filesystem.capabilities.push(PluginCapability::Filesystem);
    assert!(!should_apply_linux_plugin_sandbox(&filesystem));

    let mut network = base;
    network.capabilities.push(PluginCapability::Network);
    assert!(!should_apply_linux_plugin_sandbox(&network));
}

/// When a coalesced update is present at response time, it is sent
/// immediately and `in_flight` stays true until that second response arrives.
#[test]
fn coalesce_dispatches_pending_update_on_response() {
    use crate::tabs::ViewId;
    let call_count = Arc::new(AtomicUsize::new(0));
    let pending_cb: Arc<Mutex<Option<Box<dyn Callback>>>> = Arc::new(Mutex::new(None));
    let peer = MockPeer::new(Arc::clone(&call_count), Arc::clone(&pending_cb));

    let in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let coalesced: Arc<Mutex<Option<PluginUpdate>>> = Arc::new(Mutex::new(Some(dummy_update(2))));
    let weak_core = crate::core::dummy_weak_core();

    drive_plugin_update(
        &peer,
        dummy_update(1),
        Arc::clone(&coalesced),
        Arc::clone(&in_flight),
        None,
        test_termination_handle(),
        weak_core,
        PluginId::default(),
        ViewId::from(0usize),
    );

    // After first response, coalesced update should be dispatched immediately.
    let cb1 = pending_cb.lock().unwrap().take().expect("first callback");
    cb1.call(Ok(Value::Null));

    assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 2, "coalesced update sent");
    assert!(
        in_flight.load(std::sync::atomic::Ordering::Acquire),
        "still in-flight for coalesced update"
    );

    // Completing the coalesced update's response clears in_flight.
    let cb2 = pending_cb.lock().unwrap().take().expect("second callback");
    cb2.call(Ok(Value::Null));

    assert!(!in_flight.load(std::sync::atomic::Ordering::Acquire));
}
