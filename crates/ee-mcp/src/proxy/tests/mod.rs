use super::*;
use rmcp::ClientHandler;
use rmcp::model::ClientCapabilities;

use rmcp::service::{ClientLifecycleMode, RoleClient, RunningService};
use std::time::Duration;

mod deny_backend;
mod manifest_tests;
mod routing_tests;
mod scripted_backend;

pub(crate) use deny_backend::DenyWriteBackend;
pub(crate) use scripted_backend::ScriptedBackend;

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

fn workspace_fact(key: &str, selection_reason: Option<&str>) -> WorkspaceFact {
    WorkspaceFact {
        id: 7,
        namespace: String::from("project"),
        key: key.to_owned(),
        value: String::from("Tree-sitter owns parsing"),
        kind: String::from("architecture"),
        authority: String::from("user_asserted"),
        freshness: String::from("current"),
        state: String::from("active"),
        provenance: WorkspaceFactProvenance {
            source_kind: String::from("user"),
            source_id: String::from("turn-1"),
            revision: Some(String::from("rev-1")),
            fingerprint: Some(String::from("sha256:source")),
            verified_at: Some(String::from("2026-09-01T00:00:00Z")),
        },
        selection_reason: selection_reason.map(str::to_owned),
        created_at: String::from("2026-09-01T00:00:00Z"),
        updated_at: String::from("2026-09-01T00:00:01Z"),
        expires_at: None,
        content_hash: String::from("sha256:fact"),
        schema_version: 1,
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
) -> (RunningService<RoleClient, TestClientHandler>, RunningService<RoleServer, EeMcpProxy>) {
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

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(entries) => {
            let sorted = entries
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        value => value,
    }
}

fn assert_schema_example_is_valid(schema: &serde_json::Value, example: &serde_json::Value) {
    let example = example.as_object().expect("example is an object");
    for name in schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        let value = example.get(name).unwrap_or_else(|| panic!("missing example argument {name}"));
        let expected = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .and_then(|properties| properties.get(name))
            .and_then(|property| property.get("type"))
            .and_then(serde_json::Value::as_str);
        match expected {
            Some("string") => assert!(value.is_string(), "{name}"),
            Some("integer") => {
                assert!(value.as_i64().is_some() || value.as_u64().is_some(), "{name}")
            }
            Some("number") => assert!(value.is_number(), "{name}"),
            Some("array") => assert!(value.is_array(), "{name}"),
            Some("object") => assert!(value.is_object(), "{name}"),
            Some("boolean") => assert!(value.is_boolean(), "{name}"),
            _ => {}
        }
    }
}
