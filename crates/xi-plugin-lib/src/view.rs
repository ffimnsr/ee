// Copyright 2018 The xi-editor Authors.
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

use log::warn;
use serde::Deserialize;
use serde_json::{self, Value, json};
use std::path::{Path, PathBuf};

use xi_core_lib::annotations::AnnotationType;
use xi_core_lib::plugin_rpc::DataSpan;
use xi_core_lib::plugin_rpc::{
    CodeAction, CodeActionRequest, CodeActionResponse, Diagnostic, FormatDocumentRequest,
    FormatDocumentResponse, GetDataResponse, GetDiagnosticsResponse, GetSelectionsResponse,
    PluginBufferInfo, PluginEdit, PluginEditAck, ScopeSpan, SelectionRange, TextEdit, TextUnit,
};
use xi_core_lib::{BufferConfig, ConfigTable, LanguageId, PluginPid, ViewId};
use xi_rope::RopeDelta;
use xi_rope::interval::IntervalBounds;

use xi_rpc::RpcPeer;

use super::{Cache, DataSource, Error};

/// A type that acts as a proxy for a remote view. Provides access to
/// a document cache, and implements various methods for querying and modifying
/// view state.
pub struct View<C> {
    pub(crate) cache: C,
    pub(crate) peer: RpcPeer,
    pub(crate) path: Option<PathBuf>,
    pub(crate) config: BufferConfig,
    pub(crate) config_table: ConfigTable,
    plugin_id: PluginPid,
    // TODO: this is only public to avoid changing the syntect impl
    // this should go away with async edits
    pub rev: u64,
    pub undo_group: Option<usize>,
    buf_size: usize,
    active_view_id: ViewId,
    view_ids: Vec<ViewId>,
    pub(crate) language_id: LanguageId,
}

impl<C: Cache> View<C> {
    pub(crate) fn new(
        peer: RpcPeer,
        plugin_id: PluginPid,
        info: PluginBufferInfo,
    ) -> Result<Self, Error> {
        let PluginBufferInfo { views, rev, path, config, buf_size, nb_lines, syntax, .. } = info;

        let Some(active_view_id) = views.first().copied() else {
            return Err(Error::BadRequest);
        };
        let path = path.map(PathBuf::from);
        let parsed_config =
            serde_json::from_value(Value::Object(config.clone())).map_err(|source| {
                Error::ConfigDeserialization { context: "initial buffer config", source }
            })?;
        Ok(View {
            cache: C::new(buf_size, rev, nb_lines),
            peer,
            config_table: config.clone(),
            config: parsed_config,
            path,
            plugin_id,
            active_view_id,
            view_ids: views,
            rev,
            undo_group: None,
            buf_size,
            language_id: syntax,
        })
    }

    pub(crate) fn update(
        &mut self,
        delta: Option<&RopeDelta>,
        new_len: usize,
        new_num_lines: usize,
        rev: u64,
        undo_group: Option<usize>,
    ) {
        self.cache.update(delta, new_len, new_num_lines, rev);
        self.rev = rev;
        self.undo_group = undo_group;
        self.buf_size = new_len;
    }

    pub(crate) fn set_language(&mut self, new_language_id: LanguageId) {
        self.language_id = new_language_id;
    }

    //NOTE: (discuss in review) this feels bad, but because we're mutating cache,
    // which we own, we can't just pass in a reference to something else we own;
    // so we create this on each call. The `clone`is only cloning an `Arc`,
    // but we could maybe use a RefCell or something and make this cleaner.
    /// Returns a `FetchCtx`, a thin wrapper around an RpcPeer that implements
    /// the `DataSource` trait and can be used when updating a cache.
    pub(crate) fn make_ctx(&self) -> FetchCtx {
        FetchCtx {
            view_id: self.active_view_id,
            plugin_id: self.plugin_id,
            peer: self.peer.clone(),
        }
    }

    /// Returns the length of the view's buffer, in bytes.
    pub fn get_buf_size(&self) -> usize {
        self.buf_size
    }

    pub fn get_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn get_language_id(&self) -> &LanguageId {
        &self.language_id
    }

    pub fn get_config(&self) -> &BufferConfig {
        &self.config
    }

    pub fn get_cache(&mut self) -> &mut C {
        &mut self.cache
    }

    pub fn get_id(&self) -> ViewId {
        self.active_view_id
    }

    pub fn get_view_ids(&self) -> &[ViewId] {
        &self.view_ids
    }

    pub(crate) fn primary_view_id(&self) -> ViewId {
        self.view_ids[0]
    }

    pub(crate) fn has_view(&self, view_id: ViewId) -> bool {
        self.view_ids.contains(&view_id)
    }

    pub(crate) fn set_active_view(&mut self, view_id: ViewId) -> Result<(), Error> {
        if self.has_view(view_id) {
            self.active_view_id = view_id;
            Ok(())
        } else {
            Err(Error::BadRequest)
        }
    }

    pub(crate) fn remove_view_id(&mut self, view_id: ViewId) -> bool {
        let Some(index) = self.view_ids.iter().position(|candidate| *candidate == view_id) else {
            return false;
        };

        self.view_ids.remove(index);
        if self.active_view_id == view_id && !self.view_ids.is_empty() {
            self.active_view_id = self.view_ids[0];
        }
        true
    }

    pub fn get_line(&mut self, line_num: usize) -> Result<&str, Error> {
        let ctx = self.make_ctx();
        self.cache.get_line(&ctx, line_num)
    }

    /// Returns a region of the view's buffer.
    pub fn get_region<I: IntervalBounds>(&mut self, interval: I) -> Result<&str, Error> {
        let ctx = self.make_ctx();
        self.cache.get_region(&ctx, interval)
    }

    pub fn get_document(&mut self) -> Result<String, Error> {
        let ctx = self.make_ctx();
        self.cache.get_document(&ctx)
    }

    pub fn offset_of_line(&mut self, line_num: usize) -> Result<usize, Error> {
        let ctx = self.make_ctx();
        self.cache.offset_of_line(&ctx, line_num)
    }

    pub fn line_of_offset(&mut self, offset: usize) -> Result<usize, Error> {
        let ctx = self.make_ctx();
        self.cache.line_of_offset(&ctx, offset)
    }

    pub fn add_scopes(&self, scopes: &[Vec<String>]) {
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "scopes": scopes,
        });
        self.peer.send_rpc_notification("add_scopes", &params);
    }

    pub fn edit(
        &self,
        delta: RopeDelta,
        priority: u64,
        after_cursor: bool,
        new_undo_group: bool,
        author: String,
    ) {
        if let Err(err) = self.try_edit(delta, priority, after_cursor, new_undo_group, author) {
            warn!("plugin edit request failed for {:?}: {:?}", self.active_view_id, err);
        }
    }

    pub fn try_edit(
        &self,
        delta: RopeDelta,
        priority: u64,
        after_cursor: bool,
        new_undo_group: bool,
        author: String,
    ) -> Result<PluginEditAck, Error> {
        let undo_group = if new_undo_group { None } else { self.undo_group };
        let edit = PluginEdit { rev: self.rev, delta, priority, after_cursor, undo_group, author };
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "edit": edit
        });
        let response =
            self.peer.send_rpc_request("apply_edit", &params).map_err(Error::RpcError)?;
        serde_json::from_value(response).map_err(|_| Error::WrongReturnType)
    }

    pub fn line_count(&self) -> Result<usize, Error> {
        let response = self
            .peer
            .send_rpc_request(
                "line_count",
                &json!({
                    "plugin_id": self.plugin_id,
                    "view_id": self.active_view_id,
                }),
            )
            .map_err(Error::RpcError)?;
        serde_json::from_value(response).map_err(|_| Error::WrongReturnType)
    }

    pub fn get_selections(&self) -> Result<Vec<SelectionRange>, Error> {
        let response = self
            .peer
            .send_rpc_request(
                "get_selections",
                &json!({
                    "plugin_id": self.plugin_id,
                    "view_id": self.active_view_id,
                }),
            )
            .map_err(Error::RpcError)?;
        let response: GetSelectionsResponse =
            serde_json::from_value(response).map_err(|_| Error::WrongReturnType)?;
        Ok(response.selections)
    }

    pub fn get_diagnostics(&self) -> Result<Vec<Diagnostic>, Error> {
        let response = self
            .peer
            .send_rpc_request(
                "get_diagnostics",
                &json!({
                    "plugin_id": self.plugin_id,
                    "view_id": self.active_view_id,
                }),
            )
            .map_err(Error::RpcError)?;
        let response: GetDiagnosticsResponse =
            serde_json::from_value(response).map_err(|_| Error::WrongReturnType)?;
        Ok(response.diagnostics)
    }

    pub fn format_document(&self, request: FormatDocumentRequest) -> Result<Vec<TextEdit>, Error> {
        let response = self
            .peer
            .send_rpc_request(
                "format_document",
                &json!({
                    "plugin_id": self.plugin_id,
                    "view_id": self.active_view_id,
                    "options": request.options,
                }),
            )
            .map_err(Error::RpcError)?;
        let response: FormatDocumentResponse =
            serde_json::from_value(response).map_err(|_| Error::WrongReturnType)?;
        Ok(response.edits)
    }

    pub fn get_code_actions(&self, request: CodeActionRequest) -> Result<Vec<CodeAction>, Error> {
        let response = self
            .peer
            .send_rpc_request(
                "get_code_actions",
                &json!({
                    "plugin_id": self.plugin_id,
                    "view_id": self.active_view_id,
                    "range": request.range,
                    "diagnostics": request.diagnostics,
                }),
            )
            .map_err(Error::RpcError)?;
        let response: CodeActionResponse =
            serde_json::from_value(response).map_err(|_| Error::WrongReturnType)?;
        Ok(response.actions)
    }

    pub fn update_spans(&self, start: usize, len: usize, spans: &[ScopeSpan]) {
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "start": start,
            "len": len,
            "rev": self.rev,
            "spans": spans,
        });
        self.peer.send_rpc_notification("update_spans", &params);
    }

    pub fn update_annotations(
        &self,
        start: usize,
        len: usize,
        annotation_spans: &[DataSpan],
        annotation_type: &AnnotationType,
    ) {
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "start": start,
            "len": len,
            "rev": self.rev,
            "spans": annotation_spans,
            "annotation_type": annotation_type,
        });
        self.peer.send_rpc_notification("update_annotations", &params);
    }

    pub fn schedule_idle(&self) {
        let token: usize = self.active_view_id.into();
        self.peer.schedule_idle(token);
    }

    /// Returns `true` if an incoming RPC is pending. This is intended
    /// to reduce latency for bulk operations done in the background.
    pub fn request_is_pending(&self) -> bool {
        self.peer.request_is_pending()
    }

    pub fn add_status_item(&self, key: &str, value: &str, alignment: &str) {
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "key": key,
            "value": value,
            "alignment": alignment
        });
        self.peer.send_rpc_notification("add_status_item", &params);
    }

    pub fn update_status_item(&self, key: &str, value: &str) {
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "key": key,
            "value": value
        });
        self.peer.send_rpc_notification("update_status_item", &params);
    }

    pub fn remove_status_item(&self, key: &str) {
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.active_view_id,
            "key": key
        });
        self.peer.send_rpc_notification("remove_status_item", &params);
    }
}

/// A simple wrapper type that acts as a `DataSource`.
pub struct FetchCtx {
    plugin_id: PluginPid,
    view_id: ViewId,
    peer: RpcPeer,
}

impl DataSource for FetchCtx {
    fn get_data(
        &self,
        start: usize,
        unit: TextUnit,
        max_size: usize,
        rev: u64,
    ) -> Result<GetDataResponse, Error> {
        let _t = tracing::trace_span!("FetchCtx::get_data", categories = "plugin").entered();
        let params = json!({
            "plugin_id": self.plugin_id,
            "view_id": self.view_id,
            "start": start,
            "unit": unit,
            "max_size": max_size,
            "rev": rev,
        });
        let result = self.peer.send_rpc_request("get_data", &params).map_err(Error::RpcError)?;
        GetDataResponse::deserialize(result).map_err(|_| Error::WrongReturnType)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use xi_rope::DeltaBuilder;
    use xi_rpc::{Callback, Peer, RequestId};

    use super::*;
    use crate::ChunkCache;

    #[derive(Clone, Default)]
    struct RecordingPeer {
        requests: Arc<Mutex<Vec<(String, Value)>>>,
        responses: Arc<Mutex<HashMap<String, Value>>>,
    }

    impl RecordingPeer {
        fn with_response(method: &str, response: Value) -> Self {
            let peer = Self::default();
            peer.responses.lock().unwrap().insert(method.to_owned(), response);
            peer
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Peer for RecordingPeer {
        fn box_clone(&self) -> Box<dyn Peer> {
            Box::new(self.clone())
        }

        fn send_rpc_notification(&self, method: &str, params: &Value) {
            self.requests.lock().unwrap().push((method.to_owned(), params.clone()));
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

        fn send_rpc_request(&self, method: &str, params: &Value) -> Result<Value, xi_rpc::Error> {
            self.requests.lock().unwrap().push((method.to_owned(), params.clone()));
            self.responses
                .lock()
                .unwrap()
                .get(method)
                .cloned()
                .ok_or(xi_rpc::Error::PeerExited { exit_status: None })
        }

        fn send_rpc_request_timeout(
            &self,
            method: &str,
            params: &Value,
            _timeout: Duration,
        ) -> Result<Value, xi_rpc::Error> {
            self.send_rpc_request(method, params)
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

    fn valid_config() -> serde_json::Map<String, Value> {
        json!({
            "line_ending": "\n",
            "tab_size": 4,
            "translate_tabs_to_spaces": false,
            "use_tab_stops": true,
            "font_face": "monospace",
            "font_size": 14.0,
            "auto_indent": true,
            "scroll_past_end": false,
            "wrap_width": 0,
            "word_wrap": false,
            "autodetect_whitespace": true,
            "surrounding_pairs": [["(", ")"]],
            "save_with_newline": true
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn buffer_info(config: serde_json::Map<String, Value>) -> PluginBufferInfo {
        serde_json::from_value(json!({
            "buffer_id": 1,
            "views": ["view-id-1"],
            "rev": 7,
            "buf_size": 12,
            "nb_lines": 1,
            "path": null,
            "syntax": "plain_text",
            "config": config,
        }))
        .unwrap()
    }

    fn buffer_info_with_views(
        config: serde_json::Map<String, Value>,
        views: &[&str],
    ) -> PluginBufferInfo {
        serde_json::from_value(json!({
            "buffer_id": 1,
            "views": views,
            "rev": 7,
            "buf_size": 12,
            "nb_lines": 1,
            "path": null,
            "syntax": "plain_text",
            "config": config,
        }))
        .unwrap()
    }

    #[test]
    fn view_new_returns_structured_config_error() {
        let mut config = valid_config();
        config.insert("tab_size".to_owned(), json!(0));

        let err = match View::<ChunkCache>::new(
            Box::new(RecordingPeer::default()),
            serde_json::from_value(json!(9)).unwrap(),
            buffer_info(config),
        ) {
            Ok(_) => panic!("invalid config should fail"),
            Err(err) => err,
        };

        match err {
            Error::ConfigDeserialization { context, .. } => {
                assert_eq!(context, "initial buffer config");
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn format_document_uses_typed_request_shape() {
        let peer = RecordingPeer::with_response(
            "format_document",
            json!({
                "edits": [
                    {
                        "range": { "start": 1, "end": 3 },
                        "new_text": "xx"
                    }
                ]
            }),
        );
        let view = View::<ChunkCache>::new(
            Box::new(peer.clone()),
            serde_json::from_value(json!(3)).unwrap(),
            buffer_info(valid_config()),
        )
        .expect("valid config should build view");

        let edits = view
            .format_document(FormatDocumentRequest {
                options: Some(xi_core_lib::plugin_rpc::FormattingOptions {
                    tab_size: 2,
                    insert_spaces: true,
                }),
            })
            .expect("format request should deserialize");

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "xx");
        assert_eq!(
            peer.requests(),
            vec![(
                "format_document".to_owned(),
                json!({
                    "plugin_id": 3,
                    "view_id": "view-id-1",
                    "options": {
                        "tab_size": 2,
                        "insert_spaces": true,
                    }
                })
            )]
        );
    }

    #[test]
    fn try_edit_returns_rejection_reason() {
        let peer = RecordingPeer::with_response(
            "apply_edit",
            json!({
                "applied": false,
                "rev": 7,
                "reason": "revision conflict"
            }),
        );
        let view = View::<ChunkCache>::new(
            Box::new(peer),
            serde_json::from_value(json!(3)).unwrap(),
            buffer_info(valid_config()),
        )
        .expect("valid config should build view");

        let ack = view
            .try_edit(DeltaBuilder::new(0).build(), 1, false, true, "plugin".to_string())
            .expect("edit ack should deserialize");

        assert!(!ack.applied);
        assert_eq!(ack.reason.as_deref(), Some("revision conflict"));
    }

    #[test]
    fn multi_view_buffer_switches_active_rpc_target() {
        let peer = RecordingPeer::with_response("line_count", json!(42));
        let mut view = View::<ChunkCache>::new(
            Box::new(peer.clone()),
            serde_json::from_value(json!(3)).unwrap(),
            buffer_info_with_views(valid_config(), &["view-id-1", "view-id-2"]),
        )
        .expect("valid config should build view");

        assert_eq!(view.get_view_ids().len(), 2);
        view.set_active_view(2usize.into()).expect("secondary view should exist");
        let line_count = view.line_count().expect("line count should deserialize");

        assert_eq!(line_count, 42);
        assert_eq!(
            peer.requests(),
            vec![(
                "line_count".to_owned(),
                json!({
                    "plugin_id": 3,
                    "view_id": "view-id-2",
                })
            )]
        );
    }
}
