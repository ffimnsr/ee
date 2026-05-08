// Copyright 2018 The xi-editor Authors.
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

//! Utilities for detecting and working with line endings

use memchr::memchr2;
use xi_rope::Rope;

/// An enumeration of valid line endings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineEnding {
    CrLf, // DOS style, \r\n
    Lf,   // *nix style, \n
}

/// A struct representing a mixed line ending error.
#[derive(Debug)]
pub struct MixedLineEndingError;

/// Errors produced when parsing line endings from a buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum LineEndingError {
    /// The document mixes `\r\n` and `\n` line endings.
    Mixed,
    /// The document uses legacy CR-only (`\r`) line endings, which are not supported.
    LegacyCr,
}

/// Maximum bytes sampled by [`LineEnding::parse_bounded`].
/// For line-ending detection the first 64 KiB is more than enough;
/// scanning the whole file (potentially gigabytes) before first render
/// would violate the normal-mode performance budget.
const MAX_LINE_ENDING_PROBE_BYTES: usize = 65_536;

impl LineEnding {
    /// Breaks a rope down into chunks, and checks each chunk for line endings.
    ///
    /// Internally delegates to [`parse_bounded`] with [`MAX_LINE_ENDING_PROBE_BYTES`].
    ///
    /// [`parse_bounded`]: LineEnding::parse_bounded
    pub fn parse(rope: &Rope) -> Result<Option<Self>, LineEndingError> {
        Self::parse_bounded(rope, MAX_LINE_ENDING_PROBE_BYTES)
    }

    /// Like [`parse`] but stops after reading `max_bytes` of rope content.
    ///
    /// Limits the scan so that large files do not stall the open path.
    /// Sampling the file head is sufficient for line-ending detection.
    ///
    /// [`parse`]: LineEnding::parse
    pub fn parse_bounded(rope: &Rope, max_bytes: usize) -> Result<Option<Self>, LineEndingError> {
        let mut crlf = false;
        let mut lf = false;
        let mut seen: usize = 0;

        for chunk in rope.iter_chunks(..) {
            if seen >= max_bytes {
                break;
            }
            // Truncate the chunk to the remaining byte budget, staying on a
            // UTF-8 char boundary so `parse_chunk` receives valid str slices.
            let budget = max_bytes - seen;
            let end = if budget >= chunk.len() {
                chunk.len()
            } else {
                // Walk backward from `budget` to find a char boundary.
                let mut b = budget;
                while b > 0 && !chunk.is_char_boundary(b) {
                    b -= 1;
                }
                b
            };
            let slice = &chunk[..end];
            match LineEnding::parse_chunk(slice) {
                Ok(Some(LineEnding::CrLf)) => crlf = true,
                Ok(Some(LineEnding::Lf)) => lf = true,
                Ok(None) => (),
                Err(e) => return Err(e),
            }
            seen += end;
        }

        match (crlf, lf) {
            (true, false) => Ok(Some(LineEnding::CrLf)),
            (false, true) => Ok(Some(LineEnding::Lf)),
            (false, false) => Ok(None),
            _ => Err(LineEndingError::Mixed),
        }
    }

    /// Checks a chunk for line endings, assuming `\n` or `\r\n`.
    ///
    /// Returns `Err(LineEndingError::LegacyCr)` for CR-only (`\r`) line endings
    /// and `Err(LineEndingError::Mixed)` for malformed sequences like `\r ` before `\n`.
    pub fn parse_chunk(chunk: &str) -> Result<Option<Self>, LineEndingError> {
        let bytes = chunk.as_bytes();
        let newline = memchr2(b'\n', b'\r', bytes);
        match newline {
            Some(x) if bytes[x] == b'\r' && bytes.len() > x + 1 && bytes[x + 1] == b'\n' => {
                Ok(Some(LineEnding::CrLf))
            }
            Some(x) if bytes[x] == b'\n' => Ok(Some(LineEnding::Lf)),
            // A bare \r with nothing following (end of chunk) or followed by a non-\n character
            // is a legacy CR-only line ending.
            Some(x) if bytes[x] == b'\r' => Err(LineEndingError::LegacyCr),
            Some(_) => Err(LineEndingError::Mixed),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf() {
        let result = LineEnding::parse_chunk("\r\n");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(LineEnding::CrLf));
    }

    #[test]
    fn lf() {
        let result = LineEnding::parse_chunk("\n");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(LineEnding::Lf));
    }

    #[test]
    fn legacy_mac_errors() {
        assert_eq!(LineEnding::parse_chunk("\r"), Err(LineEndingError::LegacyCr));
    }

    #[test]
    fn bad_space() {
        assert!(LineEnding::parse_chunk("\r \n").is_err());
    }

    #[test]
    fn parse_bounded_stops_at_byte_cap() {
        // First 65 KiB are LF-only; bytes after the cap contain CRLF.
        let head = "a\n".repeat(32_768); // 65_536 bytes
        let tail = "b\r\n".repeat(1_000);
        let rope = Rope::from(format!("{head}{tail}"));

        // Bounded at exactly 65_536 bytes should only see LF.
        let result = LineEnding::parse_bounded(&rope, 65_536).unwrap();
        assert_eq!(result, Some(LineEnding::Lf));
    }

    #[test]
    fn parse_bounded_with_zero_max_bytes_returns_none() {
        let rope = Rope::from("a\r\nb\n");
        let result = LineEnding::parse_bounded(&rope, 0).unwrap();
        assert_eq!(result, None);
    }

    /// Performance budget: `LineEnding::parse` on a 20 MB rope must complete
    /// within 20 ms because it is capped at `MAX_LINE_ENDING_PROBE_BYTES`
    /// (64 KiB) regardless of file size.
    #[test]
    fn parse_bounded_large_rope_stays_within_budget() {
        use std::time::Instant;
        // ~20 MB of LF-terminated lines.
        const LINES: usize = 1_000_000;
        const BUDGET_MS: u128 = 50;

        let content: String = (0..LINES).map(|_| "hello\n").collect();
        let rope = xi_rope::Rope::from(content);

        let start = Instant::now();
        let _ = LineEnding::parse(&rope);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < BUDGET_MS,
            "LineEnding::parse on {LINES}-line rope took {}ms, expected < {BUDGET_MS}ms \
             (MAX_LINE_ENDING_PROBE_BYTES cap may be missing)",
            elapsed.as_millis()
        );
    }
}
