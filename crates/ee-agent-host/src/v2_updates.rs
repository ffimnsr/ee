//! Host-side v2 → internal update translation.
//!
//! The reducer, session threads, and UI all consume v1-typed
//! [`SessionUpdate`](ee_agent_protocol::SessionUpdate) values.  On a v2 connection the inbound
//! `session/update` notifications carry the v2 surface; this module maps the
//! shared variants back to the v1-typed shape so the entire host state
//! machinery stays version-agnostic.
//!
//! Mapping rules:
//!
//! - message chunks carry over unchanged (both wire shapes use the same
//!   fields); whole-message upserts (`user_message`, `agent_message`,
//!   `agent_thought`) replace or clear by `messageId` directly through
//!   `apply_v2_update` — the v1 wire has no whole-message or clear form;
//! - `tool_call_update` maps field-for-field (v2 `name`); structured v2
//!   diffs have no mechanical v1 oldText/newText mapping and are dropped per
//!   item; v2 terminal references map to the v1 reference (display state is
//!   v2-only and dropped);
//! - `plan_update` items become the v1 plan replacement;
//! - `available_commands_update` inputs (tagged) become the v1 untagged
//!   input shape;
//! - config option updates rename `configId`/`groupId` back to the v1 keys;
//! - v2-only variants (`state_update`, terminal updates, tool-call content
//!   chunks, unknown extensions) are handled by the caller or dropped.
//!
//! `state_update` never reaches this module: idle resolves the pending turn's
//! stop reason (see `connection.rs`), running/requires_action are pure
//! foreground signals the v1 reducers cannot represent.

use ee_agent_protocol::v2;
use ee_agent_protocol::{
    AvailableCommand as V1AvailableCommand, AvailableCommandInput as V1CommandInput,
    ContentBlock as V1ContentBlock, SessionUpdate as V1SessionUpdate,
    ToolCallUpdate as V1ToolCallUpdate, ToolCallUpdateFields, UnstructuredCommandInput,
};

/// Translates one v2 session update into the v1-typed updates the host state
/// reduces.  Returns a vector because some v2 variants expand into several v1
/// updates and others have no v1 representation at all (empty result — the
/// caller decides whether that is expected).
///
/// Whole-message upserts are handled by `apply_v2_update` (replace/clear
/// semantics), never here: flattening them to chunks would append where v2
/// mandates replacement.
#[must_use]
pub fn translate_v2_update(update: &v2::SessionUpdate) -> Vec<V1SessionUpdate> {
    match update {
        v2::SessionUpdate::UserMessageChunk(chunk) => {
            chunk_to_v1(chunk, V1SessionUpdate::UserMessageChunk).into_iter().collect()
        }
        v2::SessionUpdate::AgentMessageChunk(chunk) => {
            chunk_to_v1(chunk, V1SessionUpdate::AgentMessageChunk).into_iter().collect()
        }
        v2::SessionUpdate::AgentThoughtChunk(chunk) => {
            chunk_to_v1(chunk, V1SessionUpdate::AgentThoughtChunk).into_iter().collect()
        }
        v2::SessionUpdate::ToolCallUpdate(tool_call) => {
            translate_tool_call_update(tool_call).into_iter().collect()
        }
        v2::SessionUpdate::PlanUpdate(plan) => translate_plan(plan).into_iter().collect(),
        v2::SessionUpdate::AvailableCommandsUpdate(commands) => {
            vec![translate_available_commands(commands)]
        }
        v2::SessionUpdate::ConfigOptionUpdate(options) => {
            translate_config_options(options).into_iter().collect()
        }
        v2::SessionUpdate::SessionInfoUpdate(info) => serde_json::to_value(info)
            .ok()
            .and_then(value_to_v1::<ee_agent_protocol::SessionInfoUpdate>)
            .map(V1SessionUpdate::SessionInfoUpdate)
            .into_iter()
            .collect(),
        v2::SessionUpdate::UsageUpdate(usage) => serde_json::to_value(usage)
            .ok()
            .and_then(value_to_v1::<ee_agent_protocol::UsageUpdate>)
            .map(V1SessionUpdate::UsageUpdate)
            .into_iter()
            .collect(),
        // Whole messages never flatten to chunks: v2 patch semantics say a
        // whole message REPLACES the content stored for its messageId (and
        // `null`/`[]` clears it), which the v1 chunk pipeline cannot carry.
        // [`apply_v2_update`] routes them straight into the reducer.
        v2::SessionUpdate::UserMessage(_)
        | v2::SessionUpdate::AgentMessage(_)
        | v2::SessionUpdate::AgentThought(_) => Vec::new(),
        // Terminal output: the v2 terminal surface has no v1 reducer form.
        // The sanitized-transcript fallback renders decoded output lines as
        // same-id agent chunks under a per-terminal message id, so agent-owned
        // terminal activity stays visible without a terminal pane.
        v2::SessionUpdate::TerminalOutputChunk(chunk) => {
            terminal_output_to_v1_chunk(chunk).into_iter().collect()
        }
        // v2-only surfaces without a v1 reducer form.
        v2::SessionUpdate::StateUpdate(_)
        | v2::SessionUpdate::ToolCallContentChunk(_)
        | v2::SessionUpdate::TerminalUpdate(_)
        | v2::SessionUpdate::Other(_)
        | _ => {
            tracing::debug!("dropping v2-only session update variant");
            Vec::new()
        }
    }
}

/// One whole-message upsert carried on a v2 `session/update`, with v2 patch
/// semantics: concrete `content` replaces the message body, `null`/`[]`
/// clears it.
pub enum WholeMessage<'a> {
    /// `user_message` upsert (the host renders its own submitted prompt
    /// optimistically and does not retain this echo).
    User {
        /// The agent-owned message id.
        message_id: &'a v2::MessageId,
        /// The full content array, or `None` for a clear.
        blocks: Option<&'a [v2::ContentBlock]>,
    },
    /// `agent_message` upsert.
    Agent {
        /// The agent-owned message id.
        message_id: &'a v2::MessageId,
        /// The full content array, or `None` for a clear.
        blocks: Option<&'a [v2::ContentBlock]>,
    },
    /// `agent_thought` upsert.
    Thought {
        /// The agent-owned message id.
        message_id: &'a v2::MessageId,
        /// The full content array, or `None` for a clear.
        blocks: Option<&'a [v2::ContentBlock]>,
    },
}

/// Borrows one whole-message upsert from a v2 session update, when it is one.
#[must_use]
pub fn whole_message_of(update: &v2::SessionUpdate) -> Option<WholeMessage<'_>> {
    match update {
        v2::SessionUpdate::UserMessage(message) => Some(WholeMessage::User {
            message_id: &message.message_id,
            blocks: message.content.value().map(Vec::as_slice),
        }),
        v2::SessionUpdate::AgentMessage(message) => Some(WholeMessage::Agent {
            message_id: &message.message_id,
            blocks: message.content.value().map(Vec::as_slice),
        }),
        v2::SessionUpdate::AgentThought(message) => Some(WholeMessage::Thought {
            message_id: &message.message_id,
            blocks: message.content.value().map(Vec::as_slice),
        }),
        _ => None,
    }
}

/// Applies one v2 `session/update` to the thread with v2 semantics: whole
/// messages replace or clear by `messageId` (their blocks still stream as
/// same-id chunk events so v1 transcript consumers keep rendering), every
/// other representable variant translates to the v1-typed updates the reducer
/// handles.
pub(crate) fn apply_v2_update(thread: &crate::session::ThreadShared, update: &v2::SessionUpdate) {
    use crate::reducer::MessageKind;
    match whole_message_of(update) {
        // The host renders its submitted prompt optimistically; the v2
        // `user_message` upsert is that echo (an ack, not new state).
        Some(WholeMessage::User { .. }) => {}
        Some(WholeMessage::Agent { message_id, blocks }) => thread.apply_whole_message(
            MessageKind::Assistant,
            &message_id.0,
            blocks.map(|blocks| blocks.iter().map(translate_block_v1).collect()),
        ),
        Some(WholeMessage::Thought { message_id, blocks }) => thread.apply_whole_message(
            MessageKind::Thought,
            &message_id.0,
            blocks.map(|blocks| blocks.iter().map(translate_block_v1).collect()),
        ),
        None => {
            for update in translate_v2_update(update) {
                thread.apply_update(update);
            }
        }
    }
}

/// Maps a v2 tool status onto the v1-typed value.  v1 has no `cancelled`
/// status; the closest honest fallback is `failed` (the tool did not
/// complete).  Unknown future values default to `pending`.
fn translate_tool_status_v1(status: v2::ToolCallStatus) -> ee_agent_protocol::ToolCallStatus {
    if matches!(status, v2::ToolCallStatus::Cancelled) {
        return ee_agent_protocol::ToolCallStatus::Failed;
    }
    serde_json::from_value(serde_json::to_value(status).expect("v2 status serializes"))
        .unwrap_or_default()
}

/// One v2 terminal output chunk → a sanitized same-id agent chunk under a
/// per-terminal message id.  Binary bytes beyond printable text are stripped;
/// decoded chunks are capped so one buffer never floods the transcript.
fn terminal_output_to_v1_chunk(chunk: &v2::TerminalOutputChunk) -> Option<V1SessionUpdate> {
    use base64::Engine as _;
    let bytes =
        base64::engine::general_purpose::STANDARD.decode(chunk.data.as_bytes()).unwrap_or_default();
    let mut text = String::with_capacity(bytes.len());
    for byte in bytes {
        // Keep printable text plus newline/tab; strip ANSI/control noise so
        // the transcript stays readable and clean.
        if byte == b'\n' || byte == b'\t' || (0x20..=0x7e).contains(&byte) {
            text.push(byte as char);
        }
    }
    if text.trim().is_empty() {
        return None;
    }
    const MAX_TERMINAL_CHUNK_CHARS: usize = 4 * 1024;
    text.truncate(MAX_TERMINAL_CHUNK_CHARS);
    let message_id = format!("terminal-{}", chunk.terminal_id.0);
    Some(V1SessionUpdate::AgentMessageChunk(
        ee_agent_protocol::ContentChunk::new(V1ContentBlock::Text(
            ee_agent_protocol::TextContent::new(text),
        ))
        .message_id(ee_agent_protocol::MessageId::new(message_id)),
    ))
}

/// v2 `tool_call_update` (upsert) → v1 patch-shaped update: present fields
/// are mapped, absent fields stay absent.
fn translate_tool_call_update(update: &v2::ToolCallUpdate) -> Option<V1SessionUpdate> {
    Some(V1SessionUpdate::ToolCallUpdate(tool_call_update_to_v1(update)))
}

/// v2 `tool_call_update` → the v1-typed upsert the broker and reducer use.
#[must_use]
pub fn tool_call_update_to_v1(update: &v2::ToolCallUpdate) -> V1ToolCallUpdate {
    let mut fields = ToolCallUpdateFields::new();
    // v2 `name` is a MaybeUndefined patch: omission stays omitted, `null`
    // (explicit clear) is lossy in v1, which treats `None` as omission, so
    // only concrete names map.
    if let Some(name) = update.name.value() {
        fields = fields.name(name.clone());
    }
    if let Some(title) = update.title.value() {
        fields = fields.title(title.clone());
    }
    if let Some(kind) = update.kind.value() {
        fields = fields.kind(translate_tool_kind_v1(kind.clone()));
    }
    if let Some(status) = update.status.value() {
        fields = fields.status(translate_tool_status_v1(status.clone()));
    }
    if let Some(content) = update.content.value() {
        let translated = content.iter().filter_map(translate_tool_call_content).collect::<Vec<_>>();
        if !content.is_empty() && translated.len() != content.len() {
            tracing::warn!("dropped v2 tool call content items with no v1 representation");
        }
        fields = fields.content(translated);
    }
    if let Some(locations) = update.locations.value() {
        let translated = locations
            .iter()
            .filter_map(|location| {
                serde_json::from_value(serde_json::to_value(location).ok()?).ok()
            })
            .collect::<Vec<ee_agent_protocol::ToolCallLocation>>();
        fields = fields.locations(translated);
    }
    if let Some(raw) = update.raw_input.value() {
        fields = fields.raw_input(raw.clone());
    }
    if let Some(raw) = update.raw_output.value() {
        fields = fields.raw_output(raw.clone());
    }
    V1ToolCallUpdate::new(ee_agent_protocol::ToolCallId::new(update.tool_call_id.0.clone()), fields)
}

/// Builds the v1-shaped broker tool call for one v2 `session/request_permission`
/// subject.  Tool-call subjects map field-for-field.  Command subjects embed
/// the command text in the displayed title (the v1 broker has no command
/// shape) and keep the command/cwd pair in `rawInput` for provenance, reusing
/// the subject's optional `toolCallId` association when present.  Absent and
/// unknown (`Other`) subjects fall back to a title-only placeholder — the
/// guide's generic prompt.  `fallback_id` keeps repeated title-only rows
/// distinct per session.
#[must_use]
pub fn permission_subject_tool_call(
    title: &str,
    subject: Option<&v2::RequestPermissionSubject>,
    fallback_id: &str,
) -> V1ToolCallUpdate {
    match subject {
        Some(v2::RequestPermissionSubject::ToolCall(subject)) => {
            tool_call_update_to_v1(&subject.tool_call)
        }
        Some(v2::RequestPermissionSubject::Command(command)) => {
            let id = command
                .tool_call_id
                .as_ref()
                .map(|id| ee_agent_protocol::ToolCallId::new(id.0.clone()))
                .unwrap_or(ee_agent_protocol::ToolCallId::new(fallback_id));
            let mut fields = ToolCallUpdateFields::new();
            fields = fields.title(format!("{} · $ {}", title, command.command));
            let mut raw_input = serde_json::json!({ "command": command.command });
            if !command.cwd.0.as_os_str().is_empty() {
                raw_input["cwd"] =
                    serde_json::Value::String(command.cwd.0.to_string_lossy().into_owned());
            }
            fields = fields.raw_input(raw_input);
            V1ToolCallUpdate::new(id, fields)
        }
        None | Some(_) => {
            let mut fields = ToolCallUpdateFields::new();
            fields = fields.title(title.to_string());
            V1ToolCallUpdate::new(ee_agent_protocol::ToolCallId::new(fallback_id), fields)
        }
    }
}

/// Maps a v2 tool kind onto the v1-typed value via the shared wire shape.
fn translate_tool_kind_v1(kind: v2::ToolKind) -> ee_agent_protocol::ToolKind {
    serde_json::from_value(serde_json::to_value(kind).expect("v2 kind serializes"))
        .unwrap_or_default()
}

fn translate_tool_call_content(
    content: &v2::ToolCallContent,
) -> Option<ee_agent_protocol::ToolCallContent> {
    match content {
        v2::ToolCallContent::Content(content) => Some(ee_agent_protocol::ToolCallContent::Content(
            ee_agent_protocol::Content::new(translate_block_v1(&content.content)),
        )),
        v2::ToolCallContent::Terminal(terminal) => {
            Some(ee_agent_protocol::ToolCallContent::Terminal(ee_agent_protocol::Terminal::new(
                ee_agent_protocol::TerminalId::new(terminal.terminal_id.0.clone()),
            )))
        }
        // v2 diffs are structured changes with optional renderable patch
        // text; v1 requires oldText/newText, so there is no faithful mapping.
        v2::ToolCallContent::Diff(_) | v2::ToolCallContent::Other(_) => {
            tracing::debug!("dropping v2 tool call content with no v1 representation");
            None
        }
        _ => {
            tracing::debug!("dropping unknown v2 tool call content");
            None
        }
    }
}

/// v2 `plan_update` → v1 plan replacement (entries only; the v2 plan id is
/// not representable in v1).
fn translate_plan(plan: &v2::PlanUpdate) -> Option<V1SessionUpdate> {
    let v2::PlanUpdateContent::Items(items) = &plan.plan else {
        tracing::debug!("dropping non-items v2 plan update");
        return None;
    };
    let entries = items
        .entries
        .iter()
        .filter_map(|entry| serde_json::from_value(serde_json::to_value(entry).ok()?).ok())
        .collect();
    Some(V1SessionUpdate::Plan(ee_agent_protocol::Plan::new(entries)))
}

/// v2 `available_commands_update` → v1 (the tagged text input becomes the v1
/// untagged input shape).
fn translate_available_commands(update: &v2::AvailableCommandsUpdate) -> V1SessionUpdate {
    let commands = update
        .available_commands
        .iter()
        .map(|command| {
            let mut converted =
                V1AvailableCommand::new(command.name.clone(), command.description.clone());
            if let Some(v2::AvailableCommandInput::Text(input)) = &command.input {
                converted = converted.input(Some(V1CommandInput::Unstructured(
                    UnstructuredCommandInput::new(input.hint.clone()),
                )));
            }
            converted
        })
        .collect();
    V1SessionUpdate::AvailableCommandsUpdate(ee_agent_protocol::AvailableCommandsUpdate::new(
        commands,
    ))
}

/// v2 `config_option_update` → v1 wire rename (`configId` → `id`,
/// `groupId` → `group`).
fn translate_config_options(update: &v2::ConfigOptionUpdate) -> Option<V1SessionUpdate> {
    let value = serde_json::to_value(update).ok()?;
    let renamed = rename_config_keys(value);
    let converted: ee_agent_protocol::ConfigOptionUpdate = serde_json::from_value(renamed).ok()?;
    Some(V1SessionUpdate::ConfigOptionUpdate(converted))
}

/// v2 `session/new`/`session/resume` response config options → the v1 wire
/// keys (`configId` → `id`, `groupId` → `group`).  Options without a v1
/// representation are dropped.
#[must_use]
pub fn config_options_to_v1(
    options: Vec<v2::SessionConfigOption>,
) -> Vec<ee_agent_protocol::SessionConfigOption> {
    let Some(value) = serde_json::to_value(options).ok() else {
        return Vec::new();
    };
    serde_json::from_value(rename_config_keys(value)).unwrap_or_default()
}

fn rename_config_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut renamed = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let key = match key.as_str() {
                    "configId" => "id".to_string(),
                    "groupId" => "group".to_string(),
                    other => other.to_string(),
                };
                renamed.insert(key, rename_config_keys(value));
            }
            serde_json::Value::Object(renamed)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(rename_config_keys).collect())
        }
        other => other,
    }
}

/// v2 content block → v1 block via the shared wire JSON shape.
fn translate_block_v1(block: &v2::ContentBlock) -> V1ContentBlock {
    serde_json::from_value(serde_json::to_value(block).expect("v2 block serializes"))
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "dropping v2 content block with no v1 representation");
            V1ContentBlock::Text(ee_agent_protocol::TextContent::new(""))
        })
}

/// Round-trips one chunk value through its shared wire JSON into the v1
/// chunk type and wraps it in the session update the reducer consumes.
fn chunk_to_v1(
    chunk: &v2::ContentChunk,
    make: fn(ee_agent_protocol::ContentChunk) -> V1SessionUpdate,
) -> Option<V1SessionUpdate> {
    value_to_v1::<ee_agent_protocol::ContentChunk>(serde_json::to_value(chunk).ok()?).map(make)
}

/// Round-trips a value through its shared wire JSON into the target v1 type.
fn value_to_v1<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Option<T> {
    match serde_json::from_value(value) {
        Ok(converted) => Some(converted),
        Err(error) => {
            tracing::warn!(%error, "dropping v2 update with no v1 representation");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::v2;

    fn chunk_update(text: &str) -> v2::SessionUpdate {
        v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            v2::ContentBlock::Text(v2::TextContent::new(text)),
            v2::MessageId::new("m-1"),
        ))
    }

    #[test]
    fn agent_message_chunk_maps_to_v1_chunk() {
        let update = chunk_update("hello");
        let translated = translate_v2_update(&update);
        let Some(translated) = translated.first() else {
            let raw = serde_json::to_string_pretty(&update).expect("serializes");
            panic!("chunk did not translate; raw update:\n{raw}")
        };
        let V1SessionUpdate::AgentMessageChunk(chunk) = translated else {
            panic!("expected agent message chunk");
        };
        assert_eq!(chunk.message_id, Some(ee_agent_protocol::MessageId::new("m-1")));
        let ee_agent_protocol::ContentBlock::Text(text) = &chunk.content else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "hello");
    }

    #[test]
    fn tool_call_update_maps_present_fields_only() {
        let update = v2::ToolCallUpdate::new(v2::ToolCallId::new("tc-1"))
            .title("Run tests")
            .status(v2::ToolCallStatus::Completed);
        let translated =
            translate_v2_update(&v2::SessionUpdate::ToolCallUpdate(update)).pop().expect("maps");
        let V1SessionUpdate::ToolCallUpdate(update) = translated else {
            panic!("expected tool call update");
        };
        assert_eq!(update.tool_call_id, ee_agent_protocol::ToolCallId::new("tc-1"));
        assert_eq!(update.fields.title.as_deref(), Some("Run tests"));
        assert_eq!(update.fields.status, Some(ee_agent_protocol::ToolCallStatus::Completed));
        assert!(update.fields.kind.is_none(), "absent fields stay absent");
    }

    #[test]
    fn tool_call_update_maps_name_across() {
        let update = v2::ToolCallUpdate::new(v2::ToolCallId::new("tc-1"))
            .name("read_text_file")
            .status(v2::ToolCallStatus::InProgress);
        let translated =
            translate_v2_update(&v2::SessionUpdate::ToolCallUpdate(update)).pop().expect("maps");
        let V1SessionUpdate::ToolCallUpdate(update) = translated else {
            panic!("expected tool call update");
        };
        assert_eq!(update.tool_call_id, ee_agent_protocol::ToolCallId::new("tc-1"));
        assert_eq!(update.fields.name.as_deref(), Some("read_text_file"));
        assert_eq!(update.fields.status, Some(ee_agent_protocol::ToolCallStatus::InProgress));
    }

    #[test]
    fn plan_update_items_become_plan_replacement() {
        let plan_id = v2::PlanId::new("plan-1");
        let entries = vec![
            serde_json::from_value(serde_json::json!({
                "content": "step one",
                "priority": "high",
                "status": "pending",
            }))
            .expect("v2 plan entry parses"),
        ];
        let plan =
            v2::PlanUpdate::new(v2::PlanUpdateContent::Items(v2::PlanItems::new(plan_id, entries)));
        let translated =
            translate_v2_update(&v2::SessionUpdate::PlanUpdate(plan)).pop().expect("maps");
        let V1SessionUpdate::Plan(plan) = translated else {
            panic!("expected plan update");
        };
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].content, "step one");
    }

    #[test]
    fn text_command_input_becomes_untagged_v1_input() {
        let command = v2::AvailableCommand::new("search", "Search the codebase")
            .input(Some(v2::AvailableCommandInput::Text(v2::TextCommandInput::new("hint"))));
        let update = v2::AvailableCommandsUpdate::new(vec![command]);
        let translated = translate_v2_update(&v2::SessionUpdate::AvailableCommandsUpdate(update))
            .pop()
            .expect("maps");
        let V1SessionUpdate::AvailableCommandsUpdate(update) = translated else {
            panic!("expected available commands update");
        };
        let Some(ee_agent_protocol::AvailableCommandInput::Unstructured(input)) =
            &update.available_commands[0].input
        else {
            panic!("expected untagged v1 input");
        };
        assert_eq!(input.hint, "hint");
    }

    #[test]
    fn state_and_terminal_updates_have_no_v1_form() {
        assert!(
            translate_v2_update(&v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                v2::RunningStateUpdate::new()
            )))
            .is_empty()
        );
        assert!(
            translate_v2_update(&v2::SessionUpdate::TerminalUpdate(v2::TerminalUpdate::new(
                v2::TerminalId::new("t-1")
            )))
            .is_empty()
        );
        // Terminal output chunks take the sanitized transcript fallback.
        let chunk = translate_v2_update(&v2::SessionUpdate::TerminalOutputChunk(
            v2::TerminalOutputChunk::new(v2::TerminalId::new("t-1"), base64_encode("git status\n")),
        ));
        assert_eq!(chunk.len(), 1, "terminal output must surface in the transcript");
        assert!(matches!(chunk[0], V1SessionUpdate::AgentMessageChunk(_)));
    }

    /// Base64 helper (the crate already depends on `base64`).
    fn base64_encode(text: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    }

    #[test]
    fn whole_message_of_borrows_replace_and_clear() {
        let replaced = v2::AgentMessage::new(v2::MessageId::new("m-1"))
            .content(vec![v2::ContentBlock::Text(v2::TextContent::new("replacement"))]);
        let update = v2::SessionUpdate::AgentMessage(replaced);
        let Some(WholeMessage::Agent { message_id, blocks }) = whole_message_of(&update) else {
            panic!("whole agent message must be recognized");
        };
        assert_eq!(message_id, &v2::MessageId::new("m-1"));
        assert_eq!(blocks.expect("content present").len(), 1);

        // `content: null` is an explicit clear.
        let cleared =
            v2::AgentMessage::new(v2::MessageId::new("m-1")).content(None::<Vec<v2::ContentBlock>>);
        let update = v2::SessionUpdate::AgentMessage(cleared);
        let Some(WholeMessage::Agent { blocks, .. }) = whole_message_of(&update) else {
            panic!("whole agent message must be recognized");
        };
        assert!(blocks.is_none(), "null content is a clear");

        // Chunks are not whole messages; translation still flattens them.
        assert!(whole_message_of(&chunk_update("hello")).is_none());
        assert_eq!(translate_v2_update(&chunk_update("hello")).len(), 1);
    }

    #[test]
    fn whole_messages_never_flatten_to_chunks() {
        let whole = v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(v2::MessageId::new("m-1"))
                .content(vec![v2::ContentBlock::Text(v2::TextContent::new("body"))]),
        );
        assert!(
            translate_v2_update(&whole).is_empty(),
            "flattening would append where v2 mandates replacement"
        );
    }

    #[test]
    fn cancelled_tool_status_maps_to_failed() {
        let update = v2::ToolCallUpdate::new(v2::ToolCallId::new("tc-1"))
            .status(v2::ToolCallStatus::Cancelled);
        let translated =
            translate_v2_update(&v2::SessionUpdate::ToolCallUpdate(update)).pop().expect("maps");
        let V1SessionUpdate::ToolCallUpdate(update) = translated else {
            panic!("expected tool call update");
        };
        assert_eq!(
            update.fields.status,
            Some(ee_agent_protocol::ToolCallStatus::Failed),
            "v1 has no cancelled status; failed is the honest fallback"
        );
    }

    #[test]
    fn command_permission_subject_embeds_command_in_title() {
        let subject = v2::RequestPermissionSubject::Command(
            serde_json::from_value(serde_json::json!({
                "command": "cargo test",
                "cwd": "/work",
                "toolCallId": "call_1",
            }))
            .expect("command subject parses"),
        );
        let tool_call =
            permission_subject_tool_call("Run the suite?", Some(&subject), "permission-s1");
        assert_eq!(tool_call.tool_call_id, ee_agent_protocol::ToolCallId::new("call_1"));
        assert_eq!(tool_call.fields.title.as_deref(), Some("Run the suite? · $ cargo test"));
        let raw = tool_call.fields.raw_input.as_ref().expect("raw input recorded");
        assert_eq!(raw["command"], "cargo test");
        assert_eq!(raw["cwd"], "/work");
    }

    #[test]
    fn absent_permission_subject_falls_back_to_title_only() {
        let tool_call = permission_subject_tool_call("Approve?", None, "permission-s1");
        assert_eq!(tool_call.tool_call_id, ee_agent_protocol::ToolCallId::new("permission-s1"));
        assert_eq!(tool_call.fields.title.as_deref(), Some("Approve?"));
    }

    #[test]
    fn config_option_update_renames_keys_back_to_v1() {
        let update = v2::ConfigOptionUpdate::new(vec![
            serde_json::from_value(serde_json::json!({
                "configId": "mode",
                "name": "Mode",
                "type": "select",
                "currentValue": "ask",
                "options": [
                    { "value": "ask", "name": "Ask" },
                ],
            }))
            .expect("v2 option parses"),
        ]);
        let translated = translate_v2_update(&v2::SessionUpdate::ConfigOptionUpdate(update))
            .pop()
            .expect("maps");
        let V1SessionUpdate::ConfigOptionUpdate(update) = translated else {
            panic!("expected config option update");
        };
        assert_eq!(update.config_options[0].id, ee_agent_protocol::SessionConfigId::new("mode"));
    }
}
