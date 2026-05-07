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

//! Implementation for Language Server Client

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{self, Child, ExitStatus};
use std::sync::{Arc, Mutex};

use log::{error, trace, warn};

use jsonrpc_lite::{Error, Id, JsonRpc, Params};
use lsp_server::Message as LspServerMessage;
use lsp_types::Uri;
use serde_json::{Value, json, to_value};
use xi_plugin_lib::CoreProxy;

use crate::conversion_utils::core_diagnostic_from_lsp_document;
use crate::result_queue::ResultQueue;
use crate::types::{Callback, Error as LspError, LspResponse};
use lsp_types::*;
use xi_core_lib::ViewId;

const STATUS_ALIGNMENT: &str = "left";
const LONG_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct OpenDocumentState {
    pub uri: Uri,
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// A type to abstract communication with the language server
pub struct LanguageServerClient {
    writer: Box<dyn Write + Send>,
    process: Arc<Mutex<Child>>,
    pending: HashMap<u64, Callback>,
    next_id: u64,
    language_id: String,
    pub result_queue: ResultQueue,
    pub status_items: HashSet<String>,
    pub core: CoreProxy,
    pub initialization_pending: bool,
    pub is_initialized: bool,
    pub opened_documents: HashMap<ViewId, OpenDocumentState>,
    pub server_capabilities: Option<ServerCapabilities>,
    pub file_extensions: Vec<String>,
}

/// Get numeric id from the request id.
fn number_from_id(id: Option<&Id>) -> Result<u64, LspError> {
    match id {
        Some(Id::Num(n)) if *n >= 0 => Ok(*n as u64),
        Some(Id::Num(n)) => Err(LspError::Protocol(format!("negative request id {n}"))),
        Some(Id::Str(s)) => s
            .parse()
            .map_err(|_| LspError::Protocol(format!("failed to convert string id {s:?} to u64"))),
        Some(other) => Err(LspError::Protocol(format!("unexpected id value: {other:?}"))),
        None => Err(LspError::Protocol(String::from("missing id field"))),
    }
}

fn number_from_lsp_request_id(id: &lsp_server::RequestId) -> Result<u64, LspError> {
    match serde_json::to_value(id).map_err(|err| LspError::Serialization(err.to_string()))? {
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| LspError::Protocol(format!("unexpected id value: {number}"))),
        Value::String(value) => value.parse().map_err(|_| {
            LspError::Protocol(format!("failed to convert string id {value:?} to u64"))
        }),
        other => Err(LspError::Protocol(format!("unexpected id value: {other:?}"))),
    }
}

impl LanguageServerClient {
    pub fn new(
        writer: Box<dyn Write + Send>,
        process: Arc<Mutex<Child>>,
        core: CoreProxy,
        result_queue: ResultQueue,
        language_id: String,
        file_extensions: Vec<String>,
    ) -> Self {
        LanguageServerClient {
            writer,
            process,
            pending: HashMap::new(),
            next_id: 1,
            initialization_pending: false,
            is_initialized: false,
            core,
            result_queue,
            status_items: HashSet::new(),
            language_id,
            server_capabilities: None,
            opened_documents: HashMap::new(),
            file_extensions,
        }
    }

    fn status_item_key(&self) -> String {
        format!("lsp:{}:status", self.language_id)
    }

    pub(crate) fn fail_pending_requests(&mut self, message: &str) {
        let callbacks = self.pending.drain().map(|(_, callback)| callback).collect::<Vec<_>>();
        let error = Error { code: -32098, message: message.to_string(), data: None };
        for callback in callbacks {
            callback.call(self, Err(error.clone()));
        }
    }

    pub fn record_server_failure(&mut self, message: impl Into<String>) {
        let message = message.into();
        error!("language server {}: {}", self.language_id, message);
        let status_key = self.status_item_key();
        if self.status_items.insert(status_key.clone()) {
            for view_id in self.opened_documents.keys() {
                self.core.add_status_item(*view_id, &status_key, &message, STATUS_ALIGNMENT);
            }
        } else {
            for view_id in self.opened_documents.keys() {
                self.core.update_status_item(*view_id, &status_key, &message);
            }
        }
    }

    pub fn clear_server_failure(&mut self) {
        let status_key = self.status_item_key();
        if self.status_items.remove(&status_key) {
            for view_id in self.opened_documents.keys() {
                self.core.remove_status_item(*view_id, &status_key);
            }
        }
    }

    pub fn exit_status(&self) -> Result<Option<ExitStatus>, LspError> {
        let mut process =
            self.process.lock().map_err(|_| LspError::LockPoisoned("language server process"))?;
        process.try_wait().map_err(Into::into)
    }

    pub fn process_handle(&self) -> Arc<Mutex<Child>> {
        Arc::clone(&self.process)
    }

    pub fn open_document_states(&self) -> Vec<(ViewId, OpenDocumentState)> {
        self.opened_documents.iter().map(|(view_id, state)| (*view_id, state.clone())).collect()
    }

    pub fn handle_message(&mut self, message: &str) {
        match JsonRpc::parse(message) {
            Ok(JsonRpc::Request(obj)) => trace!("client received unexpected request: {:?}", obj),
            Ok(value @ JsonRpc::Notification(_)) => {
                match (value.get_method(), value.get_params()) {
                    (Some(method), Some(params)) => self.handle_notification(method, params),
                    _ => self.record_server_failure("malformed notification from language server"),
                }
            }
            Ok(value @ JsonRpc::Success(_)) => {
                match (number_from_id(value.get_id().as_ref()), value.get_result()) {
                    (Ok(id), Some(result)) => self.handle_response(id, Ok(result.clone())),
                    (Err(err), _) => self.record_server_failure(err.to_string()),
                    (_, None) => {
                        self.record_server_failure("success response missing result field")
                    }
                }
            }
            Ok(value @ JsonRpc::Error(_)) => {
                match (number_from_id(value.get_id().as_ref()), value.get_error()) {
                    (Ok(id), Some(error)) => self.handle_response(id, Err(error.clone())),
                    (Err(err), _) => self.record_server_failure(err.to_string()),
                    (_, None) => self.record_server_failure("error response missing error field"),
                }
            }
            Err(err) => self.record_server_failure(format!("error parsing incoming string: {err}")),
        }
    }

    pub(crate) fn handle_lsp_message(&mut self, message: LspServerMessage) {
        match message {
            LspServerMessage::Request(request) => {
                trace!("client received unexpected request: {:?}", request)
            }
            LspServerMessage::Notification(notification) => {
                self.handle_notification(&notification.method, Params::from(notification.params));
            }
            LspServerMessage::Response(response) => {
                let Ok(id) = number_from_lsp_request_id(&response.id) else {
                    self.record_server_failure(format!(
                        "unexpected response id: {:?}",
                        response.id
                    ));
                    return;
                };

                match (response.result, response.error) {
                    (Some(result), None) => self.handle_response(id, Ok(result)),
                    (None, Some(error)) => self.handle_response(
                        id,
                        Err(Error {
                            code: i64::from(error.code),
                            message: error.message,
                            data: error.data,
                        }),
                    ),
                    (None, None) => {
                        self.record_server_failure("response missing result and error fields")
                    }
                    (Some(_), Some(_)) => {
                        self.record_server_failure("response contained both result and error")
                    }
                }
            }
        }
    }

    pub fn handle_response(&mut self, id: u64, result: Result<Value, Error>) {
        let Some(callback) = self.pending.remove(&id) else {
            warn!("ignoring response for non-pending request id {}", id);
            return;
        };
        callback.call(self, result);
    }

    pub fn handle_notification(&mut self, method: &str, params: Params) {
        trace!("Notification Received =>\n Method: {}, params: {:?}", method, params);
        match method {
            "window/showMessage" => {}
            "window/logMessage" => {}
            "textDocument/publishDiagnostics" => self.handle_publish_diagnostics(params),
            "telemetry/event" => {}
            _ => self.handle_misc_notification(method, params),
        }
    }

    fn handle_publish_diagnostics(&mut self, params: Params) {
        let parsed = match params {
            Params::Map(map) => {
                serde_json::from_value::<PublishDiagnosticsParams>(Value::Object(map))
            }
            Params::Array(values) => {
                serde_json::from_value::<PublishDiagnosticsParams>(Value::Array(values))
            }
            Params::None(()) => Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "publishDiagnostics missing params",
            ))),
        };

        let params = match parsed {
            Ok(params) => params,
            Err(err) => {
                self.record_server_failure(format!("invalid publishDiagnostics payload: {err}"));
                return;
            }
        };

        let Some((view_id, state)) = self
            .opened_documents
            .iter()
            .find(|(_, state)| state.uri == params.uri)
            .map(|(view_id, state)| (*view_id, state.clone()))
        else {
            warn!("dropping diagnostics for unopened document {}", params.uri.as_str());
            return;
        };

        let diagnostics = params
            .diagnostics
            .clone()
            .into_iter()
            .map(|diagnostic| core_diagnostic_from_lsp_document(&state.text, diagnostic))
            .collect::<Result<Vec<_>, _>>();

        if let Some(state) = self.opened_documents.get_mut(&view_id) {
            state.diagnostics = params.diagnostics;
        }

        self.result_queue.push_result(view_id.into(), LspResponse::Diagnostics(diagnostics));
        self.core.schedule_idle(view_id);
    }

    pub fn handle_misc_notification(&mut self, method: &str, params: Params) {
        match self.language_id.to_lowercase().as_ref() {
            "rust" => self.handle_rust_misc_notification(method, params),
            _ => warn!("Unknown notification: {}", method),
        }
    }

    fn remove_status_item(&mut self, id: &str) {
        self.status_items.remove(id);
        for view_id in self.opened_documents.keys() {
            self.core.remove_status_item(*view_id, id);
        }
    }

    fn add_status_item(&mut self, id: &str, value: &str, alignment: &str) {
        self.status_items.insert(id.to_string());
        for view_id in self.opened_documents.keys() {
            self.core.add_status_item(*view_id, id, value, alignment);
        }
    }

    fn update_status_item(&mut self, id: &str, value: &str) {
        for view_id in self.opened_documents.keys() {
            self.core.update_status_item(*view_id, id, value);
        }
    }

    pub fn send_request(&mut self, method: &str, params: Params, completion: Callback) -> u64 {
        match self.try_send_request(method, params, completion) {
            Ok(request_id) => request_id,
            Err(err) => {
                self.record_server_failure(err.to_string());
                0
            }
        }
    }

    pub fn try_send_request(
        &mut self,
        method: &str,
        params: Params,
        completion: Callback,
    ) -> Result<u64, LspError> {
        let request_id = self.next_id;
        let request = JsonRpc::request_with_params(Id::Num(request_id as i64), method, params);
        let value = to_value(&request).map_err(|err| LspError::Serialization(err.to_string()))?;

        self.pending.insert(request_id, completion);
        self.next_id += 1;

        if let Err(err) = self.send_rpc(&value) {
            self.pending.remove(&request_id);
            return Err(err);
        }
        Ok(request_id)
    }

    pub fn cancel_request(&mut self, id: u64) -> bool {
        if self.pending.remove(&id).is_none() {
            return false;
        }

        let Ok(cancel_id) = i32::try_from(id) else {
            warn!("cannot cancel request {} because it exceeds i32 range", id);
            return false;
        };

        let params =
            match serde_json::to_value(CancelParams { id: NumberOrString::Number(cancel_id) }) {
                Ok(value) => Params::from(value),
                Err(err) => {
                    self.record_server_failure(format!("failed to encode cancel request: {err}"));
                    return false;
                }
            };
        self.send_notification("$/cancelRequest", params).is_ok()
    }

    fn send_rpc(&mut self, value: &Value) -> Result<(), LspError> {
        let text =
            serde_json::to_string(value).map_err(|err| LspError::Serialization(err.to_string()))?;
        trace!("Sending RPC: {:?}", text);
        write!(self.writer, "Content-Length: {}\r\n\r\n{}", text.len(), text)
            .map_err(LspError::from)?;
        self.writer.flush().map_err(LspError::from)
    }

    pub fn send_notification(&mut self, method: &str, params: Params) -> Result<(), LspError> {
        let notification = JsonRpc::notification_with_params(method, params);
        let res =
            to_value(&notification).map_err(|err| LspError::Serialization(err.to_string()))?;
        self.send_rpc(&res)
    }
}

/// Methods to abstract sending notifications and requests to the language server
impl LanguageServerClient {
    /// Send the Initialize Request given the Root URI of the
    /// Workspace. It is None for non-workspace projects.
    pub fn send_initialize<CB>(
        &mut self,
        root_uri: Option<Uri>,
        on_init: CB,
    ) -> Result<(), LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        #[derive(serde::Serialize)]
        struct InitializeParamsCompat {
            process_id: Option<u32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            initialization_options: Option<Value>,
            capabilities: ClientCapabilities,
            #[serde(skip_serializing_if = "Option::is_none")]
            trace: Option<TraceValue>,
            #[serde(skip_serializing_if = "Option::is_none")]
            workspace_folders: Option<Vec<WorkspaceFolder>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            client_info: Option<ClientInfo>,
            #[serde(skip_serializing_if = "Option::is_none")]
            locale: Option<String>,
            #[serde(flatten)]
            work_done_progress_params: WorkDoneProgressParams,
        }

        let client_capabilities = ClientCapabilities::default();
        let workspace_folders = root_uri.clone().map(|uri| {
            let name = uri
                .as_str()
                .trim_end_matches('/')
                .rsplit('/')
                .find(|segment| !segment.is_empty())
                .unwrap_or("workspace")
                .to_string();
            vec![WorkspaceFolder { uri, name }]
        });

        let init_params = InitializeParamsCompat {
            process_id: Some(process::id()),
            initialization_options: None,
            capabilities: client_capabilities,
            trace: Some(TraceValue::Verbose),
            workspace_folders,
            client_info: None,
            locale: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let params = Params::from(serde_json::to_value(init_params).map_err(LspError::from)?);
        self.initialization_pending = true;
        self.try_send_request("initialize", params, Box::new(on_init)).map(|_| ())
    }

    /// Send textDocument/didOpen Notification to the Language Server
    pub fn send_did_open(
        &mut self,
        view_id: ViewId,
        document_uri: Uri,
        document_text: String,
    ) -> Result<(), LspError> {
        self.opened_documents.insert(
            view_id,
            OpenDocumentState {
                uri: document_uri.clone(),
                text: document_text.clone(),
                diagnostics: Vec::new(),
            },
        );

        if !self.is_initialized {
            return Ok(());
        }

        let text_document_did_open_params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                language_id: self.language_id.clone(),
                uri: document_uri,
                version: 0,
                text: document_text,
            },
        };

        let params = Params::from(
            serde_json::to_value(text_document_did_open_params).map_err(LspError::from)?,
        );
        self.send_notification("textDocument/didOpen", params)
    }

    pub fn resend_open_documents(&mut self) -> Result<(), LspError> {
        let documents = self
            .opened_documents
            .iter()
            .map(|(view_id, state)| (*view_id, state.uri.clone(), state.text.clone()))
            .collect::<Vec<_>>();

        for (view_id, uri, text) in documents {
            self.send_did_open(view_id, uri, text)?;
        }
        Ok(())
    }

    /// Send textDocument/didClose Notification to the Language Server
    pub fn send_did_close(&mut self, view_id: ViewId) -> Result<(), LspError> {
        let Some(state) = self.opened_documents.remove(&view_id) else {
            return Ok(());
        };
        if !self.is_initialized {
            return Ok(());
        }
        let text_document_did_close_params =
            DidCloseTextDocumentParams { text_document: TextDocumentIdentifier { uri: state.uri } };

        let params = Params::from(
            serde_json::to_value(text_document_did_close_params).map_err(LspError::from)?,
        );
        self.send_notification("textDocument/didClose", params)
    }

    /// Send textDocument/didChange Notification to the Language Server
    pub fn send_did_change(
        &mut self,
        view_id: ViewId,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        document_text: String,
    ) -> Result<(), LspError> {
        let Some(state) = self.opened_documents.get_mut(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        state.text = document_text;
        if !self.is_initialized {
            return Ok(());
        }

        let text_document_did_change_params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: state.uri.clone(), version },
            content_changes: changes,
        };

        let params = Params::from(
            serde_json::to_value(text_document_did_change_params).map_err(LspError::from)?,
        );
        self.send_notification("textDocument/didChange", params)
    }

    /// Send textDocument/didSave notification to the Language Server
    pub fn send_did_save(&mut self, view_id: ViewId, _document_text: &str) -> Result<(), LspError> {
        // Add support for sending document text as well. Currently missing in LSP types
        // and is optional in LSP Specification
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Ok(());
        }
        let text_document_did_save_params = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: state.uri.clone() },
            text: None,
        };
        let params = Params::from(
            serde_json::to_value(text_document_did_save_params).map_err(LspError::from)?,
        );
        self.send_notification("textDocument/didSave", params)
    }

    pub fn request_hover<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }
        let text_document_position_params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: state.uri.clone() },
            position,
        };

        let params = Params::from(
            serde_json::to_value(text_document_position_params).map_err(LspError::from)?,
        );
        self.try_send_request("textDocument/hover", params, Box::new(on_result))
    }

    pub fn request_completion<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/completion",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
                "context": null,
            })),
            Box::new(on_result),
        )
    }

    pub fn request_definition<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/definition",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
            })),
            Box::new(on_result),
        )
    }

    pub fn request_declaration<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/declaration",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
            })),
            Box::new(on_result),
        )
    }

    pub fn request_type_definition<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/typeDefinition",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
            })),
            Box::new(on_result),
        )
    }

    pub fn request_references<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/references",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
                "context": { "includeDeclaration": true },
            })),
            Box::new(on_result),
        )
    }

    pub fn request_implementation<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/implementation",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
            })),
            Box::new(on_result),
        )
    }

    pub fn request_document_formatting<CB>(
        &mut self,
        view_id: ViewId,
        options: Option<xi_core_lib::plugin_rpc::FormattingOptions>,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        let options = options.unwrap_or(xi_core_lib::plugin_rpc::FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
        });

        self.try_send_request(
            "textDocument/formatting",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "options": {
                    "tabSize": options.tab_size,
                    "insertSpaces": options.insert_spaces,
                },
            })),
            Box::new(on_result),
        )
    }

    pub fn request_code_actions<CB>(
        &mut self,
        view_id: ViewId,
        range: Range,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/codeAction",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "range": range,
                "context": {
                    "diagnostics": state.diagnostics,
                },
            })),
            Box::new(on_result),
        )
    }

    pub fn request_rename<CB>(
        &mut self,
        view_id: ViewId,
        position: Position,
        new_name: String,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }

        self.try_send_request(
            "textDocument/rename",
            Params::from(json!({
                "textDocument": { "uri": state.uri.clone() },
                "position": position,
                "newName": new_name,
            })),
            Box::new(on_result),
        )
    }

    pub fn long_request_timeout(&self) -> std::time::Duration {
        LONG_REQUEST_TIMEOUT
    }
}

/// Helper methods to query the capabilities of the Language Server before making
/// a request. For example: we can check if the Language Server supports sending
/// incremental edits before proceeding to send one.
impl LanguageServerClient {
    /// Method to get the sync kind Supported by the Server
    pub fn get_sync_kind(&mut self) -> TextDocumentSyncKind {
        match self.server_capabilities.as_ref().and_then(|c| c.text_document_sync.as_ref()) {
            Some(&TextDocumentSyncCapability::Kind(kind)) => kind,
            _ => TextDocumentSyncKind::FULL,
        }
    }

    /// Request document symbols (`textDocument/documentSymbol`) for the given view.
    pub fn request_document_symbols<CB>(
        &mut self,
        view_id: ViewId,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        let Some(state) = self.opened_documents.get(&view_id) else {
            return Err(LspError::Protocol(format!("missing open document for view {view_id}")));
        };
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }
        self.try_send_request(
            "textDocument/documentSymbol",
            Params::from(json!({ "textDocument": { "uri": state.uri.clone() } })),
            Box::new(on_result),
        )
    }

    /// Request workspace symbols (`workspace/symbol`) for the given query string.
    pub fn request_workspace_symbols<CB>(
        &mut self,
        view_id: ViewId,
        query: &str,
        on_result: CB,
    ) -> Result<u64, LspError>
    where
        CB: 'static + Send + FnOnce(&mut LanguageServerClient, Result<Value, Error>),
    {
        if !self.is_initialized {
            return Err(LspError::Protocol(format!(
                "language server {} not initialized",
                self.language_id
            )));
        }
        // Store view_id so the callback can route the result back.
        let _ = view_id;
        self.try_send_request(
            "workspace/symbol",
            Params::from(json!({ "query": query })),
            Box::new(on_result),
        )
    }
}

/// Language Specific Notification handling implementations
impl LanguageServerClient {
    pub fn handle_rust_misc_notification(&mut self, method: &str, params: Params) {
        match method {
            "window/progress" => {
                match params {
                    Params::Map(m) => {
                        let done = m.get("done").unwrap_or(&Value::Bool(false));
                        if let Value::Bool(done) = done {
                            let Some(id_value) = m.get("id") else {
                                warn!("window/progress notification missing id");
                                return;
                            };
                            let Ok(id) = serde_json::from_value::<String>(id_value.clone()) else {
                                warn!("window/progress notification had invalid id");
                                return;
                            };
                            if *done {
                                self.remove_status_item(&id);
                            } else {
                                let mut value = String::new();
                                if let Some(Value::String(s)) = &m.get("title") {
                                    value.push_str(&format!("{} ", s));
                                }

                                if let Some(Value::Number(n)) = &m.get("percentage") {
                                    if let Some(percentage) = n.as_f64() {
                                        value.push_str(&format!(
                                            "{} %",
                                            (percentage * 100.00).round()
                                        ));
                                    }
                                }

                                if let Some(Value::String(s)) = &m.get("message") {
                                    value.push_str(s);
                                }
                                // Add or update item
                                if self.status_items.contains(&id) {
                                    self.update_status_item(&id, &value);
                                } else {
                                    self.add_status_item(&id, &value, "left");
                                }
                            }
                        }
                    }
                    _ => warn!("Unexpected type"),
                }
            }
            _ => warn!("Unknown Notification from RLS: {} ", method),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use serde_json::json;
    use xi_plugin_lib::CoreProxy;
    use xi_rpc::test_utils::make_reader;
    use xi_rpc::{Handler, NewlineWriter, RpcCtx, RpcLoop};

    use super::*;

    #[derive(Default)]
    struct CaptureCoreProxy {
        proxy: Option<CoreProxy>,
    }

    impl Handler for CaptureCoreProxy {
        type Notification = serde_json::Value;
        type Request = serde_json::Value;

        fn handle_notification(&mut self, ctx: &RpcCtx, _rpc: Self::Notification) {
            let plugin_id = serde_json::from_value(json!(1)).expect("plugin id should deserialize");
            self.proxy = Some(CoreProxy::new(plugin_id, ctx, 1, []));
            ctx.get_peer().request_shutdown();
        }

        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            _rpc: Self::Request,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<Value, xi_rpc::RemoteError> {
            Ok(Value::Null)
        }
    }

    fn test_core_proxy() -> CoreProxy {
        let mut handler = CaptureCoreProxy::default();
        let mut looper = RpcLoop::new(NewlineWriter::new(io::sink()));
        let reader = make_reader(r#"{"method":"ping","params":{}}"#);
        looper.mainloop(|| reader, &mut handler).expect("test rpc loop should exit cleanly");
        handler.proxy.expect("core proxy should be captured")
    }

    #[test]
    fn malformed_success_response_sets_failure_status_without_panicking() {
        let process = Arc::new(Mutex::new(
            process::Command::new("sh")
                .arg("-c")
                .arg("cat >/dev/null")
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::null())
                .spawn()
                .expect("test child should spawn"),
        ));
        let mut client = LanguageServerClient::new(
            Box::new(io::sink()),
            process,
            test_core_proxy(),
            ResultQueue::new(),
            String::from("rust"),
            vec![String::from("rs")],
        );

        client.handle_message(&json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string());

        assert!(client.status_items.contains(&client.status_item_key()));
    }

    #[test]
    fn publish_diagnostics_queues_results_for_matching_view() {
        let process = Arc::new(Mutex::new(
            process::Command::new("sh")
                .arg("-c")
                .arg("cat >/dev/null")
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::null())
                .spawn()
                .expect("test child should spawn"),
        ));
        let mut queue = ResultQueue::new();
        let mut client = LanguageServerClient::new(
            Box::new(io::sink()),
            process,
            test_core_proxy(),
            queue.clone(),
            String::from("rust"),
            vec![String::from("rs")],
        );
        let uri: Uri = "file:///tmp/test.rs".parse().expect("uri should parse");
        client.opened_documents.insert(
            7.into(),
            OpenDocumentState {
                uri: uri.clone(),
                text: String::from("fn main() {}\n"),
                diagnostics: Vec::new(),
            },
        );

        client.handle_notification(
            "textDocument/publishDiagnostics",
            Params::from(json!({
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": { "line": 0, "character": 3 },
                        "end": { "line": 0, "character": 7 }
                    },
                    "severity": 1,
                    "message": "boom",
                    "source": "test"
                }]
            })),
        );

        let drained = queue.drain_results_for(7);
        assert_eq!(drained.len(), 1);
        match drained.into_iter().next().expect("diagnostic response expected") {
            LspResponse::Diagnostics(Ok(diagnostics)) => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].message, "boom");
                assert_eq!(diagnostics[0].range.start, 3);
                assert_eq!(diagnostics[0].range.end, 7);
            }
            other => panic!("unexpected response: {:?}", other),
        }
    }
}
