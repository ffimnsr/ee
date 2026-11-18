//! Normalized message roles, content blocks, and message builders.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::compaction_checkpoint::CompactionCheckpoint;
use crate::opaque_state::OpaqueProviderState;
use crate::tools::ToolResult;
use crate::trust::{TrustLevel, trust_for_role};

/// Cap on diagnostic metadata entries; keeps provider metadata bounded.
pub const MAX_METADATA_ENTRIES: usize = 16;

/// Tool-result characters above which the middle is pruned for the request.
pub const TOOL_RESULT_PRUNE_THRESHOLD: usize = 8192;
/// Characters kept from the start of an oversized tool result.
pub const TOOL_RESULT_PRUNE_HEAD: usize = 4096;
/// Characters kept from the end of an oversized tool result.
pub const TOOL_RESULT_PRUNE_TAIL: usize = 1024;
/// Exact marker substituted for the pruned middle span.
pub const TOOL_RESULT_PRUNE_MARKER: &str = "\n\n[... tool result middle pruned ...]\n\n";

/// Message metadata key: the tool name that produced a result.
pub const TOOL_NAME_METADATA_KEY: &str = "tool_name";

/// Metadata key prefix marking an assistant tool call as provider-executed.
///
/// The suffix is the tool-call id, so one assistant message can carry several
/// provider-executed calls without a delimiter scheme.
pub const PROVIDER_EXECUTED_METADATA_PREFIX: &str = "provider_executed:";

/// Metadata key marking a tool-result message as provider-produced.
pub const PROVIDER_EXECUTED_RESULT_KEY: &str = "provider_executed";
/// Message metadata key: the tool's side-effect class label.
pub const TOOL_SIDE_EFFECT_METADATA_KEY: &str = "tool_side_effect";
/// Message metadata key: the primary path argument of a read/write tool.
pub const TOOL_PATH_METADATA_KEY: &str = "tool_path";
/// Message metadata key marking a cacheable prompt-prefix boundary.
///
/// Providers that support explicit prompt caching (Anthropic `cache_control`,
/// OpenAI `prompt_cache_key`) read this marker instead of guessing where the
/// stable prefix ends. The marker never changes message text.
pub const CACHE_BREAKPOINT_METADATA_KEY: &str = "ee_cache_breakpoint";
/// Metadata value stored under [`CACHE_BREAKPOINT_METADATA_KEY`].
pub const CACHE_BREAKPOINT_METADATA_VALUE: &str = "true";

/// Role of one normalized transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelRole {
    /// System-level instructions.
    System,
    /// User (or delegated client) content.
    User,
    /// Assistant text and reasoning.
    Assistant,
    /// A tool observation appended after execution.
    Tool,
    /// A subagent summary (subagent phase).
    Subagent,
}

/// One content block inside a normalized [`ModelMessage`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ModelContent {
    /// Plain text.
    Text(String),
    /// A tool execution result, correlated by the stable tool-call id.
    ToolResult {
        /// The tool-call id used by the model and by update correlation.
        tool_call_id: String,
        /// The normalized execution outcome.
        result: ToolResult,
    },
    /// A tool call the model requested.
    ///
    /// Assistant tool-call turns are kept so the next request replays the call
    /// the observations answer; providers reject an orphan tool result, and
    /// signed reasoning is only meaningful alongside its own call.
    ToolCall {
        /// The tool-call id the matching result carries.
        tool_call_id: String,
        /// The function name the model requested.
        name: String,
        /// Parsed argument object.
        arguments: serde_json::Value,
    },
    /// A reference to a workspace file.
    FileReference { path: String },
    /// A reference to a terminal session (later phases).
    TerminalReference { terminal_id: String },
    /// An image provided by the client, base64-encoded.
    Image {
        /// Base64-encoded image payload.
        data: String,
        /// MIME type describing the encoded payload.
        mime_type: String,
        /// Optional source URI reported by the client.
        uri: Option<String>,
    },
    /// Audio provided by the client, base64-encoded.
    Audio {
        /// Base64-encoded audio payload.
        data: String,
        /// MIME type describing the encoded payload.
        mime_type: String,
    },
    /// A resource embedded in the prompt, either text or binary.
    EmbeddedResource {
        /// URI identifying the resource.
        uri: String,
        /// MIME type when the client reported one.
        mime_type: Option<String>,
        /// Text payload for a text resource.
        text: Option<String>,
        /// Base64-encoded payload for a binary resource.
        blob: Option<String>,
    },
    /// Opaque provider compaction checkpoint, replayed verbatim.
    ///
    /// Only the codec that produced it understands the payload; every other
    /// codec ignores the part, so a checkpoint never reaches a text prompt or a
    /// different provider.
    CompactionCheckpoint(CompactionCheckpoint),
}

/// One normalized transcript message.
///
/// Non-exhaustive so later phases can add fields without breaking adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelMessage {
    /// Who produced the message.
    pub role: ModelRole,
    /// The message content blocks.
    pub content: Vec<ModelContent>,
    /// Optional condensed reasoning summary kept separate from the text.
    pub reasoning_summary: Option<String>,
    /// Opaque provider reasoning state replayed only to the producing model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<OpaqueProviderState>,
    /// Bounded diagnostic metadata; never carries required fields.  Provider
    /// metadata is flattened into at most [`MAX_METADATA_ENTRIES`] string
    /// entries so the transcript stays deterministic and bounded.
    pub metadata: BTreeMap<String, String>,
    /// Trust level of the content: tool output and subagent summaries are
    /// untrusted and may never override policy (see [`crate::prompt_injection`]).
    pub trust: TrustLevel,
}

impl ModelMessage {
    /// Creates an empty message with the given role; trust defaults from the
    /// role (system → system policy, tool → untrusted tool output, ...).
    #[must_use]
    pub fn new(role: ModelRole) -> Self {
        Self {
            role,
            content: Vec::new(),
            reasoning_summary: None,
            native: None,
            metadata: BTreeMap::new(),
            trust: trust_for_role(role),
        }
    }

    /// Creates a text-only message with the given role.
    #[must_use]
    pub fn text(role: ModelRole, text: impl Into<String>) -> Self {
        Self::new(role).with_content(vec![ModelContent::Text(text.into())])
    }

    /// Creates a tool-observation message from a tool result, keeping the
    /// stable tool-call id for correlation.
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, result: ToolResult) -> Self {
        Self::new(ModelRole::Tool).with_content(vec![ModelContent::ToolResult {
            tool_call_id: tool_call_id.into(),
            result,
        }])
    }

    /// Creates an assistant message replaying one tool call.
    #[must_use]
    pub fn tool_call(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::new(ModelRole::Assistant).with_content(vec![ModelContent::ToolCall {
            tool_call_id: tool_call_id.into(),
            name: name.into(),
            arguments,
        }])
    }

    /// Replaces the message content.
    #[must_use]
    pub fn with_content(mut self, content: Vec<ModelContent>) -> Self {
        self.content = content;
        self
    }

    /// Sets the optional reasoning summary.
    /// Sets the reasoning summary.
    #[must_use]
    pub fn with_reasoning_summary(mut self, summary: impl Into<String>) -> Self {
        self.reasoning_summary = Some(summary.into());
        self
    }

    /// Sets opaque provider reasoning state.
    #[must_use]
    pub fn with_native(mut self, native: Option<OpaqueProviderState>) -> Self {
        self.native = native;
        self
    }

    /// Sets diagnostic metadata entries, keeping at most
    /// [`MAX_METADATA_ENTRIES`] (earlier entries win).
    #[must_use]
    pub fn with_metadata(mut self, entries: impl IntoIterator<Item = (String, String)>) -> Self {
        for (key, value) in entries.into_iter().take(MAX_METADATA_ENTRIES) {
            self.metadata.insert(key, value);
        }
        self
    }

    /// Overrides the trust level of the message.
    #[must_use]
    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Marks this message as the end of a cacheable prompt-prefix plane.
    ///
    /// The marker is metadata only: message text is never rewritten, and a
    /// provider without prompt caching simply ignores it.
    #[must_use]
    pub fn with_cache_breakpoint(mut self) -> Self {
        self.metadata.insert(
            CACHE_BREAKPOINT_METADATA_KEY.to_string(),
            CACHE_BREAKPOINT_METADATA_VALUE.to_string(),
        );
        self
    }

    /// Whether this message closes a cacheable prompt-prefix plane.
    #[must_use]
    pub fn is_cache_breakpoint(&self) -> bool {
        self.metadata.get(CACHE_BREAKPOINT_METADATA_KEY).map(String::as_str)
            == Some(CACHE_BREAKPOINT_METADATA_VALUE)
    }

    /// Concatenated text content of this message (empty when the message has
    /// no text blocks).
    #[must_use]
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ModelContent::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl ModelMessage {
    /// Returns a request copy with an oversized tool-result middle pruned.
    ///
    /// Only `text_output` is rewritten; structured output, arguments, and
    /// assistant text are untouched. Returns `None` when nothing changes.
    #[must_use]
    pub(crate) fn with_pruned_tool_result(&self) -> Option<Self> {
        let mut message = self.clone();
        let mut changed = false;
        for block in &mut message.content {
            if let ModelContent::ToolResult { result, .. } = block
                && let Some(pruned) = prune_tool_result_text(&result.text_output)
            {
                result.text_output = pruned;
                changed = true;
            }
        }
        changed.then_some(message)
    }
}

/// Prunes the middle of one oversized tool-result string by characters.
#[must_use]
pub(crate) fn prune_tool_result_text(text: &str) -> Option<String> {
    if text.chars().count() <= TOOL_RESULT_PRUNE_THRESHOLD {
        return None;
    }
    let head: String = text.chars().take(TOOL_RESULT_PRUNE_HEAD).collect();
    let tail: String =
        text.chars().rev().take(TOOL_RESULT_PRUNE_TAIL).collect::<String>().chars().rev().collect();
    Some(format!("{head}{TOOL_RESULT_PRUNE_MARKER}{tail}"))
}
