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

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{env, fs};

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::Value;
use xi_core_lib::config::Table as XiConfigTable;
use xi_lsp_lib::{
    Config as PluginLspConfig, DisabledLanguageConfig as PluginDisabledLanguageConfig,
    LanguageConfig as PluginLanguageConfig,
};

use crate::keymap::{self, KeymapOperation, KeymapSettings, SequenceBinding};

const SYSTEM_CONFIG_PATH: &str = "/etc/ee/config.toml";
pub(crate) const LSP_PLUGIN_NAME: &str = "xi-lsp-plugin";

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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspSettings {
    pub servers: BTreeMap<String, LspServerSettings>,
    pub disabled_servers: BTreeMap<String, DisabledLspServerSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisabledLspServerSettings {
    pub extensions: Vec<String>,
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
                    (id, DisabledLspServerSettings { extensions: server.extensions })
                })
                .collect(),
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
                        PluginDisabledLanguageConfig { extensions: server.extensions.clone() },
                    )
                })
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
    pub supports_single_file: bool,
    pub workspace_identifier: Option<String>,
    pub env: BTreeMap<String, String>,
    pub initialization_options: Option<Value>,
}

#[derive(Debug, Clone)]
struct LspSettingsBuilder {
    servers: BTreeMap<String, LspServerSettingsBuilder>,
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
                            supports_single_file: Some(server.supports_single_file),
                            workspace_identifier: server.workspace_identifier.clone(),
                            enabled: Some(true),
                            env: server.env.clone(),
                            initialization_options: server.initialization_options.clone(),
                        },
                    )
                })
                .collect(),
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

    fn finalize(self) -> LspSettings {
        let mut servers = BTreeMap::new();
        let mut disabled_servers = BTreeMap::new();
        for (id, server) in self.servers {
            if server.enabled == Some(false) {
                if let Some(extensions) = server.extensions {
                    let extensions = normalize_lsp_extensions(&id, &extensions);
                    if !extensions.is_empty() {
                        disabled_servers.insert(id, DisabledLspServerSettings { extensions });
                    }
                }
                continue;
            }

            let missing = [
                ("language_name", server.language_name.is_none()),
                ("command", server.command.is_none()),
                ("extensions", server.extensions.is_none()),
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

            let extensions =
                normalize_lsp_extensions(&id, server.extensions.as_ref().expect("validated above"));

            if extensions.is_empty() {
                eprintln!(
                    "ee: warning: invalid lsp server config for {}: missing non-empty extensions",
                    id
                );
                continue;
            }

            servers.insert(
                id,
                LspServerSettings {
                    language_name: server.language_name.expect("validated above"),
                    command: server.command.expect("validated above"),
                    args: server.args.unwrap_or_default(),
                    extensions,
                    supports_single_file: server.supports_single_file.unwrap_or(true),
                    workspace_identifier: server.workspace_identifier,
                    env: server.env,
                    initialization_options: server.initialization_options,
                },
            );
        }
        resolve_lsp_extension_ownership(&mut servers, &mut disabled_servers);
        LspSettings { servers, disabled_servers }
    }
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

fn resolve_lsp_extension_ownership(
    servers: &mut BTreeMap<String, LspServerSettings>,
    disabled_servers: &mut BTreeMap<String, DisabledLspServerSettings>,
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

    for (id, server) in servers.iter_mut() {
        server.extensions.retain(|extension| owner_by_extension.get(extension) == Some(id));
    }
    servers.retain(|_, server| !server.extensions.is_empty());

    for (id, server) in disabled_servers.iter_mut() {
        server.extensions.retain(|extension| owner_by_extension.get(extension) == Some(id));
    }
    disabled_servers.retain(|_, server| !server.extensions.is_empty());
}

#[derive(Debug, Clone, Default)]
struct LspServerSettingsBuilder {
    language_name: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    supports_single_file: Option<bool>,
    workspace_identifier: Option<String>,
    enabled: Option<bool>,
    env: BTreeMap<String, String>,
    initialization_options: Option<Value>,
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
        table.insert("auto_indent".into(), Value::Bool(true));
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
#[derive(Debug, Default, Deserialize)]
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
    pub keymap: Option<KeymapToml>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LspToml {
    #[serde(default)]
    pub servers: BTreeMap<String, LspServerToml>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LspServerToml {
    pub language_name: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub supports_single_file: Option<bool>,
    pub workspace_identifier: Option<String>,
    pub enabled: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub initialization_options: Option<Value>,
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
struct ConfigEnvironment {
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

    fn xdg_user_config_path(&self) -> Option<PathBuf> {
        self.config_dir.as_ref().map(|dir| dir.join("ee").join("config.toml"))
    }

    fn legacy_user_config_path(&self) -> Option<PathBuf> {
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

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyBindingTargetToml {
    pub mode: String,
    pub key: String,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyBindingEntryToml {
    pub mode: String,
    pub key: String,
    pub prefix: Option<String>,
    pub action: String,
}

#[derive(Debug, Clone, Deserialize)]
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
    fn merge_toml(&mut self, patch: &EeToml) {
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
            settings.merge_toml(&patch);
            if let Some(lsp_patch) = &patch.lsp {
                lsp.merge_toml(lsp_patch);
            }
        }
    }

    if let Some(file_path) = file_path {
        apply_editorconfig(&mut settings, file_path);
    }

    settings.lsp = lsp.finalize();

    settings
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
    toml::from_str::<EeToml>(&contents)
        .map(|_| ())
        .map_err(|err| format!("Config parse error in {}: {err}", path.display()))
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
        s.merge_toml(&raw);
        assert_eq!(s.indent_style, IndentStyle::Tabs);
        assert_eq!(s.indent_size, 2);
        assert_eq!(s.tab_width, 4);
    }

    #[test]
    fn ee_toml_defaults_unchanged_when_field_absent() {
        let toml = r#"trim_trailing_whitespace = true"#;
        let raw: EeToml = toml::from_str(toml).unwrap();
        let mut s = EditorSettings::default();
        s.merge_toml(&raw);
        assert_eq!(s.indent_style, IndentStyle::Spaces); // unchanged
        assert!(s.trim_trailing_whitespace);
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

    fn test_env(root: &Path) -> ConfigEnvironment {
        ConfigEnvironment {
            cwd: root.join("workspace"),
            home_dir: Some(root.join("home")),
            config_dir: Some(root.join("xdg")),
            system_config_path: root.join("etc").join("ee").join("config.toml"),
        }
    }

    fn layer_paths(layers: &[ConfigLayer]) -> Vec<PathBuf> {
        layers.iter().map(|layer| layer.path.clone()).collect()
    }

    #[test]
    fn xdg_user_config_preferred_over_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
                &[String::from("ts"), String::from("js"), String::from("jsx"), String::from("tsx")]
                    [..]
            )
        );
    }

    #[test]
    fn lsp_config_normalizes_extensions_and_rejects_empty_values() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
    fn lsp_config_table_includes_disabled_matching_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_env(temp.path());
        let project = env.cwd.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ee.toml"), "[lsp.servers.typescript]\nenabled = false\n")
            .unwrap();

        let settings = load_config_with_env(Some(&project.join("main.ts")), &env);
        let table = settings.lsp.to_config_table();

        assert_eq!(
            table
                .get("disabled_language_config")
                .and_then(Value::as_object)
                .and_then(|servers| servers.get("typescript"))
                .and_then(|server| server.get("extensions"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(4)
        );
    }

    #[test]
    fn lsp_config_root_true_stops_project_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_env(temp.path());
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
    fn system_config_is_lowest_priority_external_layer() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_env(temp.path());
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
        let env = test_env(temp.path());
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
        settings.merge_toml(&raw);

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
"#;
        let raw: EeToml = toml::from_str(toml).unwrap();

        let server = raw.lsp.as_ref().unwrap().servers.get("typescript").unwrap();
        assert_eq!(server.enabled, Some(false));
        assert_eq!(server.command, None);
        assert_eq!(server.extensions, None);
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
        settings.merge_toml(&raw);

        assert_eq!(settings.keymap.sequence_bindings.len(), 1);
        assert_eq!(settings.keymap.sequence_timeout_ms, 250);
        assert_eq!(settings.keymap.sequence_bindings[0].description, "find files");
        assert_eq!(settings.keymap.sequence_bindings[0].sequence.len(), 3);
    }
}
