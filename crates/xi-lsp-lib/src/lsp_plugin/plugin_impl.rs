//! `impl Plugin for LspPlugin`: xi plugin lifecycle.
use super::*;

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

            let Ok(ls_clients) = self.clients_for_view(view) else {
                return;
            };
            let sync_kind = ls_clients
                .iter()
                .find_map(|client| client.lock().ok().map(|mut client| client.get_sync_kind()));
            let next_version = {
                let Some(view_info) = self.view_info.get_mut(&view.get_id()) else {
                    return;
                };
                view_info.version += 1;
                view_info.version
            };
            if let Some(sync_kind) = sync_kind
                && let Some(changes) = get_change_for_sync_kind(sync_kind, view, delta)
            {
                for ls_client_arc in ls_clients {
                    let Ok(mut ls_client) = ls_client_arc.lock() else {
                        error!("language server client lock poisoned for view {}", view.get_id());
                        continue;
                    };
                    if let Err(err) = ls_client.send_did_change(
                        view.get_id(),
                        changes.clone(),
                        next_version,
                        document_text.clone(),
                    ) {
                        ls_client.record_server_failure(format!("failed to send didChange: {err}"));
                    }
                }
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

        if let Ok(ls_clients) = self.clients_for_view(view) {
            for ls_client_arc in ls_clients {
                if let Ok(mut ls_client) = ls_client_arc.lock()
                    && let Err(err) = ls_client.send_did_save(view.get_id(), &document_text)
                {
                    ls_client.record_server_failure(format!("failed to send didSave: {err}"));
                }
            }
        }
    }

    fn did_close(&mut self, view: &View<Self::Cache>) {
        trace!("close view {}", view.get_id());

        if let Some(view_info) = self.view_info.remove(&view.get_id()) {
            for route in view_info.routes {
                if let Some(ls_client_arc) =
                    self.language_server_clients.get(&route.ls_identifier).cloned()
                    && let Ok(mut ls_client) = ls_client_arc.lock()
                    && let Err(err) = ls_client.send_did_close(view.get_id())
                {
                    ls_client.record_server_failure(format!("failed to send didClose: {err}"));
                }
            }
        }
        if let Some(key) = self.disabled_views.remove(&view.get_id()) {
            self.remove_status_item(view.get_id(), &key);
        }
        if let Some(key) = self.inactive_views.remove(&view.get_id()) {
            self.remove_status_item(view.get_id(), &key);
        }
        if let Some(key) = self.route_views.remove(&view.get_id()) {
            self.remove_status_item(view.get_id(), &key);
        }
    }

    fn new_view(&mut self, view: &mut View<Self::Cache>) {
        trace!("new view {}", view.get_id());

        let language_id = self.normalized_view_language_id(view.get_language_id());
        let language_matches = self.language_matches_for_view(view);
        if language_matches.is_empty() {
            return;
        }

        let disabled_server_ids = language_matches
            .iter()
            .filter_map(|language_match| match language_match {
                LanguageMatch::Disabled(server_id) => Some(server_id.clone()),
                LanguageMatch::Enabled(_) => None,
            })
            .collect::<Vec<_>>();
        let Some(path) = view.get_path() else {
            return;
        };
        let path = path.to_path_buf();
        let routes = self.routes_for_path(&path, &language_matches);

        if routes.is_empty() {
            if !disabled_server_ids.is_empty() {
                let key = self.add_disabled_status(view.get_id(), &language_id);
                self.disabled_views.insert(view.get_id(), key);
            } else {
                let key = self.add_unsupported_workspace_status(view.get_id(), &language_id);
                self.inactive_views.insert(view.get_id(), key);
            }
            return;
        }

        let mut active_routes = Vec::new();
        for route in routes {
            if let Some((identifier, ls_client)) =
                self.get_lsclient_from_workspace_root(&route.server_id, &route.workspace_root)
            {
                let route = ViewServerRoute {
                    server_id: route.server_id,
                    ls_identifier: identifier,
                    workspace_root: route.workspace_root,
                };
                if let Err(err) =
                    self.open_view_on_client(view, route.workspace_root.clone(), &ls_client)
                {
                    error!(
                        "failed to initialize language server view {}: {:?}",
                        view.get_id(),
                        err
                    );
                    continue;
                }
                active_routes.push(route);
            } else if let Some(config) = self.config.language_config.get(&route.server_id) {
                let key = self.add_spawn_failure_status(
                    view.get_id(),
                    &route.server_id,
                    &config.start_command,
                );
                self.inactive_views.insert(view.get_id(), key);
            }
        }

        if active_routes.is_empty() {
            return;
        }

        self.view_info.insert(
            view.get_id(),
            ViewInfo {
                version: 0,
                language_id: language_id.clone(),
                routes: active_routes.clone(),
                path,
            },
        );
        self.update_route_status(view.get_id(), &language_id, &active_routes);
    }

    fn language_changed(&mut self, view: &mut View<Self::Cache>, _old_lang: LanguageId) {
        self.did_close(view);
        self.new_view(view);
    }

    fn plugin_config_changed(&mut self, changes: &ConfigTable) {
        let next_config = match self.parse_plugin_config_update(changes) {
            Ok(config) => config,
            Err(err) => {
                error!("failed to parse lsp plugin config update: {}", err);
                return;
            }
        };

        self.apply_plugin_config(next_config);
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
            "request_declaration" | "lsp.declaration" => self.request_declaration(view),
            "request_definition" | "lsp.definition" => self.request_definition(view),
            "request_type_definition" | "lsp.type_definition" => {
                self.request_type_definition(view);
            }
            "request_references" | "lsp.references" => self.request_references(view),
            "ee.agent.references" => self.request_agent_references(view),
            "request_implementation" | "lsp.implementation" => {
                self.request_implementation(view);
            }
            "request_document_symbols" | "lsp.document_symbols" => {
                self.request_document_symbols(view);
            }
            "ee.agent.document_symbols" => self.request_agent_document_symbols(view),
            "request_workspace_symbols" | "lsp.workspace_symbols" => {
                let query = params.get("query").and_then(Value::as_str).unwrap_or("").to_owned();
                self.request_workspace_symbols(view, query);
            }
            "format_document" | "lsp.format_document" => self.request_document_formatting(view),
            "ee.agent.format_preview" => self.request_agent_format_preview(view),
            "request_code_actions" | "lsp.code_action" => {
                let index = params
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok());
                self.request_or_apply_code_action(view, index);
            }
            "ee.agent.list_code_actions" => self.request_agent_code_actions(view),
            "request_rename" => {
                let Some(new_name) = params.get("new_name").and_then(Value::as_str) else {
                    self.record_view_failure(view, String::from("rename failed: missing new_name"));
                    return;
                };
                self.request_rename(view, new_name.to_owned());
            }
            "ee.agent.preview_rename" => {
                let Some(new_name) = params.get("new_name").and_then(Value::as_str) else {
                    self.record_view_failure(view, String::from("rename failed: missing new_name"));
                    return;
                };
                self.request_agent_preview_rename(view, new_name.to_owned());
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
