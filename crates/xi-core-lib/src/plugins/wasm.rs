use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasmtime::{Caller, Engine, Extern, Linker, Memory, Module, Store, TypedFunc};

use super::manifest::PluginDescription;
use super::rpc::{HostNotification, HostRequest, PluginCommand, PluginNotification, PluginRequest};
use super::{PluginController, PluginId, PluginStartErrorKind, build_plugin};
use crate::WeakXiCore;
use xi_rpc::{Callback, Error as RpcError, Peer, ReadError, RemoteError, RequestId, RpcPeer};

const HOST_MODULE: &str = "xi_host";
const IMPORT_SEND_NOTIFICATION: &str = "send_notification";
const IMPORT_SEND_REQUEST: &str = "send_request";
const EXPORT_MEMORY: &str = "memory";
const EXPORT_ALLOC: &str = "alloc";
const EXPORT_DEALLOC: &str = "dealloc";
const EXPORT_HANDLE_NOTIFICATION: &str = "handle_host_notification";
const EXPORT_HANDLE_REQUEST: &str = "handle_host_request";

pub(super) fn run_wasm_plugin(
    plugin_desc: Arc<PluginDescription>,
    id: PluginId,
    core: WeakXiCore,
) -> Result<(), PluginStartErrorKind> {
    let (command_tx, command_rx) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let controller: Arc<dyn PluginController> = Arc::new(WasmProcessController {
        command_tx: command_tx.clone(),
        exited: Arc::clone(&exited),
    });
    let peer: RpcPeer = Box::new(WasmPeer::new(command_tx, Arc::clone(&exited)));
    let plugin = build_plugin(peer, controller, plugin_desc.clone(), id, core.clone());
    let mut guest = WasmGuest::new(plugin_desc, core.clone())?;

    core.plugin_connect(Ok(plugin));
    let exit = guest.run(id, command_rx, exited);
    core.plugin_exit(id, exit);
    Ok(())
}

struct WasmProcessController {
    command_tx: Sender<WasmCommand>,
    exited: Arc<AtomicBool>,
}

impl PluginController for WasmProcessController {
    fn has_exited(&self) -> io::Result<bool> {
        Ok(self.exited.load(Ordering::Acquire))
    }

    fn terminate(&self) -> io::Result<()> {
        let _ = self.command_tx.send(WasmCommand::ForceStop);
        Ok(())
    }
}

struct WasmPeer {
    command_tx: Sender<WasmCommand>,
    exited: Arc<AtomicBool>,
    next_request_id: Arc<AtomicUsize>,
    pending_async: Arc<Mutex<HashMap<RequestId, Arc<AtomicBool>>>>,
}

impl WasmPeer {
    fn new(command_tx: Sender<WasmCommand>, exited: Arc<AtomicBool>) -> Self {
        Self {
            command_tx,
            exited,
            next_request_id: Arc::new(AtomicUsize::new(0)),
            pending_async: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn next_id(&self) -> RequestId {
        RequestId::Number(self.next_request_id.fetch_add(1, Ordering::Relaxed) as u64)
    }

    fn send_request_inner(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Receiver<Result<Value, RpcError>>, RpcError> {
        let (response_tx, response_rx) = mpsc::channel();
        self.command_tx
            .send(WasmCommand::Request {
                method: method.to_string(),
                params: params.clone(),
                response_tx,
            })
            .map_err(|_| RpcError::PeerExited { exit_status: None })?;
        Ok(response_rx)
    }
}

impl Peer for WasmPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(Self {
            command_tx: self.command_tx.clone(),
            exited: Arc::clone(&self.exited),
            next_request_id: Arc::clone(&self.next_request_id),
            pending_async: Arc::clone(&self.pending_async),
        })
    }

    fn send_rpc_notification(&self, method: &str, params: &Value) {
        let _ = self
            .command_tx
            .send(WasmCommand::Notification { method: method.to_string(), params: params.clone() });
    }

    fn send_rpc_request_async(
        &self,
        method: &str,
        params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        let request_id = self.next_id();
        let request_id_for_worker = request_id.clone();
        let canceled = Arc::new(AtomicBool::new(false));
        self.pending_async.lock().unwrap().insert(request_id.clone(), Arc::clone(&canceled));
        let pending = Arc::clone(&self.pending_async);
        match self.send_request_inner(method, params) {
            Ok(response_rx) => {
                thread::spawn(move || {
                    let result = response_rx
                        .recv()
                        .unwrap_or(Err(RpcError::PeerExited { exit_status: None }));
                    pending.lock().unwrap().remove(&request_id_for_worker);
                    if !canceled.load(Ordering::Acquire) {
                        f.call(result);
                    }
                });
            }
            Err(err) => {
                self.pending_async.lock().unwrap().remove(&request_id);
                thread::spawn(move || f.call(Err(err)));
            }
        }
        request_id
    }

    fn send_rpc_request(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let response_rx = self.send_request_inner(method, params)?;
        response_rx.recv().unwrap_or(Err(RpcError::PeerExited { exit_status: None }))
    }

    fn send_rpc_request_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let response_rx = self.send_request_inner(method, params)?;
        match response_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(RpcError::PeerTimedOut {
                after_ms: timeout.as_millis().min(u64::MAX as u128) as u64,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(RpcError::PeerExited { exit_status: None }),
        }
    }

    fn cancel_rpc_request(&self, id: RequestId) -> bool {
        self.pending_async
            .lock()
            .unwrap()
            .remove(&id)
            .map(|flag| {
                flag.store(true, Ordering::Release);
                true
            })
            .unwrap_or(false)
    }

    fn request_is_pending(&self) -> bool {
        !self.exited.load(Ordering::Acquire)
    }

    fn schedule_idle(&self, _token: usize) {}

    fn schedule_timer(&self, _after: Instant, _token: usize) {}

    fn cancel_timer(&self, _token: usize) -> bool {
        false
    }

    fn request_shutdown(&self) {
        let _ = self.command_tx.send(WasmCommand::ForceStop);
    }
}

enum WasmCommand {
    Notification { method: String, params: Value },
    Request { method: String, params: Value, response_tx: Sender<Result<Value, RpcError>> },
    ForceStop,
}

#[derive(Clone)]
struct WasmHostBridge {
    core: WeakXiCore,
}

impl WasmHostBridge {
    fn handle_notification(&self, cmd: PluginCommand<PluginNotification>) {
        self.core.handle_plugin_notification_message(cmd.view_id, cmd.plugin_id, cmd.cmd);
    }

    fn handle_request(&self, cmd: PluginCommand<PluginRequest>) -> Result<Value, RemoteError> {
        self.core.handle_plugin_request_message(cmd.view_id, cmd.plugin_id, cmd.cmd)
    }
}

struct WasmStoreState {
    bridge: WasmHostBridge,
}

struct WasmGuest {
    store: Store<WasmStoreState>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    dealloc: TypedFunc<(i32, i32), ()>,
    handle_notification: TypedFunc<(i32, i32), ()>,
    handle_request: TypedFunc<(i32, i32), i64>,
}

impl WasmGuest {
    fn new(
        plugin_desc: Arc<PluginDescription>,
        core: WeakXiCore,
    ) -> Result<Self, PluginStartErrorKind> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, &plugin_desc.exec_path).map_err(|err| {
            PluginStartErrorKind::Wasm(format!("failed to compile wasm module: {err}"))
        })?;
        let bridge = WasmHostBridge { core };
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap(
                HOST_MODULE,
                IMPORT_SEND_NOTIFICATION,
                |mut caller: Caller<'_, WasmStoreState>,
                 ptr: i32,
                 len: i32|
                 -> wasmtime::Result<()> {
                    let bytes = read_memory_from_caller(&mut caller, ptr, len)?;
                    let cmd = serde_json::from_slice::<PluginCommand<PluginNotification>>(&bytes)
                        .map_err(|err| {
                        wasmtime::Error::msg(format!("invalid plugin notification: {err}"))
                    })?;
                    caller.data().bridge.handle_notification(cmd);
                    Ok(())
                },
            )
            .map_err(|err| {
                PluginStartErrorKind::Wasm(format!("failed to bind notification import: {err}"))
            })?;
        linker
            .func_wrap(
                HOST_MODULE,
                IMPORT_SEND_REQUEST,
                |mut caller: Caller<'_, WasmStoreState>,
                 ptr: i32,
                 len: i32|
                 -> wasmtime::Result<i64> {
                    let bytes = read_memory_from_caller(&mut caller, ptr, len)?;
                    let cmd = serde_json::from_slice::<PluginCommand<PluginRequest>>(&bytes)
                        .map_err(|err| {
                            wasmtime::Error::msg(format!("invalid plugin request: {err}"))
                        })?;
                    let payload = serde_json::to_vec(&WasmGuestResponse::from_result(
                        caller.data().bridge.handle_request(cmd),
                    ))
                    .map_err(|err| {
                        wasmtime::Error::msg(format!("failed to serialize host response: {err}"))
                    })?;
                    write_memory_for_caller(&mut caller, &payload)
                },
            )
            .map_err(|err| {
                PluginStartErrorKind::Wasm(format!("failed to bind request import: {err}"))
            })?;

        let mut store = Store::new(&engine, WasmStoreState { bridge });
        let instance = linker.instantiate(&mut store, &module).map_err(|err| {
            PluginStartErrorKind::Wasm(format!("failed to instantiate wasm plugin: {err}"))
        })?;
        let memory = instance.get_memory(&mut store, EXPORT_MEMORY).ok_or_else(|| {
            PluginStartErrorKind::Wasm("wasm plugin missing exported memory".into())
        })?;
        let alloc =
            instance.get_typed_func::<i32, i32>(&mut store, EXPORT_ALLOC).map_err(|err| {
                PluginStartErrorKind::Wasm(format!("wasm plugin missing alloc(len) export: {err}"))
            })?;
        let dealloc = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, EXPORT_DEALLOC)
            .map_err(|err| {
                PluginStartErrorKind::Wasm(format!(
                    "wasm plugin missing dealloc(ptr,len) export: {err}"
                ))
            })?;
        let handle_notification = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, EXPORT_HANDLE_NOTIFICATION)
            .map_err(|err| {
                PluginStartErrorKind::Wasm(format!(
                    "wasm plugin missing handle_host_notification(ptr,len) export: {err}"
                ))
            })?;
        let handle_request = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, EXPORT_HANDLE_REQUEST)
            .map_err(|err| {
                PluginStartErrorKind::Wasm(format!(
                    "wasm plugin missing handle_host_request(ptr,len) export: {err}"
                ))
            })?;

        Ok(Self { store, memory, alloc, dealloc, handle_notification, handle_request })
    }

    fn run(
        &mut self,
        _id: PluginId,
        command_rx: Receiver<WasmCommand>,
        exited: Arc<AtomicBool>,
    ) -> Result<(), ReadError> {
        let result = loop {
            match command_rx.recv() {
                Ok(WasmCommand::Notification { method, params }) => {
                    let is_shutdown = method == "shutdown";
                    match self.dispatch_notification(&method, &params) {
                        Ok(()) if is_shutdown => break Ok(()),
                        Ok(()) => {}
                        Err(err) => break Err(err),
                    }
                }
                Ok(WasmCommand::Request { method, params, response_tx }) => {
                    let result = self.dispatch_request(&method, &params);
                    let _ = response_tx.send(result);
                }
                Ok(WasmCommand::ForceStop) | Err(_) => break Ok(()),
            }
        };

        exited.store(true, Ordering::Release);
        result
    }

    fn dispatch_notification(&mut self, method: &str, params: &Value) -> Result<(), ReadError> {
        let notification = host_notification_from_rpc(method, params)
            .map_err(|err| ReadError::Io(io::Error::other(format!("{err:?}"))))?;
        let request_ptr = self.write_guest_bytes(
            &serde_json::to_vec(&notification)
                .map_err(|err| ReadError::Io(io::Error::other(err.to_string())))?,
        )?;
        let result = self
            .handle_notification
            .call(&mut self.store, request_ptr)
            .map_err(|err| ReadError::Io(io::Error::other(err.to_string())));
        let _ = self.dealloc.call(&mut self.store, request_ptr);
        result
    }

    fn dispatch_request(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let request = host_request_from_rpc(method, params)?;
        let request_bytes = serde_json::to_vec(&request)
            .map_err(|err| RpcError::Io(io::Error::other(err.to_string())))?;
        let request_ptr = self
            .write_guest_bytes(&request_bytes)
            .map_err(|err| RpcError::Io(io::Error::other(err.to_string())))?;
        let response_ptr = self
            .handle_request
            .call(&mut self.store, request_ptr)
            .map_err(|err| RpcError::Io(io::Error::other(err.to_string())))?;
        let _ = self.dealloc.call(&mut self.store, request_ptr);
        let (ptr, len) =
            unpack_ptr_len(response_ptr).map_err(|err| RpcError::Io(io::Error::other(err)))?;
        let bytes = self
            .read_guest_bytes(ptr, len)
            .map_err(|err| RpcError::Io(io::Error::other(format!("{err:?}"))))?;
        let _ = self.dealloc.call(&mut self.store, (ptr as i32, len as i32));
        let response: WasmGuestResponse = serde_json::from_slice(&bytes)
            .map_err(|err| RpcError::Io(io::Error::other(err.to_string())))?;
        response.into_result()
    }

    fn write_guest_bytes(&mut self, bytes: &[u8]) -> Result<(i32, i32), ReadError> {
        let ptr = self
            .alloc
            .call(&mut self.store, bytes.len() as i32)
            .map_err(|err| ReadError::Io(io::Error::other(err.to_string())))?;
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|err| ReadError::Io(io::Error::other(err.to_string())))?;
        Ok((ptr, bytes.len() as i32))
    }

    fn read_guest_bytes(&mut self, ptr: usize, len: usize) -> Result<Vec<u8>, wasmtime::Error> {
        let mut buf = vec![0; len];
        self.memory.read(&mut self.store, ptr, &mut buf)?;
        Ok(buf)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
enum WasmGuestResponse {
    Ok(Value),
    Err(RemoteError),
}

impl WasmGuestResponse {
    fn from_result(result: Result<Value, RemoteError>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(err) => Self::Err(err),
        }
    }

    fn into_result(self) -> Result<Value, RpcError> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(err) => Err(RpcError::RemoteError(err)),
        }
    }
}

fn host_notification_from_rpc(method: &str, params: &Value) -> Result<HostNotification, RpcError> {
    serde_json::from_value(json!({ "method": method, "params": params }))
        .map_err(|err| RpcError::Io(io::Error::other(err.to_string())))
}

fn host_request_from_rpc(method: &str, params: &Value) -> Result<HostRequest, RpcError> {
    serde_json::from_value(json!({ "method": method, "params": params }))
        .map_err(|err| RpcError::Io(io::Error::other(err.to_string())))
}

fn pack_ptr_len(ptr: i32, len: i32) -> i64 {
    ((len as u32 as u64) << 32 | (ptr as u32 as u64)) as i64
}

fn unpack_ptr_len(packed: i64) -> Result<(usize, usize), String> {
    let raw = packed as u64;
    let ptr = (raw & 0xffff_ffff) as usize;
    let len = (raw >> 32) as usize;
    if len == 0 {
        return Err("wasm response had zero length buffer".into());
    }
    Ok((ptr, len))
}

fn read_memory_from_caller(
    caller: &mut Caller<'_, WasmStoreState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<Vec<u8>> {
    let memory = caller
        .get_export(EXPORT_MEMORY)
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest memory export missing"))?;
    let mut buf = vec![0; len as usize];
    memory.read(caller, ptr as usize, &mut buf)?;
    Ok(buf)
}

fn write_memory_for_caller(
    caller: &mut Caller<'_, WasmStoreState>,
    bytes: &[u8],
) -> wasmtime::Result<i64> {
    let memory = caller
        .get_export(EXPORT_MEMORY)
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest memory export missing"))?;
    let alloc = caller
        .get_export(EXPORT_ALLOC)
        .and_then(Extern::into_func)
        .ok_or_else(|| wasmtime::Error::msg("guest alloc export missing"))?
        .typed::<i32, i32>(&mut *caller)?;
    let ptr = alloc.call(&mut *caller, bytes.len() as i32)?;
    memory.write(caller, ptr as usize, bytes)?;
    Ok(pack_ptr_len(ptr, bytes.len() as i32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dummy_weak_core;
    use crate::plugins::manifest::{PluginLaunchConfig, PluginRuntime, PluginScope};

    #[test]
    fn ptr_len_roundtrip() {
        let packed = pack_ptr_len(42, 256);
        assert_eq!(unpack_ptr_len(packed).unwrap(), (42, 256));
    }

    #[test]
    fn wasm_guest_handles_collect_trace_requests() {
        let mut guest = test_guest(wasm_collect_trace_module());

        let value = guest.dispatch_request("collect_trace", &json!({})).unwrap();

        assert_eq!(value, json!({ "trace": "ok" }));
    }

    #[test]
    fn wasm_guest_can_send_plugin_notification_to_core() {
        let mut guest = test_guest(wasm_notification_module());

        guest.dispatch_notification("ping", &json!([])).unwrap();
    }

    fn test_guest(module_text: &'static str) -> WasmGuest {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), module_text).unwrap();
        let manifest = PluginDescription {
            name: "wasm-test".into(),
            version: "0.1.0".into(),
            requires: Vec::new(),
            scope: PluginScope::Global,
            runtime: PluginRuntime::Wasm,
            capabilities: Vec::new(),
            launch: PluginLaunchConfig::default(),
            max_rss_bytes: None,
            max_cpu_seconds: None,
            rpc_timeout_ms: None,
            exec_path: file.path().to_path_buf(),
            activations: Vec::new(),
            commands: Vec::new(),
            languages: Vec::new(),
        };
        WasmGuest::new(Arc::new(manifest), dummy_weak_core()).unwrap()
    }

    fn wasm_collect_trace_module() -> &'static str {
        r#"(module
            (memory (export "memory") 1)
            (global $heap (mut i32) (i32.const 512))
            (data (i32.const 0) "{\22status\22:\22ok\22,\22payload\22:{\22trace\22:\22ok\22}}")
            (func (export "alloc") (param $len i32) (result i32)
                (local $ptr i32)
                global.get $heap
                local.tee $ptr
                local.get $len
                i32.add
                global.set $heap
                local.get $ptr)
            (func (export "dealloc") (param i32 i32))
            (func (export "handle_host_notification") (param i32 i32))
            (func (export "handle_host_request") (param i32 i32) (result i64)
                i64.const 171798691840)
        )"#
    }

    fn wasm_notification_module() -> &'static str {
        r#"(module
            (import "xi_host" "send_notification" (func $send_notification (param i32 i32)))
            (memory (export "memory") 1)
            (global $heap (mut i32) (i32.const 512))
            (data (i32.const 64) "{\22method\22:\22alert\22,\22params\22:{\22view_id\22:\22view-id-1\22,\22plugin_id\22:7,\22msg\22:\22hello\22}}")
            (func (export "alloc") (param $len i32) (result i32)
                (local $ptr i32)
                global.get $heap
                local.tee $ptr
                local.get $len
                i32.add
                global.set $heap
                local.get $ptr)
            (func (export "dealloc") (param i32 i32))
            (func (export "handle_host_notification") (param i32 i32)
                i32.const 64
                i32.const 79
                call $send_notification)
            (func (export "handle_host_request") (param i32 i32) (result i64)
                i64.const 0)
        )"#
    }
}
