//! Small loop helpers: workspace-instruction detection and repeated-fetch
//! compaction.

use super::*;

/// citation evidence without injecting the same untrusted body again.
/// ee-manifest tools whose file access may reveal nested workspace instructions.
///
/// Keep every entry in sync with [`ee_mcp::STABLE_TOOL_NAMES`]; the
/// `classified_workspace_instruction_tools_exist_in_ee_manifest` test fails on drift.
pub(super) const WORKSPACE_INSTRUCTION_TOOL_NAMES: &[&str] = &[
    "ee_read_text_file",
    "ee_write_text_file",
    "ee_apply_patch",
    "ee_replace_text",
    "ee_document_symbols",
];

/// Bridge-manifest tools whose file access may reveal nested instructions.
const WORKSPACE_INSTRUCTION_BRIDGE_NAMES: &[&str] = &["read_file", "write_file", "edit_file"];

/// Namespaced MCP spellings, for example `server__ee_document_symbols`.
const WORKSPACE_INSTRUCTION_TOOL_SUFFIXES: &[&str] = &[
    "_apply_patch",
    "_replace_text",
    "_document_symbols",
    "_read_text_file",
    "_write_text_file",
    "_read_file",
    "_write_file",
];

/// Tools whose file access may reveal nested workspace instructions.
pub(super) fn touches_workspace_instructions(tool_name: &str) -> bool {
    WORKSPACE_INSTRUCTION_TOOL_NAMES.contains(&tool_name)
        || WORKSPACE_INSTRUCTION_BRIDGE_NAMES.contains(&tool_name)
        || WORKSPACE_INSTRUCTION_TOOL_SUFFIXES.iter().any(|suffix| tool_name.ends_with(suffix))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: every classified ee tool must exist in the canonical proxy
    /// manifest, so a rename cannot silently stop instruction discovery again.
    #[test]
    fn classified_workspace_instruction_tools_exist_in_ee_manifest() {
        for name in WORKSPACE_INSTRUCTION_TOOL_NAMES {
            assert!(name.starts_with("ee_"), "ee classification must stay namespaced: {name}");
            assert!(
                ee_mcp::STABLE_TOOL_NAMES.contains(name),
                "classified tool {name} is missing from the ee tool manifest"
            );
            assert!(touches_workspace_instructions(name), "{name}");
        }
        for name in WORKSPACE_INSTRUCTION_BRIDGE_NAMES {
            assert!(
                !name.starts_with("ee_"),
                "bridge classification must stay unnamespaced: {name}"
            );
            assert!(touches_workspace_instructions(name), "{name}");
        }
    }
}
