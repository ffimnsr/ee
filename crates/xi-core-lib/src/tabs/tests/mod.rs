//! Core-state tests: harness.
use std::fs;
use std::io::{ErrorKind, Write};
use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use xi_rpc::test_utils::DummyPeer;
use xi_rpc::{Callback, Error as RpcError, Handler, Peer, RequestId, RpcCtx};

use crate::open_policy::{OpenPolicy, OpenThresholds};
use crate::text_store::TextStore;

use super::{
    CoreState, NEW_VIEW_IDLE_TOKEN, PLUGIN_RESTART_MAX_DELAY_MS, SAVE_VIEW_IDLE_MASK,
    VERIFY_LINE_ENDINGS_IDLE_TOKEN, ViewId, stderr_is_user_visible,
};

#[derive(Clone, Default)]
struct RecordingPeer {
    notifications: Arc<Mutex<Vec<(String, Value)>>>,
}

impl RecordingPeer {
    fn take_notifications(&self) -> Vec<(String, Value)> {
        let mut notifications = self.notifications.lock().expect("recording peer poisoned");
        mem::take(&mut *notifications)
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

    fn schedule_timer(&self, _time: std::time::Instant, _token: usize) {}

    fn cancel_timer(&self, _token: usize) -> bool {
        false
    }

    fn request_shutdown(&self) {}
}

fn drive_save_idle(core: &mut crate::XiCore, view_id: ViewId) {
    core.inner().handle_idle(SAVE_VIEW_IDLE_MASK | usize::from(view_id));
}

fn vlf_text(core: &crate::XiCore, buffer_id: super::BufferId) -> String {
    let inner = core.inner();
    let editor = inner.editors.get(&buffer_id).unwrap().borrow();
    let store = editor.vlf_store.as_ref().expect("expected VLF store");
    match store.read_byte_range(crate::text_store::ByteRange::new(0, store.len_bytes())) {
        crate::text_store::TextChunkResult::Ready(chunk) => chunk.text,
        other => panic!("expected Ready VLF text, got {other:?}"),
    }
}

use super::*;

mod core_tests;
mod save_tests;
