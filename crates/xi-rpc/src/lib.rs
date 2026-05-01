// Copyright 2016 The xi-editor Authors.
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

//! Generic RPC handling (used for both front end and plugin communication).
//!
//! The RPC protocol is based on [JSON-RPC](http://www.jsonrpc.org/specification),
//! but with some modifications. Unlike JSON-RPC 2.0, requests and notifications
//! are allowed in both directions, rather than imposing client and server roles.
//! Further, the batch form is not supported.
//!
//! Requests and responses use JSON-RPC 2.0 envelopes, including the
//! `"jsonrpc": "2.0"` member.
#![allow(clippy::boxed_local, clippy::or_fun_call)]

mod error;
mod parse;
pub mod transport;

pub mod test_utils;

use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{trace, trace_span};

use std::cmp;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;

/// Maximum number of distinct tokens that may sit in the idle queue at once.
/// Duplicate tokens are already coalesced; this cap prevents unbounded growth
/// from many unique tokens sent by a runaway caller.
const MAX_IDLE_QUEUE_SIZE: usize = 256;

pub use crate::error::{Error, ReadError, RemoteError};
pub use crate::parse::RequestId;
use crate::parse::{Call, MessageReader, Response, RpcObject};
pub use crate::transport::{
    ContentLengthReader, ContentLengthWriter, NewlineReader, NewlineWriter, ReadTransport,
    WriteTransport,
};

/// An interface to access the other side of the RPC channel. The main purpose
/// is to send RPC requests and notifications to the peer.
///
/// A single shared `RawPeer` exists for each `RpcLoop`; a reference can
/// be taken with `RpcLoop::get_peer()`.
///
/// In general, `RawPeer` shouldn't be used directly, but behind a pointer as
/// the `Peer` trait object.
pub struct RawPeer<W: WriteTransport>(Arc<RpcState<W>>);

/// The `Peer` trait represents the interface for the other side of the RPC
/// channel. It is intended to be used behind a pointer, a trait object.
pub trait Peer: Send + 'static {
    /// Used to implement `clone` in an object-safe way.
    fn box_clone(&self) -> Box<dyn Peer>;
    /// Sends a notification (asynchronous RPC) to the peer.
    fn send_rpc_notification(&self, method: &str, params: &Value);
    /// Sends a request asynchronously, and the supplied callback will
    /// be called when the response arrives.
    fn send_rpc_request_async(
        &self,
        method: &str,
        params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId;
    /// Sends a request (synchronous RPC) to the peer, and waits for the result.
    fn send_rpc_request(&self, method: &str, params: &Value) -> Result<Value, Error>;
    /// Sends a synchronous request with an explicit timeout.
    fn send_rpc_request_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, Error>;
    /// Cancels a pending request by id.
    fn cancel_rpc_request(&self, id: RequestId) -> bool;
    /// Determines whether an incoming request (or notification) is pending.
    fn request_is_pending(&self) -> bool;
    /// Adds a token to the idle queue.
    fn schedule_idle(&self, token: usize);
    /// Like `schedule_idle`, with the guarantee that the handler's `idle`
    /// fn will not be called before the provided `Instant`.
    fn schedule_timer(&self, after: Instant, token: usize);
    /// Cancels a previously scheduled timer identified by `token`.
    fn cancel_timer(&self, token: usize) -> bool;
    /// Requests orderly shutdown of the current RPC loop.
    fn request_shutdown(&self);
}

/// The `Peer` trait object.
pub type RpcPeer = Box<dyn Peer>;

pub struct RpcCtx {
    peer: RpcPeer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// An RPC command.
pub struct RpcCall {
    pub method: String,
    pub params: Value,
}

/// A trait for types which can handle RPCs.
pub trait Handler {
    type Notification: DeserializeOwned;
    type Request: DeserializeOwned;
    fn handle_notification(&mut self, ctx: &RpcCtx, rpc: Self::Notification);
    fn handle_request(
        &mut self,
        ctx: &RpcCtx,
        rpc: Self::Request,
        cancel: CancellationToken,
    ) -> Result<Value, RemoteError>;
    #[allow(unused_variables)]
    fn idle(&mut self, ctx: &RpcCtx, token: usize) {}
}

pub trait Callback: Send {
    fn call(self: Box<Self>, result: Result<Value, Error>);
}

impl<F: Send + FnOnce(Result<Value, Error>)> Callback for F {
    fn call(self: Box<F>, result: Result<Value, Error>) {
        (*self)(result)
    }
}

/// A helper type which shuts down the runloop if a panic occurs while
/// handling an RPC.
struct PanicGuard<'a, W: WriteTransport>(&'a RawPeer<W>);

impl<'a, W: WriteTransport> Drop for PanicGuard<'a, W> {
    fn drop(&mut self) {
        if thread::panicking() {
            error!("panic guard hit, closing runloop");
            self.0.disconnect();
        }
    }
}

#[allow(dead_code)]
trait IdleProc: Send {
    fn call(self: Box<Self>, token: usize);
}

impl<F: Send + FnOnce(usize)> IdleProc for F {
    fn call(self: Box<F>, token: usize) {
        (*self)(token)
    }
}

enum ResponseHandler {
    Chan(mpsc::Sender<Result<Value, Error>>),
    Callback(Box<dyn Callback>),
}

impl ResponseHandler {
    fn invoke(self, result: Result<Value, Error>) {
        match self {
            ResponseHandler::Chan(tx) => {
                let _ = tx.send(result);
            }
            ResponseHandler::Callback(f) => f.call(result),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Timer {
    fire_after: Instant,
    token: usize,
}

struct RpcState<W: WriteTransport> {
    writer: Mutex<W>,
    id: AtomicUsize,
    pending: Mutex<HashMap<RequestId, ResponseHandler>>,
    idle_queue: Mutex<VecDeque<usize>>,
    timers: Mutex<BinaryHeap<Timer>>,
    needs_exit: AtomicBool,
    /// Count of non-response messages queued for the main loop.
    rx_pending: AtomicUsize,
    /// Notified when idle tokens or timers are added, waking the main loop.
    idle_notify: tokio::sync::Notify,
    /// Limits the number of concurrent synchronous outbound requests.
    request_semaphore: tokio::sync::Semaphore,
    max_in_flight: usize,
}

/// A structure holding the state of a main loop for handling RPC's.
pub struct RpcLoop<W: WriteTransport> {
    peer: RawPeer<W>,
}

impl<W: WriteTransport> RpcLoop<W> {
    /// Creates a new `RpcLoop` with the given write transport.
    pub fn new(writer: W) -> Self {
        Self::new_with_max_in_flight(writer, 1)
    }

    /// Creates a new `RpcLoop` allowing up to `max_in_flight` concurrent
    /// synchronous outbound requests.
    pub fn new_with_max_in_flight(writer: W, max_in_flight: usize) -> Self {
        assert!(max_in_flight >= 1, "max_in_flight must be at least 1");
        let rpc_peer = RawPeer(Arc::new(RpcState {
            writer: Mutex::new(writer),
            id: AtomicUsize::new(0),
            pending: Mutex::new(HashMap::new()),
            idle_queue: Mutex::new(VecDeque::new()),
            timers: Mutex::new(BinaryHeap::new()),
            needs_exit: AtomicBool::new(false),
            rx_pending: AtomicUsize::new(0),
            idle_notify: tokio::sync::Notify::new(),
            request_semaphore: tokio::sync::Semaphore::new(max_in_flight),
            max_in_flight,
        }));
        RpcLoop { peer: rpc_peer }
    }

    /// Gets a reference to the peer.
    pub fn get_raw_peer(&self) -> RawPeer<W> {
        self.peer.clone()
    }

    /// Starts the event loop, reading framed messages from the reader until
    /// EOF or an error occurs.
    pub fn mainloop<R, RF, H>(&mut self, rf: RF, handler: &mut H) -> Result<(), ReadError>
    where
        R: ReadTransport + Send + 'static,
        RF: FnOnce() -> R + Send + 'static,
        H: Handler,
    {
        let peer = self.get_raw_peer();

        let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");

            rt.block_on(async {
                peer.reset_needs_exit();
                let ctx = RpcCtx { peer: Box::new(peer.clone()) };
                let (msg_tx, mut msg_rx) =
                    tokio::sync::mpsc::unbounded_channel::<Result<RpcObject, ReadError>>();

                let peer_reader = peer.clone();
                let jh = tokio::task::spawn_blocking(move || {
                    let mut stream = rf();
                    let mut reader = MessageReader::default();
                    loop {
                        if peer_reader.needs_exit() {
                            trace!(target: "xi_rpc", "read loop exit");
                            break;
                        }
                        match reader.next(&mut stream) {
                            Ok(json) => {
                                if json.is_response() {
                                    let id = json.get_id().unwrap();
                                    let span = trace_span!(
                                        target: "xi_rpc",
                                        "read_loop_response",
                                        request_id = ?id
                                    );
                                    let _entered = span.enter();
                                    match json.into_response() {
                                        Ok(resp) => {
                                            peer_reader
                                                .handle_response(id, resp.map_err(Error::from));
                                        }
                                        Err(msg) => {
                                            error!("failed to parse response: {}", msg);
                                            peer_reader
                                                .handle_response(id, Err(Error::InvalidResponse));
                                        }
                                    }
                                } else {
                                    peer_reader.0.rx_pending.fetch_add(1, Ordering::Release);
                                    let _ = msg_tx.send(Ok(json));
                                }
                            }
                            Err(ReadError::BatchNotSupported) => {
                                peer_reader.0.rx_pending.fetch_add(1, Ordering::Release);
                                let _ = msg_tx.send(Err(ReadError::BatchNotSupported));
                                break;
                            }
                            Err(err) => {
                                // If a sync request is in-flight and we get an error, the
                                // response will never arrive — disconnect so the caller unblocks.
                                if peer_reader.0.request_semaphore.available_permits()
                                    < peer_reader.0.max_in_flight
                                {
                                    error!("read error with in-flight request: {}", err);
                                    peer_reader.disconnect();
                                }
                                peer_reader.0.rx_pending.fetch_add(1, Ordering::Release);
                                let _ = msg_tx.send(Err(err));
                                break;
                            }
                        }
                    }
                });

                let exit = loop {
                    let _guard = PanicGuard(&peer);

                    // Drain all expired timers.
                    while let Some(Ok(token)) = peer.check_timers() {
                        do_idle(handler, &ctx, token);
                    }

                    // Drain the idle queue.
                    while let Some(token) = peer.try_get_idle() {
                        do_idle(handler, &ctx, token);
                    }

                    // Compute how long to sleep until the next scheduled timer.
                    let sleep_dur = match peer.check_timers() {
                        Some(Err(dur)) => dur,
                        _ => Duration::from_secs(60),
                    };

                    let read_result = tokio::select! {
                        biased;
                        msg = msg_rx.recv() => {
                            match msg {
                                Some(msg) => {
                                    peer.0.rx_pending.fetch_sub(1, Ordering::Release);
                                    msg
                                }
                                // Channel closed — reader exited without sending EOF error.
                                None => break ReadError::Disconnect,
                            }
                        }
                        _ = peer.0.idle_notify.notified() => continue,
                        _ = tokio::time::sleep(sleep_dur) => continue,
                    };

                    let span = trace_span!(target: "xi_rpc", "mainloop_iteration");
                    let _entered = span.enter();

                    let json = match read_result {
                        Ok(json) => json,
                        Err(ReadError::BatchNotSupported) => {
                            let batch_err = json!({
                                "jsonrpc": "2.0",
                                "id": null,
                                "error": {
                                    "code": -32600,
                                    "message": "Batch requests are not supported"
                                }
                            });
                            if let Err(e) = peer.send_raw(&batch_err) {
                                peer.disconnect();
                                break ReadError::Io(e);
                            }
                            peer.disconnect();
                            break ReadError::BatchNotSupported;
                        }
                        Err(err) => {
                            trace!(target: "xi_rpc", error = %err, "main loop error");
                            if let Some(idle_token) = peer.try_get_idle() {
                                handler.idle(&ctx, idle_token);
                            }
                            peer.disconnect();
                            break err;
                        }
                    };

                    let method = json.get_method().map(String::from);
                    match json.into_rpc::<H::Notification, H::Request>() {
                        Call::Request(id, cmd) => {
                            let method = method.unwrap();
                            let span = trace_span!(
                                target: "xi_rpc",
                                "handle_request",
                                method = %method
                            );
                            let _entered = span.enter();
                            let cancel = CancellationToken::new();
                            let result = handler.handle_request(&ctx, cmd, cancel);
                            if let Err(err) = peer.respond(result, id) {
                                peer.disconnect();
                                break ReadError::Io(err);
                            }
                        }
                        Call::Notification(cmd) => {
                            let method = method.unwrap();
                            let span = trace_span!(
                                target: "xi_rpc",
                                "handle_notification",
                                method = %method
                            );
                            let _entered = span.enter();
                            handler.handle_notification(&ctx, cmd);
                        }
                        Call::InvalidRequest(id, err) => {
                            if let Err(io_err) = peer.respond(Err(err), id) {
                                peer.disconnect();
                                break ReadError::Io(io_err);
                            }
                        }
                        Call::UnknownNotification(err) => {
                            warn!("ignoring unknown notification: {}", err);
                        }
                    }
                };

                let _ = jh.await;
                exit
            })
        }))
        .unwrap_or_else(|_| {
            error!("reader thread panicked; run loop is terminating");
            ReadError::ThreadPanic
        });

        if exit.is_disconnect() { Ok(()) } else { Err(exit) }
    }
}

fn do_idle<H: Handler>(handler: &mut H, ctx: &RpcCtx, token: usize) {
    let span = trace_span!(target: "xi_rpc", "do_idle", token = token);
    let _entered = span.enter();
    handler.idle(ctx, token);
}

impl RpcCtx {
    pub fn get_peer(&self) -> &RpcPeer {
        &self.peer
    }

    /// Schedule the idle handler to be run when there are no requests pending.
    pub fn schedule_idle(&self, token: usize) {
        self.peer.schedule_idle(token)
    }

    /// Requests orderly shutdown of the current RPC loop.
    pub fn request_shutdown(&self) {
        self.peer.request_shutdown()
    }
}

impl<W: WriteTransport> Peer for RawPeer<W> {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new((*self).clone())
    }

    fn send_rpc_notification(&self, method: &str, params: &Value) {
        let span = trace_span!(target: "xi_rpc", "send_notification", method = %method);
        let _entered = span.enter();
        if let Err(e) = self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })) {
            error!("send error on send_rpc_notification method {}: {}", method, e);
        }
    }

    fn send_rpc_request_async(
        &self,
        method: &str,
        params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        let span = trace_span!(target: "xi_rpc", "send_request_async", method = %method);
        let _entered = span.enter();
        self.send_rpc_request_common(method, params, ResponseHandler::Callback(f))
    }

    fn send_rpc_request(&self, method: &str, params: &Value) -> Result<Value, Error> {
        let span = trace_span!(target: "xi_rpc", "send_request_sync", method = %method);
        let _entered = span.enter();
        // Acquire a semaphore permit to track the in-flight sync request.
        // Released automatically when `_permit` drops.
        let _permit = loop {
            match self.0.request_semaphore.try_acquire() {
                Ok(p) => break p,
                Err(_) => thread::yield_now(),
            }
        };
        let (tx, rx) = mpsc::channel();
        self.send_rpc_request_common(method, params, ResponseHandler::Chan(tx));
        rx.recv().unwrap_or(Err(Error::PeerDisconnect))
    }

    fn send_rpc_request_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let span = trace_span!(target: "xi_rpc", "send_request_timeout", method = %method);
        let _entered = span.enter();
        let deadline = Instant::now() + timeout;
        let _permit = loop {
            match self.0.request_semaphore.try_acquire() {
                Ok(p) => break p,
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(Error::RequestTimeout);
                    }
                    thread::yield_now();
                }
            }
        };
        let (tx, rx) = mpsc::channel();
        self.send_rpc_request_common(method, params, ResponseHandler::Chan(tx));
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::RequestTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::PeerDisconnect),
        }
    }

    fn cancel_rpc_request(&self, id: RequestId) -> bool {
        let handler = {
            let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(&id)
        };
        match handler {
            Some(rh) => {
                rh.invoke(Err(Error::RequestCancelled));
                true
            }
            None => false,
        }
    }

    fn request_is_pending(&self) -> bool {
        self.0.rx_pending.load(Ordering::Acquire) > 0
    }

    fn schedule_idle(&self, token: usize) {
        let mut queue = self.0.idle_queue.lock().unwrap_or_else(|e| e.into_inner());
        if !queue.contains(&token) {
            // Bound the idle queue to prevent unbounded accumulation of
            // distinct tokens from a misbehaving or runaway caller.
            if queue.len() >= MAX_IDLE_QUEUE_SIZE {
                warn!(
                    "idle queue at capacity ({}), dropping token {}",
                    MAX_IDLE_QUEUE_SIZE, token
                );
                return;
            }
            queue.push_back(token);
            // Wake the main loop so it picks up the new idle task promptly.
            self.0.idle_notify.notify_one();
        }
    }

    fn schedule_timer(&self, after: Instant, token: usize) {
        self.0
            .timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Timer { fire_after: after, token });
        // Wake the main loop to re-evaluate the earliest timer deadline.
        self.0.idle_notify.notify_one();
    }

    fn cancel_timer(&self, token: usize) -> bool {
        let mut timers = self.0.timers.lock().unwrap_or_else(|e| e.into_inner());
        let before = timers.len();
        let remaining: Vec<Timer> = timers.drain().filter(|t| t.token != token).collect();
        let removed = before - remaining.len();
        *timers = remaining.into_iter().collect();
        removed > 0
    }

    fn request_shutdown(&self) {
        self.disconnect();
    }
}

impl<W: WriteTransport> RawPeer<W> {
    fn send(&self, v: &Value) -> Result<(), io::Error> {
        let span = trace_span!(target: "xi_rpc", "send_raw_message");
        let _entered = span.enter();
        let s = serde_json::to_string(v).unwrap();
        self.0.writer.lock().unwrap_or_else(|e| e.into_inner()).write_message(s.as_bytes())
    }

    /// Like `send` but accessible outside of the `RawPeer` impl block.
    pub(crate) fn send_raw(&self, v: &Value) -> Result<(), io::Error> {
        self.send(v)
    }

    fn respond(&self, result: Response, id: RequestId) -> Result<(), io::Error> {
        let mut response = json!({ "jsonrpc": "2.0", "id": id });
        match result {
            Ok(result) => response["result"] = result,
            Err(error) => response["error"] = json!(error),
        };
        self.send(&response)
    }

    fn send_rpc_request_common(
        &self,
        method: &str,
        params: &Value,
        rh: ResponseHandler,
    ) -> RequestId {
        let id = RequestId::Number(self.0.id.fetch_add(1, Ordering::Relaxed) as u64);
        {
            let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(id.clone(), rh);
        }
        if let Err(e) = self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })) {
            let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(rh) = pending.remove(&id) {
                rh.invoke(Err(Error::Io(e)));
            }
        }
        id
    }

    fn handle_response(&self, id: RequestId, resp: Result<Value, Error>) {
        let handler = {
            let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(&id)
        };
        match handler {
            Some(responsehandler) => responsehandler.invoke(resp),
            None => warn!("id {:?} not found in pending", id),
        }
    }

    fn try_get_idle(&self) -> Option<usize> {
        self.0.idle_queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    /// Checks status of the most imminent timer.
    fn check_timers(&self) -> Option<Result<usize, Duration>> {
        let mut timers = self.0.timers.lock().unwrap_or_else(|e| e.into_inner());
        match timers.peek() {
            None => return None,
            Some(t) => {
                let now = Instant::now();
                if t.fire_after > now {
                    return Some(Err(t.fire_after - now));
                }
            }
        }
        Some(Ok(timers.pop().unwrap().token))
    }

    /// Send disconnect error to pending requests and signal exit.
    fn disconnect(&self) {
        let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
        let ids = pending.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let callback = pending.remove(&id).unwrap();
            callback.invoke(Err(Error::PeerDisconnect));
        }
        self.0.needs_exit.store(true, Ordering::Release);
    }

    fn needs_exit(&self) -> bool {
        self.0.needs_exit.load(Ordering::Acquire)
    }

    fn reset_needs_exit(&self) {
        self.0.needs_exit.store(false, Ordering::Release);
    }
}

impl Clone for Box<dyn Peer> {
    fn clone(&self) -> Box<dyn Peer> {
        self.box_clone()
    }
}

impl<W: WriteTransport> Clone for RawPeer<W> {
    fn clone(&self) -> Self {
        RawPeer(self.0.clone())
    }
}

impl Ord for Timer {
    fn cmp(&self, other: &Timer) -> cmp::Ordering {
        other.fire_after.cmp(&self.fire_after)
    }
}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Timer) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_notif() {
        let reader = MessageReader::default();
        let json = reader.parse(r#"{"method": "hi", "params": {"words": "plz"}}"#).unwrap();
        assert!(!json.is_response());
        let rpc = json.into_rpc::<Value, Value>();
        match rpc {
            Call::Notification(_) => (),
            _ => panic!("parse failed"),
        }
    }

    #[test]
    fn test_parse_req() {
        let reader = MessageReader::default();
        let json =
            reader.parse(r#"{"id": 5, "method": "hi", "params": {"words": "plz"}}"#).unwrap();
        assert!(!json.is_response());
        let rpc = json.into_rpc::<Value, Value>();
        match rpc {
            Call::Request(..) => (),
            _ => panic!("parse failed"),
        }
    }

    #[test]
    fn test_parse_bad_json() {
        let reader = MessageReader::default();
        let json =
            reader.parse(r#"{"id": 5, "method": "hi", params: {"words": "plz"}}"#).err().unwrap();

        match json {
            ReadError::Json(..) => (),
            _ => panic!("parse failed"),
        }
        let json = reader.parse(r#"[5, "hi", {"arg": "val"}]"#).err().unwrap();

        match json {
            ReadError::BatchNotSupported => (),
            _ => panic!("parse failed"),
        }
    }
}
