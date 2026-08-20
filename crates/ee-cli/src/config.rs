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

use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{env, fs};

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use globset::GlobBuilder;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xi_core_lib::config::Table as XiConfigTable;
use xi_core_lib::runtime_loader::{
    RuntimeLanguageConfig, RuntimeLanguageOverrides, configure_default_runtime_loader_overrides,
    validate_runtime_language_overrides,
};
use xi_lsp_lib::{
    Config as PluginLspConfig, DisabledLanguageConfig as PluginDisabledLanguageConfig,
    LanguageConfig as PluginLanguageConfig,
};

use crate::keymap::{self, KeymapOperation, KeymapSettings, SequenceBinding};

const SYSTEM_CONFIG_PATH: &str = "/etc/ee/config.toml";
pub(crate) const LSP_PLUGIN_NAME: &str = "xi-lsp-plugin";

const CONFIG_TEMPLATE: &str = r#"# ee configuration
#
# Remove `# ` from settings you want to override. All settings below are
# optional; omitted settings inherit from lower-priority config layers.
#
# root = false
# indent_style = "spaces"
# indent_size = 4
# tab_width = 4
# end_of_line = "lf"
# charset = "utf-8"
# trim_trailing_whitespace = false
# insert_final_newline = false
# auto_indent = true
# smart_indent = true
# number_style = "absolute"
# color_column = 80
# show_visible_whitespace = false
# scroll_offset = 5
# wrap_lines = false
# sign_column = true
# cursor_line = false
# statusline_format = "default"
#
# [lsp.servers.example]
# language_name = "Example"
# command = "example-language-server"
# args = ["--stdio"]
# extensions = ["example"]
# filenames = ["Examplefile"]
# supports_single_file = true
# workspace_identifier = "Example.toml"
# enabled = true
# env = { EXAMPLE_LOG = "info" }
# initialization_options = { diagnostics = { enable = true } }
#
# [languages.example]
# name = "Example"
# file_types = ["example"]
# aliases = ["example-lang"]
# globs = ["*.example"]
# shebangs = ["example"]
# scope = "source.example"
# query_language = "example"
# content_regex = "^example"
# first_line_regex = "^#!.*example"
# injection_regex = "example"
# match_priority = 0
# supported_query_kinds = ["highlights", "injections", "locals", "tags", "textobjects", "indents", "folds", "rainbows"]
# lsp = ["example"]
# enabled = true
#
# [languages.example.grammar]
# library = "tree-sitter-example"
# symbol = "tree_sitter_example"
#
# [languages.example.grammar.source.crate]
# name = "tree-sitter-example"
# version = "1.0.0"
#
# # Instead of `source.crate`, use exactly one Git source reference:
# # [languages.example.grammar.source.git]
# # url = "https://github.com/example/tree-sitter-example"
# # rev = "0123456789abcdef0123456789abcdef01234567"
# # branch = "main"
# # tag = "v1.0.0"
#
# [keymap]
# inherit_defaults = true
# sequence_timeout_ms = 500
#
# [[keymap.unbind]]
# mode = "normal"
# key = "q"
# prefix = "space"
#
# [[keymap.bindings]]
# mode = "insert"
# key = "C-s"
# prefix = ""
# action = "save"
#
# [[keymap.sequence_bindings]]
# mode = "normal"
# keys = ["g", "g"]
# action = "goto_start"
# description = "Go to start of file"
#
# [agents]
# enabled = false
# default_agent = "assistant"
#
# [agents.servers.assistant]
# command = "agent-command"
# args = ["--stdio"]
# env = { API_KEY = "secret://agent-api-key" }
# cwd = "/path/to/workspace"
#
# [mcp.proxy]
# enabled = false
#
# [mcp.servers.example]
# transport = "stdio"
# command = "mcp-server"
# args = ["--stdio"]
# env = { EXAMPLE_LOG = "info" }
# cwd = "/path/to/workspace"
#
# # For a remote MCP server, replace stdio fields with:
# # [mcp.servers.remote]
# # transport = "streamable_http"
# # url = "https://mcp.example.com/mcp"
# # headers = { Authorization = "Bearer secret://mcp-token" }
# # timeout_ms = 30000
"#;

// ── Public settings ───────────────────────────────────────────────────────────

/// Line-number display style in the gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum NumberStyle {
    /// Always show the absolute 1-based line number.
    #[default]
    Absolute,
    /// Show distance from cursor; cursor line shows `0`.
    Relative,
    /// Show absolute number on cursor line, relative distance on all others.
    RelativeAbsolute,
}

/// Statusline format variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StatuslineFormat {
    /// Full statusline: mode, file, modified flag, buffer indicator, position.
    #[default]
    Default,
    /// Minimal: mode + filename + position only (no buffer counter).
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum IndentStyle {
    #[default]
    Spaces,
    Tabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EndOfLine {
    #[default]
    Lf,
    CrLf,
    Cr,
}

/// Fully resolved editor settings with all defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EditorSettings {
    pub indent_style: IndentStyle,
    /// Number of spaces per indent level (or soft-tab width when `indent_style = spaces`).
    pub indent_size: usize,
    /// Visual width of a hard-tab character.
    pub tab_width: usize,
    pub end_of_line: EndOfLine,
    /// Expected charset, e.g. `"utf-8"`, `"utf-8-bom"`, `"latin1"`.
    pub charset: String,
    pub trim_trailing_whitespace: bool,
    pub insert_final_newline: bool,
    pub auto_indent: bool,
    pub smart_indent: bool,
    // ── Display options ───────────────────────────────────────────────────
    /// How line numbers are displayed in the gutter.
    pub number_style: NumberStyle,
    /// Highlight the column at this position (e.g. 80) when `Some`.  Disabled when `None`.
    pub color_column: Option<usize>,
    /// Show whitespace characters (spaces as `·`, tabs as `→`) in the buffer.
    pub show_visible_whitespace: bool,
    /// Minimum number of screen rows to keep between cursor and the top/bottom edge.
    pub scroll_offset: usize,
    /// Soft-wrap long lines instead of truncating at the viewport right edge.
    pub wrap_lines: bool,
    /// Show a sign column to the left of line numbers (used for fold and diagnostic markers).
    pub sign_column: bool,
    /// Highlight the row containing the cursor with a distinct background.
    pub cursor_line: bool,
    /// Statusline layout variant.
    pub statusline_format: StatuslineFormat,
    /// Effective LSP settings resolved from bundled defaults and ee TOML layers.
    pub lsp: LspSettings,
    /// Resolved keymap overrides layered from `.ee.toml` files.
    pub keymap: KeymapSettings,
    /// Effective agents-mode settings resolved from ee TOML layers.
    pub agents: AgentsSettings,
    /// Effective shared MCP server configuration resolved from ee TOML layers.
    pub mcp: McpSettings,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            indent_style: IndentStyle::Spaces,
            indent_size: 4,
            tab_width: 4,
            end_of_line: EndOfLine::Lf,
            charset: "utf-8".to_owned(),
            trim_trailing_whitespace: false,
            insert_final_newline: false,
            auto_indent: true,
            smart_indent: true,
            number_style: NumberStyle::Absolute,
            color_column: None,
            show_visible_whitespace: false,
            scroll_offset: 5,
            wrap_lines: false,
            sign_column: true,
            cursor_line: false,
            statusline_format: StatuslineFormat::Default,
            lsp: LspSettings::default(),
            keymap: KeymapSettings::default(),
            agents: AgentsSettings::default(),
            mcp: McpSettings::default(),
        }
    }
}

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

    fn to_config_table(&self) -> XiConfigTable {
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

// ── Agents-mode settings ───────────────────────────────────────────────────────

/// Default request timeout for Streamable HTTP MCP servers, in milliseconds.
const DEFAULT_MCP_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Resolved agents-mode settings.  Agents mode is disabled by default at
/// runtime; `enabled` only becomes `true` through an explicit config layer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AgentsSettings {
    pub enabled: bool,
    pub default_agent: Option<String>,
    pub servers: BTreeMap<String, AgentServerSettings>,
}

/// One agent environment value with its config-layer provenance (phase 5).
///
/// An exact `secret://<name>` value is kept as raw text through parsing,
/// merging, schema generation, and display; it is resolved from the
/// host-bound secrets store only at agent launch, and only when the source
/// layer is user-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentEnvValue {
    /// The config layer this value came from.
    pub layer: ConfigLayerKind,
    /// Raw text exactly as written in config: a literal or an exact
    /// `secret://<name>` reference. Never resolved at merge time.
    pub raw: String,
}

/// Resolved ACP agent subprocess definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentServerSettings {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, AgentEnvValue>,
    pub cwd: Option<PathBuf>,
}

/// Resolved shared MCP server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct McpSettings {
    pub servers: BTreeMap<String, McpServerSettings>,
    /// Optional ee MCP proxy mode (off by default).
    pub proxy: McpProxySettings,
}

/// Resolved ee MCP proxy runtime settings.
///
/// The proxy exposes `ee_*` tools (file read/write, terminal create,
/// diagnostics) as a local MCP server that ACP agents can connect to; every
/// tool call routes through the same approval and bridge paths as direct ACP
/// client methods.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct McpProxySettings {
    /// Whether the proxy is started when agents mode is enabled.
    pub enabled: bool,
}

/// Resolved MCP server transport.  Only stdio and Streamable HTTP are
/// supported; HTTP+SSE and other transports are not implemented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpServerSettings {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    StreamableHttp {
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone)]
struct LspSettingsBuilder {
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

    fn merge_toml(&mut self, patch: &LspToml) {
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

    fn merge_language_toml(&mut self, language_id: &str, patch: &RuntimeLanguageConfig) {
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

    fn finalize(self) -> LspSettings {
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

fn normalize_lsp_server_ids(language_id: &str, server_ids: &[String]) -> Vec<String> {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RuntimeLanguageSettings {
    pub user_overrides: RuntimeLanguageOverrides,
    pub workspace_overrides: RuntimeLanguageOverrides,
}

#[derive(Debug, Clone, Default)]
struct RuntimeLanguageSettingsBuilder {
    user_overrides: RuntimeLanguageOverrides,
    workspace_overrides: RuntimeLanguageOverrides,
}

impl RuntimeLanguageSettingsBuilder {
    fn merge_toml(&mut self, patch: &EeToml, kind: ConfigLayerKind) {
        let target = match kind {
            ConfigLayerKind::Ancestor => &mut self.workspace_overrides,
            ConfigLayerKind::System | ConfigLayerKind::UserXdg | ConfigLayerKind::UserLegacy => {
                &mut self.user_overrides
            }
        };

        for (language_id, language_patch) in &patch.languages {
            let normalized_id = normalize_runtime_language_id(language_id);
            merge_runtime_language_patch(
                target.entry(normalized_id).or_default(),
                language_patch,
                language_id,
            );
        }
    }

    fn finalize(self) -> RuntimeLanguageSettings {
        RuntimeLanguageSettings {
            user_overrides: self.user_overrides,
            workspace_overrides: self.workspace_overrides,
        }
    }
}

fn normalize_runtime_language_id(language_id: &str) -> String {
    language_id.trim().to_ascii_lowercase()
}

fn normalize_runtime_file_types(language_id: &str, file_types: &[String]) -> Vec<String> {
    file_types
        .iter()
        .filter_map(|file_type| {
            let normalized = file_type.trim().trim_start_matches('.').to_string();
            if normalized.is_empty() {
                eprintln!(
                    "ee: warning: invalid runtime language config for {}: empty file type ignored",
                    language_id
                );
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

fn merge_runtime_language_patch(
    target: &mut RuntimeLanguageConfig,
    patch: &RuntimeLanguageConfig,
    language_id: &str,
) {
    if let Some(enabled) = patch.enabled {
        target.enabled = Some(enabled);
    }
    if let Some(lsp) = &patch.lsp {
        target.lsp = Some(normalize_lsp_server_ids(language_id, lsp));
    }
    if let Some(name) = &patch.name {
        target.name = Some(name.clone());
    }
    if let Some(query_language) = &patch.query_language {
        target.query_language = Some(query_language.clone());
    }
    if let Some(scope) = &patch.scope {
        target.scope = Some(scope.clone());
    }
    if let Some(content_regex) = &patch.content_regex {
        target.content_regex = Some(content_regex.clone());
    }
    if let Some(first_line_regex) = &patch.first_line_regex {
        target.first_line_regex = Some(first_line_regex.clone());
    }
    if let Some(injection_regex) = &patch.injection_regex {
        target.injection_regex = Some(injection_regex.clone());
    }
    if let Some(aliases) = &patch.aliases {
        target.aliases = Some(aliases.clone());
    }
    if let Some(file_types) = &patch.file_types {
        target.file_types = Some(normalize_runtime_file_types(language_id, file_types));
    }
    if let Some(globs) = &patch.globs {
        target.globs = Some(globs.clone());
    }
    if let Some(shebangs) = &patch.shebangs {
        target.shebangs = Some(shebangs.clone());
    }
    if let Some(supported_query_kinds) = &patch.supported_query_kinds {
        target.supported_query_kinds = Some(supported_query_kinds.clone());
    }
    if let Some(match_priority) = patch.match_priority {
        target.match_priority = Some(match_priority);
    }
    if let Some(grammar_patch) = &patch.grammar {
        let grammar = target.grammar.get_or_insert_with(Default::default);
        if let Some(library) = &grammar_patch.library {
            grammar.library = Some(library.clone());
        }
        if let Some(symbol) = &grammar_patch.symbol {
            grammar.symbol = Some(symbol.clone());
        }
        if let Some(source) = &grammar_patch.source {
            grammar.source = Some(source.clone());
        }
    }
}

impl EditorSettings {
    pub(crate) fn to_xi_config_table(&self) -> XiConfigTable {
        let mut table = XiConfigTable::new();
        table.insert("line_ending".into(), Value::String(self.end_of_line.as_xi_string().into()));
        table.insert("tab_size".into(), Value::from(self.indent_size.max(1)));
        table.insert(
            "translate_tabs_to_spaces".into(),
            Value::Bool(matches!(self.indent_style, IndentStyle::Spaces)),
        );
        table.insert("use_tab_stops".into(), Value::Bool(true));
        table.insert("font_face".into(), Value::String(String::from("Noto Mono")));
        table.insert("font_size".into(), Value::from(14.0_f32));
        table.insert("auto_indent".into(), Value::Bool(self.auto_indent));
        table.insert("smart_indent".into(), Value::Bool(self.smart_indent));
        table.insert("scroll_past_end".into(), Value::Bool(false));
        table.insert("wrap_width".into(), Value::from(0));
        table.insert("word_wrap".into(), Value::Bool(self.wrap_lines));
        table.insert("autodetect_whitespace".into(), Value::Bool(true));
        table.insert(
            "surrounding_pairs".into(),
            Value::Array(vec![
                Value::Array(vec![Value::String("\"".into()), Value::String("\"".into())]),
                Value::Array(vec![Value::String("'".into()), Value::String("'".into())]),
                Value::Array(vec![Value::String("{".into()), Value::String("}".into())]),
                Value::Array(vec![Value::String("[".into()), Value::String("]".into())]),
            ]),
        );
        table.insert("save_with_newline".into(), Value::Bool(self.insert_final_newline));
        table
    }
}

impl EndOfLine {
    fn as_xi_string(self) -> &'static str {
        match self {
            EndOfLine::Lf => "\n",
            EndOfLine::CrLf => "\r\n",
            EndOfLine::Cr => "\r",
        }
    }
}

pub(crate) fn xi_config_tables_for_file(
    file_path: Option<&Path>,
) -> (EditorSettings, XiConfigTable, XiConfigTable) {
    let general = load_config(None);
    let effective = load_config(file_path);
    let general_table = general.to_xi_config_table();
    let effective_table = effective.to_xi_config_table();
    let override_table = diff_xi_config_tables(&general_table, &effective_table);
    (effective, general_table, override_table)
}

pub(crate) fn lsp_config_table_for_file(file_path: Option<&Path>) -> XiConfigTable {
    load_config(file_path).lsp.to_config_table()
}

fn diff_xi_config_tables(base: &XiConfigTable, updated: &XiConfigTable) -> XiConfigTable {
    updated
        .iter()
        .filter_map(|(key, value)| match base.get(key) {
            Some(existing) if existing == value => None,
            _ => Some((key.clone(), value.clone())),
        })
        .collect()
}

// ── .ee.toml raw shape ────────────────────────────────────────────────────────

/// Raw `.ee.toml` shape; all fields optional so partial files work.
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EeToml {
    pub root: Option<bool>,
    /// `"spaces"` or `"tabs"` (aliases: `"space"`, `"tab"`).
    pub indent_style: Option<String>,
    /// Number of spaces per indent level.
    pub indent_size: Option<usize>,
    /// Visual width of a hard-tab character.
    pub tab_width: Option<usize>,
    /// `"lf"`, `"crlf"`, or `"cr"`.
    pub end_of_line: Option<String>,
    /// Expected charset, e.g. `"utf-8"`.
    pub charset: Option<String>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
    pub auto_indent: Option<bool>,
    pub smart_indent: Option<bool>,
    // ── Display options ───────────────────────────────────────────────────
    /// `"absolute"`, `"relative"`, or `"relative_absolute"`.
    pub number_style: Option<String>,
    /// Column position for the color column guide (e.g. `80`).  Omit to disable.
    pub color_column: Option<usize>,
    pub show_visible_whitespace: Option<bool>,
    /// Minimum rows between cursor and screen top/bottom edge.
    pub scroll_offset: Option<usize>,
    pub wrap_lines: Option<bool>,
    pub sign_column: Option<bool>,
    pub cursor_line: Option<bool>,
    /// `"default"` or `"minimal"`.
    pub statusline_format: Option<String>,
    pub lsp: Option<LspToml>,
    #[serde(default)]
    pub languages: BTreeMap<String, RuntimeLanguageConfig>,
    pub keymap: Option<KeymapToml>,
    pub agents: Option<AgentsToml>,
    pub mcp: Option<McpToml>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LspToml {
    #[serde(default)]
    pub servers: BTreeMap<String, LspServerToml>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LspServerToml {
    pub language_name: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub filenames: Option<Vec<String>>,
    pub supports_single_file: Option<bool>,
    pub workspace_identifier: Option<String>,
    pub enabled: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub initialization_options: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentsToml {
    /// Runtime switch for agents mode.  Defaults to `false`; agents mode is
    /// disabled unless a config layer sets this to `true`.
    pub enabled: Option<bool>,
    /// Agent server id used when the user starts a session without choosing
    /// an explicit agent.
    pub default_agent: Option<String>,
    #[serde(default)]
    pub servers: BTreeMap<String, AgentServerToml>,
}

/// Raw `[agents.servers.<id>]` definition.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentServerToml {
    /// Executable invoked to start the ACP agent subprocess.
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    /// Environment variables for the agent subprocess. An exact
    /// `secret://<name>` value is resolved from the host-bound encrypted
    /// secrets store (`ee do secrets`) only when the agent launches, and only
    /// when the value comes from a user config layer (XDG or legacy user
    /// config); system and workspace config layers cannot reference secrets.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory for the agent subprocess; inherits `ee` when unset.
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpToml {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerToml>,
    /// ee MCP proxy mode (off by default).
    #[serde(default)]
    pub proxy: Option<McpProxyToml>,
}

/// Raw `[mcp.proxy]` settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpProxyToml {
    /// Whether the ee MCP proxy starts with agents mode.
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerToml {
    /// Transport discriminator: `"stdio"` or `"streamable_http"`.
    pub transport: McpTransportToml,
    // ── stdio transport fields ─────────────────────────────────────────────
    /// Executable invoked to start the MCP server.
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory for the MCP server; inherits `ee` when unset.
    pub cwd: Option<PathBuf>,
    // ── streamable_http transport fields ───────────────────────────────────
    /// Absolute `http(s)` endpoint URL of the MCP server.
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Request timeout in milliseconds; defaults to 30 000.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum McpTransportToml {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigLayerKind {
    System,
    UserXdg,
    UserLegacy,
    Ancestor,
}

impl ConfigLayerKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::UserXdg => "user xdg",
            Self::UserLegacy => "user legacy fallback",
            Self::Ancestor => "ancestor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigScope {
    Global,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigLayer {
    pub kind: ConfigLayerKind,
    pub path: PathBuf,
    pub root: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigLayerReport {
    pub kind: ConfigLayerKind,
    pub path: PathBuf,
    pub exists: bool,
    pub loaded: bool,
    pub root: Option<bool>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigSearchReport {
    pub anchor: PathBuf,
    pub layers: Vec<ConfigLayerReport>,
    pub editorconfig_applies: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigEnvironment {
    cwd: PathBuf,
    home_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    system_config_path: PathBuf,
}

impl ConfigEnvironment {
    fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            home_dir: dirs::home_dir(),
            config_dir: process_config_dir(),
            system_config_path: PathBuf::from(SYSTEM_CONFIG_PATH),
        }
    }

    fn anchor_dir(&self, file_path: Option<&Path>) -> PathBuf {
        match file_path {
            Some(path) if path.is_dir() => path.to_path_buf(),
            Some(path) => path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.cwd.clone()),
            None => self.cwd.clone(),
        }
    }

    pub(crate) fn xdg_user_config_path(&self) -> Option<PathBuf> {
        self.config_dir.as_ref().map(|dir| dir.join("ee").join("config.toml"))
    }

    pub(crate) fn legacy_user_config_path(&self) -> Option<PathBuf> {
        self.home_dir.as_ref().map(|home| home.join(".ee.toml"))
    }

    fn workspace_candidate_paths(&self, file_path: Option<&Path>) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        let mut dir = self.anchor_dir(file_path);
        loop {
            candidates.push(dir.join(".ee.toml"));
            if !dir.pop() {
                break;
            }
        }
        candidates.reverse();
        candidates
    }
}

pub(crate) fn xi_core_config_dir() -> Option<PathBuf> {
    process_config_dir().map(|dir| dir.join("ee"))
}

pub(crate) fn xi_core_client_extras_dir() -> Option<PathBuf> {
    let bundled_plugins_dir = bundled_runtime_root().join("plugins");
    fs::metadata(&bundled_plugins_dir).ok().filter(|meta| meta.is_dir())?;
    Some(bundled_plugins_dir)
}

fn process_config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
}

fn bundled_runtime_root() -> PathBuf {
    let env_override = env::var_os("EE_RUNTIME_DIR").map(PathBuf::from);
    let exe_path = env::current_exe().ok();
    let fallback_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_bundled_runtime_root(env_override.as_deref(), exe_path.as_deref(), &fallback_dir)
}

fn resolve_bundled_runtime_root(
    env_override: Option<&Path>,
    exe_path: Option<&Path>,
    fallback_dir: &Path,
) -> PathBuf {
    if let Some(path) = env_override.filter(|path| !path.as_os_str().is_empty()) {
        return path.to_path_buf();
    }
    if let Some(exe_path) = exe_path {
        if cfg!(windows) {
            if let Some(parent) = exe_path.parent() {
                return parent.join("runtime");
            }
        } else if let Some(bin_dir) = exe_path.parent()
            && bin_dir.file_name().is_some_and(|name| name == "bin")
            && let Some(prefix_dir) = bin_dir.parent()
        {
            return prefix_dir.join("share").join("ee");
        }
    }
    fallback_dir.join("runtime")
}

#[derive(Debug, Clone)]
struct ConfigProbe {
    exists: bool,
    root: Option<bool>,
}

#[derive(Debug, Clone)]
struct ConfigDiscovery {
    layers: Vec<ConfigLayer>,
    root_stop_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeymapToml {
    pub inherit_defaults: Option<bool>,
    pub sequence_timeout_ms: Option<u64>,
    #[serde(default)]
    pub unbind: Vec<KeyBindingTargetToml>,
    #[serde(default)]
    pub bindings: Vec<KeyBindingEntryToml>,
    #[serde(default)]
    pub sequence_bindings: Vec<KeySequenceBindingToml>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyBindingTargetToml {
    pub mode: String,
    pub key: String,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyBindingEntryToml {
    pub mode: String,
    pub key: String,
    pub prefix: Option<String>,
    pub action: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeySequenceBindingToml {
    pub mode: String,
    pub keys: Vec<String>,
    pub action: String,
    pub description: Option<String>,
}

// ── Merging ───────────────────────────────────────────────────────────────────

impl EditorSettings {
    /// Apply any set fields from `patch`, leaving unset fields unchanged.
    fn merge_toml(&mut self, patch: &EeToml, kind: ConfigLayerKind) {
        if let Some(s) = &patch.indent_style {
            match s.to_lowercase().as_str() {
                "spaces" | "space" => self.indent_style = IndentStyle::Spaces,
                "tabs" | "tab" => self.indent_style = IndentStyle::Tabs,
                _ => {}
            }
        }
        if let Some(v) = patch.indent_size {
            self.indent_size = v;
        }
        if let Some(v) = patch.tab_width {
            self.tab_width = v;
        }
        if let Some(s) = &patch.end_of_line {
            match s.to_lowercase().as_str() {
                "lf" => self.end_of_line = EndOfLine::Lf,
                "crlf" => self.end_of_line = EndOfLine::CrLf,
                "cr" => self.end_of_line = EndOfLine::Cr,
                _ => {}
            }
        }
        if let Some(v) = &patch.charset {
            self.charset = v.clone();
        }
        if let Some(v) = patch.trim_trailing_whitespace {
            self.trim_trailing_whitespace = v;
        }
        if let Some(v) = patch.insert_final_newline {
            self.insert_final_newline = v;
        }
        if let Some(v) = patch.auto_indent {
            self.auto_indent = v;
        }
        if let Some(v) = patch.smart_indent {
            self.smart_indent = v;
        }
        if let Some(s) = &patch.number_style {
            match s.to_lowercase().as_str() {
                "absolute" => self.number_style = NumberStyle::Absolute,
                "relative" => self.number_style = NumberStyle::Relative,
                "relative_absolute" | "relativenumber" => {
                    self.number_style = NumberStyle::RelativeAbsolute;
                }
                _ => {}
            }
        }
        if let Some(v) = patch.color_column {
            self.color_column = if v == 0 { None } else { Some(v) };
        }
        if let Some(v) = patch.show_visible_whitespace {
            self.show_visible_whitespace = v;
        }
        if let Some(v) = patch.scroll_offset {
            self.scroll_offset = v;
        }
        if let Some(v) = patch.wrap_lines {
            self.wrap_lines = v;
        }
        if let Some(v) = patch.sign_column {
            self.sign_column = v;
        }
        if let Some(v) = patch.cursor_line {
            self.cursor_line = v;
        }
        if let Some(s) = &patch.statusline_format {
            match s.to_lowercase().as_str() {
                "default" => self.statusline_format = StatuslineFormat::Default,
                "minimal" => self.statusline_format = StatuslineFormat::Minimal,
                _ => {}
            }
        }
        if let Some(keymap) = &patch.keymap {
            self.merge_keymap_toml(keymap);
        }
        if let Some(agents) = &patch.agents {
            self.merge_agents_toml(agents, kind);
        }
        if let Some(mcp) = &patch.mcp {
            self.merge_mcp_toml(mcp);
        }
    }

    fn merge_keymap_toml(&mut self, patch: &KeymapToml) {
        if let Some(inherit_defaults) = patch.inherit_defaults {
            self.keymap.inherit_defaults = inherit_defaults;
        }
        if let Some(sequence_timeout_ms) = patch.sequence_timeout_ms {
            self.keymap.sequence_timeout_ms = sequence_timeout_ms;
        }

        for entry in &patch.unbind {
            match keymap::parse_binding_spec(&entry.mode, &entry.key, entry.prefix.as_deref()) {
                Ok(binding) => self.keymap.operations.push(KeymapOperation::Unbind(binding)),
                Err(err) => {
                    eprintln!(
                        "ee: warning: invalid keymap unbind ({}, {}): {err}",
                        entry.mode, entry.key
                    );
                }
            }
        }

        for entry in &patch.bindings {
            let binding = match keymap::parse_binding_spec(
                &entry.mode,
                &entry.key,
                entry.prefix.as_deref(),
            ) {
                Ok(binding) => binding,
                Err(err) => {
                    eprintln!(
                        "ee: warning: invalid keymap binding ({}, {}): {err}",
                        entry.mode, entry.key
                    );
                    continue;
                }
            };
            let action = match keymap::parse_action_spec(&entry.action) {
                Ok(action) => action,
                Err(err) => {
                    eprintln!(
                        "ee: warning: invalid keymap action ({}, {}): {err}",
                        entry.mode, entry.action
                    );
                    continue;
                }
            };
            self.keymap.operations.push(KeymapOperation::Bind { binding, action });
        }

        for entry in &patch.sequence_bindings {
            let mode = match keymap::parse_binding_mode(&entry.mode) {
                Ok(mode) => mode,
                Err(err) => {
                    eprintln!("ee: warning: invalid keymap sequence mode ({}): {err}", entry.mode);
                    continue;
                }
            };
            let sequence = match keymap::parse_key_sequence_spec(&entry.keys) {
                Ok(sequence) => sequence,
                Err(err) => {
                    eprintln!("ee: warning: invalid keymap sequence ({:?}): {err}", entry.keys);
                    continue;
                }
            };
            let action = match keymap::parse_action_spec(&entry.action) {
                Ok(action) => action,
                Err(err) => {
                    eprintln!(
                        "ee: warning: invalid keymap sequence action ({}, {}): {err}",
                        entry.mode, entry.action
                    );
                    continue;
                }
            };
            let description = entry.description.clone().unwrap_or_else(|| entry.action.clone());
            self.keymap.sequence_bindings.push(SequenceBinding {
                mode,
                sequence,
                action,
                description,
            });
        }
    }

    fn merge_agents_toml(&mut self, patch: &AgentsToml, kind: ConfigLayerKind) {
        if let Some(enabled) = patch.enabled {
            self.agents.enabled = enabled;
        }
        if let Some(default_agent) = &patch.default_agent {
            self.agents.default_agent = Some(default_agent.clone());
        }
        for (id, server) in &patch.servers {
            match resolve_agent_server(id, server, kind) {
                Ok(resolved) => {
                    self.agents.servers.insert(id.clone(), resolved);
                }
                Err(err) => eprintln!("ee: warning: invalid agents server `{id}`: {err}"),
            }
        }
    }

    fn merge_mcp_toml(&mut self, patch: &McpToml) {
        if let Some(proxy) = &patch.proxy
            && let Some(enabled) = proxy.enabled
        {
            self.mcp.proxy.enabled = enabled;
        }
        for (id, server) in &patch.servers {
            match resolve_mcp_server(id, server) {
                Ok(resolved) => {
                    self.mcp.servers.insert(id.clone(), resolved);
                }
                Err(err) => eprintln!("ee: warning: invalid mcp server `{id}`: {err}"),
            }
        }
    }
}

fn resolve_agent_server(
    id: &str,
    server: &AgentServerToml,
    kind: ConfigLayerKind,
) -> Result<AgentServerSettings, String> {
    if id.trim().is_empty() {
        return Err(String::from("agent server id must not be empty"));
    }
    let command = server.command.as_deref().unwrap_or_default().trim();
    if command.is_empty() {
        return Err(String::from("agent server command must not be empty"));
    }
    let mut env = BTreeMap::new();
    for (key, value) in &server.env {
        if crate::secrets::is_secret_reference_text(value) {
            match crate::secrets::SecretReference::parse(value) {
                Ok(_reference) => {
                    if !matches!(kind, ConfigLayerKind::UserXdg | ConfigLayerKind::UserLegacy) {
                        return Err(format!(
                            "secret references are only allowed in user config layers, \
                             but agents.servers.{id}.env.{key} comes from {} config",
                            kind.label()
                        ));
                    }
                    env.insert(key.clone(), AgentEnvValue { layer: kind, raw: value.clone() });
                }
                Err(err) => {
                    return Err(format!(
                        "invalid secret reference in agents.servers.{id}.env.{key}: {err}"
                    ));
                }
            }
        } else {
            env.insert(key.clone(), AgentEnvValue { layer: kind, raw: value.clone() });
        }
    }
    Ok(AgentServerSettings {
        command: command.to_owned(),
        args: server.args.clone().unwrap_or_default(),
        env,
        cwd: server.cwd.clone(),
    })
}

fn resolve_mcp_server(id: &str, server: &McpServerToml) -> Result<McpServerSettings, String> {
    if id.trim().is_empty() {
        return Err(String::from("mcp server id must not be empty"));
    }
    match server.transport {
        McpTransportToml::Stdio => {
            let command = server.command.as_deref().unwrap_or_default().trim();
            if command.is_empty() {
                return Err(String::from("mcp stdio server command must not be empty"));
            }
            Ok(McpServerSettings::Stdio {
                command: command.to_owned(),
                args: server.args.clone().unwrap_or_default(),
                env: server.env.clone(),
                cwd: server.cwd.clone(),
            })
        }
        McpTransportToml::StreamableHttp => {
            let raw_url = server.url.as_deref().unwrap_or_default();
            let parsed = url::Url::parse(raw_url)
                .map_err(|err| format!("invalid mcp url `{raw_url}`: {err}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!("invalid mcp url `{raw_url}`: scheme must be http or https"));
            }
            Ok(McpServerSettings::StreamableHttp {
                url: parsed.to_string(),
                headers: server.headers.clone(),
                timeout_ms: server.timeout_ms.unwrap_or(DEFAULT_MCP_HTTP_TIMEOUT_MS),
            })
        }
    }
}

// ── Loading helpers ───────────────────────────────────────────────────────────

/// Parse one `.ee.toml` file if it exists and is readable.
fn parse_ee_toml(path: &Path) -> Option<EeToml> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return None,
    };
    match toml::from_str::<EeToml>(&text) {
        Ok(patch) => Some(patch),
        Err(e) => {
            // Surface parse errors so users can fix them, but don't abort.
            eprintln!("ee: warning: failed to parse {}: {}", path.display(), e);
            None
        }
    }
}

fn probe_config_file(path: &Path) -> ConfigProbe {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<EeToml>(&text) {
            Ok(config) => ConfigProbe { exists: true, root: config.root },
            Err(_) => ConfigProbe { exists: true, root: None },
        },
        Err(err) if err.kind() == ErrorKind::NotFound => ConfigProbe { exists: false, root: None },
        Err(_) => ConfigProbe { exists: true, root: None },
    }
}

fn discover_config_layers_with_env(
    env: &ConfigEnvironment,
    file_path: Option<&Path>,
) -> ConfigDiscovery {
    let workspace_candidates = env.workspace_candidate_paths(file_path);
    let mut high_to_low_layers = Vec::new();
    let mut root_stop_path = None;

    for path in workspace_candidates.iter().rev() {
        let probe = probe_config_file(path);
        if !probe.exists {
            continue;
        }
        high_to_low_layers.push(ConfigLayer {
            kind: ConfigLayerKind::Ancestor,
            path: path.clone(),
            root: probe.root,
        });
        if probe.root == Some(true) {
            root_stop_path = Some(path.clone());
            break;
        }
    }

    if root_stop_path.is_none() {
        let xdg_path = env.xdg_user_config_path();
        let xdg_exists = xdg_path.as_ref().is_some_and(|path| probe_config_file(path).exists);

        if let Some(path) = xdg_path
            && xdg_exists
        {
            let probe = probe_config_file(&path);
            high_to_low_layers.push(ConfigLayer {
                kind: ConfigLayerKind::UserXdg,
                path: path.clone(),
                root: probe.root,
            });
            if probe.root == Some(true) {
                root_stop_path = Some(path);
            }
        } else if let Some(legacy_path) = env.legacy_user_config_path() {
            let legacy_probe = probe_config_file(&legacy_path);
            if legacy_probe.exists {
                high_to_low_layers.push(ConfigLayer {
                    kind: ConfigLayerKind::UserLegacy,
                    path: legacy_path.clone(),
                    root: legacy_probe.root,
                });
                if legacy_probe.root == Some(true) {
                    root_stop_path = Some(legacy_path);
                }
            }
        }
    }

    if root_stop_path.is_none() && probe_config_file(&env.system_config_path).exists {
        high_to_low_layers.push(ConfigLayer {
            kind: ConfigLayerKind::System,
            path: env.system_config_path.clone(),
            root: Some(true),
        });
    }

    high_to_low_layers.reverse();
    ConfigDiscovery { layers: high_to_low_layers, root_stop_path }
}

fn load_config_with_env(file_path: Option<&Path>, env: &ConfigEnvironment) -> EditorSettings {
    let mut settings = EditorSettings::default();
    let mut lsp = LspSettingsBuilder::default();

    for layer in discover_config_layers_with_env(env, file_path).layers {
        if let Some(patch) = parse_ee_toml(&layer.path) {
            settings.merge_toml(&patch, layer.kind);
            if let Some(lsp_patch) = &patch.lsp {
                lsp.merge_toml(lsp_patch);
            }
            for (language_id, language_patch) in &patch.languages {
                lsp.merge_language_toml(language_id, language_patch);
            }
        }
    }

    if let Some(file_path) = file_path {
        apply_editorconfig(&mut settings, file_path);
    }

    settings.lsp = lsp.finalize();

    settings
}

fn runtime_languages_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> RuntimeLanguageSettings {
    let mut runtime_languages = RuntimeLanguageSettingsBuilder::default();

    for layer in discover_config_layers_with_env(env, file_path).layers {
        if let Some(patch) = parse_ee_toml(&layer.path) {
            runtime_languages.merge_toml(&patch, layer.kind);
        }
    }

    runtime_languages.finalize()
}

fn config_path_for_scope_with_env(
    scope: ConfigScope,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    match scope {
        ConfigScope::Global => env
            .xdg_user_config_path()
            .ok_or_else(|| String::from("cannot resolve global config path")),
        ConfigScope::Local => Ok(env.cwd.join(".ee.toml")),
    }
}

fn runtime_languages_to_toml(
    runtime_languages: RuntimeLanguageSettings,
) -> BTreeMap<String, RuntimeLanguageConfig> {
    let mut merged = runtime_languages.user_overrides;
    for (language_id, patch) in runtime_languages.workspace_overrides {
        merge_runtime_language_patch(
            merged.entry(language_id.clone()).or_default(),
            &patch,
            &language_id,
        );
    }
    merged
}

fn lsp_settings_to_toml(lsp: &LspSettings) -> Option<LspToml> {
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

fn agents_settings_to_toml(agents: &AgentsSettings) -> Option<AgentsToml> {
    Some(AgentsToml {
        enabled: Some(agents.enabled),
        default_agent: agents.default_agent.clone(),
        servers: agents
            .servers
            .iter()
            .map(|(id, server)| {
                (
                    id.clone(),
                    AgentServerToml {
                        command: Some(server.command.clone()),
                        args: Some(server.args.clone()),
                        // Display paths keep the raw text: literals and
                        // `secret://` references exactly as configured.
                        env: server
                            .env
                            .iter()
                            .map(|(key, value)| (key.clone(), value.raw.clone()))
                            .collect(),
                        cwd: server.cwd.clone(),
                    },
                )
            })
            .collect(),
    })
}

fn mcp_settings_to_toml(mcp: &McpSettings) -> Option<McpToml> {
    if mcp.servers.is_empty() && !mcp.proxy.enabled {
        return None;
    }
    Some(McpToml {
        servers: mcp
            .servers
            .iter()
            .map(|(id, server)| {
                let toml = match server {
                    McpServerSettings::Stdio { command, args, env, cwd } => McpServerToml {
                        transport: McpTransportToml::Stdio,
                        command: Some(command.clone()),
                        args: Some(args.clone()),
                        env: env.clone(),
                        cwd: cwd.clone(),
                        url: None,
                        headers: BTreeMap::new(),
                        timeout_ms: None,
                    },
                    McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
                        McpServerToml {
                            transport: McpTransportToml::StreamableHttp,
                            command: None,
                            args: None,
                            env: BTreeMap::new(),
                            cwd: None,
                            url: Some(url.clone()),
                            headers: headers.clone(),
                            timeout_ms: Some(*timeout_ms),
                        }
                    }
                };
                (id.clone(), toml)
            })
            .collect(),
        proxy: mcp.proxy.enabled.then_some(McpProxyToml { enabled: Some(true) }),
    })
}

fn keymap_settings_to_toml(keymap: &crate::keymap::KeymapSettings) -> Option<KeymapToml> {
    let mut unbind = Vec::new();
    let mut bindings = Vec::new();
    for operation in &keymap.operations {
        match operation {
            crate::keymap::KeymapOperation::Unbind(binding) => unbind.push(KeyBindingTargetToml {
                mode: crate::keymap::format_binding_mode(binding.mode).to_string(),
                key: crate::keymap::format_key_press(crate::keymap::KeyPress {
                    key: binding.key,
                    modifiers: binding.modifiers,
                }),
                prefix: binding.prefix.map(|prefix| prefix.to_string()),
            }),
            crate::keymap::KeymapOperation::Bind { binding, action } => {
                bindings.push(KeyBindingEntryToml {
                    mode: crate::keymap::format_binding_mode(binding.mode).to_string(),
                    key: crate::keymap::format_key_press(crate::keymap::KeyPress {
                        key: binding.key,
                        modifiers: binding.modifiers,
                    }),
                    prefix: binding.prefix.map(|prefix| prefix.to_string()),
                    action: crate::keymap::format_action_spec(action),
                })
            }
        }
    }
    let sequence_bindings = keymap
        .sequence_bindings
        .iter()
        .map(|binding| KeySequenceBindingToml {
            mode: crate::keymap::format_binding_mode(binding.mode).to_string(),
            keys: binding.sequence.iter().copied().map(crate::keymap::format_key_press).collect(),
            action: crate::keymap::format_action_spec(&binding.action),
            description: Some(binding.description.clone()),
        })
        .collect::<Vec<_>>();
    let keymap = KeymapToml {
        inherit_defaults: Some(keymap.inherit_defaults),
        sequence_timeout_ms: Some(keymap.sequence_timeout_ms),
        unbind,
        bindings,
        sequence_bindings,
    };
    Some(keymap)
}

pub(crate) fn resolved_config_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> EeToml {
    let settings = load_config_with_env(file_path, env);
    let runtime_languages = runtime_languages_to_toml(runtime_languages_with_env(file_path, env));
    EeToml {
        root: None,
        indent_style: Some(match settings.indent_style {
            IndentStyle::Spaces => String::from("spaces"),
            IndentStyle::Tabs => String::from("tabs"),
        }),
        indent_size: Some(settings.indent_size),
        tab_width: Some(settings.tab_width),
        end_of_line: Some(match settings.end_of_line {
            EndOfLine::Lf => String::from("lf"),
            EndOfLine::CrLf => String::from("crlf"),
            EndOfLine::Cr => String::from("cr"),
        }),
        charset: Some(settings.charset),
        trim_trailing_whitespace: Some(settings.trim_trailing_whitespace),
        insert_final_newline: Some(settings.insert_final_newline),
        auto_indent: Some(settings.auto_indent),
        smart_indent: Some(settings.smart_indent),
        number_style: Some(match settings.number_style {
            NumberStyle::Absolute => String::from("absolute"),
            NumberStyle::Relative => String::from("relative"),
            NumberStyle::RelativeAbsolute => String::from("relative_absolute"),
        }),
        color_column: settings.color_column,
        show_visible_whitespace: Some(settings.show_visible_whitespace),
        scroll_offset: Some(settings.scroll_offset),
        wrap_lines: Some(settings.wrap_lines),
        sign_column: Some(settings.sign_column),
        cursor_line: Some(settings.cursor_line),
        statusline_format: Some(match settings.statusline_format {
            StatuslineFormat::Default => String::from("default"),
            StatuslineFormat::Minimal => String::from("minimal"),
        }),
        lsp: lsp_settings_to_toml(&settings.lsp),
        languages: runtime_languages,
        keymap: keymap_settings_to_toml(&settings.keymap),
        agents: agents_settings_to_toml(&settings.agents),
        mcp: mcp_settings_to_toml(&settings.mcp),
    }
}

pub(crate) fn merged_config_document(file_path: Option<&Path>) -> Result<String, String> {
    let document = resolved_config_with_env(file_path, &ConfigEnvironment::from_process());
    toml::to_string_pretty(&document)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|err| format!("cannot render merged config: {err}"))
}

fn parse_config_document(path: &Path) -> Result<toml::Value, String> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str::<toml::Value>(&contents)
            .map_err(|err| format!("Config parse error in {}: {err}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::map::Map::new()))
        }
        Err(err) => Err(format!("Cannot read {}: {err}", path.display())),
    }
}

fn get_value_at_path<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    if key.trim().is_empty() {
        return Some(value);
    }
    let mut current = value;
    for part in key.split('.') {
        let table = current.as_table()?;
        current = table.get(part)?;
    }
    Some(current)
}

fn ensure_table(
    value: &mut toml::Value,
) -> Result<&mut toml::map::Map<String, toml::Value>, String> {
    match value {
        toml::Value::Table(table) => Ok(table),
        _ => Err(String::from("config root must be table")),
    }
}

fn set_value_at_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<(), String> {
    let mut parts = key.split('.').peekable();
    if parts.peek().is_none() {
        return Err(String::from("config key must not be empty"));
    }
    let mut current = ensure_table(root)?;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            current.insert(part.to_string(), value);
            return Ok(());
        }
        let entry = current
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        current = match entry {
            toml::Value::Table(table) => table,
            _ => return Err(format!("config key `{part}` already exists and is not table")),
        };
    }
    Ok(())
}

fn validate_config_contents(path: &Path, contents: &str) -> Result<(), String> {
    let parsed = toml::from_str::<EeToml>(contents)
        .map_err(|err| format!("Config parse error in {}: {err}", path.display()))?;

    validate_agents_mcp_config(&parsed)
        .map_err(|err| format!("Config validation error in {}: {err}", path.display()))?;

    if parsed.languages.is_empty() {
        return Ok(());
    }

    let is_workspace_layer = path.file_name().is_some_and(|name| name == ".ee.toml");
    let mut user_overrides = RuntimeLanguageOverrides::new();
    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    if is_workspace_layer {
        workspace_overrides = parsed.languages;
    } else {
        user_overrides = parsed.languages;
    }

    validate_runtime_language_overrides(&user_overrides, &workspace_overrides, is_workspace_layer)
        .map_err(|err| format!("Config validation error in {}: {err}", path.display()))
}

/// Rejects invalid agents/MCP server definitions and ids that collide across
/// the `agents.servers` and `mcp.servers` namespaces.
fn validate_agents_mcp_config(parsed: &EeToml) -> Result<(), String> {
    let mut effective_ids = BTreeSet::new();
    if let Some(agents) = &parsed.agents {
        for (id, server) in &agents.servers {
            // Validation checks shape and reference grammar only; layer
            // provenance is enforced during the merge, not on standalone
            // files (the file's eventual layer is unknown here).
            resolve_agent_server(id, server, ConfigLayerKind::UserXdg)
                .map_err(|err| format!("agents server `{id}`: {err}"))?;
            effective_ids.insert(id.clone());
        }
    }
    if let Some(mcp) = &parsed.mcp {
        for (id, server) in &mcp.servers {
            resolve_mcp_server(id, server).map_err(|err| format!("mcp server `{id}`: {err}"))?;
            if !effective_ids.insert(id.clone()) {
                return Err(format!(
                    "duplicate effective server id `{id}` in agents.servers and mcp.servers"
                ));
            }
        }
    }
    Ok(())
}

fn get_config_value_with_env(
    scope: ConfigScope,
    key: &str,
    env: &ConfigEnvironment,
) -> Result<Option<String>, String> {
    let path = config_path_for_scope_with_env(scope, env)?;
    let document = parse_config_document(&path)?;
    get_value_at_path(&document, key)
        .map(ToString::to_string)
        .ok_or_else(|| format!("config key `{key}` not found in {}", path.display()))
        .map(Some)
}

pub(crate) fn get_config_value(scope: ConfigScope, key: &str) -> Result<Option<String>, String> {
    get_config_value_with_env(scope, key, &ConfigEnvironment::from_process())
}

fn parse_set_value(raw: &str) -> toml::Value {
    let wrapped = format!("value = {raw}");
    toml::from_str::<toml::Value>(&wrapped)
        .ok()
        .and_then(|value| value.get("value").cloned())
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

fn set_config_value_with_env(
    scope: ConfigScope,
    key: &str,
    raw_value: &str,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    let path = config_path_for_scope_with_env(scope, env)?;
    let mut document = parse_config_document(&path)?;
    set_value_at_path(&mut document, key, parse_set_value(raw_value))?;
    let text = toml::to_string_pretty(&document)
        .map_err(|err| format!("cannot serialize config {}: {err}", path.display()))?;
    validate_config_contents(&path, &text)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }
    fs::write(&path, text).map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
    Ok(path)
}

pub(crate) fn set_config_value(
    scope: ConfigScope,
    key: &str,
    raw_value: &str,
) -> Result<PathBuf, String> {
    set_config_value_with_env(scope, key, raw_value, &ConfigEnvironment::from_process())
}

fn init_config_with_env(scope: ConfigScope, env: &ConfigEnvironment) -> Result<PathBuf, String> {
    let path = config_path_for_scope_with_env(scope, env)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }

    let mut file =
        fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|err| {
            if err.kind() == ErrorKind::AlreadyExists {
                format!("Config already exists: {}", path.display())
            } else {
                format!("Cannot create {}: {err}", path.display())
            }
        })?;
    file.write_all(CONFIG_TEMPLATE.as_bytes())
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
    Ok(path)
}

pub(crate) fn init_config(scope: ConfigScope) -> Result<PathBuf, String> {
    init_config_with_env(scope, &ConfigEnvironment::from_process())
}

pub(crate) fn configure_runtime_loader_for_file(
    file_path: Option<&Path>,
    workspace_trusted: bool,
) -> Result<(), String> {
    let runtime_languages =
        runtime_languages_with_env(file_path, &ConfigEnvironment::from_process());
    configure_default_runtime_loader_overrides(
        runtime_languages.user_overrides,
        runtime_languages.workspace_overrides,
        workspace_trusted,
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn default_config_layers(file_path: Option<&Path>) -> Vec<ConfigLayer> {
    discover_config_layers_with_env(&ConfigEnvironment::from_process(), file_path).layers
}

pub(crate) fn config_search_report(file_path: Option<&Path>) -> ConfigSearchReport {
    config_search_report_with_env(&ConfigEnvironment::from_process(), file_path)
}

fn config_search_report_with_env(
    env: &ConfigEnvironment,
    file_path: Option<&Path>,
) -> ConfigSearchReport {
    let discovery = discover_config_layers_with_env(env, file_path);
    let workspace_candidates = env.workspace_candidate_paths(file_path);
    let xdg_path = env.xdg_user_config_path();
    let legacy_path = env.legacy_user_config_path();
    let xdg_exists = xdg_path.as_ref().is_some_and(|path| probe_config_file(path).exists);

    let mut layers = Vec::new();

    let system_probe = probe_config_file(&env.system_config_path);
    layers.push(ConfigLayerReport {
        kind: ConfigLayerKind::System,
        path: env.system_config_path.clone(),
        exists: system_probe.exists,
        loaded: discovery.layers.iter().any(|layer| layer.path == env.system_config_path),
        root: Some(true),
        note: if !system_probe.exists {
            Some(String::from("not found"))
        } else if discovery.layers.iter().any(|layer| layer.path == env.system_config_path) {
            Some(String::from("terminal fallback"))
        } else {
            discovery
                .root_stop_path
                .as_ref()
                .map(|path| format!("skipped: root=true at {}", path.display()))
        },
    });

    if let Some(path) = xdg_path {
        let probe = probe_config_file(&path);
        let loaded = discovery.layers.iter().any(|layer| layer.path == path);
        layers.push(ConfigLayerReport {
            kind: ConfigLayerKind::UserXdg,
            path,
            exists: probe.exists,
            loaded,
            root: probe.root,
            note: if !probe.exists {
                Some(String::from("not found"))
            } else if loaded {
                None
            } else {
                discovery
                    .root_stop_path
                    .as_ref()
                    .map(|stop| format!("skipped: root=true at {}", stop.display()))
            },
        });
    }

    if let Some(path) = legacy_path {
        let probe = probe_config_file(&path);
        let loaded = discovery.layers.iter().any(|layer| layer.path == path);
        layers.push(ConfigLayerReport {
            kind: ConfigLayerKind::UserLegacy,
            path,
            exists: probe.exists,
            loaded,
            root: probe.root,
            note: if xdg_exists {
                Some(String::from("skipped: XDG user config takes precedence"))
            } else if !probe.exists {
                Some(String::from("not found"))
            } else if loaded {
                Some(String::from("loaded because XDG user config is missing"))
            } else {
                discovery
                    .root_stop_path
                    .as_ref()
                    .map(|stop| format!("skipped: root=true at {}", stop.display()))
            },
        });
    }

    for path in workspace_candidates {
        let probe = probe_config_file(&path);
        let loaded = discovery.layers.iter().any(|layer| layer.path == path);
        layers.push(ConfigLayerReport {
            kind: ConfigLayerKind::Ancestor,
            path,
            exists: probe.exists,
            loaded,
            root: probe.root,
            note: if !probe.exists {
                Some(String::from("not found"))
            } else if loaded {
                None
            } else {
                discovery
                    .root_stop_path
                    .as_ref()
                    .map(|stop| format!("skipped: root=true at {}", stop.display()))
            },
        });
    }

    ConfigSearchReport {
        anchor: env.anchor_dir(file_path),
        layers,
        editorconfig_applies: file_path.is_some(),
    }
}

pub(crate) fn validate_config_file(path: &Path) -> Result<(), String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|err| format!("Cannot read {}: {err}", path.display()))?;
    validate_config_contents(path, &contents)
}

pub(crate) fn config_schema_json() -> Result<String, String> {
    let schema = serde_json::to_value(schema_for!(EeToml))
        .map_err(|err| format!("Cannot serialize config schema: {err}"))?;
    serde_json::to_string_pretty(&schema)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|err| format!("Cannot format config schema: {err}"))
}

pub(crate) fn write_config_schema(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }
    std::fs::write(path, config_schema_json()?)
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))
}

pub(crate) fn check_config_schema(path: &Path) -> Result<(), String> {
    let expected = config_schema_json()?;
    let actual = std::fs::read_to_string(path)
        .map_err(|err| format!("Cannot read {}: {err}", path.display()))?;
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "Config schema drift detected at {}. Run `cargo run -p ee-cli -- do schema generate`.",
        path.display()
    ))
}

/// Walk up directory tree from `start` looking for `.git` or `.git` file.
/// Returns the directory that contains `.git`, or `None`.
pub(crate) fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ── .editorconfig support ─────────────────────────────────────────────────────

/// Apply the first matching `.editorconfig` found by walking up from `file_path`.
/// Follows the spec: stop at `root = true` or filesystem root.
fn apply_editorconfig(settings: &mut EditorSettings, file_path: &Path) {
    let file_path = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => file_path.to_path_buf(),
    };

    // Collect all .editorconfig files from the file's directory up to the root.
    // Process them from outermost (lowest priority) to innermost (highest priority).
    let mut config_stack: Vec<(PathBuf, bool)> = Vec::new();

    let search_dir = if file_path.is_dir() {
        file_path.clone()
    } else {
        file_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| file_path.clone())
    };

    let mut dir = search_dir.clone();
    loop {
        let ec_path = dir.join(".editorconfig");
        if ec_path.exists() {
            let is_root = is_editorconfig_root(&ec_path);
            config_stack.push((ec_path, is_root));
            if is_root {
                break;
            }
        }
        if !dir.pop() {
            break;
        }
    }

    // Apply outermost first (root .editorconfig), innermost last (closest wins).
    config_stack.reverse();
    for (ec_path, _) in config_stack {
        let text = match std::fs::read_to_string(&ec_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        apply_editorconfig_text(settings, &text, &file_path);
    }
}

/// Returns `true` if the editorconfig file contains `root = true`.
fn is_editorconfig_root(path: &Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // Once we hit the first section, preamble is over.
            break;
        }
        if let Some((k, v)) = parse_ec_kv(line)
            && k == "root"
            && v == "true"
        {
            return true;
        }
    }
    false
}

/// Parse and apply one editorconfig file text for the given target file.
fn apply_editorconfig_text(settings: &mut EditorSettings, text: &str, target: &Path) {
    let mut in_matching_section = false;

    for line in text.lines() {
        let line = line.trim();

        // Skip comments and blanks.
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            let pattern = &line[1..line.len() - 1];
            in_matching_section = ec_section_matches(pattern, target);
            continue;
        }

        if !in_matching_section {
            continue;
        }

        if let Some((key, value)) = parse_ec_kv(line) {
            match key.as_str() {
                "indent_style" => match value.as_str() {
                    "space" | "spaces" => settings.indent_style = IndentStyle::Spaces,
                    "tab" | "tabs" => settings.indent_style = IndentStyle::Tabs,
                    _ => {}
                },
                "indent_size" => {
                    if let Ok(n) = usize::from_str(&value) {
                        settings.indent_size = n;
                    }
                }
                "tab_width" => {
                    if let Ok(n) = usize::from_str(&value) {
                        settings.tab_width = n;
                    }
                }
                "end_of_line" => match value.as_str() {
                    "lf" => settings.end_of_line = EndOfLine::Lf,
                    "crlf" => settings.end_of_line = EndOfLine::CrLf,
                    "cr" => settings.end_of_line = EndOfLine::Cr,
                    _ => {}
                },
                "charset" => settings.charset = value,
                "trim_trailing_whitespace" => {
                    settings.trim_trailing_whitespace = value == "true";
                }
                "insert_final_newline" => {
                    settings.insert_final_newline = value == "true";
                }
                _ => {}
            }
        }
    }
}

/// Parse `key = value` line, returning `(lowercase_key, lowercase_value)`.
fn parse_ec_kv(line: &str) -> Option<(String, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim().to_lowercase();
    let value = line[eq + 1..].trim().to_lowercase();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Returns `true` when the editorconfig section `[pattern]` matches `target`.
///
/// Delegates to globset which natively handles `*`, `**`, `?`, `{a,b}`, and `[...]`.
fn ec_section_matches(pattern: &str, target: &Path) -> bool {
    let file_name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let full = target.to_str().unwrap_or(file_name);
    // Patterns containing `/` match the full path; otherwise match just filename.
    let haystack = if pattern.contains('/') { full } else { file_name };
    glob_match(pattern, haystack)
}

/// Glob match using globset. Supports `*`, `**`, `?`, `{a,b}`, and `[...]`.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    match GlobBuilder::new(pattern).build() {
        Ok(glob) => glob.compile_matcher().is_match(text),
        Err(_) => false,
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Load and merge all config layers for the given open file (if any).
pub(crate) fn load_config(file_path: Option<&Path>) -> EditorSettings {
    #[cfg(test)]
    let _cwd_lock = test_cwd_lock().lock().unwrap();

    load_config_with_env(file_path, &ConfigEnvironment::from_process())
}

// ── Test-only config environment helpers (phase 6 e2e fixtures) ───────────────

/// Builds an isolated layered-config environment under `root`: workspace,
/// home, XDG, and system config paths never touch the developer machine.
#[cfg(test)]
pub(crate) fn test_config_environment(root: &Path) -> ConfigEnvironment {
    ConfigEnvironment {
        cwd: root.join("workspace"),
        home_dir: Some(root.join("home")),
        config_dir: Some(root.join("xdg")),
        system_config_path: root.join("etc").join("ee").join("config.toml"),
    }
}

/// Writes one config layer file inside a test environment.
#[cfg(test)]
pub(crate) fn write_config_layer(env: &ConfigEnvironment, kind: ConfigLayerKind, contents: &str) {
    let path = match kind {
        ConfigLayerKind::System => env.system_config_path.clone(),
        ConfigLayerKind::UserXdg => env.xdg_user_config_path().expect("xdg path in test env"),
        ConfigLayerKind::UserLegacy => {
            env.legacy_user_config_path().expect("legacy path in test env")
        }
        ConfigLayerKind::Ancestor => env.cwd.join(".ee.toml"),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Loads merged settings from an isolated test config environment.
#[cfg(test)]
pub(crate) fn load_config_for_test(env: &ConfigEnvironment) -> EditorSettings {
    load_config_with_env(None, env)
}

#[cfg(test)]
thread_local! {
    static TEST_CWD_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) struct TestCwdLock {
    inner: Mutex<()>,
}

#[cfg(test)]
pub(crate) struct TestCwdGuard {
    _guard: Option<MutexGuard<'static, ()>>,
}

#[cfg(test)]
pub(crate) struct TestEnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl TestCwdLock {
    pub(crate) fn lock(
        &'static self,
    ) -> Result<TestCwdGuard, PoisonError<MutexGuard<'static, ()>>> {
        if TEST_CWD_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            if current == 0 {
                return false;
            }
            depth.set(current + 1);
            true
        }) {
            return Ok(TestCwdGuard { _guard: None });
        }

        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        TEST_CWD_LOCK_DEPTH.with(|depth| depth.set(1));
        Ok(TestCwdGuard { _guard: Some(guard) })
    }
}

#[cfg(test)]
impl Drop for TestCwdGuard {
    fn drop(&mut self) {
        TEST_CWD_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "cwd lock depth underflow");
            depth.set(current.saturating_sub(1));
        });
    }
}

#[cfg(test)]
impl TestEnvVarGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

#[cfg(test)]
impl Drop for TestEnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}

#[cfg(test)]
pub(crate) fn test_cwd_lock() -> &'static TestCwdLock {
    static LOCK: OnceLock<TestCwdLock> = OnceLock::new();
    LOCK.get_or_init(|| TestCwdLock { inner: Mutex::new(()) })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ee_toml_parses_indent_style() {
        let toml = r#"indent_style = "tabs"
indent_size = 2
tab_width = 4
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        let mut s = EditorSettings::default();
        s.merge_toml(&raw, ConfigLayerKind::UserXdg);
        assert_eq!(s.indent_style, IndentStyle::Tabs);
        assert_eq!(s.indent_size, 2);
        assert_eq!(s.tab_width, 4);
    }

    #[test]
    fn ee_toml_defaults_unchanged_when_field_absent() {
        let toml = r#"trim_trailing_whitespace = true"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        let mut s = EditorSettings::default();
        s.merge_toml(&raw, ConfigLayerKind::UserXdg);
        assert_eq!(s.indent_style, IndentStyle::Spaces); // unchanged
        assert!(s.trim_trailing_whitespace);
    }

    #[test]
    fn checked_in_config_schema_is_current() {
        let schema_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ee-config.schema.json");
        check_config_schema(&schema_path).unwrap();
    }

    #[test]
    fn runtime_git_source_schema_requires_exactly_one_pin() {
        let schema = serde_json::to_value(schema_for!(EeToml)).unwrap();
        let git_source = schema
            .get("$defs")
            .and_then(|defs| defs.get("RuntimeGrammarGitSource"))
            .and_then(Value::as_object)
            .unwrap();

        assert_eq!(
            git_source.get("required").and_then(Value::as_array).cloned().unwrap_or_default(),
            vec![Value::String(String::from("url"))]
        );

        let one_of = git_source.get("oneOf").and_then(Value::as_array).unwrap();
        assert_eq!(one_of.len(), 3);
        for pin in ["branch", "tag", "rev"] {
            assert!(one_of.iter().any(|branch| {
                branch
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|required| required == &[Value::String(pin.to_string())])
            }));
        }
    }

    #[test]
    fn editorconfig_star_section_applies() {
        let ec = "[*]\nindent_style = tab\nindent_size = 2\n";
        let target = std::path::Path::new("/foo/bar.rs");
        let mut s = EditorSettings::default();
        apply_editorconfig_text(&mut s, ec, target);
        assert_eq!(s.indent_style, IndentStyle::Tabs);
        assert_eq!(s.indent_size, 2);
    }

    #[test]
    fn editorconfig_extension_section_matches() {
        let ec = "[*.rs]\nindent_size = 2\n[*.toml]\nindent_size = 4\n";
        let target = std::path::Path::new("/foo/main.rs");
        let mut s = EditorSettings::default();
        apply_editorconfig_text(&mut s, ec, target);
        assert_eq!(s.indent_size, 2);
    }

    #[test]
    fn editorconfig_brace_group_matches() {
        let ec = "[*.{rs,toml}]\ninsert_final_newline = true\n";
        let target = std::path::Path::new("/foo/Cargo.toml");
        let mut s = EditorSettings::default();
        apply_editorconfig_text(&mut s, ec, target);
        assert!(s.insert_final_newline);
    }

    #[test]
    fn editorconfig_non_matching_section_skipped() {
        let ec = "[*.py]\nindent_size = 2\n";
        let target = std::path::Path::new("/foo/main.rs");
        let mut s = EditorSettings::default(); // indent_size = 4
        apply_editorconfig_text(&mut s, ec, target);
        assert_eq!(s.indent_size, 4); // unchanged
    }

    #[test]
    fn glob_match_star_basic() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.toml"));
    }

    #[test]
    fn glob_match_double_star() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "a/b/c/lib.rs"));
    }

    #[test]
    fn xi_config_tables_split_global_and_file_overrides() {
        // The process cwd is process-global; lock it while mutating.
        let _cwd_lock = test_cwd_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("main.rs");
        let editorconfig = temp.path().join(".editorconfig");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        std::fs::write(&editorconfig, "[*]\nindent_style = tab\nindent_size = 2\n").unwrap();

        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let (_, general, overrides) = xi_config_tables_for_file(Some(&file));

        std::env::set_current_dir(cwd).unwrap();

        assert_eq!(general.get("tab_size").and_then(Value::as_u64), Some(4));
        assert_eq!(overrides.get("tab_size").and_then(Value::as_u64), Some(2));
        assert_eq!(overrides.get("translate_tabs_to_spaces").and_then(Value::as_bool), Some(false));
    }

    fn layer_paths(layers: &[ConfigLayer]) -> Vec<PathBuf> {
        layers.iter().map(|layer| layer.path.clone()).collect()
    }

    #[test]
    fn xdg_user_config_preferred_over_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(env.home_dir.as_ref().unwrap().join(".ee.toml"), "cursor_line = true\n")
            .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "wrap_lines = true\n",
        )
        .unwrap();

        let layers = discover_config_layers_with_env(&env, None).layers;

        assert_eq!(
            layer_paths(&layers),
            vec![env.config_dir.as_ref().unwrap().join("ee").join("config.toml")]
        );

        let settings = load_config_with_env(None, &env);
        assert!(settings.wrap_lines);
        assert!(!settings.cursor_line);
    }

    #[test]
    fn legacy_user_config_used_when_xdg_missing() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
        std::fs::write(env.home_dir.as_ref().unwrap().join(".ee.toml"), "cursor_line = true\n")
            .unwrap();

        let layers = discover_config_layers_with_env(&env, None).layers;

        assert_eq!(layer_paths(&layers), vec![env.home_dir.as_ref().unwrap().join(".ee.toml")]);

        let settings = load_config_with_env(None, &env);
        assert!(settings.cursor_line);
    }

    #[test]
    fn xi_core_config_dir_prefers_xdg_config_home() {
        let temp = tempfile::tempdir().unwrap();
        let xdg_config_home = temp.path().join("xdg-home");
        let _guard = super::TestEnvVarGuard::set("XDG_CONFIG_HOME", &xdg_config_home);

        assert_eq!(xi_core_config_dir(), Some(xdg_config_home.join("ee")));
    }

    #[test]
    fn bundled_runtime_root_prefers_env_then_release_layouts() {
        let fallback = Path::new("/tmp/runtime-fallback");
        let windows_exe = Path::new("C:/Program Files/ee/ee.exe");

        assert_eq!(
            resolve_bundled_runtime_root(
                Some(Path::new("/custom/runtime")),
                Some(Path::new("/opt/ee/bin/ee")),
                fallback
            ),
            PathBuf::from("/custom/runtime")
        );
        assert_eq!(
            resolve_bundled_runtime_root(None, Some(Path::new("/opt/ee/bin/ee")), fallback),
            PathBuf::from("/opt/ee/share/ee")
        );
        let expected_windows = if cfg!(windows) {
            PathBuf::from("C:/Program Files/ee/runtime")
        } else {
            fallback.join("runtime")
        };
        assert_eq!(
            resolve_bundled_runtime_root(None, Some(windows_exe), fallback),
            expected_windows
        );
    }

    #[test]
    fn xi_core_client_extras_dir_uses_bundled_plugin_tree() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("runtime");
        let plugins_dir = runtime_root.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let _guard = super::TestEnvVarGuard::set("EE_RUNTIME_DIR", &runtime_root);

        assert_eq!(xi_core_client_extras_dir(), Some(plugins_dir));
    }

    #[test]
    fn ancestor_chain_merges_outer_to_inner() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.rs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(project.join(".ee.toml"), "cursor_line = true\nindent_size = 2\n").unwrap();
        std::fs::write(folder.join(".ee.toml"), "indent_size = 8\nwrap_lines = true\n").unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert!(settings.cursor_line);
        assert!(settings.wrap_lines);
        assert_eq!(settings.indent_size, 8);
    }

    #[test]
    fn root_true_in_folder_stops_user_and_system_layers() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.rs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
        std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n")
            .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "insert_final_newline = true\n",
        )
        .unwrap();
        std::fs::write(project.join(".ee.toml"), "cursor_line = true\n").unwrap();
        std::fs::write(folder.join(".ee.toml"), "root = true\nwrap_lines = true\n").unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert!(settings.wrap_lines);
        assert!(!settings.cursor_line);
        assert!(!settings.insert_final_newline);
        assert!(!settings.trim_trailing_whitespace);
    }

    #[test]
    fn root_true_in_project_stops_user_and_system_but_keeps_inner_folder() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.rs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
        std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n")
            .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "insert_final_newline = true\n",
        )
        .unwrap();
        std::fs::write(project.join(".ee.toml"), "root = true\ncursor_line = true\n").unwrap();
        std::fs::write(folder.join(".ee.toml"), "wrap_lines = true\n").unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert!(settings.cursor_line);
        assert!(settings.wrap_lines);
        assert!(!settings.insert_final_newline);
        assert!(!settings.trim_trailing_whitespace);
    }

    #[test]
    fn root_true_in_user_stops_system_but_keeps_workspace_layers() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.rs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
        std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n")
            .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "root = true\ninsert_final_newline = true\n",
        )
        .unwrap();
        std::fs::write(project.join(".ee.toml"), "cursor_line = true\n").unwrap();
        std::fs::write(folder.join(".ee.toml"), "wrap_lines = true\n").unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert!(settings.insert_final_newline);
        assert!(settings.cursor_line);
        assert!(settings.wrap_lines);
        assert!(!settings.trim_trailing_whitespace);
    }

    #[test]
    fn lsp_config_merges_system_user_and_project_layers() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.rs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            env.system_config_path.as_path(),
            "[lsp.servers.gleam]\nlanguage_name = \"Gleam\"\ncommand = \"gleam\"\nextensions = [\"gleam\"]\n",
        )
        .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[lsp.servers.gleam]\nargs = [\"lsp\"]\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.gleam]\nsupports_single_file = false\nworkspace_identifier = \"gleam.toml\"\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&file), &env);
        let gleam = settings.lsp.servers.get("gleam").unwrap();

        assert_eq!(gleam.language_name, "Gleam");
        assert_eq!(gleam.command, "gleam");
        assert_eq!(gleam.args, vec!["lsp"]);
        assert_eq!(gleam.extensions, vec!["gleam"]);
        assert!(!gleam.supports_single_file);
        assert_eq!(gleam.workspace_identifier.as_deref(), Some("gleam.toml"));
    }

    #[test]
    fn lsp_config_replaces_scalars_and_arrays() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.rust]\nlanguage_name = \"Rust\"\ncommand = \"rust-analyzer\"\nargs = [\"--stdio\"]\nextensions = [\"rs\", \"ron\"]\nworkspace_identifier = \"Cargo.toml\"\n",
        )
        .unwrap();
        std::fs::write(
            folder.join(".ee.toml"),
            "[lsp.servers.rust]\ncommand = \"rust-analyzer-nightly\"\nargs = [\"--nightly\"]\nextensions = [\"rs\"]\nworkspace_identifier = \"Rust.toml\"\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&folder.join("main.rs")), &env);
        let rust = settings.lsp.servers.get("rust").unwrap();

        assert_eq!(rust.command, "rust-analyzer-nightly");
        assert_eq!(rust.args, vec!["--nightly"]);
        assert_eq!(rust.extensions, vec!["rs"]);
        assert_eq!(rust.workspace_identifier.as_deref(), Some("Rust.toml"));
    }

    #[test]
    fn lsp_config_shallow_merges_env_and_replaces_initialization_options() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let file = project.join("main.ts");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[lsp.servers.typescript]\nlanguage_name = \"Typescript\"\ncommand = \"typescript-language-server\"\nextensions = [\"ts\"]\nenv = { PATH_HINT = \"/opt/bin\", KEEP = \"yes\" }\ninitialization_options = { format = true }\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.typescript]\nenv = { PATH_HINT = \"/custom/bin\", EXTRA = \"1\" }\ninitialization_options = { format = false, lint = true }\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&file), &env);
        let ts = settings.lsp.servers.get("typescript").unwrap();

        assert_eq!(ts.env.get("PATH_HINT").map(String::as_str), Some("/custom/bin"));
        assert_eq!(ts.env.get("KEEP").map(String::as_str), Some("yes"));
        assert_eq!(ts.env.get("EXTRA").map(String::as_str), Some("1"));
        assert_eq!(
            ts.initialization_options
                .as_ref()
                .and_then(|value| value.get("format"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            ts.initialization_options
                .as_ref()
                .and_then(|value| value.get("lint"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn lsp_config_enabled_false_removes_server() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let file = project.join("main.ts");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ee.toml"), "[lsp.servers.typescript]\nenabled = false\n")
            .unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert!(!settings.lsp.servers.contains_key("typescript"));
        assert_eq!(
            settings
                .lsp
                .disabled_servers
                .get("typescript")
                .map(|server| server.extensions.as_slice()),
            Some(
                &[
                    String::from("ts"),
                    String::from("tsx"),
                    String::from("mts"),
                    String::from("cts")
                ][..]
            )
        );
        assert_eq!(
            settings.lsp.language_servers.get("typescript"),
            Some(&vec![String::from("typescript")])
        );
    }

    #[test]
    fn lsp_config_normalizes_extensions_and_rejects_empty_values() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.gleam]\nlanguage_name = \"Gleam\"\ncommand = \"gleam\"\nextensions = [\".gleam\", \".\", \"\"]\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&project.join("main.gleam")), &env);
        let gleam = settings.lsp.servers.get("gleam").unwrap();

        assert_eq!(gleam.extensions, vec!["gleam"]);
    }

    #[test]
    fn lsp_config_duplicate_extensions_later_server_wins() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.alpha]\nlanguage_name = \"Alpha\"\ncommand = \"alpha\"\nextensions = [\"demo\", \"alpha\"]\n\n[lsp.servers.beta]\nlanguage_name = \"Beta\"\ncommand = \"beta\"\nextensions = [\"demo\", \"beta\"]\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&project.join("main.demo")), &env);
        let alpha = settings.lsp.servers.get("alpha").unwrap();
        let beta = settings.lsp.servers.get("beta").unwrap();

        assert_eq!(alpha.extensions, vec!["alpha"]);
        assert_eq!(beta.extensions, vec!["demo", "beta"]);
    }

    #[test]
    fn lsp_config_normalizes_filenames_and_rejects_invalid_values() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.dockerfile]\nlanguage_name = \"Dockerfile\"\ncommand = \"docker-langserver\"\nfilenames = [\" Dockerfile \", \"\", \"nested/Dockerfile\", \"nested\\\\Dockerfile\", \"Containerfile\"]\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&project.join("Dockerfile")), &env);
        let dockerfile = settings.lsp.servers.get("dockerfile").unwrap();

        assert_eq!(dockerfile.filenames, vec!["Dockerfile", "Containerfile"]);
    }

    #[test]
    fn lsp_config_duplicate_filenames_later_server_wins() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.alpha]\nlanguage_name = \"Alpha\"\ncommand = \"alpha\"\nfilenames = [\"Sharedfile\", \"Alphafile\"]\n\n[lsp.servers.beta]\nlanguage_name = \"Beta\"\ncommand = \"beta\"\nfilenames = [\"Sharedfile\", \"Betafile\"]\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&project.join("Sharedfile")), &env);
        let alpha = settings.lsp.servers.get("alpha").unwrap();
        let beta = settings.lsp.servers.get("beta").unwrap();

        assert_eq!(alpha.filenames, vec!["Alphafile"]);
        assert_eq!(beta.filenames, vec!["Sharedfile", "Betafile"]);
    }

    #[test]
    fn lsp_config_table_includes_disabled_matching_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ee.toml"), "[lsp.servers.dockerfile]\nenabled = false\n")
            .unwrap();

        let settings = load_config_with_env(Some(&project.join("Dockerfile")), &env);
        let table = settings.lsp.to_config_table();

        assert_eq!(
            table
                .get("disabled_language_config")
                .and_then(Value::as_object)
                .and_then(|servers| servers.get("dockerfile"))
                .and_then(|server| server.get("filenames"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            table
                .get("language_servers")
                .and_then(Value::as_object)
                .and_then(|languages| languages.get("dockerfile"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn unified_language_lsp_attachments_allow_extensionless_server_defs() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let file = project.join("main.ts");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.tsserver]\nlanguage_name = \"TypeScript\"\ncommand = \"typescript-language-server\"\n\n[languages.typescript]\nlsp = [\"tsserver\"]\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert!(settings.lsp.servers.contains_key("tsserver"));
        assert_eq!(
            settings.lsp.language_servers.get("typescript"),
            Some(&vec![String::from("tsserver")])
        );
    }

    #[test]
    fn unified_language_lsp_attachments_replace_across_layers() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.ts");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[lsp.servers.typescript]\nlanguage_name = \"TypeScript\"\ncommand = \"typescript-language-server\"\nextensions = [\"ts\"]\n\n[lsp.servers.eslint]\nlanguage_name = \"ESLint\"\ncommand = \"vscode-eslint-language-server\"\n\n[languages.typescript]\nlsp = [\"typescript\", \"eslint\"]\n",
        )
        .unwrap();
        std::fs::write(project.join(".ee.toml"), "[languages.typescript]\nlsp = [\"eslint\"]\n")
            .unwrap();

        let settings = load_config_with_env(Some(&file), &env);

        assert_eq!(
            settings.lsp.language_servers.get("typescript"),
            Some(&vec![String::from("eslint")])
        );
    }

    #[test]
    fn lsp_config_root_true_stops_project_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.rs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[lsp.servers.rust]\ncommand = \"rust-analyzer\"\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "root = true\n[lsp.servers.rust]\ncommand = \"project-rust\"\n",
        )
        .unwrap();
        std::fs::write(folder.join(".ee.toml"), "[lsp.servers.rust]\ncommand = \"inner-rust\"\n")
            .unwrap();

        let settings = load_config_with_env(Some(&file), &env);
        let rust = settings.lsp.servers.get("rust").unwrap();

        assert_eq!(rust.command, "inner-rust");
    }

    #[test]
    fn runtime_language_toml_parses_crate_source() {
        let raw: EeToml = toml::from_str(
            r#"
[languages.gleam]
name = "Gleam"
file_types = ["gleam"]

[languages.gleam.grammar]
library = "tree-sitter-gleam"
symbol = "tree_sitter_gleam"

[languages.gleam.grammar.source.crate]
name = "tree-sitter-gleam"
version = "1.2.3"
"#,
        )
        .unwrap();

        let gleam = raw.languages.get("gleam").unwrap();
        assert_eq!(gleam.name.as_deref(), Some("Gleam"));
        assert_eq!(gleam.file_types.as_deref(), Some(&[String::from("gleam")][..]));
        assert!(matches!(
            gleam.grammar.as_ref().and_then(|grammar| grammar.source.as_ref()),
            Some(xi_core_lib::runtime_loader::RuntimeGrammarSource::Crate(source))
                if source.name == "tree-sitter-gleam" && source.version == "1.2.3"
        ));
    }

    #[test]
    fn runtime_language_toml_parses_git_branch_tag_and_rev_sources() {
        for (label, source_table) in [
            ("branch", "branch = \"main\""),
            ("tag", "tag = \"v1.0.0\""),
            ("rev", "rev = \"abc123\""),
        ] {
            let text = format!(
                "[languages.demo]\nname = \"Demo\"\nfile_types = [\"demo\"]\n\n[languages.demo.grammar]\nlibrary = \"tree-sitter-demo\"\nsymbol = \"tree_sitter_demo\"\n\n[languages.demo.grammar.source.git]\nurl = \"https://example.com/tree-sitter-demo\"\n{source_table}\n"
            );
            let raw: EeToml = toml::from_str(&text).unwrap();
            let demo = raw.languages.get("demo").unwrap();
            match demo.grammar.as_ref().and_then(|grammar| grammar.source.as_ref()) {
                Some(xi_core_lib::runtime_loader::RuntimeGrammarSource::Git(source)) => {
                    assert_eq!(source.url, "https://example.com/tree-sitter-demo");
                    match label {
                        "branch" => assert_eq!(source.branch.as_deref(), Some("main")),
                        "tag" => assert_eq!(source.tag.as_deref(), Some("v1.0.0")),
                        "rev" => assert_eq!(source.rev.as_deref(), Some("abc123")),
                        _ => unreachable!(),
                    }
                }
                other => panic!("unexpected source for {label}: {other:?}"),
            }
        }
    }

    #[test]
    fn runtime_language_toml_rejects_mixed_source_kinds() {
        let error = toml::from_str::<EeToml>(
            r#"
[languages.demo]
name = "Demo"
file_types = ["demo"]

[languages.demo.grammar]
library = "tree-sitter-demo"
symbol = "tree_sitter_demo"

[languages.demo.grammar.source.crate]
name = "tree-sitter-demo"
version = "1.2.3"

[languages.demo.grammar.source.git]
url = "https://example.com/tree-sitter-demo"
rev = "abc123"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("source"));
    }

    #[test]
    fn validate_config_file_rejects_incomplete_runtime_language_definition() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        std::fs::write(
            &path,
            r#"
[languages.newlang]
lsp = ["newlang"]
"#,
        )
        .unwrap();

        let error = validate_config_file(&path).unwrap_err();

        assert!(error.contains("Config validation error"));
        assert!(error.contains("runtime language `newlang` is missing non-empty file_types"));
    }

    #[test]
    fn validate_config_file_allows_partial_builtin_runtime_override() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        std::fs::write(
            &path,
            r#"
[languages.rust]
lsp = ["rust"]
"#,
        )
        .unwrap();

        validate_config_file(&path).unwrap();
    }

    #[test]
    fn runtime_language_config_uses_same_layer_precedence_as_lsp() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let folder = project.join("folder");
        let file = folder.join("main.gleam");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            env.system_config_path.as_path(),
            "[languages.gleam]\nname = \"Gleam\"\nfile_types = [\".gleam\"]\n\n[languages.gleam.grammar]\nlibrary = \"tree-sitter-gleam\"\nsymbol = \"tree_sitter_gleam\"\n\n[languages.gleam.grammar.source.crate]\nname = \"tree-sitter-gleam\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[languages.gleam.grammar]\nlibrary = \"tree-sitter-gleam-user\"\n\n[languages.gleam.grammar.source.crate]\nname = \"tree-sitter-gleam-user\"\nversion = \"1.1.0\"\n",
        )
        .unwrap();
        std::fs::write(project.join(".ee.toml"), "[languages.gleam]\nenabled = false\n").unwrap();

        let runtime = runtime_languages_with_env(Some(&file), &env);
        let user = runtime.user_overrides.get("gleam").unwrap();
        let workspace = runtime.workspace_overrides.get("gleam").unwrap();

        assert_eq!(user.file_types.as_deref(), Some(&[String::from("gleam")][..]));
        assert_eq!(
            user.grammar.as_ref().and_then(|grammar| grammar.library.as_deref()),
            Some("tree-sitter-gleam-user")
        );
        assert_eq!(workspace.enabled, Some(false));
    }

    #[test]
    fn system_config_is_lowest_priority_external_layer() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
        std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n")
            .unwrap();

        let layers = discover_config_layers_with_env(&env, None).layers;
        let settings = load_config_with_env(None, &env);

        assert_eq!(layer_paths(&layers), vec![env.system_config_path.clone()]);
        assert!(settings.trim_trailing_whitespace);
    }

    #[test]
    fn search_report_marks_legacy_as_fallback_when_xdg_missing() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
        std::fs::write(env.home_dir.as_ref().unwrap().join(".ee.toml"), "cursor_line = true\n")
            .unwrap();

        let report = config_search_report_with_env(&env, None);
        let legacy = report
            .layers
            .into_iter()
            .find(|layer| layer.kind == ConfigLayerKind::UserLegacy)
            .unwrap();

        assert!(legacy.loaded);
        assert_eq!(legacy.note.as_deref(), Some("loaded because XDG user config is missing"));
    }

    #[test]
    fn ee_toml_parses_keymap_overrides() {
        let toml = r#"
[keymap]
inherit_defaults = false

[[keymap.bindings]]
mode = "normal"
key = "H"
action = "request_hover"

[[keymap.unbind]]
mode = "normal"
key = "K"
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        let mut settings = EditorSettings::default();
        settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

        assert!(!settings.keymap.inherit_defaults);
        assert_eq!(settings.keymap.operations.len(), 2);
    }

    #[test]
    fn ee_toml_parses_lsp_servers() {
        let toml = r#"
[lsp.servers.gleam]
language_name = "Gleam"
command = "gleam"
args = ["lsp"]
extensions = ["gleam"]
supports_single_file = false
workspace_identifier = "gleam.toml"

[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]

[lsp.servers.dockerfile]
command = "docker-langserver"
filenames = ["Dockerfile", "Containerfile"]
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        let gleam = raw.lsp.as_ref().unwrap().servers.get("gleam").unwrap();
        assert_eq!(gleam.language_name.as_deref(), Some("Gleam"));
        assert_eq!(gleam.command.as_deref(), Some("gleam"));
        assert_eq!(gleam.args, Some(vec!["lsp".to_owned()]));
        assert_eq!(gleam.extensions, Some(vec!["gleam".to_owned()]));
        assert_eq!(gleam.supports_single_file, Some(false));
        assert_eq!(gleam.workspace_identifier.as_deref(), Some("gleam.toml"));
        assert_eq!(gleam.enabled, None);
        assert!(gleam.env.is_empty());
        assert_eq!(gleam.initialization_options, None);

        let rust = raw.lsp.as_ref().unwrap().servers.get("rust").unwrap();
        assert_eq!(rust.command.as_deref(), Some("rust-analyzer"));
        assert_eq!(rust.extensions, Some(vec!["rs".to_owned()]));
        assert_eq!(rust.language_name, None);

        let dockerfile = raw.lsp.as_ref().unwrap().servers.get("dockerfile").unwrap();
        assert_eq!(dockerfile.command.as_deref(), Some("docker-langserver"));
        assert_eq!(
            dockerfile.filenames,
            Some(vec!["Dockerfile".to_owned(), "Containerfile".to_owned()])
        );
    }

    #[test]
    fn ee_toml_parses_unified_language_lsp_attachments() {
        let toml = r#"
[languages.typescript]
lsp = ["typescript", "eslint"]
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        assert_eq!(
            raw.languages.get("typescript").and_then(|language| language.lsp.as_deref()),
            Some(&[String::from("typescript"), String::from("eslint")][..])
        );
    }

    #[test]
    fn ee_toml_rejects_unknown_lsp_server_fields() {
        let toml = r#"
[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]
bogus = true
"#;

        let err = toml::from_str::<EeToml>(toml).unwrap_err();

        assert!(err.to_string().contains("unknown field `bogus`"));
    }

    #[test]
    fn ee_toml_parses_disabled_lsp_server() {
        let toml = r#"
[lsp.servers.typescript]
enabled = false
filenames = ["tsconfig.json"]
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        let server = raw.lsp.as_ref().unwrap().servers.get("typescript").unwrap();
        assert_eq!(server.enabled, Some(false));
        assert_eq!(server.command, None);
        assert_eq!(server.extensions, None);
        assert_eq!(server.filenames, Some(vec!["tsconfig.json".to_owned()]));
    }

    #[test]
    fn ee_toml_parses_lsp_env() {
        let toml = r#"
[lsp.servers.typescript]
command = "typescript-language-server"
extensions = ["ts"]
env = { NODE_NO_WARNINGS = "1", PATH_HINT = "/opt/bin" }
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        let server = raw.lsp.as_ref().unwrap().servers.get("typescript").unwrap();
        assert_eq!(server.env.get("NODE_NO_WARNINGS").map(String::as_str), Some("1"));
        assert_eq!(server.env.get("PATH_HINT").map(String::as_str), Some("/opt/bin"));
    }

    #[test]
    fn ee_toml_parses_lsp_initialization_options() {
        let toml = r#"
[lsp.servers.json]
command = "vscode-json-languageserver"
extensions = ["json"]
initialization_options = { provideFormatter = true, nested = { mode = "strict" } }
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        let server = raw.lsp.as_ref().unwrap().servers.get("json").unwrap();
        let init = server.initialization_options.as_ref().unwrap();
        assert_eq!(init.get("provideFormatter").and_then(Value::as_bool), Some(true));
        assert_eq!(
            init.get("nested")
                .and_then(Value::as_object)
                .and_then(|nested| nested.get("mode"))
                .and_then(Value::as_str),
            Some("strict")
        );
    }

    #[test]
    fn readme_documents_lsp_server_config() {
        let readme = include_str!("../../../README.md");

        assert!(readme.contains("[lsp.servers.<id>]"));
        assert!(readme.contains("lsp = [\"typescript\", \"eslint\"]"));
        assert!(readme.contains("Config precedence"));
        assert!(readme.contains("typescript"));
    }

    #[test]
    fn ee_toml_parses_key_sequence_overrides() {
        let toml = r#"
[keymap]
inherit_defaults = true
sequence_timeout_ms = 250

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "file_picker"
description = "find files"
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        let mut settings = EditorSettings::default();
        settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

        assert_eq!(settings.keymap.sequence_bindings.len(), 1);
        assert_eq!(settings.keymap.sequence_timeout_ms, 250);
        assert_eq!(settings.keymap.sequence_bindings[0].description, "find files");
        assert_eq!(settings.keymap.sequence_bindings[0].sequence.len(), 3);
    }

    #[test]
    fn config_scope_paths_use_xdg_for_global_and_cwd_for_local() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());

        assert_eq!(
            config_path_for_scope_with_env(ConfigScope::Global, &env).unwrap(),
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml")
        );
        assert_eq!(
            config_path_for_scope_with_env(ConfigScope::Local, &env).unwrap(),
            env.cwd.join(".ee.toml")
        );
    }

    #[test]
    fn merged_config_document_shows_effective_merged_values() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "wrap_lines = true\n",
        )
        .unwrap();
        std::fs::write(project.join(".ee.toml"), "indent_size = 2\n").unwrap();
        let file = project.join("main.rs");

        let text = toml::to_string_pretty(&resolved_config_with_env(Some(&file), &env)).unwrap();

        assert!(text.contains("wrap_lines = true"));
        assert!(text.contains("indent_size = 2"));
        assert!(text.contains("auto_indent = true"));
        assert!(text.contains("smart_indent = true"));
        assert!(text.contains("statusline_format = \"default\""));
    }

    #[test]
    fn xi_config_table_uses_configured_auto_and_smart_indent() {
        let raw: EeToml = toml::from_str("auto_indent = false\nsmart_indent = false\n").unwrap();
        let mut settings = EditorSettings::default();
        settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

        let table = settings.to_xi_config_table();

        assert_eq!(table.get("auto_indent").and_then(Value::as_bool), Some(false));
        assert_eq!(table.get("smart_indent").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn set_config_value_creates_global_file_and_get_reads_it() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();

        let written =
            set_config_value_with_env(ConfigScope::Global, "wrap_lines", "true", &env).unwrap();
        let value =
            get_config_value_with_env(ConfigScope::Global, "wrap_lines", &env).unwrap().unwrap();

        assert_eq!(written, temp.path().join("xdg").join("ee").join("config.toml"));
        assert_eq!(value, "true");
    }

    #[test]
    fn set_config_value_writes_local_nested_keys() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();

        let written = set_config_value_with_env(
            ConfigScope::Local,
            "lsp.servers.rust.command",
            "rust-analyzer",
            &env,
        )
        .unwrap();
        let contents = std::fs::read_to_string(&written).unwrap();

        assert_eq!(written, env.cwd.join(".ee.toml"));
        assert!(contents.contains("[lsp.servers.rust]"));
        assert!(contents.contains("command = \"rust-analyzer\""));
    }

    #[test]
    fn init_config_writes_fully_commented_local_template_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();

        let path = init_config_with_env(ConfigScope::Local, &env).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();

        assert_eq!(path, env.cwd.join(".ee.toml"));
        assert!(contents.lines().all(|line| line.is_empty() || line.starts_with('#')));
        assert!(contents.contains("# [lsp.servers.example]"));
        assert!(contents.contains("# [languages.example.grammar.source.crate]"));
        assert!(contents.contains("# [agents.servers.assistant]"));
        assert!(contents.contains("# [mcp.servers.example]"));

        let error = init_config_with_env(ConfigScope::Local, &env).unwrap_err();
        assert_eq!(error, format!("Config already exists: {}", path.display()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn init_config_creates_global_template_in_xdg_directory() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());

        let path = init_config_with_env(ConfigScope::Global, &env).unwrap();

        assert_eq!(path, temp.path().join("xdg").join("ee").join("config.toml"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), CONFIG_TEMPLATE);
    }

    // ── Agents-mode config ──────────────────────────────────────────────────

    #[test]
    fn agents_settings_disabled_by_default() {
        let settings = EditorSettings::default();

        assert!(!settings.agents.enabled);
        assert!(settings.agents.default_agent.is_none());
        assert!(settings.agents.servers.is_empty());
        assert!(settings.mcp.servers.is_empty());
    }

    #[test]
    fn ee_toml_parses_agents_settings() {
        let toml = r#"
[agents]
enabled = true
default_agent = "helper"

[agents.servers.helper]
command = "ee-helper"
args = ["serve"]
env = { EE_AGENT_MODE = "1" }
cwd = "/tmp/agent"

[agents.servers.other]
command = "other-agent"
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        assert_eq!(raw.agents.as_ref().unwrap().enabled, Some(true));

        let mut settings = EditorSettings::default();
        settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

        assert!(settings.agents.enabled);
        assert_eq!(settings.agents.default_agent.as_deref(), Some("helper"));
        let helper = settings.agents.servers.get("helper").unwrap();
        assert_eq!(helper.command, "ee-helper");
        assert_eq!(helper.args, vec!["serve"]);
        assert_eq!(helper.env.get("EE_AGENT_MODE").map(|v| v.raw.as_str()), Some("1"));
        assert_eq!(helper.cwd.as_deref(), Some(Path::new("/tmp/agent")));
        assert_eq!(settings.agents.servers.len(), 2);
    }

    // ── Agent env secret references (phase 5) ────────────────────────────────

    const AGENT_REF_TOML: &str = r#"
[agents]
enabled = true

[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "secret://openrouter-api-key", LANG = "en_US.UTF-8" }
"#;

    fn load_for(env: &ConfigEnvironment) -> EditorSettings {
        load_config_with_env(None, env)
    }

    #[test]
    fn agent_env_secret_reference_from_xdg_layer_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(&env, ConfigLayerKind::UserXdg, AGENT_REF_TOML);

        let settings = load_for(&env);
        let server = settings.agents.servers.get("gh").expect("server merged");
        let key = server.env.get("OPENROUTER_API_KEY").expect("env value");
        assert_eq!(key.raw, "secret://openrouter-api-key", "raw reference preserved");
        assert_eq!(key.layer, ConfigLayerKind::UserXdg);
        let literal = server.env.get("LANG").expect("literal");
        assert_eq!(literal.raw, "en_US.UTF-8");
        assert_eq!(literal.layer, ConfigLayerKind::UserXdg);
    }

    #[test]
    fn agent_env_secret_reference_from_legacy_user_layer_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(&env, ConfigLayerKind::UserLegacy, AGENT_REF_TOML);

        let settings = load_for(&env);
        let key = settings
            .agents
            .servers
            .get("gh")
            .expect("server merged")
            .env
            .get("OPENROUTER_API_KEY")
            .expect("env value");
        assert_eq!(key.raw, "secret://openrouter-api-key");
        assert_eq!(key.layer, ConfigLayerKind::UserLegacy);
    }

    #[test]
    fn agent_env_secret_reference_from_system_layer_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(&env, ConfigLayerKind::System, AGENT_REF_TOML);

        let settings = load_for(&env);
        assert!(
            !settings.agents.servers.contains_key("gh"),
            "system-layer secret reference must not merge"
        );
    }

    #[test]
    fn agent_env_secret_reference_from_ancestor_layer_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(&env, ConfigLayerKind::Ancestor, AGENT_REF_TOML);

        let settings = load_for(&env);
        assert!(
            !settings.agents.servers.contains_key("gh"),
            "ancestor-layer secret reference must not merge"
        );
    }

    #[test]
    fn agent_env_project_literal_override_wins_over_global_literal() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            r#"
[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "global-literal" }
"#,
        );
        write_config_layer(
            &env,
            ConfigLayerKind::Ancestor,
            r#"
[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "project-literal" }
"#,
        );

        let settings = load_for(&env);
        let key = settings
            .agents
            .servers
            .get("gh")
            .expect("server merged")
            .env
            .get("OPENROUTER_API_KEY")
            .expect("env value");
        assert_eq!(key.raw, "project-literal", "higher-priority literal replaces global");
        assert_eq!(key.layer, ConfigLayerKind::Ancestor);
    }

    #[test]
    fn rejected_ancestor_reference_keeps_lower_layer_server() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            r#"
[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "global-literal" }
"#,
        );
        // The ancestor cannot override with, or cause launch of, a reference.
        write_config_layer(&env, ConfigLayerKind::Ancestor, AGENT_REF_TOML);

        let settings = load_for(&env);
        let key = settings
            .agents
            .servers
            .get("gh")
            .expect("lower-layer server survives")
            .env
            .get("OPENROUTER_API_KEY")
            .expect("env value");
        assert_eq!(key.raw, "global-literal");
    }

    #[test]
    fn resolve_agent_server_rejects_malformed_secret_reference_with_field_path() {
        let server = AgentServerToml {
            command: Some(String::from("agent-bin")),
            args: None,
            env: BTreeMap::from([(
                String::from("OPENROUTER_API_KEY"),
                String::from("secret://bad name"),
            )]),
            cwd: None,
        };
        let err =
            resolve_agent_server("gh", &server, ConfigLayerKind::UserXdg).expect_err("rejected");
        assert!(err.contains("agents.servers.gh.env.OPENROUTER_API_KEY"), "field path: {err}");
        assert!(!err.contains("bad name"), "no raw value echo: {err}");
    }

    #[test]
    fn agent_env_secret_reference_substring_stays_literal() {
        let server = AgentServerToml {
            command: Some(String::from("agent-bin")),
            args: None,
            env: BTreeMap::from([
                (
                    String::from("ENDPOINT"),
                    String::from("https://api.example.com/secret://openrouter-api-key"),
                ),
                (String::from("NOTE"), String::from("see secret://docs")),
            ]),
            cwd: None,
        };
        let resolved =
            resolve_agent_server("gh", &server, ConfigLayerKind::Ancestor).expect("literals");
        assert_eq!(
            resolved.env.get("ENDPOINT").expect("literal").raw,
            "https://api.example.com/secret://openrouter-api-key"
        );
        assert_eq!(resolved.env.get("NOTE").expect("literal").raw, "see secret://docs");
        // Even from an ancestor layer, substrings are never treated as refs.
        assert_eq!(resolved.env.get("ENDPOINT").expect("literal").layer, ConfigLayerKind::Ancestor);
    }

    #[test]
    fn config_show_preserves_secret_reference_text() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(&env, ConfigLayerKind::UserXdg, AGENT_REF_TOML);

        let document = toml::to_string_pretty(&resolved_config_with_env(None, &env)).unwrap();
        assert!(
            document.contains("secret://openrouter-api-key"),
            "config show exposes the reference, never the plaintext: {document}"
        );
        assert!(!document.contains("sk-live"), "no resolved plaintext in show output");
    }

    #[test]
    fn ee_toml_parses_mcp_servers() {
        let toml = r#"
[mcp.servers.filesystem]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]

[mcp.servers.remote]
transport = "streamable_http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer token" }
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        let mut settings = EditorSettings::default();
        settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

        let servers = &settings.mcp.servers;
        match servers.get("filesystem").unwrap() {
            McpServerSettings::Stdio { command, args, env, cwd } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args,
                    &vec!["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()]
                );
                assert!(env.is_empty());
                assert!(cwd.is_none());
            }
            other => panic!("expected stdio transport, got {other:?}"),
        }
        match servers.get("remote").unwrap() {
            McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer token"));
                assert_eq!(*timeout_ms, DEFAULT_MCP_HTTP_TIMEOUT_MS);
            }
            other => panic!("expected streamable_http transport, got {other:?}"),
        }
    }

    #[test]
    fn ee_toml_rejects_unknown_agents_and_mcp_fields() {
        let err = toml::from_str::<EeToml>("[agents]\nbogus = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `bogus`"));

        let err =
            toml::from_str::<EeToml>("[mcp.servers.foo]\ntransport = \"stdio\"\nbogus = true\n")
                .unwrap_err();
        assert!(err.to_string().contains("unknown field `bogus`"));
    }

    #[test]
    fn ee_toml_mcp_server_requires_transport() {
        let err = toml::from_str::<EeToml>("[mcp.servers.foo]\ncommand = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("transport"));
    }

    #[test]
    fn project_config_enables_agents_while_defaults_stay_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let file = project.join("main.rs");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();

        // Built-in defaults keep agents disabled even with no config layers.
        let defaults = load_config_with_env(Some(&file), &env);
        assert!(!defaults.agents.enabled);
        assert!(defaults.agents.servers.is_empty());

        // User layer defines an agent server but never enables agents mode.
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[agents.servers.user-agent]\ncommand = \"user-agent\"\n",
        )
        .unwrap();

        // Project-local `.ee.toml` enables agents and refines the server.
        std::fs::write(
            project.join(".ee.toml"),
            "[agents]\nenabled = true\ndefault_agent = \"user-agent\"\n\n[agents.servers.user-agent]\ncommand = \"user-agent\"\nargs = [\"--stdio\"]\n",
        )
        .unwrap();

        let settings = load_config_with_env(Some(&file), &env);
        assert!(settings.agents.enabled);
        assert_eq!(settings.agents.default_agent.as_deref(), Some("user-agent"));
        let agent = settings.agents.servers.get("user-agent").unwrap();
        assert_eq!(agent.command, "user-agent");
        assert_eq!(agent.args, vec!["--stdio"]);
    }

    #[test]
    fn validate_config_file_rejects_empty_agent_command() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        std::fs::write(&path, "[agents.servers.broken]\ncommand = \"\"\n").unwrap();

        let error = validate_config_file(&path).unwrap_err();

        assert!(error.contains("Config validation error"));
        assert!(error.contains("agents server `broken`"));
        assert!(error.contains("command must not be empty"));
    }

    #[test]
    fn validate_config_file_rejects_invalid_mcp_url() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");

        std::fs::write(
            &path,
            "[mcp.servers.remote]\ntransport = \"streamable_http\"\nurl = \"ftp://example.com/mcp\"\n",
        )
        .unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("mcp server `remote`"));
        assert!(error.contains("scheme must be http or https"));

        std::fs::write(
            &path,
            "[mcp.servers.remote]\ntransport = \"streamable_http\"\nurl = \"not a url\"\n",
        )
        .unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("invalid mcp url"));
    }

    #[test]
    fn validate_config_file_rejects_empty_server_ids() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");

        std::fs::write(&path, "[agents.servers.\"\"]\ncommand = \"x\"\n").unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("agent server id must not be empty"));

        std::fs::write(&path, "[mcp.servers.\"\"]\ntransport = \"stdio\"\ncommand = \"x\"\n")
            .unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("mcp server id must not be empty"));
    }

    #[test]
    fn validate_config_file_rejects_duplicate_effective_server_ids() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        std::fs::write(
            &path,
            "[agents.servers.dup]\ncommand = \"agent\"\n\n[mcp.servers.dup]\ntransport = \"stdio\"\ncommand = \"mcp-server\"\n",
        )
        .unwrap();

        let error = validate_config_file(&path).unwrap_err();

        assert!(error.contains("Config validation error"));
        assert!(error.contains("duplicate effective server id `dup`"));
    }

    #[test]
    fn config_schema_includes_agents_and_mcp_fields() {
        let schema: Value = serde_json::from_str(&config_schema_json().unwrap()).unwrap();
        let properties = schema.get("properties").and_then(Value::as_object).unwrap();
        assert!(properties.contains_key("agents"));
        assert!(properties.contains_key("mcp"));

        let defs = schema.get("$defs").and_then(Value::as_object).unwrap();
        let agents = defs.get("AgentsToml").unwrap().get("properties").unwrap();
        assert!(agents.get("enabled").is_some());
        assert!(agents.get("default_agent").is_some());
        assert!(agents.get("servers").is_some());

        let mcp = defs.get("McpToml").unwrap().get("properties").unwrap();
        assert!(mcp.get("servers").is_some());
        assert!(mcp.get("proxy").is_some());
        let proxy = defs.get("McpProxyToml").unwrap().get("properties").unwrap();
        assert!(proxy.get("enabled").is_some());

        // Agent `env` documentation exposes the `secret://<name>` reference
        // syntax and its user-global-only resolution boundary (phase 5)
        // without changing the config shape.
        let agent_server = defs.get("AgentServerToml").unwrap();
        let env_schema =
            agent_server.get("properties").and_then(|p| p.get("env")).expect("env property");
        let description =
            env_schema.get("description").and_then(Value::as_str).expect("description");
        assert!(description.contains("secret://<name>"), "documents reference syntax");
        assert!(description.contains("user config layer"), "documents user-only boundary");
        assert!(
            !env_schema.as_object().unwrap().contains_key("pattern"),
            "reference syntax does not narrow the config shape"
        );
    }

    #[test]
    fn mcp_proxy_disabled_by_default_and_parsed_from_toml() {
        assert!(!EditorSettings::default().mcp.proxy.enabled);

        let toml = r#"
[mcp.proxy]
enabled = true
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        let mut settings = EditorSettings::default();
        settings.merge_toml(&raw, ConfigLayerKind::UserXdg);
        assert!(settings.mcp.proxy.enabled);

        // Serialize back through the full document shape: the resolved
        // document carries the proxy flag.
        let document = EeToml {
            mcp: Some(McpToml {
                proxy: Some(McpProxyToml { enabled: Some(true) }),
                servers: BTreeMap::new(),
            }),
            ..Default::default()
        };
        let text = toml::to_string(&document).unwrap();
        let roundtrip: EeToml = toml::from_str(&text).unwrap();
        let mut restored = EditorSettings::default();
        restored.merge_toml(&roundtrip, ConfigLayerKind::UserXdg);
        assert!(restored.mcp.proxy.enabled);
    }

    #[test]
    fn mcp_settings_to_toml_includes_proxy_when_enabled() {
        let settings = EditorSettings::default();
        assert!(mcp_settings_to_toml(&settings.mcp).is_none());

        let mut enabled = EditorSettings::default();
        enabled.mcp.proxy.enabled = true;
        let toml = mcp_settings_to_toml(&enabled.mcp).expect("proxy present");
        assert_eq!(toml.proxy.as_ref().and_then(|p| p.enabled), Some(true));
    }

    #[test]
    fn merged_config_document_includes_agents_and_mcp() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let project = env.cwd.join("project");
        let file = project.join("main.rs");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(".ee.toml"),
            "[agents]\nenabled = true\ndefault_agent = \"helper\"\n\n[agents.servers.helper]\ncommand = \"helper-agent\"\n\n[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\n",
        )
        .unwrap();

        let text = toml::to_string_pretty(&resolved_config_with_env(Some(&file), &env)).unwrap();

        assert!(text.contains("enabled = true"));
        assert!(text.contains("default_agent = \"helper\""));
        assert!(text.contains("command = \"helper-agent\""));
        assert!(text.contains("transport = \"stdio\""));
        assert!(text.contains("command = \"mcp-tools\""));
    }

    #[test]
    fn merged_config_document_keeps_agents_disabled_without_config() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();

        let text = toml::to_string_pretty(&resolved_config_with_env(None, &env)).unwrap();

        assert!(text.contains("enabled = false"));
        assert!(!text.contains("[mcp]"));
    }
}
