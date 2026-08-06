//! Structured terminal command trust (Phase 2).
//!
//! Command identity is the executable token plus structured argv tokens from
//! `CreateTerminalRequest`; display text is never matched.  Shell wrappers,
//! control characters, empty tokens, and invalid Unicode boundaries are
//! rejected during rule creation and load, and the request cwd must resolve
//! to a canonical workspace root (relative, external, traversal, and
//! symlink-escape cwds prompt instead).

use std::path::{Path, PathBuf};

use super::WorkspaceIdentity;

/// Shell wrappers that never qualify for persistent command trust: shell
/// interpretation would bypass the structured argv contract.
pub(crate) const SHELL_WRAPPERS: [&str; 8] =
    ["sh", "bash", "zsh", "fish", "dash", "cmd", "powershell", "pwsh"];

/// Whether the executable basename is an ineligible shell wrapper.
pub(crate) fn is_shell_wrapper(executable: &str) -> bool {
    Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SHELL_WRAPPERS.contains(&name))
}

/// Validates one executable token: non-empty, control-free, and never a
/// shell wrapper.
pub(crate) fn validate_executable(executable: &str) -> Result<(), String> {
    if executable.is_empty() {
        return Err("executable must not be empty".to_string());
    }
    if executable.chars().any(|c| c.is_control() || c == '\u{0}') {
        return Err("executable contains control characters".to_string());
    }
    if is_shell_wrapper(executable) {
        return Err(format!("executable {executable:?} is an ineligible shell wrapper"));
    }
    Ok(())
}

/// Validates structured argv tokens.  Tokens are UTF-8 `String`s by
/// construction (ACP/JSON), so invalid Unicode boundaries cannot occur; NUL
/// and control characters are rejected because they would be unsafe or
/// ambiguous at exec time.
pub(crate) fn validate_argv_tokens(argv: &[String]) -> Result<(), String> {
    for token in argv {
        if token.is_empty() {
            return Err("argv token must not be empty".to_string());
        }
        if token.chars().any(|c| c.is_control() || c == '\u{0}') {
            return Err("argv token contains control characters".to_string());
        }
    }
    Ok(())
}

/// Validates the full command identity: executable plus every argv token.
pub(crate) fn validate_command_tokens(executable: &str, argv: &[String]) -> Result<(), String> {
    validate_executable(executable)?;
    validate_argv_tokens(argv)
}

/// Resolves the request cwd: it must be absolute, exist, canonicalize
/// through symlinks to a real directory, and stay inside one of the
/// canonical workspace roots.  Relative, external, traversal, and
/// symlink-escape cwds are rejected.
pub(crate) fn resolve_command_cwd(
    raw: &Path,
    canonical_roots: &[PathBuf],
) -> Result<PathBuf, String> {
    if !raw.is_absolute() {
        return Err("terminal cwd must be absolute".to_string());
    }
    let canonical = std::fs::canonicalize(raw)
        .map_err(|error| format!("terminal cwd does not resolve: {error}"))?;
    if !canonical.is_dir() {
        return Err("terminal cwd is not a directory".to_string());
    }
    if !canonical_roots.iter().any(|root| canonical.starts_with(root)) {
        return Err("terminal cwd escapes the workspace".to_string());
    }
    Ok(canonical)
}

/// Validated command invocation: structured executable + argv tokens and the
/// canonical in-workspace cwd.  Built only after request validation; never
/// derived from display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandInvocation {
    /// Canonical workspace identity the invocation belongs to.
    pub(crate) workspace: WorkspaceIdentity,
    pub(crate) executable: String,
    pub(crate) argv: Vec<String>,
    /// Canonical (symlink-resolved) in-workspace cwd.
    pub(crate) canonical_cwd: PathBuf,
}

/// Stable rule id for a newly created command grant (`cmd_…`).
pub(crate) fn generate_command_rule_id() -> String {
    format!("cmd_{:016x}", rand::random::<u64>())
}
