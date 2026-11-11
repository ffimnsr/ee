//! Transcript assembly order for one turn.
//!
//! `TurnSystemContext` owns every message that precedes the current prompt:
//! the stable system prefix, trusted global instructions, memory facts,
//! retained history, the previously retained runtime snapshot, workspace
//! instructions, and the current runtime snapshot.

use sha2::{Digest, Sha256};

use crate::context_planner::ContextPlan;
use crate::model::{ModelMessage, ModelRole, Transcript};
use crate::prompt::mcp::McpInstructionMessage;
use crate::trust::TrustLevel;

/// Exact prefix of the full runtime-context snapshot message.
pub(crate) const RUNTIME_CONTEXT_HEADER: &str =
    "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.";

/// Exact prefix of a delta runtime-context message.
pub(crate) const RUNTIME_CONTEXT_DELTA_HEADER: &str = concat!(
    "Current runtime context delta. Sections marked unchanged retain the values ",
    "from the retained runtime snapshot above."
);

/// Whether a runtime snapshot is complete or a delta against a retained one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeSnapshotMode {
    /// Complete runtime snapshot.
    #[default]
    Full,
    /// Only changed sections plus the names of unchanged sections.
    Delta,
}

/// One rendered runtime-context section with a stable content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSection {
    /// DSH-compatible slot name.
    pub name: String,
    /// Numeric slot order.
    pub order: i32,
    /// Rendered section body.
    pub text: String,
    /// SHA-256 hex digest of `text`.
    pub hash: String,
}

impl RuntimeSection {
    /// Creates a section and computes its content hash.
    #[must_use]
    pub fn new(name: impl Into<String>, order: i32, text: impl Into<String>) -> Self {
        let text = text.into();
        let hash = section_hash(&text);
        Self { name: name.into(), order, text, hash }
    }
}

/// Host-authored runtime facts delivered as one user-role snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    /// Rendered context text. Full snapshots contain every section; delta
    /// snapshots contain changed sections plus the unchanged-section list.
    pub text: String,
    /// Sections contributing to this snapshot. Delta snapshots contain only
    /// the changed sections.
    pub sections: Vec<RuntimeSection>,
    /// Whether `text` is complete or incremental.
    pub mode: RuntimeSnapshotMode,
    /// Section names omitted from a delta because their values did not change.
    pub unchanged: Vec<String>,
}

impl RuntimeSnapshot {
    /// Appends one rendered block without section identity.
    ///
    /// Used by repair context, which is rebuilt per loop and never emitted as
    /// a delta.
    pub(crate) fn push_block(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if !self.text.trim().is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
    }

    /// Appends one identified section and its rendered text.
    pub(crate) fn push_section(&mut self, section: RuntimeSection) {
        let text = section.text.trim();
        if text.is_empty() {
            return;
        }
        if !self.text.trim().is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
        self.sections.push(section);
    }

    /// Captures the full snapshot state retained for the next turn.
    #[must_use]
    pub(crate) fn full_state(&self) -> RuntimeSnapshotState {
        RuntimeSnapshotState { sections: self.sections.clone(), full_text: self.text.clone() }
    }

    /// Builds a delta against a retained full snapshot.
    ///
    /// Returns `None` when the snapshot has no identified sections or the
    /// retained state has no full text, because a delta without a retained
    /// base would hide unchanged facts from the model.
    #[must_use]
    pub(crate) fn delta_against(&self, previous: &RuntimeSnapshotState) -> Option<Self> {
        if self.sections.is_empty() || previous.sections.is_empty() || previous.full_text.is_empty()
        {
            return None;
        }
        let mut changed = Vec::new();
        let mut unchanged = Vec::new();
        for section in &self.sections {
            match previous.sections.iter().find(|candidate| candidate.name == section.name) {
                Some(before) if before.hash == section.hash => unchanged.push(section.name.clone()),
                _ => changed.push(section.clone()),
            }
        }
        let removed: Vec<String> = previous
            .sections
            .iter()
            .filter(|before| !self.sections.iter().any(|section| section.name == before.name))
            .map(|before| before.name.clone())
            .collect();
        let mut blocks = changed
            .iter()
            .map(|section| format!("{}:\n{}", section.name, section.text.trim()))
            .collect::<Vec<_>>();
        if !removed.is_empty() {
            blocks.push(format!("Removed sections: {}", removed.join(", ")));
        }
        let unchanged_line = if unchanged.is_empty() {
            "Unchanged sections: none".to_string()
        } else {
            format!("Unchanged sections: {}", unchanged.join(", "))
        };
        blocks.push(unchanged_line);
        Some(Self {
            text: blocks.join("\n\n"),
            sections: changed,
            mode: RuntimeSnapshotMode::Delta,
            unchanged,
        })
    }
}

/// Full runtime snapshot retained per session for delta projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSnapshotState {
    /// Every section of the retained full snapshot.
    pub sections: Vec<RuntimeSection>,
    /// Complete rendered text delivered to the model.
    pub full_text: String,
}

/// One workspace instruction message with its source path.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceMessage {
    /// Source path used for precedence and dedupe diagnostics.
    pub path: String,
    /// Pre-labeled message. Phase 3 supplies the workspace trust label.
    pub message: ModelMessage,
}

/// Messages and context prepended before one model/tool loop.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnSystemContext {
    /// Prior normalized user, assistant, and tool messages from this session.
    pub history: Vec<ModelMessage>,
    /// Compact memory facts from the runtime store.
    pub memory: Option<String>,
    /// Fresh task context. Repository and tool content retain their untrusted
    /// transcript role; this must never be flattened into system context.
    pub task_context: Option<ContextPlan>,
    /// Stable system prompt. Rendered first so it stays a cacheable prefix.
    pub stable: Option<String>,
    /// Trusted user-level instructions delivered after the stable prefix.
    pub global: Vec<WorkspaceMessage>,
    /// Previous full runtime snapshot retained for delta mode.
    pub retained_runtime: Option<String>,
    /// Runtime context snapshot delivered after retained history.
    pub runtime: Option<RuntimeSnapshot>,
    /// Workspace instruction messages delivered after retained history.
    pub workspace: Vec<WorkspaceMessage>,
    /// MCP server instruction messages delivered after the runtime snapshot.
    pub mcp: Vec<McpInstructionMessage>,
}

impl TurnSystemContext {
    /// Applies the documented transcript order.
    ///
    /// `prompt_messages` is the current prompt only. Retained history lives in
    /// [`TurnSystemContext::history`], so callers cannot interleave runtime
    /// facts after the current user prompt.
    pub(crate) fn apply_to_transcript(
        self,
        transcript: &mut Transcript,
        prompt_messages: Vec<ModelMessage>,
    ) {
        let mut messages = Vec::new();
        if let Some(stable) = self.stable.filter(|text| !text.trim().is_empty()) {
            messages.push(ModelMessage::text(ModelRole::System, stable));
        }
        messages.extend(self.global.into_iter().map(|instruction| instruction.message));
        if let Some(facts) = self.memory.filter(|text| !text.trim().is_empty()) {
            messages.push(ModelMessage::text(ModelRole::System, format!("Memory facts:\n{facts}")));
        }
        messages.extend(self.history.into_iter().filter(|message| !is_refreshable_prompt(message)));
        if let Some(retained) = self.retained_runtime.filter(|text| !text.trim().is_empty()) {
            messages.push(retained_runtime_message(&retained));
        }
        messages.extend(self.workspace.into_iter().map(|workspace| workspace.message));
        if let Some(runtime) = self.runtime.filter(|snapshot| !snapshot.text.trim().is_empty()) {
            messages.push(runtime_message(&runtime));
        }
        messages.extend(self.mcp.into_iter().map(|instruction| instruction.message));
        messages.extend(prompt_messages);
        transcript.messages = messages;
        if let Some(task_context) = self.task_context {
            task_context.apply_to_transcript(transcript);
        }
    }
}

/// Prompt-plane messages that the current turn rebuilds from live state.
///
/// A resumed transcript can contain an earlier workspace, global, MCP, or
/// runtime snapshot. Keeping it would duplicate stale facts after the fresh
/// planes are appended, so those are dropped before the current snapshot is
/// inserted. The retained runtime snapshot uses a distinct marker and is
/// intentionally preserved as the delta base.
fn is_refreshable_prompt(message: &ModelMessage) -> bool {
    matches!(
        message.metadata.get("ee_prompt").map(String::as_str),
        Some(
            "runtime_context"
                | "workspace_instructions"
                | "global_instructions"
                | "mcp_server_instructions"
        )
    )
}

/// Builds a full runtime snapshot as host policy, not user input.
fn runtime_message(snapshot: &RuntimeSnapshot) -> ModelMessage {
    let header = match snapshot.mode {
        RuntimeSnapshotMode::Full => RUNTIME_CONTEXT_HEADER,
        RuntimeSnapshotMode::Delta => RUNTIME_CONTEXT_DELTA_HEADER,
    };
    ModelMessage::text(ModelRole::User, format!("{header}\n\n{}", snapshot.text))
        .with_trust(TrustLevel::SystemPolicy)
        .with_metadata([("ee_prompt".to_string(), "runtime_context".to_string())])
}

/// Preserves the previous full snapshot as the delta base.
fn retained_runtime_message(text: &str) -> ModelMessage {
    ModelMessage::text(ModelRole::User, format!("{RUNTIME_CONTEXT_HEADER}\n\n{text}"))
        .with_trust(TrustLevel::SystemPolicy)
        .with_metadata([("ee_prompt".to_string(), "runtime_context_retained".to_string())])
}

fn section_hash(text: &str) -> String {
    let digest = Sha256::digest(text.trim().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Transcript;

    fn text(message: &ModelMessage) -> String {
        message.text_content()
    }

    fn section(name: &str, order: i32, text: &str) -> RuntimeSection {
        RuntimeSection::new(name, order, text)
    }

    #[test]
    fn stable_global_memory_history_prompt_order() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            history: vec![ModelMessage::text(ModelRole::User, "old")],
            memory: Some("facts".into()),
            stable: Some("stable".into()),
            global: vec![WorkspaceMessage {
                path: "(global) AGENTS.md".into(),
                message: ModelMessage::text(ModelRole::User, "global")
                    .with_trust(TrustLevel::UserInstructionTrusted),
            }],
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let roles: Vec<_> = transcript.messages.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            vec![
                ModelRole::System,
                ModelRole::User,
                ModelRole::System,
                ModelRole::User,
                ModelRole::User
            ]
        );
        assert_eq!(text(&transcript.messages[0]), "stable");
        assert_eq!(text(&transcript.messages[1]), "global");
        assert_eq!(transcript.messages[1].trust, TrustLevel::UserInstructionTrusted);
        assert_eq!(text(&transcript.messages[2]), "Memory facts:\nfacts");
        assert_eq!(text(&transcript.messages[3]), "old");
        assert_eq!(text(&transcript.messages[4]), "current");
    }

    #[test]
    fn workspace_and_runtime_precede_the_prompt() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            history: vec![ModelMessage::text(ModelRole::Assistant, "done")],
            workspace: vec![WorkspaceMessage {
                path: "AGENTS.md".into(),
                message: ModelMessage::text(ModelRole::User, "workspace"),
            }],
            runtime: Some(RuntimeSnapshot { text: "cwd".into(), ..RuntimeSnapshot::default() }),
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let texts: Vec<_> = transcript.messages.iter().map(text).collect();
        assert_eq!(
            texts,
            vec![
                "done".to_string(),
                "workspace".to_string(),
                format!("{RUNTIME_CONTEXT_HEADER}\n\ncwd"),
                "current".to_string(),
            ]
        );
    }

    #[test]
    fn retained_runtime_precedes_workspace_and_delta() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            retained_runtime: Some("old full".into()),
            workspace: vec![WorkspaceMessage {
                path: "AGENTS.md".into(),
                message: ModelMessage::text(ModelRole::User, "workspace"),
            }],
            runtime: Some(RuntimeSnapshot {
                text: "WORKING_DIRECTORY:\n/new".into(),
                sections: vec![section("WORKING_DIRECTORY", 100, "new")],
                mode: RuntimeSnapshotMode::Delta,
                unchanged: vec!["APPROVAL_POLICY".into()],
            }),
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let texts: Vec<_> = transcript.messages.iter().map(text).collect();
        assert_eq!(
            texts,
            vec![
                format!("{RUNTIME_CONTEXT_HEADER}\n\nold full"),
                "workspace".to_string(),
                format!("{RUNTIME_CONTEXT_DELTA_HEADER}\n\nWORKING_DIRECTORY:\n/new"),
                "current".to_string(),
            ]
        );
        assert_eq!(
            transcript.messages[2].metadata.get("ee_prompt").map(String::as_str),
            Some("runtime_context")
        );
        assert_eq!(
            transcript.messages[0].metadata.get("ee_prompt").map(String::as_str),
            Some("runtime_context_retained"),
            "retained base survives history refresh"
        );
    }

    #[test]
    fn mcp_instructions_follow_the_runtime_snapshot() {
        use crate::prompt::mcp::McpInstructionMessage;
        use crate::trust::TrustLevel;

        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            runtime: Some(RuntimeSnapshot { text: "cwd".into(), ..RuntimeSnapshot::default() }),
            mcp: vec![McpInstructionMessage {
                server: "docs".into(),
                message: ModelMessage::text(ModelRole::User, "server instructions")
                    .with_trust(TrustLevel::McpServerUntrusted)
                    .with_metadata([("ee_prompt".into(), "mcp_server_instructions".into())]),
            }],
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let texts: Vec<_> = transcript.messages.iter().map(text).collect();
        assert_eq!(
            texts,
            vec![
                format!("{RUNTIME_CONTEXT_HEADER}\n\ncwd"),
                "server instructions".to_string(),
                "current".to_string(),
            ]
        );
        assert_eq!(transcript.messages[1].trust, TrustLevel::McpServerUntrusted);
    }

    #[test]
    fn stale_mcp_instructions_are_replaced_by_the_fresh_snapshot() {
        use crate::prompt::mcp::McpInstructionMessage;

        let stale = ModelMessage::text(ModelRole::User, "old server instructions")
            .with_metadata([("ee_prompt".into(), "mcp_server_instructions".into())]);
        let context = TurnSystemContext {
            history: vec![stale],
            mcp: vec![McpInstructionMessage {
                server: "docs".into(),
                message: ModelMessage::text(ModelRole::User, "new server instructions"),
            }],
            ..TurnSystemContext::default()
        };
        let mut transcript = Transcript::new();
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );
        let texts: Vec<_> = transcript.messages.iter().map(text).collect();
        assert_eq!(texts, vec!["new server instructions".to_string(), "current".to_string()]);
    }

    #[test]
    fn runtime_snapshot_is_system_policy_not_user_prompt() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            runtime: Some(RuntimeSnapshot { text: "cwd".into(), ..RuntimeSnapshot::default() }),
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let runtime = &transcript.messages[0];
        assert_eq!(runtime.role, ModelRole::User);
        assert_eq!(runtime.trust, TrustLevel::SystemPolicy);
        assert_eq!(runtime.metadata.get("ee_prompt").map(String::as_str), Some("runtime_context"));
        assert!(runtime.text_content().starts_with(RUNTIME_CONTEXT_HEADER));
    }

    #[test]
    fn stale_prompt_planes_are_replaced_by_the_fresh_snapshot() {
        let mut transcript = Transcript::new();
        let stale_workspace = ModelMessage::text(ModelRole::User, "old workspace")
            .with_trust(TrustLevel::WorkspaceInstructionUntrusted)
            .with_metadata([("ee_prompt".into(), "workspace_instructions".into())]);
        let stale_global = ModelMessage::text(ModelRole::User, "old global")
            .with_trust(TrustLevel::UserInstructionTrusted)
            .with_metadata([("ee_prompt".into(), "global_instructions".into())]);
        let stale_runtime = ModelMessage::text(ModelRole::User, "old cwd")
            .with_metadata([("ee_prompt".into(), "runtime_context".into())]);
        let repeat = ModelMessage::text(ModelRole::User, "repeat reminder")
            .with_metadata([("ee_prompt".into(), "repeat_reminder".into())]);
        let context = TurnSystemContext {
            history: vec![stale_global, stale_workspace, stale_runtime, repeat],
            global: vec![WorkspaceMessage {
                path: "(global) AGENTS.md".into(),
                message: ModelMessage::text(ModelRole::User, "new global")
                    .with_trust(TrustLevel::UserInstructionTrusted),
            }],
            workspace: vec![WorkspaceMessage {
                path: "AGENTS.md".into(),
                message: ModelMessage::text(ModelRole::User, "new workspace")
                    .with_trust(TrustLevel::WorkspaceInstructionUntrusted),
            }],
            runtime: Some(RuntimeSnapshot { text: "new cwd".into(), ..RuntimeSnapshot::default() }),
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let texts: Vec<_> = transcript.messages.iter().map(text).collect();
        assert_eq!(
            texts,
            vec![
                "new global".to_string(),
                "repeat reminder".to_string(),
                "new workspace".to_string(),
                format!("{RUNTIME_CONTEXT_HEADER}\n\nnew cwd"),
                "current".to_string(),
            ]
        );
    }

    #[test]
    fn empty_optional_context_is_dropped() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            stable: Some("  ".into()),
            memory: Some("\n".into()),
            retained_runtime: Some(" ".into()),
            runtime: Some(RuntimeSnapshot { text: " ".into(), ..RuntimeSnapshot::default() }),
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(text(&transcript.messages[0]), "current");
    }

    #[test]
    fn push_block_preserves_blank_line_join() {
        let mut snapshot = RuntimeSnapshot::default();
        snapshot.push_block("first");
        snapshot.push_block("  ");
        snapshot.push_block("second");
        assert_eq!(snapshot.text, "first\n\nsecond");
    }

    #[test]
    fn first_full_snapshot_and_unchanged_delta() {
        let full = RuntimeSnapshot {
            text: "cwd\n\nsandbox".into(),
            sections: vec![
                section("WORKING_DIRECTORY", 100, "cwd"),
                section("SANDBOX_POLICY", 110, "sandbox"),
            ],
            ..RuntimeSnapshot::default()
        };
        let state = full.full_state();
        let delta = full.delta_against(&state).expect("delta");
        assert_eq!(delta.mode, RuntimeSnapshotMode::Delta);
        assert!(delta.sections.is_empty(), "no changed sections");
        assert_eq!(delta.text, "Unchanged sections: WORKING_DIRECTORY, SANDBOX_POLICY");
        assert_eq!(delta.unchanged, vec!["WORKING_DIRECTORY", "SANDBOX_POLICY"]);
    }

    #[test]
    fn changed_cwd_emits_only_that_section() {
        let previous = RuntimeSnapshot {
            text: "old cwd\n\nsandbox".into(),
            sections: vec![
                section("WORKING_DIRECTORY", 100, "old cwd"),
                section("SANDBOX_POLICY", 110, "sandbox"),
            ],
            ..RuntimeSnapshot::default()
        }
        .full_state();
        let current = RuntimeSnapshot {
            text: "new cwd\n\nsandbox".into(),
            sections: vec![
                section("WORKING_DIRECTORY", 100, "new cwd"),
                section("SANDBOX_POLICY", 110, "sandbox"),
            ],
            ..RuntimeSnapshot::default()
        };
        let delta = current.delta_against(&previous).expect("delta");
        assert_eq!(delta.sections.len(), 1);
        assert_eq!(delta.sections[0].name, "WORKING_DIRECTORY");
        assert!(!delta.text.contains("sandbox"), "{}", delta.text);
        assert!(delta.text.contains("WORKING_DIRECTORY:\nnew cwd"));
        assert!(delta.text.contains("Unchanged sections: SANDBOX_POLICY"));
    }

    #[test]
    fn delta_requires_retained_state() {
        let current = RuntimeSnapshot {
            text: "cwd".into(),
            sections: vec![section("WORKING_DIRECTORY", 100, "cwd")],
            ..RuntimeSnapshot::default()
        };
        assert!(current.delta_against(&RuntimeSnapshotState::default()).is_none());
    }
}
