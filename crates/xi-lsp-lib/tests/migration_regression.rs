use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ServiceId;
use ee_plugin_api::model::ViewId;
use lsp_types::{Hover, Position, Range, ServerCapabilities, TextDocumentContentChangeEvent, Uri};
use serde_json::{Value, json};
use xi_lsp_lib::language_server_client::LanguageServerClient;
use xi_lsp_lib::lsp_plugin::HostUi;
use xi_lsp_lib::{ResultQueue, ServerStartOptions, shutdown_language_server, start_new_server};

struct CaptureCaller;

impl ServiceCaller for CaptureCaller {
    fn call_service(
        &self,
        _service: ServiceId,
        _method: &str,
        _params: Value,
    ) -> Result<Value, PluginError> {
        Ok(Value::Null)
    }

    fn submit_effects(
        &self,
        _effects: Vec<ee_plugin_api::effect::Effect>,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

fn test_core_proxy() -> HostUi {
    HostUi::new(std::sync::Arc::new(CaptureCaller))
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
    let binary_path = target_dir.join("debug").join("xi_lsp_fake_server");
    assert!(
        binary_path.exists(),
        "fake server binary `{}` should be built by cargo test",
        binary_path.display()
    );
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

fn read_log_entries(path: &PathBuf) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("log should be readable")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("log line should parse"))
        .collect()
}

#[test]
fn fake_language_server_covers_migration_flows() {
    let log_path = test_log_path("xi-lsp-migration");
    let server_path = fake_server_binary_path();
    let queue = ResultQueue::new();
    let client = start_new_server(
        server_path.display().to_string(),
        vec![log_path.display().to_string()],
        "rust",
        test_core_proxy(),
        queue.clone(),
        ServerStartOptions {
            file_extensions: vec![String::from("rs")],
            ..ServerStartOptions::default()
        },
    )
    .expect("fake language server should start");

    let uri: Uri = "file:///tmp/migration.rs".parse().expect("uri should parse");
    let view_id = ViewId(9);

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

    let methods = read_log_entries(&log_path)
        .into_iter()
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

#[test]
fn fake_language_server_receives_args_env_and_initialization_options() {
    let log_path = test_log_path("xi-lsp-spawn-config");
    let server_path = fake_server_binary_path();
    let client = start_new_server(
        server_path.display().to_string(),
        vec![log_path.display().to_string(), String::from("--probe-arg"), String::from("payload")],
        "gleam",
        test_core_proxy(),
        ResultQueue::new(),
        ServerStartOptions {
            file_extensions: vec![String::from("gleam")],
            env_overrides: BTreeMap::from([(
                String::from("XI_LSP_FAKE_ENV"),
                String::from("configured-value"),
            )]),
            initialization_options: Some(json!({ "phase": 4, "feature": "enabled" })),
        },
    )
    .expect("fake language server should start");

    initialize_client(&client);
    shutdown_language_server(&client).expect("shutdown should succeed");

    let entries = read_log_entries(&log_path);
    let _ = fs::remove_file(log_path);

    let startup = entries
        .iter()
        .find(|entry| entry.get("method") == Some(&Value::String(String::from("process"))))
        .expect("startup log should exist");
    assert_eq!(startup["params"]["args"], json!(["--probe-arg", "payload"]));
    assert_eq!(
        startup["params"]["env"]["XI_LSP_FAKE_ENV"],
        Value::String(String::from("configured-value"))
    );

    let initialize = entries
        .iter()
        .find(|entry| entry.get("method") == Some(&Value::String(String::from("initialize"))))
        .expect("initialize log should exist");
    assert_eq!(
        initialize["params"]["initializationOptions"],
        json!({ "phase": 4, "feature": "enabled" })
    );
}
