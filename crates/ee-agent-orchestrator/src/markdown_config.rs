//! Shared discovery and parsing for markdown-defined agent configuration.
//!
//! Agent definitions and slash-command templates both use a `---` YAML
//! frontmatter block followed by a markdown body. Discovery is bounded and
//! fail-closed: oversized files, unreadable paths, invalid UTF-8, malformed
//! YAML, and symlink escapes become errors the caller can log and skip.

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

/// Reads one markdown config file, bounded by `max_bytes`, after resolving it
/// inside `scope_root`.
pub(crate) fn read_bounded(
    path: &Path,
    scope_root: &Path,
    max_bytes: usize,
) -> Result<(PathBuf, String), String> {
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("cannot resolve path: {error}"))?;
    if !canonical.starts_with(scope_root) {
        return Err("path escapes the configuration root".to_string());
    }
    let metadata = fs::metadata(&canonical).map_err(|error| format!("cannot stat: {error}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".to_string());
    }
    if metadata.len() > max_bytes as u64 {
        return Err(format!("file exceeds the {max_bytes}-byte limit"));
    }
    let bytes = fs::read(&canonical).map_err(|error| format!("cannot read: {error}"))?;
    let content = String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8".to_string())?;
    Ok((canonical, content))
}

/// Splits `---` frontmatter from the markdown body.
pub(crate) fn split_frontmatter(content: &str) -> Result<(&str, &str), String> {
    let mut lines = content.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Err("file is empty".to_string());
    };
    if first.trim() != "---" {
        return Err("missing YAML frontmatter".to_string());
    }
    let mut offset = first.len();
    let start = offset;
    for line in lines {
        if line.trim() == "---" {
            return Ok((&content[start..offset], &content[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err("unterminated YAML frontmatter".to_string())
}

/// Parses YAML frontmatter into a typed struct via JSON, so nested mappings
/// and scalar coercions follow `serde_json` rules.
pub(crate) fn parse_frontmatter<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    if text.trim().is_empty() {
        return serde_json::from_value(serde_json::json!({}))
            .map_err(|error| format!("invalid frontmatter: {error}"));
    }
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    let value =
        serde_json::to_value(&yaml).map_err(|error| format!("invalid frontmatter: {error}"))?;
    serde_json::from_value(value).map_err(|error| format!("invalid frontmatter: {error}"))
}

/// Collects `*.md` files below `root`, bounded by depth and file count.
/// Directory entries are sorted for deterministic precedence.
pub(crate) fn collect_markdown(
    root: &Path,
    depth: usize,
    max_depth: usize,
    max_files: usize,
    files: &mut Vec<PathBuf>,
) {
    if depth > max_depth || files.len() >= max_files {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else { return };
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if files.len() >= max_files {
            return;
        }
        if path.is_dir() {
            collect_markdown(&path, depth + 1, max_depth, max_files, files);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
}

/// Validates a command/agent identifier: ASCII alphanumeric first character,
/// then letters, digits, `_`, or `-`, bounded by the child-role cap.
pub(crate) fn validate_identifier(name: &str, label: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let max = crate::child_registry::MAX_CHILD_ROLE_CHARS;
    let valid = !name.is_empty()
        && name.chars().count() <= max
        && chars.next().is_some_and(|character| character.is_ascii_alphanumeric())
        && chars
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{label} {name:?} must be a 1-{max} character ASCII identifier starting with a letter or digit"
        ))
    }
}

/// Truncates `text` to `max_chars`, appending an ellipsis when shortened.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_splits_and_parses_nested_yaml() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            mode: String,
            #[serde(default)]
            permission: Option<serde_json::Value>,
        }
        let content = "---\nmode: subagent\npermission:\n  edit: deny\n---\nbody\n";
        let (frontmatter, body) = split_frontmatter(content).expect("splits");
        let parsed: Fixture = parse_frontmatter(frontmatter).expect("parses");
        assert_eq!(parsed.mode, "subagent");
        assert!(parsed.permission.is_some());
        assert_eq!(body, "body\n");
    }

    #[test]
    fn missing_frontmatter_fails_closed() {
        let error = split_frontmatter("body only\n").expect_err("rejects");
        assert!(error.contains("missing YAML frontmatter"));
    }

    #[test]
    fn identifier_validation_rejects_spaces_and_leading_punctuation() {
        assert!(validate_identifier("reviewer-2", "command name").is_ok());
        assert!(validate_identifier("two words", "command name").is_err());
        assert!(validate_identifier("_hidden", "command name").is_err());
    }

    #[test]
    fn markdown_collection_is_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..8 {
            std::fs::write(temp.path().join(format!("{index}.md")), "---\n---\nbody\n")
                .expect("write");
        }
        let mut files = Vec::new();
        collect_markdown(temp.path(), 0, 1, 3, &mut files);
        assert_eq!(files.len(), 3);
    }
}
