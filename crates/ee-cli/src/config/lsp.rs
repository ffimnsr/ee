//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use super::raw::{LspServerToml, LspToml};
use super::runtime_languages::normalize_runtime_language_id;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use xi_core_lib::config::Table as XiConfigTable;
use xi_core_lib::runtime_loader::RuntimeLanguageConfig;
use xi_lsp_lib::{
    Config as PluginLspConfig, DisabledLanguageConfig as PluginDisabledLanguageConfig,
    LanguageConfig as PluginLanguageConfig,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspSettings {
    pub servers: BTreeMap<String, LspServerSettings>,
    pub disabled_servers: BTreeMap<String, DisabledLspServerSettings>,
    pub language_servers: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisabledLspServerSettings {
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self::from_plugin_config(PluginLspConfig::bundled())
    }
}

impl LspSettings {
    fn from_plugin_config(config: PluginLspConfig) -> Self {
        Self {
            servers: config
                .language_config
                .into_iter()
                .map(|(id, server)| {
                    (
                        id,
                        LspServerSettings {
                            language_name: server.language_name,
                            command: server.start_command,
                            args: server.start_arguments,
                            extensions: server.extensions,
                            filenames: server.filenames,
                            supports_single_file: server.supports_single_file,
                            workspace_identifier: server.workspace_identifier,
                            env: server.env,
                            initialization_options: server.initialization_options,
                        },
                    )
                })
                .collect(),
            disabled_servers: config
                .disabled_language_config
                .into_iter()
                .map(|(id, server)| {
                    (
                        id,
                        DisabledLspServerSettings {
                            extensions: server.extensions,
                            filenames: server.filenames,
                        },
                    )
                })
                .collect(),
            language_servers: config.language_servers.into_iter().collect(),
        }
    }

    fn to_plugin_config(&self) -> PluginLspConfig {
        PluginLspConfig {
            language_config: self
                .servers
                .iter()
                .map(|(id, server)| {
                    (
                        id.clone(),
                        PluginLanguageConfig {
                            language_name: server.language_name.clone(),
                            start_command: server.command.clone(),
                            start_arguments: server.args.clone(),
                            extensions: server.extensions.clone(),
                            filenames: server.filenames.clone(),
                            supports_single_file: server.supports_single_file,
                            workspace_identifier: server.workspace_identifier.clone(),
                            env: server.env.clone(),
                            initialization_options: server.initialization_options.clone(),
                        },
                    )
                })
                .collect(),
            disabled_language_config: self
                .disabled_servers
                .iter()
                .map(|(id, server)| {
                    (
                        id.clone(),
                        PluginDisabledLanguageConfig {
                            extensions: server.extensions.clone(),
                            filenames: server.filenames.clone(),
                        },
                    )
                })
                .collect(),
            language_servers: self
                .language_servers
                .iter()
                .map(|(language_id, server_ids)| (language_id.clone(), server_ids.clone()))
                .collect(),
        }
    }

    pub(super) fn to_config_table(&self) -> XiConfigTable {
        match serde_json::to_value(self.to_plugin_config()) {
            Ok(Value::Object(table)) => table,
            Ok(_) | Err(_) => XiConfigTable::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspServerSettings {
    pub language_name: String,
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
    pub supports_single_file: bool,
    pub workspace_identifier: Option<String>,
    pub env: BTreeMap<String, String>,
    pub initialization_options: Option<Value>,
}

#[derive(Debug, Clone)]
pub(super) struct LspSettingsBuilder {
    servers: BTreeMap<String, LspServerSettingsBuilder>,
    language_servers: BTreeMap<String, Vec<String>>,
    disabled_languages: BTreeSet<String>,
}

impl Default for LspSettingsBuilder {
    fn default() -> Self {
        Self::from_settings(&LspSettings::default())
    }
}

impl LspSettingsBuilder {
    fn from_settings(settings: &LspSettings) -> Self {
        Self {
            servers: settings
                .servers
                .iter()
                .map(|(id, server)| {
                    (
                        id.clone(),
                        LspServerSettingsBuilder {
                            language_name: Some(server.language_name.clone()),
                            command: Some(server.command.clone()),
                            args: Some(server.args.clone()),
                            extensions: Some(server.extensions.clone()),
                            filenames: Some(server.filenames.clone()),
                            supports_single_file: Some(server.supports_single_file),
                            workspace_identifier: server.workspace_identifier.clone(),
                            enabled: Some(true),
                            env: server.env.clone(),
                            initialization_options: server.initialization_options.clone(),
                        },
                    )
                })
                .collect(),
            language_servers: settings.language_servers.clone(),
            disabled_languages: BTreeSet::new(),
        }
    }

    pub(super) fn merge_toml(&mut self, patch: &LspToml) {
        for (id, server_patch) in &patch.servers {
            let server = self.servers.entry(id.clone()).or_default();
            if let Some(language_name) = &server_patch.language_name {
                server.language_name = Some(language_name.clone());
            }
            if let Some(command) = &server_patch.command {
                server.command = Some(command.clone());
            }
            if let Some(args) = &server_patch.args {
                server.args = Some(args.clone());
            }
            if let Some(extensions) = &server_patch.extensions {
                server.extensions = Some(extensions.clone());
            }
            if let Some(filenames) = &server_patch.filenames {
                server.filenames = Some(filenames.clone());
            }
            if let Some(supports_single_file) = server_patch.supports_single_file {
                server.supports_single_file = Some(supports_single_file);
            }
            if let Some(workspace_identifier) = &server_patch.workspace_identifier {
                server.workspace_identifier = Some(workspace_identifier.clone());
            }
            if let Some(enabled) = server_patch.enabled {
                server.enabled = Some(enabled);
            }
            for (key, value) in &server_patch.env {
                server.env.insert(key.clone(), value.clone());
            }
            if let Some(initialization_options) = &server_patch.initialization_options {
                server.initialization_options = Some(initialization_options.clone());
            }
        }
    }

    pub(super) fn merge_language_toml(&mut self, language_id: &str, patch: &RuntimeLanguageConfig) {
        let normalized_id = normalize_runtime_language_id(language_id);

        if let Some(enabled) = patch.enabled {
            if enabled {
                self.disabled_languages.remove(&normalized_id);
            } else {
                self.disabled_languages.insert(normalized_id.clone());
            }
        }

        if let Some(server_ids) = &patch.lsp {
            self.language_servers
                .insert(normalized_id, normalize_lsp_server_ids(language_id, server_ids));
        }
    }

    pub(super) fn finalize(self) -> LspSettings {
        let mut servers = BTreeMap::new();
        let mut disabled_servers = BTreeMap::new();
        let referenced_server_ids = self
            .language_servers
            .values()
            .flat_map(|server_ids| server_ids.iter().cloned())
            .collect::<BTreeSet<_>>();

        for (id, server) in self.servers {
            if server.enabled == Some(false) {
                let extensions = server
                    .extensions
                    .as_ref()
                    .map(|extensions| normalize_lsp_extensions(&id, extensions))
                    .unwrap_or_default();
                let filenames = server
                    .filenames
                    .as_ref()
                    .map(|filenames| normalize_lsp_filenames(&id, filenames))
                    .unwrap_or_default();
                disabled_servers.insert(id, DisabledLspServerSettings { extensions, filenames });
                continue;
            }

            let missing = [
                ("language_name", server.language_name.is_none()),
                ("command", server.command.is_none()),
            ]
            .into_iter()
            .filter_map(|(field, missing)| missing.then_some(field))
            .collect::<Vec<_>>();

            if !missing.is_empty() {
                eprintln!(
                    "ee: warning: invalid lsp server config for {}: missing {}",
                    id,
                    missing.join(", ")
                );
                continue;
            }

            let extensions = server
                .extensions
                .as_ref()
                .map(|extensions| normalize_lsp_extensions(&id, extensions))
                .unwrap_or_default();
            let filenames = server
                .filenames
                .as_ref()
                .map(|filenames| normalize_lsp_filenames(&id, filenames))
                .unwrap_or_default();

            if extensions.is_empty() && filenames.is_empty() && !referenced_server_ids.contains(&id)
            {
                eprintln!(
                    "ee: warning: lsp server config for {} has no routing metadata; add [languages.<id>].lsp, extensions, or filenames",
                    id
                );
            }

            servers.insert(
                id,
                LspServerSettings {
                    language_name: server.language_name.expect("validated above"),
                    command: server.command.expect("validated above"),
                    args: server.args.unwrap_or_default(),
                    extensions,
                    filenames,
                    supports_single_file: server.supports_single_file.unwrap_or(true),
                    workspace_identifier: server.workspace_identifier,
                    env: server.env,
                    initialization_options: server.initialization_options,
                },
            );
        }
        resolve_lsp_extension_ownership(
            &mut servers,
            &mut disabled_servers,
            &referenced_server_ids,
        );
        let mut language_servers = self.language_servers;
        for language_id in self.disabled_languages {
            language_servers.insert(language_id, Vec::new());
        }

        for (language_id, server_ids) in &mut language_servers {
            server_ids.retain(|server_id| {
                if servers.contains_key(server_id) || disabled_servers.contains_key(server_id) {
                    true
                } else {
                    eprintln!(
                        "ee: warning: language {} references unknown lsp server {}",
                        language_id, server_id
                    );
                    false
                }
            });
        }

        LspSettings { servers, disabled_servers, language_servers }
    }
}

pub(super) fn normalize_lsp_server_ids(language_id: &str, server_ids: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();

    for server_id in server_ids {
        let server_id = server_id.trim();
        if server_id.is_empty() {
            eprintln!(
                "ee: warning: invalid runtime language config for {}: empty lsp server id ignored",
                language_id
            );
            continue;
        }
        if seen.insert(server_id.to_string()) {
            normalized.push(server_id.to_string());
        }
    }

    normalized
}

fn normalize_lsp_extensions(server_id: &str, extensions: &[String]) -> Vec<String> {
    extensions
        .iter()
        .filter_map(|extension| {
            let normalized = extension.trim_start_matches('.').to_owned();
            if normalized.is_empty() {
                eprintln!(
                    "ee: warning: invalid lsp server config for {}: empty extension ignored",
                    server_id
                );
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

fn normalize_lsp_filenames(server_id: &str, filenames: &[String]) -> Vec<String> {
    filenames
        .iter()
        .filter_map(|filename| {
            let normalized = filename.trim().to_owned();
            if normalized.is_empty() {
                eprintln!(
                    "ee: warning: invalid lsp server config for {}: empty filename ignored",
                    server_id
                );
                None
            } else if normalized.contains(std::path::MAIN_SEPARATOR) || normalized.contains('/') || normalized.contains('\\') {
                eprintln!(
                    "ee: warning: invalid lsp server config for {}: filename {} must not include path separators",
                    server_id, normalized
                );
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

fn resolve_lsp_extension_ownership(
    servers: &mut BTreeMap<String, LspServerSettings>,
    disabled_servers: &mut BTreeMap<String, DisabledLspServerSettings>,
    referenced_server_ids: &BTreeSet<String>,
) {
    let mut owner_by_extension = BTreeMap::<String, String>::new();
    let mut ids = servers.keys().chain(disabled_servers.keys()).cloned().collect::<Vec<_>>();
    ids.sort();
    ids.dedup();

    for id in ids {
        let extensions = servers
            .get(&id)
            .map(|server| &server.extensions)
            .or_else(|| disabled_servers.get(&id).map(|server| &server.extensions));
        if let Some(extensions) = extensions {
            for extension in extensions {
                if let Some(previous) = owner_by_extension.insert(extension.clone(), id.clone()) {
                    eprintln!(
                        "ee: warning: lsp extension .{} moved from {} to {}",
                        extension, previous, id
                    );
                }
            }
        }
    }

    let mut owner_by_filename = BTreeMap::<String, String>::new();
    let mut ids = servers.keys().chain(disabled_servers.keys()).cloned().collect::<Vec<_>>();
    ids.sort();
    ids.dedup();

    for id in ids {
        let filenames = servers
            .get(&id)
            .map(|server| &server.filenames)
            .or_else(|| disabled_servers.get(&id).map(|server| &server.filenames));
        if let Some(filenames) = filenames {
            for filename in filenames {
                if let Some(previous) = owner_by_filename.insert(filename.clone(), id.clone()) {
                    eprintln!(
                        "ee: warning: lsp filename {} moved from {} to {}",
                        filename, previous, id
                    );
                }
            }
        }
    }

    for (id, server) in servers.iter_mut() {
        server.extensions.retain(|extension| owner_by_extension.get(extension) == Some(id));
        server.filenames.retain(|filename| owner_by_filename.get(filename) == Some(id));
    }
    servers.retain(|id, server| {
        !server.extensions.is_empty()
            || !server.filenames.is_empty()
            || referenced_server_ids.contains(id)
    });

    for (id, server) in disabled_servers.iter_mut() {
        server.extensions.retain(|extension| owner_by_extension.get(extension) == Some(id));
        server.filenames.retain(|filename| owner_by_filename.get(filename) == Some(id));
    }
    disabled_servers.retain(|id, server| {
        !server.extensions.is_empty()
            || !server.filenames.is_empty()
            || referenced_server_ids.contains(id)
    });
}

#[derive(Debug, Clone, Default)]
struct LspServerSettingsBuilder {
    language_name: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    filenames: Option<Vec<String>>,
    supports_single_file: Option<bool>,
    workspace_identifier: Option<String>,
    enabled: Option<bool>,
    env: BTreeMap<String, String>,
    initialization_options: Option<Value>,
}

pub(super) fn lsp_settings_to_toml(lsp: &LspSettings) -> Option<LspToml> {
    if lsp.servers.is_empty() {
        return None;
    }
    Some(LspToml {
        servers: lsp
            .servers
            .iter()
            .map(|(id, server)| {
                (
                    id.clone(),
                    LspServerToml {
                        language_name: Some(server.language_name.clone()),
                        command: Some(server.command.clone()),
                        args: Some(server.args.clone()),
                        extensions: Some(server.extensions.clone()),
                        filenames: Some(server.filenames.clone()),
                        supports_single_file: Some(server.supports_single_file),
                        workspace_identifier: server.workspace_identifier.clone(),
                        enabled: Some(true),
                        env: server.env.clone(),
                        initialization_options: server.initialization_options.clone(),
                    },
                )
            })
            .collect(),
    })
}
