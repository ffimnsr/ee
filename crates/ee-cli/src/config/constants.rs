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

pub(super) const SYSTEM_CONFIG_PATH: &str = "/etc/ee/config.toml";
// Raw config validation also runs in builds without the optional agents host.
// Keep these synchronized with the public host provider caps.
pub(super) const MAX_EXA_RESULTS: usize = 50;
pub(super) const MAX_TAVILY_RESULTS: usize = 50;
pub(super) const MAX_TAVILY_CHUNKS_PER_SOURCE: usize = 3;
pub(super) const MAX_BRAVE_RESULTS: usize = 20;
pub(super) const MAX_BRAVE_TOKENS: usize = 10_000;
pub(super) const MAX_BRAVE_URLS: usize = 20;
pub(super) const MAX_BRAVE_SNIPPETS: usize = 20;
pub(crate) const LSP_PLUGIN_NAME: &str = "xi-lsp-plugin";

pub(super) const CONFIG_TEMPLATE: &str = r#"# ee configuration
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
