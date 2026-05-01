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

use std::io;
use std::path::Path;

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

#[derive(Debug)]
pub enum Error {
    RpcError(xi_rpc::Error),
    WrongReturnType,
    BadRequest,
    PeerDisconnect,
    ConfigDeserialization { context: &'static str, source: serde_json::Error },
    // Just used in tests
    Other(String),
}

/// Run `plugin` until it exits, blocking the current thread.
pub fn mainloop<P: Plugin>(plugin: &mut P) -> Result<(), ReadError> {
    xi_core_lib::tracing_support::install();
    let stdout = io::stdout();
    let mut rpc_looper = RpcLoop::new(NewlineWriter::new(stdout));
    let mut dispatcher = Dispatcher::new(plugin);

    rpc_looper
        .mainloop(|| NewlineReader::new(std::io::BufReader::new(io::stdin())), &mut dispatcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

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
}
