//! Display formatting, slash-command parsing, and local command tables.

use std::time::Duration;

use ee_agent_host::ToolCallState;
use ee_agent_host::events::{AgentConnectionState, TurnMetrics};
use ee_agent_protocol::{
    AvailableCommand, ContentBlock, PlanEntryPriority, PlanEntryStatus, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionValue, SessionConfigSelectOptions,
    SessionConfigValueId, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolKind,
};

use super::thread_ui::{
    AgentContextFile, AgentThreadUi, MessageRenderKind, ThreadUiState, TranscriptItem,
};

// ── Rendering helpers ────────────────────────────────────────────────────────

/// Extracts display text from a content block (non-text blocks get markers).
pub(super) fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => String::from("[image]"),
        ContentBlock::Audio(_) => String::from("[audio]"),
        ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
        ContentBlock::Resource(resource) => {
            let uri = match &resource.resource {
                ee_agent_protocol::EmbeddedResourceResource::TextResourceContents(contents) => {
                    contents.uri.clone()
                }
                ee_agent_protocol::EmbeddedResourceResource::BlobResourceContents(contents) => {
                    contents.uri.clone()
                }
                // Non-exhaustive upstream.
                _ => String::from("(unknown)"),
            };
            format!("[resource: {uri}]")
        }
        // Non-exhaustive upstream; unknown blocks render as a marker.
        _ => String::from("[content]"),
    }
}

pub(super) fn tool_call_status_label(status: ToolCallStatus) -> String {
    match status {
        ToolCallStatus::Pending => String::from("pending"),
        ToolCallStatus::InProgress => String::from("in_progress"),
        ToolCallStatus::Completed => String::from("completed"),
        ToolCallStatus::Failed => String::from("failed"),
        // Non-exhaustive upstream.
        _ => String::from("?"),
    }
}

pub(super) fn tool_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
        _ => "other",
    }
}

pub(super) fn tool_call_content_summary(content: &ToolCallContent) -> String {
    match content {
        ToolCallContent::Content(content) => content_block_text(&content.content),
        ToolCallContent::Diff(diff) => {
            let path = diff.path.display();
            if diff.old_text.is_some() {
                format!("diff: {path}")
            } else {
                format!("diff: new file {path}")
            }
        }
        ToolCallContent::Terminal(terminal) => {
            format!("terminal: {}", terminal.terminal_id.0)
        }
        _ => String::from("content: [unknown]"),
    }
}

pub(super) fn tool_call_location_summary(location: &ToolCallLocation) -> String {
    match location.line {
        Some(line) => format!("{}:{line}", location.path.display()),
        None => location.path.display().to_string(),
    }
}

pub(super) fn tool_call_detail_from_state(tool_call: &ToolCallState) -> String {
    if tool_call.kind == ToolKind::Fetch {
        // ACP may carry remote request/response bytes in generic content or raw
        // fields. Keep lifecycle visible without treating that content as local
        // transcript, planner, or export data.
        return String::from(
            "kind: fetch · external content: untrusted · remote payload withheld · use source provenance from tool result",
        );
    }
    let mut sections = vec![format!("kind: {}", tool_kind_label(tool_call.kind))];

    if !tool_call.content.is_empty() {
        let content = tool_call
            .content
            .iter()
            .map(tool_call_content_summary)
            .filter(|item| !item.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        if !content.is_empty() {
            sections.push(format!("content: {content}"));
        }
    }

    if !tool_call.locations.is_empty() {
        let locations = tool_call
            .locations
            .iter()
            .map(tool_call_location_summary)
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!("locations: {locations}"));
    }

    match (tool_call.raw_input.is_some(), tool_call.raw_output.is_some()) {
        (true, true) => sections.push(String::from("diagnostics: raw input/output captured")),
        (true, false) => sections.push(String::from("diagnostics: raw input captured")),
        (false, true) => sections.push(String::from("diagnostics: raw output captured")),
        (false, false) => {}
    }

    sections.join(" · ")
}

pub(super) fn plan_entry_marker(status: PlanEntryStatus) -> char {
    match status {
        PlanEntryStatus::Pending => '-',
        PlanEntryStatus::InProgress => '>',
        PlanEntryStatus::Completed => 'x',
        // Non-exhaustive upstream.
        _ => '!',
    }
}

pub(super) fn plan_entry_priority_label(priority: PlanEntryPriority) -> &'static str {
    match priority {
        PlanEntryPriority::High => "high",
        PlanEntryPriority::Medium => "medium",
        PlanEntryPriority::Low => "low",
        _ => "?",
    }
}

pub(super) fn thread_display_name(
    index: usize,
    agent_id: &str,
    session_name: Option<&str>,
    session_title: Option<&str>,
) -> String {
    let label =
        session_name.or(session_title).filter(|title| !title.trim().is_empty()).unwrap_or(agent_id);
    format!("{}.{}", index + 1, label)
}

/// Formats a duration compactly: `1.2s`, `45s`, or `3m 12s`.
pub(crate) fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs_f64();
    if total < 60.0 {
        return format!("{total:.1}s");
    }
    let minutes = total as u64 / 60;
    let seconds = total as u64 % 60;
    format!("{minutes}m {seconds}s")
}

/// Formats a token count with thousands separators (`8,431`).
pub(crate) fn format_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Renders one completed turn's metrics: `12.4s` or
/// `12.4s · 8,431 tokens (6,120 in / 2,311 out)`; unknown token usage is
/// never shown as zero.
pub(crate) fn turn_metrics_label(metrics: &TurnMetrics) -> String {
    let mut label = format_duration(metrics.elapsed);
    if let Some(usage) = &metrics.tokens {
        label.push_str(&format!(
            " · {} tokens ({} in / {} out)",
            format_tokens(usage.total_tokens),
            format_tokens(usage.input_tokens),
            format_tokens(usage.output_tokens),
        ));
    }
    label
}

pub(super) fn is_mode_config_option(option: &SessionConfigOption) -> bool {
    matches!(option.category, Some(ee_agent_protocol::SessionConfigOptionCategory::Mode))
}

pub(super) fn select_option_values(
    options: &SessionConfigSelectOptions,
) -> Vec<SessionConfigValueId> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().map(|option| option.value.clone()).collect()
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|option| option.value.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn cycle_select_value(
    options: &SessionConfigSelectOptions,
    current: &SessionConfigValueId,
    delta: isize,
) -> Option<SessionConfigValueId> {
    let values = select_option_values(options);
    if values.is_empty() {
        return None;
    }
    let current_index = values.iter().position(|value| value == current).unwrap_or_default();
    let next_index = (current_index as isize + delta).rem_euclid(values.len() as isize) as usize;
    values.get(next_index).cloned()
}

pub(super) fn config_option_summary(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => format!("{}={}", option.id.0, select.current_value.0),
        SessionConfigKind::Boolean(current) => {
            format!("{}={}", option.id.0, if current.current_value { "on" } else { "off" })
        }
        _ => format!("{}=?", option.id.0),
    }
}

pub(super) fn parse_config_option_value(
    option: &SessionConfigOption,
    raw_value: &str,
) -> Result<SessionConfigOptionValue, String> {
    match &option.kind {
        SessionConfigKind::Select(select) => {
            let value = SessionConfigValueId::new(raw_value);
            let exists = match &select.options {
                SessionConfigSelectOptions::Ungrouped(options) => {
                    options.iter().any(|option| option.value == value)
                }
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .any(|option| option.value == value),
                _ => false,
            };
            if exists {
                Ok(SessionConfigOptionValue::value_id(value))
            } else {
                Err(format!("invalid value for {}: {raw_value}", option.id.0))
            }
        }
        SessionConfigKind::Boolean(_) => parse_bool(raw_value)
            .map(SessionConfigOptionValue::boolean)
            .ok_or_else(|| format!("invalid boolean for {}: {raw_value}", option.id.0)),
        _ => Err(format!("unsupported config option kind: {}", option.id.0)),
    }
}

pub(super) fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub(super) fn split_slash_command(draft: &str) -> (Option<String>, String) {
    let trimmed = draft.trim_start();
    if !trimmed.starts_with('/') {
        return (None, String::new());
    }
    let without_slash = &trimmed[1..];
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let name = parts.next().filter(|part| !part.is_empty()).map(str::to_string);
    let rest = parts.next().unwrap_or_default().to_string();
    (name, rest)
}

pub(super) fn queue_command_is_management(args: &str) -> bool {
    matches!(
        args.split_whitespace().next(),
        None | Some("list" | "edit" | "move" | "remove" | "clear")
    )
}

pub(super) fn prompt_blocks_with_context(
    prompt_text: &str,
    context_files: &[AgentContextFile],
    next_prompt_context_files: &[AgentContextFile],
) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text(TextContent::new(prompt_text))];
    for (scope, files) in [
        ("User-selected context file", context_files),
        ("One-turn user mention", next_prompt_context_files),
    ] {
        blocks.extend(files.iter().map(|file| {
            ContentBlock::Text(TextContent::new(format!(
                "{scope}: `{}`\n--- file snapshot ---\n{}\n--- end file snapshot ---",
                file.relative_path, file.content
            )))
        }));
    }
    blocks
}

pub(super) fn agent_mode_ids(thread: &AgentThreadUi) -> Vec<String> {
    let snapshot = thread.host.snapshot();
    if let Some(mode_option) =
        snapshot.config_options.iter().find(|option| is_mode_config_option(option))
        && let SessionConfigKind::Select(select) = &mode_option.kind
    {
        return select_option_values(&select.options)
            .into_iter()
            .map(|value| value.0.to_string())
            .collect();
    }
    thread
        .host
        .advertised_modes()
        .map(|modes| modes.available_modes.into_iter().map(|mode| mode.id.0.to_string()).collect())
        .unwrap_or_default()
}

pub(super) const LOCAL_AGENT_SLASH_COMMANDS: &[&str] = &[
    "quit",
    "q",
    "quit_full",
    "qf",
    "help",
    "status",
    "doctor",
    "init",
    "review",
    "security-review",
    "diff",
    "copy",
    "rename",
    "new",
    "new_thread",
    "archive",
    "delete",
    "fork",
    "branch",
    "sessions",
    "export",
    "stop",
    "steer",
    "resume",
    "discard",
    "reconnect",
    "next",
    "prev",
    "clear",
    "layout",
    "thoughts",
    "config",
    "mcp",
    "memory",
    "approval",
    "permissions",
    "context",
    "mention",
    "add-dir",
    "tasks",
    "ps",
    "mode",
    "queue",
    "details",
    "transcript",
    "draft",
    "keys",
];

/// Provider-owned configuration aliases accepted only when the active session
/// explicitly advertises an ACP option with the matching id.
pub(super) const PROVIDER_CONFIG_ALIASES: &[&str] = &["model", "effort", "fast", "personality"];

/// Provider-owned workflow commands. EE does not emulate these; an unadvertised
/// command stops locally with guidance instead of becoming an ordinary prompt.
pub(super) const PROVIDER_OWNED_SLASH_COMMANDS: &[&str] = &[
    "compact",
    "subtask",
    "background",
    "side",
    "btw",
    "skills",
    "plugins",
    "hooks",
    "usage",
    "billing",
    "cloud",
    "remote-control",
    "web-search",
    "app",
];

pub(super) const LOCAL_AGENT_SLASH_HELP: &[(&str, &str)] = &[
    ("/help", "show local commands and provider commands"),
    ("/status", "show local session state"),
    ("/doctor", "read-only Agents TUI health report"),
    ("/init", "ask agent to preview safe AGENTS.md scaffold"),
    ("/review [target]", "send bounded EE evidence for agent-generated review"),
    ("/security-review [target]", "send bounded EE evidence for security review"),
    ("/diff", "open bounded workspace diff"),
    ("/copy [N]", "copy Nth completed assistant response"),
    ("/rename <name>", "set persisted local session name"),
    ("/new, /new_thread", "start a fresh provider session"),
    ("/clear", "clear visible local scrollback; provider context stays"),
    ("/archive", "hide idle local session; use /archive list|restore <N>"),
    ("/delete", "confirm local transcript deletion; provider session stays"),
    ("/fork", "create non-active seeded session from redacted visible transcript"),
    ("/branch", "create and switch to seeded session"),
    ("/sessions", "switch session"),
    ("/export", "write redacted Markdown transcript"),
    ("/stop [terminal-id|all]", "cancel turn or stop owned direct-child terminal"),
    ("/steer <message>", "cancel active turn; run steer message next"),
    ("/resume | /discard", "resolve paused turn"),
    ("/reconnect", "reconnect persisted session"),
    ("/next | /prev", "cycle active sessions"),
    ("/layout", "right|bottom|full"),
    ("/thoughts", "on|off|toggle"),
    ("/config", "show or change advertised options"),
    ("/mcp", "show local MCP state"),
    (
        "/memory enable|disable [--delete]|status|list|search <query>|show <key>|forget <key>|retract <key>|clear|export [--with-values]|import <path>",
        "persist local canonical-workspace memory policy; deletion and mutations confirm once",
    ),
    ("/approval", "default|autopilot|bypass; bypass keeps validation"),
    (
        "/permissions",
        "list|inspect|disable|enable|revoke|reload|reset|test|preview host-local policy",
    ),
    ("/context", "list|status|add|remove|clear session snapshots"),
    ("/mention <path>", "attach redacted file snapshot to next prompt only"),
    ("/add-dir <path>", "confirm extra root for capable agent sessions"),
    ("/tasks | /ps", "list owned background terminals; subagent tasks when supported"),
    ("/mode", "select agent-advertised mode"),
    ("/queue <message>", "run message after current turn; /queue list manages follow-ups"),
    ("/details", "on|off|toggle sanitized transcript tool detail"),
    ("/transcript", "raw|grouped|toggle|open|export local transcript"),
    ("/draft", "restore|clear long prompt draft; Ctrl-S stash, Ctrl-Shift-E edit"),
    ("/keys", "show Agents TUI keyboard shortcuts"),
    ("/quit, /q", "close Agents pane"),
    ("/quit_full, /qf", "exit EE"),
];

pub(super) fn owned_terminal_summary_line(
    summary: &crate::app::agent_bridge::OwnedTerminalSummary,
    secrets: &[String],
) -> String {
    let command = if summary.args.is_empty() {
        summary.command.clone()
    } else {
        format!("{} {}", summary.command, summary.args.join(" "))
    };
    let state = if summary.running { "running" } else { "exited" };
    let cwd = summary
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| String::from("(default)"));
    let tail = ee_agent_host::redact::redact_secret_values(&summary.output_tail, secrets);
    let truncation = if summary.output_truncated { " truncated" } else { "" };
    format!(
        "{} {state} {} · cwd:{} · output:{} bytes{truncation}{}",
        summary.terminal_id,
        command,
        cwd,
        summary.output_total_bytes,
        if tail.trim().is_empty() { String::new() } else { format!("\n  tail: {tail}") },
    )
}

pub(super) fn agent_connection_state_label(state: AgentConnectionState) -> &'static str {
    match state {
        AgentConnectionState::Starting => "starting",
        AgentConnectionState::Initializing => "initializing",
        AgentConnectionState::Ready { .. } => "connected",
        AgentConnectionState::Failed(_) => "failed",
        AgentConnectionState::Closed(_) => "closed",
    }
}

pub(super) fn thread_state_label(state: ThreadUiState) -> &'static str {
    match state {
        ThreadUiState::Starting => "starting",
        ThreadUiState::Ready => "ready",
        ThreadUiState::Queued => "queued",
        ThreadUiState::Running => "running",
        ThreadUiState::AwaitingPermission => "awaiting permission",
        ThreadUiState::AwaitingElicitation => "awaiting elicitation",
        ThreadUiState::Cancelling => "cancelling",
        ThreadUiState::PausedRecoverable => "paused",
        ThreadUiState::Closed => "closed",
        ThreadUiState::Failed => "failed",
    }
}

pub(super) fn fork_seed(thread: &AgentThreadUi, secrets: &[String]) -> Vec<ContentBlock> {
    const FORK_SEED_MAX_BYTES: usize = 48 * 1024;
    let mut transcript = String::from(
        "This is a locally seeded fork. Treat following redacted visible messages as prior context; it is not provider-side session cloning.\n\n",
    );
    for item in &thread.transcript {
        let TranscriptItem::Message { nick, text, kind, .. } = item else { continue };
        let role = match kind {
            MessageRenderKind::User => "User",
            MessageRenderKind::Assistant => "Assistant",
            MessageRenderKind::Thought => continue,
        };
        transcript.push_str(&format!("## {role} ({nick})\n{text}\n\n"));
    }
    let mut transcript = ee_agent_host::redact::redact_secret_values(&transcript, secrets);
    if transcript.len() > FORK_SEED_MAX_BYTES {
        let mut end = FORK_SEED_MAX_BYTES;
        while !transcript.is_char_boundary(end) {
            end -= 1;
        }
        transcript.truncate(end);
        transcript.push_str("\n\n[local fork seed truncated]\n");
    }
    vec![ContentBlock::Text(TextContent::new(transcript))]
}

pub(super) fn sanitize_session_name(raw: &str) -> Option<String> {
    const MAX_SESSION_NAME_CHARS: usize = 80;
    let mut name = raw
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{00AD}'
                        | '\u{034F}'
                        | '\u{061C}'
                        | '\u{115F}'..='\u{1160}'
                        | '\u{17B4}'..='\u{17B5}'
                        | '\u{180E}'
                        | '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2060}'..='\u{206F}'
                        | '\u{3164}'
                        | '\u{FE00}'..='\u{FE0F}'
                        | '\u{FEFF}'
                        | '\u{FFA0}'
                )
        })
        .collect::<String>();
    name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then(|| name.chars().take(MAX_SESSION_NAME_CHARS).collect())
}

/// Lists local and agent-advertised slash commands without duplicate names.
pub(crate) fn agent_slash_command_names(
    commands: &[AvailableCommand],
    external_rubber_duck: bool,
) -> Vec<&str> {
    let mut names = LOCAL_AGENT_SLASH_COMMANDS.to_vec();
    if external_rubber_duck {
        names.push("rubber-duck");
    }
    for command in commands {
        let name = command.name.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Draft text for a cycled command, preserving any trailing user text.
pub(super) fn critic_context_revision(context: &str) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(context.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2 + 7);
    hex.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub(super) fn external_critic_cost_micros(usage: Option<&serde_json::Value>) -> Option<u64> {
    usage
        .and_then(|value| value.get("cost"))
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| (value * 1_000_000.0).round())
        .filter(|value| *value <= u64::MAX as f64)
        .map(|value| value as u64)
}

pub(super) fn slash_command_draft(command_name: &str, rest: &str) -> String {
    if rest.trim().is_empty() {
        format!("/{command_name}")
    } else {
        format!("/{command_name} {}", rest.trim_start())
    }
}

pub(super) fn is_agents_quit_slash_command(draft: &str) -> bool {
    let (name, rest) = split_slash_command(draft);
    matches!(name.as_deref(), Some("q" | "quit")) && rest.trim().is_empty()
}

pub(super) fn is_agents_quit_full_slash_command(draft: &str) -> bool {
    let (name, rest) = split_slash_command(draft);
    matches!(name.as_deref(), Some("qf" | "quit_full")) && rest.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_markers_match_issue_contract() {
        assert_eq!(plan_entry_marker(PlanEntryStatus::Pending), '-');
        assert_eq!(plan_entry_marker(PlanEntryStatus::InProgress), '>');
        assert_eq!(plan_entry_marker(PlanEntryStatus::Completed), 'x');
    }

    #[test]
    fn queue_management_forms_do_not_consume_prompt_messages() {
        assert!(queue_command_is_management(""));
        assert!(queue_command_is_management("list"));
        assert!(queue_command_is_management("edit 1 revised prompt"));
        assert!(queue_command_is_management("move 2 1"));
        assert!(queue_command_is_management("remove 1"));
        assert!(queue_command_is_management("clear"));
        assert!(!queue_command_is_management("review changed files"));
    }

    fn compact_command() -> AvailableCommand {
        AvailableCommand::new("compact", "Summarize the session history").input(
            ee_agent_protocol::AvailableCommandInput::Unstructured(
                ee_agent_protocol::UnstructuredCommandInput::new("optional instructions"),
            ),
        )
    }

    #[test]
    fn split_slash_command_parses_only_leading_slashes() {
        assert_eq!(split_slash_command("hello world"), (None, String::new()));
        assert_eq!(split_slash_command(""), (None, String::new()));
        assert_eq!(split_slash_command("/compact"), (Some(String::from("compact")), String::new()));
        assert_eq!(
            split_slash_command("/compact focus on auth"),
            (Some(String::from("compact")), String::from("focus on auth"))
        );
        assert_eq!(
            split_slash_command("/compactness"),
            (Some(String::from("compactness")), String::new())
        );
    }

    #[test]
    fn slash_command_draft_inserts_only_the_command_name() {
        assert_eq!(slash_command_draft("compact", ""), "/compact");
        assert_eq!(slash_command_draft("compact", "  keep API v2 "), "/compact keep API v2 ");
    }

    #[test]
    fn agent_slash_command_names_include_local_and_advertised_commands() {
        assert_eq!(
            agent_slash_command_names(
                &[compact_command(), AvailableCommand::new("quit", "")],
                false,
            ),
            vec![
                "quit",
                "q",
                "quit_full",
                "qf",
                "help",
                "status",
                "doctor",
                "init",
                "review",
                "security-review",
                "diff",
                "copy",
                "rename",
                "new",
                "new_thread",
                "archive",
                "delete",
                "fork",
                "branch",
                "sessions",
                "export",
                "stop",
                "steer",
                "resume",
                "discard",
                "reconnect",
                "next",
                "prev",
                "clear",
                "layout",
                "thoughts",
                "config",
                "mcp",
                "memory",
                "approval",
                "permissions",
                "context",
                "mention",
                "add-dir",
                "tasks",
                "ps",
                "mode",
                "queue",
                "details",
                "transcript",
                "draft",
                "keys",
                "compact",
            ]
        );
    }

    #[test]
    fn local_slash_command_aliases_are_recognized() {
        assert!(is_agents_quit_slash_command("/q"));
        assert!(is_agents_quit_slash_command("/quit"));
        assert!(is_agents_quit_full_slash_command("/qf"));
        assert!(is_agents_quit_full_slash_command("/quit_full"));
        assert!(!is_agents_quit_full_slash_command("/qf now"));
    }
}
