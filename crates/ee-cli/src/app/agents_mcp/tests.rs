//! MCP proxy tests.
use std::time::Duration;

use super::proxy_calls::ProxyCall;
use super::proxy_calls::ProxyReply;
use super::proxy_server::proxy_reply_from_bridge;
use super::proxy_server::serve_proxy_connection;
use super::proxy_server::start_proxy_call_to_bridge;
use super::*;
use tokio::io::AsyncWriteExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_disconnect_cancels_pending_network_approval() {
    let (server, mut client) = tokio::net::UnixStream::pair().expect("create proxy socket pair");
    let (bridge_tx, bridge_rx) = std::sync::mpsc::channel();
    let serving = tokio::spawn(serve_proxy_connection(server, String::from("token"), bridge_tx));
    let request = serde_json::json!({
        "id": 1,
        "params": { "method": "web_search", "query": "Rust MCP" },
    });
    client
        .write_all(format!("token\n{request}\n").as_bytes())
        .await
        .expect("send proxy network request");

    let message = bridge_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("network request reaches the editor bridge");
    let BridgeUiMessage::ProxyTool { call, route, reply } = message else {
        panic!("network request must use ProxyTool bridge message");
    };
    let ProxyToolCall::WebSearch { cancellation, .. } = call else {
        panic!("network request must preserve cancellation token");
    };
    assert!(matches!(route, ProxyRoute::Stdio));

    drop(client);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !reply.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("proxy disconnect closes pending bridge reply");
    tokio::time::timeout(Duration::from_secs(1), cancellation.cancelled())
        .await
        .expect("proxy disconnect cancels in-flight web request");
    serving.await.expect("proxy connection task completes");
}

#[test]
fn stdio_tool_list_contains_exact_workspace_memory_surface() {
    let names = ee_mcp::tool_names_for_transport(ee_mcp::ToolTransport::Stdio);
    for name in [
        "ee_remember_workspace_fact",
        "ee_recall_workspace_facts",
        "ee_read_workspace_fact",
        "ee_forget_workspace_fact",
        "ee_list_workspace_facts",
        "ee_retract_workspace_fact",
        "ee_export_workspace_memory",
        "ee_import_workspace_memory",
        "ee_clear_workspace_memory",
    ] {
        assert!(names.contains(&name), "missing stdio workspace-memory tool {name}");
    }
}

#[test]
fn workspace_memory_stdio_calls_route_all_exact_tools() {
    let calls = [
        ProxyCall::RememberWorkspaceFact {
            key: String::from("key"),
            value: String::from("secret-value"),
        },
        ProxyCall::RecallWorkspaceFacts { query: String::from("build") },
        ProxyCall::ReadWorkspaceFact { key: String::from("key") },
        ProxyCall::ForgetWorkspaceFact { key: String::from("key") },
        ProxyCall::ListWorkspaceFacts { limit: 8 },
        ProxyCall::RetractWorkspaceFact { key: String::from("key") },
        ProxyCall::ExportWorkspaceMemory { include_values: true },
        ProxyCall::ImportWorkspaceMemory { export_json: String::from("{\"facts\":[]}") },
        ProxyCall::ClearWorkspaceMemory,
    ];
    let expected_methods = [
        "remember_workspace_fact",
        "recall_workspace_facts",
        "read_workspace_fact",
        "forget_workspace_fact",
        "list_workspace_facts",
        "retract_workspace_fact",
        "export_workspace_memory",
        "import_workspace_memory",
        "clear_workspace_memory",
    ];
    for (call, expected_method) in calls.into_iter().zip(expected_methods) {
        let encoded = serde_json::to_value(&call).unwrap();
        assert_eq!(encoded["method"], expected_method);
        let (bridge_tx, bridge_rx) = std::sync::mpsc::channel();
        let pending = start_proxy_call_to_bridge(
            call,
            "scope",
            tokio_util::sync::CancellationToken::new(),
            &bridge_tx,
        )
        .expect("workspace memory call reaches bridge");
        let BridgeUiMessage::ProxyTool { call, route, reply } =
            bridge_rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("workspace memory call must use proxy bridge");
        };
        assert_eq!(route, ProxyRoute::Stdio);
        match (expected_method, call) {
            ("remember_workspace_fact", ProxyToolCall::RememberWorkspaceFact { key, value }) => {
                assert_eq!(key, "key");
                assert_eq!(value, "secret-value");
            }
            ("recall_workspace_facts", ProxyToolCall::RecallWorkspaceFacts { query }) => {
                assert_eq!(query, "build");
            }
            ("read_workspace_fact", ProxyToolCall::ReadWorkspaceFact { key })
            | ("forget_workspace_fact", ProxyToolCall::ForgetWorkspaceFact { key })
            | ("retract_workspace_fact", ProxyToolCall::RetractWorkspaceFact { key }) => {
                assert_eq!(key, "key");
            }
            ("list_workspace_facts", ProxyToolCall::ListWorkspaceFacts { limit }) => {
                assert_eq!(limit, 8);
            }
            (
                "export_workspace_memory",
                ProxyToolCall::ExportWorkspaceMemory { include_values },
            ) => assert!(include_values),
            ("import_workspace_memory", ProxyToolCall::ImportWorkspaceMemory { export_json }) => {
                assert_eq!(export_json, "{\"facts\":[]}")
            }
            ("clear_workspace_memory", ProxyToolCall::ClearWorkspaceMemory) => {}
            _ => panic!("workspace memory call changed route"),
        }
        drop(reply);
        assert!(matches!(pending.blocking_recv(), Err(_) | Ok(Err(AgentError::Cancelled))));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_proxy_calls_keep_payloads_and_stdio_route() {
    let (bridge_tx, bridge_rx) = std::sync::mpsc::channel();

    for (request, expected) in [
        (
            ProxyCall::WebSearch { query: String::from("Rust MCP") },
            ProxyToolCall::WebSearch {
                query: String::from("Rust MCP"),
                approval_scope: String::from("scope"),
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        ),
        (
            ProxyCall::FetchUrl { url: String::from("https://example.com/docs") },
            ProxyToolCall::FetchUrl {
                url: String::from("https://example.com/docs"),
                approval_scope: String::from("scope"),
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        ),
    ] {
        let sender = bridge_tx.clone();
        let pending = tokio::spawn(async move {
            match start_proxy_call_to_bridge(
                request,
                "scope",
                tokio_util::sync::CancellationToken::new(),
                &sender,
            ) {
                Ok(reply_rx) => proxy_reply_from_bridge(reply_rx.await),
                Err(reply) => reply,
            }
        });
        let message = bridge_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("web proxy call reaches the editor bridge");

        let BridgeUiMessage::ProxyTool { call, route, reply } = message else {
            panic!("web proxy call must use ProxyTool bridge message");
        };
        assert!(matches!(route, ProxyRoute::Stdio));
        match (call, expected) {
            (
                ProxyToolCall::WebSearch { query: actual, approval_scope: actual_scope, .. },
                ProxyToolCall::WebSearch { query: wanted, approval_scope: wanted_scope, .. },
            ) => {
                assert_eq!(actual, wanted);
                assert_eq!(actual_scope, wanted_scope);
            }
            (
                ProxyToolCall::FetchUrl { url: actual, approval_scope: actual_scope, .. },
                ProxyToolCall::FetchUrl { url: wanted, approval_scope: wanted_scope, .. },
            ) => {
                assert_eq!(actual, wanted);
                assert_eq!(actual_scope, wanted_scope);
            }
            _ => panic!("web proxy call changed its bridge payload"),
        }

        drop(reply);
        let reply = pending.await.expect("web proxy bridge task completes");
        assert!(
            matches!(reply, ProxyReply::Err { error } if error.message == "approval channel closed")
        );
    }
}
