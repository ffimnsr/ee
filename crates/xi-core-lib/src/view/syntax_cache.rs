// Copyright 2026 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Per-view cache for backend syntax spans.
//!
//! Span production (parse plus query walk) is the largest per-render cost on the
//! interactive path, and the render plan is allowed to discard rows that fall
//! outside the viewport. Scrolling back therefore re-walks rows the core had already
//! highlighted a moment ago — measured at ~2.2 ms and 159 span rows for a 200-line
//! window.
//!
//! # Why the cache covers rows instead of matching requests
//!
//! A return to a discarded window does not ask for the window the way it was asked
//! for the first time: the plan re-segments it (measured: the cold pass asks for
//! `0..199`, the return asks for `0..12` and `12..199`), because the shadow's spans
//! split the render differently. An exact-window key therefore never hits on the one
//! flow this cache exists for.
//!
//! So an entry records the rows a walk produced, and a request is served when the
//! entries together cover every row it asks for, assembled row by row in
//! least-recently-used order. Rows are therefore always spans that an actual walk
//! produced (never a re-derivation), and a request that is not fully covered walks
//! exactly like the uncached path does.
//!
//! # Invalidation
//!
//! Entries are dropped whenever the document text or the wrapping changes:
//!
//! - [`View::after_edit`] — the text changed.
//! - [`View::rewrap`] — visual line numbering changed, so a row index means
//!   something else.
//!
//! Language and syntax toggles are part of an entry's identity, so they cannot serve
//! each other. Each entry also stores the byte offset its first row resolved to when
//! it was walked, and is dropped when that offset no longer matches: a missed
//! invalidation (a mutation path that forgets to call `after_edit`) becomes a cache
//! miss instead of stale highlighting.

use super::*;

/// One walked window of spans.
#[derive(Debug)]
struct Entry {
    /// First row this entry describes.
    start_line: usize,
    language: String,
    syntax_enabled: bool,
    /// Byte offset `start_line` resolved to when the window was walked.
    start_offset: usize,
    /// Spans per row, `rows[i]` for `start_line + i`.
    rows: Vec<Vec<VisibleSyntaxSpan>>,
}

impl Entry {
    fn end_line(&self) -> usize {
        self.start_line + self.rows.len()
    }

    fn matches_request(&self, language: &str, syntax_enabled: bool) -> bool {
        self.syntax_enabled == syntax_enabled && self.language == language
    }
}

/// Least-recently-used cache of walked span windows for one view.
#[derive(Debug, Default)]
pub(super) struct SyntaxSpanCache {
    /// Least-recently-used first.
    entries: Vec<Entry>,
    hits: u64,
    misses: u64,
}

impl SyntaxSpanCache {
    /// Windows kept per view.
    ///
    /// Eight covers a scroll-back session over a few viewports (a 200-line window of
    /// source text is ~15 KB of spans) while staying far below what a long session
    /// could accumulate if every window were kept.
    pub(super) const MAX_ENTRIES: usize = 8;

    /// Rows kept per view across all windows.
    ///
    /// Bounds the memory a pathological plan (one segment covering the document)
    /// could pin; ~16 viewports of source text.
    pub(super) const MAX_ROWS: usize = 4_096;

    /// Spans for `start_line..start_line + line_count`, when covered.
    pub(super) fn get(
        &mut self,
        start_line: usize,
        line_count: usize,
        language: &str,
        syntax_enabled: bool,
    ) -> Option<Vec<Vec<VisibleSyntaxSpan>>> {
        if line_count == 0 {
            return Some(Vec::new());
        }
        let end_line = start_line + line_count;
        let mut rows: Vec<Option<Vec<VisibleSyntaxSpan>>> = vec![None; line_count];
        let mut served_by: Vec<usize> = Vec::new();

        // Most recently used first, so a row covered by several windows is served by
        // the newest walk.
        for index in (0..self.entries.len()).rev() {
            let entry = &self.entries[index];
            if !entry.matches_request(language, syntax_enabled) {
                continue;
            }
            let from = start_line.max(entry.start_line);
            let to = end_line.min(entry.end_line());
            if from >= to {
                continue;
            }
            served_by.push(index);
            for row in from..to {
                let slot = &mut rows[row - start_line];
                if slot.is_none() {
                    *slot = Some(entry.rows[row - entry.start_line].clone());
                }
            }
        }

        if rows.iter().any(Option::is_none) {
            self.misses += 1;
            return None;
        }
        self.hits += 1;
        // Touch what served this request: those windows are the ones likely to be
        // asked for again.
        served_by.sort_unstable();
        for index in served_by.into_iter().rev() {
            let entry = self.entries.remove(index);
            self.entries.push(entry);
        }
        Some(rows.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Records one walked window.
    pub(super) fn insert(
        &mut self,
        start_line: usize,
        start_offset: usize,
        language: &str,
        syntax_enabled: bool,
        rows: Vec<Vec<VisibleSyntaxSpan>>,
    ) {
        if rows.is_empty() {
            return;
        }
        let end_line = start_line + rows.len();
        // A newer walk of the same rows replaces the older one (they describe the
        // same text, but the newer one matches how the renderer asked for them).
        self.entries.retain(|entry| {
            !(entry.start_line == start_line
                && entry.end_line() == end_line
                && entry.matches_request(language, syntax_enabled))
        });
        self.entries.push(Entry {
            start_line,
            language: language.to_owned(),
            syntax_enabled,
            start_offset,
            rows,
        });
        self.enforce_caps();
    }

    /// Drops entries whose first row no longer sits at the byte offset it was walked
    /// at, and enforces the size caps (least-recently-used first).
    fn enforce_caps(&mut self) {
        while self.entries.len() > Self::MAX_ENTRIES
            || self.entries.iter().map(|entry| entry.rows.len()).sum::<usize>() > Self::MAX_ROWS
        {
            self.entries.remove(0);
        }
    }

    /// `(start_line, start_offset)` of every entry, for the staleness guard.
    pub(super) fn entry_starts(&self) -> Vec<(usize, usize)> {
        self.entries.iter().map(|entry| (entry.start_line, entry.start_offset)).collect()
    }

    /// Drops entries whose first row no longer sits at the offset it was walked at.
    pub(super) fn drop_moved_entries(&mut self, current: &[(usize, usize)]) {
        self.entries.retain(|entry| {
            current
                .iter()
                .any(|(line, offset)| *line == entry.start_line && *offset == entry.start_offset)
        });
    }

    /// Drops every entry. Called whenever the text or the wrapping changed.
    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
    }

    /// `(hits, misses, cached windows, cached rows)`, for tests and diagnostics.
    #[cfg(test)]
    pub(super) fn stats(&self) -> (u64, u64, usize, usize) {
        (
            self.hits,
            self.misses,
            self.entries.len(),
            self.entries.iter().map(|e| e.rows.len()).sum(),
        )
    }
}

impl View {
    /// Backend syntax spans for one segment, served from the per-view cache when the
    /// rows it asks for were already walked for the same text.
    ///
    /// Non-rope sources (VLF) and disabled syntax return empty spans without a walk,
    /// so they are not worth a cache entry.
    pub(super) fn cached_syntax_spans_for_segment(
        &mut self,
        text: &dyn RenderSource,
        start_line: usize,
        line_count: usize,
        language_name: &str,
        syntax_enabled: bool,
    ) -> Vec<Vec<VisibleSyntaxSpan>> {
        let Some(rope) = text.as_rope() else {
            return self.backend_syntax_spans_for_segment(
                text,
                start_line,
                line_count,
                language_name,
                syntax_enabled,
            );
        };
        if !syntax_enabled || line_count == 0 {
            return self.backend_syntax_spans_for_segment(
                text,
                start_line,
                line_count,
                language_name,
                syntax_enabled,
            );
        }

        // Cheap guard first: an entry whose first row moved describes other rows.
        let current: Vec<(usize, usize)> = self
            .syntax_cache
            .entry_starts()
            .into_iter()
            .map(|(line, _)| (line, self.offset_of_line(rope, line)))
            .collect();
        self.syntax_cache.drop_moved_entries(&current);
        if let Some(spans) =
            self.syntax_cache.get(start_line, line_count, language_name, syntax_enabled)
        {
            return spans;
        }

        let spans = self.backend_syntax_spans_for_segment(
            text,
            start_line,
            line_count,
            language_name,
            syntax_enabled,
        );
        self.syntax_cache.insert(
            start_line,
            self.offset_of_line(rope, start_line),
            language_name,
            syntax_enabled,
            spans.clone(),
        );
        spans
    }

    /// Cache counters for tests and diagnostics:
    /// `(hits, misses, cached windows, cached rows)`.
    #[cfg(test)]
    pub(crate) fn syntax_cache_stats(&self) -> (u64, u64, usize, usize) {
        self.syntax_cache.stats()
    }
}
