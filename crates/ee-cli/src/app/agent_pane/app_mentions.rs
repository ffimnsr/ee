//! `impl App`: composer `@` mention completion, picker, and insertion.

use std::path::PathBuf;

use crate::picker::{PickerItem, PickerState};
use crate::policy::is_protected_relative_path;
use ignore::WalkBuilder;

use super::super::*;

/// Files scanned per picker open before the walk stops.
const MENTION_PICKER_SCAN_MAX: usize = 20_000;
/// Workspace files listed in the mention picker.
const MENTION_PICKER_FILE_LIMIT: usize = 2_000;
/// Files scanned while resolving one path prefix.
const MENTION_COMPLETION_SCAN_MAX: usize = 1_024;
/// Read-on-demand file reference token prefix.
const REFERENCE_MENTION_PREFIX: &str = "ref:";

impl App {
    /// Completes one trailing `@workspace/path` token, or opens the mention
    /// picker when the token is bare `@` or a path prefix is ambiguous.
    /// Completion never reads or attaches file content.
    pub(super) fn agents_complete_mention_path(&mut self) -> bool {
        let draft = self
            .agents
            .active_thread_index()
            .and_then(|active| self.agents.threads.get(active).map(|thread| thread.draft.clone()))
            .unwrap_or_else(|| self.agents.pending_draft.clone());
        let Some((_, partial)) = trailing_mention_token(&draft) else {
            return false;
        };
        if partial.contains(['\\', '\n', '\r']) {
            return false;
        }
        if partial.is_empty() {
            return self.agents_open_mention_picker("");
        }
        if let Some(reference) = partial.strip_prefix(REFERENCE_MENTION_PREFIX) {
            return self.agents_complete_reference_path(reference);
        }
        let matches = self.mention_path_matches(partial);
        match matches.as_slice() {
            [completed] => {
                let replacement = format!("@{completed}");
                if !self.agents_replace_trailing_mention(&replacement) {
                    return false;
                }
                self.backend.status_message = Some(format!("mention path completed: {completed}"));
                true
            }
            [] => false,
            _ => self.agents_open_mention_picker(partial),
        }
    }

    /// Opens a picker over workspace files plus `@skill:` and `@memory:`
    /// names. `seed` pre-filters the list for ambiguous path prefixes.
    fn agents_open_mention_picker(&mut self, seed: &str) -> bool {
        let items = self.mention_picker_items();
        if items.is_empty() {
            self.backend.status_message =
                Some(String::from("no workspace files, skills, or memories for @ mention"));
            return true;
        }
        let mut picker = PickerState::new_agent_mentions(items);
        for ch in seed.chars() {
            picker.push_char(ch);
        }
        self.open_picker(picker);
        self.backend.status_message = Some(String::from(
            "insert mention: files, @skill:<name>, @memory:<name>, @ref:<path> (no content)",
        ));
        true
    }

    /// Completes a trailing `@ref:<path>` token. `partial` is the path part
    /// after `ref:`; it never reads file content.
    fn agents_complete_reference_path(&mut self, partial: &str) -> bool {
        if partial.is_empty() {
            return self.agents_open_reference_picker("");
        }
        let matches = self.mention_path_matches(partial);
        match matches.as_slice() {
            [completed] => {
                let replacement = format!("@{REFERENCE_MENTION_PREFIX}{completed}");
                if !self.agents_replace_trailing_mention(&replacement) {
                    return false;
                }
                self.backend.status_message =
                    Some(format!("reference path completed: {completed}"));
                true
            }
            [] => false,
            _ => self.agents_open_reference_picker(partial),
        }
    }

    /// Opens a file picker that inserts read-on-demand `@ref:<path>` tokens.
    fn agents_open_reference_picker(&mut self, seed: &str) -> bool {
        let items = self
            .mention_picker_file_paths()
            .into_iter()
            .map(|path| mention_item(format!("@{REFERENCE_MENTION_PREFIX}{path}"), "reference"))
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no workspace files for @ref mention"));
            return true;
        }
        let mut picker = PickerState::new_agent_mentions(items);
        picker.title = String::from("Reference File");
        for ch in seed.chars() {
            picker.push_char(ch);
        }
        self.open_picker(picker);
        self.backend.status_message = Some(String::from(
            "insert reference: @ref:<path> (agent reads it on demand; content stays out of context)",
        ));
        true
    }

    /// Replaces the composer's trailing `@...` token with an accepted mention.
    /// The bare `@ref:` template keeps the picker flow going: accepting it
    /// opens the reference file picker instead of leaving a dangling prefix.
    pub(crate) fn agents_insert_mention(&mut self, mention: &str) {
        if !self.agents_replace_trailing_mention(mention) {
            self.backend.status_message =
                Some(String::from("mention picker closed: composer has no trailing @ token"));
            return;
        }
        if mention.strip_prefix('@') == Some(REFERENCE_MENTION_PREFIX) {
            self.agents_open_reference_picker("");
            return;
        }
        self.backend.status_message = Some(format!("mention inserted: {mention}"));
    }

    fn agents_replace_trailing_mention(&mut self, mention: &str) -> bool {
        let replacement =
            if mention.starts_with('@') { mention.to_string() } else { format!("@{mention}") };
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            let Some((token_start, _)) = trailing_mention_token(&thread.draft) else {
                return false;
            };
            thread.draft.replace_range(token_start.., &replacement);
            thread.draft_cursor_to_end();
        } else {
            let Some((token_start, _)) = trailing_mention_token(&self.agents.pending_draft) else {
                return false;
            };
            self.agents.pending_draft.replace_range(token_start.., &replacement);
        }
        true
    }

    /// Safe workspace files matching a path prefix, capped at two so the
    /// caller can distinguish unique from ambiguous.
    fn mention_path_matches(&self, partial: &str) -> Vec<String> {
        let Ok(root) = std::fs::canonicalize(&self.working_dir) else {
            return Vec::new();
        };
        let mut matches = Vec::new();
        for entry in WalkBuilder::new(&root)
            .max_depth(Some(12))
            .standard_filters(true)
            .build()
            .filter_map(Result::ok)
            .take(MENTION_COMPLETION_SCAN_MAX)
        {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&root) else {
                continue;
            };
            let relative = slash_join(relative);
            if relative.is_empty()
                || !relative.starts_with(partial)
                || is_protected_relative_path(&relative)
                || self.is_secret_store_path(entry.path())
            {
                continue;
            }
            matches.push(relative);
            if matches.len() > 1 {
                break;
            }
        }
        matches
    }

    fn mention_picker_items(&self) -> Vec<PickerItem> {
        let workspace_roots: Vec<PathBuf> =
            self.agents.additional_workspace_roots.iter().cloned().collect();
        let config_root = crate::config::xi_core_config_dir();
        let names = super::mention_catalog::discover(
            &self.working_dir,
            &workspace_roots,
            config_root.as_deref(),
        );
        let mut items = Vec::with_capacity(names.skills.len() + names.memories.len() + 1);
        // First entry is the reference template; selecting it opens the
        // reference file picker so `@ref:` is discoverable from bare `@`.
        items.push(mention_item(format!("@{REFERENCE_MENTION_PREFIX}"), "reference file"));
        for name in names.skills {
            items.push(mention_item(format!("@skill:{name}"), "skill"));
        }
        for name in names.memories {
            items.push(mention_item(format!("@memory:{name}"), "memory"));
        }
        items.extend(
            self.mention_picker_file_paths()
                .into_iter()
                .map(|path| mention_item(format!("@{path}"), "file")),
        );
        items
    }

    fn mention_picker_file_paths(&self) -> Vec<String> {
        let Ok(root) = std::fs::canonicalize(&self.working_dir) else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for entry in WalkBuilder::new(&root)
            .max_depth(Some(12))
            .standard_filters(true)
            .build()
            .filter_map(Result::ok)
            .take(MENTION_PICKER_SCAN_MAX)
        {
            if files.len() >= MENTION_PICKER_FILE_LIMIT {
                break;
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&root) else {
                continue;
            };
            let relative = slash_join(relative);
            if relative.is_empty()
                || is_protected_relative_path(&relative)
                || self.is_secret_store_path(entry.path())
            {
                continue;
            }
            files.push(relative);
        }
        files.sort();
        files
    }
}

fn mention_item(token: String, kind: &str) -> PickerItem {
    PickerItem {
        label: token,
        detail: Some(kind.to_string()),
        path: None,
        buf_id: None,
        line: None,
        col: None,
        choice_index: None,
    }
}

/// Returns the trailing whitespace-delimited `@token` split as
/// `(token_start, partial)`. `partial` is empty for a bare `@`.
fn trailing_mention_token(draft: &str) -> Option<(usize, &str)> {
    let token_start = draft
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    let partial = draft.get(token_start..)?.strip_prefix('@')?;
    Some((token_start, partial))
}

fn slash_join(path: &std::path::Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_mention_token_requires_a_bare_word_prefix() {
        assert_eq!(trailing_mention_token("review @"), Some((7, "")));
        assert_eq!(trailing_mention_token("@src/lib"), Some((0, "src/lib")));
        assert_eq!(trailing_mention_token("review @src/lib"), Some((7, "src/lib")));
        assert_eq!(trailing_mention_token("review @src/lib tail"), None);
        assert_eq!(trailing_mention_token("no mention"), None);
    }

    #[test]
    fn slash_join_uses_forward_slashes() {
        let path = std::path::Path::new("src").join("app").join("main.rs");
        assert_eq!(slash_join(&path), "src/app/main.rs");
    }
}
