//! ACP v2 wire translation for v2 connections.
//!
//! Providers keep speaking the v1 update surface ([`SessionUpdate`]); on a
//! negotiated v2 connection the server translates those updates into v2
//! `session/update` notifications at the wire boundary.  Translation rules:
//!
//! - message chunks carry over (the v1 chunks always carry a `messageId`;
//!   chunks without one cannot be represented in v2 and are dropped with a
//!   warning);
//! - `tool_call` (v1 create) becomes a `tool_call_update` — v2 creates tool
//!   calls on first-seen `toolCallId`;
//! - `tool_call_update` patch fields map one-to-one; v1 `Diff` content becomes
//!   a structured v2 `changes[{operation: modify, path}]` change without
//!   renderable patch text (v2 has no mechanical oldText/newText mapping);
//! - v1 `plan` becomes a v2 `plan_update` under a session-stable `plan-1`
//!   plan id (v1 semantics = one plan per session; each update replaces it);
//! - `available_commands_update` gains the required `type: "text"` input
//!   discriminator;
//! - any other v1 update (config options, usage, session info, extension
//!   variants) is round-tripped through JSON: v2 receivers either parse it
//!   into the matching v2 type or preserve it as an unknown `sessionUpdate`
//!   variant per v2 forward-compatibility; updates that fail even that are
//!   dropped with a warning.
//!
//! The framework also synthesizes the v2-only lifecycle updates: the
//! `user_message` acknowledgment with the agent-owned `messageId` (see
//! `dispatch`), `state_update` `running` while a prompt is active, and the
//! idle `state_update` carrying the stop reason when a turn ends.

use ee_agent_protocol::v2;
use ee_agent_protocol::{
    AvailableCommand as V1AvailableCommand, AvailableCommandInput as V1CommandInput,
    AvailableCommandsUpdate as V1CommandsUpdate, ContentBlock as V1ContentBlock,
    ContentChunk as V1ContentChunk, MessageId as V1MessageId, Plan as V1Plan,
    SessionConfigOption as V1SessionConfigOption, SessionUpdate as V1SessionUpdate,
    StopReason as V1StopReason, ToolCall as V1ToolCall, ToolCallContent as V1ToolCallContent,
    ToolCallUpdate as V1ToolCallUpdate,
};

/// Session-stable plan id used for v1→v2 plan translation.
///
/// v1 carries one plan per session and replaces it wholesale on every update;
/// v2 keys plans by id.  A single stable id reproduces v1 semantics (multiple
/// concurrent plans are not expressible in v1, so nothing is lost).
const SESSION_PLAN_ID: &str = "plan-1";

/// Translates one v1 session update into the v2 updates a v2 client should
/// receive.  Returns `Vec` because some v1 variants have no v2 equivalent
/// (dropped) and unknown ones pass through as tolerated extension updates.
#[must_use]
pub fn translate_update(update: &V1SessionUpdate) -> Vec<v2::SessionUpdate> {
    match update {
        V1SessionUpdate::UserMessageChunk(chunk) => {
            translate_chunk(chunk, v2::SessionUpdate::UserMessageChunk)
        }
        V1SessionUpdate::AgentMessageChunk(chunk) => {
            translate_chunk(chunk, v2::SessionUpdate::AgentMessageChunk)
        }
        V1SessionUpdate::AgentThoughtChunk(chunk) => {
            translate_chunk(chunk, v2::SessionUpdate::AgentThoughtChunk)
        }
        V1SessionUpdate::ToolCall(call) => {
            vec![v2::SessionUpdate::ToolCallUpdate(translate_tool_call(call))]
        }
        V1SessionUpdate::ToolCallUpdate(update) => {
            vec![v2::SessionUpdate::ToolCallUpdate(translate_tool_call_update(update))]
        }
        V1SessionUpdate::Plan(plan) => vec![translate_plan(plan)],
        V1SessionUpdate::AvailableCommandsUpdate(update) => {
            vec![translate_available_commands(update)]
        }
        other => passthrough(other),
    }
}

/// One update variant is produced; `None` means the update was not
/// representable (missing required v2 field) and was dropped with a warning.
fn translate_chunk(
    chunk: &V1ContentChunk,
    make: fn(v2::ContentChunk) -> v2::SessionUpdate,
) -> Vec<v2::SessionUpdate> {
    let Some(message_id) = &chunk.message_id else {
        tracing::warn!(
            "dropping v1 message chunk without messageId: v2 requires one on every chunk"
        );
        return Vec::new();
    };
    vec![make(v2::ContentChunk::new(translate_block(&chunk.content), to_v2_message_id(message_id)))]
}

/// v1 `tool_call` (create) → v2 `tool_call_update` (v2 creates on first-seen
/// `toolCallId`; the update carries the same state).
fn translate_tool_call(call: &V1ToolCall) -> v2::ToolCallUpdate {
    let mut update = v2::ToolCallUpdate::new(to_v2_tool_call_id(&call.tool_call_id))
        .title(call.title.clone())
        .kind(translate_tool_kind(call.kind))
        .status(translate_tool_status(call.status));
    if let Some(name) = &call.name {
        update = update.name(name.clone());
    }
    if !call.content.is_empty() {
        update = update.content(translate_tool_call_contents(&call.content));
    }
    if !call.locations.is_empty() {
        update = update.locations(translate_locations(&call.locations));
    }
    if let Some(raw) = &call.raw_input {
        update = update.raw_input(raw.clone());
    }
    if let Some(raw) = &call.raw_output {
        update = update.raw_output(raw.clone());
    }
    update
}

/// v1 `tool_call_update` patch → v2 `tool_call_update` with the same patch
/// fields; omitted stays omitted.
fn translate_tool_call_update(update: &V1ToolCallUpdate) -> v2::ToolCallUpdate {
    let mut translated = v2::ToolCallUpdate::new(to_v2_tool_call_id(&update.tool_call_id));
    let fields = &update.fields;
    if let Some(name) = &fields.name {
        translated = translated.name(name.clone());
    }
    if let Some(title) = &fields.title {
        translated = translated.title(title.clone());
    }
    if let Some(kind) = &fields.kind {
        translated = translated.kind(translate_tool_kind(*kind));
    }
    if let Some(status) = &fields.status {
        translated = translated.status(translate_tool_status(*status));
    }
    if let Some(content) = &fields.content {
        translated = translated.content(translate_tool_call_contents(content));
    }
    if let Some(locations) = &fields.locations {
        translated = translated.locations(translate_locations(locations));
    }
    if let Some(raw) = &fields.raw_input {
        translated = translated.raw_input(raw.clone());
    }
    if let Some(raw) = &fields.raw_output {
        translated = translated.raw_output(raw.clone());
    }
    translated
}

fn translate_tool_call_contents(contents: &[V1ToolCallContent]) -> Vec<v2::ToolCallContent> {
    contents.iter().filter_map(translate_tool_call_content).collect()
}

/// v1 `Diff` → v2 structured change list plus an optional renderable git
/// patch synthesized from the v1 texts.
fn translate_diff(diff: &ee_agent_protocol::Diff) -> v2::ToolCallContent {
    let change = v2::DiffChange::modify(v2::AbsolutePath::new(diff.path.clone()))
        .file_type(Some(v2::DiffFileType::Text));
    let mut translated = v2::Diff::new(vec![change]);
    // v1 carries the full old/new text; v2's `patch` is optional renderable
    // text in git `--patch` format.  Synthesize a minimal whole-file patch
    // (absolute paths, no commit metadata) so clients can apply or display it.
    translated = translated.with_patch(v2::DiffPatch::new(synthesize_git_patch(
        &diff.path,
        diff.old_text.as_deref(),
        &diff.new_text,
    )));
    v2::ToolCallContent::Diff(translated)
}

/// Builds a minimal git `--patch`-format text for a whole-file replacement.
/// `old`/`new` are the previous and new file contents; a missing old text is
/// rendered as a pure insertion hunk.
fn synthesize_git_patch(path: &std::path::Path, old: Option<&str>, new: &str) -> String {
    let old = old.unwrap_or("");
    let old_lines = if old.is_empty() { 0 } else { old.lines().count() };
    let new_lines = if new.is_empty() { 0 } else { new.lines().count() };
    let old_start = if old_lines == 0 { 0 } else { 1 };
    let new_start = if new_lines == 0 { 0 } else { 1 };
    let path = path.display();
    let mut patch = format!("--- a/{path}\n+++ b/{path}\n");
    patch.push_str(&format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n"));
    for line in old.lines() {
        patch.push('-');
        patch.push_str(line);
        patch.push('\n');
    }
    for line in new.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

fn translate_tool_call_content(content: &V1ToolCallContent) -> Option<v2::ToolCallContent> {
    match content {
        V1ToolCallContent::Content(content) => Some(v2::ToolCallContent::Content(Box::new(
            v2::Content::new(translate_block(&content.content)),
        ))),
        V1ToolCallContent::Diff(diff) => Some(translate_diff(diff)),
        V1ToolCallContent::Terminal(terminal) => Some(v2::ToolCallContent::Terminal(
            v2::Terminal::new(to_v2_terminal_id(&terminal.terminal_id)),
        )),
        // Unknown/vendor v1 content variants have no v2 representation; the
        // tool call still renders, only this content item is dropped.
        other => {
            tracing::warn!(?other, "dropping v1 tool call content with no v2 representation");
            None
        }
    }
}

fn translate_locations(
    locations: &[ee_agent_protocol::ToolCallLocation],
) -> Vec<v2::ToolCallLocation> {
    locations
        .iter()
        .filter_map(|location| serde_json::from_value(serde_json::to_value(location).ok()?).ok())
        .collect()
}

/// v1 `plan` → v2 `plan_update` under the session-stable plan id.
fn translate_plan(plan: &V1Plan) -> v2::SessionUpdate {
    plan_update(SESSION_PLAN_ID, &plan.entries)
}

/// Builds a v2 `plan_update` carrying entries under a caller-chosen plan id
/// (provider-controlled ids for concurrent plans; v1-origin updates keep the
/// session-stable `plan-1`).
#[must_use]
pub fn plan_update(plan_id: &str, entries: &[ee_agent_protocol::PlanEntry]) -> v2::SessionUpdate {
    let translated = entries
        .iter()
        .filter_map(|entry| serde_json::from_value(serde_json::to_value(entry).ok()?).ok())
        .collect::<Vec<v2::PlanEntry>>();
    if translated.len() != entries.len() {
        tracing::warn!("dropped v1 plan entries that carry no v2 representation");
    }
    v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(v2::PlanUpdateContent::Items(
        v2::PlanItems::new(v2::PlanId::new(plan_id), translated),
    )))
}

/// v2 `tool_call_content_chunk` for one streamed tool-call content item.
/// Returns `None` when the v1 item has no v2 representation.
#[must_use]
pub fn tool_call_content_chunk(
    tool_call_id: &ee_agent_protocol::ToolCallId,
    content: &V1ToolCallContent,
) -> Option<v2::SessionUpdate> {
    let translated = translate_tool_call_content(content)?;
    Some(v2::SessionUpdate::ToolCallContentChunk(v2::ToolCallContentChunk::new(
        to_v2_tool_call_id(tool_call_id),
        translated,
    )))
}

/// v1 `available_commands_update` → v2, with the required `type: "text"`
/// input discriminator.
fn translate_available_commands(update: &V1CommandsUpdate) -> v2::SessionUpdate {
    let commands = update.available_commands.iter().map(translate_command).collect();
    v2::SessionUpdate::AvailableCommandsUpdate(v2::AvailableCommandsUpdate::new(commands))
}

fn translate_command(command: &V1AvailableCommand) -> v2::AvailableCommand {
    let mut translated =
        v2::AvailableCommand::new(command.name.clone(), command.description.clone());
    if let Some(V1CommandInput::Unstructured(input)) = &command.input {
        translated = translated.input(Some(v2::AvailableCommandInput::Text(
            v2::TextCommandInput::new(input.hint.clone()),
        )));
    }
    translated
}

/// Round-trips a v1 update through JSON into the v2 enum.  v2 receivers parse
/// matching shapes into their typed variants and preserve anything else as an
/// unknown `sessionUpdate` value (forward compatibility); an update that
/// cannot even be preserved is dropped with a warning.
fn passthrough(update: &V1SessionUpdate) -> Vec<v2::SessionUpdate> {
    let Ok(value) = serde_json::to_value(update) else {
        tracing::warn!("failed to serialize v1 session update for v2 passthrough");
        return Vec::new();
    };
    match serde_json::from_value(value) {
        Ok(translated) => vec![translated],
        Err(error) => {
            tracing::warn!(
                %error,
                "dropping v1 session update with no v2 representation"
            );
            Vec::new()
        }
    }
}

/// The v2 `user_message` acknowledgment for an inserted user message: carries
/// the agent-owned `messageId` and the full prompt content.
#[must_use]
pub fn user_message_update(
    message_id: &V1MessageId,
    blocks: &[V1ContentBlock],
) -> v2::SessionUpdate {
    let content = blocks.iter().map(translate_block).collect::<Vec<_>>();
    v2::SessionUpdate::UserMessage(
        v2::UserMessage::new(to_v2_message_id(message_id)).content(content),
    )
}

/// `state_update` reporting that foreground work started.
#[must_use]
pub fn state_running() -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(v2::RunningStateUpdate::new()))
}

/// `state_update` reporting that foreground work is blocked on user action
/// (an agent → client request is pending).
#[must_use]
pub fn state_requires_action() -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(v2::StateUpdate::RequiresAction(
        v2::RequiresActionStateUpdate::new(),
    ))
}

/// `state_update` reporting that foreground work ended; `stop_reason` is only
/// set when the transition ends a turn.
#[must_use]
pub fn state_idle(stop_reason: Option<v2::StopReason>) -> v2::SessionUpdate {
    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
        v2::IdleStateUpdate::new().stop_reason(stop_reason),
    ))
}

/// Maps a v1 stop reason to its v2 equivalent.  The variant set is identical
/// across versions (`end_turn`, `max_tokens`, `max_turn_requests`, `refusal`,
/// `cancelled`).
#[must_use]
pub fn stop_reason_v2(reason: V1StopReason) -> v2::StopReason {
    match reason {
        V1StopReason::EndTurn => v2::StopReason::EndTurn,
        V1StopReason::MaxTokens => v2::StopReason::MaxTokens,
        V1StopReason::MaxTurnRequests => v2::StopReason::MaxTurnRequests,
        V1StopReason::Refusal => v2::StopReason::Refusal,
        V1StopReason::Cancelled => v2::StopReason::Cancelled,
        // `NonExhaustive` guard: unknown future v1 reasons end the turn like
        // a refusal would, never a success.
        other => {
            tracing::warn!(?other, "unknown v1 stop reason mapped to refusal");
            v2::StopReason::Refusal
        }
    }
}

/// Converts a v1 content block to its v2 equivalent via the shared wire JSON
/// shape (both generations carry the same five block types).
#[must_use]
pub fn translate_block(block: &V1ContentBlock) -> v2::ContentBlock {
    match serde_json::to_value(block).ok().and_then(|value| serde_json::from_value(value).ok()) {
        Some(translated) => translated,
        None => {
            tracing::warn!("dropping v1 content block with no v2 representation");
            v2::ContentBlock::Text(v2::TextContent::new(""))
        }
    }
}

/// Transfers v1 session config options onto the v2 wire.
///
/// The option shape is identical across versions except for two renames
/// (the id field and the select-group field); the rest is carried over
/// unchanged.  Options that fail v2 deserialization are dropped with a
/// warning — config options are UX hints, never protocol-critical.
#[must_use]
pub fn translate_config_options(options: &[V1SessionConfigOption]) -> Vec<v2::SessionConfigOption> {
    options
        .iter()
        .filter_map(|option| {
            let value = serde_json::to_value(option).ok()?;
            let renamed = rename_config_keys(value);
            let translated = serde_json::from_value(renamed).ok();
            if translated.is_none() {
                tracing::warn!("dropping v1 config option with no v2 representation");
            }
            translated
        })
        .collect()
}

/// Renames the two v1→v2 config-option keys recursively: `id` → `configId`
/// (option identifier) and `group` → `groupId` (select group identifier).
/// Option *values* are carried under `value`, so neither rename can collide
/// with option data.
fn rename_config_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut renamed = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let key = match key.as_str() {
                    "id" => "configId".to_string(),
                    "group" => "groupId".to_string(),
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

/// Maps a v1 tool kind to its v2 equivalent via the shared wire value.
fn translate_tool_kind(kind: ee_agent_protocol::ToolKind) -> v2::ToolKind {
    serde_json::from_value(serde_json::to_value(kind).expect("tool kind serializes"))
        .unwrap_or_default()
}

/// Maps a v1 tool status to its v2 equivalent via the shared wire value.
fn translate_tool_status(status: ee_agent_protocol::ToolCallStatus) -> v2::ToolCallStatus {
    serde_json::from_value(serde_json::to_value(status).expect("tool status serializes"))
        .unwrap_or_default()
}

fn to_v2_message_id(message_id: &V1MessageId) -> v2::MessageId {
    v2::MessageId::new(message_id.0.clone())
}

fn to_v2_tool_call_id(tool_call_id: &ee_agent_protocol::ToolCallId) -> v2::ToolCallId {
    v2::ToolCallId::new(tool_call_id.0.clone())
}

fn to_v2_terminal_id(terminal_id: &ee_agent_protocol::TerminalId) -> v2::TerminalId {
    v2::TerminalId::new(terminal_id.0.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{
        AvailableCommand, AvailableCommandInput, ContentBlock, ContentChunk, MessageId, Plan,
        PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionUpdate, TextContent, ToolCall,
        ToolCallContent, ToolCallStatus, ToolKind, UnstructuredCommandInput, v2,
    };

    fn text_block(text: &str) -> V1ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    #[test]
    fn agent_message_chunk_carries_message_id_and_block() {
        let chunk = ContentChunk::new(text_block("hello")).message_id(MessageId::new("m-1"));
        let translated = translate_update(&SessionUpdate::AgentMessageChunk(chunk));
        assert_eq!(translated.len(), 1);
        let v2::SessionUpdate::AgentMessageChunk(translated_chunk) = &translated[0] else {
            panic!("expected agent message chunk");
        };
        assert_eq!(translated_chunk.message_id, v2::MessageId::new("m-1"));
        assert_eq!(
            serde_json::to_value(&translated_chunk.content).expect("block serializes"),
            serde_json::to_value(text_block("hello")).expect("block serializes")
        );
    }

    #[test]
    fn chunk_without_message_id_is_dropped() {
        // Construct a v1 chunk without messageId via raw JSON (the SDK builder
        // always sets one).
        let chunk: ContentChunk = serde_json::from_value(serde_json::json!({
            "content": text_block("hello"),
        }))
        .expect("v1 chunk without messageId parses");
        assert!(translate_update(&SessionUpdate::AgentMessageChunk(chunk)).is_empty());
    }

    #[test]
    fn tool_call_becomes_tool_call_update() {
        let call =
            ToolCall::new("tc-1", "Read file").kind(ToolKind::Read).status(ToolCallStatus::Pending);
        let translated = translate_update(&SessionUpdate::ToolCall(call));
        assert_eq!(translated.len(), 1);
        let v2::SessionUpdate::ToolCallUpdate(update) = &translated[0] else {
            panic!("expected tool call update");
        };
        assert_eq!(update.tool_call_id, v2::ToolCallId::new("tc-1"));
        assert_eq!(update.title.value(), Some(&"Read file".to_string()));
        assert_eq!(update.status.value(), Some(&v2::ToolCallStatus::Pending));
    }

    #[test]
    fn tool_call_create_and_update_carry_name_across() {
        let call = ToolCall::new("tc-1", "Read file")
            .name("read_text_file")
            .kind(ToolKind::Read)
            .status(ToolCallStatus::Pending);
        let translated = translate_update(&SessionUpdate::ToolCall(call));
        let v2::SessionUpdate::ToolCallUpdate(update) = &translated[0] else {
            panic!("expected tool call update");
        };
        assert_eq!(update.name.value(), Some(&"read_text_file".to_string()));

        let mut fields = ee_agent_protocol::ToolCallUpdateFields::new();
        fields = fields.name("write_text_file");
        let translated = translate_update(&SessionUpdate::ToolCallUpdate(
            ee_agent_protocol::ToolCallUpdate::new("tc-1", fields),
        ));
        let v2::SessionUpdate::ToolCallUpdate(update) = &translated[0] else {
            panic!("expected tool call update");
        };
        assert_eq!(update.name.value(), Some(&"write_text_file".to_string()));
    }

    #[test]
    fn plan_becomes_plan_update_with_session_stable_id() {
        let plan = Plan::new(vec![PlanEntry::new(
            "step one",
            PlanEntryPriority::High,
            PlanEntryStatus::Pending,
        )]);
        let translated = translate_update(&SessionUpdate::Plan(plan));
        assert_eq!(translated.len(), 1);
        let v2::SessionUpdate::PlanUpdate(update) = &translated[0] else {
            panic!("expected plan update");
        };
        let v2::PlanUpdateContent::Items(items) = &update.plan else {
            panic!("expected items plan");
        };
        assert_eq!(items.plan_id, v2::PlanId::new(SESSION_PLAN_ID));
        assert_eq!(items.entries.len(), 1);
        assert_eq!(items.entries[0].content, "step one");
    }

    #[test]
    fn available_commands_gain_text_input_discriminator() {
        let command = AvailableCommand::new("search", "Search the codebase").input(Some(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("query to search")),
        ));
        let update = ee_agent_protocol::AvailableCommandsUpdate::new(vec![command]);
        let translated = translate_update(&SessionUpdate::AvailableCommandsUpdate(update));
        let v2::SessionUpdate::AvailableCommandsUpdate(update) = &translated[0] else {
            panic!("expected available commands update");
        };
        // The wire shape must carry the `type: "text"` discriminator.
        let wire = serde_json::to_value(&translated[0]).expect("update serializes");
        assert_eq!(wire["availableCommands"][0]["input"]["type"], "text");
        let v2::AvailableCommandInput::Text(input) =
            update.available_commands[0].input.as_ref().expect("input carried over")
        else {
            panic!("expected text command input");
        };
        assert_eq!(input.hint, "query to search");
    }

    #[test]
    fn plan_update_uses_provider_plan_id_and_v1_origin_keeps_plan_1() {
        let entries =
            vec![PlanEntry::new("step", PlanEntryPriority::High, PlanEntryStatus::Pending)];
        let provider_update = plan_update("plan-42", &entries);
        let v2::SessionUpdate::PlanUpdate(update) = &provider_update else {
            panic!("expected plan update");
        };
        let v2::PlanUpdateContent::Items(items) = &update.plan else {
            panic!("expected plan items");
        };
        assert_eq!(items.plan_id, v2::PlanId::new("plan-42"));
        assert_eq!(items.entries.len(), 1);

        // v1-origin plans stay on the session-stable id.
        let v1_origin = translate_update(&SessionUpdate::Plan(Plan::new(entries)));
        let v2::SessionUpdate::PlanUpdate(update) = &v1_origin[0] else {
            panic!("expected plan update");
        };
        let v2::PlanUpdateContent::Items(items) = &update.plan else {
            panic!("expected plan items");
        };
        assert_eq!(items.plan_id, v2::PlanId::new("plan-1"));
    }

    #[test]
    fn tool_call_content_chunk_translates_single_text_item() {
        let chunk = tool_call_content_chunk(
            &ee_agent_protocol::ToolCallId::new("tc-1"),
            &ToolCallContent::from(text_block("streamed")),
        )
        .expect("text item translates");
        let v2::SessionUpdate::ToolCallContentChunk(chunk) = chunk else {
            panic!("expected tool call content chunk");
        };
        assert_eq!(chunk.tool_call_id, v2::ToolCallId::new("tc-1"));
        assert!(matches!(chunk.content, v2::ToolCallContent::Content(_)));
    }

    #[test]
    fn git_patch_synthesizes_insertion_without_old_text() {
        let patch = synthesize_git_patch(std::path::Path::new("/tmp/new.txt"), None, "line one");
        assert!(patch.starts_with("--- a//tmp/new.txt\n+++ b//tmp/new.txt\n"));
        assert!(
            patch.contains("@@ -0,0 +1,1 @@"),
            "empty old text yields an insertion hunk: {patch}"
        );
        assert!(patch.contains("+line one"));
    }

    #[test]
    fn user_message_update_carries_agent_owned_message_id() {
        let update = user_message_update(&MessageId::new("msg_user_1"), &[text_block("hi")]);
        let v2::SessionUpdate::UserMessage(message) = update else {
            panic!("expected user message update");
        };
        assert_eq!(message.message_id, v2::MessageId::new("msg_user_1"));
        assert_eq!(message.content.value().expect("content present").len(), 1);
    }

    #[test]
    fn idle_state_carries_translated_stop_reason() {
        assert!(matches!(
            state_idle(Some(stop_reason_v2(V1StopReason::EndTurn))),
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate {
                stop_reason: Some(v2::StopReason::EndTurn),
                ..
            }))
        ));
        assert!(matches!(
            state_idle(Some(stop_reason_v2(V1StopReason::Cancelled))),
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate {
                stop_reason: Some(v2::StopReason::Cancelled),
                ..
            }))
        ));
        assert!(matches!(
            state_idle(None),
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate {
                stop_reason: None,
                ..
            }))
        ));
    }

    #[test]
    fn tool_call_content_diff_becomes_structured_change() {
        let diff = ee_agent_protocol::Diff::new("/tmp/a.txt", "new text").old_text("old text");
        let content = vec![ToolCallContent::Diff(diff)];
        let translated = translate_tool_call_contents(&content);
        let Some(v2::ToolCallContent::Diff(diff)) = translated.first() else {
            panic!("expected diff content");
        };
        assert_eq!(diff.changes.len(), 1);
        assert!(matches!(diff.changes[0].operation, v2::DiffChangeOperation::Modify(_)));
        assert_eq!(diff.changes[0].file_type, Some(v2::DiffFileType::Text));
        // The v1 old/new texts synthesize a renderable git patch.
        let patch = diff.patch.as_ref().expect("v1 diff carries a synthesized git patch");
        assert_eq!(patch.format, v2::DiffPatchFormat::GitPatch);
        assert!(patch.text.starts_with("--- a//tmp/a.txt\n+++ b//tmp/a.txt\n"));
        assert!(patch.text.contains("-old text"), "patch carries the old text: {}", patch.text);
        assert!(patch.text.contains("+new text"), "patch carries the new text: {}", patch.text);
    }

    #[test]
    fn config_options_rename_id_and_group_for_v2_wire() {
        let options = vec![
            serde_json::from_value(serde_json::json!({
                "id": "mode",
                "name": "Session Mode",
                "category": "mode",
                "type": "select",
                "currentValue": "ask",
                "options": [
                    { "value": "ask", "name": "Ask" },
                    { "value": "code", "name": "Code" },
                ],
            }))
            .expect("v1 config option parses"),
            serde_json::from_value(serde_json::json!({
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "m-1",
                "options": [
                    { "group": "g-1", "name": "Group", "options": [
                        { "value": "m-1", "name": "Model 1" },
                    ] },
                ],
            }))
            .expect("v1 grouped config option parses"),
        ];
        let translated = translate_config_options(&options);
        assert_eq!(translated.len(), 2);
        assert_eq!(translated[0].config_id, v2::SessionConfigId::new("mode"));
        assert_eq!(translated[0].name, "Session Mode");
        // The select-group identifier rename must survive the round trip.
        let value = serde_json::to_value(&translated[1]).expect("serializes");
        let group = &value["options"][0];
        assert!(group.get("groupId").is_some(), "group renamed to groupId: {group}");
        assert!(group.get("group").is_none(), "v1 key must not leak: {group}");
    }
}
