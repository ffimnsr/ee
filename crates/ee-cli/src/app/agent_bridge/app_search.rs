//! `impl App`: directory listing/search proxies and path/glob helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ee_agent_host::AgentError;
use globset::Glob;
use ignore::WalkBuilder;

use super::super::*;

use super::write::ActionLogEntry;

/// Cap on entries returned by one `ee_list_directory` call.
const PROXY_LIST_DIRECTORY_LIMIT: usize = 500;
/// Cap on matches returned by one `ee_search_files` call.
const PROXY_SEARCH_FILES_LIMIT: usize = 500;
/// Cap on matches returned by one `ee_search_text` call.
const PROXY_SEARCH_TEXT_LIMIT: usize = 200;
/// Max visible context bytes returned for one `ee_search_text` match.
const PROXY_SEARCH_TEXT_CONTEXT_BYTES: usize = 200;
/// Cap on diagnostics returned by one Phase 3 diagnostics tool.
pub(super) const PROXY_DIAGNOSTICS_LIMIT: usize = 500;
/// Cap on document symbols returned by one `ee_document_symbols` call.
pub(super) const PROXY_DOCUMENT_SYMBOLS_LIMIT: usize = 500;
/// Cap on references returned by one `ee_references` call.
pub(super) const PROXY_REFERENCES_LIMIT: usize = 500;
/// Cap on code actions returned by one `ee_list_code_actions` call.
pub(super) const PROXY_CODE_ACTIONS_LIMIT: usize = 100;
/// Cap on files returned by one rename preview.
pub(super) const PROXY_RENAME_FILES_LIMIT: usize = 100;
/// Cap on edits returned by one rename preview.
pub(super) const PROXY_RENAME_EDITS_LIMIT: usize = 1000;
/// Cap on symbols returned by one review-context request.
pub(super) const PROXY_REVIEW_SYMBOLS_LIMIT: usize = 500;
/// Cap on changed files queried for document symbols during review-context assembly.
pub(super) const PROXY_REVIEW_SYMBOL_FILE_LIMIT: usize = 32;
/// Max regex pattern length accepted by `ee_search_text_regex`.
const PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES: usize = 4096;
/// Max wall time spent in one regex search before fail-closed timeout.
const PROXY_SEARCH_REGEX_TIMEOUT: Duration = Duration::from_secs(2);

impl App {
    pub(super) fn proxy_list_directory(
        &self,
        path: &Path,
        include_hidden_ignored: bool,
    ) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| AgentError::Io(format!("cannot list {}: {error}", path.display())))?;
        if !canonical.is_dir() {
            return Err(AgentError::invalid_params(format!(
                "path is not a directory: {}",
                canonical.display()
            )));
        }
        if !self.path_in_workspace(&canonical) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                canonical.display()
            )));
        }

        let mut truncated = false;
        let visible = visible_child_paths(&canonical);
        let walker = if include_hidden_ignored {
            WalkBuilder::new(&canonical)
                .max_depth(Some(1))
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_exclude(false)
                .parents(false)
                .build()
        } else {
            WalkBuilder::new(&canonical)
                .max_depth(Some(1))
                .hidden(true)
                .ignore(true)
                .git_ignore(true)
                .git_exclude(true)
                .parents(true)
                .build()
        };
        let mut entries = BTreeMap::new();
        for entry in walker.flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let entry_path = entry.into_path();
            if entry_path.parent() != Some(canonical.as_path()) {
                continue;
            }
            if entries.len() >= PROXY_LIST_DIRECTORY_LIMIT {
                truncated = true;
                break;
            }
            let hidden = is_hidden_path(&entry_path);
            let ignored = !visible.contains(&entry_path);
            let value = if include_hidden_ignored {
                serde_json::to_value(ee_mcp::DirectoryEntryAll {
                    path: entry_path.display().to_string(),
                    kind: file_kind(&entry_path),
                    size: entry_size(&entry_path),
                    hidden,
                    ignored,
                })
            } else {
                serde_json::to_value(ee_mcp::DirectoryEntry {
                    path: entry_path.display().to_string(),
                    kind: file_kind(&entry_path),
                    size: entry_size(&entry_path),
                })
            }
            .map_err(|error| AgentError::HandlerError(error.to_string()))?;
            entries.insert(entry_path.clone(), value);
        }
        if !truncated {
            for buf in self.backend.all_bufs() {
                let Some(buf_path) = &buf.path else {
                    continue;
                };
                let Some(parent) = buf_path.parent() else {
                    continue;
                };
                if !paths_equivalent(parent, &canonical)
                    || entries.len() >= PROXY_LIST_DIRECTORY_LIMIT
                {
                    if entries.len() >= PROXY_LIST_DIRECTORY_LIMIT {
                        truncated = true;
                    }
                    continue;
                }
                entries.entry(buf_path.clone()).or_insert_with(|| {
                    if include_hidden_ignored {
                        serde_json::to_value(ee_mcp::DirectoryEntryAll {
                            path: buf_path.display().to_string(),
                            kind: String::from("file"),
                            size: buffer_visible_size(buf),
                            hidden: is_hidden_path(buf_path),
                            ignored: !visible.contains(buf_path),
                        })
                        .expect("directory entry serializes")
                    } else {
                        serde_json::to_value(ee_mcp::DirectoryEntry {
                            path: buf_path.display().to_string(),
                            kind: String::from("file"),
                            size: buffer_visible_size(buf),
                        })
                        .expect("directory entry serializes")
                    }
                });
            }
        }
        if include_hidden_ignored {
            serde_json::to_value(ee_mcp::ListDirectoryAllResult {
                entries: entries
                    .into_values()
                    .map(|value| serde_json::from_value(value).expect("directory entry parses"))
                    .collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        } else {
            serde_json::to_value(ee_mcp::ListDirectoryResult {
                entries: entries
                    .into_values()
                    .map(|value| serde_json::from_value(value).expect("directory entry parses"))
                    .collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        }
    }

    pub(super) fn proxy_search_files(
        &self,
        pattern: &str,
        include_hidden_ignored: bool,
    ) -> Result<serde_json::Value, AgentError> {
        if pattern.is_empty() {
            return Err(AgentError::invalid_params("pattern must not be empty"));
        }
        let matcher = build_path_matcher(pattern)?;
        let roots = self.canonical_workspace_roots();
        let visible_by_root: Vec<(PathBuf, BTreeSet<PathBuf>)> = roots
            .iter()
            .cloned()
            .map(|root| {
                let visible = visible_descendant_paths(&root);
                (root, visible)
            })
            .collect();
        let mut matches = BTreeMap::new();
        let mut truncated = false;
        for (root, visible) in &visible_by_root {
            let walker = if include_hidden_ignored {
                WalkBuilder::new(root)
                    .hidden(false)
                    .ignore(false)
                    .git_ignore(false)
                    .git_exclude(false)
                    .parents(false)
                    .build()
            } else {
                WalkBuilder::new(root)
                    .hidden(true)
                    .ignore(true)
                    .git_ignore(true)
                    .git_exclude(true)
                    .parents(true)
                    .build()
            };
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.into_path();
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if !matcher(rel, &path) {
                    continue;
                }
                let path_text = path.display().to_string();
                if include_hidden_ignored {
                    matches.entry(path_text.clone()).or_insert_with(|| {
                        serde_json::to_value(ee_mcp::FileMatch {
                            path: path_text,
                            hidden: is_hidden_path(&path),
                            ignored: !visible.contains(&path),
                        })
                        .expect("file match serializes")
                    });
                } else {
                    matches.entry(path_text).or_insert(serde_json::Value::Null);
                }
                if matches.len() >= PROXY_SEARCH_FILES_LIMIT {
                    truncated = true;
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        if !truncated {
            for buf in self.backend.all_bufs() {
                let Some(path) = &buf.path else {
                    continue;
                };
                if !path.is_absolute() || !self.path_in_workspace(path) {
                    continue;
                }
                let rel = roots
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                    .unwrap_or(path.as_path());
                if !matcher(rel, path) {
                    continue;
                }
                let path_text = path.display().to_string();
                if include_hidden_ignored {
                    let ignored = visible_by_root
                        .iter()
                        .find(|(root, _)| path.starts_with(root))
                        .is_some_and(|(_, visible)| !visible.contains(path));
                    matches.entry(path_text.clone()).or_insert_with(|| {
                        serde_json::to_value(ee_mcp::FileMatch {
                            path: path_text,
                            hidden: is_hidden_path(path),
                            ignored,
                        })
                        .expect("file match serializes")
                    });
                } else {
                    matches.entry(path_text).or_insert(serde_json::Value::Null);
                }
                if matches.len() >= PROXY_SEARCH_FILES_LIMIT {
                    truncated = true;
                    break;
                }
            }
        }
        if include_hidden_ignored {
            serde_json::to_value(ee_mcp::SearchFilesAllResult {
                matches: matches
                    .into_values()
                    .map(|value| serde_json::from_value(value).expect("file match parses"))
                    .collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        } else {
            serde_json::to_value(ee_mcp::SearchFilesResult {
                matches: matches.into_keys().collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        }
    }

    pub(super) fn proxy_search_text(&self, query: &str) -> Result<serde_json::Value, AgentError> {
        if query.is_empty() {
            return Err(AgentError::invalid_params("query must not be empty"));
        }
        let matches = self.collect_text_matches(|path, line_number, line| {
            Ok(line.contains(query).then(|| ee_mcp::TextMatch {
                path: path.display().to_string(),
                line: line_number,
                context: trim_search_context(line, query),
            }))
        })?;
        serde_json::to_value(ee_mcp::SearchTextResult {
            truncated: matches.len() >= PROXY_SEARCH_TEXT_LIMIT,
            matches,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_search_text_regex(
        &self,
        pattern: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let regex = compile_search_regex(pattern)?;
        let deadline = Instant::now() + PROXY_SEARCH_REGEX_TIMEOUT;
        let matches = self.collect_text_matches(|path, line_number, line| {
            if Instant::now() >= deadline {
                return Err(AgentError::Io(format!(
                    "regex search timed out after {:?}",
                    PROXY_SEARCH_REGEX_TIMEOUT
                )));
            }
            Ok(regex.is_match(line).then(|| ee_mcp::TextMatch {
                path: path.display().to_string(),
                line: line_number,
                context: trim_regex_context(line, &regex),
            }))
        })?;
        serde_json::to_value(ee_mcp::SearchTextResult {
            truncated: matches.len() >= PROXY_SEARCH_TEXT_LIMIT,
            matches,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_search_text_in_files(
        &self,
        query: &str,
        file_glob: &str,
    ) -> Result<serde_json::Value, AgentError> {
        if query.is_empty() {
            return Err(AgentError::invalid_params("query must not be empty"));
        }
        let matcher = build_path_matcher(file_glob)?;
        let roots = self.canonical_workspace_roots();
        let matches = self.collect_text_matches(|path, line_number, line| {
            let rel = roots.iter().find_map(|root| path.strip_prefix(root).ok()).unwrap_or(path);
            if !matcher(rel, path) || !line.contains(query) {
                return Ok(None);
            }
            Ok(Some(ee_mcp::TextMatch {
                path: path.display().to_string(),
                line: line_number,
                context: trim_search_context(line, query),
            }))
        })?;
        serde_json::to_value(ee_mcp::SearchTextResult {
            truncated: matches.len() >= PROXY_SEARCH_TEXT_LIMIT,
            matches,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn collect_text_matches(
        &self,
        mut match_line: impl FnMut(&Path, u32, &str) -> Result<Option<ee_mcp::TextMatch>, AgentError>,
    ) -> Result<Vec<ee_mcp::TextMatch>, AgentError> {
        let mut matches = Vec::new();
        let mut seen_open_paths = BTreeSet::new();
        for buf in self.backend.all_bufs() {
            let Some(path) = &buf.path else {
                continue;
            };
            if buf.is_vlf || !self.path_in_workspace(path) {
                continue;
            }
            seen_open_paths.insert(path.clone());
            for (index, line) in buf.lines.iter().enumerate() {
                if let Some(text_match) =
                    match_line(path, u32::try_from(index + 1).unwrap_or(u32::MAX), line)?
                {
                    matches.push(text_match);
                    if matches.len() >= PROXY_SEARCH_TEXT_LIMIT {
                        return Ok(matches);
                    }
                }
            }
        }
        'roots: for root in self.canonical_workspace_roots() {
            let walker = WalkBuilder::new(&root)
                .hidden(true)
                .ignore(true)
                .git_ignore(true)
                .git_exclude(true)
                .parents(true)
                .build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.into_path();
                if seen_open_paths.iter().any(|open| paths_equivalent(open, &path)) {
                    continue;
                }
                let content = match std::fs::read_to_string(&path) {
                    Ok(content) => content,
                    Err(_) => continue,
                };
                for (index, line) in content.lines().enumerate() {
                    if let Some(text_match) =
                        match_line(&path, u32::try_from(index + 1).unwrap_or(u32::MAX), line)?
                    {
                        matches.push(text_match);
                        if matches.len() >= PROXY_SEARCH_TEXT_LIMIT {
                            break 'roots;
                        }
                    }
                }
            }
        }
        Ok(matches)
    }

    /// The recorded agent file operations (tests, future checkpointing).
    #[allow(dead_code)]
    pub(crate) fn agents_action_log(&self) -> &[ActionLogEntry] {
        &self.agents.action_log
    }
}

fn visible_child_paths(dir: &Path) -> BTreeSet<PathBuf> {
    let mut visible = BTreeSet::new();
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.into_path();
        if path.parent() == Some(dir) {
            visible.insert(path);
        }
    }
    visible
}

fn visible_descendant_paths(root: &Path) -> BTreeSet<PathBuf> {
    let mut visible = BTreeSet::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            visible.insert(entry.into_path());
        }
    }
    visible
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with('.'))
}

fn file_kind(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => String::from("symlink"),
        Ok(metadata) if metadata.is_dir() => String::from("directory"),
        Ok(metadata) if metadata.is_file() => String::from("file"),
        Ok(_) => String::from("other"),
        Err(_) => String::from("other"),
    }
}

fn entry_size(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

fn buffer_visible_size(buf: &crate::buffer::BufState) -> u64 {
    if buf.is_vlf {
        return 0;
    }
    buf.lines.iter().map(|line| line.len() + 1).sum::<usize>() as u64
}

type PathMatcher = Box<dyn Fn(&Path, &Path) -> bool + Send + Sync>;

fn build_path_matcher(pattern: &str) -> Result<PathMatcher, AgentError> {
    if pattern.chars().any(|ch| matches!(ch, '*' | '?' | '[' | '{')) {
        let glob = Glob::new(pattern)
            .map_err(|error| AgentError::invalid_params(format!("invalid glob pattern: {error}")))?
            .compile_matcher();
        Ok(Box::new(move |rel, path| glob.is_match(rel) || glob.is_match(path)))
    } else {
        let literal = pattern.to_string();
        Ok(Box::new(move |rel, path| {
            let rel = rel.to_string_lossy();
            let path_text = path.to_string_lossy();
            let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            rel.contains(&literal) || path_text.contains(&literal) || file_name.contains(&literal)
        }))
    }
}

fn compile_search_regex(pattern: &str) -> Result<regex::Regex, AgentError> {
    if pattern.is_empty() {
        return Err(AgentError::invalid_params("pattern must not be empty"));
    }
    if pattern.len() > PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES {
        return Err(AgentError::invalid_params(format!(
            "regex pattern exceeds {} bytes",
            PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES
        )));
    }
    regex::RegexBuilder::new(pattern)
        .unicode(true)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 22)
        .build()
        .map_err(|error| AgentError::invalid_params(format!("invalid regex pattern: {error}")))
}

fn trim_search_context(line: &str, query: &str) -> String {
    let Some(start) = line.find(query) else {
        return truncate_chars(line, PROXY_SEARCH_TEXT_CONTEXT_BYTES);
    };
    let end = start.saturating_add(query.len());
    let left_budget = PROXY_SEARCH_TEXT_CONTEXT_BYTES / 2;
    let right_budget = PROXY_SEARCH_TEXT_CONTEXT_BYTES.saturating_sub(left_budget);
    let left_start = previous_char_boundary(line, start.saturating_sub(left_budget));
    let right_end = next_char_boundary(line, (end + right_budget).min(line.len()));
    let mut context = line[left_start..right_end].to_string();
    if left_start > 0 {
        context = format!("…{context}");
    }
    if right_end < line.len() {
        context.push('…');
    }
    context
}

fn trim_regex_context(line: &str, regex: &regex::Regex) -> String {
    regex.find(line).map_or_else(
        || truncate_chars(line, PROXY_SEARCH_TEXT_CONTEXT_BYTES),
        |found| trim_search_context(line, found.as_str()),
    )
}

fn next_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = text[..cut].to_string();
    if cut < text.len() {
        truncated.push('…');
    }
    truncated
}

/// Equivalent-path check: canonical equality when both resolve, lexical
/// equality otherwise.
pub(super) fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return ca == cb;
    }
    a == b
}
