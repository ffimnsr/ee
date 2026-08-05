//! Minimal `.env` parsing.
//!
//! Kept local and non-mutating: [`parse_dotenv`] never touches the process
//! environment, and [`load_dotenv`] treats a missing file as an empty map.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// Reads and parses the `.env` file at `path`; a missing file yields an
/// empty map, other I/O failures propagate.
pub fn load_dotenv(path: &Path) -> io::Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_dotenv(&text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error),
    }
}

/// Parses `KEY=VALUE` lines: comments and blank lines are skipped, an
/// `export ` prefix is accepted, invalid names are skipped, and values are
/// unquoted ([`unquote_dotenv_value`]).
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(is_env_name_char) {
            continue;
        }
        values.insert(name.to_string(), unquote_dotenv_value(value.trim()));
    }
    values
}

fn is_env_name_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

/// Strips matching quotes; double-quoted values additionally decode the
/// `\n`, `\r`, `\t`, `\"`, and `\\` escapes, leaving unknown escapes as-is.
fn unquote_dotenv_value(value: &str) -> String {
    let Some(stripped) = value.strip_prefix('"').and_then(|value| value.strip_suffix('"')) else {
        return value
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .unwrap_or(value)
            .to_string();
    };
    let mut out = String::new();
    let mut chars = stripped.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_values_without_mutating_process_env() {
        let parsed = parse_dotenv(
            r#"
# comment
OPENROUTER_API_KEY=sk-test
export OPENROUTER_SITE_URL="https://example.test"
OPENROUTER_SYSTEM_PROMPT='hello agent'
BAD LINE
BAD-NAME=value
ESCAPED="line\nnext"
"#,
        );

        assert_eq!(parsed.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-test"));
        assert_eq!(
            parsed.get("OPENROUTER_SITE_URL").map(String::as_str),
            Some("https://example.test")
        );
        assert_eq!(parsed.get("OPENROUTER_SYSTEM_PROMPT").map(String::as_str), Some("hello agent"));
        assert_eq!(parsed.get("ESCAPED").map(String::as_str), Some("line\nnext"));
        assert!(!parsed.contains_key("BAD-NAME"));
    }

    #[test]
    fn loads_dotenv_from_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "OPENROUTER_API_KEY=from-file\n").unwrap();

        let loaded = load_dotenv(&path).unwrap();

        assert_eq!(loaded.get("OPENROUTER_API_KEY").map(String::as_str), Some("from-file"));
    }

    #[test]
    fn missing_dotenv_file_is_an_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_dotenv(&dir.path().join(".env")).unwrap();
        assert!(loaded.is_empty());
    }
}
