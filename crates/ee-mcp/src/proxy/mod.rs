//! In-process MCP server surface ("ee MCP proxy").
//!
//! The proxy exposes ee editor operations as MCP tools over protocol
//! `2026-07-28` only. Tool execution is delegated to a caller-provided
//! [`EeProxyBackend`] (the editor host implements it; this crate stays
//! UI-free). Tools are namespaced under the server id `ee`, so every tool
//! name literally starts with `ee.`.
//!
//! Wire handling is entirely rmcp's ([`rmcp::ServerHandler`]); ee-owned code
//! here is limited to the tool surface, argument validation, and the
//! fail-closed protocol-version pin.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
    ErrorCode, ErrorData, Implementation, InitializeRequestParams, InitializeResult, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, Tool,
    ToolAnnotations,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
use serde_json::json;

/// The single protocol version this server implements.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];

// Internal module layout:
// - `backend`: the [`EeProxyBackend`] trait (host contract).
// - `types`: DTOs and size caps shared across tool dispatch.
// - `errors`: [`ProxyToolError`] / [`WebToolError`] types.
// - `all_tools` / `dispatch`: fixed tool list and per-call validation+dispatch.
mod all_tools;
mod backend;
mod dispatch;
mod errors;
mod types;

#[cfg(test)]
mod tests;

pub use backend::EeProxyBackend;
pub use errors::{ProxyToolError, WebToolError, WebToolErrorCode};
pub use types::{
    BrowserRunAction, BrowserRunRequest, BrowserRunResult, ChangedFileEntry, ChangedFilesResult,
    CodeActionEntry, CodeActionsResult, DiagnosticEntry, DiagnosticsResult, DirectoryEntry,
    DirectoryEntryAll, DocumentSymbolEntry, DocumentSymbolsResult, EditTextResult, FetchUrlRequest,
    FetchUrlResult, FileDependencyEdge, FileDependencyMapResult, FileMatch, FilesystemResult,
    GitDiffResult, GitStatusResult, ListDirectoryAllResult, ListDirectoryResult,
    MAX_TOOL_ARGUMENT_BYTES, MAX_WORKSPACE_FACT_KEY_BYTES, MAX_WORKSPACE_FACT_LIST_RESULTS,
    MAX_WORKSPACE_FACT_QUERY_BYTES, MAX_WORKSPACE_FACT_VALUE_BYTES,
    MAX_WORKSPACE_MEMORY_IMPORT_BYTES, OpenBufferEntry, OpenBuffersResult, PlannedFileEdit,
    PlannedTextEdit, ProjectInstructionSource, ProjectInstructionsResult, ReferenceEntry,
    ReferencesResult, RenamePreviewResult, ReviewContextResult, SearchFilesAllResult,
    SearchFilesResult, SearchTextResult, SessionNoteResult, SessionNotesResult,
    SymbolDependencyLocation, SymbolDependencyMapResult, SymbolDependencyModuleHint,
    SymbolDependencyRelatedFile, SymbolDependencyRelation, SymbolDependencyTotals,
    TerminalOutputChunk, TerminalOutputResult, TerminalWaitResult, TextEdit, TextMatch, TextRange,
    ToolManifestEntry, ToolOutputCap, ToolsManifestResult, WebSearchEntry, WebSearchRequest,
    WebSearchResult, WorkspaceEditResult, WorkspaceFact, WorkspaceFactMutationResult,
    WorkspaceFactProvenance, WorkspaceFactsResult, WorkspaceRootsResult,
};

/// An in-process MCP server exposing ee editor operations as MCP tools.
///
/// The server speaks MCP `2026-07-28` only. Stable names start with `ee_`.
/// Tool execution is delegated to the [`EeProxyBackend`] supplied at construction.
pub struct EeMcpProxy {
    backend: Arc<dyn EeProxyBackend>,
    supported_tools: Option<BTreeSet<String>>,
}

impl EeMcpProxy {
    /// Creates a proxy delegating tool execution to `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn EeProxyBackend>) -> Self {
        let supported_tools = backend.supported_tools().map(|tools| tools.into_iter().collect());
        Self { backend, supported_tools }
    }

    /// Creates a proxy with an exact host-supported profile. The manifest tool is always available.
    #[must_use]
    pub fn with_supported_tools(backend: Arc<dyn EeProxyBackend>, tools: Vec<String>) -> Self {
        Self { backend, supported_tools: Some(tools.into_iter().collect()) }
    }

    fn is_supported(&self, name: &str) -> bool {
        name == "ee_tools_manifest"
            || self.supported_tools.as_ref().is_none_or(|tools| tools.contains(name))
    }

    fn tools(&self) -> Vec<Tool> {
        Self::all_tools()
            .into_iter()
            .filter(|tool| crate::governance(tool.name.as_ref()).is_some())
            .filter(|tool| {
                !matches!(
                    tool.name.as_ref(),
                    "ee_turn_evidence_summary" | "ee_verify_workspace_fact"
                ) || self.backend.exposes_turn_evidence_summary()
            })
            .filter(|tool| self.is_supported(tool.name.as_ref()))
            .map(with_read_only_annotation)
            .collect()
    }

    fn tools_manifest(&self) -> ToolsManifestResult {
        ToolsManifestResult {
            manifest_version: crate::EE_TOOL_SCHEMA_VERSION,
            tools: self.tools().into_iter().map(|tool| manifest_entry(&tool)).collect(),
        }
    }

    /// The server capabilities advertised in `initialize` and `discover`.
    fn capabilities() -> ServerCapabilities {
        ServerCapabilities::builder().enable_tools().build()
    }

    /// The server implementation identity advertised in `initialize`.
    fn server_info() -> Implementation {
        Implementation::new(crate::CLIENT_NAME, crate::CLIENT_VERSION).with_title("ee MCP proxy")
    }
}

impl rmcp::ServerHandler for EeMcpProxy {
    /// Accepts `2026-07-28` only; anything else fails closed.
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, ErrorData>> + MaybeSendFuture + '_ {
        context.peer.set_peer_info(request.clone());
        if request.protocol_version != ProtocolVersion::V_2026_07_28 {
            return std::future::ready(Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("unsupported protocol version: {}", request.protocol_version),
                None,
            )));
        }
        std::future::ready(Ok(InitializeResult::new(Self::capabilities())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Self::server_info())))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    /// Advertises `2026-07-28` with the tools capability.
    fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(DiscoverResult::new(
            SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            Self::capabilities(),
        )))
    }

    fn ping(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(()))
    }

    /// The fixed tool list; pagination is ignored (no cursor support).
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.tools())))
    }

    /// Dispatches a tool call to the backend via `EeMcpProxy::dispatch_tool`.
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(self.dispatch_tool(&request))
    }
}

/// Converts a complete tool result into a `tools/call` response.
fn complete(result: CallToolResult) -> CallToolResponse {
    CallToolResponse::from(result)
}

/// Converts a backend failure into an `isError` tool result.
///
/// Backend errors are tool-level failures the caller must see, not JSON-RPC
/// protocol errors; permission denials are prefixed so hosts can distinguish
/// them at a glance.
fn backend_error_result(error: ProxyToolError) -> CallToolResponse {
    if let Some((code, message)) = error.message.split_once(": ")
        && (matches!(
            code,
            "dependency_index_unavailable" | "dependency_index_stale" | "evidence_unavailable"
        ) || crate::tool_governance::WEB_CONTEXT_ERROR_CLASSES.contains(&code))
    {
        return complete(CallToolResult::error(vec![ContentBlock::text(
            json!({ "code": code, "message": message }).to_string(),
        )]));
    }

    let message = if error.is_permission_denied {
        format!("denied: {}", error.message)
    } else {
        error.message
    };
    complete(CallToolResult::error(vec![ContentBlock::text(message)]))
}

/// Adds standard MCP read-only metadata from the canonical governance record.
fn with_read_only_annotation(tool: Tool) -> Tool {
    let is_read_only = crate::governance(tool.name.as_ref()).is_some_and(|governance| {
        matches!(governance.side_effect, crate::classify::SideEffectClass::Read)
    });
    if is_read_only { tool.with_annotations(ToolAnnotations::new().read_only(true)) } else { tool }
}

/// Converts a `serde_json::json!` literal into a tool input schema object.
fn schema(value: serde_json::Value) -> JsonObject {
    value.as_object().expect("tool schema must be a JSON object").clone()
}

/// Builds manifest data from the advertised schema plus canonical governance.
/// `tools()` filters unknown records before this function runs.
fn manifest_entry(tool: &Tool) -> ToolManifestEntry {
    let name = tool.name.as_ref();
    let governance = crate::governance(name).expect("advertised tool has governance");
    let input_schema = serde_json::Value::Object((*tool.input_schema).clone());
    ToolManifestEntry {
        name: name.to_owned(),
        schema_version: crate::EE_TOOL_SCHEMA_VERSION,
        example: minimal_example(&input_schema),
        input_schema,
        side_effect: governance.side_effect.as_str().to_owned(),
        approval: governance.approval.to_owned(),
        transport_availability: governance
            .transports
            .iter()
            .map(|transport| transport.as_str().to_owned())
            .collect(),
        required_capabilities: governance
            .required_capabilities
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect(),
        output_caps: vec![ToolOutputCap {
            kind: governance.output_cap_kind.to_owned(),
            max: governance.output_cap,
        }],
        redaction_rules: governance.redaction_rules.iter().map(|rule| (*rule).to_owned()).collect(),
        error_classes: governance.error_classes.iter().map(|class| (*class).to_owned()).collect(),
        deprecated: governance.deprecated,
        replacement: governance.replacement.map(str::to_owned),
    }
}

/// Produces short schema-valid arguments without maintaining another per-tool
/// example table. Values illustrate argument shape only and are never paths or
/// identifiers from the current workspace.
fn minimal_example(input_schema: &serde_json::Value) -> serde_json::Value {
    let required = input_schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str);
    let properties = input_schema.get("properties").and_then(serde_json::Value::as_object);
    let mut example = serde_json::Map::new();
    for name in required {
        let schema = properties.and_then(|properties| properties.get(name));
        example.insert(name.to_owned(), minimal_value(name, schema));
    }
    serde_json::Value::Object(example)
}

fn minimal_value(name: &str, schema: Option<&serde_json::Value>) -> serde_json::Value {
    match schema.and_then(|value| value.get("type")).and_then(serde_json::Value::as_str) {
        Some("integer") | Some("number") => json!(if name == "since_seq" { 0 } else { 1 }),
        Some("boolean") => json!(false),
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => json!(match name {
            "path" => "/workspace/example.rs",
            "pattern" | "file_glob" => "*.rs",
            "query" => "example",
            "command" => "pwd",
            "terminal_id" => "terminal-1",
            "key" => "project.architecture",
            "value" | "content" => "example",
            "old_text" => "old",
            "new_text" => "new",
            "action_id" => "action-1",
            "new_name" => "renamed",
            "revision_id" => "revision-1",
            _ => "example",
        }),
    }
}

/// Requires the tool call to carry arguments.
fn require_arguments(request: &CallToolRequestParams) -> Result<&JsonObject, ErrorData> {
    request
        .arguments
        .as_ref()
        .ok_or_else(|| ErrorData::invalid_params("missing tool arguments", None))
}

/// Rejects unknown argument keys for a strict tool contract.
fn require_exact_argument_keys(arguments: &JsonObject, allowed: &[&str]) -> Result<(), ErrorData> {
    if let Some(key) = arguments.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ErrorData::invalid_params(format!("unexpected argument '{key}'"), None));
    }
    Ok(())
}

/// Validates the serialized size of one tool argument object.
///
/// Kept public for the libFuzzer target so malformed JSON objects and cap
/// boundaries exercise the exact production validation path.
pub fn validate_tool_argument_size(arguments: &JsonObject) -> Result<(), ErrorData> {
    let byte_len = serde_json::to_vec(arguments)
        .map_err(|error| {
            ErrorData::invalid_params(format!("tool arguments cannot serialize: {error}"), None)
        })?
        .len();
    if byte_len > MAX_TOOL_ARGUMENT_BYTES {
        return Err(ErrorData::invalid_params(
            format!("tool arguments exceed {MAX_TOOL_ARGUMENT_BYTES} byte cap"),
            None,
        ));
    }
    Ok(())
}

/// Rejects oversized serialized tool arguments before backend dispatch.
fn enforce_argument_cap(request: &CallToolRequestParams) -> Result<(), ErrorData> {
    request.arguments.as_ref().map_or(Ok(()), validate_tool_argument_size)
}

/// Rejects unexpected arguments for no-argument tools.
fn require_no_arguments(request: &CallToolRequestParams) -> Result<(), ErrorData> {
    if request.arguments.as_ref().is_some_and(|arguments| !arguments.is_empty()) {
        return Err(ErrorData::invalid_params("tool accepts no arguments", None));
    }
    Ok(())
}

/// Reads a required string argument.
fn require_string<'a>(arguments: &'a JsonObject, key: &str) -> Result<&'a str, ErrorData> {
    arguments.get(key).and_then(serde_json::Value::as_str).ok_or_else(|| {
        ErrorData::invalid_params(format!("missing or non-string argument '{key}'"), None)
    })
}

/// Reads a required non-empty string argument.
fn require_nonempty_string<'a>(arguments: &'a JsonObject, key: &str) -> Result<&'a str, ErrorData> {
    let value = require_string(arguments, key)?;
    if value.is_empty() {
        return Err(ErrorData::invalid_params(format!("argument '{key}' must not be empty"), None));
    }
    Ok(value)
}

/// Reads a required non-empty string and enforces a UTF-8 byte cap.
fn require_bounded_nonempty_string<'a>(
    arguments: &'a JsonObject,
    key: &str,
    max_bytes: usize,
) -> Result<&'a str, ErrorData> {
    let value = require_nonempty_string(arguments, key)?;
    if value.len() > max_bytes {
        return Err(ErrorData::invalid_params(
            format!("argument '{key}' exceeds {max_bytes} byte cap"),
            None,
        ));
    }
    Ok(value)
}

/// Reads a required boolean argument.
fn require_bool(arguments: &JsonObject, key: &str) -> Result<bool, ErrorData> {
    arguments.get(key).and_then(serde_json::Value::as_bool).ok_or_else(|| {
        ErrorData::invalid_params(format!("missing or non-boolean argument '{key}'"), None)
    })
}

/// Reads an optional string argument.
fn optional_string(arguments: &JsonObject, key: &str) -> Result<Option<String>, ErrorData> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value.as_str().map(ToOwned::to_owned).map(Some).ok_or_else(|| {
            ErrorData::invalid_params(format!("argument '{key}' must be a string"), None)
        }),
    }
}

/// Reads an optional non-empty string argument.
fn optional_nonempty_string(
    arguments: &JsonObject,
    key: &str,
) -> Result<Option<String>, ErrorData> {
    let value = optional_string(arguments, key)?;
    if value.as_deref().is_some_and(str::is_empty) {
        return Err(ErrorData::invalid_params(format!("argument '{key}' must not be empty"), None));
    }
    Ok(value)
}

/// Reads an optional positive integer argument without narrowing host turn ids.
fn optional_positive_u64(arguments: &JsonObject, key: &str) -> Result<Option<u64>, ErrorData> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let value = value.as_u64().ok_or_else(|| {
        ErrorData::invalid_params(format!("argument '{key}' must be a positive integer"), None)
    })?;
    if value == 0 {
        return Err(ErrorData::invalid_params(
            format!("argument '{key}' must be greater than zero"),
            None,
        ));
    }
    Ok(Some(value))
}

/// Reads an optional non-negative integer argument.
fn optional_u32(arguments: &JsonObject, key: &str) -> Result<Option<u32>, ErrorData> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => {
            value.as_u64().and_then(|number| u32::try_from(number).ok()).map(Some).ok_or_else(
                || {
                    ErrorData::invalid_params(
                        format!("argument '{key}' must be a non-negative integer"),
                        None,
                    )
                },
            )
        }
    }
}

/// Reads a required positive integer argument.
fn require_positive_u32(arguments: &JsonObject, key: &str) -> Result<u32, ErrorData> {
    let value = optional_u32(arguments, key)?
        .ok_or_else(|| ErrorData::invalid_params(format!("missing argument '{key}'"), None))?;
    if value == 0 {
        return Err(ErrorData::invalid_params(
            format!("argument '{key}' must be greater than zero"),
            None,
        ));
    }
    Ok(value)
}

/// Reads a required array of simple `old_text`/`new_text` edits.
fn text_edits(arguments: &JsonObject, key: &str) -> Result<Vec<TextEdit>, ErrorData> {
    let items = arguments.get(key).and_then(serde_json::Value::as_array).ok_or_else(|| {
        ErrorData::invalid_params(format!("argument '{key}' must be an array of edits"), None)
    })?;
    if items.is_empty() {
        return Err(ErrorData::invalid_params(format!("argument '{key}' must not be empty"), None));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let object = item.as_object().ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("edit {index} in '{key}' must be an object with old_text/new_text"),
                    None,
                )
            })?;
            if object.len() != 2
                || !object.contains_key("old_text")
                || !object.contains_key("new_text")
            {
                return Err(ErrorData::invalid_params(
                    format!("edit {index} in '{key}' must contain only old_text and new_text"),
                    None,
                ));
            }
            let old_text =
                object.get("old_text").and_then(serde_json::Value::as_str).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("edit {index} in '{key}' is missing string old_text"),
                        None,
                    )
                })?;
            let new_text =
                object.get("new_text").and_then(serde_json::Value::as_str).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("edit {index} in '{key}' is missing string new_text"),
                        None,
                    )
                })?;
            Ok(TextEdit { old_text: old_text.to_owned(), new_text: new_text.to_owned() })
        })
        .collect()
}

/// Reads an optional array-of-strings argument (defaults to empty).
fn string_array(arguments: &JsonObject, key: &str) -> Result<Vec<String>, ErrorData> {
    match arguments.get(key) {
        None => Ok(Vec::new()),
        Some(value) => {
            let items = value.as_array().ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("argument '{key}' must be an array of strings"),
                    None,
                )
            })?;
            items
                .iter()
                .map(|item| {
                    item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        ErrorData::invalid_params(
                            format!("argument '{key}' must be an array of strings"),
                            None,
                        )
                    })
                })
                .collect()
        }
    }
}

/// Reads the optional `env` object (string values), rejecting secret-like
/// keys before they ever reach the backend.
fn env_pairs(arguments: &JsonObject) -> Result<Vec<(String, String)>, ErrorData> {
    let Some(value) = arguments.get("env") else {
        return Ok(Vec::new());
    };
    let object = value.as_object().ok_or_else(|| {
        ErrorData::invalid_params("argument 'env' must be an object with string values", None)
    })?;
    let mut pairs = Vec::with_capacity(object.len());
    for (key, value) in object {
        if crate::handler::is_secret_field_name(key) {
            return Err(ErrorData::invalid_params(
                "secret-like environment keys are not allowed in the ee proxy",
                None,
            ));
        }
        let value = value.as_str().ok_or_else(|| {
            ErrorData::invalid_params(format!("env value for {key:?} must be a string"), None)
        })?;
        pairs.push((key.clone(), value.to_owned()));
    }
    Ok(pairs)
}

/// Requires an absolute path (relative paths fail closed).
fn require_absolute(path: &str) -> Result<(), ErrorData> {
    if std::path::Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(ErrorData::invalid_params(format!("path must be absolute: {path:?}"), None))
    }
}
