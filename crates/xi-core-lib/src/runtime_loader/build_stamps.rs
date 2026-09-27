// Copyright 2026 The ee authors. All rights reserved.

//! Incremental runtime grammar build stamps.
//!
//! `build --all` (and `mk install`) must not recompile grammars whose source
//! pin, git rev, and staged source files are unchanged. Each built grammar
//! records a stamp beside the output library; a later build skips the compile
//! when the stamp matches the current source pin + resolved rev + staged
//! source mtimes and the output `.so` still exists.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::helpers::sanitize_path_component;
use super::types::GRAMMARS_DIR_NAME;

/// Stable (seconds, nanoseconds) mtime representation; serde_json has no
/// u128 support so tuples are used instead of raw timestamps.
type Mtime = (u64, u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GrammarBuildStamp {
    pub language_id: String,
    pub source_pin: String,
    pub resolved_rev: Option<String>,
    pub library: String,
    pub sources: Vec<(String, Mtime)>,
}

impl GrammarBuildStamp {
    pub fn capture(
        language_id: &str,
        source_pin: &str,
        resolved_rev: Option<&str>,
        library: &str,
        build_source_dir: &Path,
    ) -> Self {
        Self {
            language_id: language_id.to_string(),
            source_pin: source_pin.to_string(),
            resolved_rev: resolved_rev.map(str::to_string),
            library: library.to_string(),
            sources: collect_source_mtimes(build_source_dir),
        }
    }

    /// Stamp matches the current build inputs and the output library exists.
    pub fn is_fresh(
        &self,
        source_pin: &str,
        resolved_rev: Option<&str>,
        library: &str,
        grammar_path: &Path,
        build_source_dir: &Path,
    ) -> bool {
        if self.source_pin != source_pin || self.resolved_rev.as_deref() != resolved_rev {
            return false;
        }
        if self.library != library || !grammar_path.is_file() {
            return false;
        }
        let sources = collect_source_mtimes(build_source_dir);
        if sources.is_empty() && !self.sources.is_empty() {
            return false;
        }
        self.sources == sources
    }
}

pub(crate) fn stamp_path(output_root: &Path, language_id: &str) -> PathBuf {
    output_root
        .join(GRAMMARS_DIR_NAME)
        .join(".stamps")
        .join(format!("{}.json", sanitize_path_component(language_id)))
}

pub(crate) fn load_stamp(output_root: &Path, language_id: &str) -> Option<GrammarBuildStamp> {
    let path = stamp_path(output_root, language_id);
    let contents = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

pub(crate) fn write_stamp(stamp: &GrammarBuildStamp, output_root: &Path) -> Result<(), String> {
    let path = stamp_path(output_root, &stamp.language_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed creating stamp dir {}: {error}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(stamp)
        .map_err(|error| format!("failed serializing build stamp: {error}"))?;
    fs::write(&path, contents)
        .map_err(|error| format!("failed writing build stamp {}: {error}", path.display()))
}

fn collect_source_mtimes(dir: &Path) -> Vec<(String, Mtime)> {
    let mut entries = Vec::new();
    let Ok(read_dir) = fs::read_dir(dir) else {
        return entries;
    };
    for entry in read_dir.filter_map(Result::ok) {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let relative = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
            for (child, mtime) in collect_source_mtimes(&path) {
                entries.push((relative.join(child).to_string_lossy().to_string(), mtime));
            }
        } else if file_type.is_file() {
            let Some(mtime) = file_mtime(&path) else {
                continue;
            };
            let relative = path.strip_prefix(dir).unwrap_or(&path);
            entries.push((relative.to_string_lossy().to_string(), mtime));
        }
    }
    entries.sort();
    entries
}

fn file_mtime(path: &Path) -> Option<Mtime> {
    let modified: SystemTime = fs::metadata(path).ok()?.modified().ok()?;
    match modified.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => Some((duration.as_secs(), duration.subsec_nanos().into())),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    use tempfile::TempDir;

    #[test]
    fn stamp_is_fresh_when_inputs_unchanged_and_stale_on_mtime_change() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("parser.c"), "int tree_sitter_x(void) { return 1; }\n").unwrap();

        let library_path = temp.path().join("libtree-sitter-x.so");
        fs::write(&library_path, b"fake").unwrap();

        let stamp = GrammarBuildStamp::capture(
            "x",
            "git:https://example.com/repo#rev:abc",
            Some("abc"),
            "tree-sitter-x",
            &source,
        );
        assert!(stamp.is_fresh(
            "git:https://example.com/repo#rev:abc",
            Some("abc"),
            "tree-sitter-x",
            &library_path,
            &source
        ));

        // mtime bumps on the staged source -> stale.
        fs::write(source.join("parser.c"), "int tree_sitter_x(void) { return 2; }\n").unwrap();
        // Guard against same-nanosecond mtimes on fast filesystems.
        std::thread::sleep(Duration::from_millis(5));
        assert!(!stamp.is_fresh(
            "git:https://example.com/repo#rev:abc",
            Some("abc"),
            "tree-sitter-x",
            &library_path,
            &source
        ));
    }

    #[test]
    fn stamp_is_stale_on_rev_or_pin_change_and_missing_library() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("parser.c"), "int tree_sitter_x(void) { return 1; }\n").unwrap();
        let library_path = temp.path().join("libtree-sitter-x.so");
        fs::write(&library_path, b"fake").unwrap();

        let stamp = GrammarBuildStamp::capture(
            "x",
            "git:https://example.com/repo#rev:abc",
            Some("abc"),
            "tree-sitter-x",
            &source,
        );

        assert!(!stamp.is_fresh(
            "git:https://example.com/repo#rev:def",
            Some("def"),
            "tree-sitter-x",
            &library_path,
            &source
        ));
        assert!(!stamp.is_fresh(
            "git:https://example.com/repo#rev:abc",
            Some("abc"),
            "tree-sitter-x",
            &temp.path().join("missing.so"),
            &source
        ));
    }

    #[test]
    fn stamp_roundtrips_through_disk() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("parser.c"), "int tree_sitter_x(void) { return 1; }\n").unwrap();
        let output_root = temp.path().join("out");
        fs::create_dir_all(&output_root).unwrap();

        let stamp = GrammarBuildStamp::capture(
            "x",
            "git:https://example.com/repo#rev:abc",
            Some("abc"),
            "tree-sitter-x",
            &source,
        );
        write_stamp(&stamp, &output_root).unwrap();
        let loaded = load_stamp(&output_root, "x").unwrap();
        assert_eq!(loaded, stamp);
        assert_eq!(load_stamp(&output_root, "unknown"), None);
    }
}
