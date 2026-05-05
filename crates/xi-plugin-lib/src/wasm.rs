use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use xi_core_lib::plugin_rpc::{HostNotification, HostRequest};
use xi_rpc::{
    Callback, Error as RpcError, Handler as RpcHandler, Peer, RemoteError, RequestId, RpcCtx,
};

use crate::Plugin;
use crate::dispatch::Dispatcher;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "xi_host")]
unsafe extern "C" {
    fn send_notification(ptr: u32, len: u32);
    fn send_request(ptr: u32, len: u32) -> u64;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn send_notification(_ptr: u32, _len: u32) {
    panic!("xi-plugin-lib wasm transport only available on wasm32 targets")
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn send_request(_ptr: u32, _len: u32) -> u64 {
    panic!("xi-plugin-lib wasm transport only available on wasm32 targets")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timer {
    fire_after: Instant,
    token: usize,
}

impl Ord for Timer {
    fn cmp(&self, other: &Self) -> Ordering {
        other.fire_after.cmp(&self.fire_after)
    }
}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct RuntimeState {
    idle_queue: Mutex<VecDeque<usize>>,
    timers: Mutex<BinaryHeap<Timer>>,
}

impl RuntimeState {
    fn new() -> Self {
        Self { idle_queue: Mutex::new(VecDeque::new()), timers: Mutex::new(BinaryHeap::new()) }
    }

    fn schedule_idle(&self, token: usize) {
        self.idle_queue.lock().unwrap().push_back(token);
    }

    fn try_get_idle(&self) -> Option<usize> {
        self.idle_queue.lock().unwrap().pop_front()
    }

    fn schedule_timer(&self, fire_after: Instant, token: usize) {
        self.timers.lock().unwrap().push(Timer { fire_after, token });
    }

    fn cancel_timer(&self, token: usize) -> bool {
        let mut timers = self.timers.lock().unwrap();
        let original = timers.len();
        let mut drained = timers.drain().collect::<Vec<_>>();
        drained.retain(|timer| timer.token != token);
        timers.extend(drained);
        timers.len() != original
    }

    fn pop_ready_timer(&self) -> Option<usize> {
        let mut timers = self.timers.lock().unwrap();
        match timers.peek().copied() {
            Some(timer) if timer.fire_after <= Instant::now() => {
                timers.pop().map(|timer| timer.token)
            }
            _ => None,
        }
    }
}

#[derive(Clone)]
struct WasmPeer {
    state: Arc<RuntimeState>,
}

impl WasmPeer {
    fn new(state: Arc<RuntimeState>) -> Self {
        Self { state }
    }
}

impl Peer for WasmPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(self.clone())
    }

    fn send_rpc_notification(&self, method: &str, params: &Value) {
        let payload = serde_json::to_vec(&json!({ "method": method, "params": params }))
            .expect("plugin wasm notification payload should serialize");
        unsafe {
            send_notification(payload.as_ptr() as u32, payload.len() as u32);
        }
    }

    fn send_rpc_request_async(
        &self,
        method: &str,
        params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        let result = self.send_rpc_request(method, params);
        f.call(result);
        RequestId::Number(0)
    }

    fn send_rpc_request(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let payload = serde_json::to_vec(&json!({ "method": method, "params": params }))
            .map_err(|err| RpcError::PeerProtocolError { reason: err.to_string() })?;
        let packed = unsafe { send_request(payload.as_ptr() as u32, payload.len() as u32) };
        let (ptr, len) = unpack_ptr_len(packed)?;
        let bytes = unsafe { take_alloc(ptr, len) };
        let response: WasmGuestResponse = serde_json::from_slice(&bytes)
            .map_err(|err| RpcError::PeerProtocolError { reason: err.to_string() })?;
        response.into_result()
    }

    fn send_rpc_request_timeout(
        &self,
        method: &str,
        params: &Value,
        _timeout: Duration,
    ) -> Result<Value, RpcError> {
        self.send_rpc_request(method, params)
    }

    fn cancel_rpc_request(&self, _id: RequestId) -> bool {
        false
    }

    fn request_is_pending(&self) -> bool {
        false
    }

    fn schedule_idle(&self, token: usize) {
        self.state.schedule_idle(token);
    }

    fn schedule_timer(&self, after: Instant, token: usize) {
        self.state.schedule_timer(after, token);
    }

    fn cancel_timer(&self, token: usize) -> bool {
        self.state.cancel_timer(token)
    }

    fn request_shutdown(&self) {}
}

pub struct WasmPluginRuntime<P: Plugin> {
    dispatcher: Mutex<Dispatcher<P, Box<P>>>,
    state: Arc<RuntimeState>,
}

impl<P: Plugin> WasmPluginRuntime<P> {
    pub fn new(plugin: P) -> Self {
        Self {
            dispatcher: Mutex::new(Dispatcher::new(Box::new(plugin))),
            state: Arc::new(RuntimeState::new()),
        }
    }

    pub fn handle_notification(&self, bytes: &[u8]) -> Result<(), String> {
        let notification = serde_json::from_slice::<HostNotification>(bytes)
            .map_err(|err| format!("invalid host notification payload: {err}"))?;
        let peer = self.rpc_peer();
        let ctx = RpcCtx::new(Box::new(peer.clone()));
        let mut dispatcher = self.dispatcher.lock().unwrap();
        dispatcher.handle_notification(&ctx, notification);
        self.drain_idle(&mut dispatcher, &ctx);
        Ok(())
    }

    pub fn handle_request(&self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let request = serde_json::from_slice::<HostRequest>(bytes)
            .map_err(|err| format!("invalid host request payload: {err}"))?;
        let peer = self.rpc_peer();
        let ctx = RpcCtx::new(Box::new(peer.clone()));
        let mut dispatcher = self.dispatcher.lock().unwrap();
        let response = dispatcher.handle_request(&ctx, request, CancellationToken::new());
        self.drain_idle(&mut dispatcher, &ctx);
        serde_json::to_vec(&WasmGuestResponse::from_result(response))
            .map_err(|err| format!("failed to serialize wasm guest response: {err}"))
    }

    fn rpc_peer(&self) -> WasmPeer {
        WasmPeer::new(self.state.clone())
    }

    fn drain_idle(&self, dispatcher: &mut Dispatcher<P, Box<P>>, ctx: &RpcCtx) {
        while let Some(token) = self.state.try_get_idle().or_else(|| self.state.pop_ready_timer()) {
            dispatcher.idle(ctx, token);
        }
    }
}

pub fn alloc(len: usize) -> u32 {
    let mut buf = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as u32
}

/// # Safety
///
/// `ptr` must have been returned by `alloc` or `pack_output` in this module,
/// and `len` must match the original allocation capacity contract.
pub unsafe fn dealloc(ptr: u32, len: usize) {
    if ptr == 0 {
        return;
    }
    let capacity = len.max(1);
    let _ = unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, capacity) };
}

/// # Safety
///
/// `ptr..ptr + len` must reference a valid readable guest-memory region for
/// the duration of the returned slice borrow.
pub unsafe fn read_input(ptr: u32, len: u32) -> &'static [u8] {
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

pub fn pack_output(mut bytes: Vec<u8>) -> u64 {
    let ptr = bytes.as_mut_ptr() as u32;
    let len = bytes.len() as u32;
    std::mem::forget(bytes);
    ((len as u64) << 32) | ptr as u64
}

fn unpack_ptr_len(packed: u64) -> Result<(u32, usize), RpcError> {
    let ptr = (packed & 0xffff_ffff) as u32;
    let len = (packed >> 32) as usize;
    if ptr == 0 || len == 0 {
        return Err(RpcError::PeerProtocolError {
            reason: "wasm host returned empty buffer".into(),
        });
    }
    Ok((ptr, len))
}

unsafe fn take_alloc(ptr: u32, len: usize) -> Vec<u8> {
    unsafe { Vec::from_raw_parts(ptr as *mut u8, len, len) }
}
