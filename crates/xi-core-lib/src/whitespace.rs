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

//! Utilities for detecting and working with indentation.

use std::collections::BTreeMap;
use xi_rope::Rope;

/// An enumeration of legal indentation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Indentation {
    Tabs,
    Spaces(usize),
}

/// A struct representing the mixed indentation error.
#[derive(Debug)]
pub struct MixedIndentError;

/// Maximum number of lines sampled by [`Indentation::parse_bounded`].
/// Scanning beyond this count adds no statistical benefit for indentation
/// detection and blocks the first render for large files.
const MAX_INDENT_PROBE_LINES: usize = 1_000;

impl Indentation {
    /// Parses a rope for indentation settings.
    ///
    /// Internally delegates to [`parse_bounded`] with `MAX_INDENT_PROBE_LINES`.
    /// Use [`parse_bounded`] directly when a custom cap is required.
    ///
    /// [`parse_bounded`]: Indentation::parse_bounded
    pub fn parse(rope: &Rope) -> Result<Option<Self>, MixedIndentError> {
        Self::parse_bounded(rope, MAX_INDENT_PROBE_LINES)
    }

    /// Like [`parse`] but stops after `max_lines` lines.
    ///
    /// Limits the scan so that large files (≥ 20 MB / 300 K LOC) do not stall
    /// the open path.  A sample of the first `max_lines` lines is statistically
    /// sufficient for indentation detection.
    ///
    /// [`parse`]: Indentation::parse
    pub fn parse_bounded(rope: &Rope, max_lines: usize) -> Result<Option<Self>, MixedIndentError> {
        let mut tabs = false;
        let mut spaces: BTreeMap<usize, usize> = BTreeMap::new();

        for line in rope.lines_raw(..).take(max_lines) {
            match Indentation::parse_line(&line) {
                Ok(Some(Indentation::Spaces(size))) => {
                    let counter = spaces.entry(size).or_insert(0);
                    *counter += 1;
                }
                Ok(Some(Indentation::Tabs)) => tabs = true,
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }

        match (tabs, !spaces.is_empty()) {
            (true, true) => Err(MixedIndentError),
            (true, false) => Ok(Some(Indentation::Tabs)),
            (false, true) => {
                let tab_size = extract_count(spaces);
                if tab_size > 0 { Ok(Some(Indentation::Spaces(tab_size))) } else { Ok(None) }
            }
            _ => Ok(None),
        }
    }

    /// Detects the indentation on a specific line.
    /// Parses whitespace until first occurrence of something else
    pub fn parse_line(line: &str) -> Result<Option<Self>, MixedIndentError> {
        let mut spaces = 0;

        for char in line.as_bytes() {
            match char {
                b' ' => spaces += 1,
                b'\t' if spaces > 0 => return Err(MixedIndentError),
                b'\t' => return Ok(Some(Indentation::Tabs)),
                _ => break,
            }
        }

        if spaces > 0 { Ok(Some(Indentation::Spaces(spaces))) } else { Ok(None) }
    }
}

/// Uses a heuristic to calculate the greatest common denominator of most used indentation depths.
///
/// As BTreeMaps are ordered by value, using take on the iterator ensures the indentation levels
/// most frequently used in the file are extracted.
fn extract_count(spaces: BTreeMap<usize, usize>) -> usize {
    let mut take_size = 4;

    if spaces.len() < take_size {
        take_size = spaces.len();
    }

    // Fold results using GCD, skipping numbers which result in gcd returning 1
    spaces.iter().take(take_size).fold(0, |a, (b, _)| {
        let d = gcd(a, *b);
        if d == 1 { a } else { d }
    })
}

/// Simple implementation to calculate greatest common divisor, based on Euclid's algorithm
fn gcd(a: usize, b: usize) -> usize {
    if a == 0 {
        b
    } else if b == 0 || a == b {
        a
    } else {
        let mut a = a;
        let mut b = b;

        while b > 0 {
            let r = a % b;
            a = b;
            b = r;
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcd_calculates_correctly() {
        assert_eq!(21, gcd(1071, 462));
        assert_eq!(6, gcd(270, 192));
    }

    #[test]
    fn line_gets_two_spaces() {
        let result = Indentation::parse_line("  ");
        let expected = Indentation::Spaces(2);

        assert_eq!(result.unwrap(), Some(expected));
    }

    #[test]
    fn line_gets_tabs() {
        let result = Indentation::parse_line("\t");
        let expected = Indentation::Tabs;

        assert_eq!(result.unwrap(), Some(expected));
    }

    #[test]
    fn line_errors_mixed_indent() {
        let result = Indentation::parse_line("  \t");
        assert!(result.is_err());
    }

    #[test]
    fn rope_gets_two_spaces() {
        let result = Indentation::parse(&Rope::from(
            r#"
        // This is a comment
          Testing
          Indented
            Even more indented
            # Comment
            # Comment
            # Comment
        "#,
        ));
        let expected = Indentation::Spaces(2);

        assert_eq!(result.unwrap(), Some(expected));
    }

    #[test]
    fn rope_gets_four_spaces() {
        let result = Indentation::parse(&Rope::from(
            r#"
        fn my_fun_func(&self,
                       another_arg: usize) -> Fun {
            /* Random comment describing program behavior */
            Fun::from(another_arg)
        }
        "#,
        ));
        let expected = Indentation::Spaces(4);

        assert_eq!(result.unwrap(), Some(expected));
    }

    #[test]
    fn rope_returns_none() {
        let result = Indentation::parse(&Rope::from(
            r#"# Readme example
 1. One space.
But the majority is still 0.
"#,
        ));

        assert_eq!(result.unwrap(), None);
    }
    #[test]
    fn parse_bounded_caps_at_max_lines() {
        // Build a rope with mixed indentation: first 10 lines use 2-space indent,
        // remaining 10 use tab indent.  Bounded at 10 lines should see only spaces.
        let head: String = (0..10).map(|_| "  item\n").collect();
        let tail: String = (0..10).map(|_| "\titem\n").collect();
        let rope = Rope::from(format!("{head}{tail}"));

        let bounded = Indentation::parse_bounded(&rope, 10).unwrap();
        assert_eq!(bounded, Some(Indentation::Spaces(2)));

        // Full parse sees mixed and errors.
        assert!(Indentation::parse_bounded(&rope, usize::MAX).is_err());
    }

    #[test]
    fn parse_bounded_with_zero_max_lines_returns_none() {
        let rope = Rope::from("  two space\n");
        let result = Indentation::parse_bounded(&rope, 0).unwrap();
        assert_eq!(result, None);
    }

    /// Performance budget: `Indentation::parse` on a 300 K-line rope must
    /// complete within 100 ms because it is capped at `MAX_INDENT_PROBE_LINES`
    /// (1 000 lines) regardless of file size.
    #[test]
    fn parse_bounded_large_rope_stays_within_budget() {
        use std::time::Instant;
        const LINES: usize = 300_000;
        const BUDGET_MS: u128 = 100;

        let content: String = (0..LINES).map(|_| "  item\n").collect();
        let rope = Rope::from(content);

        let start = Instant::now();
        let _ = Indentation::parse(&rope);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < BUDGET_MS,
            "Indentation::parse on {LINES}-line rope took {}ms, expected < {BUDGET_MS}ms \
             (MAX_INDENT_PROBE_LINES cap may be missing)",
            elapsed.as_millis()
        );
    }
}
