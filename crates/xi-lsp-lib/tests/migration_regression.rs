use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use lsp_types::{Hover, Position, Range, ServerCapabilities, TextDocumentContentChangeEvent, Uri};
use serde_json::{Value, json};
use xi_lsp_lib::language_server_client::LanguageServerClient;
use xi_lsp_lib::{ResultQueue, shutdown_language_server, start_new_server};
use xi_plugin_lib::CoreProxy;
use xi_rpc::test_utils::make_reader;
use xi_rpc::{Handler, NewlineWriter, RpcCtx, RpcLoop};

#[derive(Default)]
struct CaptureCoreProxy {
    proxy: Option<CoreProxy>,
}

impl Handler for CaptureCoreProxy {
    type Notification = serde_json::Value;
    type Request = serde_json::Value;

    fn handle_notification(&mut self, ctx: &RpcCtx, _rpc: Self::Notification) {
        let plugin_id = serde_json::from_value(json!(1)).expect("plugin id should deserialize");
        self.proxy = Some(CoreProxy::new(plugin_id, ctx, 1, []));
        ctx.get_peer().request_shutdown();
    }

    fn handle_request(
        &mut self,
        _ctx: &RpcCtx,
        _rpc: Self::Request,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, xi_rpc::RemoteError> {
        Ok(serde_json::Value::Null)
    }
}

fn test_core_proxy() -> CoreProxy {
    let mut handler = CaptureCoreProxy::default();
    let mut looper = RpcLoop::new(NewlineWriter::new(io::sink()));
    let reader = make_reader(r#"{"method":"ping","params":{}}"#);
    looper.mainloop(|| reader, &mut handler).expect("test rpc loop should exit cleanly");
    handler.proxy.expect("core proxy should be captured")
}

fn wait_until(label: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {label}");
}

fn test_log_path(name: &str) -> PathBuf {
    let unique = format!(
        "{}-{}-{}.jsonl",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be valid")
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

fn fake_server_binary_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_xi_lsp_fake_server") {
        return PathBuf::from(path);
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root should exist")
        .to_path_buf();
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let binary_name = format!("xi_lsp_fake_server{}", std::env::consts::EXE_SUFFIX);
    let binary_path = target_dir.join("debug").join(binary_name);
    if binary_path.exists() {
        return binary_path;
    }

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&workspace_root)
        .args(["build", "-p", "xi-lsp-lib", "--bin", "xi_lsp_fake_server"])
        .status()
        .expect("cargo build for fake server should start");
    assert!(status.success(), "cargo build for fake server should succeed");
    assert!(binary_path.exists(), "fake server binary should exist after build");
    binary_path
}

fn initialize_client(client: &std::sync::Arc<std::sync::Mutex<LanguageServerClient>>) {
    let (tx, rx) = mpsc::channel();
    client
        .lock()
        .expect("client lock should succeed")
        .send_initialize(None, move |client, result| {
            client.initialization_pending = false;
            match result {
                Ok(_) => {
                    client.server_capabilities = Some(ServerCapabilities::default());
                    client.is_initialized = true;
                    client.clear_server_failure();
                    client
                        .resend_open_documents()
                        .expect("open documents should resend after initialize");
                    tx.send(()).expect("initialize completion should notify test");
                }
                Err(err) => panic!("initialize request should succeed: {err:?}"),
            }
        })
        .expect("initialize request should send");
    rx.recv_timeout(Duration::from_secs(2)).expect("initialize callback should complete");
}

#[test]
fn fake_language_server_covers_migration_flows() {
    let log_path = test_log_path("xi-lsp-migration");
    let server_path = fake_server_binary_path();
    let queue = ResultQueue::new();
    let client = start_new_server(
        server_path.display().to_string(),
        vec![log_path.display().to_string()],
        vec![String::from("rs")],
        "rust",
        test_core_proxy(),
        queue.clone(),
    )
    .expect("fake language server should start");

    let uri: Uri = "file:///tmp/migration.rs".parse().expect("uri should parse");
    let view_id = 9.into();

    client
        .lock()
        .expect("client lock should succeed")
        .send_did_open(view_id, uri.clone(), String::from("fn main() {}\n"))
        .expect("didOpen state update should succeed");
    initialize_client(&client);

    wait_until("initialize", Duration::from_secs(2), || {
        client.lock().expect("client lock should succeed").is_initialized
    });

    wait_until("open diagnostics", Duration::from_secs(2), || {
        client
            .lock()
            .expect("client lock should succeed")
            .opened_documents
            .get(&view_id)
            .map(|state| !state.diagnostics.is_empty())
            .unwrap_or(false)
    });

    client
        .lock()
        .expect("client lock should succeed")
        .send_did_change(
            view_id,
            vec![TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 3), Position::new(0, 7))),
                range_length: Some(4),
                text: String::from("updated"),
            }],
            1,
            String::from("fn updated() {}\n"),
        )
        .expect("didChange should send");

    wait_until("changed diagnostics", Duration::from_secs(2), || {
        client
            .lock()
            .expect("client lock should succeed")
            .opened_documents
            .get(&view_id)
            .and_then(|state| state.diagnostics.first())
            .map(|diagnostic| diagnostic.message == "changed diagnostic")
            .unwrap_or(false)
    });
    let diagnostics = client
        .lock()
        .expect("client lock should succeed")
        .opened_documents
        .get(&view_id)
        .expect("view should remain open")
        .diagnostics
        .clone();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "changed diagnostic");

    client
        .lock()
        .expect("client lock should succeed")
        .send_did_save(view_id, "fn updated() {}\n")
        .expect("didSave should send");

    let (hover_tx, hover_rx) = mpsc::channel();
    client
        .lock()
        .expect("client lock should succeed")
        .request_hover(view_id, Position::new(0, 1), move |_client, result| {
            hover_tx.send(result).expect("hover result channel should send");
        })
        .expect("hover should send");

    let hover = hover_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("hover response should arrive")
        .expect("hover response should succeed");
    let hover = serde_json::from_value::<Option<Hover>>(hover)
        .expect("hover payload should parse")
        .expect("hover should be present");
    match hover.contents {
        lsp_types::HoverContents::Markup(markup) => assert_eq!(markup.value, "fake hover"),
        other => panic!("unexpected hover contents: {other:?}"),
    }

    client
        .lock()
        .expect("client lock should succeed")
        .send_did_close(view_id)
        .expect("didClose should send");
    shutdown_language_server(&client).expect("shutdown should succeed");

    let methods = fs::read_to_string(&log_path)
        .expect("log should be readable")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("log line should parse"))
        .map(|entry| {
            entry
                .get("method")
                .and_then(Value::as_str)
                .expect("log entry method should exist")
                .to_string()
        })
        .collect::<Vec<_>>();
    let _ = fs::remove_file(log_path);

    assert!(methods.contains(&String::from("initialize")));
    assert!(methods.contains(&String::from("textDocument/didOpen")));
    assert!(methods.contains(&String::from("textDocument/didChange")));
    assert!(methods.contains(&String::from("textDocument/didSave")));
    assert!(methods.contains(&String::from("textDocument/hover")));
    assert!(methods.contains(&String::from("textDocument/didClose")));
    assert!(methods.contains(&String::from("shutdown")));
    assert!(methods.contains(&String::from("exit")));
}
