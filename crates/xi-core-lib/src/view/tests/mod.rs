//! View tests: harness.
use super::*;
use crate::rpc::FindQuery;
use serde_json::Value;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use xi_rpc::{Callback, Error as RpcError, Peer, RequestId};

#[derive(Clone, Default)]
struct RecordingPeer {
    notifications: Arc<Mutex<Vec<(String, Value)>>>,
    binary_frames: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Whether this peer's transport advertises byte frames.
    binary_capable: bool,
}

impl RecordingPeer {
    fn take_notifications(&self) -> Vec<(String, Value)> {
        let mut notifications = self.notifications.lock().expect("recording peer poisoned");
        mem::take(&mut *notifications)
    }

    fn take_binary_frames(&self) -> Vec<Vec<u8>> {
        let mut frames = self.binary_frames.lock().expect("recording peer poisoned");
        mem::take(&mut *frames)
    }
}

impl Peer for RecordingPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(self.clone())
    }

    fn send_rpc_notification(&self, method: &str, params: &Value) {
        self.notifications
            .lock()
            .expect("recording peer poisoned")
            .push((method.to_owned(), params.clone()));
    }

    fn send_rpc_request_async(
        &self,
        _method: &str,
        _params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        f.call(Ok(Value::Null));
        RequestId::Number(0)
    }

    fn send_rpc_request(&self, _method: &str, _params: &Value) -> Result<Value, RpcError> {
        Ok(Value::Null)
    }

    fn send_rpc_request_timeout(
        &self,
        _method: &str,
        _params: &Value,
        _timeout: std::time::Duration,
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

    fn schedule_timer(&self, _time: Instant, _token: usize) {}

    fn cancel_timer(&self, _token: usize) -> bool {
        false
    }

    fn request_shutdown(&self) {}

    fn supports_binary_frames(&self) -> bool {
        self.binary_capable
    }

    fn send_binary_frame(&self, data: &[u8]) -> std::io::Result<()> {
        if !self.binary_capable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "recording peer has no byte carrier",
            ));
        }
        self.binary_frames.lock().expect("recording peer poisoned").push(data.to_vec());
        Ok(())
    }
}

fn recording_client() -> (Client, RecordingPeer) {
    let peer = RecordingPeer::default();
    let client = Client::new(Box::new(peer.clone()));
    (client, peer)
}

/// A client whose peer advertises byte frames, so the renderer may use the blob
/// text carrier.
fn recording_client_with_binary() -> (Client, RecordingPeer) {
    let peer = RecordingPeer { binary_capable: true, ..RecordingPeer::default() };
    let client = Client::new(Box::new(peer.clone()));
    (client, peer)
}

/// Rows that carry syntax spans across a set of collected notifications.
fn count_span_rows(notifications: &[(String, Value)]) -> usize {
    notifications
        .iter()
        .filter(|(method, _)| method == "update")
        .flat_map(|(_, params)| params["update"]["ops"].as_array().cloned().unwrap_or_default())
        .filter_map(|op| op["lines"].as_array().cloned())
        .flatten()
        .filter(|line| line.get("spans").is_some())
        .count()
}

/// The `spans` array of every row that carries one, in notification order.
fn span_values(notifications: &[(String, Value)]) -> Vec<Value> {
    notifications
        .iter()
        .filter(|(method, _)| method == "update")
        .flat_map(|(_, params)| params["update"]["ops"].as_array().cloned().unwrap_or_default())
        .filter_map(|op| op["lines"].as_array().cloned())
        .flatten()
        .filter_map(|line| line.get("spans").cloned())
        .collect()
}

/// Pre-compiles the standard queries for `languages` so the timed render
/// and span paths never pay cold query-compilation cost inside their
/// wall-clock budgets.
fn warm_syntax_queries(languages: &[&str]) {
    use crate::runtime_loader::{RuntimeQueryKind, with_default_runtime_loader_mut};
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();
    with_default_runtime_loader_mut(|loader| {
        for language in languages {
            for kind in [RuntimeQueryKind::Highlights, RuntimeQueryKind::Injections] {
                let _ = loader.compile_query_kind(language, kind);
            }
        }
    });
}

mod render_find_tests;
mod render_source_tests;
mod selection_tests;
mod syntax_cache_tests;
mod text_blob_tests;
mod viewport_clamp_tests;
