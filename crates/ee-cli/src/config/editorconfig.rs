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

use super::editor_settings::{EditorSettings, EndOfLine, IndentStyle};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use globset::GlobBuilder;

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
pub(super) fn apply_editorconfig(settings: &mut EditorSettings, file_path: &Path) {
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
pub(super) fn apply_editorconfig_text(settings: &mut EditorSettings, text: &str, target: &Path) {
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
