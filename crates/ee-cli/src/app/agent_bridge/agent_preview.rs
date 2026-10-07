//! ee-validated operation previews for the agent's own permission prompt.
//!
//! Phase 2 surface: [`AgentOperationKey::verified_summary`] renders only
//! ee-validated facts — canonical path, byte size, and content digest for
//! writes; validated argv for terminals — and [`AgentPermissionPreview`]
//! discloses what ee itself checked in the permission prompt, composer,
//! transcript, and transcript export.
//!
//! Kept separate from the alignment ledger (`agent_permissions.rs`) so that
//! file stays below the repository's 1K LOC threshold; the ledger itself never
//! renders anything.

use super::agent_permissions::{AgentGrantClass, AgentOperationKey};
use super::app_web::sha256_hex;

impl AgentOperationKey {
    /// One-line ee-computed summary shown in the agent's permission prompt.
    ///
    /// Only ee-validated facts appear — the canonical path, the byte size,
    /// and a content digest for writes; the validated argv for terminals.
    /// Raw file content is never rendered.
    #[must_use]
    pub(crate) fn verified_summary(&self) -> String {
        match self {
            Self::Write { path, content } => {
                let digest = sha256_hex(content.as_bytes());
                format!(
                    "write {} · {} bytes · sha256:{}",
                    display_token(&path.display().to_string()),
                    content.len(),
                    &digest[..12]
                )
            }
            Self::Terminal { executable, argv } => {
                let mut summary = String::from("terminal ");
                summary.push_str(&display_token(executable));
                for token in argv {
                    summary.push(' ');
                    summary.push_str(&display_token(token));
                }
                summary
            }
        }
    }
}

/// Quotes tokens that would otherwise be ambiguous in a single-line summary:
/// whitespace, embedded quotes, and control characters (a crafted in-workspace
/// path must not move the terminal cursor or forge extra summary fields).
/// `{:?}` escaping covers all three.
fn display_token(token: &str) -> String {
    if token
        .chars()
        .any(|character| character.is_whitespace() || character == '"' || character.is_control())
    {
        format!("{token:?}")
    } else {
        token.to_string()
    }
}

/// What ee could verify about an agent-proposed operation, shown in the
/// agent's own permission prompt (Phase 2) and the transcript export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum AgentPermissionPreview {
    /// ee-validated facts; exact alignment can reuse a matching decision.
    Verified(String),
    /// No ee-validated identity; heuristic alignment covers the next matching
    /// operation when the `heuristic` mode is configured.
    Heuristic { class: Option<AgentGrantClass> },
    /// No ee-validated identity and no alignment; the bridge prompts again.
    Unverifiable,
}

impl AgentPermissionPreview {
    /// Transcript line for the pending permission request.
    #[must_use]
    pub(crate) fn transcript_text(&self) -> String {
        match self {
            Self::Verified(summary) => format!("ee verified: {summary}"),
            Self::Heuristic { class } => format!(
                "ee check: unverifiable payload · heuristic alignment covers the next matching {}",
                class.map_or("write or terminal operation", AgentGrantClass::label)
            ),
            Self::Unverifiable => {
                String::from("ee check: unverifiable payload · bridge approval will still prompt")
            }
        }
    }

    /// Composer marker appended to the pending choice, when there is one.
    #[must_use]
    pub(crate) fn composer_marker(&self) -> Option<&'static str> {
        match self {
            Self::Verified(_) => Some("· ee verified"),
            Self::Heuristic { .. } => Some("· ee heuristic"),
            Self::Unverifiable => None,
        }
    }

    /// Transcript-export note, when there is one.
    #[must_use]
    pub(crate) fn export_text(&self) -> Option<String> {
        match self {
            Self::Verified(summary) => Some(format!("ee verified: {summary}")),
            Self::Heuristic { .. } => Some(self.transcript_text()),
            Self::Unverifiable => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn verified_summary_reports_validated_facts_only() {
        let write = AgentOperationKey::Write {
            path: PathBuf::from("/w/report.txt"),
            content: String::from("hello"),
        };
        // sha256("hello") = 2cf24dba5fb0a30e…; the summary shows a stable
        // prefix, never the content itself.
        assert_eq!(write.verified_summary(), "write /w/report.txt · 5 bytes · sha256:2cf24dba5fb0");
        assert_eq!(write.verified_summary(), write.verified_summary());

        let terminal = AgentOperationKey::Terminal {
            executable: String::from("cargo"),
            argv: vec![String::from("test"), String::from("--workspace")],
        };
        assert_eq!(terminal.verified_summary(), "terminal cargo test --workspace");

        let spaced = AgentOperationKey::Terminal {
            executable: String::from("echo"),
            argv: vec![String::from("two words")],
        };
        assert_eq!(spaced.verified_summary(), "terminal echo \"two words\"");
    }

    #[test]
    fn verified_summary_escapes_control_characters_and_spaced_paths() {
        // A crafted in-workspace file name must not smuggle control bytes or
        // forge summary fields into the prompt, transcript, or export.
        let control = AgentOperationKey::Write {
            path: PathBuf::from("/w/evil\u{1b}[2J · 01 bytes · sha256:ffffffffffff.txt"),
            content: String::from("x"),
        };
        let summary = control.verified_summary();
        assert!(!summary.contains('\u{1b}'), "{summary:?}");
        assert!(summary.contains("\\u{1b}"), "{summary:?}");

        let spaced = AgentOperationKey::Write {
            path: PathBuf::from("/w/two words.txt"),
            content: String::from("x"),
        };
        assert!(
            spaced.verified_summary().contains("\"/w/two words.txt\""),
            "{}",
            spaced.verified_summary()
        );
    }

    #[test]
    fn preview_texts_mark_verification_and_alignment() {
        let verified =
            AgentPermissionPreview::Verified(String::from("write /w/a.txt · 2 bytes · sha256:00"));
        assert_eq!(verified.transcript_text(), "ee verified: write /w/a.txt · 2 bytes · sha256:00");
        assert_eq!(verified.composer_marker(), Some("· ee verified"));
        assert!(verified.export_text().is_some());

        let heuristic =
            AgentPermissionPreview::Heuristic { class: Some(AgentGrantClass::Terminal) };
        assert!(
            heuristic
                .transcript_text()
                .contains("heuristic alignment covers the next matching terminal"),
            "{heuristic:?}"
        );
        assert_eq!(heuristic.composer_marker(), Some("· ee heuristic"));

        let unverifiable = AgentPermissionPreview::Unverifiable;
        assert!(unverifiable.transcript_text().contains("unverifiable payload"));
        assert_eq!(unverifiable.composer_marker(), None);
        assert!(unverifiable.export_text().is_none());
    }
}
