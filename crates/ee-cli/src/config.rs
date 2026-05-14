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

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::Value;
use xi_core_lib::config::Table as XiConfigTable;

use crate::keymap::{self, KeymapOperation, KeymapSettings, SequenceBinding};

const SYSTEM_CONFIG_PATH: &str = "/etc/ee/config.toml";

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
            keymap: KeymapSettings::default(),
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
    pub keymap: Option<KeymapToml>,
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
            config_dir: dirs::config_dir(),
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

/// Parse and apply one `.ee.toml` file if it exists and is readable.
fn load_ee_toml(settings: &mut EditorSettings, path: &Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return,
    };
    match toml::from_str::<EeToml>(&text) {
        Ok(patch) => settings.merge_toml(&patch),
        Err(e) => {
            // Surface parse errors so users can fix them, but don't abort.
            eprintln!("ee: warning: failed to parse {}: {}", path.display(), e);
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

    for layer in discover_config_layers_with_env(env, file_path).layers {
        load_ee_toml(&mut settings, &layer.path);
    }

    if let Some(file_path) = file_path {
        apply_editorconfig(&mut settings, file_path);
    }

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
