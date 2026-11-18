//! Small loop helpers: workspace-instruction detection and repeated-fetch
//! compaction.

use super::*;

/// citation evidence without injecting the same untrusted body again.
/// Tools whose file access may reveal nested workspace instructions.
pub(super) fn touches_workspace_instructions(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file" | "write_file" | "edit_file" | "ee_edit_apply" | "ee_file_outline"
    ) || tool_name.ends_with("_edit_apply")
        || tool_name.ends_with("_file_outline")
        || tool_name.ends_with("_read_file")
        || tool_name.ends_with("_write_file")
}

pub(super) fn compact_repeated_web_fetch(
    intent: &ToolIntent,
    result: ToolResult,
    observed_sources: &mut HashSet<String>,
) -> ToolResult {
    if intent.name != "ee_fetch_url" || !result.success {
        return result;
    }
    let value = result
        .structured_output
        .as_ref()
        .cloned()
        .or_else(|| serde_json::from_str::<serde_json::Value>(&result.text_output).ok());
    let Some(value) = value else { return result };
    let Some(url) = value.get("url").and_then(serde_json::Value::as_str) else { return result };
    let Some(sha256) = value.get("sha256").and_then(serde_json::Value::as_str) else {
        return result;
    };
    if value.get("trust").and_then(serde_json::Value::as_str) != Some("untrusted_external_content")
    {
        return result;
    }
    let source_id = format!("{url}#{sha256}");
    if observed_sources.insert(source_id.clone()) {
        return result;
    }
    let source = serde_json::json!({
        "url": url,
        "sha256": sha256,
        "retrieved_at": value.get("retrieved_at").cloned().unwrap_or(serde_json::Value::Null),
        "cached": value.get("cached").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "truncated": value.get("truncated").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "trust": "untrusted_external_content",
        "content_reused": true,
    });
    ToolResult::success_structured(
        format!("untrusted external source reused: {source_id}; refer to earlier fetched evidence"),
        source,
    )
}
