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
mod wasm;

use std::fmt;
#[cfg(target_os = "linux")]
use std::fs;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::process::{Child, Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
use self::wasm::run_wasm_plugin;

pub(crate) use self::catalog::PluginCatalog;
pub use self::manifest::{
    Command, ManifestValidationError, PlaceholderRpc, PluginCapability, PluginDescription,
    PluginRuntime, PluginTransport,
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
    controller: Arc<dyn PluginController>,
    /// True while an `update` RPC is awaiting a response from the plugin.
    update_in_flight: Arc<AtomicBool>,
    /// The latest update that arrived while `update_in_flight` was set.
    /// Coalesces repeated updates so a slow plugin never accumulates unbounded
    /// pending work; only the most recent un-acknowledged update is queued.
    coalesced_update: Arc<Mutex<Option<PluginUpdate>>>,
    rpc_timeout: Option<Duration>,
    termination: Arc<PluginTerminationHandle>,
    local_request_ids: Arc<AtomicUsize>,
    local_pending_requests: Arc<Mutex<Vec<PendingRpcRequest>>>,
}

type PendingRpcRequest = (RequestId, Arc<AtomicBool>);

#[derive(Debug)]
pub struct PluginStartError {
    pub(crate) name: String,
    pub(crate) source: PluginStartErrorKind,
}

#[derive(Debug)]
pub enum PluginStartErrorKind {
    Io(std::io::Error),
    UnsupportedTransport(PluginTransport),
    Sandbox(String),
    Wasm(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginTerminationReason {
    MaxRssBytes { limit_bytes: u64, observed_bytes: u64 },
    MaxCpuSeconds { limit_seconds: u64, observed_seconds: u64 },
    RpcTimedOut { limit_ms: u64, method: String },
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PluginResourceUsage {
    rss_bytes: Option<u64>,
    cpu_seconds: Option<u64>,
}

pub(crate) trait PluginController: Send + Sync {
    fn has_exited(&self) -> io::Result<bool>;
    fn terminate(&self) -> io::Result<()>;

    fn resource_usage(&self) -> io::Result<Option<PluginResourceUsage>> {
        Ok(None)
    }
}

struct ChildProcessController {
    child: Arc<Mutex<Child>>,
}

impl ChildProcessController {
    fn new(child: Arc<Mutex<Child>>) -> Self {
        Self { child }
    }
}

impl PluginController for ChildProcessController {
    fn has_exited(&self) -> io::Result<bool> {
        let Some(mut child) = self.child.lock().ok() else {
            return Err(io::Error::other("child process lock poisoned"));
        };
        Ok(child.try_wait()?.is_some())
    }

    fn terminate(&self) -> io::Result<()> {
        let Some(mut child) = self.child.lock().ok() else {
            return Err(io::Error::other("child process lock poisoned"));
        };
        child.kill()?;
        let _ = child.wait();
        Ok(())
    }

    fn resource_usage(&self) -> io::Result<Option<PluginResourceUsage>> {
        #[cfg(target_os = "linux")]
        {
            let Some(child) = self.child.lock().ok() else {
                return Err(io::Error::other("child process lock poisoned"));
            };
            read_linux_process_resource_usage(child.id()).map(Some)
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(None)
        }
    }
}

struct PluginTerminationHandle {
    plugin_id: PluginId,
    controller: Arc<dyn PluginController>,
    core: WeakXiCore,
    triggered: AtomicBool,
}

impl PluginTerminationHandle {
    fn new(plugin_id: PluginId, controller: Arc<dyn PluginController>, core: WeakXiCore) -> Self {
        Self { plugin_id, controller, core, triggered: AtomicBool::new(false) }
    }

    fn notify_breach(&self, reason: PluginTerminationReason) {
        if self.triggered.swap(true, Ordering::AcqRel) {
            return;
        }
        self.core.plugin_terminated(self.plugin_id, reason);
        let _ = self.controller.terminate();
    }
}

impl Plugin {
    //TODO: initialize should be sent automatically during launch,
    //and should only send the plugin_id. We can just use the existing 'new_buffer'
    // RPC for adding views
    pub fn initialize(&self, info: Vec<PluginBufferInfo>, plugin_config: &Table) {
        self.peer.send_rpc_notification(
            "initialize",
            &json!({
                "plugin_id": self.id,
                "buffer_info": info,
                "plugin_config": plugin_config,
                "protocol_version": crate::plugins::rpc::PLUGIN_PROTOCOL_VERSION,
                "core_capabilities": core_protocol_capabilities(),
            }),
        )
    }

    pub fn plugin_config_changed(&self, changes: &Table) {
        self.peer.send_rpc_notification(
            "plugin_config_changed",
            &json!({
                "changes": changes,
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
            self.rpc_timeout,
            Arc::clone(&self.termination),
            weak_core,
            self.id,
            view_id,
        );
    }

    pub fn toggle_tracing(&self, enabled: bool) {
        self.peer.send_rpc_notification("tracing_config", &json!({ "enabled": enabled }))
    }

    pub fn collect_trace(&self) -> Result<Value, xi_rpc::Error> {
        let result = match self.rpc_timeout {
            Some(timeout) => {
                self.peer.send_rpc_request_timeout("collect_trace", &json!({}), timeout)
            }
            None => self.peer.send_rpc_request("collect_trace", &json!({})),
        };
        self.notify_timeout("collect_trace", &result);
        result
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
        let params = json!({
            "view_id": view_id,
            "position": position,
        });
        if let Some(timeout) = self.rpc_timeout {
            let request_id =
                RequestId::Number(self.local_request_ids.fetch_add(1, Ordering::Relaxed) as u64);
            let request_id_for_worker = request_id.clone();
            let cancelled = Arc::new(AtomicBool::new(false));
            self.local_pending_requests
                .lock()
                .expect("local pending request lock")
                .push((request_id.clone(), Arc::clone(&cancelled)));
            let peer = self.peer.clone();
            let termination = Arc::clone(&self.termination);
            let pending = Arc::clone(&self.local_pending_requests);
            thread::spawn(move || {
                let result = peer.send_rpc_request_timeout("get_hover", &params, timeout);
                let removed = {
                    let mut pending = pending.lock().expect("local pending request lock");
                    pending
                        .iter()
                        .position(|(candidate, _)| *candidate == request_id_for_worker)
                        .map(|index| pending.remove(index))
                };
                if let Some((_, flag)) = removed {
                    if !flag.load(Ordering::Acquire) {
                        if let Err(xi_rpc::Error::PeerTimedOut { after_ms }) = &result {
                            termination.notify_breach(PluginTerminationReason::RpcTimedOut {
                                limit_ms: *after_ms,
                                method: "get_hover".into(),
                            });
                        }
                        callback(result);
                    }
                }
            });
            request_id
        } else {
            self.peer.send_rpc_request_async("get_hover", &params, Box::new(callback))
        }
    }

    pub fn cancel_request(&self, id: RequestId) -> bool {
        let mut pending = self.local_pending_requests.lock().expect("local pending request lock");
        if let Some(index) = pending.iter().position(|(candidate, _)| *candidate == id) {
            let (_, flag) = pending.remove(index);
            flag.store(true, Ordering::Release);
            return true;
        }
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

    pub(crate) fn controller_handle(&self) -> Arc<dyn PluginController> {
        Arc::clone(&self.controller)
    }

    fn notify_timeout(&self, method: &str, result: &Result<Value, xi_rpc::Error>) {
        if let Err(xi_rpc::Error::PeerTimedOut { after_ms }) = result {
            self.termination.notify_breach(PluginTerminationReason::RpcTimedOut {
                limit_ms: *after_ms,
                method: method.to_string(),
            });
        }
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
    rpc_timeout: Option<Duration>,
    termination: Arc<PluginTerminationHandle>,
    weak_core: WeakXiCore,
    id: PluginId,
    view_id: ViewId,
) {
    let params = json!(update);
    let in_flight_cb = Arc::clone(&in_flight);
    let coalesced_cb = Arc::clone(&coalesced);
    let peer_cb = peer.box_clone();
    let finish = move |resp: Result<Value, xi_rpc::Error>| {
        if let Err(xi_rpc::Error::PeerTimedOut { after_ms }) = &resp {
            termination.notify_breach(PluginTerminationReason::RpcTimedOut {
                limit_ms: *after_ms,
                method: "update".into(),
            });
        }
        weak_core.handle_plugin_update(id, view_id, resp);
        let next = coalesced_cb.lock().expect("coalesced_update lock").take();
        match next {
            Some(next_update) => {
                drive_plugin_update(
                    &*peer_cb,
                    next_update,
                    coalesced_cb,
                    in_flight_cb,
                    rpc_timeout,
                    termination,
                    weak_core,
                    id,
                    view_id,
                );
            }
            None => {
                in_flight_cb.store(false, Ordering::Release);
            }
        }
    };

    if let Some(timeout) = rpc_timeout {
        let peer = peer.box_clone();
        thread::spawn(move || {
            finish(peer.send_rpc_request_timeout("update", &params, timeout));
        });
    } else {
        peer.send_rpc_request_async("update", &params, Box::new(finish));
    }
}

pub(crate) fn start_plugin_process(
    plugin_desc: Arc<PluginDescription>,
    id: PluginId,
    core: WeakXiCore,
) {
    let spawn_result = thread::Builder::new()
        .name(format!("<{}> core host thread", plugin_desc.name))
        .spawn(move || {
            info!("starting plugin {}", plugin_desc.name);
            let result = match plugin_desc.runtime {
                PluginRuntime::Native => run_native_plugin(plugin_desc.clone(), id, core.clone()),
                PluginRuntime::Wasm => run_wasm_plugin(plugin_desc.clone(), id, core.clone()),
            };

            if let Err(source) = result {
                core.plugin_connect(Err(PluginStartError {
                    name: plugin_desc.name.clone(),
                    source,
                }));
            }
        });

    if let Err(err) = spawn_result {
        error!("thread spawn failed for {}, {:?}", id, err);
    }
}

fn run_native_plugin(
    plugin_desc: Arc<PluginDescription>,
    id: PluginId,
    core: WeakXiCore,
) -> Result<(), PluginStartErrorKind> {
    let mut child = spawn_child_process(&plugin_desc)?;
    let stderr = child.stderr.take();
    let child_stdin = child.stdin.take().ok_or_else(|| {
        PluginStartErrorKind::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "child stdin was not piped",
        ))
    })?;
    let child_stdout = child.stdout.take().ok_or_else(|| {
        PluginStartErrorKind::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "child stdout was not piped",
        ))
    })?;
    let process = Arc::new(Mutex::new(child));
    let controller: Arc<dyn PluginController> =
        Arc::new(ChildProcessController::new(Arc::clone(&process)));
    if let Some(stderr) = stderr {
        spawn_stderr_thread(plugin_desc.name.clone(), stderr, core.clone());
    }

    match plugin_desc.launch.transport {
        PluginTransport::StdioNewline => {
            let mut looper = RpcLoop::new(NewlineWriter::new(child_stdin));
            let raw_peer = looper.get_raw_peer();
            spawn_native_exit_watcher(process, raw_peer.clone());
            let peer: RpcPeer = Box::new(raw_peer);
            let plugin = build_plugin(peer, controller, plugin_desc.clone(), id, core.clone());
            core.plugin_connect(Ok(plugin));
            let mut core = core;
            let err =
                looper.mainloop(|| NewlineReader::new(BufReader::new(child_stdout)), &mut core);
            core.plugin_exit(id, err);
        }
        PluginTransport::StdioContentLength => {
            let mut looper = RpcLoop::new(ContentLengthWriter::new(child_stdin));
            let raw_peer = looper.get_raw_peer();
            spawn_native_exit_watcher(process, raw_peer.clone());
            let peer: RpcPeer = Box::new(raw_peer);
            let plugin = build_plugin(peer, controller, plugin_desc.clone(), id, core.clone());
            core.plugin_connect(Ok(plugin));
            let mut core = core;
            let err = looper
                .mainloop(|| ContentLengthReader::new(BufReader::new(child_stdout)), &mut core);
            core.plugin_exit(id, err);
        }
    }

    Ok(())
}

fn build_plugin(
    peer: RpcPeer,
    controller: Arc<dyn PluginController>,
    plugin_desc: Arc<PluginDescription>,
    id: PluginId,
    core: WeakXiCore,
) -> Plugin {
    peer.send_rpc_notification("ping", &Value::Array(Vec::new()));
    let termination = Arc::new(PluginTerminationHandle::new(id, Arc::clone(&controller), core));
    let plugin = Plugin {
        peer,
        controller,
        name: plugin_desc.name.clone(),
        id,
        manifest: plugin_desc.clone(),
        update_in_flight: Arc::new(AtomicBool::new(false)),
        coalesced_update: Arc::new(Mutex::new(None)),
        rpc_timeout: plugin_desc.rpc_timeout_ms.map(Duration::from_millis),
        termination: Arc::clone(&termination),
        local_request_ids: Arc::new(AtomicUsize::new(1)),
        local_pending_requests: Arc::new(Mutex::new(Vec::new())),
    };

    spawn_resource_monitor(plugin.manifest.clone(), plugin.controller_handle(), termination);

    if tracing_support::is_enabled() {
        plugin.toggle_tracing(true);
    }

    plugin
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

    configure_native_plugin_sandbox(&mut command, plugin_desc)?;

    let child = command.spawn().map_err(PluginStartErrorKind::Io)?;
    apply_windows_job_limits(&child, plugin_desc)?;
    Ok(child)
}

fn configure_native_plugin_sandbox(
    command: &mut ProcCommand,
    plugin_desc: &PluginDescription,
) -> Result<(), PluginStartErrorKind> {
    let _ = plugin_desc;

    #[cfg(target_os = "linux")]
    {
        configure_linux_plugin_sandbox(command)?;
    }

    #[cfg(target_os = "macos")]
    {
        let _ = command;
        info!(
            "plugin {} runtime sandbox unavailable on macOS; stable syscall filtering not yet implemented",
            plugin_desc.name
        );
    }

    #[cfg(windows)]
    {
        let _ = plugin_desc;
        let _ = command;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_linux_plugin_sandbox(command: &mut ProcCommand) -> Result<(), PluginStartErrorKind> {
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::os::unix::process::CommandExt;

    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};

    let denied = linux_denied_syscalls();
    let filter: BpfProgram = SeccompFilter::new(
        denied.into_iter().map(|syscall| (syscall, Vec::new())).collect::<BTreeMap<_, _>>(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH.try_into().map_err(|err| {
            PluginStartErrorKind::Sandbox(format!("unsupported seccomp target arch: {err:?}"))
        })?,
    )
    .map_err(|err| PluginStartErrorKind::Sandbox(format!("seccomp filter build failed: {err}")))?
    .try_into()
    .map_err(|err| PluginStartErrorKind::Sandbox(format!("seccomp BPF compile failed: {err}")))?;

    unsafe {
        command.pre_exec(move || {
            seccompiler::apply_filter(&filter).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("seccomp filter apply failed: {err}"),
                )
            })?;
            Ok(())
        });
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_denied_syscalls() -> Vec<i64> {
    let mut syscalls = vec![
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_ptrace,
        libc::SYS_socket,
        libc::SYS_open,
        libc::SYS_openat,
    ];

    #[cfg(target_arch = "x86")]
    {
        syscalls.push(libc::SYS_socketcall);
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
    {
        syscalls.push(libc::SYS_clone3);
    }

    syscalls.sort_unstable();
    syscalls.dedup();
    syscalls
}

#[cfg(windows)]
pub(crate) fn apply_windows_job_limits(
    child: &Child,
    plugin_desc: &PluginDescription,
) -> Result<(), PluginStartErrorKind> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_TIME,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    const DEFAULT_WINDOWS_PLUGIN_MAX_RSS_BYTES: usize = 512 * 1024 * 1024;
    const DEFAULT_WINDOWS_PLUGIN_MAX_CPU_100NS: i64 = 5 * 60 * 10_000_000;

    let job_memory_limit =
        plugin_desc.max_rss_bytes.unwrap_or(DEFAULT_WINDOWS_PLUGIN_MAX_RSS_BYTES as u64) as usize;
    let cpu_limit_100ns = plugin_desc
        .max_cpu_seconds
        .map(|seconds| seconds.saturating_mul(10_000_000) as i64)
        .unwrap_or(DEFAULT_WINDOWS_PLUGIN_MAX_CPU_100NS);

    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return Err(PluginStartErrorKind::Sandbox("CreateJobObjectW failed".to_string()));
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.BasicLimitInformation.PerProcessUserTimeLimit = cpu_limit_100ns;
        limits.JobMemoryLimit = job_memory_limit;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut limits as *mut _ as *mut _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return Err(PluginStartErrorKind::Sandbox(
                "SetInformationJobObject failed".to_string(),
            ));
        }

        let ok = AssignProcessToJobObject(job, child.as_raw_handle() as isize);
        if ok == 0 {
            CloseHandle(job);
            return Err(PluginStartErrorKind::Sandbox(
                "AssignProcessToJobObject failed".to_string(),
            ));
        }
    }

    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn apply_windows_job_limits(
    _child: &Child,
    _plugin_desc: &PluginDescription,
) -> Result<(), PluginStartErrorKind> {
    Ok(())
}

fn spawn_native_exit_watcher<W: xi_rpc::WriteTransport + 'static>(
    process: Arc<Mutex<Child>>,
    peer: xi_rpc::RawPeer<W>,
) {
    thread::spawn(move || {
        let exit_status = {
            let Some(mut child) = process.lock().ok() else {
                return;
            };
            child.wait().ok().and_then(|status| status.code())
        };
        peer.disconnect_with_error(xi_rpc::Error::PeerExited { exit_status });
    });
}

fn spawn_resource_monitor(
    plugin_desc: Arc<PluginDescription>,
    controller: Arc<dyn PluginController>,
    termination: Arc<PluginTerminationHandle>,
) {
    if plugin_desc.max_rss_bytes.is_none() && plugin_desc.max_cpu_seconds.is_none() {
        return;
    }

    thread::spawn(move || {
        while controller.has_exited().ok() == Some(false) {
            match controller.resource_usage() {
                Ok(Some(usage)) => {
                    if let (Some(limit), Some(observed)) =
                        (plugin_desc.max_rss_bytes, usage.rss_bytes)
                        && observed > limit
                    {
                        termination.notify_breach(PluginTerminationReason::MaxRssBytes {
                            limit_bytes: limit,
                            observed_bytes: observed,
                        });
                        return;
                    }
                    if let (Some(limit), Some(observed)) =
                        (plugin_desc.max_cpu_seconds, usage.cpu_seconds)
                        && observed > limit
                    {
                        termination.notify_breach(PluginTerminationReason::MaxCpuSeconds {
                            limit_seconds: limit,
                            observed_seconds: observed,
                        });
                        return;
                    }
                }
                Ok(None) => return,
                Err(err) => {
                    error!("plugin resource monitor failed: {:?}", err);
                    return;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

#[cfg(target_os = "linux")]
fn read_linux_process_resource_usage(pid: u32) -> io::Result<PluginResourceUsage> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let rss_bytes = status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb.saturating_mul(1024))
    });

    let after_paren = stat
        .rsplit_once(')')
        .map(|(_, tail)| tail.trim())
        .ok_or_else(|| io::Error::other("malformed /proc stat payload"))?;
    let fields = after_paren.split_whitespace().collect::<Vec<_>>();
    if fields.len() <= 12 {
        return Err(io::Error::other("incomplete /proc stat payload"));
    }
    let user_ticks = fields[11]
        .parse::<u64>()
        .map_err(|err| io::Error::other(format!("invalid utime ticks: {err}")))?;
    let system_ticks = fields[12]
        .parse::<u64>()
        .map_err(|err| io::Error::other(format!("invalid stime ticks: {err}")))?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return Err(io::Error::other("sysconf(_SC_CLK_TCK) failed"));
    }
    let cpu_seconds = Some((user_ticks + system_ticks) / ticks_per_second as u64);

    Ok(PluginResourceUsage { rss_bytes, cpu_seconds })
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

    #[cfg(target_os = "linux")]
    use super::linux_denied_syscalls;
    use super::rpc::PluginUpdate;
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
            None,
            test_termination_handle(),
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
