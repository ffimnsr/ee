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

//! The library base for implementing xi-editor plugins.
mod base_cache;
mod core_proxy;
mod dispatch;
mod state_cache;
mod view;
pub mod wasm;

use std::backtrace::Backtrace;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::{any::Any, panic};

use serde_json::Value;
use xi_core_lib::plugin_rpc::{GetDataResponse, TextUnit};
use xi_core_lib::{ConfigTable, LanguageId};
use xi_rope::RopeDelta;
use xi_rope::interval::IntervalBounds;
use xi_rpc::{NewlineReader, NewlineWriter, ReadError, RemoteError, RpcLoop};

use self::dispatch::Dispatcher;

pub use crate::base_cache::ChunkCache;
pub use crate::core_proxy::CoreProxy;
pub use crate::state_cache::StateCache;
pub use crate::view::View;
pub use xi_core_lib::plugin_rpc::{
    CodeAction, CodeActionRequest, Diagnostic, DiagnosticSeverity, FormatDocumentRequest,
    FormattingOptions, Hover, PluginEdit, PluginEditAck, Range, SelectionRange, TextEdit,
};
pub use xi_plugin_derive::xi_plugin;

pub extern crate self as xi_plugin;
pub extern crate self as xi_plugin_lib;

/// Abstracts getting data from the peer. Mainly exists for mocking in tests.
pub trait DataSource {
    fn get_data(
        &self,
        start: usize,
        unit: TextUnit,
        max_size: usize,
        rev: u64,
    ) -> Result<GetDataResponse, Error>;
}

/// A generic interface for types that cache a remote document.
///
/// In general, users of this library should not need to implement this trait;
/// we provide two concrete Cache implementations, [`ChunkCache`] and
/// [`StateCache`]. If however a plugin's particular needs are not met by
/// those implementations, a user may choose to implement their own.
///
/// [`ChunkCache`]: ../base_cache/struct.ChunkCache.html
/// [`StateCache`]: ../state_cache/struct.StateCache.html
pub trait Cache {
    /// Create a new instance of this type; instances are created automatically
    /// as relevant views are added.
    fn new(buf_size: usize, rev: u64, num_lines: usize) -> Self;
    /// Returns the line at `line_num` (zero-indexed). Returns an `Err(_)` if
    /// there is a problem connecting to the peer, or if the requested line
    /// is out of bounds.
    ///
    /// The `source` argument is some type that implements [`DataSource`]; in
    /// the general case this is backed by the remote peer.
    ///
    /// [`DataSource`]: trait.DataSource.html
    fn get_line<DS: DataSource>(&mut self, source: &DS, line_num: usize) -> Result<&str, Error>;

    /// Returns the specified region of the buffer. Returns an `Err(_)` if
    /// there is a problem connecting to the peer, or if the requested line
    /// is out of bounds.
    ///
    /// The `source` argument is some type that implements [`DataSource`]; in
    /// the general case this is backed by the remote peer.
    ///
    /// [`DataSource`]: trait.DataSource.html
    fn get_region<DS, I>(&mut self, source: &DS, interval: I) -> Result<&str, Error>
    where
        DS: DataSource,
        I: IntervalBounds;

    /// Returns the entire contents of the remote document, fetching as needed.
    fn get_document<DS: DataSource>(&mut self, source: &DS) -> Result<String, Error>;

    /// Returns the offset of the line at `line_num`, zero-indexed, fetching
    /// data from `source` if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if `line_num` is greater than the total number of lines
    /// in the document, or if there is a problem communicating with `source`.
    fn offset_of_line<DS: DataSource>(
        &mut self,
        source: &DS,
        line_num: usize,
    ) -> Result<usize, Error>;
    /// Returns the index of the line containing `offset`, fetching
    /// data from `source` if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if `offset` is greater than the total length of
    /// the document, or if there is a problem communicating with `source`.
    fn line_of_offset<DS: DataSource>(
        &mut self,
        source: &DS,
        offset: usize,
    ) -> Result<usize, Error>;
    /// Updates the cache by applying this delta.
    fn update(&mut self, delta: Option<&RopeDelta>, buf_size: usize, num_lines: usize, rev: u64);
    /// Flushes any state held by this cache.
    fn clear(&mut self);
}

/// An interface for plugins.
///
/// Users of this library must implement this trait for some type.
pub trait Plugin {
    type Cache: Cache;

    /// Called when the Plugin is initialized. The plugin receives CoreProxy
    /// object that is a wrapper around the RPC Peer and can be used to call
    /// related methods on the Core in a type-safe manner.
    #[allow(unused_variables)]
    fn initialize(&mut self, core: CoreProxy) {}

    /// Called when an edit has occurred in the remote view. If the plugin wishes
    /// to add its own edit, it must do so using asynchronously via the edit notification.
    fn update(
        &mut self,
        view: &mut View<Self::Cache>,
        delta: Option<&RopeDelta>,
        edit_type: String,
        author: String,
    );
    /// Called when a buffer has been saved to disk. The buffer's previous
    /// path, if one existed, is passed as `old_path`.
    fn did_save(&mut self, view: &mut View<Self::Cache>, old_path: Option<&Path>);
    /// Called when a view has been closed. By the time this message is received,
    /// It is possible to send messages to this view. The plugin may wish to
    /// perform cleanup, however.
    fn did_close(&mut self, view: &View<Self::Cache>);
    /// Called when there is a new view that this buffer is interested in.
    /// This is called once per view, and is paired with a call to
    /// `Plugin::did_close` when the view is closed.
    fn new_view(&mut self, view: &mut View<Self::Cache>);

    /// Called when a config option has changed for this view. `changes`
    /// is a map of keys/values that have changed; previous values are available
    /// in the existing config, accessible through `view.get_config()`.
    fn config_changed(&mut self, view: &mut View<Self::Cache>, changes: &ConfigTable);

    /// Called when plugin-scoped config changes. This is delivered before any
    /// `new_view` callbacks during initialize if host has plugin config.
    #[allow(unused_variables)]
    fn plugin_config_changed(&mut self, changes: &ConfigTable) {}

    /// Called when syntax language has changed for this view.
    /// New language is available in the `view`, and old language is available in `old_lang`.
    #[allow(unused_variables)]
    fn language_changed(&mut self, view: &mut View<Self::Cache>, old_lang: LanguageId) {}

    /// Called with a custom command.
    #[allow(unused_variables)]
    fn custom_command(&mut self, view: &mut View<Self::Cache>, method: &str, params: Value) {}

    /// Called when the runloop is idle, if the plugin has previously
    /// asked to be scheduled via `View::schedule_idle()`. Plugins that
    /// are doing things like full document analysis can use this mechanism
    /// to perform their work incrementally while remaining responsive.
    #[allow(unused_variables)]
    fn idle(&mut self, view: &mut View<Self::Cache>) {}

    /// Called before the plugin RPC loop shuts down in response to a core
    /// shutdown notification.
    #[allow(unused_variables)]
    fn shutdown(&mut self) {}

    /// Language Plugins specific methods
    #[allow(unused_variables)]
    fn get_hover(
        &mut self,
        view: &mut View<Self::Cache>,
        position: usize,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Hover, RemoteError> {
        Err(RemoteError::custom(404, "hover not supported", None))
    }
}

pub trait SimplePlugin {
    fn initialize(&mut self, _core: CoreProxy) {}

    fn update(
        &mut self,
        _view: &mut View<ChunkCache>,
        _delta: Option<&RopeDelta>,
        _edit_type: String,
        _author: String,
    ) {
    }

    fn did_save(&mut self, _view: &mut View<ChunkCache>, _old_path: Option<&Path>) {}

    fn did_close(&mut self, _view: &View<ChunkCache>) {}

    fn new_view(&mut self, _view: &mut View<ChunkCache>) {}

    fn config_changed(&mut self, _view: &mut View<ChunkCache>, _changes: &ConfigTable) {}

    fn language_changed(&mut self, _view: &mut View<ChunkCache>, _old_lang: LanguageId) {}

    fn custom_command(&mut self, _view: &mut View<ChunkCache>, _method: &str, _params: Value) {}

    fn idle(&mut self, _view: &mut View<ChunkCache>) {}

    fn shutdown(&mut self) {}

    fn get_hover(
        &mut self,
        _view: &mut View<ChunkCache>,
        _position: usize,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Hover, RemoteError> {
        Err(RemoteError::custom(404, "hover not supported", None))
    }
}

#[macro_export]
macro_rules! log {
    ($plugin:expr, $level:expr, $message:expr $(,)?) => {
        $crate::log!($plugin, $level, $message, serde_json::Value::Null)
    };
    ($plugin:expr, $level:expr, $message:expr, $fields:expr $(,)?) => {{
        use std::io::Write as _;

        let plugin_name = ::std::borrow::Cow::<str>::from($plugin);
        let fields = ::serde_json::to_value($fields).unwrap_or_else(|err| {
            ::serde_json::json!({
                "serialization_error": err.to_string(),
            })
        });
        let record = ::serde_json::json!({
            "plugin": plugin_name,
            "level": $level,
            "message": $message,
            "fields": fields,
        });
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "[plugin:{}] {}", plugin_name, record);
    }};
}

#[derive(Debug)]
pub enum Error {
    RpcError(xi_rpc::Error),
    WrongReturnType,
    BadRequest,
    PeerUnavailable,
    ConfigDeserialization { context: &'static str, source: serde_json::Error },
    // Just used in tests
    Other(String),
}

trait PanicResponseSink: Send + Sync {
    fn write_panic_response(&self, response: &Value);
}

thread_local! {
    static PANIC_RESPONSE_SINK: std::cell::RefCell<Option<Arc<dyn PanicResponseSink>>> =
        const { std::cell::RefCell::new(None) };
}

static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

struct SharedWriter<W> {
    inner: Arc<Mutex<W>>,
}

impl<W> SharedWriter<W> {
    fn new(inner: Arc<Mutex<W>>) -> Self {
        Self { inner }
    }

    fn lock(&self) -> MutexGuard<'_, W> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl<W> Clone for SharedWriter<W> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<W: Write + Send + 'static> Write for SharedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock().flush()
    }
}

impl<W: Write + Send + 'static> PanicResponseSink for SharedWriter<W> {
    fn write_panic_response(&self, response: &Value) {
        let mut writer = self.lock();
        let _ = serde_json::to_writer(&mut *writer, response);
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    }
}

struct PanicHookGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>,
}

impl PanicHookGuard {
    fn install(sink: Arc<dyn PanicResponseSink>) -> Self {
        let lock = PANIC_HOOK_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        PANIC_RESPONSE_SINK.with(|slot| {
            *slot.borrow_mut() = Some(sink);
        });
        let previous = panic::take_hook();
        panic::set_hook(Box::new(|info| {
            emit_plugin_panicked_response(info);
        }));
        Self { _lock: lock, previous }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        PANIC_RESPONSE_SINK.with(|slot| {
            *slot.borrow_mut() = None;
        });
        let previous = std::mem::replace(&mut self.previous, Box::new(|_| {}));
        panic::set_hook(previous);
    }
}

fn emit_plugin_panicked_response(info: &panic::PanicHookInfo<'_>) {
    let Some(request_id) = xi_rpc::current_request_id() else {
        return;
    };
    let payload = panic_payload(info.payload());
    let backtrace = Backtrace::force_capture().to_string();
    let location = info
        .location()
        .map(|location| format!("{}:{}:{}", location.file(), location.line(), location.column()));
    let error = RemoteError::custom(
        -32099,
        "PluginPanicked",
        Some(serde_json::json!({
            "payload": payload,
            "backtrace": backtrace,
            "location": location,
        })),
    );
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": error,
    });
    PANIC_RESPONSE_SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink.write_panic_response(&response);
        }
    });
}

fn panic_payload(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        String::from("non-string panic payload")
    }
}

fn mainloop_with_newline_io<P, W, R, RF>(
    plugin: &mut P,
    writer: Arc<Mutex<W>>,
    reader_factory: RF,
) -> Result<(), ReadError>
where
    P: Plugin,
    W: Write + Send + 'static,
    R: xi_rpc::ReadTransport + Send + 'static,
    RF: FnOnce() -> R + Send + 'static,
{
    xi_core_lib::tracing_support::install();
    let shared_writer = SharedWriter::new(writer);
    let panic_sink: Arc<dyn PanicResponseSink> =
        Arc::new(SharedWriter::new(Arc::clone(&shared_writer.inner)));
    let _panic_hook = PanicHookGuard::install(panic_sink);
    let mut rpc_looper = RpcLoop::new(NewlineWriter::new(shared_writer));
    let mut dispatcher = Dispatcher::new(plugin);

    rpc_looper.mainloop(reader_factory, &mut dispatcher)
}

/// Run `plugin` until it exits, blocking the current thread.
pub fn mainloop<P: Plugin>(plugin: &mut P) -> Result<(), ReadError> {
    mainloop_with_newline_io(plugin, Arc::new(Mutex::new(io::stdout())), || {
        NewlineReader::new(std::io::BufReader::new(io::stdin()))
    })
}

#[macro_export]
macro_rules! xi_plugin_wasm {
    ($plugin_ty:ty, $init:expr) => {
        static XI_PLUGIN_RUNTIME: std::sync::OnceLock<$crate::wasm::WasmPluginRuntime<$plugin_ty>> =
            std::sync::OnceLock::new();

        fn xi_plugin_runtime() -> &'static $crate::wasm::WasmPluginRuntime<$plugin_ty> {
            XI_PLUGIN_RUNTIME.get_or_init(|| $crate::wasm::WasmPluginRuntime::new($init))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn alloc(len: usize) -> u32 {
            $crate::wasm::alloc(len)
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn dealloc(ptr: u32, len: usize) {
            unsafe { $crate::wasm::dealloc(ptr, len) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn handle_host_notification(ptr: u32, len: u32) {
            let runtime = xi_plugin_runtime();
            let bytes = unsafe { $crate::wasm::read_input(ptr, len) };
            if let Err(err) = runtime.handle_notification(bytes) {
                panic!("wasm plugin notification bridge failed: {err}");
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn handle_host_request(ptr: u32, len: u32) -> u64 {
            let runtime = xi_plugin_runtime();
            let bytes = unsafe { $crate::wasm::read_input(ptr, len) };
            let response = runtime.handle_request(bytes).unwrap_or_else(|err| {
                serde_json::to_vec(&serde_json::json!({
                    "status": "err",
                    "payload": {
                        "code": 500,
                        "message": err,
                        "data": null,
                    }
                }))
                .expect("wasm plugin error payload should serialize")
            });
            $crate::wasm::pack_output(response)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::io::Cursor;

    use std::panic::AssertUnwindSafe;
    use xi_rpc::RequestId;
    use xi_rpc::test_utils::make_reader;

    struct ShutdownPlugin {
        shutdown_called: bool,
    }

    impl Plugin for ShutdownPlugin {
        type Cache = ChunkCache;

        fn update(
            &mut self,
            _view: &mut View<Self::Cache>,
            _delta: Option<&RopeDelta>,
            _edit_type: String,
            _author: String,
        ) {
        }

        fn did_save(&mut self, _view: &mut View<Self::Cache>, _old_path: Option<&Path>) {}

        fn did_close(&mut self, _view: &View<Self::Cache>) {}

        fn new_view(&mut self, _view: &mut View<Self::Cache>) {}

        fn config_changed(&mut self, _view: &mut View<Self::Cache>, _changes: &ConfigTable) {}

        fn shutdown(&mut self) {
            self.shutdown_called = true;
        }
    }

    #[test]
    fn shutdown_notification_exits_plugin_loop_cleanly() {
        let mut plugin = ShutdownPlugin { shutdown_called: false };
        let mut dispatcher = Dispatcher::new(&mut plugin);
        let mut rpc_looper = RpcLoop::new(NewlineWriter::new(io::sink()));
        let reader = make_reader(r#"{"method":"shutdown","params":{}}"#);

        let result = rpc_looper.mainloop(|| reader, &mut dispatcher);

        assert!(result.is_ok(), "shutdown should end plugin loop cleanly: {:?}", result);
        assert!(plugin.shutdown_called, "plugin shutdown hook should run before exit");
    }

    #[test]
    fn panic_hook_replies_with_plugin_panicked_error() {
        let writer = Arc::new(Mutex::new(Cursor::new(Vec::<u8>::new())));
        let shared_writer = SharedWriter::new(Arc::clone(&writer));
        let panic_sink: Arc<dyn PanicResponseSink> =
            Arc::new(SharedWriter::new(Arc::clone(&shared_writer.inner)));
        let _panic_hook = PanicHookGuard::install(panic_sink);
        let _request_scope = xi_rpc::enter_request_scope(RequestId::Number(1));

        let panic_result = panic::catch_unwind(AssertUnwindSafe(|| {
            panic!("hover panic");
        }));

        assert!(panic_result.is_err(), "panic should still unwind after hook response");
        let raw = {
            let guard = writer.lock().unwrap();
            String::from_utf8(guard.clone().into_inner()).unwrap()
        };
        let response: serde_json::Value =
            serde_json::from_str(raw.lines().next().expect("panic response should be present"))
                .unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], -32099);
        assert_eq!(response["error"]["message"], "PluginPanicked");
        assert_eq!(response["error"]["data"]["payload"], "hover panic");
        assert!(
            response["error"]["data"]["backtrace"]
                .as_str()
                .is_some_and(|backtrace| !backtrace.is_empty())
        );
    }
}
