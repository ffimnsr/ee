//! `impl LspPlugin`: config parsing, view routes, and status items.
use super::*;

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
            disabled_views: HashMap::new(),
            inactive_views: HashMap::new(),
            route_views: HashMap::new(),
        }
    }

    pub(super) fn parse_plugin_config_update(
        &self,
        changes: &ConfigTable,
    ) -> Result<Config, serde_json::Error> {
        let mut merged = match serde_json::to_value(&self.config) {
            Ok(Value::Object(config)) => config,
            Ok(_) => Map::new(),
            Err(_) => Map::new(),
        };
        merge_json_object(&mut merged, changes);

        match serde_json::from_value(Value::Object(merged)) {
            Ok(config) => Ok(config),
            Err(_err)
                if changes.keys().all(|key| {
                    !matches!(
                        key.as_str(),
                        "language_config" | "disabled_language_config" | "language_servers"
                    )
                }) =>
            {
                Ok(self.config.clone())
            }
            Err(err) => Err(err),
        }
    }

    pub(super) fn apply_plugin_config(&mut self, next_config: Config) {
        let affected_languages = self.changed_language_ids(&next_config);
        self.config = next_config;

        if affected_languages.is_empty() {
            return;
        }

        let restart_groups = self.rebuild_view_mappings(&affected_languages);
        self.restart_groups(restart_groups);
    }

    pub(super) fn changed_language_ids(&self, next_config: &Config) -> HashSet<String> {
        let known_ids = self
            .config
            .language_config
            .keys()
            .chain(next_config.language_config.keys())
            .chain(self.config.disabled_language_config.keys())
            .chain(next_config.disabled_language_config.keys())
            .chain(self.config.language_servers.keys())
            .chain(next_config.language_servers.keys())
            .cloned()
            .collect::<HashSet<_>>();

        known_ids
            .into_iter()
            .filter(|language_id| {
                serde_json::to_value(self.config.language_config.get(language_id)).ok()
                    != serde_json::to_value(next_config.language_config.get(language_id)).ok()
                    || serde_json::to_value(self.config.disabled_language_config.get(language_id))
                        .ok()
                        != serde_json::to_value(
                            next_config.disabled_language_config.get(language_id),
                        )
                        .ok()
                    || serde_json::to_value(self.config.language_servers.get(language_id)).ok()
                        != serde_json::to_value(next_config.language_servers.get(language_id)).ok()
            })
            .collect()
    }

    pub(super) fn rebuild_view_mappings(
        &mut self,
        affected_languages: &HashSet<String>,
    ) -> BTreeMap<String, ClientRestartGroup> {
        let mut previous_documents = HashMap::<ViewId, OpenDocumentState>::new();
        let mut groups = BTreeMap::<String, ClientRestartGroup>::new();
        let affected_keys = self
            .language_server_clients
            .keys()
            .filter(|key| affected_languages.iter().any(|id| key.starts_with(&format!("{id}:"))))
            .cloned()
            .collect::<Vec<_>>();

        for key in &affected_keys {
            if let Some(client) = self.language_server_clients.remove(key) {
                if let Ok(client) = client.lock() {
                    for (view_id, state) in client.open_document_states() {
                        previous_documents.insert(view_id, state);
                    }
                }
                if let Err(err) = shutdown_language_server(&client) {
                    error!("failed to shutdown language server during config update: {}", err);
                }
            }
        }

        // Recompute every tracked view. A changed server id does not necessarily match the
        // buffer's detected language id, especially for extension-routed legacy configs.
        let affected_views = self
            .view_info
            .iter()
            .map(|(view_id, info)| (*view_id, info.clone()))
            .collect::<Vec<_>>();

        for (view_id, info) in affected_views {
            let path = info.path.clone();
            let matches = self.language_matches_for_path(&path, Some(&info.language_id));
            let routes = self.routes_for_path(&path, &matches);
            if routes.is_empty() {
                self.view_info.remove(&view_id);
                continue;
            }

            if let Some(view_info) = self.view_info.get_mut(&view_id) {
                view_info.routes = routes.clone();
            }

            if let Some(state) = previous_documents.remove(&view_id) {
                for route in routes {
                    let entry = groups.entry(route.ls_identifier.clone()).or_insert_with(|| {
                        ClientRestartGroup {
                            server_id: route.server_id.clone(),
                            workspace_root: route.workspace_root.clone(),
                            documents: Vec::new(),
                        }
                    });
                    entry.documents.push((view_id, state.clone()));
                }
            }
        }

        groups
    }

    pub(super) fn restart_groups(&mut self, restart_groups: BTreeMap<String, ClientRestartGroup>) {
        for (identifier, group) in restart_groups {
            if self.language_server_clients.contains_key(&identifier) {
                continue;
            }

            let Some(config) = self.config.language_config.get(&group.server_id) else {
                continue;
            };
            let Some(core) = self.core.clone() else {
                continue;
            };

            let client = match start_new_server(
                config.start_command.clone(),
                config.start_arguments.clone(),
                &group.server_id,
                core,
                self.result_queue.clone(),
                ServerStartOptions {
                    file_extensions: config.extensions.clone(),
                    env_overrides: config.env.clone(),
                    initialization_options: config.initialization_options.clone(),
                },
            ) {
                Ok(client) => client,
                Err(err) => {
                    Self::log_spawn_failure(&group.server_id, &config.start_command, &err);
                    continue;
                }
            };

            let initialized = if let Ok(mut server) = client.lock() {
                for (view_id, state) in group.documents {
                    server.opened_documents.insert(view_id, state);
                }
                if !server.is_initialized && !server.initialization_pending {
                    let workspace_root = group.workspace_root.clone();
                    server
                        .send_initialize(workspace_root, move |ls_client, result| {
                            ls_client.initialization_pending = false;
                            match result {
                                Ok(result) => match serde_json::from_value::<InitializeResult>(result) {
                                    Ok(init_result) => {
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
                                Err(err) => ls_client.record_server_failure(format!(
                                    "initialize request failed: {err:?}"
                                )),
                            }
                        })
                        .is_ok()
                } else {
                    true
                }
            } else {
                false
            };

            if initialized {
                self.language_server_clients.insert(identifier, client);
            }
        }
    }

    pub(super) fn workspace_root_for_path(&self, path: &Path, server_id: &str) -> Option<Uri> {
        let config = self.config.language_config.get(server_id)?;

        config
            .workspace_identifier
            .as_ref()
            .and_then(|identifier| get_workspace_root_uri(identifier, path).ok())
    }

    pub(super) fn configured_server_matches_for_language(
        &self,
        language_id: &str,
    ) -> Option<Vec<LanguageMatch>> {
        self.config.language_servers.get(language_id).map(|server_ids| {
            server_ids
                .iter()
                .filter_map(|server_id| {
                    if self.config.language_config.contains_key(server_id) {
                        Some(LanguageMatch::Enabled(server_id.clone()))
                    } else if self.config.disabled_language_config.contains_key(server_id) {
                        Some(LanguageMatch::Disabled(server_id.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        })
    }

    pub(super) fn language_match_for_path(&self, path: &Path) -> Option<LanguageMatch> {
        if let Some(filename) = path.file_name().and_then(|name| name.to_str()) {
            for (lang, config) in &self.config.language_config {
                if config.filenames.iter().any(|candidate| candidate == filename) {
                    return Some(LanguageMatch::Enabled(lang.clone()));
                }
            }
            for (lang, config) in &self.config.disabled_language_config {
                if config.filenames.iter().any(|candidate| candidate == filename) {
                    return Some(LanguageMatch::Disabled(lang.clone()));
                }
            }
        }

        path.extension().and_then(|extension| extension.to_str()).and_then(|extension_str| {
            for (lang, config) in &self.config.language_config {
                if config.extensions.iter().any(|candidate| candidate == extension_str) {
                    return Some(LanguageMatch::Enabled(lang.clone()));
                }
            }
            for (lang, config) in &self.config.disabled_language_config {
                if config.extensions.iter().any(|candidate| candidate == extension_str) {
                    return Some(LanguageMatch::Disabled(lang.clone()));
                }
            }
            None
        })
    }

    pub(super) fn normalized_view_language_id(&self, language_id: impl AsRef<str>) -> String {
        language_id.as_ref().trim().to_ascii_lowercase()
    }

    pub(super) fn language_matches_for_path(
        &self,
        path: &Path,
        language_id: Option<&str>,
    ) -> Vec<LanguageMatch> {
        if let Some(language_id) = language_id {
            let normalized_id = self.normalized_view_language_id(language_id);
            if let Some(matches) = self.configured_server_matches_for_language(&normalized_id) {
                return matches;
            }
        }

        self.language_match_for_path(path).into_iter().collect()
    }

    pub(super) fn routes_for_path(
        &self,
        path: &Path,
        matches: &[LanguageMatch],
    ) -> Vec<ViewServerRoute> {
        matches
            .iter()
            .filter_map(|language_match| match language_match {
                LanguageMatch::Enabled(server_id) => {
                    let workspace_root = self.workspace_root_for_path(path, server_id);
                    self.language_server_key(server_id, &workspace_root).map(|ls_identifier| {
                        ViewServerRoute {
                            server_id: server_id.clone(),
                            ls_identifier,
                            workspace_root,
                        }
                    })
                }
                LanguageMatch::Disabled(_) => None,
            })
            .collect()
    }

    pub(super) fn add_status_item(&self, view_id: ViewId, key: &str, value: &str) {
        if let Some(core) = &self.core {
            core.add_status_item(view_id, key, value, "left");
        }
    }

    pub(super) fn remove_status_item(&self, view_id: ViewId, key: &str) {
        if let Some(core) = &self.core {
            core.remove_status_item(view_id, key);
        }
    }

    pub(super) fn add_spawn_failure_status(
        &self,
        view_id: ViewId,
        language_id: &str,
        command: &str,
    ) -> String {
        let (key, value) = Self::spawn_failure_status(language_id, command);
        self.add_status_item(view_id, &key, &value);
        key
    }

    pub(super) fn spawn_failure_status(language_id: &str, command: &str) -> (String, String) {
        let hint = Self::spawn_failure_hint(command);
        (
            format!("lsp:{language_id}:status"),
            format!("lsp:{language_id}:spawn failed: {command}; hint: {hint}"),
        )
    }

    pub(super) fn spawn_failure_hint(command: &str) -> String {
        match command {
            "bash-language-server" => String::from("npm install -g bash-language-server"),
            "clangd" => String::from("install clangd and ensure it is in PATH"),
            "elixir-ls" => String::from("install elixir-ls and ensure it is in PATH"),
            "gleam" => String::from("install gleam and ensure it is in PATH"),
            "gopls" => String::from("go install golang.org/x/tools/gopls@latest"),
            "intelephense" => String::from("npm install -g intelephense"),
            "jdtls" => String::from("install jdtls and ensure it is in PATH"),
            "jedi-language-server" => String::from("pip install jedi-language-server"),
            "kotlin-language-server" => {
                String::from("install kotlin-language-server and ensure it is in PATH")
            }
            "lua-language-server" => {
                String::from("install lua-language-server and ensure it is in PATH")
            }
            "marksman" => String::from("install marksman and ensure it is in PATH"),
            "metals" => String::from("install metals and ensure it is in PATH"),
            "nil" => String::from("install nil and ensure it is in PATH"),
            "ocamllsp" => String::from("install ocaml-lsp-server and ensure `ocamllsp` is in PATH"),
            "ruby-lsp" => String::from("install ruby-lsp and ensure it is in PATH"),
            "rust-analyzer" => String::from("install rust-analyzer and ensure it is in PATH"),
            "sourcekit-lsp" => String::from("install sourcekit-lsp and ensure it is in PATH"),
            "svelteserver" => String::from("npm install -g svelte-language-server"),
            "taplo" => String::from("install taplo-cli and ensure `taplo` is in PATH"),
            "typescript-language-server" => {
                String::from("npm install -g typescript typescript-language-server")
            }
            "vscode-css-language-server"
            | "vscode-html-language-server"
            | "vscode-json-languageserver" => {
                String::from("npm install -g vscode-langservers-extracted")
            }
            "vue-language-server" => String::from("npm install -g @vue/language-server"),
            "yaml-language-server" => String::from("npm install -g yaml-language-server"),
            "zls" => String::from("install zls and ensure it is in PATH"),
            _ => format!("install {command} and ensure it is in PATH"),
        }
    }

    pub(super) fn add_disabled_status(&self, view_id: ViewId, language_id: &str) -> String {
        let key = format!("lsp:{language_id}:disabled");
        self.add_status_item(view_id, &key, &key);
        key
    }

    pub(super) fn add_unsupported_workspace_status(
        &self,
        view_id: ViewId,
        language_id: &str,
    ) -> String {
        let key = format!("lsp:{language_id}:unsupported-workspace");
        self.add_status_item(view_id, &key, &key);
        key
    }

    pub(super) fn route_status_key(view_id: ViewId) -> String {
        format!("lsp:{}:routes", view_id)
    }

    pub(super) fn route_status_value(language_id: &str, routes: &[ViewServerRoute]) -> String {
        let mut server_ids = routes.iter().map(|route| route.server_id.as_str());
        let Some(primary) = server_ids.next() else {
            return format!("lsp:{language_id}: inactive");
        };
        let secondary = server_ids.collect::<Vec<_>>();
        if secondary.is_empty() {
            format!("lsp:{language_id}: {primary}")
        } else {
            format!("lsp:{language_id}: primary {primary}; secondary {}", secondary.join(", "))
        }
    }

    pub(super) fn update_route_status(
        &mut self,
        view_id: ViewId,
        language_id: &str,
        routes: &[ViewServerRoute],
    ) {
        if let Some(key) = self.route_views.remove(&view_id) {
            self.remove_status_item(view_id, &key);
        }
        if routes.is_empty() {
            return;
        }
        let key = Self::route_status_key(view_id);
        let value = Self::route_status_value(language_id, routes);
        self.add_status_item(view_id, &key, &value);
        self.route_views.insert(view_id, key);
    }

    pub(super) fn log_spawn_failure(language_id: &str, command: &str, err: &Error) {
        let hint = Self::spawn_failure_hint(command);
        error!("lsp:{language_id}: spawn failed for command {command}: {err}; hint: {hint}");
    }
}
