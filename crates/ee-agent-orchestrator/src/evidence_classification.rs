//! Classifies tool executions into completion-evidence roles.
//!
//! The stop gate previously treated every tool name containing `valid`,
//! `test`, or `check` as validation evidence. That both missed real validation
//! run through a terminal and accepted unrelated names. This module owns the
//! classification policy:
//!
//! - explicit tool-name tokens (`cargo_check`, `ee_get_diagnostics`, ...),
//! - terminal command intent for `create_terminal` / `ee_terminal_create`,
//! - diagnostics and diff-review observation.
//!
//! Classification never upgrades a failed execution. It only decides which
//! evidence bucket an execution log entry belongs to.

use std::path::Path;

use crate::tools::ToolExecutionLogEntry;

/// Returns whether a tool name is a validation tool by token, not substring.
#[must_use]
pub fn is_validation_tool_name(name: &str) -> bool {
    name_tokens(name).any(is_validation_token)
}

/// Returns whether a tool name observes diagnostics.
#[must_use]
pub fn is_diagnostics_tool_name(name: &str) -> bool {
    name_tokens(name).any(is_diagnostics_token)
}

/// Returns whether a tool name observes a diff review.
#[must_use]
pub fn is_diff_review_tool_name(name: &str) -> bool {
    name_tokens(name).any(is_diff_token)
}

/// Returns whether an execution log entry is validation evidence.
#[must_use]
pub fn is_validation_log_entry(entry: &ToolExecutionLogEntry) -> bool {
    is_validation_tool_name(&entry.tool_name)
        || terminal_command(entry).is_some_and(command_is_validation)
}

/// Returns whether an execution log entry observes diagnostics.
#[must_use]
pub fn is_diagnostics_log_entry(entry: &ToolExecutionLogEntry) -> bool {
    is_diagnostics_tool_name(&entry.tool_name)
}

/// Returns whether an execution log entry observes a diff review.
#[must_use]
pub fn is_diff_review_log_entry(entry: &ToolExecutionLogEntry) -> bool {
    is_diff_review_tool_name(&entry.tool_name)
        || terminal_command(entry).is_some_and(command_is_diff_review)
}

/// Returns the command that produced validation evidence.
///
/// Terminal entries use the executed command string; server-side validation
/// tools fall back to their stable tool name.
#[must_use]
pub fn validation_command_for_entry(entry: &ToolExecutionLogEntry) -> String {
    terminal_command(entry).unwrap_or(entry.tool_name.as_str()).to_string()
}

/// Returns whether a terminal command is a validation command.
#[must_use]
pub fn command_is_validation(command: &str) -> bool {
    let words = command_words(command);
    if words.is_empty() {
        return false;
    }
    let first = basename(&words[0]);
    if matches!(
        first,
        "pytest" | "jest" | "vitest" | "mocha" | "ctest" | "tox" | "nox" | "cargo-nextest"
    ) {
        return true;
    }
    for pair in words.windows(2) {
        let program = basename(&pair[0]);
        let subcommand = pair[1].as_str();
        if program == "cargo"
            && matches!(subcommand, "test" | "check" | "clippy" | "nextest" | "build")
        {
            return true;
        }
        if program == "go" && matches!(subcommand, "test" | "vet") {
            return true;
        }
        if matches!(program, "npm" | "pnpm" | "yarn" | "bun") && subcommand == "test" {
            return true;
        }
        if matches!(program, "make" | "just" | "task" | "mage")
            && matches!(subcommand, "test" | "check" | "lint" | "validate")
        {
            return true;
        }
        if matches!(program, "dotnet" | "mvn" | "gradle" | "gradlew" | "mix" | "deno")
            && matches!(subcommand, "test" | "check" | "verify" | "build")
        {
            return true;
        }
        if matches!(program, "npx" | "pnpx" | "bunx")
            && matches!(subcommand, "pytest" | "jest" | "vitest" | "mocha" | "tsc")
        {
            return true;
        }
    }
    for triple in words.windows(3) {
        let program = basename(&triple[0]);
        let verb = triple[1].as_str();
        let task = triple[2].as_str();
        if matches!(program, "npm" | "pnpm" | "yarn" | "bun")
            && verb == "run"
            && matches!(task, "test" | "lint" | "check" | "validate")
        {
            return true;
        }
        if matches!(program, "python" | "python3")
            && verb == "-m"
            && matches!(task, "pytest" | "unittest")
        {
            return true;
        }
        if program == "cargo" && verb == "fmt" && task == "--check" {
            return true;
        }
        if program == "pnpm"
            && verb == "exec"
            && matches!(task, "jest" | "vitest" | "pytest" | "tsc")
        {
            return true;
        }
    }
    false
}

/// Returns whether a terminal command reviews a diff.
#[must_use]
pub fn command_is_diff_review(command: &str) -> bool {
    let words = command_words(command);
    words.first().is_some_and(|word| basename(word) == "diff")
        || words
            .windows(2)
            .any(|pair| basename(&pair[0]) == "git" && matches!(pair[1].as_str(), "diff" | "show"))
}

fn terminal_command(entry: &ToolExecutionLogEntry) -> Option<&str> {
    if !matches!(entry.tool_name.as_str(), "create_terminal" | "ee_terminal_create") {
        return None;
    }
    entry.arguments.get("command").and_then(serde_json::Value::as_str)
}

fn name_tokens(name: &str) -> impl Iterator<Item = &str> {
    name.split(['_', '-', '.']).filter(|token| !token.is_empty())
}

fn is_validation_token(token: &str) -> bool {
    matches!(
        token,
        "test"
            | "tests"
            | "testing"
            | "check"
            | "checks"
            | "checking"
            | "lint"
            | "lints"
            | "linting"
            | "valid"
            | "validate"
            | "validation"
            | "verify"
            | "verification"
    )
}

fn is_diagnostics_token(token: &str) -> bool {
    matches!(token, "diagnostic" | "diagnostics")
}

fn is_diff_token(token: &str) -> bool {
    matches!(token, "diff" | "diffs")
}

fn command_words(command: &str) -> Vec<String> {
    command
        .to_ascii_lowercase()
        .split(|character: char| {
            character.is_whitespace()
                || matches!(character, ';' | '&' | '|' | '(' | ')' | '{' | '}' | '\n' | '\r')
        })
        .map(|word| word.trim_matches(|character| matches!(character, '"' | '\'' | '`')))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn basename(word: &str) -> &str {
    Path::new(word).file_name().and_then(|name| name.to_str()).unwrap_or(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::SideEffectClass;
    use serde_json::json;

    fn entry(name: &str, arguments: serde_json::Value) -> ToolExecutionLogEntry {
        ToolExecutionLogEntry {
            tool_call_id: "tc-1".into(),
            tool_name: name.into(),
            side_effect_class: Some(SideEffectClass::Execute),
            arguments,
            success: true,
            summary: "done".into(),
        }
    }

    #[test]
    fn terminal_cargo_test_is_validation_evidence() {
        assert!(is_validation_log_entry(&entry(
            "create_terminal",
            json!({ "command": "cargo test --quiet" }),
        )));
        assert!(is_validation_log_entry(&entry(
            "ee_terminal_create",
            json!({ "command": "cd crates/ee && cargo test --quiet some_test" }),
        )));
    }

    #[test]
    fn common_validation_commands_are_recognized() {
        for command in [
            "cargo check",
            "cargo clippy -- -D warnings",
            "npm test",
            "npm run test",
            "pnpm exec vitest run",
            "python -m pytest tests/",
            "go test ./...",
            "make check",
            "dotnet test",
            "deno check src/mod.ts",
        ] {
            assert!(command_is_validation(command), "{command}");
        }
    }

    #[test]
    fn non_validation_commands_are_not_validation() {
        for command in ["cargo fmt", "ls", "cat Cargo.toml", "git status"] {
            assert!(!command_is_validation(command), "{command}");
        }
    }

    #[test]
    fn terminal_git_diff_is_diff_review_evidence() {
        let diff = entry("ee_terminal_create", json!({ "command": "git diff -- src/lib.rs" }));
        assert!(is_diff_review_log_entry(&diff));
        assert!(!is_validation_log_entry(&diff));
    }

    #[test]
    fn token_matching_does_not_accept_substrings() {
        assert!(!is_validation_tool_name("contest"));
        assert!(!is_validation_tool_name("checkout"));
        assert!(is_validation_tool_name("cargo_check"));
        assert!(is_diagnostics_tool_name("ee_diagnostics"));
        assert!(is_diff_review_tool_name("ee_git_diff"));
    }

    #[test]
    fn validation_command_uses_terminal_command_string() {
        let validation = entry("ee_terminal_create", json!({ "command": "cargo test --quiet" }));
        assert_eq!(validation_command_for_entry(&validation), "cargo test --quiet");
        let tool = entry("cargo_check", json!({}));
        assert_eq!(validation_command_for_entry(&tool), "cargo_check");
    }
}
