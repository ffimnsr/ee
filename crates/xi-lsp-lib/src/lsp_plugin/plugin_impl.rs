//! `impl ee_plugin_sdk::Plugin for LspPlugin`: v2 lifecycle.
use super::*;
use ee_plugin_api::context::PluginContext;
use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::PluginUpdate;
use ee_plugin_api::wire::EventParams;

impl Plugin for LspPlugin {
    fn name(&self) -> &str {
        "xi-lsp-plugin"
    }

    fn apply(&mut self, ctx: &PluginContext) -> Result<(), PluginError> {
        let caller = ctx
            .service_caller()
            .ok_or(PluginError::Unsupported { feature: "native service transport" })?;
        self.core = Some(HostUi::new(Arc::clone(&caller)));
        self.caller = Some(caller);
        Ok(())
    }

    fn on_event(
        &mut self,
        _ctx: &PluginContext,
        event: &EventParams,
    ) -> Result<Vec<Effect>, PluginError> {
        match event.name.as_str() {
            "editor.view.opened" => self.on_view_opened(&event.payload),
            "editor.document.changed" => self.on_document_changed(&event.payload),
            "editor.document.saved" => self.on_document_saved(&event.payload),
            "editor.view.closed" => self.on_view_closed(&event.payload),
            "editor.language.changed" => self.on_language_changed(&event.payload),
            "editor.config.changed" => self.on_config_changed(&event.payload),
            _ => {}
        }
        Ok(Vec::new())
    }

    fn on_command(
        &mut self,
        _ctx: &PluginContext,
        request: &ee_plugin_api::effect::CommandRequest,
    ) -> Result<Vec<Effect>, PluginError> {
        let Some(mut view) = self.view_for_command(request.view_id) else {
            return Ok(Vec::new());
        };
        let view_id = view.get_id();
        let index = request
            .args
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        match request.id.as_str() {
            "lsp.completion" => self.request_completion(&mut view, index),
            "lsp.hover" => self.request_hover(&mut view),
            "lsp.declaration" => self.request_declaration(&mut view),
            "lsp.definition" => self.request_definition(&mut view),
            "lsp.type_definition" => self.request_type_definition(&mut view),
            "lsp.references" => self.request_references(&mut view),
            "lsp.implementation" => self.request_implementation(&mut view),
            "lsp.document_symbols" => self.request_document_symbols(&mut view),
            "lsp.workspace_symbols" => {
                let query =
                    request.args.get("query").and_then(Value::as_str).unwrap_or("").to_owned();
                self.request_workspace_symbols(&mut view, query);
            }
            "lsp.format_document" => self.request_document_formatting(&mut view),
            "lsp.code_actions" => {
                if let Some(only) = request.args.get("only").and_then(Value::as_array) {
                    let only = only
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    if !only.is_empty() {
                        self.request_code_actions_on_save(&mut view, only);
                        return Ok(Vec::new());
                    }
                }
                self.request_or_apply_code_action(&mut view, index);
            }
            "lsp.rename" => {
                let Some(new_name) = request.args.get("new_name").and_then(Value::as_str) else {
                    self.record_view_failure(
                        &mut view,
                        String::from("rename failed: missing new_name"),
                    );
                    return Ok(Vec::new());
                };
                self.request_rename(&mut view, new_name.to_owned());
            }
            "lsp.agent_document_symbols" => self.request_agent_document_symbols(&mut view),
            "lsp.agent_references" => self.request_agent_references(&mut view),
            "lsp.agent_code_actions" => self.request_agent_code_actions(&mut view),
            "lsp.agent_format_preview" => self.request_agent_format_preview(&mut view),
            "lsp.agent_rename_preview" => {
                let Some(new_name) = request.args.get("new_name").and_then(Value::as_str) else {
                    self.record_view_failure(
                        &mut view,
                        String::from("rename failed: missing new_name"),
                    );
                    return Ok(Vec::new());
                };
                self.request_agent_preview_rename(&mut view, new_name.to_owned());
            }
            other => {
                return Err(PluginError::Runtime {
                    message: format!("unhandled LSP command `{other}`"),
                });
            }
        }
        let _ = view_id;
        Ok(Vec::new())
    }

    fn on_tick(&mut self, _ctx: &PluginContext) -> Result<Vec<Effect>, PluginError> {
        let views = self.views.keys().copied().collect::<Vec<_>>();
        for view_id in views {
            if let Some(mut view) = self.views.get(&view_id).cloned() {
                self.idle(&mut view);
            }
        }
        Ok(Vec::new())
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(20))
    }

    fn on_shutdown(&mut self, _ctx: &PluginContext) {
        self.shutdown_servers();
    }
}

impl LspPlugin {
    fn view_for_command(&mut self, view_id: ViewId) -> Option<PluginView> {
        if let Some(view) = self.views.get(&view_id) {
            return Some(view.clone());
        }
        self.views.values().next().cloned()
    }

    fn on_view_opened(&mut self, payload: &Value) {
        let Some(view_id) =
            payload.get("view_id").and_then(|value| serde_json::from_value(value.clone()).ok())
        else {
            return;
        };
        let Some(caller) = self.caller.clone() else {
            return;
        };
        let mut view = match PluginView::load(caller, view_id) {
            Ok(view) => view,
            Err(error) => {
                error!("failed to load view {view_id:?}: {error}");
                return;
            }
        };
        if let Some(config) = payload.get("config") {
            view.set_config(config.clone());
        }
        self.views.insert(view_id, view.clone());
        self.new_view(&mut view);
    }

    fn on_document_changed(&mut self, payload: &Value) {
        let Ok(update) = serde_json::from_value::<PluginUpdate>(payload.clone()) else {
            return;
        };
        let Some(mut view) = self.views.get(&update.view_id).cloned() else {
            return;
        };
        if let Err(error) = view.apply_update(&update) {
            error!("failed to apply document update for {:?}: {error}", update.view_id);
            return;
        }
        self.update(&mut view);
    }

    fn on_document_saved(&mut self, payload: &Value) {
        let Some(view_id) =
            payload.get("view_id").and_then(|value| serde_json::from_value(value.clone()).ok())
        else {
            return;
        };
        let Some(mut view) = self.views.get(&view_id).cloned() else {
            return;
        };
        self.did_save(&mut view);
    }

    fn on_view_closed(&mut self, payload: &Value) {
        let Some(view_id) =
            payload.get("view_id").and_then(|value| serde_json::from_value(value.clone()).ok())
        else {
            return;
        };
        if let Some(view) = self.views.remove(&view_id) {
            self.did_close(&view);
        }
    }

    fn on_language_changed(&mut self, payload: &Value) {
        let Some(view_id) =
            payload.get("view_id").and_then(|value| serde_json::from_value(value.clone()).ok())
        else {
            return;
        };
        let language =
            payload.get("new").and_then(|value| serde_json::from_value(value.clone()).ok());
        let Some(mut view) = self.views.get(&view_id).cloned() else {
            return;
        };
        self.did_close(&view);
        if let Some(language) = language {
            view.set_language(language);
            self.views.insert(view_id, view.clone());
        }
        self.new_view(&mut view);
    }

    fn on_config_changed(&mut self, payload: &Value) {
        let Some(changes) = payload.get("changes").and_then(Value::as_object) else {
            return;
        };
        let table = changes.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        match self.parse_plugin_config_update(&table) {
            Ok(config) => self.apply_plugin_config(config),
            Err(error) => error!("failed to parse lsp plugin config update: {error}"),
        }
    }

    fn update(&mut self, view: &mut PluginView) {
        if !self.view_info.contains_key(&view.get_id()) {
            return;
        }
        let document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                error!("failed to fetch document for view {:?} update: {err}", view.get_id());
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
            && let Some(changes) = get_change_for_sync_kind(sync_kind, view)
        {
            for ls_client_arc in ls_clients {
                let Ok(mut ls_client) = ls_client_arc.lock() else {
                    error!("language server client lock poisoned for view {:?}", view.get_id());
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

    fn did_save(&mut self, view: &mut PluginView) {
        trace!("saved view {:?}", view.get_id());
        let document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                error!("failed to fetch document for view {:?} save: {err}", view.get_id());
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

    fn did_close(&mut self, view: &PluginView) {
        trace!("close view {:?}", view.get_id());
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
        self.pending_symbol_retries.remove(&view.get_id());
    }

    fn new_view(&mut self, view: &mut PluginView) {
        trace!("new view {:?}", view.get_id());
        let language_id = self.normalized_view_language_id(view.get_language_id().as_str());
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
                    error!("failed to initialize language server view {:?}: {err}", view.get_id());
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

    fn shutdown_servers(&mut self) {
        let clients = self.language_server_clients.values().cloned().collect::<Vec<_>>();
        for client in clients {
            if let Err(err) = shutdown_language_server(&client) {
                error!("failed to shutdown language server: {err}");
            }
        }
    }

    fn idle(&mut self, view: &mut PluginView) {
        let queued = self.result_queue.drain_results_for(view.get_id().0 as usize);
        for response in queued {
            match response {
                LspResponse::Hover(_) => {}
                LspResponse::Diagnostics(result) => match result {
                    Ok(diagnostics) => {
                        if let Some(core) = &self.core {
                            core.set_diagnostics(view.get_id(), view.rev(), &diagnostics);
                        }
                    }
                    Err(err) => {
                        if let Ok(client) = self.client_for_view(view)
                            && let Ok(mut client) = client.lock()
                        {
                            client.record_server_failure(format!(
                                "failed to convert diagnostics: {err:?}"
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
                LspResponse::AutoCodeActions(result) => match result {
                    Ok(actions) => {
                        for action in actions {
                            self.apply_code_action(view, &action);
                        }
                    }
                    Err(err) => self
                        .record_view_failure(view, format!("code actions on save failed: {err:?}")),
                },
                LspResponse::Rename { title, result } => match result {
                    Ok(edit) => self.handle_rename_result(view, &title, edit),
                    Err(err) => self.record_view_failure(view, format!("{title} failed: {err:?}")),
                },
            }
        }
        if self.pending_symbol_retries.contains_key(&view.get_id()) {
            self.request_document_symbols(view);
        }
    }
}
