//! Typed JSON-RPC method registry for ACP v1.
//!
//! Every ACP v1 method name maps to exactly one params type and one result
//! type.  Method-name constants are derived from the official SDK's
//! [`AGENT_METHOD_NAMES`]/[`CLIENT_METHOD_NAMES`] structs, and all params /
//! result types are official `agent-client-protocol` types, so the registry
//! carries no duplicated wire schema.
//!
//! Directions mirror the SDK routing enums:
//!
//! - [`ClientRequestMethod`] — client→agent requests, mirroring
//!   [`schema::v1::ClientRequest`].
//! - [`AgentRequestMethod`] — agent→client requests, mirroring
//!   [`schema::v1::AgentRequest`].
//! - [`ClientNotificationMethod`] — client→agent notifications, mirroring
//!   [`schema::v1::ClientNotification`].
//! - [`AgentNotificationMethod`] — agent→client notifications, mirroring
//!   [`schema::v1::AgentNotification`].
//!
//! Known SDK gap (documented in tests): the SDK routing enums are untagged
//! and cannot validate params per method — empty-params variants such as
//! `LogoutRequest` accept any object and win in declaration order.  Params
//! validation therefore deserializes against the exact per-method wire type,
//! then applies ee policy (e.g. elicitation modes), failing closed with a
//! JSON-RPC `invalid params` error.

use agent_client_protocol::schema::v1::{
    AGENT_METHOD_NAMES, AuthenticateRequest, AuthenticateResponse, CLIENT_METHOD_NAMES,
    CancelNotification, CompleteElicitationNotification, CreateElicitationRequest,
    CreateElicitationResponse, CreateTerminalRequest, CreateTerminalResponse, ElicitationMode,
    InitializeRequest, InitializeResponse, KillTerminalRequest, KillTerminalResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, SetSessionModeRequest, SetSessionModeResponse, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use serde_json::Value;

use crate::Error;
use serde::de::Error as _;

// ── Method name constants (derived from the official SDK) ─────────────────────

/// Method name for the `initialize` request.
pub const INITIALIZE_METHOD_NAME: &str = AGENT_METHOD_NAMES.initialize;
/// Method name for the `authenticate` request.
pub const AUTHENTICATE_METHOD_NAME: &str = AGENT_METHOD_NAMES.authenticate;
/// Method name for the `logout` request.
pub const LOGOUT_METHOD_NAME: &str = AGENT_METHOD_NAMES.logout;
/// Method name for the `session/new` request.
pub const SESSION_NEW_METHOD_NAME: &str = AGENT_METHOD_NAMES.session_new;
/// Method name for the `session/load` request.
pub const SESSION_LOAD_METHOD_NAME: &str = AGENT_METHOD_NAMES.session_load;
/// Method name for the `session/set_mode` request.
pub const SESSION_SET_MODE_METHOD_NAME: &str = AGENT_METHOD_NAMES.session_set_mode;
/// Method name for the `session/prompt` request.
pub const SESSION_PROMPT_METHOD_NAME: &str = AGENT_METHOD_NAMES.session_prompt;
/// Method name for the `session/cancel` notification.
pub const SESSION_CANCEL_NOTIFICATION: &str = AGENT_METHOD_NAMES.session_cancel;

/// Method name for the `session/request_permission` request.
pub const SESSION_REQUEST_PERMISSION_METHOD_NAME: &str =
    CLIENT_METHOD_NAMES.session_request_permission;
/// Method name for the `session/update` notification.
pub const SESSION_UPDATE_NOTIFICATION: &str = CLIENT_METHOD_NAMES.session_update;
/// Method name for the `fs/read_text_file` request.
pub const FS_READ_TEXT_FILE_METHOD_NAME: &str = CLIENT_METHOD_NAMES.fs_read_text_file;
/// Method name for the `fs/write_text_file` request.
pub const FS_WRITE_TEXT_FILE_METHOD_NAME: &str = CLIENT_METHOD_NAMES.fs_write_text_file;
/// Method name for the `terminal/create` request.
pub const TERMINAL_CREATE_METHOD_NAME: &str = CLIENT_METHOD_NAMES.terminal_create;
/// Method name for the `terminal/output` request.
pub const TERMINAL_OUTPUT_METHOD_NAME: &str = CLIENT_METHOD_NAMES.terminal_output;
/// Method name for the `terminal/release` request.
pub const TERMINAL_RELEASE_METHOD_NAME: &str = CLIENT_METHOD_NAMES.terminal_release;
/// Method name for the `terminal/wait_for_exit` request.
pub const TERMINAL_WAIT_FOR_EXIT_METHOD_NAME: &str = CLIENT_METHOD_NAMES.terminal_wait_for_exit;
/// Method name for the `terminal/kill` request.
pub const TERMINAL_KILL_METHOD_NAME: &str = CLIENT_METHOD_NAMES.terminal_kill;
/// Method name for the `elicitation/create` request.
pub const ELICITATION_CREATE_METHOD_NAME: &str = CLIENT_METHOD_NAMES.elicitation_create;
/// Method name for the `elicitation/complete` notification.
pub const ELICITATION_COMPLETE_NOTIFICATION: &str = CLIENT_METHOD_NAMES.elicitation_complete;

fn invalid_params(method: &str, params_type: &str, reason: impl std::fmt::Display) -> Error {
    Error::invalid_params().data(serde_json::json!({
        "method": method,
        "paramsType": params_type,
        "reason": reason.to_string(),
    }))
}

// ── Client → agent requests ───────────────────────────────────────────────────

/// Every request the client can send to an ACP v1 agent.
///
/// Mirrors the SDK's [`schema::v1::ClientRequest`] enum, restricted to the
/// methods `ee` implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientRequestMethod {
    Initialize,
    Authenticate,
    Logout,
    SessionNew,
    SessionLoad,
    SessionSetMode,
    SessionPrompt,
}

impl ClientRequestMethod {
    /// All client→agent request methods, in registry order.
    pub const ALL: [ClientRequestMethod; 7] = [
        ClientRequestMethod::Initialize,
        ClientRequestMethod::Authenticate,
        ClientRequestMethod::Logout,
        ClientRequestMethod::SessionNew,
        ClientRequestMethod::SessionLoad,
        ClientRequestMethod::SessionSetMode,
        ClientRequestMethod::SessionPrompt,
    ];

    /// The wire method name, exactly as required by ACP v1.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Initialize => INITIALIZE_METHOD_NAME,
            Self::Authenticate => AUTHENTICATE_METHOD_NAME,
            Self::Logout => LOGOUT_METHOD_NAME,
            Self::SessionNew => SESSION_NEW_METHOD_NAME,
            Self::SessionLoad => SESSION_LOAD_METHOD_NAME,
            Self::SessionSetMode => SESSION_SET_MODE_METHOD_NAME,
            Self::SessionPrompt => SESSION_PROMPT_METHOD_NAME,
        }
    }

    /// The params type name used by this method.
    #[must_use]
    pub fn params_type_name(self) -> &'static str {
        match self {
            Self::Initialize => "InitializeRequest",
            Self::Authenticate => "AuthenticateRequest",
            Self::Logout => "LogoutRequest",
            Self::SessionNew => "NewSessionRequest",
            Self::SessionLoad => "LoadSessionRequest",
            Self::SessionSetMode => "SetSessionModeRequest",
            Self::SessionPrompt => "PromptRequest",
        }
    }

    /// The result type name used by this method.
    #[must_use]
    pub fn result_type_name(self) -> &'static str {
        match self {
            Self::Initialize => "InitializeResponse",
            Self::Authenticate => "AuthenticateResponse",
            Self::Logout => "LogoutResponse",
            Self::SessionNew => "NewSessionResponse",
            Self::SessionLoad => "LoadSessionResponse",
            Self::SessionSetMode => "SetSessionModeResponse",
            Self::SessionPrompt => "PromptResponse",
        }
    }

    /// Looks up the method by its exact wire name.  Returns `None` for
    /// unknown names (including CamelCase or v2 method spellings).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.name() == name)
    }

    /// Deserializes `params` into this method's exact params type.
    ///
    /// Fails closed with a JSON-RPC `invalid params` error when the payload
    /// does not match the ACP v1 wire type.
    pub fn validate_params(self, params: &Value) -> std::result::Result<(), Error> {
        let result = match self {
            Self::Initialize => {
                serde_json::from_value::<InitializeRequest>(params.clone()).map(|_| ())
            }
            Self::Authenticate => {
                serde_json::from_value::<AuthenticateRequest>(params.clone()).map(|_| ())
            }
            Self::Logout => {
                serde_json::from_value::<serde_json::Map<String, Value>>(params.clone()).map(|_| ())
            }
            Self::SessionNew => {
                serde_json::from_value::<NewSessionRequest>(params.clone()).map(|_| ())
            }
            Self::SessionLoad => {
                serde_json::from_value::<LoadSessionRequest>(params.clone()).map(|_| ())
            }
            Self::SessionSetMode => {
                serde_json::from_value::<SetSessionModeRequest>(params.clone()).map(|_| ())
            }
            Self::SessionPrompt => {
                serde_json::from_value::<PromptRequest>(params.clone()).map(|_| ())
            }
        };
        result.map_err(|err| invalid_params(self.name(), self.params_type_name(), err))
    }
}

// ── Agent → client requests ───────────────────────────────────────────────────

/// Every request an ACP v1 agent can send to the client.
///
/// Mirrors the SDK's [`schema::v1::AgentRequest`] enum, restricted to the
/// methods `ee` implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRequestMethod {
    SessionRequestPermission,
    FsReadTextFile,
    FsWriteTextFile,
    TerminalCreate,
    TerminalOutput,
    TerminalWaitForExit,
    TerminalKill,
    TerminalRelease,
    ElicitationCreate,
}

impl AgentRequestMethod {
    /// All agent→client request methods, in registry order.
    pub const ALL: [AgentRequestMethod; 9] = [
        AgentRequestMethod::SessionRequestPermission,
        AgentRequestMethod::FsReadTextFile,
        AgentRequestMethod::FsWriteTextFile,
        AgentRequestMethod::TerminalCreate,
        AgentRequestMethod::TerminalOutput,
        AgentRequestMethod::TerminalWaitForExit,
        AgentRequestMethod::TerminalKill,
        AgentRequestMethod::TerminalRelease,
        AgentRequestMethod::ElicitationCreate,
    ];

    /// The wire method name, exactly as required by ACP v1.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SessionRequestPermission => SESSION_REQUEST_PERMISSION_METHOD_NAME,
            Self::FsReadTextFile => FS_READ_TEXT_FILE_METHOD_NAME,
            Self::FsWriteTextFile => FS_WRITE_TEXT_FILE_METHOD_NAME,
            Self::TerminalCreate => TERMINAL_CREATE_METHOD_NAME,
            Self::TerminalOutput => TERMINAL_OUTPUT_METHOD_NAME,
            Self::TerminalWaitForExit => TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
            Self::TerminalKill => TERMINAL_KILL_METHOD_NAME,
            Self::TerminalRelease => TERMINAL_RELEASE_METHOD_NAME,
            Self::ElicitationCreate => ELICITATION_CREATE_METHOD_NAME,
        }
    }

    /// The params type name used by this method.
    #[must_use]
    pub fn params_type_name(self) -> &'static str {
        match self {
            Self::SessionRequestPermission => "RequestPermissionRequest",
            Self::FsReadTextFile => "ReadTextFileRequest",
            Self::FsWriteTextFile => "WriteTextFileRequest",
            Self::TerminalCreate => "CreateTerminalRequest",
            Self::TerminalOutput => "TerminalOutputRequest",
            Self::TerminalWaitForExit => "WaitForTerminalExitRequest",
            Self::TerminalKill => "KillTerminalRequest",
            Self::TerminalRelease => "ReleaseTerminalRequest",
            Self::ElicitationCreate => "CreateElicitationRequest",
        }
    }

    /// The result type name used by this method.
    #[must_use]
    pub fn result_type_name(self) -> &'static str {
        match self {
            Self::SessionRequestPermission => "RequestPermissionResponse",
            Self::FsReadTextFile => "ReadTextFileResponse",
            Self::FsWriteTextFile => "WriteTextFileResponse",
            Self::TerminalCreate => "CreateTerminalResponse",
            Self::TerminalOutput => "TerminalOutputResponse",
            Self::TerminalWaitForExit => "WaitForTerminalExitResponse",
            Self::TerminalKill => "KillTerminalResponse",
            Self::TerminalRelease => "ReleaseTerminalResponse",
            Self::ElicitationCreate => "CreateElicitationResponse",
        }
    }

    /// Looks up the method by its exact wire name.  Returns `None` for
    /// unknown names (including CamelCase or v2 method spellings).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.name() == name)
    }

    /// Deserializes `params` into this method's exact params type, then
    /// applies ee policy checks.
    ///
    /// Fails closed with a JSON-RPC `invalid params` error when the payload
    /// does not match the ACP v1 wire type or when `ee` does not support the
    /// requested behavior (e.g. unknown elicitation modes).
    pub fn validate_params(self, params: &Value) -> std::result::Result<(), Error> {
        let result = match self {
            Self::SessionRequestPermission => {
                serde_json::from_value::<RequestPermissionRequest>(params.clone()).map(|_| ())
            }
            Self::FsReadTextFile => {
                serde_json::from_value::<ReadTextFileRequest>(params.clone()).map(|_| ())
            }
            Self::FsWriteTextFile => {
                serde_json::from_value::<WriteTextFileRequest>(params.clone()).map(|_| ())
            }
            Self::TerminalCreate => {
                serde_json::from_value::<CreateTerminalRequest>(params.clone()).map(|_| ())
            }
            Self::TerminalOutput => {
                serde_json::from_value::<TerminalOutputRequest>(params.clone()).map(|_| ())
            }
            Self::TerminalWaitForExit => {
                serde_json::from_value::<WaitForTerminalExitRequest>(params.clone()).map(|_| ())
            }
            Self::TerminalKill => {
                serde_json::from_value::<KillTerminalRequest>(params.clone()).map(|_| ())
            }
            Self::TerminalRelease => {
                serde_json::from_value::<ReleaseTerminalRequest>(params.clone()).map(|_| ())
            }
            Self::ElicitationCreate => serde_json::from_value::<CreateElicitationRequest>(
                params.clone(),
            )
            .and_then(|request| {
                validate_elicitation_mode(&request.mode).map_err(serde_json::Error::custom)
            }),
        };
        result.map_err(|err| invalid_params(self.name(), self.params_type_name(), err))
    }
}

/// Rejects elicitation modes `ee` does not implement.
///
/// ACP v1 preserves unknown modes on the wire type (diagnostics), but `ee`
/// only implements `form` and `url`; anything else fails closed here.
fn validate_elicitation_mode(mode: &ElicitationMode) -> std::result::Result<(), String> {
    match mode {
        ElicitationMode::Form(_) | ElicitationMode::Url(_) => Ok(()),
        ElicitationMode::Other(other) => {
            Err(format!("unsupported elicitation mode `{}` (supported: form, url)", other.mode))
        }
        // `ElicitationMode` is non-exhaustive upstream; unknown future modes
        // fail closed the same way as explicit `Other` modes.
        _ => Err(String::from("unsupported elicitation mode")),
    }
}

// ── Notifications ─────────────────────────────────────────────────────────────

/// Every notification the client can send to an ACP v1 agent.
///
/// Mirrors the SDK's [`schema::v1::ClientNotification`] enum, restricted to
/// the notifications `ee` implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientNotificationMethod {
    SessionCancel,
}

impl ClientNotificationMethod {
    /// All client→agent notification methods.
    pub const ALL: [ClientNotificationMethod; 1] = [ClientNotificationMethod::SessionCancel];

    /// The wire method name, exactly as required by ACP v1.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SessionCancel => SESSION_CANCEL_NOTIFICATION,
        }
    }

    /// The params type name used by this notification.
    #[must_use]
    pub fn params_type_name(self) -> &'static str {
        match self {
            Self::SessionCancel => "CancelNotification",
        }
    }

    /// Looks up the notification by its exact wire name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.name() == name)
    }

    /// Deserializes `params` into this notification's exact params type.
    pub fn validate_params(self, params: &Value) -> std::result::Result<(), Error> {
        let result = serde_json::from_value::<CancelNotification>(params.clone()).map(|_| ());
        result.map_err(|err| invalid_params(self.name(), self.params_type_name(), err))
    }
}

/// Every notification an ACP v1 agent can send to the client.
///
/// Mirrors the SDK's [`schema::v1::AgentNotification`] enum, restricted to
/// the notifications `ee` implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentNotificationMethod {
    SessionUpdate,
    ElicitationComplete,
}

impl AgentNotificationMethod {
    /// All agent→client notification methods.
    pub const ALL: [AgentNotificationMethod; 2] =
        [AgentNotificationMethod::SessionUpdate, AgentNotificationMethod::ElicitationComplete];

    /// The wire method name, exactly as required by ACP v1.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SessionUpdate => SESSION_UPDATE_NOTIFICATION,
            Self::ElicitationComplete => ELICITATION_COMPLETE_NOTIFICATION,
        }
    }

    /// The params type name used by this notification.
    #[must_use]
    pub fn params_type_name(self) -> &'static str {
        match self {
            Self::SessionUpdate => "SessionNotification",
            Self::ElicitationComplete => "CompleteElicitationNotification",
        }
    }

    /// Looks up the notification by its exact wire name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.name() == name)
    }

    /// Deserializes `params` into this notification's exact params type.
    pub fn validate_params(self, params: &Value) -> std::result::Result<(), Error> {
        let result = match self {
            Self::SessionUpdate => {
                serde_json::from_value::<SessionNotification>(params.clone()).map(|_| ())
            }
            Self::ElicitationComplete => {
                serde_json::from_value::<CompleteElicitationNotification>(params.clone())
                    .map(|_| ())
            }
        };
        result.map_err(|err| invalid_params(self.name(), self.params_type_name(), err))
    }
}

// ── Unused result types referenced for the registry contract ─────────────────

#[allow(dead_code)]
type RegistryResultTypes = (
    InitializeResponse,
    AuthenticateResponse,
    NewSessionResponse,
    LoadSessionResponse,
    SetSessionModeResponse,
    PromptResponse,
    RequestPermissionResponse,
    ReadTextFileResponse,
    WriteTextFileResponse,
    CreateTerminalResponse,
    TerminalOutputResponse,
    WaitForTerminalExitResponse,
    KillTerminalResponse,
    ReleaseTerminalResponse,
    CreateElicitationResponse,
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use agent_client_protocol::schema::v1::ClientRequest;
    use serde_json::json;

    #[test]
    fn registry_method_names_match_acp_v1_exactly() {
        let client_expected = [
            "initialize",
            "authenticate",
            "logout",
            "session/new",
            "session/load",
            "session/set_mode",
            "session/prompt",
        ];
        let actual: Vec<_> = ClientRequestMethod::ALL.iter().map(|m| m.name()).collect();
        assert_eq!(actual, client_expected);

        let agent_expected = [
            "session/request_permission",
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
            "elicitation/create",
        ];
        let actual: Vec<_> = AgentRequestMethod::ALL.iter().map(|m| m.name()).collect();
        assert_eq!(actual, agent_expected);

        assert_eq!(ClientNotificationMethod::SessionCancel.name(), "session/cancel");
        assert_eq!(AgentNotificationMethod::SessionUpdate.name(), "session/update");
        assert_eq!(AgentNotificationMethod::ElicitationComplete.name(), "elicitation/complete");
    }

    #[test]
    fn sdk_derived_constants_match_expected_wire_names() {
        // Constants are derived from the SDK; spot-check the derivation
        // itself (a broken SDK rename would surface here and in fixtures).
        assert_eq!(INITIALIZE_METHOD_NAME, "initialize");
        assert_eq!(SESSION_PROMPT_METHOD_NAME, "session/prompt");
        assert_eq!(SESSION_CANCEL_NOTIFICATION, "session/cancel");
        assert_eq!(FS_READ_TEXT_FILE_METHOD_NAME, "fs/read_text_file");
        assert_eq!(TERMINAL_CREATE_METHOD_NAME, "terminal/create");
        assert_eq!(ELICITATION_CREATE_METHOD_NAME, "elicitation/create");
        assert_eq!(SESSION_UPDATE_NOTIFICATION, "session/update");
    }

    #[test]
    fn from_name_round_trips_and_rejects_unknown_spellings() {
        for method in ClientRequestMethod::ALL {
            assert_eq!(ClientRequestMethod::from_name(method.name()), Some(method));
        }
        for method in AgentRequestMethod::ALL {
            assert_eq!(AgentRequestMethod::from_name(method.name()), Some(method));
        }
        assert_eq!(ClientRequestMethod::from_name("session/Update"), None);
        assert_eq!(ClientRequestMethod::from_name("sessionUpdate"), None);
        assert_eq!(ClientRequestMethod::from_name(""), None);
    }

    #[test]
    fn every_method_maps_to_unique_params_and_result_types() {
        let client_params: Vec<_> =
            ClientRequestMethod::ALL.iter().map(|m| m.params_type_name()).collect();
        let client_results: Vec<_> =
            ClientRequestMethod::ALL.iter().map(|m| m.result_type_name()).collect();
        let agent_params: Vec<_> =
            AgentRequestMethod::ALL.iter().map(|m| m.params_type_name()).collect();
        let agent_results: Vec<_> =
            AgentRequestMethod::ALL.iter().map(|m| m.result_type_name()).collect();
        assert_unique(&client_params);
        assert_unique(&client_results);
        assert_unique(&agent_params);
        assert_unique(&agent_results);
    }

    fn assert_unique(values: &[&'static str]) {
        let unique: std::collections::HashSet<_> = values.iter().collect();
        assert_eq!(values.len(), unique.len());
    }

    #[test]
    fn valid_params_pass_validation() {
        let initialize = json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": true}},
        });
        ClientRequestMethod::Initialize.validate_params(&initialize).unwrap();

        let session_new = json!({"cwd": "/home/user/project", "mcpServers": []});
        ClientRequestMethod::SessionNew.validate_params(&session_new).unwrap();

        let prompt = json!({
            "sessionId": "sess_1",
            "prompt": [{"type": "text", "text": "hi"}],
        });
        ClientRequestMethod::SessionPrompt.validate_params(&prompt).unwrap();

        let read = json!({
            "sessionId": "sess_1",
            "path": "/main.rs",
            "line": 1,
            "limit": 10,
        });
        AgentRequestMethod::FsReadTextFile.validate_params(&read).unwrap();

        let cancel = json!({"sessionId": "sess_1"});
        ClientNotificationMethod::SessionCancel.validate_params(&cancel).unwrap();

        let update = json!({
            "sessionId": "sess_1",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}},
        });
        AgentNotificationMethod::SessionUpdate.validate_params(&update).unwrap();
    }

    #[test]
    fn malformed_params_fail_closed_with_invalid_params() {
        let err = ClientRequestMethod::Initialize
            .validate_params(&json!({"protocolVersion": "one"}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        let err = ClientRequestMethod::SessionPrompt
            .validate_params(&json!({"sessionId": 42}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        let err = ClientRequestMethod::SessionNew.validate_params(&json!({"cwd": 7})).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        let err = AgentRequestMethod::FsReadTextFile
            .validate_params(&json!({"sessionId": "s", "path": 7}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn unsupported_elicitation_modes_produce_invalid_params() {
        let request = json!({
            "mode": "bogus",
            "message": "tell me more",
            "sessionId": "sess_1",
        });
        let err = AgentRequestMethod::ElicitationCreate.validate_params(&request).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        let reason = err.data.as_ref().and_then(|d| d["reason"].as_str()).unwrap_or_default();
        assert!(reason.contains("unsupported elicitation mode `bogus`"), "{reason}");

        // form and url modes pass.
        let form = json!({
            "mode": "form",
            "message": "choose",
            "sessionId": "sess_1",
            "requestedSchema": {"type": "object", "properties": {}},
        });
        AgentRequestMethod::ElicitationCreate.validate_params(&form).unwrap();
    }

    #[test]
    fn sdk_untagged_routing_enum_cannot_validate_params_per_method() {
        // Documented SDK gap: `ClientRequest` is an untagged enum, so an
        // empty-params variant (`LogoutRequest`) accepts any object and wins
        // in declaration order.  Per-method validation must stay local.
        let malformed_prompt = json!({
            "sessionId": 42,
            "prompt": "not-an-array",
        });
        let parsed: ClientRequest = serde_json::from_value(malformed_prompt.clone()).unwrap();
        assert!(matches!(parsed, ClientRequest::LogoutRequest(_)));
        assert_eq!(parsed.method(), "logout");

        // Our registry rejects the same malformed payload for
        // `session/prompt` because it must match the exact `PromptRequest`
        // wire type.
        let err =
            ClientRequestMethod::SessionPrompt.validate_params(&malformed_prompt).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        // Valid prompt params are accepted by the registry even though the
        // untagged SDK enum cannot route them (they parse as LogoutRequest).
        ClientRequestMethod::SessionPrompt
            .validate_params(&json!({
                "sessionId": "sess_1",
                "prompt": [{"type": "text", "text": "hi"}],
            }))
            .unwrap();
    }
}
