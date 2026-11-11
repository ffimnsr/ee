//! Transcript assembly order for one turn.
//!
//! `TurnSystemContext` owns every message that precedes the current prompt:
//! the stable system prefix, memory facts, retained history, workspace
//! instructions, and the runtime snapshot.

use crate::context_planner::ContextPlan;
use crate::model::{ModelMessage, ModelRole, Transcript};
use crate::trust::TrustLevel;

/// Exact prefix of the runtime-context snapshot message.
pub(crate) const RUNTIME_CONTEXT_HEADER: &str =
    "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.";

/// Host-authored runtime facts delivered as one user-role snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    /// Rendered context sections, already joined with blank lines.
    pub text: String,
}

impl RuntimeSnapshot {
    /// Appends one rendered block, preserving the plane's blank-line join.
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
    /// Runtime context snapshot delivered after retained history.
    pub runtime: Option<RuntimeSnapshot>,
    /// Workspace instruction messages delivered after retained history.
    pub workspace: Vec<WorkspaceMessage>,
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
        if let Some(facts) = self.memory.filter(|text| !text.trim().is_empty()) {
            messages.push(ModelMessage::text(ModelRole::System, format!("Memory facts:\n{facts}")));
        }
        messages.extend(self.history);
        messages.extend(self.workspace.into_iter().map(|workspace| workspace.message));
        if let Some(runtime) = self.runtime.filter(|snapshot| !snapshot.text.trim().is_empty()) {
            messages.push(runtime_message(&runtime.text));
        }
        messages.extend(prompt_messages);
        transcript.messages = messages;
        if let Some(task_context) = self.task_context {
            task_context.apply_to_transcript(transcript);
        }
    }
}

/// Builds the runtime snapshot as host policy, not user input.
fn runtime_message(text: &str) -> ModelMessage {
    ModelMessage::text(ModelRole::User, format!("{RUNTIME_CONTEXT_HEADER}\n\n{text}"))
        .with_trust(TrustLevel::SystemPolicy)
        .with_metadata([("ee_prompt".to_string(), "runtime_context".to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Transcript;

    fn text(message: &ModelMessage) -> String {
        message.text_content()
    }

    #[test]
    fn stable_memory_history_prompt_order() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            history: vec![ModelMessage::text(ModelRole::User, "old")],
            memory: Some("facts".into()),
            stable: Some("stable".into()),
            ..TurnSystemContext::default()
        };
        context.apply_to_transcript(
            &mut transcript,
            vec![ModelMessage::text(ModelRole::User, "current")],
        );

        let roles: Vec<_> = transcript.messages.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            vec![ModelRole::System, ModelRole::System, ModelRole::User, ModelRole::User]
        );
        assert_eq!(text(&transcript.messages[0]), "stable");
        assert_eq!(text(&transcript.messages[1]), "Memory facts:\nfacts");
        assert_eq!(text(&transcript.messages[2]), "old");
        assert_eq!(text(&transcript.messages[3]), "current");
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
            runtime: Some(RuntimeSnapshot { text: "cwd".into() }),
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
    fn runtime_snapshot_is_system_policy_not_user_prompt() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            runtime: Some(RuntimeSnapshot { text: "cwd".into() }),
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
    fn empty_optional_context_is_dropped() {
        let mut transcript = Transcript::new();
        let context = TurnSystemContext {
            stable: Some("  ".into()),
            memory: Some("\n".into()),
            runtime: Some(RuntimeSnapshot { text: " ".into() }),
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
}
