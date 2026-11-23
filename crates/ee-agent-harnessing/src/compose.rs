//! Composition-note parser.
//!
//! The composer contract requires one line per decision:
//! `kept|revised|dropped -> <edit> -> <trajectory moment with counts>`.
//! The parser is deliberately tolerant of surrounding prose while recording
//! only lines that satisfy the contract, so a malformed composer reply is
//! visible without discarding an otherwise valid composed tree.

/// Maximum notes retained by one task.
pub const MAX_COMPOSE_NOTES: usize = 64;
/// Maximum characters retained from one note line.
pub const MAX_NOTE_CHARS: usize = 512;

/// One parsed composition decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeNote {
    /// `kept`, `revised`, or `dropped`.
    pub decision: String,
    /// The edit the decision applies to.
    pub edit: String,
    /// The trajectory moment with counter evidence.
    pub moment: String,
}

/// Parses composition notes, retaining at most [`MAX_COMPOSE_NOTES`] rows.
#[must_use]
pub fn parse_notes(text: &str) -> Vec<ComposeNote> {
    let mut notes = Vec::new();
    for line in text.lines().map(str::trim) {
        if notes.len() >= MAX_COMPOSE_NOTES {
            break;
        }
        let mut parts = line.splitn(3, "->");
        let Some(decision) = parts.next() else { continue };
        let Some(edit) = parts.next() else { continue };
        let Some(moment) = parts.next() else { continue };
        let decision = decision.trim().to_ascii_lowercase();
        if !matches!(decision.as_str(), "kept" | "revised" | "dropped") {
            continue;
        }
        let edit = edit.trim();
        let moment = moment.trim();
        if edit.is_empty() || moment.is_empty() {
            continue;
        }
        notes.push(ComposeNote { decision, edit: truncate(edit), moment: truncate(moment) });
    }
    notes
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_NOTE_CHARS {
        return text.to_string();
    }
    let mut value: String = text.chars().take(MAX_NOTE_CHARS).collect();
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_lines_parse_in_order() {
        let notes = parse_notes(
            "intro\n\
             kept -> guardrails.md -> probe-0-inv delivered (3 tool calls)\n\
             dropped -> scripts/old.sh -> reference run had no delivery\n\
             revised -> skills/demo/SKILL.md -> composed probe 4/4 delivered",
        );
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].decision, "kept");
        assert_eq!(notes[0].edit, "guardrails.md");
        assert!(notes[0].moment.contains("3 tool calls"));
        assert_eq!(notes[2].decision, "revised");
    }

    #[test]
    fn malformed_lines_are_ignored_and_bounds_hold() {
        assert!(parse_notes("kept guardrails.md no arrows").is_empty());
        assert!(parse_notes("accepted -> x -> y").is_empty());
        assert!(parse_notes("kept ->  -> y").is_empty());
        let many = (0..100)
            .map(|index| format!("kept -> edit-{index} -> moment-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse_notes(&many).len(), MAX_COMPOSE_NOTES);
    }
}
