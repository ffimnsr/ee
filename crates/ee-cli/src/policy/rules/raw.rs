//! Raw persisted rule shapes.
use super::validate::is_zero;
use super::*;

// ── Raw TOML forms (canonical field order, deny_unknown_fields) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCommandRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) executable: String,
    #[serde(rename = "match")]
    pub(crate) match_mode: MatchMode,
    pub(crate) argv: Vec<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) category: Option<TrustCategory>,
    pub(crate) arguments_json: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawReadPathRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) path_prefix: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpReadRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) path_prefix: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpReadProfileRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) profile: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfileRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) profile: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawWriteRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) operation: WriteOperationKind,
    pub(crate) path_prefix: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_files: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_total_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_file_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawNetworkRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) scheme: NetworkScheme,
    pub(crate) host: String,
    pub(crate) host_match: HostMatchMode,
    pub(crate) port: u16,
    pub(crate) method: NetworkMethodClass,
    pub(crate) browser_action: BrowserActionClass,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawFilesystemRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) operations: Vec<FilesystemOperationKind>,
    pub(crate) path_prefix: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawToolRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) native_tool: Option<String>,
    pub(crate) server: Option<String>,
    pub(crate) transport_identity: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) tool_schema_version: Option<u64>,
    pub(crate) category: Option<TrustCategory>,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}
