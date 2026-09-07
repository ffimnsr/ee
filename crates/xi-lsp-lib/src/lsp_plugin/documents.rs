//! `impl LspPlugin` methods: documents.
use super::*;

impl LspPlugin {
    pub(super) fn request_document_symbols(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let Some(context) = DocumentSymbolContext::capture(view).map(Arc::new) else {
            self.record_view_failure(
                view,
                String::from("document symbols failed: missing file path"),
            );
            return;
        };
        let ls_client_arc = match self.client_for_view(view) {
            Ok(client) => client,
            Err(err) => {
                debug!("document symbols using Tree-sitter fallback: {err}");
                self.queue_document_symbols(view_id, context.fallback_symbols());
                return;
            }
        };
        let callback_context = Arc::clone(&context);
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_document_symbols(view_id, move |ls_client, result| {
                        let symbols = callback_context.resolve_lsp_response(result);
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Symbols {
                                title: String::from("symbols"),
                                result: Ok(symbols),
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            debug!("document symbols request failed; using Tree-sitter fallback: {err}");
            self.queue_document_symbols(view_id, context.fallback_symbols());
        }
    }

    pub(super) fn queue_document_symbols(
        &mut self,
        view_id: ViewId,
        symbols: Vec<xi_core_lib::plugin_rpc::SymbolItem>,
    ) {
        self.result_queue.push_result(
            view_id.into(),
            LspResponse::Symbols { title: String::from("symbols"), result: Ok(symbols) },
        );
        if let Some(core) = &self.core {
            core.schedule_idle(view_id);
        }
    }

    pub(super) fn request_workspace_symbols(&mut self, view: &mut View<ChunkCache>, query: String) {
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

    pub(super) fn request_document_formatting(&mut self, view: &mut View<ChunkCache>) {
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

    pub(super) fn request_or_apply_code_action(
        &mut self,
        view: &mut View<ChunkCache>,
        index: Option<usize>,
    ) {
        let view_id = view.get_id();
        if let Some(index) = index
            && let Some(actions) = self.pending_code_actions.get(&view_id)
            && let Some(action) = index.checked_sub(1).and_then(|idx| actions.get(idx)).cloned()
        {
            self.apply_code_action(view, &action);
            return;
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

    pub(super) fn handle_code_actions_result(
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

    pub(super) fn apply_completion(
        &mut self,
        view: &mut View<ChunkCache>,
        item: &PendingCompletionItem,
    ) {
        match completion_text_edits(view, &item.item) {
            Ok(edits) => self.apply_named_edits(view, "completion", &edits),
            Err(err) => self.record_view_failure(view, format!("completion failed: {err:?}")),
        }
    }

    pub(super) fn request_rename(&mut self, view: &mut View<ChunkCache>, new_name: String) {
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

    pub(super) fn handle_rename_result(
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

    pub(super) fn apply_code_action(
        &mut self,
        view: &mut View<ChunkCache>,
        action: &LspCodeAction,
    ) {
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

    pub(super) fn apply_named_edits(
        &mut self,
        view: &mut View<ChunkCache>,
        title: &str,
        edits: &[TextEdit],
    ) {
        match apply_lsp_text_edits(view, edits, title) {
            Ok(ack) if ack.applied => {}
            Ok(ack) => {
                let reason = ack.reason.unwrap_or_else(|| String::from("edit rejected"));
                self.record_view_failure(view, format!("{title} rejected: {reason}"));
            }
            Err(err) => self.record_view_failure(view, format!("{title} failed: {err}")),
        }
    }

    pub(super) fn language_server_key(
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
    pub(super) fn get_lsclient_from_workspace_root(
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
                        language_id,
                        self.core.clone()?,
                        self.result_queue.clone(),
                        ServerStartOptions {
                            file_extensions: config.extensions.clone(),
                            env_overrides: config.env.clone(),
                            initialization_options: config.initialization_options.clone(),
                        },
                    );

                    match client {
                        Ok(client) => {
                            let client_clone = client.clone();
                            self.language_server_clients
                                .insert(language_server_identifier.clone(), client);

                            Some((language_server_identifier, client_clone))
                        }
                        Err(err) => {
                            Self::log_spawn_failure(
                                language_id,
                                &self.config.language_config[language_id].start_command,
                                &err,
                            );
                            None
                        }
                    }
                }
            },
        )
    }

    pub(super) fn language_matches_for_view(&self, view: &View<ChunkCache>) -> Vec<LanguageMatch> {
        let Some(path) = view.get_path() else {
            return Vec::new();
        };
        let language_id = self.normalized_view_language_id(view.get_language_id());
        self.language_matches_for_path(path, Some(&language_id))
    }

    pub(super) fn open_view_on_client(
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

    pub(super) fn restart_client_for_route(
        &mut self,
        view: &mut View<ChunkCache>,
        route: &ViewServerRoute,
    ) -> Result<Arc<Mutex<LanguageServerClient>>, Error> {
        let Some(config) = self.config.language_config.get(&route.server_id) else {
            return Err(Error::Protocol(format!(
                "missing language config for {}",
                route.server_id
            )));
        };

        let previous_documents = self
            .language_server_clients
            .get(&route.ls_identifier)
            .and_then(|client| client.lock().ok().map(|client| client.open_document_states()))
            .unwrap_or_default();

        let core =
            self.core.clone().ok_or_else(|| Error::Protocol(String::from("missing core proxy")))?;
        let client = start_new_server(
            config.start_command.clone(),
            config.start_arguments.clone(),
            &route.server_id,
            core,
            self.result_queue.clone(),
            ServerStartOptions {
                file_extensions: config.extensions.clone(),
                env_overrides: config.env.clone(),
                initialization_options: config.initialization_options.clone(),
            },
        )?;

        {
            let mut new_client =
                client.lock().map_err(|_| Error::LockPoisoned("language server client"))?;
            for (view_id, state) in previous_documents {
                new_client.opened_documents.insert(view_id, state);
            }
        }

        self.language_server_clients.insert(route.ls_identifier.clone(), client.clone());
        self.open_view_on_client(view, route.workspace_root.clone(), &client)?;
        Ok(client)
    }

    pub(super) fn client_for_route(
        &mut self,
        view: &mut View<ChunkCache>,
        route: &ViewServerRoute,
    ) -> Result<Arc<Mutex<LanguageServerClient>>, Error> {
        let Some(client) = self.language_server_clients.get(&route.ls_identifier).cloned() else {
            return self.restart_client_for_route(view, route);
        };

        let exited = {
            let client_guard =
                client.lock().map_err(|_| Error::LockPoisoned("language server client"))?;
            client_guard.exit_status()?.is_some()
        };

        if exited {
            return self.restart_client_for_route(view, route);
        }

        Ok(client)
    }

    pub(super) fn clients_for_view(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Vec<Arc<Mutex<LanguageServerClient>>>, Error> {
        let Some(view_info) = self.view_info.get(&view.get_id()).cloned() else {
            return Err(Error::Protocol(format!("missing language server view {}", view.get_id())));
        };

        view_info.routes.iter().map(|route| self.client_for_route(view, route)).collect()
    }

    pub(super) fn client_for_view(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Arc<Mutex<LanguageServerClient>>, Error> {
        let Some(view_info) = self.view_info.get(&view.get_id()).cloned() else {
            return Err(Error::Protocol(format!("missing language server view {}", view.get_id())));
        };
        let Some(route) = view_info.routes.first() else {
            return Err(Error::Protocol(format!(
                "missing primary language server view {}",
                view.get_id()
            )));
        };
        self.client_for_route(view, route)
    }
}
