//! Shared interactive prompt helpers for setup wizards (agents, mcp).
//!
//! Kept in one module so both wizards prompt, confirm, and validate input
//! identically.

use std::io::{self, Write as _};

use crate::secrets;

/// Reads one trimmed line from stdin. Returns an error when input closes.
pub(crate) fn prompt_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|error| format!("cannot write setup prompt: {error}"))?;
    let mut line = String::new();
    let read = io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("cannot read setup input: {error}"))?;
    if read == 0 {
        return Err(String::from("setup input closed"));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Asks a `[y/N]` style question; only `y`/`yes` counts as yes.
pub(crate) fn confirm(prompt: &str) -> Result<bool, String> {
    Ok(matches!(prompt_line(prompt)?.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Prompts for an optional-or-required value, honoring defaults. Never
/// returns `None` when required.
pub(crate) fn prompt_value(
    label: &str,
    default: Option<&str>,
    required: bool,
) -> Result<Option<String>, String> {
    let prompt = match default {
        Some(default) => format!("{label} [{default}]: "),
        None if required => format!("{label}: "),
        None => format!("{label} (press Enter to skip): "),
    };
    loop {
        let value = prompt_line(&prompt)?;
        if !value.is_empty() {
            return Ok(Some(value));
        }
        if let Some(default) = default {
            return Ok(Some(default.to_owned()));
        }
        if !required {
            return Ok(None);
        }
        eprintln!("{label} is required.");
    }
}

/// Reads a value without terminal echo (prompted only once the caller has
/// already established the value is a secret).
pub(crate) fn read_hidden_value(
    required: bool,
) -> Result<Option<zeroize::Zeroizing<String>>, String> {
    let mut stdin = io::empty();
    let mut terminal = secrets::cli::HiddenTerminalSecretSource;
    match secrets::cli::read_secret_value(false, &mut stdin, &mut terminal) {
        Ok(value) => Ok(Some(value)),
        Err(secrets::cli::SecretsCliError::EmptySecret) if !required => Ok(None),
        Err(error) => Err(format!("cannot read secret value: {error}")),
    }
}

/// Server-id style validation shared by the agent and mcp setup wizards.
/// Restricted to TOML bare-key-safe characters so dotted-key nesting and
/// secret-store name breakage are impossible.
pub(crate) fn validate_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Environment variable name rule shared by manifest and wizard validation.
pub(crate) fn validate_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(character) if character.is_ascii_alphabetic() || character == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// HTTP header name rule: ASCII token characters only, no colon or spaces.
pub(crate) fn validate_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|byte| byte.is_ascii_graphic() && !matches!(byte, b':' | b' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_accepts_bare_key_safe_ids_and_rejects_dots_and_slashes() {
        for valid in ["opencode", "claude-code", "gemini_cli", "Agent2"] {
            assert!(validate_server_name(valid), "{valid} should be valid");
        }
        for invalid in ["", "a.b", "a/b", "a b", "-leading", "_leading", "x".repeat(65).as_str()] {
            assert!(!validate_server_name(invalid), "{invalid} should be invalid");
        }
    }

    #[test]
    fn env_name_requires_letter_or_underscore_start() {
        for valid in ["API_KEY", "_SECRET", "openrouter_model2"] {
            assert!(validate_env_name(valid), "{valid} should be valid");
        }
        for invalid in ["", "1KEY", "A-B", "A B", "KEY:VALUE"] {
            assert!(!validate_env_name(invalid), "{invalid} should be invalid");
        }
    }

    #[test]
    fn header_name_requires_ascii_token_without_colon_or_space() {
        for valid in ["Authorization", "X-API-Key", "x_request-id", "Accept"] {
            assert!(validate_header_name(valid), "{valid} should be valid");
        }
        for invalid in ["", "X-Key:", "X Key", "X\tKey", "colón"] {
            assert!(!validate_header_name(invalid), "{invalid} should be invalid");
        }
    }
}
