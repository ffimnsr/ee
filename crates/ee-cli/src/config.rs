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

#[cfg(any(feature = "agents", test))]
use ee_agent_host::{
    AgentWebContextConfig, DEFAULT_WORKSPACE_MEMORY_CANDIDATE_RETENTION_DAYS,
    DEFAULT_WORKSPACE_MEMORY_EXPIRY_DAYS, DEFAULT_WORKSPACE_MEMORY_STALE_RETENTION_DAYS,
    DEFAULT_WORKSPACE_MEMORY_SUPERSEDED_RETENTION_DAYS, ResolvedRubberDuckConfig,
    RubberDuckBackend, RubberDuckConfig, RubberDuckMode, WebContextLimits,
    WorkspaceMemoryHostConfig,
    web_context::{
        BraveFreshness, BraveLlmContextOptions, BraveSafeSearchMode, BraveThresholdMode,
        ExaSearchMode, ExaSearchOptions, TavilySearchDepth, TavilySearchOptions, WebSearchProvider,
        WebSearchProviderOptions,
    },
};
use globset::GlobBuilder;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xi_core_lib::config::Table as XiConfigTable;
use xi_core_lib::runtime_loader::{
    RuntimeLanguageConfig, RuntimeLanguageOverrides,
    configure_default_runtime_loader_overrides_if_changed, validate_runtime_language_overrides,
};
use xi_lsp_lib::{
    Config as PluginLspConfig, DisabledLanguageConfig as PluginDisabledLanguageConfig,
    LanguageConfig as PluginLanguageConfig,
};

use crate::keymap::{self, KeymapOperation, KeymapSettings, SequenceBinding};

const SYSTEM_CONFIG_PATH: &str = "/etc/ee/config.toml";
// Raw config validation also runs in builds without the optional agents host.
// Keep these synchronized with the public host provider caps.
const MAX_EXA_RESULTS: usize = 50;
const MAX_TAVILY_RESULTS: usize = 50;
const MAX_TAVILY_CHUNKS_PER_SOURCE: usize = 3;
const MAX_BRAVE_RESULTS: usize = 20;
const MAX_BRAVE_TOKENS: usize = 10_000;
const MAX_BRAVE_URLS: usize = 20;
const MAX_BRAVE_SNIPPETS: usize = 20;
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
# max_concurrent_prompts = 4 # per configured agent connection; valid 1..32
#
# # Optional rubber duck. Select at most one backend.
# # [agents.rubber_duck]
# # mode = "manual" # "off", "manual", or "automatic"
# # internal_model_id = "critic" # ee-owned agent model registry id
# # external_agent_id = "critic-agent" # configured [agents.servers] id
# # max_calls = 2
# # max_context_bytes = 65536
# # max_output_bytes = 32768
# # timeout_ms = 90000
#
# # Web context is disabled by default. Put this only in user-global config;
# # workspace .ee.toml files may disable or tighten it, never enable/widen it.
# # [agents.web_context]
# # enabled = false
# # backend = "searxng" # or "exa", "brave_llm_context", "tavily"
# # endpoint = "https://search.example/search" # required only for SearXNG
# # provider_secret_reference = "secret://web-search-api-key"
# # browser_run_account_id = "0123456789abcdef0123456789abcdef"
# # browser_run_api_token_reference = "secret://cloudflare-browser-run-token"
# # browser_run_max_attempts = 3
# # browser_run_base_delay_ms = 500
# # browser_run_max_delay_ms = 10000
# # hosts = ["search.example"] # optional preapproval; search/fetch/browser grants stay separate
# # [agents.web_context.exa]
# # search_mode = "auto"
# # max_results = 10
# # [agents.web_context.limits]
# # max_response_bytes = 1048576
# # max_text_bytes = 262144
# # max_search_results = 10
# # max_redirects = 3
# # request_timeout_ms = 30000
# # max_concurrent_requests = 2
#
# # Agent local shortcut example. Agent actions only run while Agents TUI owns focus.
# [[keymap.bindings]]
# mode = "agent"
# key = "ctrl+r"
# action = "agent_history_search_reverse"
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
const DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS: usize = 4;
const MAX_AGENT_MAX_CONCURRENT_PROMPTS: usize = 32;
const MAX_WORKSPACE_MEMORY_VALUE_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_MEMORY_ACTIVE_FACTS: usize = 100_000;
const MAX_WORKSPACE_MEMORY_ACTIVE_BYTES: usize = 1024 * 1024 * 1024;
const MAX_WORKSPACE_MEMORY_TOTAL_FACTS: usize = 100_000;
const MAX_WORKSPACE_MEMORY_TOTAL_BYTES: usize = 1024 * 1024 * 1024;
const MAX_WORKSPACE_MEMORY_RECALL_RESULTS: usize = 100;
const MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS: u64 = 60_000;
const MAX_WORKSPACE_MEMORY_RETENTION_DAYS: u64 = 3_650;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceMemorySettings {
    pub enabled: bool,
    pub max_value_bytes: usize,
    pub max_active_facts: usize,
    pub max_active_bytes: usize,
    pub max_total_facts: usize,
    pub max_total_bytes: usize,
    pub max_recall_results: usize,
    pub busy_timeout_ms: u64,
    pub default_expiry_days: u64,
    pub candidate_retention_days: u64,
    pub stale_retention_days: u64,
    pub superseded_retention_days: u64,
}

impl Default for WorkspaceMemorySettings {
    fn default() -> Self {
        #[cfg(any(feature = "agents", test))]
        {
            let defaults = WorkspaceMemoryHostConfig::default();
            Self {
                enabled: false,
                max_value_bytes: defaults.quotas.max_value_bytes,
                max_active_facts: defaults.quotas.max_active_facts,
                max_active_bytes: defaults.quotas.max_active_bytes,
                max_total_facts: defaults.quotas.max_total_facts,
                max_total_bytes: defaults.quotas.max_total_bytes,
                max_recall_results: defaults.quotas.max_recall_results,
                busy_timeout_ms: u64::try_from(defaults.busy_timeout.as_millis())
                    .unwrap_or(MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS),
                default_expiry_days: DEFAULT_WORKSPACE_MEMORY_EXPIRY_DAYS,
                candidate_retention_days: DEFAULT_WORKSPACE_MEMORY_CANDIDATE_RETENTION_DAYS,
                stale_retention_days: DEFAULT_WORKSPACE_MEMORY_STALE_RETENTION_DAYS,
                superseded_retention_days: DEFAULT_WORKSPACE_MEMORY_SUPERSEDED_RETENTION_DAYS,
            }
        }
        #[cfg(not(any(feature = "agents", test)))]
        {
            Self {
                enabled: false,
                max_value_bytes: 4 * 1024,
                max_active_facts: 256,
                max_active_bytes: 512 * 1024,
                max_total_facts: 256,
                max_total_bytes: 512 * 1024,
                max_recall_results: 8,
                busy_timeout_ms: 2_000,
                default_expiry_days: 0,
                candidate_retention_days: 7,
                stale_retention_days: 30,
                superseded_retention_days: 90,
            }
        }
    }
}

/// Resolved agents-mode settings.  Agents mode is disabled by default at
/// runtime; `enabled` only becomes `true` through an explicit config layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentsSettings {
    pub enabled: bool,
    pub default_agent: Option<String>,
    /// Maximum provider prompts in flight on each configured agent connection.
    pub max_concurrent_prompts: usize,
    pub servers: BTreeMap<String, AgentServerSettings>,
    /// Frontend-resolved critic policy; translated to backend policy on use.
    pub rubber_duck: RubberDuckSettings,
    /// Durable workspace memory. Explicitly disabled unless configured.
    pub workspace_memory: WorkspaceMemorySettings,
    /// Trusted web retrieval policy. This exists only in agents-enabled builds;
    /// raw config remains parseable in every build so schema validation is stable.
    #[cfg(any(feature = "agents", test))]
    pub web_context: AgentWebContextConfig,
}

impl Default for AgentsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            default_agent: None,
            max_concurrent_prompts: DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS,
            servers: BTreeMap::new(),
            rubber_duck: RubberDuckSettings::default(),
            workspace_memory: WorkspaceMemorySettings::default(),
            #[cfg(any(feature = "agents", test))]
            web_context: AgentWebContextConfig::default(),
        }
    }
}

const DEFAULT_CRITIC_MAX_CALLS: usize = 2;
const MAX_CRITIC_MAX_CALLS: usize = 16;
const DEFAULT_CRITIC_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_CRITIC_CONTEXT_BYTES: usize = 64 * 1024;
const DEFAULT_CRITIC_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_CRITIC_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_CRITIC_TIMEOUT_MS: u64 = 90_000;
const MAX_CRITIC_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RubberDuckModeSetting {
    Off,
    #[default]
    Manual,
    Automatic,
}

/// Fully merged frontend-owned rubber-duck settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RubberDuckSettings {
    pub mode: RubberDuckModeSetting,
    pub internal_model_id: Option<String>,
    pub external_agent_id: Option<String>,
    pub max_calls: usize,
    pub max_context_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for RubberDuckSettings {
    fn default() -> Self {
        Self {
            mode: RubberDuckModeSetting::Manual,
            internal_model_id: None,
            external_agent_id: None,
            max_calls: DEFAULT_CRITIC_MAX_CALLS,
            max_context_bytes: DEFAULT_CRITIC_CONTEXT_BYTES,
            max_output_bytes: DEFAULT_CRITIC_OUTPUT_BYTES,
            timeout_ms: DEFAULT_CRITIC_TIMEOUT_MS,
        }
    }
}

#[cfg(any(feature = "agents", test))]
impl RubberDuckSettings {
    /// Translates frontend config into validated backend policy. Unknown optional
    /// backend ids degrade critic only; ordinary agent operation remains usable.
    pub(crate) fn resolve_backend_policy(
        &self,
        model_ids: &BTreeSet<String>,
        agent_ids: &BTreeSet<String>,
    ) -> Result<ResolvedRubberDuckConfig, String> {
        let backend = RubberDuckBackend::from_optional_ids(
            self.internal_model_id.clone(),
            self.external_agent_id.clone(),
        )
        .map_err(|error| error.to_string())?;
        let config = RubberDuckConfig {
            mode: match self.mode {
                RubberDuckModeSetting::Off => RubberDuckMode::Off,
                RubberDuckModeSetting::Manual => RubberDuckMode::Manual,
                RubberDuckModeSetting::Automatic => RubberDuckMode::Automatic,
            },
            backend,
            max_calls: self.max_calls,
            max_context_bytes: self.max_context_bytes,
            max_output_bytes: self.max_output_bytes,
            timeout: std::time::Duration::from_millis(self.timeout_ms),
        };
        config.resolve(model_ids, agent_ids).map_err(|error| error.to_string())
    }
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
    /// Optional frontend-only label shown in agent pickers.
    pub label: Option<String>,
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
    /// Per-connection prompt concurrency. Valid range: 1 through 32.
    pub max_concurrent_prompts: Option<usize>,
    #[serde(default)]
    pub servers: BTreeMap<String, AgentServerToml>,
    /// Optional bounded critic policy.
    pub rubber_duck: Option<RubberDuckToml>,
    /// Explicit opt-in and quotas for durable canonical-workspace memory.
    pub workspace_memory: Option<WorkspaceMemoryToml>,
    /// Trusted configuration for optional agent web retrieval. Only user-global
    /// config can grant access; workspace files can only disable or restrict it.
    pub web_context: Option<AgentWebContextToml>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceMemoryToml {
    /// Enables local durable workspace memory. Defaults to `false`.
    pub enabled: Option<bool>,
    /// Maximum UTF-8 bytes in one fact value.
    pub max_value_bytes: Option<usize>,
    /// Maximum active facts in one canonical workspace scope.
    pub max_active_facts: Option<usize>,
    /// Maximum combined UTF-8 bytes across active facts.
    pub max_active_bytes: Option<usize>,
    /// Maximum retained fact rows, including candidates and history.
    pub max_total_facts: Option<usize>,
    /// Maximum retained fact value bytes, including candidates and history.
    pub max_total_bytes: Option<usize>,
    /// Maximum facts returned by one recall operation.
    pub max_recall_results: Option<usize>,
    /// SQLite busy timeout in milliseconds.
    pub busy_timeout_ms: Option<u64>,
    /// Default fact lifetime in days. `0` disables implicit expiry.
    pub default_expiry_days: Option<u64>,
    /// Days to retain unverified agent candidates.
    pub candidate_retention_days: Option<u64>,
    /// Days to retain stale and retracted facts.
    pub stale_retention_days: Option<u64>,
    /// Days to retain superseded fact history.
    pub superseded_retention_days: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RubberDuckToml {
    /// `off`, `manual`, or `automatic`.
    pub mode: Option<String>,
    /// Explicit ee-owned model registry id. Mutually exclusive with external_agent_id.
    pub internal_model_id: Option<String>,
    /// Explicit configured ACP agent id. Mutually exclusive with internal_model_id.
    pub external_agent_id: Option<String>,
    pub max_calls: Option<usize>,
    pub max_context_bytes: Option<usize>,
    pub max_output_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
}

/// Raw `[agents.web_context]` definition. A user-global opaque provider secret
/// reference is resolved only when web search service construction needs it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentWebContextToml {
    /// Enables optional web retrieval. Defaults to `false`.
    pub enabled: Option<bool>,
    /// Selected trusted search provider.
    pub backend: Option<WebContextBackendToml>,
    /// HTTPS endpoint for SearXNG only.
    #[serde(alias = "search_endpoint")]
    pub endpoint: Option<String>,
    /// Exact hosts preapproved by user-global configuration.
    #[serde(alias = "preapproved_hosts")]
    pub hosts: Option<BTreeSet<String>>,
    /// Bounded response, text, search-result, redirect, timeout, and concurrency limits.
    pub limits: Option<WebContextLimitsToml>,
    /// Exact `secret://<name>` reference accepted only from user-global config.
    /// It remains opaque until lazy web-search service construction.
    pub provider_secret_reference: Option<String>,
    /// Cloudflare account id for Browser Run. Browser Run stays disabled unless
    /// this and `browser_run_api_token_reference` are configured user-globally.
    pub browser_run_account_id: Option<String>,
    /// Exact `secret://<name>` reference for Cloudflare Browser Run API token.
    pub browser_run_api_token_reference: Option<String>,
    /// Total Browser Run attempts, including initial request. Defaults to 3; max 5.
    pub browser_run_max_attempts: Option<u8>,
    /// Exponential fallback delay before first retry. Defaults to 500 ms.
    pub browser_run_base_delay_ms: Option<u64>,
    /// Cap for Retry-After and exponential retry delay. Defaults to 10_000 ms.
    pub browser_run_max_delay_ms: Option<u64>,
    /// Exa-specific search options. Valid only when `backend = "exa"`.
    pub exa: Option<ExaSearchOptionsToml>,
    /// Brave LLM Context-specific search options. Valid only when
    /// `backend = "brave_llm_context"`.
    pub brave_llm_context: Option<BraveLlmContextOptionsToml>,
    /// Tavily-specific search options. Valid only when `backend = "tavily"`.
    pub tavily: Option<TavilySearchOptionsToml>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WebContextBackendToml {
    #[default]
    Searxng,
    Exa,
    BraveLlmContext,
    Tavily,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExaSearchOptionsToml {
    /// Maximum results. Defaults to bounded host result limit.
    pub max_results: Option<usize>,
    /// Exa search mode. Defaults to `auto`; result highlights remain enabled by adapter policy.
    pub search_mode: Option<ExaSearchModeToml>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExaSearchModeToml {
    #[default]
    Auto,
    Neural,
    Fast,
}

#[cfg(any(feature = "agents", test))]
impl From<ExaSearchModeToml> for ExaSearchMode {
    fn from(value: ExaSearchModeToml) -> Self {
        match value {
            ExaSearchModeToml::Auto => Self::Auto,
            ExaSearchModeToml::Neural => Self::Neural,
            ExaSearchModeToml::Fast => Self::Fast,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TavilySearchOptionsToml {
    /// Maximum results. Defaults to bounded host result limit.
    pub max_results: Option<usize>,
    /// Chunks per source. Defaults to `3` and cannot exceed it.
    pub chunks_per_source: Option<usize>,
    /// Search depth. Defaults to `advanced`.
    pub search_depth: Option<TavilySearchDepthToml>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TavilySearchDepthToml {
    #[default]
    Basic,
    Advanced,
}

#[cfg(any(feature = "agents", test))]
impl From<TavilySearchDepthToml> for TavilySearchDepth {
    fn from(value: TavilySearchDepthToml) -> Self {
        match value {
            TavilySearchDepthToml::Basic => Self::Basic,
            TavilySearchDepthToml::Advanced => Self::Advanced,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BraveLlmContextOptionsToml {
    /// Bounded cited result count.
    pub max_results: Option<usize>,
    /// Bounded grounding token budget, clamped to host text limit.
    pub max_tokens: Option<usize>,
    /// Bounded cited URL count.
    pub max_urls: Option<usize>,
    /// Bounded grounding snippet count.
    pub max_snippets: Option<usize>,
    pub threshold_mode: Option<BraveThresholdModeToml>,
    pub freshness: Option<BraveFreshnessToml>,
    /// Safe-search mode. Local recall is deliberately unavailable.
    pub safe_search: Option<BraveSafeSearchModeToml>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BraveThresholdModeToml {
    #[default]
    Balanced,
    Strict,
}

#[cfg(any(feature = "agents", test))]
impl From<BraveThresholdModeToml> for BraveThresholdMode {
    fn from(value: BraveThresholdModeToml) -> Self {
        match value {
            BraveThresholdModeToml::Balanced => Self::Balanced,
            BraveThresholdModeToml::Strict => Self::Strict,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BraveFreshnessToml {
    #[default]
    Any,
    Day,
    Week,
    Month,
}

#[cfg(any(feature = "agents", test))]
impl From<BraveFreshnessToml> for BraveFreshness {
    fn from(value: BraveFreshnessToml) -> Self {
        match value {
            BraveFreshnessToml::Any => Self::Any,
            BraveFreshnessToml::Day => Self::Day,
            BraveFreshnessToml::Week => Self::Week,
            BraveFreshnessToml::Month => Self::Month,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BraveSafeSearchModeToml {
    Off,
    #[default]
    Moderate,
    Strict,
}

#[cfg(any(feature = "agents", test))]
impl From<BraveSafeSearchModeToml> for BraveSafeSearchMode {
    fn from(value: BraveSafeSearchModeToml) -> Self {
        match value {
            BraveSafeSearchModeToml::Off => Self::Off,
            BraveSafeSearchModeToml::Moderate => Self::Moderate,
            BraveSafeSearchModeToml::Strict => Self::Strict,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebContextLimitsToml {
    pub max_response_bytes: Option<usize>,
    pub max_text_bytes: Option<usize>,
    pub max_search_results: Option<usize>,
    pub max_redirects: Option<usize>,
    /// Total request timeout in milliseconds.
    pub request_timeout_ms: Option<u64>,
    /// Maximum simultaneous web requests.
    pub max_concurrent_requests: Option<usize>,
}

/// Raw `[agents.servers.<id>]` definition.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentServerToml {
    /// Optional display label. Never used as subprocess identity.
    pub label: Option<String>,
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
        let Some(path) = file_path else {
            return self.cwd.clone();
        };
        let path = if path.is_absolute() { path.to_path_buf() } else { self.cwd.join(path) };
        if path.is_dir() {
            path
        } else {
            path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.cwd.clone())
        }
    }

    pub(crate) fn xdg_user_config_path(&self) -> Option<PathBuf> {
        self.config_dir.as_ref().map(|dir| dir.join("ee").join("config.toml"))
    }

    pub(crate) fn legacy_user_config_path(&self) -> Option<PathBuf> {
        self.home_dir.as_ref().map(|home| home.join(".ee.toml"))
    }

    fn workspace_candidate_paths(&self, file_path: Option<&Path>) -> Vec<PathBuf> {
        let legacy_user_path = self.legacy_user_config_path();
        let mut candidates = Vec::new();
        let mut dir = self.anchor_dir(file_path);
        loop {
            let candidate = dir.join(".ee.toml");
            if legacy_user_path.as_ref() != Some(&candidate) {
                candidates.push(candidate);
            }
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
        if let Some(limit) = patch.max_concurrent_prompts {
            if (1..=MAX_AGENT_MAX_CONCURRENT_PROMPTS).contains(&limit) {
                self.agents.max_concurrent_prompts = limit;
            } else {
                eprintln!(
                    "ee: warning: agents.max_concurrent_prompts must be between 1 and {MAX_AGENT_MAX_CONCURRENT_PROMPTS}; keeping {}",
                    self.agents.max_concurrent_prompts
                );
            }
        }
        if let Some(rubber_duck) = &patch.rubber_duck {
            match merge_rubber_duck(&self.agents.rubber_duck, rubber_duck) {
                Ok(resolved) => self.agents.rubber_duck = resolved,
                Err(error) => eprintln!("ee: warning: invalid agents.rubber_duck: {error}"),
            }
        }
        if let Some(workspace_memory) = &patch.workspace_memory {
            merge_workspace_memory(&mut self.agents.workspace_memory, workspace_memory);
        }
        for (id, server) in &patch.servers {
            let existing = self.agents.servers.get(id);
            match merge_agent_server(id, server, existing, kind) {
                Ok(resolved) => {
                    self.agents.servers.insert(id.clone(), resolved);
                }
                Err(err) => eprintln!("ee: warning: invalid agents server `{id}`: {err}"),
            }
        }
    }

    fn finalize_agents(&mut self) {
        self.agents.servers.retain(|id, server| {
            if server.command.trim().is_empty() {
                eprintln!(
                    "ee: warning: invalid agents server `{id}`: agent server command must not be empty"
                );
                return false;
            }
            true
        });
        #[cfg(any(feature = "agents", test))]
        {
            // External ids are frontend-known. Internal model existence is
            // process-owned and revalidated by ee-owned provider registry.
            let model_ids =
                self.agents.rubber_duck.internal_model_id.iter().cloned().collect::<BTreeSet<_>>();
            let agent_ids = self.agents.servers.keys().cloned().collect::<BTreeSet<_>>();
            match self.agents.rubber_duck.resolve_backend_policy(&model_ids, &agent_ids) {
                Ok(resolved) if resolved.unavailable.is_some() => eprintln!(
                    "ee: warning: configured rubber duck backend is unavailable; ordinary agent operation remains enabled"
                ),
                Ok(_) => {}
                Err(error) => eprintln!("ee: warning: invalid agents.rubber_duck: {error}"),
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

fn merge_rubber_duck(
    existing: &RubberDuckSettings,
    patch: &RubberDuckToml,
) -> Result<RubberDuckSettings, String> {
    validate_rubber_duck_toml(patch)?;
    let mut resolved = existing.clone();
    if let Some(mode) = patch.mode.as_deref() {
        resolved.mode = match mode {
            "off" => RubberDuckModeSetting::Off,
            "manual" => RubberDuckModeSetting::Manual,
            "automatic" => RubberDuckModeSetting::Automatic,
            _ => return Err(String::from("mode must be off, manual, or automatic")),
        };
    }
    if let Some(model_id) = &patch.internal_model_id {
        resolved.internal_model_id = Some(model_id.clone());
    }
    if let Some(agent_id) = &patch.external_agent_id {
        resolved.external_agent_id = Some(agent_id.clone());
    }
    if resolved.internal_model_id.is_some() && resolved.external_agent_id.is_some() {
        return Err(String::from("internal_model_id and external_agent_id are mutually exclusive"));
    }
    if let Some(value) = patch.max_calls {
        resolved.max_calls = value;
    }
    if let Some(value) = patch.max_context_bytes {
        resolved.max_context_bytes = value;
    }
    if let Some(value) = patch.max_output_bytes {
        resolved.max_output_bytes = value;
    }
    if let Some(value) = patch.timeout_ms {
        resolved.timeout_ms = value;
    }
    Ok(resolved)
}

fn validate_rubber_duck_toml(config: &RubberDuckToml) -> Result<(), String> {
    if config.internal_model_id.is_some() && config.external_agent_id.is_some() {
        return Err(String::from("internal_model_id and external_agent_id are mutually exclusive"));
    }
    for (field, value) in [
        ("internal_model_id", config.internal_model_id.as_deref()),
        ("external_agent_id", config.external_agent_id.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(format!("{field} must not be empty"));
        }
    }
    if let Some(value) = config.max_calls
        && !(1..=MAX_CRITIC_MAX_CALLS).contains(&value)
    {
        return Err(format!("max_calls must be between 1 and {MAX_CRITIC_MAX_CALLS}"));
    }
    for (field, value, max) in [
        ("max_context_bytes", config.max_context_bytes, MAX_CRITIC_CONTEXT_BYTES),
        ("max_output_bytes", config.max_output_bytes, MAX_CRITIC_OUTPUT_BYTES),
    ] {
        if value.is_some_and(|value| value == 0 || value > max) {
            return Err(format!("{field} must be between 1 and {max}"));
        }
    }
    if config.timeout_ms.is_some_and(|value| value == 0 || value > MAX_CRITIC_TIMEOUT_MS) {
        return Err(format!("timeout_ms must be between 1 and {MAX_CRITIC_TIMEOUT_MS}"));
    }
    Ok(())
}

fn validate_agent_server(id: &str, server: &AgentServerToml) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(String::from("agent server id must not be empty"));
    }
    if server.command.as_deref().is_some_and(|command| command.trim().is_empty()) {
        return Err(String::from("agent server command must not be empty"));
    }
    if server.label.as_deref().is_some_and(|label| label.trim().is_empty()) {
        return Err(String::from("agent server label must not be empty"));
    }
    for (key, value) in &server.env {
        if crate::secrets::is_secret_reference_text(value) {
            crate::secrets::SecretReference::parse(value).map_err(|err| {
                format!("invalid secret reference in agents.servers.{id}.env.{key}: {err}")
            })?;
        }
    }
    Ok(())
}

fn merge_agent_server(
    id: &str,
    server: &AgentServerToml,
    existing: Option<&AgentServerSettings>,
    kind: ConfigLayerKind,
) -> Result<AgentServerSettings, String> {
    validate_agent_server(id, server)?;

    let command = server
        .command
        .as_deref()
        .map(str::trim)
        .map(str::to_owned)
        .or_else(|| existing.map(|server| server.command.clone()))
        .unwrap_or_default();
    let args = server
        .args
        .clone()
        .or_else(|| existing.map(|server| server.args.clone()))
        .unwrap_or_default();
    let mut env = existing.map(|server| server.env.clone()).unwrap_or_default();
    for (key, value) in &server.env {
        if crate::secrets::is_secret_reference_text(value)
            && !matches!(kind, ConfigLayerKind::UserXdg | ConfigLayerKind::UserLegacy)
        {
            return Err(format!(
                "secret references are only allowed in user config layers, \
                 but agents.servers.{id}.env.{key} comes from {} config",
                kind.label()
            ));
        }
        env.insert(key.clone(), AgentEnvValue { layer: kind, raw: value.clone() });
    }

    Ok(AgentServerSettings {
        label: server
            .label
            .as_deref()
            .map(str::trim)
            .map(str::to_owned)
            .or_else(|| existing.and_then(|server| server.label.clone())),
        command,
        args,
        env,
        cwd: server.cwd.clone().or_else(|| existing.and_then(|server| server.cwd.clone())),
    })
}

fn validate_agent_web_context_config(web_context: &AgentWebContextToml) -> Result<(), String> {
    if let Some(reference) = &web_context.provider_secret_reference {
        crate::secrets::SecretReference::parse(reference).map_err(|_| {
            String::from("invalid secret reference in agents.web_context.provider_secret_reference")
        })?;
    }
    if let Some(reference) = &web_context.browser_run_api_token_reference {
        crate::secrets::SecretReference::parse(reference).map_err(|_| {
            String::from(
                "invalid secret reference in agents.web_context.browser_run_api_token_reference",
            )
        })?;
    }
    if let Some(account_id) = &web_context.browser_run_account_id
        && !(account_id.len() == 32 && account_id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(String::from(
            "agents.web_context.browser_run_account_id must be a 32-character hexadecimal Cloudflare account id",
        ));
    }
    let max_attempts = web_context.browser_run_max_attempts.unwrap_or(3);
    let base_delay_ms = web_context.browser_run_base_delay_ms.unwrap_or(500);
    let max_delay_ms = web_context.browser_run_max_delay_ms.unwrap_or(10_000);
    if max_attempts == 0 || max_attempts > 5 {
        return Err(String::from(
            "agents.web_context.browser_run_max_attempts must be 1 through 5",
        ));
    }
    if base_delay_ms == 0
        || base_delay_ms > 30_000
        || max_delay_ms == 0
        || max_delay_ms > 30_000
        || base_delay_ms > max_delay_ms
    {
        return Err(String::from(
            "Browser Run retry delay values must be within 1 through 30000 ms and base must not exceed max",
        ));
    }

    match web_context.backend {
        Some(WebContextBackendToml::Searxng)
            if web_context
                .endpoint
                .as_deref()
                .is_none_or(|endpoint| endpoint.trim().is_empty()) =>
        {
            return Err(String::from(
                "agents.web_context.endpoint is required when backend is searxng",
            ));
        }
        Some(
            WebContextBackendToml::Exa
            | WebContextBackendToml::BraveLlmContext
            | WebContextBackendToml::Tavily,
        ) if web_context.endpoint.is_some() => {
            return Err(String::from(
                "agents.web_context.endpoint is only permitted when backend is searxng",
            ));
        }
        None if web_context.endpoint.is_some() => {
            return Err(String::from(
                "agents.web_context.endpoint is only permitted when backend is searxng",
            ));
        }
        _ => {}
    }

    validate_web_context_provider_options(
        "exa",
        web_context.exa.is_some(),
        web_context.backend,
        WebContextBackendToml::Exa,
    )?;
    validate_web_context_provider_options(
        "brave_llm_context",
        web_context.brave_llm_context.is_some(),
        web_context.backend,
        WebContextBackendToml::BraveLlmContext,
    )?;
    validate_web_context_provider_options(
        "tavily",
        web_context.tavily.is_some(),
        web_context.backend,
        WebContextBackendToml::Tavily,
    )?;
    if let Some(exa) = &web_context.exa {
        validate_web_context_provider_limit("exa.max_results", exa.max_results, MAX_EXA_RESULTS)?;
    }
    if let Some(tavily) = &web_context.tavily {
        validate_web_context_provider_limit(
            "tavily.max_results",
            tavily.max_results,
            MAX_TAVILY_RESULTS,
        )?;
        validate_web_context_provider_limit(
            "tavily.chunks_per_source",
            tavily.chunks_per_source,
            MAX_TAVILY_CHUNKS_PER_SOURCE,
        )?;
    }
    if let Some(brave) = &web_context.brave_llm_context {
        validate_web_context_provider_limit(
            "brave_llm_context.max_results",
            brave.max_results,
            MAX_BRAVE_RESULTS,
        )?;
        validate_web_context_provider_limit(
            "brave_llm_context.max_tokens",
            brave.max_tokens,
            MAX_BRAVE_TOKENS,
        )?;
        validate_web_context_provider_limit(
            "brave_llm_context.max_urls",
            brave.max_urls,
            MAX_BRAVE_URLS,
        )?;
        validate_web_context_provider_limit(
            "brave_llm_context.max_snippets",
            brave.max_snippets,
            MAX_BRAVE_SNIPPETS,
        )?;
    }

    if let Some(limits) = &web_context.limits {
        validate_web_context_raw_limit("request_timeout_ms", limits.request_timeout_ms)?;
        validate_web_context_raw_limit(
            "max_concurrent_requests",
            limits.max_concurrent_requests.map(|value| value as u64),
        )?;
    }
    Ok(())
}

fn validate_web_context_provider_options(
    name: &str,
    configured: bool,
    backend: Option<WebContextBackendToml>,
    expected_backend: WebContextBackendToml,
) -> Result<(), String> {
    if configured && backend != Some(expected_backend) {
        return Err(format!(
            "agents.web_context.{name} is only permitted when backend is {}",
            web_context_backend_name(expected_backend),
        ));
    }
    Ok(())
}

fn web_context_backend_name(backend: WebContextBackendToml) -> &'static str {
    match backend {
        WebContextBackendToml::Searxng => "searxng",
        WebContextBackendToml::Exa => "exa",
        WebContextBackendToml::BraveLlmContext => "brave_llm_context",
        WebContextBackendToml::Tavily => "tavily",
    }
}

fn validate_web_context_raw_limit(name: &str, value: Option<u64>) -> Result<(), String> {
    if value == Some(0) {
        return Err(format!("agents.web_context.limits.{name} must be greater than zero"));
    }
    Ok(())
}

fn validate_web_context_provider_limit(
    name: &str,
    value: Option<usize>,
    maximum: usize,
) -> Result<(), String> {
    if value.is_some_and(|value| value == 0 || value > maximum) {
        return Err(format!("agents.web_context.{name} must be within supported bounds"));
    }
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn resolve_agent_web_context_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> AgentWebContextConfig {
    let mut config = AgentWebContextConfig::default();

    if let Some(web_context) = user_global_agent_web_context_toml(env)
        && let Err(err) = merge_user_global_web_context(&mut config, &web_context)
    {
        eprintln!("ee: warning: invalid user-global agents.web_context config: {err}");
    }

    for layer in discover_config_layers_with_env(env, file_path)
        .layers
        .into_iter()
        .filter(|layer| matches!(layer.kind, ConfigLayerKind::Ancestor))
    {
        let Some(patch) = parse_ee_toml(&layer.path) else {
            continue;
        };
        let Some(web_context) = patch.agents.and_then(|agents| agents.web_context) else {
            continue;
        };
        restrict_workspace_web_context(&mut config, &web_context, &layer.path);
    }

    config
}

#[cfg(any(feature = "agents", test))]
fn user_global_agent_web_context_toml(env: &ConfigEnvironment) -> Option<AgentWebContextToml> {
    let user_config = env
        .xdg_user_config_path()
        .filter(|path| probe_config_file(path).exists)
        .or_else(|| env.legacy_user_config_path().filter(|path| probe_config_file(path).exists))?;
    parse_ee_toml(&user_config)?.agents?.web_context
}

#[cfg(any(feature = "agents", test))]
fn merge_user_global_web_context(
    config: &mut AgentWebContextConfig,
    patch: &AgentWebContextToml,
) -> Result<(), String> {
    validate_agent_web_context_config(patch)?;

    let mut limits = config.limits.clone();
    apply_user_global_web_context_limits(&mut limits, patch.limits.as_ref())?;

    if let Some(enabled) = patch.enabled {
        config.enabled = enabled;
    }
    if let Some(backend) = patch.backend {
        config.provider = web_search_provider(backend);
        config.provider_options = web_search_provider_options(backend, patch);
        config.search_endpoint = patch.endpoint.clone();
    }
    if let Some(hosts) = &patch.hosts {
        config.preapproved_hosts = hosts.clone();
    }
    if let Some(reference) = &patch.provider_secret_reference {
        config.provider_secret_reference = Some(reference.clone());
    }
    if let Some(account_id) = &patch.browser_run_account_id {
        config.browser_run_account_id = Some(account_id.clone());
    }
    if let Some(reference) = &patch.browser_run_api_token_reference {
        config.browser_run_api_token_reference = Some(reference.clone());
    }
    if let Some(max_attempts) = patch.browser_run_max_attempts {
        config.browser_run_retry.max_attempts = max_attempts;
    }
    if let Some(base_delay_ms) = patch.browser_run_base_delay_ms {
        config.browser_run_retry.base_delay_ms = base_delay_ms;
    }
    if let Some(max_delay_ms) = patch.browser_run_max_delay_ms {
        config.browser_run_retry.max_delay_ms = max_delay_ms;
    }
    config.limits = limits;
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn web_search_provider(backend: WebContextBackendToml) -> WebSearchProvider {
    match backend {
        WebContextBackendToml::Searxng => WebSearchProvider::Searxng,
        WebContextBackendToml::Exa => WebSearchProvider::Exa,
        WebContextBackendToml::BraveLlmContext => WebSearchProvider::BraveLlmContext,
        WebContextBackendToml::Tavily => WebSearchProvider::Tavily,
    }
}

#[cfg(any(feature = "agents", test))]
fn web_search_provider_options(
    backend: WebContextBackendToml,
    patch: &AgentWebContextToml,
) -> WebSearchProviderOptions {
    match backend {
        WebContextBackendToml::Searxng => WebSearchProviderOptions::Searxng,
        WebContextBackendToml::Exa => {
            let mut options = ExaSearchOptions::default();
            if let Some(exa) = &patch.exa {
                if let Some(max_results) = exa.max_results {
                    options.max_results = max_results;
                }
                if let Some(search_mode) = exa.search_mode {
                    options.search_mode = search_mode.into();
                }
            }
            WebSearchProviderOptions::Exa(options)
        }
        WebContextBackendToml::BraveLlmContext => {
            let mut options = BraveLlmContextOptions::default();
            if let Some(brave) = &patch.brave_llm_context {
                if let Some(max_results) = brave.max_results {
                    options.max_results = max_results;
                }
                if let Some(max_tokens) = brave.max_tokens {
                    options.max_tokens = max_tokens;
                }
                if let Some(max_urls) = brave.max_urls {
                    options.max_urls = max_urls;
                }
                if let Some(max_snippets) = brave.max_snippets {
                    options.max_snippets = max_snippets;
                }
                if let Some(threshold_mode) = brave.threshold_mode {
                    options.threshold_mode = threshold_mode.into();
                }
                if let Some(freshness) = brave.freshness {
                    options.freshness = freshness.into();
                }
                if let Some(safe_search) = brave.safe_search {
                    options.safe_search = safe_search.into();
                }
            }
            WebSearchProviderOptions::BraveLlmContext(options)
        }
        WebContextBackendToml::Tavily => {
            let mut options = TavilySearchOptions::default();
            if let Some(tavily) = &patch.tavily {
                if let Some(max_results) = tavily.max_results {
                    options.max_results = max_results;
                }
                if let Some(chunks_per_source) = tavily.chunks_per_source {
                    options.chunks_per_source = chunks_per_source;
                }
                if let Some(search_depth) = tavily.search_depth {
                    options.search_depth = search_depth.into();
                }
            }
            WebSearchProviderOptions::Tavily(options)
        }
    }
}

#[cfg(any(feature = "agents", test))]
fn apply_user_global_web_context_limits(
    limits: &mut WebContextLimits,
    patch: Option<&WebContextLimitsToml>,
) -> Result<(), String> {
    let Some(patch) = patch else {
        return Ok(());
    };

    if let Some(value) = patch.max_response_bytes {
        validate_web_context_limit(
            "max_response_bytes",
            value,
            ee_agent_host::web_context::MAX_RESPONSE_BYTES,
            false,
        )?;
        limits.max_response_bytes = value;
    }
    if let Some(value) = patch.max_text_bytes {
        validate_web_context_limit(
            "max_text_bytes",
            value,
            ee_agent_host::web_context::MAX_TEXT_BYTES,
            false,
        )?;
        limits.max_text_bytes = value;
    }
    if let Some(value) = patch.max_search_results {
        validate_web_context_limit(
            "max_search_results",
            value,
            ee_agent_host::web_context::MAX_SEARCH_RESULTS,
            false,
        )?;
        limits.max_search_results = value;
    }
    if let Some(value) = patch.max_redirects {
        validate_web_context_limit(
            "max_redirects",
            value,
            ee_agent_host::web_context::MAX_REDIRECTS,
            true,
        )?;
        limits.max_redirects = value;
    }
    if let Some(value) = patch.request_timeout_ms {
        if value == 0 || value > ee_agent_host::web_context::MAX_REQUEST_TIMEOUT_MS {
            return Err(String::from("request_timeout_ms must be within supported bounds"));
        }
        limits.request_timeout_ms = value;
    }
    if let Some(value) = patch.max_concurrent_requests {
        validate_web_context_limit(
            "max_concurrent_requests",
            value,
            ee_agent_host::web_context::MAX_CONCURRENT_REQUESTS,
            false,
        )?;
        limits.max_concurrent_requests = value;
    }

    if limits.max_text_bytes > limits.max_response_bytes {
        limits.max_text_bytes = limits.max_response_bytes;
    }
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn validate_web_context_limit(
    name: &str,
    value: usize,
    maximum: usize,
    zero_allowed: bool,
) -> Result<(), String> {
    if (!zero_allowed && value == 0) || value > maximum {
        return Err(format!("{name} must be within supported bounds"));
    }
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn restrict_workspace_web_context(
    config: &mut AgentWebContextConfig,
    patch: &AgentWebContextToml,
    path: &Path,
) {
    if patch.enabled == Some(true) {
        eprintln!(
            "ee: warning: ignoring agents.web_context.enabled = true in workspace config {}",
            path.display()
        );
    }
    if patch.enabled == Some(false) {
        config.enabled = false;
    }
    if patch.backend.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context.backend in workspace config {}",
            path.display()
        );
    }
    if patch.exa.is_some() || patch.brave_llm_context.is_some() || patch.tavily.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context provider options in workspace config {}",
            path.display()
        );
    }
    if patch.endpoint.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context.endpoint in workspace config {}",
            path.display()
        );
    }
    if patch.provider_secret_reference.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context.provider_secret_reference outside user-global config in {}",
            path.display()
        );
    }
    if patch.browser_run_account_id.is_some()
        || patch.browser_run_api_token_reference.is_some()
        || patch.browser_run_max_attempts.is_some()
        || patch.browser_run_base_delay_ms.is_some()
        || patch.browser_run_max_delay_ms.is_some()
    {
        eprintln!(
            "ee: warning: ignoring agents.web_context Browser Run configuration outside user-global config in {}",
            path.display()
        );
    }
    if let Some(hosts) = &patch.hosts {
        if !hosts.is_subset(&config.preapproved_hosts) {
            eprintln!(
                "ee: warning: ignoring workspace agents.web_context hosts not approved by user-global config in {}",
                path.display()
            );
        }
        config.preapproved_hosts = config.preapproved_hosts.intersection(hosts).cloned().collect();
    }
    restrict_workspace_web_context_limits(&mut config.limits, patch.limits.as_ref(), path);
}

#[cfg(any(feature = "agents", test))]
fn restrict_workspace_web_context_limits(
    limits: &mut WebContextLimits,
    patch: Option<&WebContextLimitsToml>,
    path: &Path,
) {
    let Some(patch) = patch else {
        return;
    };

    restrict_web_context_limit(
        "max_response_bytes",
        &mut limits.max_response_bytes,
        patch.max_response_bytes,
        false,
        path,
    );
    restrict_web_context_limit(
        "max_text_bytes",
        &mut limits.max_text_bytes,
        patch.max_text_bytes,
        false,
        path,
    );
    restrict_web_context_limit(
        "max_search_results",
        &mut limits.max_search_results,
        patch.max_search_results,
        false,
        path,
    );
    restrict_web_context_limit(
        "max_redirects",
        &mut limits.max_redirects,
        patch.max_redirects,
        true,
        path,
    );
    restrict_web_context_u64_limit(
        "request_timeout_ms",
        &mut limits.request_timeout_ms,
        patch.request_timeout_ms,
        path,
    );
    restrict_web_context_limit(
        "max_concurrent_requests",
        &mut limits.max_concurrent_requests,
        patch.max_concurrent_requests,
        false,
        path,
    );
    limits.max_text_bytes = limits.max_text_bytes.min(limits.max_response_bytes);
}

#[cfg(any(feature = "agents", test))]
fn restrict_web_context_limit(
    name: &str,
    current: &mut usize,
    requested: Option<usize>,
    zero_allowed: bool,
    path: &Path,
) {
    let Some(requested) = requested else {
        return;
    };
    if !zero_allowed && requested == 0 {
        eprintln!(
            "ee: warning: ignoring invalid workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else if requested > *current {
        eprintln!(
            "ee: warning: ignoring widening workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else {
        *current = requested;
    }
}

#[cfg(any(feature = "agents", test))]
fn restrict_web_context_u64_limit(
    name: &str,
    current: &mut u64,
    requested: Option<u64>,
    path: &Path,
) {
    let Some(requested) = requested else {
        return;
    };
    if requested == 0 {
        eprintln!(
            "ee: warning: ignoring invalid workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else if requested > *current {
        eprintln!(
            "ee: warning: ignoring widening workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else {
        *current = requested;
    }
}

#[cfg(any(feature = "agents", test))]
fn agent_web_context_settings_to_toml(
    web_context: &AgentWebContextConfig,
) -> Option<AgentWebContextToml> {
    if web_context == &AgentWebContextConfig::default() {
        return None;
    }
    let backend = match web_context.provider {
        WebSearchProvider::Searxng => WebContextBackendToml::Searxng,
        WebSearchProvider::Exa => WebContextBackendToml::Exa,
        WebSearchProvider::BraveLlmContext => WebContextBackendToml::BraveLlmContext,
        WebSearchProvider::Tavily => WebContextBackendToml::Tavily,
    };
    let (exa, brave_llm_context, tavily) = match &web_context.provider_options {
        WebSearchProviderOptions::Searxng => (None, None, None),
        WebSearchProviderOptions::Exa(options) => (
            Some(ExaSearchOptionsToml {
                max_results: Some(options.max_results),
                search_mode: Some(match options.search_mode {
                    ExaSearchMode::Auto => ExaSearchModeToml::Auto,
                    ExaSearchMode::Neural => ExaSearchModeToml::Neural,
                    ExaSearchMode::Fast => ExaSearchModeToml::Fast,
                }),
            }),
            None,
            None,
        ),
        WebSearchProviderOptions::BraveLlmContext(options) => (
            None,
            Some(BraveLlmContextOptionsToml {
                max_results: Some(options.max_results),
                max_tokens: Some(options.max_tokens),
                max_urls: Some(options.max_urls),
                max_snippets: Some(options.max_snippets),
                threshold_mode: Some(match options.threshold_mode {
                    BraveThresholdMode::Balanced => BraveThresholdModeToml::Balanced,
                    BraveThresholdMode::Strict => BraveThresholdModeToml::Strict,
                }),
                freshness: Some(match options.freshness {
                    BraveFreshness::Any => BraveFreshnessToml::Any,
                    BraveFreshness::Day => BraveFreshnessToml::Day,
                    BraveFreshness::Week => BraveFreshnessToml::Week,
                    BraveFreshness::Month => BraveFreshnessToml::Month,
                }),
                safe_search: Some(match options.safe_search {
                    BraveSafeSearchMode::Off => BraveSafeSearchModeToml::Off,
                    BraveSafeSearchMode::Moderate => BraveSafeSearchModeToml::Moderate,
                    BraveSafeSearchMode::Strict => BraveSafeSearchModeToml::Strict,
                }),
            }),
            None,
        ),
        WebSearchProviderOptions::Tavily(options) => (
            None,
            None,
            Some(TavilySearchOptionsToml {
                max_results: Some(options.max_results),
                chunks_per_source: Some(options.chunks_per_source),
                search_depth: Some(match options.search_depth {
                    TavilySearchDepth::Basic => TavilySearchDepthToml::Basic,
                    TavilySearchDepth::Advanced => TavilySearchDepthToml::Advanced,
                }),
            }),
        ),
    };
    Some(AgentWebContextToml {
        enabled: Some(web_context.enabled),
        backend: Some(backend),
        endpoint: (backend == WebContextBackendToml::Searxng)
            .then(|| web_context.search_endpoint.clone())
            .flatten(),
        hosts: Some(web_context.preapproved_hosts.clone()),
        limits: Some(WebContextLimitsToml {
            max_response_bytes: Some(web_context.limits.max_response_bytes),
            max_text_bytes: Some(web_context.limits.max_text_bytes),
            max_search_results: Some(web_context.limits.max_search_results),
            max_redirects: Some(web_context.limits.max_redirects),
            request_timeout_ms: Some(web_context.limits.request_timeout_ms),
            max_concurrent_requests: Some(web_context.limits.max_concurrent_requests),
        }),
        provider_secret_reference: web_context.provider_secret_reference.clone(),
        browser_run_account_id: web_context.browser_run_account_id.clone(),
        browser_run_api_token_reference: web_context.browser_run_api_token_reference.clone(),
        browser_run_max_attempts: Some(web_context.browser_run_retry.max_attempts),
        browser_run_base_delay_ms: Some(web_context.browser_run_retry.base_delay_ms),
        browser_run_max_delay_ms: Some(web_context.browser_run_retry.max_delay_ms),
        exa,
        brave_llm_context,
        tavily,
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
    #[cfg(any(feature = "agents", test))]
    {
        settings.agents.web_context = resolve_agent_web_context_with_env(file_path, env);
    }
    settings.finalize_agents();

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

fn merge_workspace_memory(resolved: &mut WorkspaceMemorySettings, patch: &WorkspaceMemoryToml) {
    if let Some(enabled) = patch.enabled {
        resolved.enabled = enabled;
    }
    merge_bounded_usize(
        "agents.workspace_memory.max_value_bytes",
        &mut resolved.max_value_bytes,
        patch.max_value_bytes,
        MAX_WORKSPACE_MEMORY_VALUE_BYTES,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_active_facts",
        &mut resolved.max_active_facts,
        patch.max_active_facts,
        MAX_WORKSPACE_MEMORY_ACTIVE_FACTS,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_active_bytes",
        &mut resolved.max_active_bytes,
        patch.max_active_bytes,
        MAX_WORKSPACE_MEMORY_ACTIVE_BYTES,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_total_facts",
        &mut resolved.max_total_facts,
        patch.max_total_facts,
        MAX_WORKSPACE_MEMORY_TOTAL_FACTS,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_total_bytes",
        &mut resolved.max_total_bytes,
        patch.max_total_bytes,
        MAX_WORKSPACE_MEMORY_TOTAL_BYTES,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_recall_results",
        &mut resolved.max_recall_results,
        patch.max_recall_results,
        MAX_WORKSPACE_MEMORY_RECALL_RESULTS,
    );
    merge_retention_days(
        "agents.workspace_memory.default_expiry_days",
        &mut resolved.default_expiry_days,
        patch.default_expiry_days,
        true,
    );
    merge_retention_days(
        "agents.workspace_memory.candidate_retention_days",
        &mut resolved.candidate_retention_days,
        patch.candidate_retention_days,
        false,
    );
    merge_retention_days(
        "agents.workspace_memory.stale_retention_days",
        &mut resolved.stale_retention_days,
        patch.stale_retention_days,
        false,
    );
    merge_retention_days(
        "agents.workspace_memory.superseded_retention_days",
        &mut resolved.superseded_retention_days,
        patch.superseded_retention_days,
        false,
    );
    if let Some(value) = patch.busy_timeout_ms {
        if (1..=MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS).contains(&value) {
            resolved.busy_timeout_ms = value;
        } else {
            eprintln!(
                "ee: warning: agents.workspace_memory.busy_timeout_ms must be between 1 and {MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS}; keeping {}",
                resolved.busy_timeout_ms
            );
        }
    }
}

fn merge_retention_days(name: &str, resolved: &mut u64, patch: Option<u64>, allow_zero: bool) {
    let Some(value) = patch else { return };
    let minimum = u64::from(!allow_zero);
    if (minimum..=MAX_WORKSPACE_MEMORY_RETENTION_DAYS).contains(&value) {
        *resolved = value;
    } else {
        eprintln!(
            "ee: warning: {name} must be between {minimum} and {MAX_WORKSPACE_MEMORY_RETENTION_DAYS}; keeping {resolved}"
        );
    }
}

fn merge_bounded_usize(name: &str, resolved: &mut usize, patch: Option<usize>, max: usize) {
    let Some(value) = patch else { return };
    if (1..=max).contains(&value) {
        *resolved = value;
    } else {
        eprintln!("ee: warning: {name} must be between 1 and {max}; keeping {resolved}");
    }
}

fn agents_settings_to_toml(agents: &AgentsSettings) -> Option<AgentsToml> {
    Some(AgentsToml {
        enabled: Some(agents.enabled),
        default_agent: agents.default_agent.clone(),
        max_concurrent_prompts: Some(agents.max_concurrent_prompts),
        workspace_memory: Some(WorkspaceMemoryToml {
            enabled: Some(agents.workspace_memory.enabled),
            max_value_bytes: Some(agents.workspace_memory.max_value_bytes),
            max_active_facts: Some(agents.workspace_memory.max_active_facts),
            max_active_bytes: Some(agents.workspace_memory.max_active_bytes),
            max_total_facts: Some(agents.workspace_memory.max_total_facts),
            max_total_bytes: Some(agents.workspace_memory.max_total_bytes),
            max_recall_results: Some(agents.workspace_memory.max_recall_results),
            busy_timeout_ms: Some(agents.workspace_memory.busy_timeout_ms),
            default_expiry_days: Some(agents.workspace_memory.default_expiry_days),
            candidate_retention_days: Some(agents.workspace_memory.candidate_retention_days),
            stale_retention_days: Some(agents.workspace_memory.stale_retention_days),
            superseded_retention_days: Some(agents.workspace_memory.superseded_retention_days),
        }),
        rubber_duck: Some(RubberDuckToml {
            mode: Some(
                match agents.rubber_duck.mode {
                    RubberDuckModeSetting::Off => "off",
                    RubberDuckModeSetting::Manual => "manual",
                    RubberDuckModeSetting::Automatic => "automatic",
                }
                .into(),
            ),
            internal_model_id: agents.rubber_duck.internal_model_id.clone(),
            external_agent_id: agents.rubber_duck.external_agent_id.clone(),
            max_calls: Some(agents.rubber_duck.max_calls),
            max_context_bytes: Some(agents.rubber_duck.max_context_bytes),
            max_output_bytes: Some(agents.rubber_duck.max_output_bytes),
            timeout_ms: Some(agents.rubber_duck.timeout_ms),
        }),
        web_context: {
            #[cfg(any(feature = "agents", test))]
            {
                agent_web_context_settings_to_toml(&agents.web_context)
            }
            #[cfg(not(any(feature = "agents", test)))]
            {
                None
            }
        },
        servers: agents
            .servers
            .iter()
            .map(|(id, server)| {
                (
                    id.clone(),
                    AgentServerToml {
                        label: server.label.clone(),
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
fn validate_workspace_memory_toml(memory: &WorkspaceMemoryToml) -> Result<(), String> {
    for (name, value, max) in [
        ("max_value_bytes", memory.max_value_bytes, MAX_WORKSPACE_MEMORY_VALUE_BYTES),
        ("max_active_facts", memory.max_active_facts, MAX_WORKSPACE_MEMORY_ACTIVE_FACTS),
        ("max_active_bytes", memory.max_active_bytes, MAX_WORKSPACE_MEMORY_ACTIVE_BYTES),
        ("max_total_facts", memory.max_total_facts, MAX_WORKSPACE_MEMORY_TOTAL_FACTS),
        ("max_total_bytes", memory.max_total_bytes, MAX_WORKSPACE_MEMORY_TOTAL_BYTES),
        ("max_recall_results", memory.max_recall_results, MAX_WORKSPACE_MEMORY_RECALL_RESULTS),
    ] {
        if value.is_some_and(|value| !(1..=max).contains(&value)) {
            return Err(format!("agents.workspace_memory.{name} must be between 1 and {max}"));
        }
    }
    for (name, value, allow_zero) in [
        ("default_expiry_days", memory.default_expiry_days, true),
        ("candidate_retention_days", memory.candidate_retention_days, false),
        ("stale_retention_days", memory.stale_retention_days, false),
        ("superseded_retention_days", memory.superseded_retention_days, false),
    ] {
        let minimum = u64::from(!allow_zero);
        if value
            .is_some_and(|value| !(minimum..=MAX_WORKSPACE_MEMORY_RETENTION_DAYS).contains(&value))
        {
            return Err(format!(
                "agents.workspace_memory.{name} must be between {minimum} and {MAX_WORKSPACE_MEMORY_RETENTION_DAYS}"
            ));
        }
    }
    if memory
        .busy_timeout_ms
        .is_some_and(|value| !(1..=MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS).contains(&value))
    {
        return Err(format!(
            "agents.workspace_memory.busy_timeout_ms must be between 1 and {MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS}"
        ));
    }
    Ok(())
}

fn validate_agents_mcp_config(parsed: &EeToml) -> Result<(), String> {
    let mut effective_ids = BTreeSet::new();
    if let Some(agents) = &parsed.agents {
        if let Some(workspace_memory) = &agents.workspace_memory {
            validate_workspace_memory_toml(workspace_memory)?;
        }
        if let Some(web_context) = &agents.web_context {
            validate_agent_web_context_config(web_context)?;
        }
        if let Some(rubber_duck) = &agents.rubber_duck {
            validate_rubber_duck_toml(rubber_duck)?;
        }
        for (id, server) in &agents.servers {
            // Validation checks shape and reference grammar only; layer
            // provenance and required effective fields are enforced during
            // the merge, because this file may contain only a server patch.
            validate_agent_server(id, server)
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

#[cfg(feature = "agents")]
/// Writes one complete agent-server definition to the user config layer.
///
/// Agent setup is deliberately global: workspace config must never receive
/// machine-local executable paths or encrypted secret references.
pub(crate) fn configure_global_agent_server(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
) -> Result<PathBuf, String> {
    configure_global_agent_server_with_env(
        agent_id,
        command,
        args,
        env_values,
        &ConfigEnvironment::from_process(),
    )
}

#[cfg(feature = "agents")]
fn configure_global_agent_server_with_env(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    let command =
        command.to_str().ok_or_else(|| String::from("agent executable path is not valid UTF-8"))?;
    let path = config_path_for_scope_with_env(ConfigScope::Global, env)?;
    let mut document = parse_config_document(&path)?;
    let root = ensure_table(&mut document)?;
    let agents = match root
        .entry(String::from("agents"))
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
    {
        toml::Value::Table(table) => table,
        _ => return Err(String::from("config key `agents` already exists and is not table")),
    };
    agents.insert(String::from("enabled"), toml::Value::Boolean(true));
    agents.insert(String::from("default_agent"), toml::Value::String(agent_id.to_owned()));
    let servers = match agents
        .entry(String::from("servers"))
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
    {
        toml::Value::Table(table) => table,
        _ => {
            return Err(String::from(
                "config key `agents.servers` already exists and is not table",
            ));
        }
    };
    let mut server = toml::map::Map::new();
    server.insert(String::from("command"), toml::Value::String(command.to_owned()));
    server.insert(
        String::from("args"),
        toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
    );
    server.insert(
        String::from("env"),
        toml::Value::Table(
            env_values
                .iter()
                .map(|(name, value)| (name.clone(), toml::Value::String(value.clone())))
                .collect(),
        ),
    );
    servers.insert(agent_id.to_owned(), toml::Value::Table(server));

    let text = toml::to_string_pretty(&document)
        .map_err(|error| format!("cannot serialize config {}: {error}", path.display()))?;
    validate_config_contents(&path, &text)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    fs::write(&path, text).map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    Ok(path)
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
    configure_default_runtime_loader_overrides_if_changed(
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

/// Persists an explicit workspace-memory switch without reserializing unrelated
/// config, comments, ordering, or formatting.
pub(crate) fn persist_workspace_memory_enabled(path: &Path, enabled: bool) -> Result<(), String> {
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "workspace config must be a regular non-symlink file: {}",
                    path.display()
                ));
            }
            Some(
                fs::read_to_string(path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
            )
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    let contents = existing.as_deref().unwrap_or_default();
    let parsed: toml::Value = toml::from_str(contents).map_err(|error| {
        format!("refusing to modify invalid config {}: {error}", path.display())
    })?;
    let has_memory_table =
        parsed.get("agents").and_then(|agents| agents.get("workspace_memory")).is_some();

    let newline = if contents.contains("\r\n") { "\r\n" } else { "\n" };
    let mut section_start = None;
    let mut section_end = contents.len();
    let mut line_start = 0;
    for line in contents.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let trimmed = body.trim();
        if trimmed == "[agents.workspace_memory]" {
            section_start = Some(line_start + line.len());
        } else if section_start.is_some() && trimmed.starts_with('[') {
            section_end = line_start;
            break;
        }
        line_start += line.len();
    }
    if section_start.is_none() && has_memory_table {
        return Err(format!(
            "refusing to rewrite non-canonical [agents.workspace_memory] table in {}",
            path.display()
        ));
    }

    let value = if enabled { "true" } else { "false" };
    let updated = if let Some(section_start) = section_start {
        let mut enabled_line = None;
        let mut offset = section_start;
        for line in contents[section_start..section_end].split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let trimmed = body.trim_start();
            if trimmed
                .strip_prefix("enabled")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
            {
                enabled_line = Some((offset, offset + body.len(), body));
                break;
            }
            offset += line.len();
        }
        if let Some((start, end, body)) = enabled_line {
            let indent_len = body.len() - body.trim_start().len();
            let comment = body.find('#').map(|index| &body[index..]).unwrap_or_default();
            let comment = if comment.is_empty() { String::new() } else { format!(" {comment}") };
            format!(
                "{}{}enabled = {value}{comment}{}",
                &contents[..start],
                &body[..indent_len],
                &contents[end..]
            )
        } else {
            let separator = if section_start == 0 || contents[..section_start].ends_with('\n') {
                ""
            } else {
                newline
            };
            format!(
                "{}{separator}enabled = {value}{newline}{}",
                &contents[..section_start],
                &contents[section_start..]
            )
        }
    } else {
        let separator = if contents.is_empty() || contents.ends_with('\n') { "" } else { newline };
        let blank = if contents.is_empty() { "" } else { newline };
        format!(
            "{contents}{separator}{blank}[agents.workspace_memory]{newline}enabled = {value}{newline}"
        )
    };

    validate_config_contents(path, &updated)?;
    write_config_atomically(path, updated.as_bytes())
}

fn write_config_atomically(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("config.toml");
    let mut temporary = None;
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), attempt));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "cannot create temporary config in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    let (temporary_path, mut file) = temporary
        .ok_or_else(|| format!("cannot allocate temporary config in {}", parent.display()))?;
    let result = (|| {
        file.write_all(contents)
            .map_err(|error| format!("cannot write {}: {error}", temporary_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync {}: {error}", temporary_path.display()))?;
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temporary_path, metadata.permissions()).map_err(|error| {
                format!("cannot preserve permissions for {}: {error}", path.display())
            })?;
        }
        fs::rename(&temporary_path, path)
            .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
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
    fn legacy_user_config_is_not_misclassified_as_workspace_config() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let mut env = test_config_environment(temp.path());
        env.home_dir = Some(home.clone());
        env.cwd = home.join("project");
        std::fs::create_dir_all(&env.cwd).unwrap();
        std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
        std::fs::write(
            home.join(".ee.toml"),
            "[languages.yaml]\nfile_types = [\"yaml\", \"yml\"]\n",
        )
        .unwrap();
        std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "wrap_lines = true\n",
        )
        .unwrap();

        let runtime = runtime_languages_with_env(None, &env);
        let layers = discover_config_layers_with_env(&env, None).layers;

        assert!(!runtime.workspace_overrides.contains_key("yaml"));
        assert_eq!(
            layer_paths(&layers),
            vec![env.config_dir.as_ref().unwrap().join("ee").join("config.toml")]
        );
    }

    #[test]
    fn relative_file_path_discovers_workspace_config_from_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let file = env.cwd.join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();
        std::fs::write(env.cwd.join(".ee.toml"), "cursor_line = true\n").unwrap();

        let layers = discover_config_layers_with_env(&env, Some(Path::new("src/main.rs"))).layers;

        assert!(layers.iter().any(|layer| {
            layer.kind == ConfigLayerKind::Ancestor && layer.path == env.cwd.join(".ee.toml")
        }));
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
        assert!(
            contents
                .contains("backend = \"searxng\" # or \"exa\", \"brave_llm_context\", \"tavily\"")
        );
        assert!(contents.contains("provider_secret_reference = \"secret://web-search-api-key\""));
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
        assert_eq!(settings.agents.max_concurrent_prompts, 4);
        assert!(settings.agents.servers.is_empty());
        assert!(settings.mcp.servers.is_empty());
    }

    #[test]
    fn ee_toml_parses_agents_settings() {
        let toml = r#"
[agents]
enabled = true
default_agent = "helper"
max_concurrent_prompts = 2

[agents.servers.helper]
label = "Local Helper"
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
        assert_eq!(settings.agents.max_concurrent_prompts, 2);
        let helper = settings.agents.servers.get("helper").unwrap();
        assert_eq!(helper.label.as_deref(), Some("Local Helper"));
        assert_eq!(helper.command, "ee-helper");
        assert_eq!(helper.args, vec!["serve"]);
        assert_eq!(helper.env.get("EE_AGENT_MODE").map(|v| v.raw.as_str()), Some("1"));
        assert_eq!(helper.cwd.as_deref(), Some(Path::new("/tmp/agent")));
        assert_eq!(settings.agents.servers.len(), 2);
    }

    // ── Agent web context trusted config (phase 1) ───────────────────────────

    #[test]
    fn agent_web_context_is_disabled_by_default() {
        assert!(!EditorSettings::default().agents.web_context.enabled);

        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(&env.cwd).unwrap();
        let settings = load_config_with_env(None, &env);
        assert!(!settings.agents.web_context.enabled);
    }

    #[test]
    fn agent_web_context_exa_uses_defaults_and_user_global_options() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(&env.cwd).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            r#"
[agents.web_context]
enabled = true
backend = "exa"
provider_secret_reference = "secret://exa-api-key"

[agents.web_context.exa]
max_results = 7
search_mode = "neural"
"#,
        );

        let settings = load_for(&env);
        let web = &settings.agents.web_context;
        assert!(web.enabled);
        assert_eq!(web.provider, WebSearchProvider::Exa);
        let WebSearchProviderOptions::Exa(options) = &web.provider_options else {
            panic!("expected Exa provider options");
        };
        assert_eq!(options.max_results, 7);
        assert_eq!(options.search_mode, ExaSearchMode::Neural);
        assert!(web.search_endpoint.is_none());
        assert_eq!(web.provider_secret_reference.as_deref(), Some("secret://exa-api-key"));

        let defaults = AgentWebContextConfig::default();
        assert_eq!(defaults.provider, WebSearchProvider::Searxng);
        assert_eq!(defaults.provider_options, WebSearchProviderOptions::Searxng);
    }

    #[test]
    fn agent_web_context_provider_defaults_and_excluded_options_are_stable() {
        assert_eq!(web_search_provider(WebContextBackendToml::Searxng), WebSearchProvider::Searxng);
        assert_eq!(web_search_provider(WebContextBackendToml::Exa), WebSearchProvider::Exa);
        assert_eq!(
            web_search_provider(WebContextBackendToml::BraveLlmContext),
            WebSearchProvider::BraveLlmContext
        );
        assert_eq!(web_search_provider(WebContextBackendToml::Tavily), WebSearchProvider::Tavily);

        let tavily: EeToml = toml::from_str(
            "[agents.web_context]\nbackend = \"tavily\"\n\n[agents.web_context.tavily]\n",
        )
        .unwrap();
        let tavily = tavily.agents.unwrap().web_context.unwrap();
        assert_eq!(
            web_search_provider_options(WebContextBackendToml::Tavily, &tavily),
            WebSearchProviderOptions::Tavily(TavilySearchOptions::default())
        );

        let brave: EeToml = toml::from_str(
            "[agents.web_context]\nbackend = \"brave_llm_context\"\n\n[agents.web_context.brave_llm_context]\n",
        )
        .unwrap();
        let brave = brave.agents.unwrap().web_context.unwrap();
        assert_eq!(
            web_search_provider_options(WebContextBackendToml::BraveLlmContext, &brave),
            WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default())
        );

        for excluded in [
            "[agents.web_context]\nbackend = \"brave_llm_context\"\n\n[agents.web_context.brave_llm_context]\nenable_local = true\n",
            "[agents.web_context]\nbackend = \"tavily\"\n\n[agents.web_context.tavily]\nresearch = true\n",
            "[agents.web_context]\nbackend = \"exa\"\n\n[agents.web_context.exa]\ndomains = [\"example.com\"]\n",
        ] {
            assert!(
                toml::from_str::<EeToml>(excluded).is_err(),
                "excluded option parsed: {excluded}"
            );
        }
    }

    #[test]
    fn agent_web_context_provider_limits_fail_config_validation() {
        assert_eq!(MAX_EXA_RESULTS, ee_agent_host::web_context::MAX_EXA_RESULTS);
        assert_eq!(MAX_TAVILY_RESULTS, ee_agent_host::web_context::MAX_TAVILY_RESULTS);
        assert_eq!(
            MAX_TAVILY_CHUNKS_PER_SOURCE,
            ee_agent_host::web_context::MAX_TAVILY_CHUNKS_PER_SOURCE
        );
        assert_eq!(MAX_BRAVE_RESULTS, ee_agent_host::web_context::MAX_BRAVE_RESULTS);
        assert_eq!(MAX_BRAVE_TOKENS, ee_agent_host::web_context::MAX_BRAVE_TOKENS);
        assert_eq!(MAX_BRAVE_URLS, ee_agent_host::web_context::MAX_BRAVE_URLS);
        assert_eq!(MAX_BRAVE_SNIPPETS, ee_agent_host::web_context::MAX_BRAVE_SNIPPETS);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        for (field, config) in [
            (
                "exa.max_results",
                "[agents.web_context]\nbackend = \"exa\"\n\n[agents.web_context.exa]\nmax_results = 0\n",
            ),
            (
                "tavily.chunks_per_source",
                "[agents.web_context]\nbackend = \"tavily\"\n\n[agents.web_context.tavily]\nchunks_per_source = 4\n",
            ),
            (
                "brave_llm_context.max_tokens",
                "[agents.web_context]\nbackend = \"brave_llm_context\"\n\n[agents.web_context.brave_llm_context]\nmax_tokens = 10001\n",
            ),
        ] {
            std::fs::write(&path, config).unwrap();
            let error = validate_config_file(&path).unwrap_err();
            assert!(error.contains(field), "{error}");
        }
    }

    #[test]
    fn agent_web_context_rejects_vendor_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "[agents.web_context]\nbackend = \"tavily\"\nendpoint = \"https://search.example\"\n",
        )
        .unwrap();

        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("endpoint is only permitted when backend is searxng"));

        std::fs::write(&path, "[agents.web_context]\nbackend = \"searxng\"\n").unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("endpoint is required when backend is searxng"));
    }

    #[test]
    fn agent_web_context_uses_user_global_config_across_workspace_root_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let file = env.cwd.join("project").join("main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            r#"
[agents.web_context]
enabled = true
backend = "searxng"
endpoint = "https://search.example/search"
hosts = ["search.example", "docs.example"]

[agents.web_context.limits]
max_response_bytes = 8192
max_text_bytes = 4096
max_search_results = 12
max_redirects = 2
request_timeout_ms = 30000
max_concurrent_requests = 4
"#,
        );
        std::fs::write(
            env.cwd.join(".ee.toml"),
            r#"
root = true

[agents.web_context]
hosts = ["docs.example"]

[agents.web_context.limits]
max_text_bytes = 2048
"#,
        )
        .unwrap();

        let settings = load_config_with_env(Some(&file), &env);
        let web = &settings.agents.web_context;
        assert!(web.enabled);
        assert_eq!(web.search_endpoint.as_deref(), Some("https://search.example/search"));
        assert_eq!(web.preapproved_hosts, BTreeSet::from([String::from("docs.example")]));
        assert_eq!(web.limits.max_response_bytes, 8192);
        assert_eq!(web.limits.max_text_bytes, 2048);
        assert_eq!(web.limits.max_search_results, 12);
        assert_eq!(web.limits.max_redirects, 2);
        assert_eq!(web.limits.request_timeout_ms, 30_000);
        assert_eq!(web.limits.max_concurrent_requests, 4);
        assert!(web.provider_secret_reference.is_none());

        let rendered =
            toml::to_string_pretty(&resolved_config_with_env(Some(&file), &env)).unwrap();
        assert!(!rendered.contains("provider_secret_reference"));
    }

    #[test]
    fn agent_web_context_workspace_cannot_widen_or_enable_untrusted_config() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(&env.cwd).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            r#"
[agents.web_context]
enabled = true
backend = "searxng"
endpoint = "https://search.example/search"
hosts = ["search.example", "docs.example"]

[agents.web_context.limits]
max_response_bytes = 8192
max_text_bytes = 4096
max_search_results = 12
max_redirects = 2
"#,
        );
        write_config_layer(
            &env,
            ConfigLayerKind::Ancestor,
            r#"
[agents.web_context]
enabled = true
backend = "searxng"
endpoint = "https://untrusted.example/search"
hosts = ["docs.example", "untrusted.example"]
provider_secret_reference = "secret://workspace-provider"

[agents.web_context.limits]
max_response_bytes = 16384
max_text_bytes = 2048
max_search_results = 20
max_redirects = 3
"#,
        );

        let settings = load_config_with_env(None, &env);
        let web = &settings.agents.web_context;
        assert!(web.enabled);
        assert_eq!(web.search_endpoint.as_deref(), Some("https://search.example/search"));
        assert_eq!(web.preapproved_hosts, BTreeSet::from([String::from("docs.example")]));
        assert_eq!(web.limits.max_response_bytes, 8192);
        assert_eq!(web.limits.max_text_bytes, 2048);
        assert_eq!(web.limits.max_search_results, 12);
        assert_eq!(web.limits.max_redirects, 2);
        assert!(web.provider_secret_reference.is_none());
        assert_eq!(web.provider, WebSearchProvider::Searxng);
        assert_eq!(web.provider_options, WebSearchProviderOptions::Searxng);
    }

    #[test]
    fn agent_web_context_workspace_cannot_change_provider_or_semantic_options() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(&env.cwd).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            r#"
[agents.web_context]
enabled = true
backend = "exa"

[agents.web_context.exa]
max_results = 7
search_mode = "neural"
"#,
        );
        write_config_layer(
            &env,
            ConfigLayerKind::Ancestor,
            r#"
[agents.web_context]
backend = "tavily"

[agents.web_context.tavily]
max_results = 99
chunks_per_source = 9
search_depth = "advanced"
"#,
        );

        let web = &load_for(&env).agents.web_context;
        assert_eq!(web.provider, WebSearchProvider::Exa);
        let WebSearchProviderOptions::Exa(options) = &web.provider_options else {
            panic!("expected Exa provider options");
        };
        assert_eq!(options.max_results, 7);
        assert_eq!(options.search_mode, ExaSearchMode::Neural);
    }

    #[test]
    fn agent_web_context_provider_reference_from_xdg_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        write_config_layer(
            &env,
            ConfigLayerKind::UserXdg,
            "[agents.web_context]\nprovider_secret_reference = \"secret://web-provider\"\n",
        );

        let settings = load_for(&env);
        assert_eq!(
            settings.agents.web_context.provider_secret_reference.as_deref(),
            Some("secret://web-provider")
        );
        let rendered = toml::to_string_pretty(&resolved_config_with_env(None, &env)).unwrap();
        assert!(rendered.contains("secret://web-provider"));
    }

    #[test]
    fn agent_web_context_provider_reference_from_system_or_workspace_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        std::fs::create_dir_all(env.cwd.as_path()).unwrap();
        let reference =
            "[agents.web_context]\nprovider_secret_reference = \"secret://web-provider\"\n";
        write_config_layer(&env, ConfigLayerKind::System, reference);
        write_config_layer(&env, ConfigLayerKind::Ancestor, reference);

        let settings = load_for(&env);
        assert!(settings.agents.web_context.provider_secret_reference.is_none());
    }

    #[test]
    fn agent_web_context_rejects_malformed_provider_reference_without_echoing_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "[agents.web_context]\nprovider_secret_reference = \"secret://bad name\"\n",
        )
        .unwrap();

        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("agents.web_context.provider_secret_reference"));
        assert!(!error.contains("bad name"));
    }

    #[test]
    fn agent_web_context_raw_request_limits_require_nonzero_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "[agents.web_context.limits]\nrequest_timeout_ms = 30000\nmax_concurrent_requests = 4\n",
        )
        .unwrap();
        validate_config_file(&path).unwrap();

        std::fs::write(
            &path,
            "[agents.web_context.limits]\nrequest_timeout_ms = 0\nmax_concurrent_requests = 0\n",
        )
        .unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("agents.web_context.limits.request_timeout_ms"));

        std::fs::write(
            &path,
            "[agents.web_context.limits]\nrequest_timeout_ms = 1\nmax_concurrent_requests = 0\n",
        )
        .unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains("agents.web_context.limits.max_concurrent_requests"));
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
    fn validate_agent_server_rejects_malformed_secret_reference_with_field_path() {
        let server = AgentServerToml {
            label: None,
            command: Some(String::from("agent-bin")),
            args: None,
            env: BTreeMap::from([(
                String::from("OPENROUTER_API_KEY"),
                String::from("secret://bad name"),
            )]),
            cwd: None,
        };
        let err = validate_agent_server("gh", &server).expect_err("rejected");
        assert!(err.contains("agents.servers.gh.env.OPENROUTER_API_KEY"), "field path: {err}");
        assert!(!err.contains("bad name"), "no raw value echo: {err}");
    }

    #[test]
    fn agent_env_secret_reference_substring_stays_literal() {
        let server = AgentServerToml {
            label: None,
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
            merge_agent_server("gh", &server, None, ConfigLayerKind::Ancestor).expect("literals");
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
        assert!(agents.get("max_concurrent_prompts").is_some());
        assert!(agents.get("workspace_memory").is_some());
        assert!(agents.get("servers").is_some());
        let workspace_memory = defs.get("WorkspaceMemoryToml").unwrap().get("properties").unwrap();
        for field in [
            "enabled",
            "max_value_bytes",
            "max_active_facts",
            "max_active_bytes",
            "max_recall_results",
            "busy_timeout_ms",
            "default_expiry_days",
            "candidate_retention_days",
            "stale_retention_days",
            "superseded_retention_days",
        ] {
            assert!(
                workspace_memory.get(field).is_some(),
                "missing workspace-memory field {field}"
            );
        }

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

        let web_context = defs.get("AgentWebContextToml").unwrap();
        let web_properties = web_context.get("properties").and_then(Value::as_object).unwrap();
        for field in ["backend", "provider_secret_reference", "exa", "brave_llm_context", "tavily"]
        {
            assert!(web_properties.contains_key(field), "missing web-context schema field {field}");
        }
        assert!(!web_properties.contains_key("endpoint_override"));
        let backends = defs
            .get("WebContextBackendToml")
            .and_then(|value| value.get("enum"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(
            backends,
            &[
                Value::String(String::from("searxng")),
                Value::String(String::from("exa")),
                Value::String(String::from("brave_llm_context")),
                Value::String(String::from("tavily")),
            ]
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
    fn workspace_memory_defaults_disabled_and_merges_field_by_field() {
        let mut settings = EditorSettings::default();
        let defaults = settings.agents.workspace_memory.clone();
        assert!(!defaults.enabled);

        let first: EeToml = toml::from_str(
            "[agents.workspace_memory]\nenabled = true\nmax_value_bytes = 8192\nmax_recall_results = 12\ndefault_expiry_days = 180\ncandidate_retention_days = 5\n",
        )
        .unwrap();
        settings.merge_toml(&first, ConfigLayerKind::System);
        let second: EeToml = toml::from_str(
            "[agents.workspace_memory]\nmax_active_facts = 512\nbusy_timeout_ms = 3500\nstale_retention_days = 45\nsuperseded_retention_days = 120\n",
        )
        .unwrap();
        settings.merge_toml(&second, ConfigLayerKind::Ancestor);

        let memory = &settings.agents.workspace_memory;
        assert!(memory.enabled);
        assert_eq!(memory.max_value_bytes, 8192);
        assert_eq!(memory.max_active_facts, 512);
        assert_eq!(memory.max_active_bytes, defaults.max_active_bytes);
        assert_eq!(memory.max_recall_results, 12);
        assert_eq!(memory.busy_timeout_ms, 3500);
        assert_eq!(memory.default_expiry_days, 180);
        assert_eq!(memory.candidate_retention_days, 5);
        assert_eq!(memory.stale_retention_days, 45);
        assert_eq!(memory.superseded_retention_days, 120);
    }

    #[test]
    fn workspace_memory_validation_rejects_out_of_range_values() {
        let parsed: EeToml =
            toml::from_str("[agents.workspace_memory]\nenabled = true\nmax_recall_results = 0\n")
                .unwrap();
        let error = validate_agents_mcp_config(&parsed).unwrap_err();
        assert!(error.contains("agents.workspace_memory.max_recall_results"));

        let parsed: EeToml =
            toml::from_str("[agents.workspace_memory]\ncandidate_retention_days = 0\n").unwrap();
        let error = validate_agents_mcp_config(&parsed).unwrap_err();
        assert!(error.contains("agents.workspace_memory.candidate_retention_days"));

        let parsed: EeToml =
            toml::from_str("[agents.workspace_memory]\ndefault_expiry_days = 0\n").unwrap();
        validate_agents_mcp_config(&parsed).unwrap();
    }

    #[test]
    fn workspace_memory_invalid_values_keep_safe_defaults() {
        let mut resolved = WorkspaceMemorySettings::default();
        let defaults = resolved.clone();
        merge_workspace_memory(
            &mut resolved,
            &WorkspaceMemoryToml {
                max_value_bytes: Some(0),
                max_active_facts: Some(MAX_WORKSPACE_MEMORY_ACTIVE_FACTS + 1),
                max_active_bytes: Some(0),
                max_recall_results: Some(MAX_WORKSPACE_MEMORY_RECALL_RESULTS + 1),
                busy_timeout_ms: Some(0),
                default_expiry_days: Some(MAX_WORKSPACE_MEMORY_RETENTION_DAYS + 1),
                candidate_retention_days: Some(0),
                stale_retention_days: Some(MAX_WORKSPACE_MEMORY_RETENTION_DAYS + 1),
                superseded_retention_days: Some(0),
                ..WorkspaceMemoryToml::default()
            },
        );
        assert_eq!(resolved, defaults);
    }

    #[test]
    fn workspace_memory_switch_persistence_preserves_unrelated_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        let original = "# keep this comment\r\n[agents]\r\nenabled = true\r\n\r\n[agents.workspace_memory]\r\nenabled = false # explicit switch\r\ncandidate_retention_days = 11\r\n\r\n[agents.servers.helper]\r\ncommand = \"helper-agent\"\r\n";
        std::fs::write(&path, original).unwrap();

        persist_workspace_memory_enabled(&path, true).unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("# keep this comment\r\n"));
        assert!(updated.contains("enabled = true # explicit switch\r\n"));
        assert!(updated.contains("candidate_retention_days = 11\r\n"));
        assert!(updated.contains("[agents.servers.helper]\r\ncommand = \"helper-agent\"\r\n"));
        let parsed: EeToml = toml::from_str(&updated).unwrap();
        assert_eq!(parsed.agents.unwrap().workspace_memory.unwrap().enabled, Some(true));
    }

    #[test]
    fn workspace_memory_switch_persistence_adds_explicit_table() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        std::fs::write(&path, "[agents]\nenabled = true\n").unwrap();

        persist_workspace_memory_enabled(&path, false).unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("[agents]\nenabled = true\n"));
        assert!(updated.contains("[agents.workspace_memory]\nenabled = false\n"));
    }

    #[test]
    fn workspace_memory_switch_persistence_handles_header_without_final_newline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        std::fs::write(&path, "[agents.workspace_memory]").unwrap();

        persist_workspace_memory_enabled(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "[agents.workspace_memory]\nenabled = true\n"
        );
    }

    #[test]
    fn workspace_memory_switch_persistence_refuses_invalid_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ee.toml");
        let invalid = "[agents.workspace_memory\nenabled = true\n";
        std::fs::write(&path, invalid).unwrap();

        let error = persist_workspace_memory_enabled(&path, false).unwrap_err();

        assert!(error.contains("refusing to modify invalid config"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), invalid);
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

    #[test]
    fn rubber_duck_config_rejects_ambiguous_backend_and_roundtrips_limits() {
        let ambiguous = RubberDuckToml {
            internal_model_id: Some("critic-model".into()),
            external_agent_id: Some("critic-agent".into()),
            ..RubberDuckToml::default()
        };
        assert!(validate_rubber_duck_toml(&ambiguous).unwrap_err().contains("mutually exclusive"));

        let patch = RubberDuckToml {
            mode: Some("automatic".into()),
            external_agent_id: Some("critic-agent".into()),
            max_calls: Some(3),
            max_context_bytes: Some(4096),
            max_output_bytes: Some(2048),
            timeout_ms: Some(5000),
            ..RubberDuckToml::default()
        };
        let settings = merge_rubber_duck(&RubberDuckSettings::default(), &patch).unwrap();
        assert_eq!(settings.mode, RubberDuckModeSetting::Automatic);
        assert_eq!(settings.external_agent_id.as_deref(), Some("critic-agent"));
        assert_eq!(settings.max_calls, 3);
    }

    #[cfg(any(feature = "agents", test))]
    #[test]
    fn rubber_duck_resolution_degrades_unknown_optional_agent_only() {
        let settings = RubberDuckSettings {
            external_agent_id: Some("missing".into()),
            ..RubberDuckSettings::default()
        };
        let resolved = settings
            .resolve_backend_policy(&BTreeSet::new(), &BTreeSet::from(["root".into()]))
            .unwrap();
        assert!(resolved.unavailable.is_some());
        assert!(!resolved.critic_available());
    }

    #[cfg(feature = "agents")]
    #[test]
    fn agent_setup_writes_complete_server_to_global_config_only() {
        let temp = tempfile::tempdir().unwrap();
        let env = test_config_environment(temp.path());
        let config_dir = env.config_dir.as_ref().expect("test config directory");
        std::fs::create_dir_all(config_dir.join("ee")).unwrap();
        std::fs::write(
            config_dir.join("ee").join("config.toml"),
            "wrap_lines = true\n[agents.servers.existing]\ncommand = \"existing-agent\"\n",
        )
        .unwrap();
        let values = BTreeMap::from([
            (
                String::from("OPENROUTER_API_KEY"),
                String::from("secret://agent.openrouter.OPENROUTER_API_KEY"),
            ),
            (String::from("OPENROUTER_MODEL"), String::from("example/model")),
            (String::from("OPENROUTER_MAX_ITERATIONS"), String::from("32")),
        ]);

        let path = configure_global_agent_server_with_env(
            "openrouter",
            Path::new("/home/example/.local/bin/ee-openrouter-agent"),
            &[String::from("--stdio")],
            &values,
            &env,
        )
        .expect("configure global agent");
        let document: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(path, config_dir.join("ee").join("config.toml"));
        assert!(!env.cwd.join(".ee.toml").exists());
        assert_eq!(document["wrap_lines"].as_bool(), Some(true));
        assert_eq!(document["agents"]["enabled"].as_bool(), Some(true));
        assert_eq!(document["agents"]["default_agent"].as_str(), Some("openrouter"));
        assert_eq!(
            document["agents"]["servers"]["openrouter"]["command"].as_str(),
            Some("/home/example/.local/bin/ee-openrouter-agent")
        );
        assert_eq!(
            document["agents"]["servers"]["openrouter"]["args"],
            toml::Value::Array(vec![toml::Value::String(String::from("--stdio"))])
        );
        assert_eq!(
            document["agents"]["servers"]["openrouter"]["env"]["OPENROUTER_API_KEY"].as_str(),
            Some("secret://agent.openrouter.OPENROUTER_API_KEY")
        );
        assert_eq!(
            document["agents"]["servers"]["openrouter"]["env"]["OPENROUTER_MAX_ITERATIONS"]
                .as_str(),
            Some("32")
        );
        assert_eq!(
            document["agents"]["servers"]["existing"]["command"].as_str(),
            Some("existing-agent")
        );
    }
}
