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

use super::agents;
use super::agents_settings::{
    AgentsSettings, MAX_AGENT_MAX_CONCURRENT_PROMPTS, merge_agent_server,
};
use super::discovery::ConfigLayerKind;
use super::init::load_config;
use super::lsp::LspSettings;
use super::mcp::{McpSettings, resolve_mcp_server};
use super::raw::{AgentsToml, EeToml, KeymapToml, McpToml};
use super::rubber_duck::merge_rubber_duck;
use super::workspace_memory::merge_workspace_memory;
use crate::keymap::{self, KeymapOperation, KeymapSettings, SequenceBinding};
#[cfg(any(feature = "agents", test))]
use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use xi_core_lib::config::Table as XiConfigTable;

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

impl EditorSettings {
    /// Apply any set fields from `patch`, leaving unset fields unchanged.
    pub(super) fn merge_toml(&mut self, patch: &EeToml, kind: ConfigLayerKind) {
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

    pub(super) fn finalize_agents(&mut self) {
        for id in agents::remove_incomplete_servers(self.agents.enabled, &mut self.agents.servers) {
            eprintln!(
                "ee: warning: invalid agents server `{id}`: agent server command must not be empty"
            );
        }
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
