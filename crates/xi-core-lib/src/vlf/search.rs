use std::cmp::{max, min};
use std::io;
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};
use serde::Serialize;

use crate::text_store::{ByteOffset, ByteRange, LineLookup, LogicalLine, TextChunk, TextStore};

use super::pager::CancelGeneration;
use super::store::VlfStore;

pub(crate) const VLF_SEARCH_BATCH_BYTES: u64 = 256 * 1024;
const MIN_SEARCH_SLOP_BYTES: u64 = 256;
const MAX_STORED_MATCHES: usize = 256;
const MAX_STATUS_RANGES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct VlfMatchRange {
    pub(crate) line: u64,
    pub(crate) start_col: usize,
    pub(crate) end_col: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct VlfSearchStatus {
    pub(crate) query: String,
    pub(crate) scanned_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) complete: bool,
    pub(crate) stored_match_count: usize,
    pub(crate) ranges: Vec<VlfMatchRange>,
}

#[derive(Clone, Debug, Default)]
pub struct VlfSearchRunOptions {
    pub case_sensitive: bool,
    pub is_regex: bool,
    pub whole_words: bool,
    pub cancel_after_batches: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct VlfSearchRunMetrics {
    pub elapsed: Duration,
    pub cancel_elapsed: Option<Duration>,
    pub batches: usize,
    pub scanned_bytes: u64,
    pub total_bytes: u64,
    pub stored_match_count: usize,
    pub complete: bool,
    pub cancelled: bool,
}

pub fn measure_streaming_search(
    store: &VlfStore,
    query: impl Into<String>,
    options: VlfSearchRunOptions,
) -> io::Result<VlfSearchRunMetrics> {
    let query = query.into();
    let Some(mut search) = VlfSearchState::new(
        store,
        query,
        options.case_sensitive,
        options.is_regex,
        options.whole_words,
    ) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "streaming search query must not be empty",
        ));
    };

    let started = Instant::now();
    let mut batches = 0usize;
    let mut cancel_started = None;

    while !search.is_complete() {
        search.scan_batch(store)?;
        batches = batches.saturating_add(1);

        if cancel_started.is_none()
            && options.cancel_after_batches.is_some_and(|limit| batches >= limit)
        {
            store.invalidate_pending_reads();
            cancel_started = Some(Instant::now());
        }
    }

    let status = search.status();
    Ok(VlfSearchRunMetrics {
        elapsed: started.elapsed(),
        cancel_elapsed: cancel_started.map(|instant| instant.elapsed()),
        batches,
        scanned_bytes: status.scanned_bytes,
        total_bytes: status.total_bytes,
        stored_match_count: status.stored_match_count,
        complete: status.complete,
        cancelled: cancel_started.is_some(),
    })
}

#[derive(Clone, Debug)]
pub(crate) struct VlfSearchState {
    query: String,
    regex: Regex,
    whole_words: bool,
    slop_bytes: u64,
    page_starts: Vec<u64>,
    next_page_idx: usize,
    scanned_bytes: u64,
    total_bytes: u64,
    matches: Vec<VlfStoredMatch>,
    active_match_idx: Option<usize>,
    cancel_generation: CancelGeneration,
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VlfStoredMatch {
    byte_start: u64,
    line: u64,
    start_col: usize,
    end_col: usize,
}

impl VlfSearchState {
    pub(crate) fn new(
        store: &VlfStore,
        query: String,
        case_sensitive: bool,
        is_regex: bool,
        whole_words: bool,
    ) -> Option<Self> {
        if query.is_empty() {
            return None;
        }

        let regex_src = if is_regex { query.clone() } else { regex::escape(&query) };
        let regex = RegexBuilder::new(&regex_src)
            .case_insensitive(!case_sensitive)
            .size_limit(1_000_000)
            .build()
            .ok()?;

        let cancel_generation = store.invalidate_pending_reads();
        let total_bytes = store.len_bytes();
        let page_starts = viewport_first_page_order(store);
        let slop_bytes = max((query.len() as u64).saturating_mul(2), MIN_SEARCH_SLOP_BYTES);

        Some(Self {
            query,
            regex,
            whole_words,
            slop_bytes,
            page_starts,
            next_page_idx: 0,
            scanned_bytes: 0,
            total_bytes,
            matches: Vec::new(),
            active_match_idx: None,
            cancel_generation,
            complete: false,
        })
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }

    pub(crate) fn scan_batch(&mut self, store: &VlfStore) -> io::Result<()> {
        if self.complete {
            return Ok(());
        }

        let mut batch_bytes = 0u64;
        while self.next_page_idx < self.page_starts.len() && batch_bytes < VLF_SEARCH_BATCH_BYTES {
            let page_start = self.page_starts[self.next_page_idx];
            self.next_page_idx += 1;

            let page_end = min(page_start + store.page_size(), self.total_bytes);
            let page_len = page_end.saturating_sub(page_start);
            if page_len == 0 {
                continue;
            }

            let overlap_start = page_start.saturating_sub(self.slop_bytes);
            let overlap_end = min(page_end.saturating_add(self.slop_bytes), self.total_bytes);

            store.scan_page_at(page_start)?;

            match store.read_search_range(
                ByteRange::new(overlap_start, overlap_end),
                self.cancel_generation,
            ) {
                Ok(chunk) => self.collect_matches(store, page_start, page_end, chunk),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    self.complete = true;
                    return Ok(());
                }
                Err(err) => return Err(err),
            }

            self.scanned_bytes = self.scanned_bytes.saturating_add(page_len);
            batch_bytes = batch_bytes.saturating_add(page_len);
        }

        if self.next_page_idx >= self.page_starts.len() {
            self.complete = true;
            self.matches.sort_by_key(|m| (m.byte_start, m.start_col));
            self.matches.dedup_by(|left, right| left.byte_start == right.byte_start);
            if self.matches.len() > MAX_STORED_MATCHES {
                self.matches.truncate(MAX_STORED_MATCHES);
            }
        }

        Ok(())
    }

    pub(crate) fn next_match(&mut self, reverse: bool, wrap: bool) -> Option<VlfMatchRange> {
        if self.matches.is_empty() {
            return None;
        }

        let next_idx = match (self.active_match_idx, reverse) {
            (Some(idx), true) if idx > 0 => Some(idx - 1),
            (Some(idx), false) if idx + 1 < self.matches.len() => Some(idx + 1),
            (Some(_), _) if wrap => Some(if reverse { self.matches.len() - 1 } else { 0 }),
            (Some(_), _) => None,
            (None, true) => Some(self.matches.len() - 1),
            (None, false) => Some(0),
        }?;

        self.active_match_idx = Some(next_idx);
        let matched = &self.matches[next_idx];
        Some(VlfMatchRange {
            line: matched.line,
            start_col: matched.start_col,
            end_col: matched.end_col,
        })
    }

    pub(crate) fn status(&self) -> VlfSearchStatus {
        VlfSearchStatus {
            query: self.query.clone(),
            scanned_bytes: self.scanned_bytes,
            total_bytes: self.total_bytes,
            complete: self.complete,
            stored_match_count: self.matches.len(),
            ranges: self
                .matches
                .iter()
                .take(MAX_STATUS_RANGES)
                .map(|matched| VlfMatchRange {
                    line: matched.line,
                    start_col: matched.start_col,
                    end_col: matched.end_col,
                })
                .collect(),
        }
    }

    fn collect_matches(
        &mut self,
        store: &VlfStore,
        page_start: u64,
        page_end: u64,
        chunk: TextChunk,
    ) {
        if self.matches.len() >= MAX_STORED_MATCHES {
            return;
        }

        for matched in self.regex.find_iter(&chunk.text) {
            let abs_start = chunk.byte_range.start.0 + matched.start() as u64;
            let abs_end = chunk.byte_range.start.0 + matched.end() as u64;

            if abs_start < page_start || abs_start >= page_end {
                continue;
            }

            if self.whole_words && !is_whole_word_match(&chunk.text, matched.start(), matched.end())
            {
                continue;
            }

            if self.matches.iter().any(|existing| existing.byte_start == abs_start) {
                continue;
            }

            let Some(line) = store.byte_to_line(ByteOffset(abs_start)).map(|line| line.0) else {
                continue;
            };
            let line_start = match store.line_to_byte(LogicalLine(line)) {
                LineLookup::Exact(offset) | LineLookup::Approximate(offset) => offset.0,
                LineLookup::Pending | LineLookup::OutOfRange => continue,
            };

            self.matches.push(VlfStoredMatch {
                byte_start: abs_start,
                line,
                start_col: abs_start.saturating_sub(line_start) as usize,
                end_col: abs_end.saturating_sub(line_start) as usize,
            });

            if self.matches.len() >= MAX_STORED_MATCHES {
                self.matches.sort_by_key(|m| (m.byte_start, m.start_col));
                self.matches.dedup_by(|left, right| left.byte_start == right.byte_start);
                if self.matches.len() > MAX_STORED_MATCHES {
                    self.matches.truncate(MAX_STORED_MATCHES);
                }
                return;
            }
        }
    }
}

fn viewport_first_page_order(store: &VlfStore) -> Vec<u64> {
    let total_bytes = store.len_bytes();
    let page_size = store.page_size();
    if total_bytes == 0 || page_size == 0 {
        return Vec::new();
    }

    let viewport = store.viewport_window();
    let vp_first = (viewport.start.0 / page_size) * page_size;
    let vp_last = if viewport.end.0 == 0 {
        0
    } else {
        ((viewport.end.0.saturating_sub(1)) / page_size) * page_size
    };

    let mut page_starts = Vec::new();
    let mut pos = vp_first;
    while pos <= vp_last && pos < total_bytes {
        page_starts.push(pos);
        pos = pos.saturating_add(page_size);
    }

    let mut forward = vp_last.saturating_add(page_size);
    let mut backward = vp_first.checked_sub(page_size);
    loop {
        let mut did_work = false;
        if forward < total_bytes {
            page_starts.push(forward);
            forward = forward.saturating_add(page_size);
            did_work = true;
        }
        if let Some(prev) = backward {
            page_starts.push(prev);
            backward = prev.checked_sub(page_size);
            did_work = true;
        }
        if !did_work {
            break;
        }
    }

    if page_starts.is_empty() {
        let mut pos = 0;
        while pos < total_bytes {
            page_starts.push(pos);
            pos = pos.saturating_add(page_size);
        }
    }

    page_starts
}

fn is_whole_word_match(text: &str, start: usize, end: usize) -> bool {
    let left_ok = text[..start].chars().next_back().is_none_or(|ch| !is_word_char(ch));
    let right_ok = text[end..].chars().next().is_none_or(|ch| !is_word_char(ch));
    left_ok && right_ok
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        let unique = format!(
            "xi-core-lib-vlf-search-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    fn store_from(name: &str, content: &str) -> VlfStore {
        let path = temp_path(name);
        fs::write(&path, content).expect("write vlf search fixture");
        let mut store =
            VlfStore::open_with_config(&path, 16, 1024 * 1024).expect("open VLF search fixture");
        store.set_batch_size(16);
        store.set_viewport(ByteOffset(0), ByteOffset(content.len() as u64));
        store
    }

    #[test]
    fn search_with_slop_finds_boundary_crossing_match() {
        let store = store_from("boundary", "aaaaMATCHbbbb");
        let mut search = VlfSearchState::new(&store, String::from("MATCH"), true, false, false)
            .expect("search state");

        search.scan_batch(&store).expect("scan batch");

        let status = search.status();
        assert_eq!(status.stored_match_count, 1);
        assert_eq!(status.ranges[0].line, 0);
        assert_eq!(status.ranges[0].start_col, 4);
        assert_eq!(status.ranges[0].end_col, 9);
    }

    #[test]
    fn viewport_pages_are_scanned_before_tail_pages() {
        let content = (0..20).map(|idx| format!("line-{idx}\n")).collect::<String>();
        let store = store_from("viewport-first", &content);
        store.set_viewport(ByteOffset(40), ByteOffset(80));

        let search = VlfSearchState::new(&store, String::from("line"), true, false, false)
            .expect("search state");

        assert!(!search.page_starts.is_empty());
        let first = search.page_starts[0];
        assert!((32..=64).contains(&first), "viewport page should come first, got {first}");
    }

    #[test]
    fn stored_matches_are_bounded() {
        let content = (0..600).map(|_| "hit\n").collect::<String>();
        let store = store_from("bounded", &content);
        let mut search = VlfSearchState::new(&store, String::from("hit"), true, false, false)
            .expect("search state");

        while !search.is_complete() {
            search.scan_batch(&store).expect("scan batch");
        }

        let stored = search.status().stored_match_count;
        assert!(stored > 0);
        assert!(stored <= MAX_STORED_MATCHES);
    }

    #[test]
    fn search_handles_overlap_larger_than_read_cap() {
        let content = (0..64).map(|_| "hit\n").collect::<String>();
        let store = store_from("chunked-overlap", &content);
        let mut search = VlfSearchState::new(&store, String::from("hit"), true, false, false)
            .expect("search state");

        search.scan_batch(&store).expect("scan batch");

        assert!(search.status().stored_match_count > 0);
    }

    #[test]
    fn search_cancellation_stops_large_fixture_scan() {
        let cancelled = AtomicBool::new(false);
        let content = (0..50_000).map(|_| "zzzzzzzzzzzzzzzz\n").collect::<String>();
        let store = store_from("cancel", &content);
        let mut search = VlfSearchState::new(&store, String::from("absent"), true, false, false)
            .expect("search state");

        store.invalidate_pending_reads();
        search.scan_batch(&store).expect("scan batch after cancellation");
        cancelled.store(search.is_complete(), Ordering::Relaxed);

        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(search.status().stored_match_count, 0);
    }
}
