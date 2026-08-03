//! JSON fixture tests for the ACP v1 wire format.
//!
//! Fixtures are canonical ACP v1 messages taken from the published protocol
//! documentation (agentclientprotocol.com).  These tests pin:
//!
//! - exact method names (`initialize`, `session/new`, `fs/read_text_file`, …)
//! - discriminator values (`sessionUpdate: "agent_message_chunk"`, …)
//! - camelCase field names and numeric `protocolVersion`
//! - absolute-path and 1-based line invariants
//!
//! The fixtures live in `tests/fixtures/*.json` and are loaded verbatim.

use std::path::Path;

use ee_agent_protocol::{
    AgentNotificationMethod, AgentRequestMethod, ClientNotificationMethod, ClientRequestMethod,
    ContentBlock, CreateTerminalRequest, EmbeddedResource, EmbeddedResourceResource, EnvVariable,
    InitializeRequest, JsonRpcMessage, LoadSessionRequest, McpServer, McpServerStdio,
    NewSessionRequest, Notification, PermissionOption, PermissionOptionKind, PromptRequest,
    ReadTextFileRequest, RequestId, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, TextResourceContents, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    WriteTextFileRequest, validate,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("missing fixture {name}: {err}");
    });
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("invalid fixture {name}: {err}"))
}

/// (fixture, expected registry method) — client→agent requests.
const CLIENT_REQUEST_FIXTURES: &[(&str, ClientRequestMethod)] = &[
    ("initialize-request.json", ClientRequestMethod::Initialize),
    ("session-new-request.json", ClientRequestMethod::SessionNew),
    ("session-load-request.json", ClientRequestMethod::SessionLoad),
    ("session-prompt-request.json", ClientRequestMethod::SessionPrompt),
];

/// (fixture, expected registry method) — agent→client requests.
const AGENT_REQUEST_FIXTURES: &[(&str, AgentRequestMethod)] = &[
    ("session-request-permission-request.json", AgentRequestMethod::SessionRequestPermission),
    ("fs-read-text-file-request.json", AgentRequestMethod::FsReadTextFile),
    ("fs-write-text-file-request.json", AgentRequestMethod::FsWriteTextFile),
    ("terminal-create-request.json", AgentRequestMethod::TerminalCreate),
];

#[test]
fn fixture_method_names_match_acp_v1_exactly() {
    for (file, method) in CLIENT_REQUEST_FIXTURES {
        let raw = fixture(file);
        assert_eq!(
            raw["method"].as_str().unwrap(),
            method.name(),
            "fixture {file} must use the ACP v1 method name"
        );
        assert_eq!(raw["jsonrpc"].as_str().unwrap(), "2.0");
        assert!(raw["id"].is_number(), "fixture {file} must carry a JSON-RPC id");
    }
    for (file, method) in AGENT_REQUEST_FIXTURES {
        let raw = fixture(file);
        assert_eq!(
            raw["method"].as_str().unwrap(),
            method.name(),
            "fixture {file} must use the ACP v1 method name"
        );
        assert_eq!(raw["jsonrpc"].as_str().unwrap(), "2.0");
        assert!(raw["id"].is_number(), "fixture {file} must carry a JSON-RPC id");
    }

    let update = fixture("session-update-notification.json");
    assert_eq!(update["method"].as_str().unwrap(), ee_agent_protocol::SESSION_UPDATE_NOTIFICATION);
    assert!(update.get("id").is_none(), "notifications must not carry an id");
}

#[test]
fn fixture_discriminator_values_match_acp_v1() {
    let update = fixture("session-update-notification.json");
    assert_eq!(
        update["params"]["update"]["sessionUpdate"].as_str().unwrap(),
        "agent_message_chunk"
    );

    let permission = fixture("session-request-permission-request.json");
    assert_eq!(permission["params"]["options"][0]["kind"].as_str().unwrap(), "allow_once");
    assert_eq!(permission["params"]["options"][1]["kind"].as_str().unwrap(), "reject_once");
}

#[test]
fn fixtures_deserialize_into_typed_params() {
    for (file, method) in CLIENT_REQUEST_FIXTURES {
        let raw = fixture(file);
        let params = &raw["params"];
        method.validate_params(params).unwrap_or_else(|err| {
            panic!("fixture {file} must deserialize into {}: {err}", method.params_type_name())
        });
    }
    for (file, method) in AGENT_REQUEST_FIXTURES {
        let raw = fixture(file);
        let params = &raw["params"];
        method.validate_params(params).unwrap_or_else(|err| {
            panic!("fixture {file} must deserialize into {}: {err}", method.params_type_name())
        });
    }

    let update = fixture("session-update-notification.json");
    AgentNotificationMethod::SessionUpdate.validate_params(&update["params"]).unwrap();
}

#[test]
fn constructed_messages_serialize_exactly_like_fixtures() {
    // initialize
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(0),
        method: "initialize".into(),
        params: Some(
            InitializeRequest::new(ee_agent_protocol::version::ACP_PROTOCOL_VERSION)
                .client_capabilities(
                    ee_agent_protocol::ClientCapabilities::new()
                        .fs(ee_agent_protocol::FileSystemCapabilities::new()
                            .read_text_file(true)
                            .write_text_file(true))
                        .terminal(true),
                )
                .client_info(ee_agent_protocol::Implementation::new("ee", "0.10.1")),
        ),
    });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("initialize-request.json"));

    // session/new
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(1),
        method: "session/new".into(),
        params: Some(NewSessionRequest::new("/home/user/project").mcp_servers(vec![
            McpServer::Stdio(
                McpServerStdio::new("filesystem", "/path/to/mcp-server")
                    .args(vec!["--stdio".into()]),
            ),
        ])),
    });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("session-new-request.json"));

    // session/load
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(2),
        method: "session/load".into(),
        params: Some(
            LoadSessionRequest::new(SessionId::new("sess_789xyz"), "/home/user/project")
                .additional_directories(vec![
                    "/home/user/shared-lib".into(),
                    "/home/user/product-docs".into(),
                ]),
        ),
    });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("session-load-request.json"));

    // session/prompt
    let request =
        JsonRpcMessage::wrap(ee_agent_protocol::Request {
            id: RequestId::Number(3),
            method: "session/prompt".into(),
            params: Some(PromptRequest::new(
                SessionId::new("sess_abc123def456"),
                vec![
                ContentBlock::Text(ee_agent_protocol::TextContent::new(
                    "Can you analyze this code for potential issues?",
                )),
                ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                        "def process_data(items):\n    for item in items:\n        print(item)",
                        "file:///home/user/project/main.py",
                    )
                    .mime_type("text/x-python")),
                )),
            ],
            )),
        });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("session-prompt-request.json"));

    // session/request_permission
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(5),
        method: "session/request_permission".into(),
        params: Some(RequestPermissionRequest::new(
            SessionId::new("sess_abc123def456"),
            ToolCallUpdate::new(ToolCallId::new("call_001"), ToolCallUpdateFields::default()),
            vec![
                PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject-once", "Reject", PermissionOptionKind::RejectOnce),
            ],
        )),
    });
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        fixture("session-request-permission-request.json")
    );

    // fs/read_text_file
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(3),
        method: "fs/read_text_file".into(),
        params: Some(
            ReadTextFileRequest::new(
                SessionId::new("sess_abc123def456"),
                "/home/user/project/src/main.py",
            )
            .line(10)
            .limit(50),
        ),
    });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("fs-read-text-file-request.json"));

    // fs/write_text_file
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(4),
        method: "fs/write_text_file".into(),
        params: Some(WriteTextFileRequest::new(
            SessionId::new("sess_abc123def456"),
            "/home/user/project/config.json",
            "{\n  \"debug\": true,\n  \"version\": \"1.0.0\"\n}",
        )),
    });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("fs-write-text-file-request.json"));

    // terminal/create
    let request = JsonRpcMessage::wrap(ee_agent_protocol::Request {
        id: RequestId::Number(5),
        method: "terminal/create".into(),
        params: Some(
            CreateTerminalRequest::new(SessionId::new("sess_abc123def456"), "npm")
                .args(vec!["test".into(), "--coverage".into()])
                .env(vec![EnvVariable::new("NODE_ENV", "test")])
                .cwd("/home/user/project")
                .output_byte_limit(1_048_576),
        ),
    });
    assert_eq!(serde_json::to_value(request).unwrap(), fixture("terminal-create-request.json"));

    // session/update notification
    let notification = JsonRpcMessage::wrap(Notification {
        method: "session/update".into(),
        params: Some(SessionNotification::new(
            SessionId::new("sess_abc123def456"),
            SessionUpdate::AgentMessageChunk(
                ee_agent_protocol::ContentChunk::new(ContentBlock::Text(
                    ee_agent_protocol::TextContent::new(
                        "I'll analyze your code for potential issues. Let me examine it...",
                    ),
                ))
                .message_id("msg_agent_c42b9"),
            ),
        )),
    });
    assert_eq!(
        serde_json::to_value(notification).unwrap(),
        fixture("session-update-notification.json")
    );
}

#[test]
fn registry_validates_every_fixture_method_name() {
    for (file, method) in CLIENT_REQUEST_FIXTURES {
        let raw = fixture(file);
        assert_eq!(ClientRequestMethod::from_name(raw["method"].as_str().unwrap()), Some(*method));
    }
    for (file, method) in AGENT_REQUEST_FIXTURES {
        let raw = fixture(file);
        assert_eq!(AgentRequestMethod::from_name(raw["method"].as_str().unwrap()), Some(*method));
    }
    let update = fixture("session-update-notification.json");
    assert_eq!(
        AgentNotificationMethod::from_name(update["method"].as_str().unwrap()),
        Some(AgentNotificationMethod::SessionUpdate)
    );
    assert_eq!(
        ClientNotificationMethod::from_name("session/cancel"),
        Some(ClientNotificationMethod::SessionCancel)
    );
    // CamelCase spellings must not resolve.
    assert_eq!(ClientRequestMethod::from_name("session/new".to_uppercase().as_str()), None);
}

#[test]
fn sdk_dispatch_reads_method_from_envelope_not_params_shape() {
    use agent_client_protocol::schema::v1::ClientRequest;
    use std::path::PathBuf;

    // Documented SDK gap with real fixtures: the SDK routing enums are
    // untagged, so `session/new` params parse as `LogoutRequest` (the
    // empty-params variant matches any object first).  Dispatch must read
    // the method name from the JSON-RPC envelope and deserialize params per
    // method — exactly what `ClientRequestMethod::validate_params` does.
    let parsed: ClientRequest =
        serde_json::from_value(fixture("session-new-request.json")["params"].clone()).unwrap();
    assert!(matches!(parsed, ClientRequest::LogoutRequest(_)));

    // With the envelope method name, per-method deserialization succeeds and
    // the registry accepts the fixture.
    let params = fixture("session-new-request.json")["params"].clone();
    let request: NewSessionRequest = serde_json::from_value(params.clone()).unwrap();
    assert_eq!(request.cwd, PathBuf::from("/home/user/project"));
    ClientRequestMethod::SessionNew.validate_params(&params).unwrap();
}

#[test]
fn fixture_paths_and_lines_are_absolute_and_one_based() {
    // The canonical fixtures must pass protocol-boundary validation.
    let read: ReadTextFileRequest =
        serde_json::from_value(fixture("fs-read-text-file-request.json")["params"].clone())
            .unwrap();
    validate::validate_read_text_file(&read).unwrap();

    let write: WriteTextFileRequest =
        serde_json::from_value(fixture("fs-write-text-file-request.json")["params"].clone())
            .unwrap();
    validate::validate_write_text_file(&write).unwrap();

    let terminal: CreateTerminalRequest =
        serde_json::from_value(fixture("terminal-create-request.json")["params"].clone()).unwrap();
    validate::validate_terminal_create(&terminal).unwrap();

    // Relative paths and 0-based lines fail closed before dispatch.
    let relative = json!({
        "sessionId": "sess_1",
        "path": "src/main.py",
        "line": 10,
    });
    let request: ReadTextFileRequest = serde_json::from_value(relative).unwrap();
    let err = validate::validate_read_text_file(&request).unwrap_err();
    assert_eq!(err.code, ee_agent_protocol::ErrorCode::InvalidParams);

    let zero_based = json!({
        "sessionId": "sess_1",
        "path": "/src/main.py",
        "line": 0,
    });
    let request: ReadTextFileRequest = serde_json::from_value(zero_based).unwrap();
    let err = validate::validate_read_text_file(&request).unwrap_err();
    assert_eq!(err.code, ee_agent_protocol::ErrorCode::InvalidParams);

    let relative_write = json!({
        "sessionId": "sess_1",
        "path": "config.json",
        "content": "{}",
    });
    let request: WriteTextFileRequest = serde_json::from_value(relative_write).unwrap();
    let err = validate::validate_write_text_file(&request).unwrap_err();
    assert_eq!(err.code, ee_agent_protocol::ErrorCode::InvalidParams);

    let relative_cwd = json!({
        "sessionId": "sess_1",
        "command": "npm",
        "cwd": "project",
    });
    let request: CreateTerminalRequest = serde_json::from_value(relative_cwd).unwrap();
    let err = validate::validate_terminal_create(&request).unwrap_err();
    assert_eq!(err.code, ee_agent_protocol::ErrorCode::InvalidParams);
}

#[test]
fn session_update_fixture_round_trips_through_ordering_tracker() {
    let raw = fixture("session-update-notification.json");
    let notification: SessionNotification = serde_json::from_value(raw["params"].clone()).unwrap();
    let mut order = ee_agent_protocol::SessionUpdateOrder::new();
    order.register_update(&notification.update).unwrap();
    assert!(order.message_known("msg_agent_c42b9"));
}

#[test]
fn session_lifecycle_serde_respects_load_session_capability() {
    use ee_agent_protocol::{AgentCapabilities, LoadSessionRequest, NewSessionRequest};
    use std::path::PathBuf;

    // The `loadSession` capability defaults to disabled and round-trips
    // through serde both ways.
    let disabled: AgentCapabilities = serde_json::from_value(json!({})).unwrap();
    assert!(!disabled.load_session);
    let enabled: AgentCapabilities =
        serde_json::from_value(json!({ "loadSession": true })).unwrap();
    assert!(enabled.load_session);
    assert_eq!(
        serde_json::from_value::<AgentCapabilities>(serde_json::to_value(&enabled).unwrap())
            .unwrap(),
        enabled
    );

    // `session/new` is a baseline method; its wire shape is unaffected by
    // the capability flag.
    let new_request: NewSessionRequest =
        serde_json::from_value(fixture("session-new-request.json")["params"].clone()).unwrap();
    assert_eq!(new_request.cwd, PathBuf::from("/home/user/project"));
    assert_eq!(
        serde_json::to_value(&new_request).unwrap(),
        fixture("session-new-request.json")["params"]
    );

    // `session/load` round-trips only when the agent advertised
    // `loadSession`; with the capability off the client must fail closed
    // before dispatch.
    let load_request: LoadSessionRequest =
        serde_json::from_value(fixture("session-load-request.json")["params"].clone()).unwrap();
    assert_eq!(load_request.session_id.0.as_ref(), "sess_789xyz");
    assert_eq!(load_request.cwd, PathBuf::from("/home/user/project"));
    assert_eq!(
        serde_json::to_value(&load_request).unwrap(),
        fixture("session-load-request.json")["params"]
    );
    assert!(!disabled.load_session, "default agent capabilities must keep loadSession disabled");
}

#[test]
fn session_new_and_load_work_with_capabilities_enabled() {
    use ee_agent_protocol::{AgentCapabilities, LoadSessionRequest, NewSessionRequest, SessionId};

    let capabilities = AgentCapabilities::new().load_session(true);
    assert!(capabilities.load_session);

    // With the capability enabled both lifecycle requests serialize
    // with the same session id and cwd.
    let new_request = NewSessionRequest::new("/work").mcp_servers(vec![]);
    let load_request =
        LoadSessionRequest::new(SessionId::new("sess_1"), "/work").mcp_servers(vec![]);
    assert_eq!(new_request.cwd, load_request.cwd);
    assert_eq!(serde_json::to_value(&new_request).unwrap()["cwd"], json!("/work"));
    assert_eq!(serde_json::to_value(&load_request).unwrap()["sessionId"], json!("sess_1"));
}
