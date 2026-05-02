// Copyright 2017 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Plugins and related functionality.

mod catalog;
pub mod manifest;
pub mod rpc;

use std::fmt;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::process::{Child, Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use log::{error, info};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use xi_rpc::{
    self, ContentLengthReader, ContentLengthWriter, NewlineReader, NewlineWriter, RequestId,
    RpcLoop, RpcPeer,
};

use crate::WeakXiCore;
use crate::config::Table;
use crate::syntax::LanguageId;
use crate::tabs::ViewId;
use crate::tracing_support;

use self::rpc::{PluginBufferInfo, PluginUpdate, core_protocol_capabilities};

pub(crate) use self::catalog::PluginCatalog;
pub use self::manifest::{
    Command, ManifestValidationError, PlaceholderRpc, PluginCapability, PluginDescription,
    PluginTransport,
};

pub type PluginName = String;

/// A process-unique identifier for a running plugin.
///
/// Note: two instances of the same executable will have different identifiers.
/// Note: this identifier is distinct from the OS's process id.
#[derive(
    Serialize, Deserialize, Default, Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct PluginPid(pub(crate) usize);

pub type PluginId = PluginPid;

impl fmt::Display for PluginPid {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "plugin-{}", self.0)
    }
}

pub struct Plugin {
    peer: RpcPeer,
    pub(crate) id: PluginId,
    pub(crate) name: String,
    pub(crate) manifest: Arc<PluginDescription>,
    process: Arc<Mutex<Child>>,
    /// True while an `update` RPC is awaiting a response from the plugin.
    update_in_flight: Arc<AtomicBool>,
    /// The latest update that arrived while `update_in_flight` was set.
    /// Coalesces repeated updates so a slow plugin never accumulates unbounded
    /// pending work; only the most recent un-acknowledged update is queued.
    coalesced_update: Arc<Mutex<Option<PluginUpdate>>>,
}

#[derive(Debug)]
pub struct PluginStartError {
    pub(crate) name: String,
    pub(crate) source: PluginStartErrorKind,
}

#[derive(Debug)]
pub enum PluginStartErrorKind {
    Io(std::io::Error),
    UnsupportedTransport(PluginTransport),
}

impl Plugin {
    //TODO: initialize should be sent automatically during launch,
    //and should only send the plugin_id. We can just use the existing 'new_buffer'
    // RPC for adding views
    pub fn initialize(&self, info: Vec<PluginBufferInfo>) {
        self.peer.send_rpc_notification(
            "initialize",
            &json!({
                "plugin_id": self.id,
                "buffer_info": info,
                "protocol_version": crate::plugins::rpc::PLUGIN_PROTOCOL_VERSION,
                "core_capabilities": core_protocol_capabilities(),
            }),
        )
    }

    pub fn shutdown(&self) {
        self.peer.send_rpc_notification("shutdown", &json!({}));
    }

    // TODO: rethink naming, does this need to be a vec?
    pub fn new_buffer(&self, info: &PluginBufferInfo) {
        self.peer.send_rpc_notification("new_buffer", &json!({ "buffer_info": [info] }))
    }

    pub fn close_view(&self, view_id: ViewId) {
        self.peer.send_rpc_notification("did_close", &json!({ "view_id": view_id }))
    }

    pub fn did_save(&self, view_id: ViewId, path: &Path) {
        self.peer.send_rpc_notification(
            "did_save",
            &json!({
                "view_id": view_id,
                "path": path,
            }),
        )
    }

    /// Delivers an update to the plugin with coalescing backpressure.
    ///
    /// If a previous update is still awaiting a response the incoming update is
    /// stored, replacing any already-queued coalesced update.  When the
    /// in-flight response arrives the queued update is dispatched immediately,
    /// keeping plugin state current without accumulating unbounded work.
    pub fn update(&self, update: &PluginUpdate, weak_core: WeakXiCore, view_id: ViewId) {
        if self
            .update_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // An update is already in-flight; coalesce by keeping only the latest.
            *self.coalesced_update.lock().expect("coalesced_update lock") = Some(update.clone());
            return;
        }
        drive_plugin_update(
            &*self.peer,
            update.clone(),
            Arc::clone(&self.coalesced_update),
            Arc::clone(&self.update_in_flight),
            weak_core,
            self.id,
            view_id,
        );
    }

    pub fn toggle_tracing(&self, enabled: bool) {
        self.peer.send_rpc_notification("tracing_config", &json!({ "enabled": enabled }))
    }

    pub fn collect_trace(&self) -> Result<Value, xi_rpc::Error> {
        self.peer.send_rpc_request("collect_trace", &json!({}))
    }

    pub fn config_changed(&self, view_id: ViewId, changes: &Table) {
        self.peer.send_rpc_notification(
            "config_changed",
            &json!({
                "view_id": view_id,
                "changes": changes,
            }),
        )
    }

    pub fn language_changed(&self, view_id: ViewId, new_lang: &LanguageId) {
        self.peer.send_rpc_notification(
            "language_changed",
            &json!({
                "view_id": view_id,
                "new_lang": new_lang,
            }),
        )
    }

    pub fn request_hover<F>(&self, view_id: ViewId, position: usize, callback: F) -> RequestId
    where
        F: FnOnce(Result<Value, xi_rpc::Error>) + Send + 'static,
    {
        self.peer.send_rpc_request_async(
            "get_hover",
            &json!({
                "view_id": view_id,
                "position": position,
            }),
            Box::new(callback),
        )
    }

    pub fn cancel_request(&self, id: RequestId) -> bool {
        self.peer.cancel_rpc_request(id)
    }

    pub fn dispatch_command(&self, view_id: ViewId, method: &str, params: &Value) {
        self.peer.send_rpc_notification(
            "custom_command",
            &json!({
                "view_id": view_id,
                "method": method,
                "params": params,
            }),
        )
    }

    pub fn receives_updates_for(&self, language: &LanguageId) -> bool {
        self.manifest.receives_updates_for(language)
    }

    pub fn is_single_invocation(&self) -> bool {
        matches!(self.manifest.scope, manifest::PluginScope::SingleInvocation)
    }

    pub fn process_handle(&self) -> Arc<Mutex<Child>> {
        Arc::clone(&self.process)
    }
}

/// Sends `update` via `peer` and, when the response arrives, either dispatches
/// the coalesced update (if one arrived while this RPC was in-flight) or clears
/// the `update_in_flight` flag.
fn drive_plugin_update(
    peer: &dyn xi_rpc::Peer,
    update: PluginUpdate,
    coalesced: Arc<Mutex<Option<PluginUpdate>>>,
    in_flight: Arc<AtomicBool>,
    weak_core: WeakXiCore,
    id: PluginId,
    view_id: ViewId,
) {
    let in_flight_cb = Arc::clone(&in_flight);
    let coalesced_cb = Arc::clone(&coalesced);
    // box_clone() shares the same underlying Arc<RpcState>, so it's cheap.
    let peer_cb = peer.box_clone();
    peer.send_rpc_request_async(
        "update",
        &json!(update),
        Box::new(move |resp| {
            weak_core.handle_plugin_update(id, view_id, resp);
            let next = coalesced_cb.lock().expect("coalesced_update lock").take();
            match next {
                Some(next_update) => {
                    // Keep in_flight=true and dispatch the coalesced update.
                    drive_plugin_update(
                        &*peer_cb,
                        next_update,
                        coalesced_cb,
                        in_flight_cb,
                        weak_core,
                        id,
                        view_id,
                    );
                }
                None => {
                    in_flight_cb.store(false, Ordering::Release);
                }
            }
        }),
    );
}

pub(crate) fn start_plugin_process(
    plugin_desc: Arc<PluginDescription>,
    id: PluginId,
    core: WeakXiCore,
) {
    let spawn_result = thread::Builder::new()
        .name(format!("<{}> core host thread", &plugin_desc.name))
        .spawn(move || {
            info!("starting plugin {}", &plugin_desc.name);
            let child = spawn_child_process(&plugin_desc);

            match child {
                Ok(mut child) => {
                    let stderr = child.stderr.take();
                    let child_stdin = match child.stdin.take() {
                        Some(s) => s,
                        None => {
                            core.plugin_connect(Err(PluginStartError {
                                name: plugin_desc.name.clone(),
                                source: PluginStartErrorKind::Io(std::io::Error::new(
                                    std::io::ErrorKind::BrokenPipe,
                                    "child stdin was not piped",
                                )),
                            }));
                            return;
                        }
                    };
                    let child_stdout = match child.stdout.take() {
                        Some(s) => s,
                        None => {
                            core.plugin_connect(Err(PluginStartError {
                                name: plugin_desc.name.clone(),
                                source: PluginStartErrorKind::Io(std::io::Error::new(
                                    std::io::ErrorKind::BrokenPipe,
                                    "child stdout was not piped",
                                )),
                            }));
                            return;
                        }
                    };
                    let process = Arc::new(Mutex::new(child));
                    if let Some(stderr) = stderr {
                        spawn_stderr_thread(plugin_desc.name.clone(), stderr, core.clone());
                    }

                    match plugin_desc.launch.transport {
                        PluginTransport::StdioNewline => {
                            let mut looper = RpcLoop::new(NewlineWriter::new(child_stdin));
                            let peer: RpcPeer = Box::new(looper.get_raw_peer());
                            let name = plugin_desc.name.clone();
                            peer.send_rpc_notification("ping", &Value::Array(Vec::new()));
                            let plugin = Plugin {
                                peer,
                                process: Arc::clone(&process),
                                name,
                                id,
                                manifest: plugin_desc.clone(),
                                update_in_flight: Arc::new(AtomicBool::new(false)),
                                coalesced_update: Arc::new(Mutex::new(None)),
                            };

                            if tracing_support::is_enabled() {
                                plugin.toggle_tracing(true);
                            }

                            core.plugin_connect(Ok(plugin));
                            let mut core = core;
                            let err = looper.mainloop(
                                || NewlineReader::new(BufReader::new(child_stdout)),
                                &mut core,
                            );
                            core.plugin_exit(id, err);
                        }
                        PluginTransport::StdioContentLength => {
                            let mut looper = RpcLoop::new(ContentLengthWriter::new(child_stdin));
                            let peer: RpcPeer = Box::new(looper.get_raw_peer());
                            let name = plugin_desc.name.clone();
                            peer.send_rpc_notification("ping", &Value::Array(Vec::new()));
                            let plugin = Plugin {
                                peer,
                                process: Arc::clone(&process),
                                name,
                                id,
                                manifest: plugin_desc.clone(),
                                update_in_flight: Arc::new(AtomicBool::new(false)),
                                coalesced_update: Arc::new(Mutex::new(None)),
                            };

                            if tracing_support::is_enabled() {
                                plugin.toggle_tracing(true);
                            }

                            core.plugin_connect(Ok(plugin));
                            let mut core = core;
                            let err = looper.mainloop(
                                || ContentLengthReader::new(BufReader::new(child_stdout)),
                                &mut core,
                            );
                            core.plugin_exit(id, err);
                        }
                    }
                }
                Err(source) => core.plugin_connect(Err(PluginStartError {
                    name: plugin_desc.name.clone(),
                    source,
                })),
            }
        });

    if let Err(err) = spawn_result {
        error!("thread spawn failed for {}, {:?}", id, err);
    }
}

fn spawn_child_process(plugin_desc: &PluginDescription) -> Result<Child, PluginStartErrorKind> {
    let mut command = ProcCommand::new(&plugin_desc.exec_path);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    if let Some(working_dir) = &plugin_desc.launch.working_dir {
        command.current_dir(working_dir);
    }

    for (key, value) in &plugin_desc.launch.env {
        command.env(key, value);
    }

    command.spawn().map_err(PluginStartErrorKind::Io)
}

fn spawn_stderr_thread(name: String, stderr: std::process::ChildStderr, core: WeakXiCore) {
    let thread_name = name.clone();
    let stderr_name = name.clone();
    let spawn_result =
        thread::Builder::new().name(format!("<{}> stderr", thread_name)).spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => {
                        core.plugin_stderr(stderr_name.clone(), line);
                    }
                    Ok(_) => (),
                    Err(err) => {
                        error!("plugin {} stderr read error: {:?}", stderr_name, err);
                        break;
                    }
                }
            }
        });

    if let Err(err) = spawn_result {
        error!("stderr thread spawn failed for {}: {:?}", name, err);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::rpc::PluginUpdate;
    use super::{PluginId, drive_plugin_update};
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

    /// When a coalesced update is present at response time, it is sent
    /// immediately and `in_flight` stays true until that second response arrives.
    #[test]
    fn coalesce_dispatches_pending_update_on_response() {
        use crate::tabs::ViewId;
        let call_count = Arc::new(AtomicUsize::new(0));
        let pending_cb: Arc<Mutex<Option<Box<dyn Callback>>>> = Arc::new(Mutex::new(None));
        let peer = MockPeer::new(Arc::clone(&call_count), Arc::clone(&pending_cb));

        let in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let coalesced: Arc<Mutex<Option<PluginUpdate>>> =
            Arc::new(Mutex::new(Some(dummy_update(2))));
        let weak_core = crate::core::dummy_weak_core();

        drive_plugin_update(
            &peer,
            dummy_update(1),
            Arc::clone(&coalesced),
            Arc::clone(&in_flight),
            weak_core,
            PluginId::default(),
            ViewId::from(0usize),
        );

        // After first response, coalesced update should be dispatched immediately.
        let cb1 = pending_cb.lock().unwrap().take().expect("first callback");
        cb1.call(Ok(Value::Null));

        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "coalesced update sent"
        );
        assert!(
            in_flight.load(std::sync::atomic::Ordering::Acquire),
            "still in-flight for coalesced update"
        );

        // Completing the coalesced update's response clears in_flight.
        let cb2 = pending_cb.lock().unwrap().take().expect("second callback");
        cb2.call(Ok(Value::Null));

        assert!(!in_flight.load(std::sync::atomic::Ordering::Acquire));
    }
}
