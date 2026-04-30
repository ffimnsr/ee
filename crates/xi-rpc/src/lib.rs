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
use tracing::{trace, trace_span};

use std::cmp;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;

pub use crate::error::{Error, ReadError, RemoteError};
use crate::parse::{Call, MessageReader, Response, RpcObject};
pub use crate::parse::RequestId;
pub use crate::transport::{
    ContentLengthReader, ContentLengthWriter, NewlineReader, NewlineWriter, ReadTransport,
    WriteTransport,
};

/// The maximum duration we will block on a reader before checking for an task.
const MAX_IDLE_WAIT: Duration = Duration::from_millis(5);

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
    /// For an explanation on this approach, see
    /// [this thread](https://users.rust-lang.org/t/solved-is-it-possible-to-clone-a-boxed-trait-object/1714/6).
    fn box_clone(&self) -> Box<dyn Peer>;
    /// Sends a notification (asynchronous RPC) to the peer.
    fn send_rpc_notification(&self, method: &str, params: &Value);
    /// Sends a request asynchronously, and the supplied callback will
    /// be called when the response arrives.
    ///
    /// `Callback` is an alias for `FnOnce(Result<Value, Error>)`; it must
    /// be boxed because trait objects cannot use generic paramaters.
    ///
    /// Returns the request id, which can be passed to `cancel_rpc_request`.
    fn send_rpc_request_async(
        &self,
        method: &str,
        params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId;
    /// Sends a request (synchronous RPC) to the peer, and waits for the result.
    fn send_rpc_request(&self, method: &str, params: &Value) -> Result<Value, Error>;
    /// Sends a synchronous request with an explicit timeout.
    ///
    /// Returns `Err(Error::RequestTimeout)` if no response arrives within `timeout`.
    fn send_rpc_request_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, Error>;
    /// Cancels a pending request by id (obtained from `send_rpc_request_async`).
    ///
    /// If the request is still pending the registered callback is invoked with
    /// `Err(Error::RequestCancelled)` and the method returns `true`.  Returns
    /// `false` if no matching pending request was found.
    fn cancel_rpc_request(&self, id: RequestId) -> bool;
    /// Determines whether an incoming request (or notification) is
    /// pending. This is intended to reduce latency for bulk operations
    /// done in the background.
    fn request_is_pending(&self) -> bool;
    /// Adds a token to the idle queue. When the runloop is idle and the
    /// queue is not empty, the handler's `idle` fn will be called
    /// with the earliest added token.
    ///
    /// Duplicate tokens are coalesced: if the same token is already
    /// queued, this call is a no-op.
    fn schedule_idle(&self, token: usize);
    /// Like `schedule_idle`, with the guarantee that the handler's `idle`
    /// fn will not be called _before_ the provided `Instant`.
    ///
    /// # Note
    ///
    /// This is not intended as a high-fidelity timer. Regular RPC messages
    /// will always take priority over an idle task.
    fn schedule_timer(&self, after: Instant, token: usize);
    /// Cancels a previously scheduled timer identified by `token`.
    ///
    /// Returns `true` if at least one matching timer was found and removed,
    /// `false` if no timer with that token was pending.
    fn cancel_timer(&self, token: usize) -> bool;
}

/// The `Peer` trait object.
pub type RpcPeer = Box<dyn Peer>;

pub struct RpcCtx {
    peer: RpcPeer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// An RPC command.
///
/// This type is used as a placeholder in various places, and can be
/// used by clients as a catchall type for implementing `MethodHandler`.
pub struct RpcCall {
    pub method: String,
    pub params: Value,
}

/// A trait for types which can handle RPCs.
///
/// Types which implement `MethodHandler` are also responsible for implementing
/// `Parser`; `Parser` is provided when Self::Notification and Self::Request
/// can be used with serde::DeserializeOwned.
pub trait Handler {
    type Notification: DeserializeOwned;
    type Request: DeserializeOwned;
    fn handle_notification(&mut self, ctx: &RpcCtx, rpc: Self::Notification);
    fn handle_request(&mut self, ctx: &RpcCtx, rpc: Self::Request) -> Result<Value, RemoteError>;
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
    rx_queue: Mutex<VecDeque<Result<RpcObject, ReadError>>>,
    rx_cvar: Condvar,
    writer: Mutex<W>,
    id: AtomicUsize,
    pending: Mutex<HashMap<RequestId, ResponseHandler>>,
    idle_queue: Mutex<VecDeque<usize>>,
    timers: Mutex<BinaryHeap<Timer>>,
    needs_exit: AtomicBool,
    is_blocked: AtomicBool,
}

/// A structure holding the state of a main loop for handling RPC's.
pub struct RpcLoop<W: WriteTransport> {
    reader: MessageReader,
    peer: RawPeer<W>,
}

impl<W: WriteTransport> RpcLoop<W> {
    /// Creates a new `RpcLoop` with the given write transport (used for
    /// sending requests, notifications, and responses).
    pub fn new(writer: W) -> Self {
        let rpc_peer = RawPeer(Arc::new(RpcState {
            rx_queue: Mutex::new(VecDeque::new()),
            rx_cvar: Condvar::new(),
            writer: Mutex::new(writer),
            id: AtomicUsize::new(0),
            pending: Mutex::new(HashMap::new()),
            idle_queue: Mutex::new(VecDeque::new()),
            timers: Mutex::new(BinaryHeap::new()),
            needs_exit: AtomicBool::new(false),
            is_blocked: AtomicBool::new(false),
        }));
        RpcLoop { reader: MessageReader::default(), peer: rpc_peer }
    }

    /// Gets a reference to the peer.
    pub fn get_raw_peer(&self) -> RawPeer<W> {
        self.peer.clone()
    }

    /// Starts the event loop, reading framed messages from the reader until
    /// EOF or an error occurs.
    ///
    /// Returns `Ok(())` on clean EOF, otherwise the underlying `ReadError`.
    ///
    /// # Note
    /// The reader is supplied via a closure so that it does not need to be
    /// `Send`.  Internally the loop starts a scoped thread for I/O; that
    /// thread calls the closure at start-up.
    ///
    /// Handler calls happen on the caller's thread in the order messages
    /// arrive.  At most one incoming request is outstanding at a time.
    pub fn mainloop<R, RF, H>(&mut self, rf: RF, handler: &mut H) -> Result<(), ReadError>
    where
        R: ReadTransport,
        RF: Send + FnOnce() -> R,
        H: Handler,
    {
        let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::thread::scope(|scope| {
            let peer = self.get_raw_peer();
            peer.reset_needs_exit();

            let ctx = RpcCtx { peer: Box::new(peer.clone()) };
            scope.spawn(move || {
                let mut stream = rf();
                loop {
                    // The main thread cannot return while this thread is active;
                    // when the main thread wants to exit it sets this flag.
                    if self.peer.needs_exit() {
                        trace!(target: "xi_rpc", "read loop exit");
                        break;
                    }

                let json = match self.reader.next(&mut stream) {
                        Ok(json) => json,
                        Err(ReadError::BatchNotSupported) => {
                            // Batch requests are rejected non-fatally: queue
                            // the error so the main thread can send a proper
                            // error response, then exit the read loop.
                            self.peer.put_rx(Err(ReadError::BatchNotSupported));
                            break;
                        }
                        Err(err) => {
                            if self.peer.0.is_blocked.load(Ordering::Acquire) {
                                error!("failed to parse response json: {}", err);
                                self.peer.disconnect();
                            }
                            self.peer.put_rx(Err(err));
                            break;
                        }
                    };
                    if json.is_response() {
                        let id = json.get_id().unwrap();
                        let span =
                            trace_span!(target: "xi_rpc", "read_loop_response", request_id = ?id);
                        let _entered = span.enter();
                        match json.into_response() {
                            Ok(resp) => {
                                let resp = resp.map_err(Error::from);
                                self.peer.handle_response(id, resp);
                            }
                            Err(msg) => {
                                error!("failed to parse response: {}", msg);
                                self.peer.handle_response(id, Err(Error::InvalidResponse));
                            }
                        }
                    } else {
                        self.peer.put_rx(Ok(json));
                    }
                }
            });

            loop {
                let _guard = PanicGuard(&peer);
                let read_result = next_read(&peer, handler, &ctx);
                let span = trace_span!(target: "xi_rpc", "mainloop_iteration");
                let _entered = span.enter();

                let json = match read_result {
                    Ok(json) => json,
                    Err(ReadError::BatchNotSupported) => {
                        // Send a well-formed JSON-RPC error with null id, then close.
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
                            return ReadError::Io(e);
                        }
                        peer.disconnect();
                        return ReadError::BatchNotSupported;
                    }
                    Err(err) => {
                        trace!(target: "xi_rpc", error = %err, "main loop error");
                        // finish idle work before disconnecting;
                        // this is mostly useful for integration tests.
                        if let Some(idle_token) = peer.try_get_idle() {
                            handler.idle(&ctx, idle_token);
                        }
                        peer.disconnect();
                        return err;
                    }
                };

                let method = json.get_method().map(String::from);
                match json.into_rpc::<H::Notification, H::Request>() {
                    Call::Request(id, cmd) => {
                        let method = method.unwrap();
                        let span =
                            trace_span!(target: "xi_rpc", "handle_request", method = %method);
                        let _entered = span.enter();
                        let result = handler.handle_request(&ctx, cmd);
                        if let Err(err) = peer.respond(result, id) {
                            peer.disconnect();
                            return ReadError::Io(err);
                        }
                    }
                    Call::Notification(cmd) => {
                        let method = method.unwrap();
                        let span = trace_span!(target: "xi_rpc", "handle_notification", method = %method);
                        let _entered = span.enter();
                        handler.handle_notification(&ctx, cmd);
                    }
                    Call::InvalidRequest(id, err) => {
                        if let Err(io_err) = peer.respond(Err(err), id) {
                            peer.disconnect();
                            return ReadError::Io(io_err);
                        }
                    }
                    Call::UnknownNotification(err) => {
                        // Unknown or malformed notification: log and continue.
                        // Do not disconnect — unknown notifications are non-fatal.
                        warn!("ignoring unknown notification: {}", err);
                    }
                }
            }
        })
        })).unwrap_or_else(|_| {
            error!("reader thread panicked; run loop is terminating");
            ReadError::ThreadPanic
        });

        if exit.is_disconnect() {
            Ok(())
        } else {
            Err(exit)
        }
    }
}

/// Returns the next read result, checking for idle work when no
/// result is available.
fn next_read<W, H>(peer: &RawPeer<W>, handler: &mut H, ctx: &RpcCtx) -> Result<RpcObject, ReadError>
where
    W: WriteTransport,
    H: Handler,
{
    loop {
        if let Some(result) = peer.try_get_rx() {
            return result;
        }
        // handle timers before general idle work
        let time_to_next_timer = match peer.check_timers() {
            Some(Ok(token)) => {
                do_idle(handler, ctx, token);
                continue;
            }
            Some(Err(duration)) => Some(duration),
            None => None,
        };

        if let Some(idle_token) = peer.try_get_idle() {
            do_idle(handler, ctx, idle_token);
            continue;
        }

        // we don't want to block indefinitely if there's no current idle work,
        // because idle work could be scheduled from another thread.
        let idle_timeout = time_to_next_timer.unwrap_or(MAX_IDLE_WAIT).min(MAX_IDLE_WAIT);

        if let Some(result) = peer.get_rx_timeout(idle_timeout) {
            return result;
        }
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
        self.0.is_blocked.store(true, Ordering::Release);
        let (tx, rx) = mpsc::channel();
        self.send_rpc_request_common(method, params, ResponseHandler::Chan(tx));
        let result = rx.recv().unwrap_or(Err(Error::PeerDisconnect));
        self.0.is_blocked.store(false, Ordering::Release);
        result
    }

    fn send_rpc_request_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let span = trace_span!(target: "xi_rpc", "send_request_timeout", method = %method);
        let _entered = span.enter();
        self.0.is_blocked.store(true, Ordering::Release);
        let (tx, rx) = mpsc::channel();
        self.send_rpc_request_common(method, params, ResponseHandler::Chan(tx));
        let result = match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::RequestTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::PeerDisconnect),
        };
        self.0.is_blocked.store(false, Ordering::Release);
        result
    }

    fn cancel_rpc_request(&self, id: RequestId) -> bool {
        let handler = {
            let mut pending =
                self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
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
        let queue = self.0.rx_queue.lock().unwrap();
        !queue.is_empty()
    }

    fn schedule_idle(&self, token: usize) {
        let mut queue = self.0.idle_queue.lock().unwrap_or_else(|e| e.into_inner());
        // Coalesce: only enqueue if the token is not already waiting.
        if !queue.contains(&token) {
            queue.push_back(token);
            // Wake the main loop so it picks up the new idle task without waiting
            // for the MAX_IDLE_WAIT polling timeout.
            self.0.rx_cvar.notify_one();
        }
    }

    fn schedule_timer(&self, after: Instant, token: usize) {
        self.0.timers.lock().unwrap_or_else(|e| e.into_inner()).push(Timer { fire_after: after, token });
        // Wake the main loop to re-evaluate the earliest timer deadline.
        self.0.rx_cvar.notify_one();
    }

    fn cancel_timer(&self, token: usize) -> bool {
        let mut timers = self.0.timers.lock().unwrap_or_else(|e| e.into_inner());
        let before = timers.len();
        // BinaryHeap has no removal by predicate; rebuild without matching tokens.
        let remaining: Vec<Timer> = timers.drain().filter(|t| t.token != token).collect();
        let removed = before - remaining.len();
        *timers = remaining.into_iter().collect();
        removed > 0
    }
}

impl<W: WriteTransport> RawPeer<W> {
    fn send(&self, v: &Value) -> Result<(), io::Error> {
        let span = trace_span!(target: "xi_rpc", "send_raw_message");
        let _entered = span.enter();
        // Serialize before acquiring the lock to keep the critical section small.
        let s = serde_json::to_string(v).unwrap();
        self.0.writer.lock().unwrap_or_else(|e| e.into_inner()).write_message(s.as_bytes())
        // Framing and flushing are handled by WriteTransport::write_message.
    }

    /// Like `send` but accessible outside of the `RawPeer` impl block,
    /// used for sending error responses that are constructed directly.
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

    /// Get a message from the receive queue if available.
    fn try_get_rx(&self) -> Option<Result<RpcObject, ReadError>> {
        let mut queue = self.0.rx_queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.pop_front()
    }

    /// Get a message from the receive queue, waiting for at most `Duration`
    /// and returning `None` if no message is available.
    fn get_rx_timeout(&self, dur: Duration) -> Option<Result<RpcObject, ReadError>> {
        let queue = self.0.rx_queue.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.0.rx_cvar.wait_timeout(queue, dur).unwrap_or_else(|e| e.into_inner());
        let mut queue = result.0;
        queue.pop_front()
    }

    /// Adds a message to the receive queue. The message should only
    /// be `None` if the read thread is exiting.
    fn put_rx(&self, json: Result<RpcObject, ReadError>) {
        let mut queue = self.0.rx_queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(json);
        self.0.rx_cvar.notify_one();
    }

    fn try_get_idle(&self) -> Option<usize> {
        self.0.idle_queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    /// Checks status of the most imminent timer. If that timer has expired,
    /// returns `Some(Ok(_))`, with the corresponding token.
    /// If a timer exists but has not expired, returns `Some(Err(_))`,
    /// with the error value being the `Duration` until the timer is ready.
    /// Returns `None` if no timers are registered.
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

    /// send disconnect error to pending requests.
    fn disconnect(&self) {
        let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
        let ids = pending.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let callback = pending.remove(&id).unwrap();
            callback.invoke(Err(Error::PeerDisconnect));
        }
        // Release ordering ensures all writes above are visible to threads
        // that subsequently read needs_exit with Acquire ordering.
        self.0.needs_exit.store(true, Ordering::Release);
    }

    /// Returns `true` if a shutdown has been signalled.
    fn needs_exit(&self) -> bool {
        // Acquire pairs with the Release store in disconnect / reset_needs_exit.
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

//NOTE: for our timers to work with Rust's BinaryHeap we want to reverse
//the default comparison; smaller `Instant`'s are considered 'greater'.
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
        // missing "" around params
        let reader = MessageReader::default();
        let json =
            reader.parse(r#"{"id": 5, "method": "hi", params: {"words": "plz"}}"#).err().unwrap();

        match json {
            ReadError::Json(..) => (),
            _ => panic!("parse failed"),
        }
        // a JSON array is rejected as a batch request
        let json = reader.parse(r#"[5, "hi", {"arg": "val"}]"#).err().unwrap();

        match json {
            ReadError::BatchNotSupported => (),
            _ => panic!("parse failed"),
        }
    }
}
