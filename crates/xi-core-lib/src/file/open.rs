// Copyright 2016 The xi-editor Authors.
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

//! File decode, open-mode analysis, and the rope load path (`try_load_file`).

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::str;

use fs2::FileExt;
use log::warn;
use xi_rope::Rope;

use crate::file::{FileError, FileInfo, mod_time_from_metadata};
use crate::line_ending::{LineEnding, LineEndingError};
use crate::text_store::DocumentMode;
use crate::vlf::store::VlfStore;
use crate::whitespace::{Indentation, MixedIndentError};

#[cfg(target_family = "unix")]
use crate::file::{change_cookie_from_metadata, permissions_from_metadata};

pub(crate) const UTF8_BOM: &str = "\u{feff}";
const MAX_FORMATTING_PROBE_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampledIndentation {
    Tabs,
    Spaces(usize),
    Mixed,
    None,
}

impl From<Result<Option<Indentation>, MixedIndentError>> for SampledIndentation {
    fn from(value: Result<Option<Indentation>, MixedIndentError>) -> Self {
        match value {
            Ok(Some(Indentation::Tabs)) => Self::Tabs,
            Ok(Some(Indentation::Spaces(width))) => Self::Spaces(width),
            Ok(None) => Self::None,
            Err(_) => Self::Mixed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampledLineEnding {
    CrLf,
    Lf,
    Mixed,
    LegacyCr,
    None,
}

impl From<Result<Option<LineEnding>, LineEndingError>> for SampledLineEnding {
    fn from(value: Result<Option<LineEnding>, LineEndingError>) -> Self {
        match value {
            Ok(Some(LineEnding::CrLf)) => Self::CrLf,
            Ok(Some(LineEnding::Lf)) => Self::Lf,
            Ok(None) => Self::None,
            Err(LineEndingError::Mixed) => Self::Mixed,
            Err(LineEndingError::LegacyCr) => Self::LegacyCr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOpenAnalysis {
    pub indentation: SampledIndentation,
    pub line_ending: SampledLineEnding,
    pub line_ending_complete: bool,
}

impl Default for FileOpenAnalysis {
    fn default() -> Self {
        Self {
            indentation: SampledIndentation::None,
            line_ending: SampledLineEnding::None,
            line_ending_complete: true,
        }
    }
}

impl FileOpenAnalysis {
    fn from_bytes(bytes: &[u8], encoding: CharacterEncoding) -> Self {
        let Some(sample) = formatting_probe_str(bytes, encoding) else {
            return Self::default();
        };

        let sample_rope = Rope::from(sample);
        let indentation =
            SampledIndentation::from(Indentation::parse_bounded(&sample_rope, usize::MAX));
        let line_ending =
            SampledLineEnding::from(LineEnding::parse_bounded(&sample_rope, usize::MAX));
        let skipped_bom =
            usize::from(matches!(encoding, CharacterEncoding::Utf8WithBom)) * UTF8_BOM.len();
        let line_ending_complete =
            bytes.len().saturating_sub(skipped_bom) <= MAX_FORMATTING_PROBE_BYTES;

        Self { indentation, line_ending, line_ending_complete }
    }

    pub fn needs_line_ending_verification(self) -> bool {
        !self.line_ending_complete
    }
}

fn formatting_probe_str(bytes: &[u8], encoding: CharacterEncoding) -> Option<&str> {
    let bytes = match encoding {
        CharacterEncoding::Utf8WithBom if bytes.starts_with(UTF8_BOM.as_bytes()) => {
            &bytes[UTF8_BOM.len()..]
        }
        _ => bytes,
    };

    let probe = &bytes[..bytes.len().min(MAX_FORMATTING_PROBE_BYTES)];
    str::from_utf8(probe).ok()
}

pub(crate) fn sampled_line_count_hint(path: &Path, file_size_bytes: u64) -> Option<u64> {
    let sample_len =
        usize::try_from(file_size_bytes.min(MAX_FORMATTING_PROBE_BYTES as u64)).ok()?;
    let mut sample = vec![0; sample_len];
    let mut file = File::open(path).ok()?;
    let bytes_read = file.read(&mut sample).ok()?;
    sample.truncate(bytes_read);
    estimate_line_count_from_sample(&sample, file_size_bytes)
}

fn estimate_line_count_from_sample(sample: &[u8], file_size_bytes: u64) -> Option<u64> {
    if sample.is_empty() {
        return Some(0);
    }

    let newline_count = sample.iter().filter(|&&byte| byte == b'\n').count() as u64;
    let sample_len = sample.len() as u64;

    if newline_count == 0 {
        return (sample_len == file_size_bytes).then_some(1);
    }

    let mut estimated = newline_count.saturating_mul(file_size_bytes).div_ceil(sample_len);
    if sample_len == file_size_bytes && !sample.ends_with(b"\n") {
        estimated = estimated.saturating_add(1);
    }

    Some(estimated.max(newline_count))
}

/// Result of opening a file, distinguishing the document mode.
///
/// - `Rope` is returned for `Normal` and `ConstrainedNormal` files loaded fully
///   into memory via `try_load_file`.
/// - `Vlf` is returned for files above the VLF threshold; the caller must use
///   the [`VlfStore`] for all reads.  No `Rope` is ever constructed.
pub enum OpenResult {
    /// Normal / ConstrainedNormal mode: full file content as a `Rope`.
    Rope { text: Rope, mode: DocumentMode },
    /// VLF mode: paged file reader with bounded cache.  No full buffer.
    Vlf(Box<VlfStore>),
}

#[derive(Debug, Clone, Copy)]
pub enum CharacterEncoding {
    Utf8,
    Utf8WithBom,
}

impl CharacterEncoding {
    fn guess(s: &[u8]) -> Self {
        if s.starts_with(UTF8_BOM.as_bytes()) {
            CharacterEncoding::Utf8WithBom
        } else {
            CharacterEncoding::Utf8
        }
    }
}

pub(crate) fn try_load_file<P>(path: P) -> Result<(Rope, FileInfo), FileError>
where
    P: AsRef<Path>,
{
    // Non-UTF-8 file contents are rejected with FileError::UnknownEncoding.
    // it's arguable that the rope crate should have file loading functionality
    #[cfg(test)]
    let read_started = std::time::Instant::now();
    let mut f =
        File::open(path.as_ref()).map_err(|e| FileError::Io(e, path.as_ref().to_owned()))?;
    let metadata = f.metadata().ok();
    let mut text = metadata
        .as_ref()
        .and_then(|meta| usize::try_from(meta.len()).ok())
        .map(String::with_capacity)
        .unwrap_or_default();
    f.read_to_string(&mut text).map_err(|e| match e.kind() {
        io::ErrorKind::InvalidData => FileError::UnknownEncoding(path.as_ref().to_owned()),
        _ => FileError::Io(e, path.as_ref().to_owned()),
    })?;

    // Acquire an advisory exclusive lock so that a second editor instance
    // cannot open the same file for writing without first detecting the lock.
    // `try_lock_exclusive` is non-blocking; if another process holds the lock
    // we warn and proceed without the lock rather than refusing to open the file.
    let lock = match f.try_lock_exclusive() {
        Ok(()) => Some(f),
        Err(e) => {
            warn!(
                "Could not acquire advisory lock on {:?}: {}. \
                     Another editor instance may have the file open.",
                path.as_ref(),
                e
            );
            None
        }
    };

    #[cfg(test)]
    crate::open_probe::record(crate::open_probe::OpenStage::ReadDecode, read_started.elapsed());

    #[cfg(test)]
    let analysis_started = std::time::Instant::now();
    let encoding = CharacterEncoding::guess(text.as_bytes());
    let open_analysis = FileOpenAnalysis::from_bytes(text.as_bytes(), encoding);
    #[cfg(test)]
    crate::open_probe::record(crate::open_probe::OpenStage::Analysis, analysis_started.elapsed());

    #[cfg(test)]
    let rope_started = std::time::Instant::now();
    let rope = match encoding {
        CharacterEncoding::Utf8 => Rope::from_owned(text),
        // Move the read buffer into the rope directly: the BOM is drained in
        // place so the decoded text is never copied.
        CharacterEncoding::Utf8WithBom => {
            text.drain(..UTF8_BOM.len());
            Rope::from_owned(text)
        }
    };
    #[cfg(test)]
    crate::open_probe::record(crate::open_probe::OpenStage::RopeBuild, rope_started.elapsed());
    let info = FileInfo {
        encoding,
        mod_time: metadata.as_ref().and_then(mod_time_from_metadata),
        len: metadata.as_ref().map(std::fs::Metadata::len),
        open_analysis,
        #[cfg(target_family = "unix")]
        permissions: metadata.as_ref().map(permissions_from_metadata),
        #[cfg(target_family = "unix")]
        change_cookie: metadata.as_ref().map(change_cookie_from_metadata),
        path: path.as_ref().to_owned(),
        has_changed: false,
        _lock: lock,
    };
    Ok((rope, info))
}

#[cfg(test)]
fn try_decode(bytes: Vec<u8>, encoding: CharacterEncoding, path: &Path) -> Result<Rope, FileError> {
    let text = match encoding {
        CharacterEncoding::Utf8 => {
            str::from_utf8(&bytes).map_err(|_e| FileError::UnknownEncoding(path.to_owned()))?
        }
        CharacterEncoding::Utf8WithBom => {
            let s =
                str::from_utf8(&bytes).map_err(|_e| FileError::UnknownEncoding(path.to_owned()))?;
            &s[UTF8_BOM.len()..]
        }
    };
    Ok(Rope::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_load_file_rejects_non_utf8_contents() {
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"ok\n\xff\n").unwrap();

        let result = try_load_file(tmp.path());

        assert!(
            matches!(result, Err(FileError::UnknownEncoding(_))),
            "expected UnknownEncoding error, got {:?}",
            result.err().map(|err| err.to_string())
        );
    }

    #[test]
    fn open_analysis_detects_small_complete_sample() {
        let bytes = b"  alpha\r\n  beta\r\n";
        let analysis = FileOpenAnalysis::from_bytes(bytes, CharacterEncoding::Utf8);

        assert_eq!(analysis.indentation, SampledIndentation::Spaces(2));
        assert_eq!(analysis.line_ending, SampledLineEnding::CrLf);
        assert!(analysis.line_ending_complete);
    }

    #[test]
    fn open_analysis_uses_head_sample_and_defers_line_ending_verification() {
        let head: String = (0..10_000).map(|_| "  item\n").collect();
        let tail = "tail\r\n";
        let bytes = format!("{head}{tail}").into_bytes();

        let analysis = FileOpenAnalysis::from_bytes(&bytes, CharacterEncoding::Utf8);

        assert_eq!(analysis.indentation, SampledIndentation::Spaces(2));
        assert_eq!(analysis.line_ending, SampledLineEnding::Lf);
        assert!(analysis.needs_line_ending_verification());
    }

    #[test]
    fn try_decode_large_bom_text_builds_multi_leaf_rope() {
        let text = format!("{}{}{}", "a".repeat(1500), "\r\n", "🙂é".repeat(400));
        let bytes = format!("{}{text}", UTF8_BOM).into_bytes();

        let rope =
            try_decode(bytes, CharacterEncoding::Utf8WithBom, Path::new("/tmp/demo")).unwrap();

        assert_eq!(String::from(&rope), text);
        assert!(rope.iter_chunks(..).count() >= 2);
    }

    #[test]
    fn try_decode_does_not_allocate_full_intermediate_string() {
        let text = format!("{}{}{}", "line\r\n".repeat(4096), "🙂é", "tail\n".repeat(1024));
        let bytes = format!("{}{text}", UTF8_BOM).into_bytes();
        let threshold = text.len();

        let (rope, large_alloc_count, largest_alloc) =
            crate::file::with_large_alloc_tracking(threshold, || {
                try_decode(bytes, CharacterEncoding::Utf8WithBom, Path::new("/tmp/demo")).unwrap()
            });

        assert_eq!(String::from(&rope), text);
        assert_eq!(large_alloc_count, 0, "unexpected >=full-buffer allocation: {largest_alloc}");
        assert_eq!(largest_alloc, 0);
    }

    #[test]
    fn line_count_estimate_scales_head_sample_to_full_file() {
        let sample = b"x\nx\nx\n";
        let estimated = estimate_line_count_from_sample(sample, 12).unwrap();
        assert_eq!(estimated, 6);
    }
}
