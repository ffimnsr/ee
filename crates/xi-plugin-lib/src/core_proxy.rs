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

//! A proxy for the methods on Core
use std::collections::HashSet;

use log::warn;
use serde::de::DeserializeOwned;
use serde_json::json;
use xi_core_lib::ViewId;
use xi_core_lib::annotations::AnnotationType;
use xi_core_lib::plugin_rpc::{
    CodeAction, CodeActionDescriptor, CodeActionRequest, CodeActionResponse, CompletionSuggestion,
    DataSpan, Diagnostic, FormatDocumentRequest, FormatDocumentResponse, GetDataResponse,
    GetDiagnosticsResponse, GetSelectionsResponse, Hover, NavigationTarget, PluginEdit,
    PluginEditAck, ProtocolCapability, ScopeSpan, SelectionRange, SymbolItem, TextEdit, TextUnit,
};
use xi_core_lib::plugins::PluginId;
use xi_rpc::{RemoteError, RpcCtx, RpcPeer};

use crate::Error;

#[derive(Clone)]
pub struct CoreProxy {
    plugin_id: PluginId,
    peer: RpcPeer,
    protocol_version: u32,
    capabilities: HashSet<ProtocolCapability>,
}

impl CoreProxy {
    pub fn new(
        plugin_id: PluginId,
        rpc_ctx: &RpcCtx,
        protocol_version: u32,
        capabilities: impl IntoIterator<Item = ProtocolCapability>,
    ) -> Self {
        CoreProxy {
            plugin_id,
            peer: rpc_ctx.get_peer().clone(),
            protocol_version,
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    pub fn supports_protocol_capability(&self, capability: ProtocolCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    fn send_rpc_notification(&self, method: &str, params: serde_json::Value) {
        self.peer.send_rpc_notification(method, &params);
    }

    fn send_rpc_request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, Error> {
        let response = self.peer.send_rpc_request(method, &params).map_err(Error::RpcError)?;
        serde_json::from_value(response).map_err(|_| Error::WrongReturnType)
    }

    pub fn add_scopes(&self, view_id: ViewId, scopes: &[Vec<String>]) {
        self.send_rpc_notification(
            "add_scopes",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "scopes": scopes,
            }),
        );
    }

    pub fn apply_edit(&self, view_id: ViewId, edit: PluginEdit) -> Result<PluginEditAck, Error> {
        self.send_rpc_request(
            "apply_edit",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "edit": edit,
            }),
        )
    }

    pub fn edit(&self, view_id: ViewId, edit: PluginEdit) {
        if let Err(err) = self.apply_edit(view_id, edit) {
            warn!("plugin edit request failed for {:?}: {:?}", view_id, err);
        }
    }

    pub fn get_data(
        &self,
        view_id: ViewId,
        start: usize,
        unit: TextUnit,
        max_size: usize,
        rev: u64,
    ) -> Result<GetDataResponse, Error> {
        self.send_rpc_request(
            "get_data",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "start": start,
                "unit": unit,
                "max_size": max_size,
                "rev": rev,
            }),
        )
    }

    pub fn line_count(&self, view_id: ViewId) -> Result<usize, Error> {
        self.send_rpc_request(
            "line_count",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
            }),
        )
    }

    pub fn get_selections(&self, view_id: ViewId) -> Result<Vec<SelectionRange>, Error> {
        let response: GetSelectionsResponse = self.send_rpc_request(
            "get_selections",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
            }),
        )?;
        Ok(response.selections)
    }

    pub fn get_diagnostics(&self, view_id: ViewId) -> Result<Vec<Diagnostic>, Error> {
        let response: GetDiagnosticsResponse = self.send_rpc_request(
            "get_diagnostics",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
            }),
        )?;
        Ok(response.diagnostics)
    }

    pub fn format_document(
        &self,
        view_id: ViewId,
        request: FormatDocumentRequest,
    ) -> Result<Vec<TextEdit>, Error> {
        let response: FormatDocumentResponse = self.send_rpc_request(
            "format_document",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "options": request.options,
            }),
        )?;
        Ok(response.edits)
    }

    pub fn get_code_actions(
        &self,
        view_id: ViewId,
        request: CodeActionRequest,
    ) -> Result<Vec<CodeAction>, Error> {
        let response: CodeActionResponse = self.send_rpc_request(
            "get_code_actions",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "range": request.range,
                "diagnostics": request.diagnostics,
            }),
        )?;
        Ok(response.actions)
    }

    pub fn update_spans(
        &self,
        view_id: ViewId,
        start: usize,
        len: usize,
        rev: u64,
        spans: &[ScopeSpan],
    ) {
        self.send_rpc_notification(
            "update_spans",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "start": start,
                "len": len,
                "rev": rev,
                "spans": spans,
            }),
        );
    }

    pub fn update_annotations(
        &self,
        view_id: ViewId,
        start: usize,
        len: usize,
        rev: u64,
        spans: &[DataSpan],
        annotation_type: &AnnotationType,
    ) {
        self.send_rpc_notification(
            "update_annotations",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "start": start,
                "len": len,
                "rev": rev,
                "spans": spans,
                "annotation_type": annotation_type,
            }),
        );
    }

    pub fn update_diagnostics(&self, view_id: ViewId, diagnostics: &[Diagnostic]) {
        self.send_rpc_notification(
            "update_diagnostics",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "diagnostics": diagnostics,
            }),
        );
    }

    pub fn request_is_pending(&self) -> bool {
        self.peer.request_is_pending()
    }

    pub fn add_status_item(&self, view_id: ViewId, key: &str, value: &str, alignment: &str) {
        self.send_rpc_notification(
            "add_status_item",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "key": key,
                "value": value,
                "alignment": alignment
            }),
        )
    }

    pub fn update_status_item(&self, view_id: ViewId, key: &str, value: &str) {
        self.send_rpc_notification(
            "update_status_item",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "key": key,
                "value": value
            }),
        )
    }

    pub fn remove_status_item(&self, view_id: ViewId, key: &str) {
        self.send_rpc_notification(
            "remove_status_item",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "key": key
            }),
        )
    }

    pub fn alert(&self, msg: impl AsRef<str>) {
        self.send_rpc_notification(
            "alert",
            json!({
                "plugin_id": self.plugin_id,
                "msg": msg.as_ref(),
            }),
        );
    }

    pub fn display_hover(
        &self,
        view_id: ViewId,
        request_id: usize,
        result: &Result<Hover, RemoteError>,
    ) {
        self.send_rpc_notification(
            "show_hover",
            json!({
                "plugin_id": self.plugin_id,
                "request_id": request_id,
                "result": result,
                "view_id": view_id
            }),
        );
    }

    pub fn show_completions(&self, view_id: ViewId, items: &[CompletionSuggestion]) {
        self.send_rpc_notification(
            "show_completions",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "items": items,
            }),
        );
    }

    pub fn show_code_actions(&self, view_id: ViewId, actions: &[CodeActionDescriptor]) {
        self.send_rpc_notification(
            "show_code_actions",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "actions": actions,
            }),
        )
    }

    pub fn show_locations(&self, view_id: ViewId, title: &str, locations: &[NavigationTarget]) {
        self.send_rpc_notification(
            "show_locations",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "title": title,
                "locations": locations,
            }),
        );
    }

    pub fn show_symbols(&self, view_id: ViewId, title: &str, symbols: &[SymbolItem]) {
        self.send_rpc_notification(
            "show_symbols",
            json!({
                "plugin_id": self.plugin_id,
                "view_id": view_id,
                "title": title,
                "symbols": symbols,
            }),
        );
    }

    pub fn schedule_idle(&self, view_id: ViewId) {
        let token: usize = view_id.into();
        self.peer.schedule_idle(token);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use xi_rpc::{Callback, Peer, RequestId};

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingPeer {
        pending: bool,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
        responses: Arc<Mutex<HashMap<String, Value>>>,
    }

    impl RecordingPeer {
        fn with_response(method: &str, response: Value) -> Self {
            let peer = Self::default();
            peer.responses.lock().unwrap().insert(method.to_owned(), response);
            peer
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
            self.pending
        }

        fn schedule_idle(&self, _token: usize) {}

        fn schedule_timer(&self, _after: Instant, _token: usize) {}

        fn cancel_timer(&self, _token: usize) -> bool {
            false
        }

        fn request_shutdown(&self) {}
    }

    #[test]
    fn get_selections_returns_typed_ranges() {
        let peer = RecordingPeer::with_response(
            "get_selections",
            json!({
                "selections": [
                    { "start": 1, "end": 4 }
                ]
            }),
        );
        let proxy = CoreProxy {
            plugin_id: serde_json::from_value(json!(5)).unwrap(),
            peer: Box::new(peer.clone()),
            protocol_version: 1,
            capabilities: HashSet::new(),
        };

        let selections = proxy.get_selections(9usize.into()).expect("request should deserialize");

        assert_eq!(selections, vec![SelectionRange { start: 1, end: 4 }]);
        assert_eq!(
            peer.requests.lock().unwrap().clone(),
            vec![(
                "get_selections".to_owned(),
                json!({
                    "plugin_id": 5,
                    "view_id": "view-id-9",
                })
            )]
        );
    }

    #[test]
    fn request_is_pending_proxies_to_peer() {
        let proxy = CoreProxy {
            plugin_id: serde_json::from_value(json!(5)).unwrap(),
            peer: Box::new(RecordingPeer { pending: true, ..RecordingPeer::default() }),
            protocol_version: 1,
            capabilities: HashSet::new(),
        };

        assert!(proxy.request_is_pending());
    }
}
