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

impl LineEnding {
    /// Breaks a rope down into chunks, and checks each chunk for line endings
    pub fn parse(rope: &Rope) -> Result<Option<Self>, LineEndingError> {
        let mut crlf = false;
        let mut lf = false;

        for chunk in rope.iter_chunks(..) {
            match LineEnding::parse_chunk(chunk) {
                Ok(Some(LineEnding::CrLf)) => crlf = true,
                Ok(Some(LineEnding::Lf)) => lf = true,
                Ok(None) => (),
                Err(e) => return Err(e),
            }
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
}
