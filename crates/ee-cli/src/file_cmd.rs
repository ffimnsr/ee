//! `ee do file` line-check/head/tail commands.
use super::*;

pub(crate) fn count_file_line_feeds(path: &Path) -> io::Result<u64> {
    VlfStore::open(path)?.count_lf_streaming()
}

pub(crate) fn cmd_file_line_check(path: &Path) {
    match count_file_line_feeds(path) {
        Ok(count) => println!("{count} {}", path.display()),
        Err(err) => {
            eprintln!("Cannot count lines in {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}

pub(crate) fn read_text_range(
    store: &dyn TextStore,
    range: ByteRange,
) -> io::Result<(String, ByteRange)> {
    if let TextChunkResult::Ready(chunk) = store.read_byte_range(range) {
        return Ok((chunk.text, chunk.byte_range));
    }

    let mut text = String::new();
    let mut decoded_start = None;
    let mut decoded_end = range.start;
    for result in store.iter_chunks(range) {
        let TextChunkResult::Ready(chunk) = result else {
            return Err(io::Error::other("failed to read requested text range"));
        };
        decoded_start.get_or_insert(chunk.byte_range.start);
        decoded_end = chunk.byte_range.end;
        text.push_str(&chunk.text);
    }

    Ok((text, ByteRange { start: decoded_start.unwrap_or(range.start), end: decoded_end }))
}

pub(crate) fn read_exact_text_range(store: &dyn TextStore, range: ByteRange) -> io::Result<String> {
    let (text, decoded_range) = read_text_range(store, range)?;
    if decoded_range == range {
        return Ok(text);
    }

    let start = usize::try_from(range.start.0.saturating_sub(decoded_range.start.0))
        .map_err(|_| io::Error::other("range start overflow"))?;
    let end = usize::try_from(range.end.0.saturating_sub(decoded_range.start.0))
        .map_err(|_| io::Error::other("range end overflow"))?;
    if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(io::Error::other("requested range split utf-8 boundary"));
    }

    Ok(text[start..end].to_owned())
}

pub(crate) fn read_file_head(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let store = VlfStore::open(path)?;
    let mut next_start = 0u64;
    let mut pending = String::new();
    let mut out = String::new();
    let mut lines_emitted = 0usize;

    while next_start < store.len_bytes() {
        let next_end = next_start.saturating_add(FILE_PREVIEW_CHUNK_BYTES).min(store.len_bytes());
        pending.push_str(&read_exact_text_range(&store, ByteRange::new(next_start, next_end))?);
        next_start = next_end;

        while lines_emitted < lines {
            let Some(newline_idx) = pending.find('\n') else { break };
            let line_end = newline_idx + 1;
            out.push_str(&pending[..line_end]);
            pending.drain(..line_end);
            lines_emitted += 1;
            if lines_emitted == lines {
                return Ok(out);
            }
        }
    }

    if lines_emitted < lines {
        out.push_str(&pending);
    }
    Ok(out)
}

pub(crate) fn read_file_tail(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let store = VlfStore::open(path)?;
    let mut start = store.len_bytes();
    let mut text = String::new();
    let mut newline_count = 0usize;

    while start > 0 && newline_count <= lines {
        let chunk_start = start.saturating_sub(FILE_PREVIEW_CHUNK_BYTES);
        let chunk = read_exact_text_range(&store, ByteRange::new(chunk_start, start))?;
        newline_count += chunk.as_bytes().iter().filter(|&&byte| byte == b'\n').count();
        text.insert_str(0, &chunk);
        start = chunk_start;
    }

    if text.is_empty() {
        return Ok(text);
    }

    let bytes = text.as_bytes();
    let mut idx = bytes.len();
    if idx > 0 && bytes[idx - 1] == b'\n' {
        idx -= 1;
    }

    let mut lines_seen = 0usize;
    while idx > 0 {
        idx -= 1;
        if bytes[idx] == b'\n' {
            lines_seen += 1;
            if lines_seen == lines {
                return Ok(text[idx + 1..].to_owned());
            }
        }
    }

    Ok(text)
}

pub(crate) fn cmd_file_head(path: &Path, lines: usize) {
    match read_file_head(path, lines) {
        Ok(text) => {
            print!("{text}");
            let _ = io::stdout().flush();
        }
        Err(err) => {
            eprintln!("Cannot read head for {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}

pub(crate) fn cmd_file_tail(path: &Path, lines: usize) {
    match read_file_tail(path, lines) {
        Ok(text) => {
            print!("{text}");
            let _ = io::stdout().flush();
        }
        Err(err) => {
            eprintln!("Cannot read tail for {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}
