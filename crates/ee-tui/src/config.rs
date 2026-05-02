//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `~/.ee.toml`
//!   3. `<git-repo-root>/.ee.toml`
//!   4. `<cwd>/.ee.toml`
//!   5. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use globset::GlobBuilder;
use serde::Deserialize;

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
#[derive(Debug, Clone)]
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
        }
    }
}

// ── .ee.toml raw shape ────────────────────────────────────────────────────────

/// Raw `.ee.toml` shape; all fields optional so partial files work.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EeToml {
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

/// Walk up directory tree from `start` looking for `.git` or `.git` file.
/// Returns the directory that contains `.git`, or `None`.
fn find_git_root(start: &Path) -> Option<PathBuf> {
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
        if let Some((k, v)) = parse_ec_kv(line) {
            if k == "root" && v == "true" {
                return true;
            }
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
    let mut settings = EditorSettings::default();

    // 1. User home config.
    if let Some(home) = dirs::home_dir() {
        load_ee_toml(&mut settings, &home.join(".ee.toml"));
    }

    let cwd = std::env::current_dir().unwrap_or_default();

    // 2. Git repo root config (skip if same as cwd to avoid double-loading).
    if let Some(root) = find_git_root(&cwd) {
        if root != cwd {
            load_ee_toml(&mut settings, &root.join(".ee.toml"));
        }
    }

    // 3. Current working directory config.
    load_ee_toml(&mut settings, &cwd.join(".ee.toml"));

    // 4. .editorconfig (highest priority, per-file aware).
    let ec_target = file_path.unwrap_or(cwd.as_path());
    apply_editorconfig(&mut settings, ec_target);

    settings
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
}
