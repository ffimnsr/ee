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

//! Implementation of Language Server Plugin

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use jsonrpc_lite::Params;
use log::{debug, error, trace};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use xi_plugin_lib::{ChunkCache, CoreProxy, Plugin, View};
use xi_rope::rope::RopeDelta;

use crate::conversion_utils::*;
use crate::language_server_client::LanguageServerClient;
use crate::result_queue::ResultQueue;
use crate::types::{
    Config, Error, LanguageResponseError, LspCodeAction, LspResponse, PendingCompletionItem,
};
use crate::utils::*;
use lsp_types::*;
use xi_core_lib::{ConfigTable, ViewId};

#[derive(Clone)]
pub struct ViewInfo {
    version: i32,
    language_id: String,
    ls_identifier: String,
    workspace_root: Option<Uri>,
}

/// Represents the state of the Language Server Plugin
pub struct LspPlugin {
    pub config: Config,
    view_info: HashMap<ViewId, ViewInfo>,
    core: Option<CoreProxy>,
    result_queue: ResultQueue,
    pending_code_actions: HashMap<ViewId, Vec<LspCodeAction>>,
    pending_completions: HashMap<ViewId, Vec<PendingCompletionItem>>,
    language_server_clients: HashMap<String, Arc<Mutex<LanguageServerClient>>>,
}

impl LspPlugin {
    pub fn new(config: Config) -> Self {
        LspPlugin {
            config,
            core: None,
            result_queue: ResultQueue::new(),
            view_info: HashMap::new(),
            pending_code_actions: HashMap::new(),
            pending_completions: HashMap::new(),
            language_server_clients: HashMap::new(),
        }
    }
}

impl Plugin for LspPlugin {
    type Cache = ChunkCache;

    fn initialize(&mut self, core: CoreProxy) {
        self.core = Some(core)
    }

    fn update(
        &mut self,
        view: &mut View<Self::Cache>,
        delta: Option<&RopeDelta>,
        _edit_type: String,
        _author: String,
    ) {
        if self.view_info.contains_key(&view.get_id()) {
            let document_text = match view.get_document() {
                Ok(text) => text,
                Err(err) => {
                    error!("failed to fetch document for view {} update: {:?}", view.get_id(), err);
                    return;
                }
            };

            let Ok(ls_client_arc) = self.client_for_view(view) else {
                return;
            };
            let Ok(mut ls_client) = ls_client_arc.lock() else {
                error!("language server client lock poisoned for view {}", view.get_id());
                return;
            };

            let sync_kind = ls_client.get_sync_kind();
            let next_version = {
                let Some(view_info) = self.view_info.get_mut(&view.get_id()) else {
                    return;
                };
                view_info.version += 1;
                view_info.version
            };
            if let Some(changes) = get_change_for_sync_kind(sync_kind, view, delta)
                && let Err(err) =
                    ls_client.send_did_change(view.get_id(), changes, next_version, document_text)
            {
                ls_client.record_server_failure(format!("failed to send didChange: {err}"));
            }
        }
    }

    fn did_save(&mut self, view: &mut View<Self::Cache>, _old: Option<&Path>) {
        trace!("saved view {}", view.get_id());

        let document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                error!("failed to fetch document for view {} save: {:?}", view.get_id(), err);
                return;
            }
        };

        if let Ok(ls_client_arc) = self.client_for_view(view)
            && let Ok(mut ls_client) = ls_client_arc.lock()
            && let Err(err) = ls_client.send_did_save(view.get_id(), &document_text)
        {
            ls_client.record_server_failure(format!("failed to send didSave: {err}"));
        }
    }

    fn did_close(&mut self, view: &View<Self::Cache>) {
        trace!("close view {}", view.get_id());

        if let Some(view_info) = self.view_info.remove(&view.get_id())
            && let Some(ls_client_arc) =
                self.language_server_clients.get(&view_info.ls_identifier).cloned()
            && let Ok(mut ls_client) = ls_client_arc.lock()
            && let Err(err) = ls_client.send_did_close(view.get_id())
        {
            ls_client.record_server_failure(format!("failed to send didClose: {err}"));
        }
    }

    fn new_view(&mut self, view: &mut View<Self::Cache>) {
        trace!("new view {}", view.get_id());

        // TODO: Use Language Idenitifier assigned by core when the
        // implementation is settled
        if let Some(language_id) = self.get_language_for_view(view) {
            let workspace_root_uri = self.workspace_root_for_view(view, &language_id);
            if let Some((identifier, ls_client)) =
                self.get_lsclient_from_workspace_root(&language_id, &workspace_root_uri)
            {
                self.view_info.insert(
                    view.get_id(),
                    ViewInfo {
                        version: 0,
                        language_id,
                        ls_identifier: identifier,
                        workspace_root: workspace_root_uri.clone(),
                    },
                );

                if let Err(err) = self.open_view_on_client(view, workspace_root_uri, &ls_client) {
                    error!(
                        "failed to initialize language server view {}: {:?}",
                        view.get_id(),
                        err
                    );
                }
            }
        }
    }

    fn config_changed(&mut self, _view: &mut View<Self::Cache>, _changes: &ConfigTable) {}

    fn custom_command(&mut self, view: &mut View<Self::Cache>, method: &str, params: Value) {
        match method {
            "request_completion" | "lsp.completion" => {
                let index = params
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok());
                self.request_completion(view, index);
            }
            "request_definition" | "lsp.definition" => self.request_definition(view),
            "request_references" | "lsp.references" => self.request_references(view),
            "request_document_symbols" | "lsp.document_symbols" => {
                self.request_document_symbols(view);
            }
            "request_workspace_symbols" | "lsp.workspace_symbols" => {
                let query = params.get("query").and_then(Value::as_str).unwrap_or("").to_owned();
                self.request_workspace_symbols(view, query);
            }
            "format_document" | "lsp.format_document" => self.request_document_formatting(view),
            "request_code_actions" | "lsp.code_action" => {
                let index = params
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok());
                self.request_or_apply_code_action(view, index);
            }
            "request_rename" => {
                let Some(new_name) = params.get("new_name").and_then(Value::as_str) else {
                    self.record_view_failure(view, String::from("rename failed: missing new_name"));
                    return;
                };
                self.request_rename(view, new_name.to_owned());
            }
            _ => {}
        }
    }

    fn shutdown(&mut self) {
        let clients = self.language_server_clients.values().cloned().collect::<Vec<_>>();
        for client in clients {
            if let Err(err) = shutdown_language_server(&client) {
                error!("failed to shutdown language server: {}", err);
            }
        }
    }

    fn idle(&mut self, view: &mut View<Self::Cache>) {
        let queued = self.result_queue.drain_results_for(usize::from(view.get_id()));
        for response in queued {
            match response {
                LspResponse::Hover(_) => {}
                LspResponse::Diagnostics(result) => match result {
                    Ok(diagnostics) => {
                        if let Some(core) = &self.core {
                            core.update_diagnostics(view.get_id(), &diagnostics);
                        }
                    }
                    Err(err) => {
                        if let Ok(client) = self.client_for_view(view)
                            && let Ok(mut client) = client.lock()
                        {
                            client.record_server_failure(format!(
                                "failed to convert diagnostics: {:?}",
                                err
                            ));
                        }
                    }
                },
                LspResponse::Completions(result) => match result {
                    Ok(items) => {
                        self.pending_completions.insert(view.get_id(), items.clone());
                        if let Some(core) = &self.core {
                            let suggestions = items
                                .iter()
                                .map(|item| item.suggestion.clone())
                                .collect::<Vec<_>>();
                            core.show_completions(view.get_id(), &suggestions);
                        }
                    }
                    Err(err) => {
                        self.pending_completions.remove(&view.get_id());
                        self.record_view_failure(view, format!("completion failed: {err:?}"))
                    }
                },
                LspResponse::Locations { title, result } => match result {
                    Ok(locations) => {
                        if let Some(core) = &self.core {
                            core.show_locations(view.get_id(), &title, &locations);
                        }
                    }
                    Err(err) => self.record_view_failure(view, format!("{title} failed: {err:?}")),
                },
                LspResponse::Symbols { title, result } => match result {
                    Ok(symbols) => {
                        if let Some(core) = &self.core {
                            core.show_symbols(view.get_id(), &title, &symbols);
                        }
                    }
                    Err(err) => self.record_view_failure(view, format!("{title} failed: {err:?}")),
                },
                LspResponse::Formatting { title, result } => match result {
                    Ok(edits) => self.apply_named_edits(view, &title, &edits),
                    Err(err) => self.record_view_failure(view, format!("{title} failed: {err:?}")),
                },
                LspResponse::CodeActions(result) => match result {
                    Ok(actions) => self.handle_code_actions_result(view, actions),
                    Err(err) => {
                        self.record_view_failure(view, format!("code actions failed: {err:?}"))
                    }
                },
                LspResponse::Rename { title, result } => match result {
                    Ok(edit) => self.handle_rename_result(view, &title, edit),
                    Err(err) => self.record_view_failure(view, format!("{title} failed: {err:?}")),
                },
            }
        }
    }

    fn get_hover(
        &mut self,
        view: &mut View<Self::Cache>,
        position: usize,
        cancel: CancellationToken,
    ) -> Result<xi_plugin_lib::Hover, xi_rpc::RemoteError> {
        let view_id = view.get_id();
        let position =
            get_position_of_offset(view, position).map_err(LanguageResponseError::from)?;
        let (tx, rx) = mpsc::channel();

        let ls_client_arc = self.client_for_view(view).map_err(LanguageResponseError::from)?;
        let pending_request_id = {
            let mut ls_client = ls_client_arc.lock().map_err(|_| {
                LanguageResponseError::Transport(String::from(
                    "language server client lock poisoned",
                ))
            })?;
            ls_client.request_hover(view_id, position, move |_ls_client, result| {
                let response = result
                    .map_err(|e| LanguageResponseError::LanguageServerError(format!("{:?}", e)))
                    .and_then(|hover| {
                        serde_json::from_value::<Option<Hover>>(hover)
                            .map_err(|err| LanguageResponseError::Transport(err.to_string()))?
                            .ok_or(LanguageResponseError::NullResponse)
                    });
                let _ = tx.send(response);
            })
        };
        let pending_request_id = pending_request_id.map_err(LanguageResponseError::from)?;

        let timeout_at = Instant::now() + {
            let ls_client = ls_client_arc.lock().map_err(|_| {
                LanguageResponseError::Transport(String::from(
                    "language server client lock poisoned",
                ))
            })?;
            ls_client.long_request_timeout()
        };

        loop {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(response) => {
                    return response
                        .and_then(|hover| core_hover_from_hover(view, hover))
                        .map_err(Into::into);
                }
                Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() => {
                    if let Ok(mut ls_client) = ls_client_arc.lock() {
                        ls_client.cancel_request(pending_request_id);
                    }
                    return Err(xi_rpc::RemoteError::custom(-32800, "request cancelled", None));
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= timeout_at => {
                    if let Ok(mut ls_client) = ls_client_arc.lock() {
                        ls_client.cancel_request(pending_request_id);
                    }
                    return Err(xi_rpc::RemoteError::custom(
                        -32097,
                        "language server request timed out",
                        None,
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(xi_rpc::RemoteError::custom(
                        500,
                        "hover request channel disconnected",
                        None,
                    ));
                }
            }
        }
    }
}

/// Util Methods
impl LspPlugin {
    fn record_view_failure(&mut self, view: &mut View<ChunkCache>, message: String) {
        if let Ok(client) = self.client_for_view(view)
            && let Ok(mut client) = client.lock()
        {
            client.record_server_failure(message);
        }
    }

    fn current_position(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Position, LanguageResponseError> {
        let selection = view
            .get_selections()
            .map_err(LanguageResponseError::from)?
            .into_iter()
            .next()
            .unwrap_or(xi_plugin_lib::SelectionRange { start: 0, end: 0 });
        get_position_of_offset(view, selection.end).map_err(LanguageResponseError::from)
    }

    fn current_range(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Range, LanguageResponseError> {
        let selection = view
            .get_selections()
            .map_err(LanguageResponseError::from)?
            .into_iter()
            .next()
            .unwrap_or(xi_plugin_lib::SelectionRange { start: 0, end: 0 });
        let start = selection.start.min(selection.end);
        let end = selection.start.max(selection.end);
        Ok(Range {
            start: get_position_of_offset(view, start).map_err(LanguageResponseError::from)?,
            end: get_position_of_offset(view, end).map_err(LanguageResponseError::from)?,
        })
    }

    fn request_completion(&mut self, view: &mut View<ChunkCache>, index: Option<usize>) {
        let view_id = view.get_id();
        if let Some(index) = index {
            if let Some(item) = self
                .pending_completions
                .get(&view_id)
                .and_then(|items| index.checked_sub(1).and_then(|idx| items.get(idx)).cloned())
            {
                self.apply_completion(view, &item);
                return;
            }
        }

        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("completion failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_completion(view_id, position, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<CompletionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                                    .map(|response| {
                                        response
                                            .map(pending_completions_from_response)
                                            .unwrap_or_default()
                                    })
                            });
                        ls_client
                            .result_queue
                            .push_result(view_id.into(), LspResponse::Completions(response));
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("completion failed: {err}"));
        }
    }

    fn request_definition(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("definition failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("definition failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("definition failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("definition failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_definition(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<GotoDefinitionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_definition_response(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("definition"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("definition failed: {err}"));
        }
    }

    fn request_references(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("references failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("references failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("references failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("references failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_references(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<Vec<Location>>>(value).map_err(
                                    |err| LanguageResponseError::Transport(err.to_string()),
                                )
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_references(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("references"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("references failed: {err}"));
        }
    }

    fn request_document_symbols(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let file_path = match view.get_path() {
            Some(path) => path.to_string_lossy().to_string(),
            None => {
                self.record_view_failure(
                    view,
                    String::from("document symbols failed: missing file path"),
                );
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("document symbols failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("document symbols failed: missing file path"),
                );
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_document_symbols(view_id, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                // LSP returns either DocumentSymbol[] or SymbolInformation[].
                                if let Ok(Some(syms)) = serde_json::from_value::<
                                    Option<Vec<lsp_types::DocumentSymbol>>,
                                >(
                                    value.clone()
                                ) {
                                    let items = symbol_items_from_document_symbols(
                                        &current_document_uri,
                                        syms,
                                        &file_path,
                                    );
                                    return Ok(items);
                                }
                                serde_json::from_value::<Option<Vec<lsp_types::SymbolInformation>>>(
                                    value,
                                )
                                .map_err(|err| LanguageResponseError::Transport(err.to_string()))
                                .map(|opt| {
                                    symbol_items_from_workspace_symbols(opt.unwrap_or_default())
                                })
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Symbols {
                                title: String::from("symbols"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("document symbols failed: {err}"));
        }
    }

    fn request_workspace_symbols(&mut self, view: &mut View<ChunkCache>, query: String) {
        let view_id = view.get_id();
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_workspace_symbols(view_id, &query, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<Vec<lsp_types::SymbolInformation>>>(
                                    value,
                                )
                                .map_err(|err| LanguageResponseError::Transport(err.to_string()))
                                .map(|opt| {
                                    symbol_items_from_workspace_symbols(opt.unwrap_or_default())
                                })
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Symbols {
                                title: String::from("workspace symbols"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("workspace symbols failed: {err}"));
        }
    }

    fn request_document_formatting(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let options = Some(xi_core_lib::plugin_rpc::FormattingOptions {
            tab_size: view.get_config().tab_size,
            insert_spaces: view.get_config().translate_tabs_to_spaces,
        });
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_document_formatting(view_id, options, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<Vec<TextEdit>>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                                    .map(|response| response.unwrap_or_default())
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Formatting {
                                title: String::from("format"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("format failed: {err}"));
        }
    }

    fn request_or_apply_code_action(&mut self, view: &mut View<ChunkCache>, index: Option<usize>) {
        let view_id = view.get_id();
        if let Some(index) = index {
            if let Some(actions) = self.pending_code_actions.get(&view_id)
                && let Some(action) = index.checked_sub(1).and_then(|idx| actions.get(idx)).cloned()
            {
                self.apply_code_action(view, &action);
                return;
            }
        }

        let range = match self.current_range(view) {
            Ok(range) => range,
            Err(err) => {
                self.record_view_failure(view, format!("code actions failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                let document_uri = ls_client
                    .opened_documents
                    .get(&view_id)
                    .map(|state| state.uri.clone())
                    .ok_or_else(|| format!("missing open document for view {view_id}"))?;
                ls_client
                    .request_code_actions(view_id, range, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<CodeActionResponse>>(value).map_err(
                                    |err| LanguageResponseError::Transport(err.to_string()),
                                )
                            })
                            .and_then(|response| {
                                response
                                    .map(|response| {
                                        code_actions_from_response(response, &document_uri)
                                    })
                                    .transpose()
                                    .map(|response| response.unwrap_or_default())
                            });
                        ls_client
                            .result_queue
                            .push_result(view_id.into(), LspResponse::CodeActions(response));
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("code actions failed: {err}"));
        }
    }

    fn handle_code_actions_result(
        &mut self,
        view: &mut View<ChunkCache>,
        actions: Vec<LspCodeAction>,
    ) {
        let view_id = view.get_id();
        if actions.is_empty() {
            if let Some(core) = &self.core {
                core.alert("no code actions available");
            }
            self.pending_code_actions.remove(&view_id);
            return;
        }

        self.pending_code_actions.insert(view_id, actions.clone());
        if actions.len() == 1 {
            self.apply_code_action(view, &actions[0]);
            self.pending_code_actions.remove(&view_id);
            return;
        }

        if let Some(core) = &self.core {
            let actions = actions
                .iter()
                .map(|action| xi_core_lib::plugin_rpc::CodeActionDescriptor {
                    title: action.title.clone(),
                })
                .collect::<Vec<_>>();
            core.show_code_actions(view_id, &actions);
        }
    }

    fn apply_completion(&mut self, view: &mut View<ChunkCache>, item: &PendingCompletionItem) {
        match completion_text_edits(view, &item.item) {
            Ok(edits) => self.apply_named_edits(view, "completion", &edits),
            Err(err) => self.record_view_failure(view, format!("completion failed: {err:?}")),
        }
    }

    fn request_rename(&mut self, view: &mut View<ChunkCache>, new_name: String) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("rename failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_rename(view_id, position, new_name, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<WorkspaceEdit>>(value).map_err(
                                    |err| LanguageResponseError::Transport(err.to_string()),
                                )
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Rename { title: String::from("rename"), result: response },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("rename failed: {err}"));
        }
    }

    fn handle_rename_result(
        &mut self,
        view: &mut View<ChunkCache>,
        title: &str,
        edit: Option<WorkspaceEdit>,
    ) {
        let Some(edit) = edit else {
            if let Some(core) = &self.core {
                core.alert("rename produced no changes");
            }
            return;
        };

        let document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("{title} failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(view, format!("{title} failed: missing file path"));
                return;
            }
        };

        match workspace_edit_changes_only_document(&edit, &document_uri) {
            Ok(true) => {}
            Ok(false) => {
                self.record_view_failure(
                    view,
                    format!("{title} failed: multi-file rename is not supported yet"),
                );
                return;
            }
            Err(err) => {
                self.record_view_failure(view, format!("{title} failed: {err:?}"));
                return;
            }
        }

        match extract_document_edits_for_uri(edit, &document_uri) {
            Ok(edits) if edits.is_empty() => {
                if let Some(core) = &self.core {
                    core.alert("rename produced no document edits");
                }
            }
            Ok(edits) => self.apply_named_edits(view, title, &edits),
            Err(err) => self.record_view_failure(view, format!("{title} failed: {err:?}")),
        }
    }

    fn apply_code_action(&mut self, view: &mut View<ChunkCache>, action: &LspCodeAction) {
        if !action.edits.is_empty() {
            self.apply_named_edits(view, &action.title, &action.edits);
        }

        if let Some(command) = action.command.clone()
            && let Ok(ls_client_arc) = self.client_for_view(view)
            && let Ok(mut ls_client) = ls_client_arc.lock()
        {
            ls_client.send_request(
                "workspace/executeCommand",
                Params::from(serde_json::json!({
                    "command": command.command,
                    "arguments": command.arguments,
                })),
                Box::new(|client: &mut LanguageServerClient, result| {
                    if let Err(err) = result {
                        client.record_server_failure(format!("executeCommand failed: {err:?}"));
                    }
                }),
            );
        }
    }

    fn apply_named_edits(&mut self, view: &mut View<ChunkCache>, title: &str, edits: &[TextEdit]) {
        match apply_lsp_text_edits(view, edits, title) {
            Ok(ack) if ack.applied => {}
            Ok(ack) => {
                let reason = ack.reason.unwrap_or_else(|| String::from("edit rejected"));
                self.record_view_failure(view, format!("{title} rejected: {reason}"));
            }
            Err(err) => self.record_view_failure(view, format!("{title} failed: {err}")),
        }
    }

    fn workspace_root_for_view(&self, view: &View<ChunkCache>, language_id: &str) -> Option<Uri> {
        let config = self.config.language_config.get(language_id)?;

        config.workspace_identifier.as_ref().and_then(|identifier| {
            let path = view.get_path()?;
            get_workspace_root_uri(identifier, path).ok()
        })
    }

    fn language_server_key(
        &self,
        language_id: &str,
        workspace_root: &Option<Uri>,
    ) -> Option<String> {
        if let Some(root) = workspace_root {
            return Some(format!("{}:{}", language_id, root.as_str()));
        }

        self.config.language_config.get(language_id).and_then(|config| {
            config.supports_single_file.then(|| format!("{}:generic", language_id))
        })
    }

    /// Get the Language Server Client given the Workspace root
    /// This method checks if a language server is running at the specified root
    /// and returns it else it tries to spawn a new language server and returns a
    /// Arc reference to it
    fn get_lsclient_from_workspace_root(
        &mut self,
        language_id: &str,
        workspace_root: &Option<Uri>,
    ) -> Option<(String, Arc<Mutex<LanguageServerClient>>)> {
        self.language_server_key(language_id, workspace_root).and_then(
            |language_server_identifier| {
                let contains =
                    self.language_server_clients.contains_key(&language_server_identifier);

                if contains {
                    let client = self.language_server_clients[&language_server_identifier].clone();

                    Some((language_server_identifier, client))
                } else {
                    let config = &self.config.language_config[language_id];
                    let client = start_new_server(
                        config.start_command.clone(),
                        config.start_arguments.clone(),
                        config.extensions.clone(),
                        language_id,
                        self.core.clone()?,
                        self.result_queue.clone(),
                    );

                    match client {
                        Ok(client) => {
                            let client_clone = client.clone();
                            self.language_server_clients
                                .insert(language_server_identifier.clone(), client);

                            Some((language_server_identifier, client_clone))
                        }
                        Err(err) => {
                            error!(
                                "Error occured while starting server for Language: {}: {:?}",
                                language_id, err
                            );
                            None
                        }
                    }
                }
            },
        )
    }

    /// Tries to get language for the View using the extension of the document.
    /// Only searches for the languages supported by the Language Plugin as
    /// defined in the config
    fn get_language_for_view(&mut self, view: &View<ChunkCache>) -> Option<String> {
        view.get_path()
            .and_then(|path| path.extension())
            .and_then(|extension| extension.to_str())
            .and_then(|extension_str| {
                for (lang, config) in &self.config.language_config {
                    if config.extensions.iter().any(|x| x == extension_str) {
                        return Some(lang.clone());
                    }
                }
                None
            })
    }

    fn open_view_on_client(
        &self,
        view: &mut View<ChunkCache>,
        workspace_root: Option<Uri>,
        ls_client: &Arc<Mutex<LanguageServerClient>>,
    ) -> Result<(), Error> {
        let document_text = view
            .get_document()
            .map_err(|err| Error::Protocol(format!("document fetch failed: {err:?}")))?;
        let path = view.get_path().ok_or_else(|| {
            Error::Protocol(format!("view {} missing filesystem path", view.get_id()))
        })?;
        let document_uri = file_path_to_uri(path)?;
        let view_id = view.get_id();
        let mut ls_client =
            ls_client.lock().map_err(|_| Error::LockPoisoned("language server client"))?;

        if !ls_client.is_initialized && !ls_client.initialization_pending {
            ls_client.send_initialize(workspace_root, move |ls_client, result| {
                ls_client.initialization_pending = false;
                match result {
                    Ok(result) => match serde_json::from_value::<InitializeResult>(result) {
                        Ok(init_result) => {
                            debug!("Init Result: {:?}", init_result);
                            ls_client.server_capabilities = Some(init_result.capabilities);
                            ls_client.is_initialized = true;
                            ls_client.clear_server_failure();
                            if let Err(err) = ls_client.resend_open_documents() {
                                ls_client.record_server_failure(format!(
                                    "failed to resend open documents after initialize: {err}"
                                ));
                            }
                        }
                        Err(err) => ls_client.record_server_failure(format!(
                            "failed to parse initialize response: {err}"
                        )),
                    },
                    Err(err) => ls_client
                        .record_server_failure(format!("initialize request failed: {err:?}")),
                }
            })?;
        }

        ls_client.send_did_open(view_id, document_uri, document_text)
    }

    fn restart_client_for_view(
        &mut self,
        view: &mut View<ChunkCache>,
        view_info: &ViewInfo,
    ) -> Result<Arc<Mutex<LanguageServerClient>>, Error> {
        let Some(config) = self.config.language_config.get(&view_info.language_id) else {
            return Err(Error::Protocol(format!(
                "missing language config for {}",
                view_info.language_id
            )));
        };

        let previous_documents = self
            .language_server_clients
            .get(&view_info.ls_identifier)
            .and_then(|client| client.lock().ok().map(|client| client.open_document_states()))
            .unwrap_or_default();

        let core =
            self.core.clone().ok_or_else(|| Error::Protocol(String::from("missing core proxy")))?;
        let client = start_new_server(
            config.start_command.clone(),
            config.start_arguments.clone(),
            config.extensions.clone(),
            &view_info.language_id,
            core,
            self.result_queue.clone(),
        )?;

        {
            let mut new_client =
                client.lock().map_err(|_| Error::LockPoisoned("language server client"))?;
            for (view_id, state) in previous_documents {
                new_client.opened_documents.insert(view_id, state);
            }
        }

        self.language_server_clients.insert(view_info.ls_identifier.clone(), client.clone());
        self.open_view_on_client(view, view_info.workspace_root.clone(), &client)?;
        Ok(client)
    }

    fn client_for_view(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Arc<Mutex<LanguageServerClient>>, Error> {
        let Some(view_info) = self.view_info.get(&view.get_id()).cloned() else {
            return Err(Error::Protocol(format!("missing language server view {}", view.get_id())));
        };
        let Some(client) = self.language_server_clients.get(&view_info.ls_identifier).cloned()
        else {
            return self.restart_client_for_view(view, &view_info);
        };

        let exited = {
            let client_guard =
                client.lock().map_err(|_| Error::LockPoisoned("language server client"))?;
            client_guard.exit_status()?.is_some()
        };

        if exited {
            return self.restart_client_for_view(view, &view_info);
        }

        Ok(client)
    }
}
