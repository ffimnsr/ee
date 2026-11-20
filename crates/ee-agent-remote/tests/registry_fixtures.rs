//! Wire-conformance fixtures for the remote registry.
//!
//! One JSON file per registry entry. Each file parses into its typed params
//! type and serializes back to an equal JSON value, so a silent schema drift
//! (renamed field, changed nullability, changed tag) fails here.

use std::fs;
use std::path::{Path, PathBuf};

use ee_agent_remote::protocol::{
    HistoryTailParams, InitializeParams, Method, PermissionApprovalRequest,
    PermissionsPendingParams, PermissionsRespondParams, SessionCloseParams, SessionListParams,
    SessionNewParams, SessionResumeParams, SessionSetModeParams, SessionUpdateNotification,
    StatusChangedNotification, TurnCompletedNotification, TurnInterruptParams, TurnStartParams,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/registry")
}

fn round_trip<T>(method: Method, stem: &str) -> Value
where
    T: DeserializeOwned + Serialize,
{
    let path = fixture_dir().join(format!("{stem}.json"));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let original: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    let typed: T = serde_json::from_value(original.clone()).unwrap_or_else(|error| {
        panic!("{} did not parse into {}: {error}", path.display(), method.params_type())
    });
    let back = serde_json::to_value(&typed)
        .unwrap_or_else(|error| panic!("{} failed to serialize: {error}", path.display()));
    assert_eq!(back, original, "{} is not byte-stable through its typed form", path.display());
    back
}

#[test]
fn every_request_fixture_parses_and_round_trips() {
    round_trip::<InitializeParams>(Method::Initialize, "initialize");
    round_trip::<SessionListParams>(Method::SessionList, "session_list");
    round_trip::<SessionNewParams>(Method::SessionNew, "session_new");
    round_trip::<SessionResumeParams>(Method::SessionResume, "session_resume");
    round_trip::<SessionCloseParams>(Method::SessionClose, "session_close");
    round_trip::<SessionSetModeParams>(Method::SessionSetMode, "session_set_mode");
    round_trip::<TurnStartParams>(Method::TurnStart, "turn_start");
    round_trip::<TurnInterruptParams>(Method::TurnInterrupt, "turn_interrupt");
    round_trip::<PermissionsPendingParams>(Method::PermissionsPending, "permissions_pending");
    round_trip::<PermissionsRespondParams>(Method::PermissionsRespond, "permissions_respond");
    round_trip::<HistoryTailParams>(Method::HistoryTail, "history_tail");
}

#[test]
fn every_notification_fixture_parses_and_round_trips() {
    round_trip::<serde_json::Map<String, Value>>(Method::Initialized, "initialized");
    round_trip::<SessionUpdateNotification>(Method::SessionUpdate, "session_update");
    round_trip::<TurnCompletedNotification>(Method::TurnCompleted, "turn_completed");
    round_trip::<StatusChangedNotification>(Method::StatusChanged, "status_changed");
}

#[test]
fn server_request_fixture_parses_and_round_trips() {
    let value = round_trip::<PermissionApprovalRequest>(
        Method::PermissionRequestApproval,
        "permission_request_approval",
    );
    assert_eq!(value["requestId"], 7);
    assert_eq!(value["toolCall"]["kind"], "execute");
}

#[test]
fn every_registered_method_has_a_fixture_file() {
    for method in Method::ALL {
        let stem = match method {
            Method::Initialize => "initialize",
            Method::Initialized => "initialized",
            Method::SessionList => "session_list",
            Method::SessionNew => "session_new",
            Method::SessionResume => "session_resume",
            Method::SessionClose => "session_close",
            Method::SessionSetMode => "session_set_mode",
            Method::TurnStart => "turn_start",
            Method::TurnInterrupt => "turn_interrupt",
            Method::PermissionsPending => "permissions_pending",
            Method::PermissionsRespond => "permissions_respond",
            Method::HistoryTail => "history_tail",
            Method::SessionUpdate => "session_update",
            Method::TurnCompleted => "turn_completed",
            Method::StatusChanged => "status_changed",
            Method::PermissionRequestApproval => "permission_request_approval",
        };
        let path = fixture_dir().join(format!("{stem}.json"));
        assert!(path.exists(), "missing fixture for {} at {}", method.name(), path.display());
    }
}

#[test]
fn every_fixture_file_belongs_to_a_registered_method() {
    let mut files = 0;
    for entry in fs::read_dir(fixture_dir()).expect("fixture dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        files += 1;
        let stem = path.file_stem().and_then(|stem| stem.to_str()).expect("stem");
        let known = [
            "initialize",
            "initialized",
            "session_list",
            "session_new",
            "session_resume",
            "session_close",
            "session_set_mode",
            "turn_start",
            "turn_interrupt",
            "permissions_pending",
            "permissions_respond",
            "history_tail",
            "session_update",
            "turn_completed",
            "status_changed",
            "permission_request_approval",
        ];
        assert!(known.contains(&stem), "unregistered fixture {stem}.json");
    }
    assert_eq!(files, Method::ALL.len(), "one fixture per registry entry");
}

#[test]
fn unknown_methods_fail_with_the_typed_error() {
    let error = Method::try_from_name("session/steer").expect_err("unknown");
    assert_eq!(error.code().jsonrpc_code(), -32601);
}
