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
use std::future::Future;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
    ErrorCode, ErrorData, Implementation, InitializeRequestParams, InitializeResult, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
use serde_json::json;

/// The single protocol version this server implements.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];

/// Error produced by an [`EeProxyBackend`] operation.
///
/// Backend failures never become JSON-RPC protocol errors: they surface as
/// `isError` tool results so the caller sees the message. The
/// `is_permission_denied` flag lets hosts distinguish host policy denials
/// from ordinary operation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyToolError {
    /// Human-readable failure description surfaced as tool content.
    pub message: String,
    /// Whether the failure was a permission denial (host policy).
    pub is_permission_denied: bool,
}

impl std::fmt::Display for ProxyToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProxyToolError {}

/// Backend implementing the editor operations the ee MCP proxy exposes.
///
/// All methods are synchronous and cheap to call; the host decides its own
/// capability policy (which paths, which commands) and applies it inside each
/// method.
pub trait EeProxyBackend: Send + Sync + 'static {
    /// Reads `path` (absolute) and returns the file text.
    ///
    /// `line` (1-based) and `limit` are optional line-window hints; the host
    /// decides how strictly to honor them.
    fn read_text_file(
        &self,
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, ProxyToolError>;

    /// Writes `content` to `path` (absolute).
    fn write_text_file(&self, path: String, content: String) -> Result<(), ProxyToolError>;

    /// Starts a terminal running `command` with `args` in `cwd` and `env`.
    ///
    /// Returns the terminal id.
    fn terminal_create(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<String, ProxyToolError>;

    /// Recent stderr/diagnostic lines, bounded; never contains secrets.
    fn diagnostics(&self) -> Vec<String>;
}

/// An in-process MCP server exposing ee editor operations as MCP tools.
///
/// The server speaks MCP `2026-07-28` only and advertises exactly the four
/// `ee.*` tools returned by [`list_tools`](rmcp::ServerHandler::list_tools).
/// Tool execution is delegated to the [`EeProxyBackend`] supplied at
/// construction.
pub struct EeMcpProxy {
    backend: Arc<dyn EeProxyBackend>,
}

impl EeMcpProxy {
    /// Creates a proxy delegating tool execution to `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn EeProxyBackend>) -> Self {
        Self { backend }
    }

    /// The server capabilities advertised in `initialize` and `discover`.
    fn capabilities() -> ServerCapabilities {
        ServerCapabilities::builder().enable_tools().build()
    }

    /// The server implementation identity advertised in `initialize`.
    fn server_info() -> Implementation {
        Implementation::new(crate::CLIENT_NAME, crate::CLIENT_VERSION).with_title("ee MCP proxy")
    }

    /// The fixed tool list (tool names are namespaced under `ee.`).
    fn tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "ee.read_text_file",
                "Read a text file from the editor workspace (absolute path).",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "limit": { "type": "integer" },
                    },
                    "required": ["path"],
                })),
            ),
            Tool::new(
                "ee.write_text_file",
                "Write text content to a file in the editor workspace (absolute path).",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                })),
            ),
            Tool::new(
                "ee.terminal_create",
                "Start a terminal in the editor workspace running a command.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" } },
                        "cwd": { "type": "string" },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                        },
                    },
                    "required": ["command"],
                })),
            ),
            Tool::new(
                "ee.diagnostics",
                "Recent editor diagnostics (stderr lines); never contains secrets.",
                schema(json!({ "type": "object", "properties": {} })),
            ),
        ]
    }

    /// Validates and dispatches one `tools/call` request.
    fn dispatch_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "ee.read_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = optional_u32(arguments, "line")?;
                let limit = optional_u32(arguments, "limit")?;
                Ok(self
                    .backend
                    .read_text_file(path.to_owned(), line, limit)
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee.write_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .write_text_file(path.to_owned(), content.to_owned())
                    .map(|()| {
                        complete(CallToolResult::success(vec![ContentBlock::text(format!(
                            "wrote {} bytes to {path}",
                            content.len()
                        ))]))
                    })
                    .unwrap_or_else(backend_error_result))
            }
            "ee.terminal_create" => {
                let arguments = require_arguments(request)?;
                let command = require_string(arguments, "command")?;
                if command.is_empty() {
                    return Err(ErrorData::invalid_params(
                        "argument 'command' must not be empty",
                        None,
                    ));
                }
                let args = string_array(arguments, "args")?;
                let cwd = match optional_string(arguments, "cwd")? {
                    Some(cwd) => {
                        require_absolute(&cwd)?;
                        Some(cwd)
                    }
                    None => None,
                };
                let env = env_pairs(arguments)?;
                Ok(self
                    .backend
                    .terminal_create(command.to_owned(), args, cwd, env)
                    .map(|id| complete(CallToolResult::success(vec![ContentBlock::text(id)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee.diagnostics" => {
                let lines = self.backend.diagnostics();
                Ok(complete(CallToolResult::success(vec![ContentBlock::text(lines.join("\n"))])))
            }
            _ => Err(ErrorData::method_not_found::<rmcp::model::CallToolRequestMethod>()),
        }
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
        std::future::ready(Ok(ListToolsResult::with_all_items(Self::tools())))
    }

    /// Dispatches a tool call to the backend; see [`EeMcpProxy::dispatch_tool`].
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
    let message = if error.is_permission_denied {
        format!("denied: {}", error.message)
    } else {
        error.message
    };
    complete(CallToolResult::error(vec![ContentBlock::text(message)]))
}

/// Converts a `serde_json::json!` literal into a tool input schema object.
fn schema(value: serde_json::Value) -> JsonObject {
    value.as_object().expect("tool schema must be a JSON object").clone()
}

/// Requires the tool call to carry arguments.
fn require_arguments(request: &CallToolRequestParams) -> Result<&JsonObject, ErrorData> {
    request
        .arguments
        .as_ref()
        .ok_or_else(|| ErrorData::invalid_params("missing tool arguments", None))
}

/// Reads a required string argument.
fn require_string<'a>(arguments: &'a JsonObject, key: &str) -> Result<&'a str, ErrorData> {
    arguments.get(key).and_then(serde_json::Value::as_str).ok_or_else(|| {
        ErrorData::invalid_params(format!("missing or non-string argument '{key}'"), None)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ClientHandler;
    use rmcp::model::ClientCapabilities;
    use rmcp::service::{ClientLifecycleMode, RoleClient, RunningService};
    use std::sync::Mutex;
    use std::time::Duration;

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Minimal client handler: 2026-07-28 only, no extra capabilities.
    #[derive(Debug, Clone, Copy, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> rmcp::model::InitializeRequestParams {
            rmcp::model::InitializeRequestParams::new(
                ClientCapabilities::builder().build(),
                rmcp::model::Implementation::new("proxy-test", "0.1"),
            )
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
        }
    }

    /// Records every backend invocation for assertions.
    #[derive(Debug, Default)]
    struct ScriptedBackend {
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedBackend {
        fn record(&self, call: String) {
            self.calls.lock().expect("calls poisoned").push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls poisoned").clone()
        }
    }

    impl EeProxyBackend for ScriptedBackend {
        fn read_text_file(
            &self,
            path: String,
            line: Option<u32>,
            limit: Option<u32>,
        ) -> Result<String, ProxyToolError> {
            self.record(format!("read:{path}:{line:?}:{limit:?}"));
            Ok(format!("content of {path}"))
        }

        fn write_text_file(&self, path: String, content: String) -> Result<(), ProxyToolError> {
            self.record(format!("write:{path}:{content}"));
            Ok(())
        }

        fn terminal_create(
            &self,
            command: String,
            args: Vec<String>,
            cwd: Option<String>,
            env: Vec<(String, String)>,
        ) -> Result<String, ProxyToolError> {
            self.record(format!("terminal:{command}:{args:?}:{cwd:?}:{env:?}"));
            Ok("term-1".to_owned())
        }

        fn diagnostics(&self) -> Vec<String> {
            vec!["line one".to_owned(), "line two".to_owned()]
        }
    }

    /// Always denies writes, for isError-result assertions.
    #[derive(Debug, Clone, Copy, Default)]
    struct DenyWriteBackend;

    impl EeProxyBackend for DenyWriteBackend {
        fn read_text_file(
            &self,
            _path: String,
            _line: Option<u32>,
            _limit: Option<u32>,
        ) -> Result<String, ProxyToolError> {
            Ok("unused".to_owned())
        }

        fn write_text_file(&self, _path: String, _content: String) -> Result<(), ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn terminal_create(
            &self,
            _command: String,
            _args: Vec<String>,
            _cwd: Option<String>,
            _env: Vec<(String, String)>,
        ) -> Result<String, ProxyToolError> {
            Ok("unused".to_owned())
        }

        fn diagnostics(&self) -> Vec<String> {
            Vec::new()
        }
    }

    /// Converts a `json!` literal into tool-call arguments.
    fn arguments(value: serde_json::Value) -> JsonObject {
        value.as_object().expect("arguments must be a JSON object").clone()
    }

    /// Spawns the proxy server and a 2026-07-28 Discover-mode client over one
    /// duplex transport; both handshakes must complete.
    async fn connect(
        backend: Arc<dyn EeProxyBackend>,
    ) -> (RunningService<RoleClient, TestClientHandler>, RunningService<RoleServer, EeMcpProxy>)
    {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(rmcp::serve_server(EeMcpProxy::new(backend), server_side));
        let client = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            rmcp::serve_client_with_lifecycle(
                TestClientHandler,
                client_side,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                },
            ),
        )
        .await
        .expect("client handshake timed out")
        .expect("client handshake failed");
        let server = tokio::time::timeout(HANDSHAKE_TIMEOUT, server_task)
            .await
            .expect("server handshake timed out")
            .expect("server task panicked")
            .expect("server handshake failed");
        (client, server)
    }

    /// Shuts both services down so tests never leave tasks behind.
    fn shutdown(
        client: &RunningService<RoleClient, TestClientHandler>,
        server: &RunningService<RoleServer, EeMcpProxy>,
    ) {
        client.cancellation_token().cancel();
        server.cancellation_token().cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_list_exposes_ee_namespaced_tools() {
        let backend: Arc<dyn EeProxyBackend> = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend).await;

        let tools = tokio::time::timeout(REQUEST_TIMEOUT, client.list_all_tools())
            .await
            .expect("list tools timed out")
            .expect("list tools failed");

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            vec!["ee.read_text_file", "ee.write_text_file", "ee.terminal_create", "ee.diagnostics",]
        );
        assert!(tools.iter().all(|tool| tool.name.starts_with("ee.")));
        assert!(tools.iter().all(|tool| tool.input_schema.contains_key("properties")));

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_text_file_routes_to_backend() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee.read_text_file")
            .with_arguments(arguments(json!({ "path": "/abs/notes.txt", "line": 3, "limit": 10 })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert_eq!(text.text, "content of /abs/notes.txt");

        assert_eq!(backend.calls(), vec!["read:/abs/notes.txt:Some(3):Some(10)".to_owned()]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relative_path_is_rejected() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee.read_text_file")
            .with_arguments(arguments(json!({ "path": "relative.txt" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "relative path must surface as a protocol error");

        assert!(backend.calls().is_empty(), "backend must not be invoked");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_text_file_result() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee.write_text_file")
            .with_arguments(arguments(json!({ "path": "/abs/out.txt", "content": "hello" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert!(text.text.contains("wrote 5 bytes to /abs/out.txt"));

        assert_eq!(backend.calls(), vec!["write:/abs/out.txt:hello".to_owned()]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permission_denied_surfaces_as_is_error_result() {
        let backend: Arc<dyn EeProxyBackend> = Arc::new(DenyWriteBackend);
        let (client, server) = connect(backend).await;

        let params = CallToolRequestParams::new("ee.write_text_file")
            .with_arguments(arguments(json!({ "path": "/abs/out.txt", "content": "hello" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("denials are tool results, not protocol errors");
        assert_eq!(result.is_error, Some(true));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert!(text.text.contains("no write access"), "message must reach the caller");
        assert!(text.text.starts_with("denied:"), "denials are prefixed");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_create_rejects_secret_env() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee.terminal_create").with_arguments(arguments(
            json!({ "command": "cargo", "env": { "API_TOKEN": "sekrit" } }),
        ));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "secret-like env keys must be rejected");

        assert!(backend.calls().is_empty(), "backend must not be invoked");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_create_routes_to_backend() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params =
            CallToolRequestParams::new("ee.terminal_create").with_arguments(arguments(json!({
                "command": "cargo",
                "args": ["check"],
                "cwd": "/abs/work",
                "env": { "RUST_BACKTRACE": "1" },
            })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert_eq!(text.text, "term-1");

        assert_eq!(
            backend.calls(),
            vec![
                "terminal:cargo:[\"check\"]:Some(\"/abs/work\"):[(\"RUST_BACKTRACE\", \"1\")]"
                    .to_owned()
            ]
        );

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_tool_fails_closed() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend).await;

        let params = CallToolRequestParams::new("ee.nope");
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "unknown tools must be method-not-found errors");

        shutdown(&client, &server);
    }
}
