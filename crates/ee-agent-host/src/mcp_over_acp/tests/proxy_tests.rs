//! MCP-over-ACP tests: proxy tool routing and terminal output.
use super::*;

#[test]
fn mode_display_is_deterministic() {
    assert_eq!(EeProxyMode::AcpNative.to_string(), "acp-native");
    assert_eq!(EeProxyMode::StdioFallback.to_string(), "stdio fallback");
    assert_eq!(EeProxyMode::Disabled.to_string(), "disabled");
}

#[test]
fn frame_cap_matches_stdio_proxy_cap() {
    // The ACP-native path must be at least as strict as the stdio proxy
    // fallback (ee-cli `PROXY_MAX_FRAME_BYTES`).
    assert_eq!(MCP_OVER_ACP_MAX_FRAME_BYTES, 4 * 1024 * 1024);
}

/// Inner MCP wire messages round-trip through the same deserialization
/// the transport bridge uses.
#[test]
fn inner_mcp_request_deserializes_into_rmcp_types() {
    let value = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": null,
    });
    let message: RxJsonRpcMessage<RoleServer> =
        serde_json::from_value(value).expect("tools/list parses");
    match message {
        ModelJsonRpcMessage::Request(request) => {
            assert_eq!(request.id, RequestId::Number(1));
        }
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn proxy_terminal_output_uses_structured_proxy_response() {
    let (jobs, mut received) = mpsc::unbounded_channel();
    let backend = HostProxyBackend {
        jobs,
        process: Arc::new(Mutex::new(None)),
        threads: Arc::new(Mutex::new(HashMap::new())),
        agent_id: String::from("test-agent"),
        scope: String::from("test"),
        supported_tools: None,
        workspace_memory: WorkspaceMemoryHost::disabled(),
        shutdown: CancellationToken::new(),
    };
    let worker = std::thread::spawn(move || {
        let job = received.blocking_recv().expect("proxy terminal output request");
        match job.request {
            ClientRequest::ProxyTerminalOutput(request) => {
                assert_eq!(request.session_id.0.as_ref(), "proxy");
                assert_eq!(request.terminal_id.0.as_ref(), "term-1");
            }
            request => panic!("unexpected request: {request:?}"),
        }
        job.reply
            .send(Ok(ClientRequestResponse::ProxyValue(json!({
                "output": "stdout",
                "chunks": [],
                "totalBytes": 6,
                "truncated": false,
                "exitStatus": null,
                "running": true,
                "elapsedMs": 1_000,
            }))))
            .expect("proxy terminal output response");
    });

    assert_eq!(
        backend.terminal_output(String::from("term-1")).expect("structured result"),
        TerminalOutputResult {
            output: String::from("stdout"),
            chunks: Vec::new(),
            total_bytes: 6,
            truncated: false,
            exit_status: None,
            running: true,
            elapsed_ms: 1_000,
        }
    );
    worker.join().expect("proxy terminal output worker");
}

#[test]
fn proxy_terminal_output_since_filters_chunks_and_reconstructs_output() {
    let (jobs, mut received) = mpsc::unbounded_channel();
    let backend = HostProxyBackend {
        jobs,
        process: Arc::new(Mutex::new(None)),
        threads: Arc::new(Mutex::new(HashMap::new())),
        agent_id: String::from("test-agent"),
        scope: String::from("test"),
        supported_tools: None,
        workspace_memory: WorkspaceMemoryHost::disabled(),
        shutdown: CancellationToken::new(),
    };
    let worker = std::thread::spawn(move || {
        let job = received.blocking_recv().expect("proxy terminal output request");
        match job.request {
            ClientRequest::ProxyTerminalOutput(request) => {
                assert_eq!(request.terminal_id.0.as_ref(), "term-1");
            }
            request => panic!("unexpected request: {request:?}"),
        }
        job.reply
            .send(Ok(ClientRequestResponse::ProxyValue(json!({
                "output": "firstsecondthird",
                "chunks": [
                    { "sequence": 1, "stream": "stdout", "text": "first" },
                    { "sequence": 2, "stream": "stderr", "text": "second" },
                    { "sequence": 3, "stream": "stdout", "text": "third" },
                ],
                "totalBytes": 16,
                "truncated": false,
                "exitStatus": null,
            }))))
            .expect("proxy terminal output response");
    });

    let output = backend.terminal_output_since(String::from("term-1"), 1).expect("filtered result");
    assert_eq!(output.output, "secondthird");
    assert_eq!(output.chunks.iter().map(|chunk| chunk.sequence).collect::<Vec<_>>(), vec![2, 3]);
    worker.join().expect("proxy terminal output worker");
}
