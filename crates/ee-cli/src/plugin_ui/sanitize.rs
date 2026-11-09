//! Bounded, escape-free text handling for plugin-provided UI data.
//!
//! The backend already validates contribution descriptors, but the frontend is
//! the last trust boundary before a terminal is written. Every string that can
//! reach the screen is checked here: control characters (including ANSI escape
//! introducers), byte caps, line caps, and item caps. Rejected data is dropped
//! instead of being stripped so a malformed payload never renders partially.

use std::fmt;

/// Maximum bytes accepted for a single text field.
pub const MAX_TEXT_BYTES: usize = 4096;
/// Maximum lines accepted for one multi-line text block.
pub const MAX_LINES: usize = 64;
/// Maximum items accepted from one contribution or dynamic update.
pub const MAX_ITEMS: usize = 512;
/// Maximum columns accepted from one picker descriptor.
pub const MAX_COLUMNS: usize = 32;

/// Why a plugin-provided value was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SanitizeError {
    Empty(&'static str),
    TooLong { field: &'static str, limit: usize },
    TooManyLines { field: &'static str, limit: usize },
    ControlCharacters { field: &'static str },
    TooManyItems { field: &'static str, limit: usize },
}

impl fmt::Display for SanitizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "`{field}` must not be empty"),
            Self::TooLong { field, limit } => write!(f, "`{field}` exceeds {limit} bytes"),
            Self::TooManyLines { field, limit } => write!(f, "`{field}` exceeds {limit} lines"),
            Self::ControlCharacters { field } => {
                write!(f, "`{field}` contains control or ANSI escape characters")
            }
            Self::TooManyItems { field, limit } => {
                write!(f, "`{field}` exceeds {limit} items")
            }
        }
    }
}

impl std::error::Error for SanitizeError {}

/// Reject empty, oversized, or control-bearing text.
pub fn sanitize_text(field: &'static str, value: &str) -> Result<(), SanitizeError> {
    if value.len() > MAX_TEXT_BYTES {
        return Err(SanitizeError::TooLong { field, limit: MAX_TEXT_BYTES });
    }
    if value.chars().any(char::is_control) {
        return Err(SanitizeError::ControlCharacters { field });
    }
    Ok(())
}

/// Reject empty, oversized, or control-bearing text.
pub fn sanitize_non_empty(field: &'static str, value: &str) -> Result<(), SanitizeError> {
    if value.trim().is_empty() {
        return Err(SanitizeError::Empty(field));
    }
    sanitize_text(field, value)
}

/// Validate multi-line text: bounded bytes, bounded lines, no controls except
/// newlines and tabs.
pub fn sanitize_multiline(field: &'static str, value: &str) -> Result<(), SanitizeError> {
    if value.len() > MAX_TEXT_BYTES {
        return Err(SanitizeError::TooLong { field, limit: MAX_TEXT_BYTES });
    }
    if value.chars().any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t')) {
        return Err(SanitizeError::ControlCharacters { field });
    }
    let lines = value.lines().count();
    if lines > MAX_LINES {
        return Err(SanitizeError::TooManyLines { field, limit: MAX_LINES });
    }
    Ok(())
}

/// Validate a collection against [`MAX_ITEMS`].
pub fn sanitize_item_count(field: &'static str, count: usize) -> Result<(), SanitizeError> {
    if count > MAX_ITEMS {
        return Err(SanitizeError::TooManyItems { field, limit: MAX_ITEMS });
    }
    Ok(())
}

/// Validate a picker column count against [`MAX_COLUMNS`].
pub fn sanitize_column_count(field: &'static str, count: usize) -> Result<(), SanitizeError> {
    if count > MAX_COLUMNS {
        return Err(SanitizeError::TooManyItems { field, limit: MAX_COLUMNS });
    }
    Ok(())
}

/// Escape markdown control characters so plugin labels render literally.
///
/// Plugin markdown bodies are rendered by the frontend markdown widget, which
/// only styles text. Titles, status items, and picker cells use this helper so a
/// plugin cannot smuggle emphasis, links, or code fences into chrome.
#[must_use]
pub fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_ansi_sequences_are_rejected() {
        assert!(matches!(
            sanitize_text("text", "\u{1b}[31mred"),
            Err(SanitizeError::ControlCharacters { .. })
        ));
        assert!(matches!(
            sanitize_text("text", "bell\u{7}"),
            Err(SanitizeError::ControlCharacters { .. })
        ));
        assert!(matches!(
            sanitize_text("text", "c1\u{9b}"),
            Err(SanitizeError::ControlCharacters { .. })
        ));
    }

    #[test]
    fn oversized_text_is_rejected() {
        let text = "a".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(sanitize_text("text", &text), Err(SanitizeError::TooLong { .. })));
    }

    #[test]
    fn too_many_lines_and_items_are_rejected() {
        let text = "line\n".repeat(MAX_LINES + 1);
        assert!(matches!(
            sanitize_multiline("text", &text),
            Err(SanitizeError::TooManyLines { .. })
        ));
        assert!(matches!(
            sanitize_item_count("items", MAX_ITEMS + 1),
            Err(SanitizeError::TooManyItems { .. })
        ));
        assert!(matches!(
            sanitize_column_count("columns", MAX_COLUMNS + 1),
            Err(SanitizeError::TooManyItems { .. })
        ));
    }

    #[test]
    fn valid_unicode_is_accepted_and_preserved() {
        for text in ["héllo", "日本語", "emoji 🦀"] {
            assert!(sanitize_text("text", text).is_ok());
        }
        assert!(sanitize_multiline("text", "one\ntwo\tthree").is_ok());
        assert!(sanitize_text("text", "tab\tinside").is_err());
    }

    #[test]
    fn markdown_escaping_neutralises_chrome_markup() {
        assert_eq!(escape_markdown("**bold** [x](y)"), "\\*\\*bold\\*\\* \\[x\\](y)");
    }
}
