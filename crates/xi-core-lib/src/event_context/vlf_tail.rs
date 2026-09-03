use crate::text_store::{ByteOffset, ByteRange, TextStore};

use super::vlf::vlf_read_text_range;

const INITIAL_TAIL_BYTES: u64 = 256 * 1024;
const MAX_TAIL_BYTES: u64 = 32 * 1024 * 1024;

struct TailWindow {
    text: String,
    byte_range: ByteRange,
    line_count: usize,
    skip_first_partial: bool,
}

impl TailWindow {
    fn owned_lines(&self, start: usize, end: usize) -> Option<Vec<String>> {
        if start >= end || end > self.line_count {
            return None;
        }

        let skip = start.checked_add(usize::from(self.skip_first_partial))?;
        let lines = self
            .text
            .split('\n')
            .skip(skip)
            .take(end - start)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        (lines.len() == end - start).then_some(lines)
    }
}

fn read_tail_window(store: &dyn TextStore, tail_len: u64) -> Option<TailWindow> {
    let tail_start = ByteOffset(store.len_bytes().saturating_sub(tail_len));
    let tail_end = ByteOffset(store.len_bytes());
    let (text, byte_range) =
        vlf_read_text_range(store, ByteRange { start: tail_start, end: tail_end })?;
    let skip_first_partial = byte_range.start.0 > 0;
    let line_count = bytecount::count(text.as_bytes(), b'\n')
        .saturating_add(1)
        .saturating_sub(usize::from(skip_first_partial));

    Some(TailWindow { text, byte_range, line_count, skip_first_partial })
}

pub(crate) fn vlf_tail_lines_for_range(
    store: &dyn TextStore,
    requested_line_start: u64,
    requested_count: usize,
    reported_line_count: u64,
) -> Option<(u64, Vec<String>, ByteRange)> {
    vlf_tail_lines_for_range_with_limit(
        store,
        requested_line_start,
        requested_count,
        reported_line_count,
        MAX_TAIL_BYTES,
    )
}

fn vlf_tail_lines_for_range_with_limit(
    store: &dyn TextStore,
    requested_line_start: u64,
    requested_count: usize,
    reported_line_count: u64,
    max_tail_bytes: u64,
) -> Option<(u64, Vec<String>, ByteRange)> {
    let file_len = store.len_bytes();
    if file_len == 0 || requested_count == 0 || max_tail_bytes == 0 {
        return None;
    }

    let max_tail_len = max_tail_bytes.min(file_len);
    let mut tail_len = INITIAL_TAIL_BYTES.min(max_tail_len);

    loop {
        let window = read_tail_window(store, tail_len)?;
        let effective_line_count = if window.byte_range.start.0 == 0 {
            window.line_count as u64
        } else {
            reported_line_count
        };
        let tail_line_start = effective_line_count.saturating_sub(window.line_count as u64);

        if tail_line_start <= requested_line_start {
            let requested_line_end = requested_line_start
                .saturating_add(requested_count as u64)
                .min(effective_line_count);
            if requested_line_start >= requested_line_end {
                return None;
            }

            let start_idx =
                usize::try_from(requested_line_start.saturating_sub(tail_line_start)).ok()?;
            let end_idx =
                usize::try_from(requested_line_end.saturating_sub(tail_line_start)).ok()?;
            let lines = window.owned_lines(start_idx, end_idx)?;
            return Some((requested_line_start, lines, window.byte_range));
        }

        if tail_len >= max_tail_len {
            return None;
        }
        tail_len = tail_len.saturating_mul(2).min(max_tail_len);
    }
}

pub(crate) fn vlf_tail_lines_for_count(
    store: &dyn TextStore,
    requested_count: usize,
    approximate_line_count: u64,
) -> Option<(u64, Vec<String>, Option<u64>, ByteRange)> {
    let tail_len = INITIAL_TAIL_BYTES.min(store.len_bytes());
    let window = read_tail_window(store, tail_len)?;

    if window.line_count == 0 {
        return None;
    }

    let exact_line_count = (window.byte_range.start.0 == 0).then_some(window.line_count as u64);
    let reported_line_count =
        exact_line_count.unwrap_or_else(|| approximate_line_count.max(window.line_count as u64));
    let start_idx = window.line_count.saturating_sub(requested_count.max(1));
    let lines = window.owned_lines(start_idx, window.line_count)?;
    let response_line_start = reported_line_count.saturating_sub(lines.len() as u64);

    Some((response_line_start, lines, exact_line_count, window.byte_range))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;
    use crate::text_store::{KnownLineCount, TextStore};
    use crate::vlf::store::VlfStore;

    fn long_line_store(line_count: usize) -> (VlfStore, NamedTempFile) {
        let mut file = NamedTempFile::new().unwrap();
        for line in 0..line_count {
            writeln!(file, "{line:06} {}", "x".repeat(120)).unwrap();
        }
        file.flush().unwrap();
        let store = VlfStore::open_with_config(file.path(), 4096, 1024 * 1024).unwrap();
        (store, file)
    }

    #[test]
    fn range_expands_tail_until_requested_start_is_covered() {
        let (store, _file) = long_line_store(5_000);
        let requested_line_start = 905;
        let requested_count = 4_096;
        assert!(matches!(store.known_line_count(), KnownLineCount::Unknown));

        let (response_line_start, lines, _) =
            vlf_tail_lines_for_range(&store, requested_line_start, requested_count, 5_001).unwrap();

        assert_eq!(response_line_start, requested_line_start);
        assert_eq!(lines.len(), requested_count);
        assert!(lines[0].starts_with("000905 "));
        assert_eq!(lines.last().map(String::as_str), Some(""));
    }

    #[test]
    fn range_rejects_partial_suffix_when_tail_limit_cannot_cover_start() {
        let (store, _file) = long_line_store(5_000);

        let response =
            vlf_tail_lines_for_range_with_limit(&store, 905, 4_096, 5_001, INITIAL_TAIL_BYTES);

        assert!(response.is_none());
    }
}
