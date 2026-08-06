//! Shared slash-command parsing for ACP agent providers.
//!
//! Agents advertise slash commands through `available_commands_update` and
//! receive them as ordinary `session/prompt` text; this module owns the
//! deterministic parsing both providers (and future ones) share.  A command
//! is recognized only when the first non-space character of the prompt is
//! `/`; the command name is the token up to the first whitespace, and any
//! remaining text is preserved exactly as the command's instructions (only
//! the whitespace separating the name from the instructions is normalized
//! away).  Prefix collisions such as `/compactness` never match `/compact`.

use crate::{AvailableCommand, AvailableCommandInput, UnstructuredCommandInput};

/// The advertised slash-command name for LLM session compaction.
pub const COMPACT_COMMAND_NAME: &str = "compact";

/// One parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// The command name token (e.g. `compact`).
    pub name: String,
    /// Optional instruction text after the command name, preserved exactly
    /// (interior and trailing whitespace untouched); `None` when the prompt
    /// carries nothing after the name.
    pub instructions: Option<String>,
}

/// Parses a prompt as a slash command, or returns `None` when the prompt is
/// not a command (first non-space character is not `/`) or names nothing.
///
/// The command name is the token up to the first whitespace; leading
/// whitespace is ignored before the `/`, and the whitespace separating the
/// name from the instructions is normalized away (the instructions
/// themselves are preserved exactly).
#[must_use]
pub fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed[1..].splitn(2, char::is_whitespace);
    let name = parts.next()?;
    if name.is_empty() {
        return None;
    }
    let rest = parts.next().unwrap_or_default();
    let instructions = rest.trim_start();
    Some(SlashCommand {
        name: name.to_string(),
        instructions: (!instructions.is_empty()).then(|| instructions.to_string()),
    })
}

/// Whether the prompt is a `/compact` (or `/compact <instructions>`) command.
#[must_use]
pub fn is_compact_command(text: &str) -> bool {
    parse_slash_command(text).is_some_and(|command| command.name == COMPACT_COMMAND_NAME)
}

/// The `/compact` command advertised by providers through
/// `available_commands_update`: name, description, and an unstructured input
/// hint shown as a draft placeholder by clients.
#[must_use]
pub fn compact_available_command() -> AvailableCommand {
    AvailableCommand::new(
        COMPACT_COMMAND_NAME,
        "Summarize the session's history into a compact continuation summary, preserving decisions, constraints, and validation results.",
    )
    .input(AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
        "optional instructions for what the summary must preserve",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_command() {
        assert_eq!(
            parse_slash_command("/compact"),
            Some(SlashCommand { name: "compact".into(), instructions: None })
        );
    }

    #[test]
    fn parses_command_with_leading_whitespace() {
        assert_eq!(
            parse_slash_command("  \t/compact"),
            Some(SlashCommand { name: "compact".into(), instructions: None })
        );
    }

    #[test]
    fn parses_command_with_instructions_preserved_exactly() {
        // Interior spacing, case, and trailing text survive untouched; only
        // the whitespace between the name and the instructions is skipped.
        assert_eq!(
            parse_slash_command("/compact   focus on the  Auth  module "),
            Some(SlashCommand {
                name: "compact".into(),
                instructions: Some("focus on the  Auth  module ".into()),
            })
        );
        assert_eq!(
            parse_slash_command("/compact keep API v2"),
            Some(SlashCommand { name: "compact".into(), instructions: Some("keep API v2".into()) })
        );
    }

    #[test]
    fn command_with_only_whitespace_after_name_has_no_instructions() {
        assert_eq!(
            parse_slash_command("/compact   "),
            Some(SlashCommand { name: "compact".into(), instructions: None })
        );
    }

    #[test]
    fn empty_and_normal_prompts_are_not_commands() {
        assert_eq!(parse_slash_command(""), None);
        assert_eq!(parse_slash_command("   "), None);
        assert_eq!(parse_slash_command("hello world"), None);
        assert_eq!(parse_slash_command("fix the /compact parser"), None);
        assert_eq!(parse_slash_command("compact"), None, "no slash, no command");
        assert_eq!(parse_slash_command("/"), None, "bare slash names nothing");
    }

    #[test]
    fn prefix_collisions_never_match() {
        assert_eq!(
            parse_slash_command("/compactness"),
            Some(SlashCommand { name: "compactness".into(), instructions: None })
        );
        assert!(!is_compact_command("/compactness"));
        assert!(!is_compact_command("/compactx"));
        assert!(!is_compact_command("  /compactor"));
    }

    #[test]
    fn compact_detection_matches_name_exactly() {
        assert!(is_compact_command("/compact"));
        assert!(is_compact_command("/compact with instructions"));
        assert!(is_compact_command("  /compact"));
        assert!(!is_compact_command("compact"));
        assert!(!is_compact_command("/compactness"));
        assert!(!is_compact_command("/compactness with args"));
        assert!(!is_compact_command("run /compact now"));
    }

    #[test]
    fn advertised_compact_command_carries_description_and_input_hint() {
        let command = compact_available_command();
        assert_eq!(command.name, COMPACT_COMMAND_NAME);
        assert!(!command.description.is_empty(), "description advertised");
        let hint = match command.input.expect("input advertised") {
            AvailableCommandInput::Unstructured(input) => input.hint,
            _ => panic!("expected unstructured input"),
        };
        assert!(!hint.is_empty(), "hint advertised");
    }

    #[test]
    fn advertised_command_roundtrips_through_json() {
        let command = compact_available_command();
        let json = serde_json::to_string(&command).expect("serializes");
        let restored: AvailableCommand = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, command);
        assert!(json.contains("compact"), "{json}");
        assert!(json.contains("hint"), "{json}");
    }
}
