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
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[cfg(any(feature = "agents", test))]
use ee_agent_host::web_context::{
    BraveFreshness, BraveSafeSearchMode, BraveThresholdMode, ExaSearchMode, TavilySearchDepth,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xi_core_lib::runtime_loader::RuntimeLanguageConfig;

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
    /// Trusted configuration for agent web retrieval. Direct fetch is enabled by
    /// default; only user-global config can grant search-provider access. Workspace
    /// files can only disable or restrict web context.
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
    /// Enables optional web retrieval. Defaults to `true`.
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

/// Parse one `.ee.toml` file if it exists and is readable.
pub(super) fn parse_ee_toml(path: &Path) -> Option<EeToml> {
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

pub(super) fn keymap_settings_to_toml(
    keymap: &crate::keymap::KeymapSettings,
) -> Option<KeymapToml> {
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

pub(super) fn parse_config_document(path: &Path) -> Result<toml::Value, String> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str::<toml::Value>(&contents)
            .map_err(|err| format!("Config parse error in {}: {err}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::map::Map::new()))
        }
        Err(err) => Err(format!("Cannot read {}: {err}", path.display())),
    }
}
