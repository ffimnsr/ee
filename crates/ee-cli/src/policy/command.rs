//! Structured terminal command trust.
//!
//! Command identity is the executable token plus structured argv tokens from
//! `CreateTerminalRequest`; display text is never matched.  Shell wrappers,
//! control characters, empty tokens, and invalid Unicode boundaries are
//! rejected during rule creation and load, and the request cwd must resolve
//! to a canonical workspace root (relative, external, traversal, and
//! symlink-escape cwds prompt instead).

use std::path::{Component, Path, PathBuf};

use super::WorkspaceIdentity;
use super::paths::canonical_or_lexical;

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

/// Whether `token` is a leading environment assignment (`NAME=value`) rather
/// than an executable or argument. Assignment prefixes change the shell
/// execution target, so they are never eligible for terminal trust.
pub(crate) fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric() && (index > 0 || !character.is_ascii_digit())
        })
}

/// Whether an executable token resolves through `PATH` instead of naming an
/// explicit filesystem path. Prompt-free execution must not auto-run a binary
/// outside the workspace by path, so the terminal execution grant requires a
/// bare program name.
pub(crate) fn is_bare_executable(executable: &str) -> bool {
    !executable.is_empty()
        && !executable.chars().any(|character| matches!(character, '/' | '\\' | '~'))
        && !has_windows_drive_prefix(executable)
}

/// Best-effort workspace containment for one argv token of a command that may
/// auto-run under the workspace terminal execution grant.
///
/// Tokens that cannot name a filesystem target are inert. Path-like tokens
/// (absolute paths, `.`/`..`, separators) must resolve inside
/// `workspace_root`, with `cwd` as the base for relative values. Ambiguous
/// option-embedded paths (`-I/usr/include`, `-C..`, `--out=/tmp/x`) and
/// `~`-prefixed values are rejected so the request falls back to approval
/// instead of guessing. This is not a sandbox: accepted executables can still
/// reach outside the workspace themselves.
pub(crate) fn terminal_operand_stays_in_workspace(
    token: &str,
    cwd: &Path,
    workspace_root: &Path,
) -> bool {
    if token == "--" || token.contains("://") {
        return true;
    }
    if token.starts_with('-') {
        if let Some((_, value)) = token.split_once('=') {
            return value.is_empty()
                || terminal_operand_stays_in_workspace(value, cwd, workspace_root);
        }
        // Attached option values (`-I/usr/include`, `-C..`) are not
        // separable from the flag without command-specific knowledge.
        return !token.chars().any(|character| matches!(character, '/' | '\\' | '~'))
            && !token.contains("..");
    }
    if let Some((_, value)) = token.split_once('=') {
        return value.is_empty() || terminal_operand_stays_in_workspace(value, cwd, workspace_root);
    }
    if token.starts_with('~') {
        return false;
    }
    // Values such as `key:/tmp/x` can hide an absolute path after the colon.
    if token.contains(':') && token.chars().any(|character| character == '/' || character == '\\') {
        return false;
    }
    if !looks_like_filesystem_path(token) {
        return true;
    }
    let path = Path::new(token);
    let resolved = if path.is_absolute() {
        canonical_or_lexical(path)
    } else {
        canonical_or_lexical(&cwd.join(path))
    };
    resolved.starts_with(workspace_root)
}

fn looks_like_filesystem_path(token: &str) -> bool {
    if token == "." || token == ".." {
        return true;
    }
    let path = Path::new(token);
    path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
        || token.contains(std::path::MAIN_SEPARATOR)
}

fn has_windows_drive_prefix(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Splits one ACP v1 `terminal/create` request into the structured command
/// used by trust evaluation. `command` may be a single simple shell command
/// (`"mk lf"`) or a bare executable with structured `args`; both forms
/// normalize to the same executable + argv tokens.
///
/// Shell syntax, control characters, empty tokens, and leading environment
/// assignments are rejected. Shell wrappers are rejected by
/// [`parse_terminal_command`]; the policy-identity variant deliberately keeps
/// them so an explicit persistent deny can still target a wrapper invocation.
pub(crate) fn split_terminal_policy_command(
    command: &str,
    args: &[String],
) -> Result<(String, Vec<String>), String> {
    let (executable, mut argv) = split_simple_shell_tokens(command)?;
    if is_env_assignment(&executable) {
        return Err("environment assignments are ineligible for terminal trust".to_string());
    }
    validate_argv_tokens(args)?;
    argv.extend_from_slice(args);
    validate_argv_tokens(&argv)?;
    Ok((executable, argv))
}

/// Strict variant used for prompt-free trust eligibility: additionally rejects
/// shell wrappers, which would bypass the structured argv contract.
pub(crate) fn parse_terminal_command(
    command: &str,
    args: &[String],
) -> Result<(String, Vec<String>), String> {
    let (executable, argv) = split_terminal_policy_command(command, args)?;
    validate_command_tokens(&executable, &argv)?;
    Ok((executable, argv))
}

/// Splits an agent-owned ACP v2 command string into an executable and argv
/// only when it is a single simple command with no shell syntax.
///
/// The host never parses this text for execution; it only classifies the
/// command against the curated registry. Anything that could change meaning
/// under a shell (quotes, escapes, expansions, redirections, pipes,
/// separators, globs, comments, control characters) is rejected so the
/// request falls back to an explicit prompt.
pub(crate) fn split_simple_shell_command(command: &str) -> Result<(String, Vec<String>), String> {
    let (executable, argv) = split_simple_shell_tokens(command)?;
    validate_command_tokens(&executable, &argv)?;
    Ok((executable, argv))
}

/// Tokenizes a simple shell command without applying the executable-trust
/// checks. Callers decide whether shell wrappers are eligible for their
/// identity.
fn split_simple_shell_tokens(command: &str) -> Result<(String, Vec<String>), String> {
    const FORBIDDEN: &str = "'\"\\$`|&;<>(){}[]*?!#~";
    if command.is_empty() {
        return Err("command must not be empty".to_string());
    }
    if let Some(character) =
        command.chars().find(|character| character.is_control() || FORBIDDEN.contains(*character))
    {
        return Err(format!("command contains shell syntax: {character:?}"));
    }
    let tokens =
        command.split(' ').filter(|token| !token.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let Some((executable, argv)) = tokens.split_first() else {
        return Err("command must not be empty".to_string());
    };
    Ok((executable.clone(), argv.to_vec()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_commands_split_into_executable_and_argv() {
        assert_eq!(
            split_simple_shell_command("mk lf").unwrap(),
            (String::from("mk"), vec![String::from("lf")])
        );
        assert_eq!(
            split_simple_shell_command("  cargo   test --quiet  ").unwrap(),
            (String::from("cargo"), vec![String::from("test"), String::from("--quiet")])
        );
        assert_eq!(split_simple_shell_command("pwd").unwrap(), (String::from("pwd"), Vec::new()));
    }

    #[test]
    fn terminal_requests_normalize_text_and_structured_args() {
        let args =
            |values: &[&str]| values.iter().map(|value| (*value).to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_terminal_command("mk lf", &[]).unwrap(),
            (String::from("mk"), vec![String::from("lf")])
        );
        assert_eq!(
            parse_terminal_command("mk", &args(&["lf"])).unwrap(),
            (String::from("mk"), vec![String::from("lf")])
        );
        assert_eq!(
            parse_terminal_command("git status", &args(&["--short"])).unwrap(),
            (String::from("git"), args(&["status", "--short"]))
        );
        for request in [
            ("FOO=bar ls", args(&[])),
            ("PATH=/tmp/evil ls", args(&[])),
            ("sh -c ls", args(&[])),
            ("mk lf | cat", args(&[])),
            ("mk `id`", args(&[])),
        ] {
            assert!(
                parse_terminal_command(request.0, &request.1).is_err(),
                "{:?} must stay ineligible for terminal trust",
                request.0
            );
        }
    }

    #[test]
    fn shell_syntax_is_rejected() {
        for command in [
            "mk lf | cat",
            "mk lf > out.txt",
            "mk lf && rm -rf /",
            "echo $HOME",
            "echo `id`",
            "echo 'quoted'",
            "echo \"quoted\"",
            "echo *.rs",
            "echo #comment",
            "mk lf;",
            "echo $(id)",
            "echo \\n",
            "echo\tvalue",
        ] {
            assert!(
                split_simple_shell_command(command).is_err(),
                "{command:?} must not parse as a simple command"
            );
        }
    }

    #[test]
    fn empty_and_control_input_is_rejected() {
        assert!(split_simple_shell_command("").is_err());
        assert!(split_simple_shell_command("   ").is_err());
        assert!(split_simple_shell_command("mk\nlf").is_err());
        assert!(split_simple_shell_command("mk\0lf").is_err());
    }
}
