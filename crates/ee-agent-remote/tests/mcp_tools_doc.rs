//! Drift test for `docs/MCP_TOOLS.md`.
//!
//! Fails when a tool is added, removed, its approval policy changes, or the
//! read-only mode exception list drifts without the matching documentation
//! update. Both in-repo MCP servers are covered: the stable `ee_` editor proxy
//! and the `ee-agent` remote-control server.

use std::collections::{BTreeMap, BTreeSet};

use ee_agent_remote::RemoteMcpServer;

const DOC: &str = include_str!("../../../docs/MCP_TOOLS.md");

const PROXY_START: &str = "<!-- mcp-tools:proxy:start -->";
const PROXY_END: &str = "<!-- mcp-tools:proxy:end -->";
const REMOTE_START: &str = "<!-- mcp-tools:remote:start -->";
const REMOTE_END: &str = "<!-- mcp-tools:remote:end -->";
const MODES_START: &str = "<!-- mcp-tools:modes:start -->";
const MODES_END: &str = "<!-- mcp-tools:modes:end -->";

fn section(start: &str, end: &str) -> &'static str {
    let start_index = DOC.find(start).unwrap_or_else(|| panic!("missing marker {start}"));
    let after_start = start_index + start.len();
    let end_offset = DOC[after_start..]
        .find(end)
        .unwrap_or_else(|| panic!("missing marker {end} after {start}"));
    &DOC[after_start..after_start + end_offset]
}

/// Parses canonical tool-matrix rows: ``| `ee_name` | ... |``.
fn tool_rows(section: &'static str) -> Vec<Vec<&'static str>> {
    section
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("| `ee_"))
        .map(|line| line.trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>())
        .collect()
}

fn documented_tools(section: &'static str) -> BTreeMap<&'static str, Vec<&'static str>> {
    let rows = tool_rows(section);
    let mut tools = BTreeMap::new();
    for row in rows {
        assert!(row.len() >= 8, "row has {} cells, expected at least 8: {row:?}", row.len());
        let name = row[0].trim_matches('`');
        assert!(tools.insert(name, row).is_none(), "tool documented more than once: {name}");
    }
    tools
}

#[test]
fn proxy_tool_matrix_matches_stable_contract() {
    let documented = documented_tools(section(PROXY_START, PROXY_END));
    let mut expected: Vec<&str> = ee_mcp::STABLE_TOOL_NAMES.to_vec();
    expected.sort_unstable();

    assert_eq!(
        documented.keys().copied().collect::<Vec<_>>(),
        expected,
        "documented proxy tools differ from ee_mcp::STABLE_TOOL_NAMES; \
         add or remove the table row in docs/MCP_TOOLS.md"
    );

    for (name, row) in &documented {
        let governance = ee_mcp::governance(name)
            .unwrap_or_else(|| panic!("documented tool has no governance: {name}"));

        assert_eq!(row[1], governance.side_effect.as_str(), "side-effect drift for {name}");
        assert_eq!(row[2], governance.approval, "approval drift for {name}");

        let transports = governance
            .transports
            .iter()
            .map(|transport| transport.as_str())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(row[3], transports, "transport drift for {name}");

        let capabilities = if governance.required_capabilities.is_empty() {
            "—".to_string()
        } else {
            governance.required_capabilities.join(",")
        };
        assert_eq!(row[4], capabilities, "capability drift for {name}");

        let cap = format!("{}:{}", governance.output_cap_kind, governance.output_cap);
        assert_eq!(row[5], cap, "output-cap drift for {name}");
    }
}

#[test]
fn remote_tool_matrix_matches_advertised_tools() {
    let documented = documented_tools(section(REMOTE_START, REMOTE_END));
    let mut expected: Vec<&str> = RemoteMcpServer::tool_names().to_vec();
    expected.sort_unstable();

    assert_eq!(
        documented.keys().copied().collect::<Vec<_>>(),
        expected,
        "documented remote tools differ from RemoteMcpServer::tool_names(); \
         add or remove the table row in docs/MCP_TOOLS.md"
    );
}

#[test]
fn read_only_mode_exceptions_match_knowledge_write_contract() {
    let modes = section(MODES_START, MODES_END);
    let mut blocks: Vec<String> = Vec::new();
    let mut block = String::new();
    for line in modes.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("- ") && !block.is_empty() {
            blocks.push(std::mem::take(&mut block));
        }
        if !block.is_empty() {
            block.push(' ');
        }
        block.push_str(line);
    }
    if !block.is_empty() {
        blocks.push(block);
    }

    let mut documented = BTreeSet::new();
    for block in blocks.iter().filter(|block| block.contains("stay dispatchable")) {
        for part in block.split('`').skip(1).step_by(2) {
            if part.starts_with("ee_") {
                documented.insert(part);
            }
        }
    }

    let expected: BTreeSet<&str> =
        ee_mcp::AGENT_KNOWLEDGE_WRITE_TOOL_NAMES.iter().copied().collect();
    assert_eq!(
        documented, expected,
        "docs/MCP_TOOLS.md read-only mode exceptions differ from \
         ee_mcp::AGENT_KNOWLEDGE_WRITE_TOOL_NAMES"
    );
    assert!(modes.contains("ee_import_workspace_memory"));
    assert!(modes.contains("ee_clear_workspace_memory"));
}

#[test]
fn documented_manifest_version_matches_constant() {
    let version = format!("schema version `{}`", ee_mcp::EE_TOOL_SCHEMA_VERSION);
    assert!(
        DOC.contains(&version),
        "docs/MCP_TOOLS.md is missing the current manifest marker: {version}"
    );
}
