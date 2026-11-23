//! Shared interactive prompt helpers for setup wizards (agents, mcp).
//!
//! Kept in one module so both wizards prompt, confirm, and validate input
//! identically.

use std::io::{self, Write as _};
use std::process::{Command, Stdio};

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

/// Result of interpreting one raw selection entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectionOutcome {
    /// The user selected a listed value (or typed an exact listed value).
    Value(String),
    /// The user selected the trailing "Other" entry.
    Other,
    /// The user pressed Enter without typing.
    Empty,
    /// The input matches neither an index nor a listed value.
    Invalid,
}

/// Interprets a raw selection against numbered choices. `len + 1` is reserved
/// for the "Other" entry, so callers can offer a custom value without allowing
/// accidental typos to become configuration.
pub(crate) fn interpret_selection(raw: &str, choices: &[(String, String)]) -> SelectionOutcome {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return SelectionOutcome::Empty;
    }
    if let Ok(index) = trimmed.parse::<usize>() {
        if index == choices.len() + 1 {
            return SelectionOutcome::Other;
        }
        if let Some((value, _)) = index.checked_sub(1).and_then(|index| choices.get(index)) {
            return SelectionOutcome::Value(value.clone());
        }
        return SelectionOutcome::Invalid;
    }
    if let Some((value, _)) = choices.iter().find(|(value, _)| value == trimmed) {
        return SelectionOutcome::Value(value.clone());
    }
    SelectionOutcome::Invalid
}

/// Prompts for one value from a numbered list. A trailing "Other" entry lets
/// the user type an exact value that is not in the suggested set.
pub(crate) fn prompt_selection(
    label: &str,
    choices: &[(String, String)],
    default: Option<&str>,
    required: bool,
) -> Result<Option<String>, String> {
    println!("{label}:");
    for (index, (_, choice_label)) in choices.iter().enumerate() {
        println!("  {}) {choice_label}", index + 1);
    }
    println!("  {}) Other (type an exact value)", choices.len() + 1);

    let range_end = choices.len() + 1;
    let hint = match default {
        Some(default) => format!(" (press Enter for {default})"),
        None if required => String::new(),
        None => String::from(" (press Enter to skip)"),
    };
    loop {
        let raw = prompt_line(&format!("Select [1-{range_end}]{hint}: "))?;
        match interpret_selection(&raw, choices) {
            SelectionOutcome::Value(value) => return Ok(Some(value)),
            SelectionOutcome::Other => return prompt_value("Exact value", None, true),
            SelectionOutcome::Empty => {
                if let Some(default) = default {
                    return Ok(Some(default.to_owned()));
                }
                if !required {
                    return Ok(None);
                }
                eprintln!("{label} is required.");
            }
            SelectionOutcome::Invalid => {
                eprintln!("Enter a number from 1 through {range_end}, or an exact listed value.")
            }
        }
    }
}

/// Shortest list that opens a searchable picker instead of printing entries.
const FZF_MIN_CHOICES: usize = 8;

/// Prompts over a curated list, but opens a fuzzy finder (`fzf`, else `skim`'s
/// `sk`) over the complete value set when the input can fetch more values
/// remotely. Aborting the picker falls back to the numbered curated list, so the
/// default/skip rules still apply.
pub(crate) fn prompt_searchable_selection(
    label: &str,
    choices: &[(String, String)],
    default: Option<&str>,
    required: bool,
    fetch_all: impl FnOnce() -> Result<Vec<(String, String)>, String>,
) -> Result<Option<String>, String> {
    if choices.len() >= FZF_MIN_CHOICES
        && let Some(picker) = SearchPicker::detect()
    {
        match fetch_all() {
            Ok(all) if all.len() > choices.len() => {
                if let PickerOutcome::Value(value) = picker_select(picker, label, &all)? {
                    return Ok(Some(value));
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!("warning: cannot fetch the full {label} list: {error}"),
        }
    }
    prompt_selection(label, choices, default, required)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchPicker {
    Fzf,
    Skim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerOutcome {
    Value(String),
    Skipped,
    Unavailable,
}

impl SearchPicker {
    /// First installed fuzzy finder; both accept the fzf-style flags used below.
    fn detect() -> Option<Self> {
        [Self::Fzf, Self::Skim].into_iter().find(|picker| picker.installed())
    }

    fn binary(self) -> &'static str {
        match self {
            Self::Fzf => "fzf",
            Self::Skim => "sk",
        }
    }

    fn installed(self) -> bool {
        Command::new(self.binary())
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// Opens a fuzzy finder over tab-separated `value<TAB>label` records.
fn picker_select(
    picker: SearchPicker,
    label: &str,
    choices: &[(String, String)],
) -> Result<PickerOutcome, String> {
    let mut child = match Command::new(picker.binary())
        .args(["--delimiter=\t", "--with-nth=2..", "--print-query"])
        .arg("--prompt")
        .arg(format!("{label}> "))
        .arg("--header")
        .arg("Enter selects · type an exact id to use it · Esc skips")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("warning: cannot start {}: {error}", picker.binary());
            return Ok(PickerOutcome::Unavailable);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        for (value, choice_label) in choices {
            if writeln!(stdin, "{}", fzf_record(value, choice_label)).is_err() {
                break;
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for {}: {error}", picker.binary()))?;
    Ok(interpret_picker_output(&String::from_utf8_lossy(&output.stdout), output.status.code()))
}

/// One picker record; embedded tabs/newlines would corrupt the protocol.
fn fzf_record(value: &str, label: &str) -> String {
    format!("{}\t{}", sanitize_field(value), sanitize_field(label))
}

fn sanitize_field(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

/// Mirrors `fzf --print-query`: line 1 is the query, line 2 (optional) is the
/// selected record. Exit code 1 means "no match", which is the picker's
/// "Other" path: the typed text becomes the exact value. An interrupt (130)
/// or any other failure skips without selecting whatever text was typed.
fn interpret_picker_output(stdout: &str, exit_code: Option<i32>) -> PickerOutcome {
    match exit_code {
        Some(0) => {}
        Some(1) => {
            let query = stdout.lines().next().unwrap_or_default().trim();
            return if query.is_empty() {
                PickerOutcome::Skipped
            } else {
                PickerOutcome::Value(String::from(query))
            };
        }
        _ => return PickerOutcome::Skipped,
    }
    let mut lines = stdout.lines();
    let query = lines.next().unwrap_or_default().trim();
    if let Some(selection) = lines.next().filter(|line| !line.trim().is_empty()) {
        let value = selection.split('\t').next().unwrap_or_default().trim();
        if !value.is_empty() {
            return PickerOutcome::Value(String::from(value));
        }
    }
    if query.is_empty() {
        PickerOutcome::Skipped
    } else {
        PickerOutcome::Value(String::from(query))
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
    fn selection_interprets_indices_exact_values_other_and_invalid_input() {
        let choices = vec![
            (String::from("zen"), String::from("zen: pay-as-you-go")),
            (String::from("go"), String::from("go: subscription")),
        ];

        assert_eq!(interpret_selection("2", &choices), SelectionOutcome::Value(String::from("go")));
        assert_eq!(
            interpret_selection("zen", &choices),
            SelectionOutcome::Value(String::from("zen"))
        );
        assert_eq!(interpret_selection("3", &choices), SelectionOutcome::Other);
        assert_eq!(interpret_selection("", &choices), SelectionOutcome::Empty);
        for invalid in ["0", "4", "opencode", "1.5"] {
            assert_eq!(interpret_selection(invalid, &choices), SelectionOutcome::Invalid);
        }
    }

    #[test]
    fn picker_output_prefers_the_selection_then_falls_back_to_the_typed_query() {
        assert_eq!(
            interpret_picker_output("gpt\ngpt-5.5\tgpt-5.5\n", Some(0)),
            PickerOutcome::Value(String::from("gpt-5.5"))
        );
        assert_eq!(
            interpret_picker_output("custom-id\n", Some(1)),
            PickerOutcome::Value(String::from("custom-id")),
            "exit code 1 is `no match`, so the typed value is used verbatim"
        );
        assert_eq!(interpret_picker_output("\n", Some(0)), PickerOutcome::Skipped);
        for exit_code in [Some(130), Some(2), None] {
            assert_eq!(
                interpret_picker_output("gpt\ngpt-5.5\tgpt-5.5\n", exit_code),
                PickerOutcome::Skipped,
                "aborting or failing never selects whatever text was typed"
            );
        }
    }

    #[test]
    fn fzf_records_escape_delimiters() {
        assert_eq!(fzf_record("model", "model\tlabel\nnext"), "model\tmodel label next");
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
