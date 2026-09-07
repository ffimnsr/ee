//! Event-context tests: shared harness.
use super::*;
use crate::config::ConfigManager;
use crate::core::dummy_weak_core;
use crate::line_offset::LineOffset;
use crate::object::SyntaxNavigationTarget;
use crate::plugin_rpc::PluginRequest;
use crate::plugins::rpc::{
    CodeActionRequest, Diagnostic, DiagnosticSeverity, FormatDocumentRequest,
    GetDiagnosticsResponse, GetSelectionsResponse, SelectionRange,
};
use crate::rpc::SelectionModifier;
use crate::selection::SelRegion;
use crate::tabs::BufferId;
use crate::text_store::DocumentMode;
use crate::text_store::{LineLookup, LogicalLine, TextStore};
use crate::vlf::store::VlfStore;
use serde_json::{Value, json};
use std::io::Write;
use std::mem;
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;
use xi_rope::Interval;
use xi_rpc::{Callback, Error as RpcError, Peer, RemoteError, RequestId};

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

struct ContextHarness {
    view: RefCell<View>,
    editor: RefCell<Editor>,
    client: Client,
    peer: RecordingPeer,
    core_ref: WeakXiCore,
    kill_ring: RefCell<Rope>,
    width_cache: RefCell<WidthCache>,
    config_manager: ConfigManager,
}

impl ContextHarness {
    fn new<S: AsRef<str>>(s: S) -> Self {
        let view_id = ViewId(1);
        let buffer_id = BufferId(2);
        let mut config_manager = ConfigManager::new(None, None);
        let config = config_manager.add_buffer(buffer_id, None);
        let view = RefCell::new(View::new(view_id, buffer_id));
        let editor = RefCell::new(Editor::with_text(s));
        let peer = RecordingPeer::default();
        let client = Client::new(Box::new(peer.clone()));
        let core_ref = dummy_weak_core();
        let kill_ring = RefCell::new(Rope::from(""));
        let width_cache = RefCell::new(WidthCache::new());
        let harness = ContextHarness {
            view,
            editor,
            client,
            peer,
            core_ref,
            kill_ring,
            width_cache,
            config_manager,
        };
        harness.make_context().view_init();
        harness.make_context().finish_init(&config);
        harness
    }

    fn debug_render(&self) -> String {
        let b = self.editor.borrow();
        let mut text: String = b.get_buffer().into();
        let v = self.view.borrow();
        for sel in v.sel_regions().iter().rev() {
            if sel.end == sel.start {
                text.insert(sel.end, '|');
            } else if sel.end > sel.start {
                text.insert_str(sel.end, "|]");
                text.insert(sel.start, '[');
            } else {
                text.insert(sel.start, ']');
                text.insert_str(sel.end, "[|");
            }
        }
        text
    }

    fn take_notifications(&self) -> Vec<(String, Value)> {
        self.peer.take_notifications()
    }

    fn make_context(&self) -> EventContext<'_> {
        let view_id = ViewId(1);
        let buffer_id = self.view.borrow().get_buffer_id();
        let config = self.config_manager.get_buffer_config(buffer_id);
        let language = self.config_manager.get_buffer_language(buffer_id);
        EventContext {
            view_id,
            buffer_id,
            view: &self.view,
            editor: &self.editor,
            config: &config.items,
            language,
            info: None,
            siblings: Vec::new(),
            plugins: Vec::new(),
            client: &self.client,
            kill_ring: &self.kill_ring,
            width_cache: &self.width_cache,
            weak_core: &self.core_ref,
        }
    }
}

fn vlf_store_from(content: &[u8], page_size: u64) -> (crate::vlf::store::VlfStore, NamedTempFile) {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(content).unwrap();
    file.flush().unwrap();
    let store =
        crate::vlf::store::VlfStore::open_with_config(file.path(), page_size, 1024 * 1024).unwrap();
    (store, file)
}

// ── Tests ──

fn vlf_harness(content: &[u8]) -> (ContextHarness, NamedTempFile) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    (harness, f)
}

mod basic_tests;
mod delete_tests;
mod edit_tests;
mod filter_tests;
mod gesture_tests;
mod motion_tests;
mod vlf_tests;
