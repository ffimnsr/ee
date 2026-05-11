use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use xi_core_lib::text_store::{ByteOffset, LineLookup, LogicalLine, TextStore};
use xi_core_lib::vlf::search::{
    VlfSearchRunMetrics, VlfSearchRunOptions, measure_streaming_search,
};
use xi_core_lib::vlf::store::VlfStore;

pub const ONE_MIB: u64 = 1024 * 1024;
pub const PAGE_SIZE: u64 = ONE_MIB;
pub const DECODED_BUDGET: u64 = 16 * ONE_MIB;
pub const PAGE_DOWN_TIMEOUT: Duration = Duration::from_secs(5);
const SEARCH_QUERY: &str = "needle-absent-for-throughput";

#[derive(Clone, Copy, Debug)]
pub struct FixtureSpec {
    pub label: &'static str,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct FixtureMeta {
    pub spec: FixtureSpec,
    pub path: PathBuf,
    pub total_lines: u64,
}

#[derive(Debug)]
pub struct GotoMetrics {
    pub index_elapsed: Duration,
    pub lookup_elapsed: Duration,
    pub kind: String,
    pub byte_offset: Option<u64>,
}

pub fn default_fixture_dir(prefix: &str) -> PathBuf {
    env::temp_dir().join(format!("{prefix}-{}", unique_suffix()))
}

pub fn build_fixture(spec: FixtureSpec, root: &Path) -> io::Result<FixtureMeta> {
    let path = root.join(format!("vlf-bench-{}.txt", spec.label));
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(&path)?;
    file.set_len(spec.size_bytes)?;

    let dense_target =
        if spec.size_bytes <= 128 * ONE_MIB { spec.size_bytes } else { 64 * ONE_MIB };
    let chunk_target = 256 * 1024usize;
    let chunk_count = dense_target.div_ceil(chunk_target as u64) as usize;

    let mut next_line = 0u64;
    if dense_target == spec.size_bytes {
        let mut writer = BufWriter::new(file);
        let mut written = 0u64;
        for chunk_idx in 0..chunk_count {
            let (chunk, lines) = build_chunk(chunk_idx, next_line, chunk_target);
            writer.write_all(&chunk)?;
            written = written.saturating_add(chunk.len() as u64);
            next_line = next_line.saturating_add(lines);
        }
        if written < spec.size_bytes {
            let padding = vec![b'\n'; (spec.size_bytes - written) as usize];
            writer.write_all(&padding)?;
            next_line = next_line.saturating_add(spec.size_bytes - written);
        }
        writer.flush()?;
    } else {
        let max_offset = spec.size_bytes.saturating_sub(chunk_target as u64);
        for chunk_idx in 0..chunk_count {
            let raw_offset = if chunk_count == 1 {
                0
            } else {
                max_offset.saturating_mul(chunk_idx as u64) / (chunk_count - 1) as u64
            };
            let offset = align_down(raw_offset, 4096).min(max_offset);
            let (chunk, lines) = build_chunk(chunk_idx, next_line, chunk_target);
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&chunk)?;
            next_line = next_line.saturating_add(lines);
        }
        file.flush()?;
    }

    Ok(FixtureMeta { spec, path, total_lines: next_line.max(1) })
}

pub fn measure_goto(path: &Path, total_lines: u64, exact: bool) -> io::Result<GotoMetrics> {
    let store = VlfStore::open_with_config(path, PAGE_SIZE, DECODED_BUDGET)?;
    let target_line = total_lines.saturating_sub(1).saturating_mul(9) / 10;

    let index_elapsed = if exact {
        let started = Instant::now();
        store.scan_all()?;
        started.elapsed()
    } else {
        store.scan_page_at(0)?;
        Duration::default()
    };

    let started = Instant::now();
    let lookup = store.line_to_byte(LogicalLine(target_line));
    let lookup_elapsed = started.elapsed();

    let (kind, byte_offset) = match lookup {
        LineLookup::Exact(offset) => (String::from("exact"), Some(offset.0)),
        LineLookup::Approximate(offset) => (String::from("approximate"), Some(offset.0)),
        LineLookup::Pending => (String::from("pending"), None),
        LineLookup::OutOfRange => (String::from("out_of_range"), None),
    };

    Ok(GotoMetrics { index_elapsed, lookup_elapsed, kind, byte_offset })
}

pub fn measure_search(path: &Path, cancel: bool) -> io::Result<VlfSearchRunMetrics> {
    let store = VlfStore::open_with_config(path, PAGE_SIZE, DECODED_BUDGET)?;
    store.set_viewport(ByteOffset(0), ByteOffset(PAGE_SIZE.min(store.len_bytes())));
    measure_streaming_search(
        &store,
        SEARCH_QUERY,
        VlfSearchRunOptions {
            case_sensitive: true,
            is_regex: false,
            whole_words: false,
            cancel_after_batches: cancel.then_some(1),
        },
    )
}

pub fn throughput_mib_per_s(bytes: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs == 0.0 {
        return 0.0;
    }
    bytes as f64 / ONE_MIB as f64 / secs
}

fn unique_suffix() -> String {
    let nanos =
        SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value - (value % alignment)
}

fn build_chunk(chunk_idx: usize, start_line: u64, target_bytes: usize) -> (Vec<u8>, u64) {
    let mut chunk = Vec::with_capacity(target_bytes);
    let mut line_no = start_line;

    while chunk.len() + 96 < target_bytes {
        let marker = if line_no % 97 == 0 { "needle" } else { "filler" };
        let line = format!(
            "line_{line_no:012} chunk_{chunk_idx:04} {marker} abcdef0123456789 lorem ipsum dolor sit amet\n"
        );
        if chunk.len() + line.len() > target_bytes {
            break;
        }
        chunk.extend_from_slice(line.as_bytes());
        line_no = line_no.saturating_add(1);
    }

    if !chunk.ends_with(b"\n") {
        chunk.push(b'\n');
        line_no = line_no.saturating_add(1);
    }

    (chunk, line_no.saturating_sub(start_line))
}
