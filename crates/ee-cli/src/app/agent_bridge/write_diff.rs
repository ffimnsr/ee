//! Bounded unified diff preview for write approvals.
//!
//! The approval prompt previously showed only `path (N bytes)`, so a user had
//! to allow a write blind.  This module turns the prepared content and the
//! current baseline into a bounded `@@`-grouped unified diff.  Previewing is
//! advisory only: the write itself, its validation, and its save pipeline are
//! unchanged, and every failure path falls back to the plain prompt.

use std::path::Path;

use similar::TextDiff;

use super::super::*;

use super::app_search::paths_equivalent;
use super::approval::{ApprovalKind, WriteExpectation};
use super::prompt::ApprovalPrompt;

/// Diff context lines kept around each change group.
const DIFF_CONTEXT_RADIUS: usize = 3;
/// Body lines shown before the user expands the preview.
pub(crate) const COLLAPSED_DIFF_LINES: usize = 10;
/// Upper bound on body lines retained for an expanded preview.
pub(crate) const MAX_DIFF_LINES: usize = 240;
/// Files above this size skip the diff and report a summary instead.
pub(crate) const MAX_DIFF_BYTES: usize = 512 * 1024;

/// Baseline text used to build the diff.
#[derive(Debug)]
pub(crate) enum WriteBaseline {
    /// The target does not exist yet (or cannot be read).
    Missing,
    /// Current buffer text when open, otherwise the on-disk content.
    Content(String),
    /// The baseline exists but is too large to diff inline.
    TooLarge,
}

/// Rendered preview: effect label, bounded diff body, and counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteDiffPreview {
    /// Human label for the write effect, e.g. `creates file`.
    pub(crate) effect: String,
    /// `@@` headers and `+` / `-` / ` ` body lines, bounded.
    pub(crate) lines: Vec<String>,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    /// Body lines dropped by [`MAX_DIFF_LINES`].
    pub(crate) omitted: usize,
    /// The diff was skipped because a side exceeded [`MAX_DIFF_BYTES`].
    pub(crate) too_large: bool,
}

impl WriteDiffPreview {
    /// Body lines visible in the current expansion state.
    pub(crate) fn visible_lines(&self, expanded: bool) -> &[String] {
        let limit = if expanded { MAX_DIFF_LINES } else { COLLAPSED_DIFF_LINES };
        &self.lines[..self.lines.len().min(limit)]
    }

    /// Body lines hidden by the collapsed state.
    pub(crate) fn hidden_by_collapse(&self, expanded: bool) -> usize {
        if expanded { 0 } else { self.lines.len().saturating_sub(COLLAPSED_DIFF_LINES) }
    }

    /// Whether a toggle key has anything to reveal or hide.
    pub(crate) fn is_expandable(&self) -> bool {
        !self.lines.is_empty()
    }
}

impl ApprovalPrompt {
    /// Toggles the diff expansion state; returns the new state.
    pub(crate) fn toggle_diff_expanded(&mut self) -> bool {
        if self.diff_preview.as_ref().is_some_and(WriteDiffPreview::is_expandable) {
            self.diff_expanded = !self.diff_expanded;
        }
        self.diff_expanded
    }
}

impl App {
    /// Attaches a bounded diff preview to a queued write prompt.  Runs only
    /// when the prompt actually reaches the UI; every failure leaves the
    /// preview unset so the plain `path (N bytes)` prompt still works.
    pub(super) fn attach_write_diff_preview(&self, prompt: &mut ApprovalPrompt) {
        if prompt.diff_preview.is_some() {
            return;
        }
        let ApprovalKind::Write { path, content, expectation, .. } = &prompt.kind else {
            return;
        };
        if !path.is_absolute() || !self.preview_path_in_scope(path) {
            return;
        }
        let baseline = self.write_preview_baseline(path);
        prompt.diff_preview = Some(build_write_diff_preview(baseline, content, expectation));
    }

    /// Containment check for the preview only.  A file that does not exist yet
    /// cannot be canonicalized, so its parent directory is resolved instead;
    /// this keeps new files inside symlinked roots (for example `/var` on
    /// macOS) previewable without widening the real write gate.
    fn preview_path_in_scope(&self, path: &Path) -> bool {
        if self.path_in_effective_workspace(path) {
            return true;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        std::fs::canonicalize(parent).is_ok_and(|dir| {
            self.allowed_fs_roots().iter().any(|root| dir.join(name).starts_with(root))
        })
    }

    /// Current baseline for a write preview: the open buffer wins over disk so
    /// unsaved text is never misrepresented.  Unreadable baselines are treated
    /// as missing; oversized ones skip the diff.
    fn write_preview_baseline(&self, path: &Path) -> WriteBaseline {
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|open| paths_equivalent(open, path)))
        {
            // Check size before materializing the whole buffer text.
            if buf.is_vlf {
                return WriteBaseline::Missing;
            }
            let bytes = buf.lines.iter().map(String::len).sum::<usize>();
            if bytes > MAX_DIFF_BYTES {
                return WriteBaseline::TooLarge;
            }
            return match buf.whole_text() {
                Some(text) => WriteBaseline::Content(text),
                None => WriteBaseline::Missing,
            };
        }
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_DIFF_BYTES as u64) {
            return WriteBaseline::TooLarge;
        }
        match std::fs::read_to_string(path) {
            Ok(text) => WriteBaseline::Content(text),
            Err(_) => WriteBaseline::Missing,
        }
    }
}

/// Builds the preview.  `Content` renders a `@@`-grouped unified diff; a
/// missing baseline renders the prepared content as all-added lines.
pub(crate) fn build_write_diff_preview(
    baseline: WriteBaseline,
    content: &str,
    expectation: &WriteExpectation,
) -> WriteDiffPreview {
    let exists = !matches!(baseline, WriteBaseline::Missing);
    let effect = effect_label(expectation, exists);
    if matches!(baseline, WriteBaseline::TooLarge) || content.len() > MAX_DIFF_BYTES {
        return WriteDiffPreview {
            effect,
            lines: Vec::new(),
            additions: 0,
            deletions: 0,
            omitted: 0,
            too_large: true,
        };
    }
    let old = match &baseline {
        WriteBaseline::Content(text) => text.as_str(),
        WriteBaseline::Missing | WriteBaseline::TooLarge => "",
    };
    let rendered = TextDiff::from_lines(old, content)
        .unified_diff()
        .context_radius(DIFF_CONTEXT_RADIUS)
        .to_string();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut body = Vec::new();
    for line in rendered.lines().skip_while(|line| !line.starts_with("@@")) {
        match line.as_bytes().first() {
            Some(b'+') => additions += 1,
            Some(b'-') => deletions += 1,
            _ => {}
        }
        body.push(line.to_string());
    }
    let omitted = body.len().saturating_sub(MAX_DIFF_LINES);
    body.truncate(MAX_DIFF_LINES);
    WriteDiffPreview { effect, lines: body, additions, deletions, omitted, too_large: false }
}

fn effect_label(expectation: &WriteExpectation, exists: bool) -> String {
    match expectation {
        WriteExpectation::MustNotExist if exists => String::from("file already exists"),
        WriteExpectation::MustNotExist => String::from("creates file"),
        WriteExpectation::ExpectRevision(revision) => format!("replaces revision {revision}"),
        WriteExpectation::Blind if exists => String::from("replaces existing content"),
        WriteExpectation::Blind => String::from("creates file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(
        baseline: WriteBaseline,
        content: &str,
        expectation: &WriteExpectation,
    ) -> WriteDiffPreview {
        build_write_diff_preview(baseline, content, expectation)
    }

    #[test]
    fn existing_file_renders_a_unified_diff() {
        let preview = preview(
            WriteBaseline::Content(String::from("alpha\nbeta\ngamma\n")),
            "alpha\nBETA\ngamma\n",
            &WriteExpectation::Blind,
        );
        assert_eq!(preview.effect, "replaces existing content");
        assert_eq!((preview.additions, preview.deletions), (1, 1));
        assert!(preview.lines.iter().any(|line| line == "-beta"));
        assert!(preview.lines.iter().any(|line| line == "+BETA"));
        assert!(preview.lines.first().is_some_and(|line| line.starts_with("@@")));
    }

    #[test]
    fn missing_file_renders_created_lines() {
        let preview =
            preview(WriteBaseline::Missing, "line one\nline two\n", &WriteExpectation::Blind);
        assert_eq!(preview.effect, "creates file");
        assert_eq!((preview.additions, preview.deletions), (2, 0));
        assert!(preview.lines.iter().all(|line| !line.starts_with('-')));
    }

    #[test]
    fn expectations_render_their_own_effect() {
        let create = preview(WriteBaseline::Missing, "x\n", &WriteExpectation::MustNotExist);
        assert_eq!(create.effect, "creates file");
        let clash = preview(
            WriteBaseline::Content(String::from("x\n")),
            "x\n",
            &WriteExpectation::MustNotExist,
        );
        assert_eq!(clash.effect, "file already exists");
        let replace = preview(
            WriteBaseline::Content(String::from("old\n")),
            "new\n",
            &WriteExpectation::ExpectRevision(String::from("0123456789abcdef")),
        );
        assert_eq!(replace.effect, "replaces revision 0123456789abcdef");
    }

    #[test]
    fn unchanged_content_has_no_body() {
        let preview = preview(
            WriteBaseline::Content(String::from("same\n")),
            "same\n",
            &WriteExpectation::Blind,
        );
        assert!(preview.lines.is_empty());
        assert_eq!((preview.additions, preview.deletions), (0, 0));
    }

    #[test]
    fn collapsed_and_expanded_visibility_is_bounded() {
        let content: String = (0..200).map(|index| format!("line {index}\n")).collect();
        let preview = preview(WriteBaseline::Missing, &content, &WriteExpectation::Blind);
        assert!(preview.lines.len() > COLLAPSED_DIFF_LINES);
        assert_eq!(preview.visible_lines(false).len(), COLLAPSED_DIFF_LINES);
        assert_eq!(preview.hidden_by_collapse(true), 0);
        assert_eq!(preview.visible_lines(true).len(), preview.lines.len().min(MAX_DIFF_LINES));
    }

    #[test]
    fn oversized_retention_reports_omitted_lines() {
        let content: String =
            (0..(MAX_DIFF_LINES + 40)).map(|index| format!("line {index}\n")).collect();
        let preview = preview(WriteBaseline::Missing, &content, &WriteExpectation::Blind);
        assert_eq!(preview.lines.len(), MAX_DIFF_LINES);
        assert!(preview.omitted >= 40);
        assert_eq!(preview.visible_lines(true).len(), MAX_DIFF_LINES);
    }

    #[test]
    fn too_large_baseline_skips_the_diff() {
        let preview = preview(WriteBaseline::TooLarge, "content\n", &WriteExpectation::Blind);
        assert!(preview.too_large);
        assert!(preview.lines.is_empty());
        assert_eq!(preview.effect, "replaces existing content");
    }

    #[test]
    fn distant_change_keeps_hunk_grouping() {
        let old: String = (0..500).map(|index| format!("old {index}\n")).collect();
        let mut new: String = old.clone();
        new.push_str("tail\n");
        let preview = preview(WriteBaseline::Content(old), &new, &WriteExpectation::Blind);
        // A single trailing change must not emit 500 context lines.
        assert!(preview.lines.len() < 20, "lines: {:?}", preview.lines.len());
        assert!(preview.lines.iter().any(|line| line == "+tail"));
    }
}
