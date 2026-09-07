//! `impl LspPlugin` methods: agent_requests.
use super::*;

impl LspPlugin {
    pub(super) fn request_agent_document_symbols(&mut self, view: &mut View<ChunkCache>) {
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
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_document_symbols(view_id, move |ls_client, result| {
                        let payload = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                if let Ok(Some(symbols)) = serde_json::from_value::<
                                    Option<Vec<lsp_types::DocumentSymbol>>,
                                >(
                                    value.clone()
                                ) {
                                    let mut out = Vec::new();
                                    flatten_agent_document_symbols(symbols, &file_path, &mut out);
                                    return Ok(json!({ "symbols": out }));
                                }
                                let symbols = serde_json::from_value::<
                                    Option<Vec<lsp_types::SymbolInformation>>,
                                >(value)
                                .map_err(|err| LanguageResponseError::Transport(err.to_string()))?
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|symbol| {
                                    let path = uri_to_file_path(&symbol.location.uri).ok()?;
                                    Some(AgentToolDocumentSymbol {
                                        name: symbol.name,
                                        kind: agent_symbol_kind_name(symbol.kind),
                                        range: agent_tool_range(symbol.location.range),
                                        selection_range: agent_tool_range(symbol.location.range),
                                        container_path: path,
                                    })
                                })
                                .collect::<Vec<_>>();
                                Ok(json!({ "symbols": symbols }))
                            });
                        match payload {
                            Ok(payload) => ls_client.core.show_agent_tool_result(
                                view_id,
                                "document_symbols",
                                payload,
                            ),
                            Err(err) => {
                                ls_client.record_server_failure(format!(
                                    "document symbols failed: {err:?}"
                                ));
                            }
                        }
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("document symbols failed: {err}"));
        }
    }

    pub(super) fn request_agent_references(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
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
                        let payload = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                let locations =
                                    serde_json::from_value::<Option<Vec<Location>>>(value)
                                        .map_err(|err| {
                                            LanguageResponseError::Transport(err.to_string())
                                        })?
                                        .unwrap_or_default();
                                let references = locations
                                    .into_iter()
                                    .map(|location| {
                                        Ok(AgentToolReference {
                                            path: uri_to_file_path(&location.uri)?,
                                            range: agent_tool_range(location.range),
                                        })
                                    })
                                    .collect::<Result<Vec<_>, LanguageResponseError>>()?;
                                Ok(json!({ "references": references }))
                            });
                        match payload {
                            Ok(payload) => ls_client.core.show_agent_tool_result(
                                view_id,
                                "references",
                                payload,
                            ),
                            Err(err) => {
                                ls_client
                                    .record_server_failure(format!("references failed: {err:?}"));
                            }
                        }
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("references failed: {err}"));
        }
    }

    pub(super) fn request_agent_code_actions(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
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
                        let payload = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<CodeActionResponse>>(value).map_err(
                                    |err| LanguageResponseError::Transport(err.to_string()),
                                )
                            })
                            .and_then(|response| {
                                let actions = response.unwrap_or_default();
                                let mut out = Vec::new();
                                for action in actions {
                                    match action {
                                        CodeActionOrCommand::Command(command) => {
                                            out.push(AgentToolCodeAction {
                                                title: command.title,
                                                kind: None,
                                                has_command: true,
                                                edits: Vec::new(),
                                            })
                                        }
                                        CodeActionOrCommand::CodeAction(action) => {
                                            let edits = action
                                                .edit
                                                .map(|edit| {
                                                    extract_document_edits_for_uri(
                                                        edit,
                                                        &document_uri,
                                                    )
                                                })
                                                .transpose()?
                                                .unwrap_or_default()
                                                .into_iter()
                                                .map(|edit| AgentToolCodeActionEdit {
                                                    range: agent_tool_range(edit.range),
                                                    new_text: edit.new_text,
                                                })
                                                .collect();
                                            out.push(AgentToolCodeAction {
                                                title: action.title,
                                                kind: action
                                                    .kind
                                                    .map(|kind| kind.as_str().to_owned()),
                                                has_command: action.command.is_some(),
                                                edits,
                                            });
                                        }
                                    }
                                }
                                Ok(json!({ "actions": out }))
                            });
                        match payload {
                            Ok(payload) => ls_client.core.show_agent_tool_result(
                                view_id,
                                "list_code_actions",
                                payload,
                            ),
                            Err(err) => ls_client
                                .record_server_failure(format!("code actions failed: {err:?}")),
                        }
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("code actions failed: {err}"));
        }
    }

    pub(super) fn request_agent_format_preview(&mut self, view: &mut View<ChunkCache>) {
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
                        let payload = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                let edits = serde_json::from_value::<Option<Vec<TextEdit>>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })?
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|edit| AgentToolCodeActionEdit {
                                        range: agent_tool_range(edit.range),
                                        new_text: edit.new_text,
                                    })
                                    .collect::<Vec<_>>();
                                Ok(json!({ "edits": edits }))
                            });
                        match payload {
                            Ok(payload) => ls_client.core.show_agent_tool_result(
                                view_id,
                                "format_preview",
                                payload,
                            ),
                            Err(err) => {
                                ls_client.record_server_failure(format!("format failed: {err:?}"))
                            }
                        }
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("format failed: {err}"));
        }
    }

    pub(super) fn request_agent_preview_rename(
        &mut self,
        view: &mut View<ChunkCache>,
        new_name: String,
    ) {
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
                        let payload = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                let edit = serde_json::from_value::<Option<WorkspaceEdit>>(value)
                                    .map_err(|err| {
                                    LanguageResponseError::Transport(err.to_string())
                                })?;
                                let files = edit
                                    .map(agent_workspace_edit_files)
                                    .transpose()?
                                    .unwrap_or_default();
                                Ok(json!({ "files": files }))
                            });
                        match payload {
                            Ok(payload) => ls_client.core.show_agent_tool_result(
                                view_id,
                                "preview_rename",
                                payload,
                            ),
                            Err(err) => {
                                ls_client.record_server_failure(format!("rename failed: {err:?}"))
                            }
                        }
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("rename failed: {err}"));
        }
    }
}
