// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use log::{error, warn};
use lsp_server::Message as LspServerMessage;
use url::Url;
use xi_plugin_lib::{Cache, ChunkCache, CoreProxy, Error as PluginLibError, PluginEditAck, View};
use xi_rope::rope::RopeDelta;
use xi_rope::{DeltaBuilder, Interval, Rope};

use crate::conversion_utils::*;
use crate::language_server_client::LanguageServerClient;
use crate::result_queue::ResultQueue;
use crate::types::Error;

const MAX_LSP_BODY_BYTES: usize = 16 * 1024 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
use lsp_types::Uri;
use lsp_types::*;

fn stderr_is_user_visible(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("panic")
        || lower.contains("panicked")
        || lower.contains("error")
        || lower.contains("failed")
}

#[doc(hidden)]
pub fn read_transport_message<R: BufRead>(
    reader: &mut R,
) -> Result<Option<LspServerMessage>, Error> {
    let Some(message) = lsp_server::Message::read(reader)? else {
        return Ok(None);
    };

    let message_bytes = serde_json::to_vec(&message)?;
    if message_bytes.len() > MAX_LSP_BODY_BYTES {
        return Err(Error::Protocol(format!(
            "LSP message too large ({} bytes)",
            message_bytes.len()
        )));
    }

    Ok(Some(message))
}

pub fn file_path_to_uri(path: &Path) -> Result<Uri, Error> {
    Url::from_file_path(path)
        .map_err(|_| Error::FileUrlParseError)?
        .as_str()
        .parse()
        .map_err(|_| Error::FileUrlParseError)
}

/// Get contents changes of a document modeled according to Language Server Protocol
/// given the RopeDelta
fn document_content_changes_from_delta<FP>(
    delta: Option<&RopeDelta>,
    mut position_of_offset: FP,
    document_text: String,
) -> Result<Vec<TextDocumentContentChangeEvent>, PluginLibError>
where
    FP: FnMut(usize) -> Result<Position, PluginLibError>,
{
    if let Some(delta) = delta {
        let (interval, _) = delta.summary();
        let (start, end) = interval.start_end();

        if let Some(node) = delta.as_simple_insert() {
            let text = String::from(node);

            return Ok(vec![TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: position_of_offset(start)?,
                    end: position_of_offset(end)?,
                }),
                range_length: Some(
                    u32::try_from(end - start).map_err(|_| PluginLibError::BadRequest)?,
                ),
                text,
            }]);
        } else if delta.is_simple_delete() {
            let mut end_position = position_of_offset(end)?;

            if end_position.character == 0 {
                let mut ep = position_of_offset(end - 1)?;
                ep.character += 1;
                end_position = ep;
            }

            return Ok(vec![TextDocumentContentChangeEvent {
                range: Some(Range { start: position_of_offset(start)?, end: end_position }),
                range_length: Some(
                    u32::try_from(end - start).map_err(|_| PluginLibError::BadRequest)?,
                ),
                text: String::new(),
            }]);
        }
    }

    Ok(vec![TextDocumentContentChangeEvent {
        range: None,
        range_length: None,
        text: document_text,
    }])
}

pub fn get_document_content_changes<C: Cache>(
    delta: Option<&RopeDelta>,
    view: &mut View<C>,
) -> Result<Vec<TextDocumentContentChangeEvent>, PluginLibError> {
    let document_text = view.get_document()?;
    document_content_changes_from_delta(
        delta,
        |offset| get_position_of_offset(view, offset),
        document_text,
    )
}

/// Get changes to be sent to server depending upon the type of Sync supported
/// by server
pub fn get_change_for_sync_kind(
    sync_kind: TextDocumentSyncKind,
    view: &mut View<ChunkCache>,
    delta: Option<&RopeDelta>,
) -> Option<Vec<TextDocumentContentChangeEvent>> {
    match sync_kind {
        TextDocumentSyncKind::NONE => None,
        TextDocumentSyncKind::FULL => {
            let text = match view.get_document() {
                Ok(text) => text,
                Err(err) => {
                    warn!("Error: {:?} Occured. Skipping didChange", err);
                    return None;
                }
            };
            let text_document_content_change_event =
                TextDocumentContentChangeEvent { range: None, range_length: None, text };
            Some(vec![text_document_content_change_event])
        }
        TextDocumentSyncKind::INCREMENTAL => match get_document_content_changes(delta, view) {
            Ok(result) => Some(result),
            Err(err) => {
                warn!("Error: {:?} Occured. Sending Whole Doc", err);
                let Ok(text) = view.get_document() else {
                    warn!("Error: {:?} Occured. Skipping didChange fallback", err);
                    return None;
                };
                let text_document_content_change_event =
                    TextDocumentContentChangeEvent { range: None, range_length: None, text };
                Some(vec![text_document_content_change_event])
            }
        },
        _ => {
            let Ok(text) = view.get_document() else {
                warn!("Failed to fetch document for full didChange fallback");
                return None;
            };
            let text_document_content_change_event =
                TextDocumentContentChangeEvent { range: None, range_length: None, text };
            Some(vec![text_document_content_change_event])
        }
    }
}

pub(crate) fn delta_from_lsp_text_edits<C: Cache>(
    view: &mut View<C>,
    edits: &[TextEdit],
) -> Result<RopeDelta, Error> {
    let document_text = view
        .get_document()
        .map_err(|err| Error::Protocol(format!("document fetch failed: {err:?}")))?;
    let mut resolved_edits = edits
        .iter()
        .map(|edit| {
            let start = offset_of_position_in_document(&document_text, edit.range.start)
                .map_err(|err| Error::Protocol(format!("invalid edit start: {err:?}")))?;
            let end = offset_of_position_in_document(&document_text, edit.range.end)
                .map_err(|err| Error::Protocol(format!("invalid edit end: {err:?}")))?;
            Ok((start, end, edit.new_text.clone()))
        })
        .collect::<Result<Vec<_>, Error>>()?;

    resolved_edits.sort_by_key(|(start, end, _)| (*start, *end));
    let mut builder = DeltaBuilder::new(document_text.len());
    let mut last_end = 0usize;

    for (start, end, new_text) in resolved_edits {
        if start < last_end {
            return Err(Error::Protocol(String::from("overlapping text edits are not supported")));
        }
        builder.replace(Interval::new(start, end), Rope::from(new_text));
        last_end = end;
    }

    Ok(builder.build())
}

pub(crate) fn apply_lsp_text_edits<C: Cache>(
    view: &mut View<C>,
    edits: &[TextEdit],
    author: &str,
) -> Result<PluginEditAck, Error> {
    let delta = delta_from_lsp_text_edits(view, edits)?;
    view.try_edit(delta, 0, false, true, author.to_string())
        .map_err(|err| Error::Protocol(format!("failed to apply plugin edit: {err:?}")))
}

fn spawn_stdout_thread(
    language_id: &str,
    stdout: std::process::ChildStdout,
    ls_client: Arc<Mutex<LanguageServerClient>>,
) -> Result<(), Error> {
    let thread_name = format!("{}-lsp-stdout-looper", language_id);
    let language_id = language_id.to_owned();
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_transport_message(&mut reader) {
                    Ok(Some(msg)) => {
                        let Ok(mut server_locked) = ls_client.lock() else {
                            error!("language server {} client lock poisoned", language_id);
                            break;
                        };
                        server_locked.handle_lsp_message(msg);
                    }
                    Ok(None) => {
                        let Ok(mut server_locked) = ls_client.lock() else {
                            error!("language server {} client lock poisoned", language_id);
                            break;
                        };
                        server_locked.initialization_pending = false;
                        server_locked.is_initialized = false;
                        server_locked.server_capabilities = None;
                        server_locked.fail_pending_requests("language server stdout closed");
                        server_locked.record_server_failure("language server stdout closed");
                        break;
                    }
                    Err(err) => {
                        let Ok(mut server_locked) = ls_client.lock() else {
                            error!("language server {} client lock poisoned", language_id);
                            break;
                        };
                        server_locked.initialization_pending = false;
                        server_locked.is_initialized = false;
                        server_locked.server_capabilities = None;
                        server_locked.fail_pending_requests("language server read failed");
                        server_locked
                            .record_server_failure(format!("language server read failed: {err}"));
                        break;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|err| Error::ServerStart {
            context: "stdout thread spawn",
            message: err.to_string(),
        })
}

fn spawn_stderr_thread(
    language_id: &str,
    stderr: std::process::ChildStderr,
    ls_client: Arc<Mutex<LanguageServerClient>>,
) -> Result<(), Error> {
    let thread_name = format!("{}-lsp-stderr-looper", language_id);
    let language_id = language_id.to_owned();
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => (),
                    Ok(line) => {
                        error!("language server {} stderr: {}", language_id, line);
                        if stderr_is_user_visible(&line) {
                            let Ok(mut server_locked) = ls_client.lock() else {
                                error!("language server {} client lock poisoned", language_id);
                                break;
                            };
                            server_locked.record_server_failure(line);
                        }
                    }
                    Err(err) => {
                        error!("language server {} stderr read failed: {:?}", language_id, err);
                        break;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|err| Error::ServerStart {
            context: "stderr thread spawn",
            message: err.to_string(),
        })
}

pub fn shutdown_language_server(ls_client: &Arc<Mutex<LanguageServerClient>>) -> Result<(), Error> {
    let (tx, rx) = mpsc::channel();
    let process = {
        let mut client =
            ls_client.lock().map_err(|_| Error::LockPoisoned("language server client"))?;
        let _ = client.try_send_request(
            "shutdown",
            jsonrpc_lite::Params::None(()),
            Box::new(move |client: &mut LanguageServerClient, result| {
                if let Err(err) = result {
                    client.record_server_failure(format!("shutdown request failed: {err:?}"));
                }
                let _ = tx.send(());
            }),
        );
        let _ = client.send_notification("exit", jsonrpc_lite::Params::None(()));
        client.process_handle()
    };

    let _ = rx.recv_timeout(SHUTDOWN_TIMEOUT);
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        let mut process_guard =
            process.lock().map_err(|_| Error::LockPoisoned("language server process"))?;
        match process_guard.try_wait()? {
            Some(_) => return Ok(()),
            None if Instant::now() >= deadline => {
                process_guard.kill()?;
                let _ = process_guard.wait();
                return Ok(());
            }
            None => drop(process_guard),
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Get workspace root using the Workspace Identifier and the opened document path
/// For example: Cargo.toml can be used to identify a Rust Workspace
/// This method traverses up to file tree to return the path to the Workspace root folder
pub fn get_workspace_root_uri(
    workspace_identifier: &str,
    document_path: &Path,
) -> Result<Uri, Error> {
    let identifier_os_str = OsStr::new(&workspace_identifier);

    let mut current_path = document_path;
    loop {
        let parent_path = current_path.parent();
        if let Some(path) = parent_path {
            for entry in (path.read_dir()?).flatten() {
                if entry.file_name() == identifier_os_str {
                    return file_path_to_uri(path);
                };
            }
            current_path = path;
        } else {
            break Err(Error::PathError);
        }
    }
}

/// Start a new Language Server Process by spawning a process given the parameters
/// Returns a Arc to the Language Server Client which abstracts connection to the
/// server
pub fn start_new_server(
    command: String,
    arguments: Vec<String>,
    file_extensions: Vec<String>,
    language_id: &str,
    core: CoreProxy,
    result_queue: ResultQueue,
) -> Result<Arc<Mutex<LanguageServerClient>>, Error> {
    let mut process = Command::new(command)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| Error::ServerStart { context: "process spawn", message: err.to_string() })?;

    let stdin = process.stdin.take().ok_or_else(|| Error::ServerStart {
        context: "stdin capture",
        message: String::from("missing child stdin"),
    })?;
    let stdout = process.stdout.take().ok_or_else(|| Error::ServerStart {
        context: "stdout capture",
        message: String::from("missing child stdout"),
    })?;
    let stderr = process.stderr.take().ok_or_else(|| Error::ServerStart {
        context: "stderr capture",
        message: String::from("missing child stderr"),
    })?;

    let process = Arc::new(Mutex::new(process));
    let writer = Box::new(BufWriter::new(stdin));

    let language_server_client = Arc::new(Mutex::new(LanguageServerClient::new(
        writer,
        Arc::clone(&process),
        core,
        result_queue,
        language_id.to_owned(),
        file_extensions,
    )));

    spawn_stdout_thread(language_id, stdout, Arc::clone(&language_server_client))?;
    spawn_stderr_thread(language_id, stderr, Arc::clone(&language_server_client))?;

    Ok(language_server_client)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::Cursor;
    use std::thread;

    use xi_rope::{DeltaBuilder, Interval, Rope};

    use serde_json::json;
    use xi_plugin_lib::CoreProxy;
    use xi_rpc::test_utils::make_reader;
    use xi_rpc::{Handler, NewlineWriter, RpcCtx, RpcLoop};

    use super::*;

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

    #[test]
    fn start_new_server_records_visible_stderr() {
        let client = start_new_server(
            String::from("sh"),
            vec![String::from("-c"), String::from("echo request failed 1>&2; tail -f /dev/null")],
            vec![String::from("rs")],
            "rust",
            test_core_proxy(),
            ResultQueue::new(),
        )
        .expect("test language server should spawn");

        thread::sleep(Duration::from_millis(50));

        let client_guard = client.lock().expect("client lock should succeed");
        assert!(client_guard.status_items.contains("lsp:rust:status"));
        drop(client_guard);
        shutdown_language_server(&client).expect("shutdown helper should succeed");
    }

    #[test]
    fn shutdown_language_server_terminates_unresponsive_process() {
        let client = start_new_server(
            String::from("sh"),
            vec![String::from("-c"), String::from("tail -f /dev/null")],
            vec![String::from("rs")],
            "rust",
            test_core_proxy(),
            ResultQueue::new(),
        )
        .expect("test language server should spawn");

        shutdown_language_server(&client).expect("shutdown helper should succeed");

        let client = client.lock().expect("client lock should succeed");
        assert!(client.exit_status().expect("exit status should be readable").is_some());
    }

    #[test]
    fn incremental_sync_reports_simple_insertions() {
        let base = Rope::from("hello world");
        let mut builder = DeltaBuilder::new(base.len());
        builder.replace(Interval::new(5, 5), Rope::from(","));
        let delta = builder.build();

        let changes = document_content_changes_from_delta(
            Some(&delta),
            |offset| {
                Ok(match offset {
                    5 => Position::new(0, 5),
                    _ => Position::new(0, offset as u32),
                })
            },
            String::from("hello, world"),
        )
        .expect("insert changes should succeed");

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].text, ",");
        assert_eq!(changes[0].range, Some(Range::new(Position::new(0, 5), Position::new(0, 5))));
    }

    #[test]
    fn read_transport_message_parses_valid_frame() {
        let body =
            r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"ok"}}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = Cursor::new(framed.into_bytes());

        let message = read_transport_message(&mut reader)
            .expect("frame should parse")
            .expect("frame should contain message");

        match message {
            lsp_server::Message::Notification(notification) => {
                assert_eq!(notification.method, "window/logMessage");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn read_transport_message_rejects_oversized_frame() {
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"window/logMessage","params":{{"type":3,"message":"{}"}}}}"#,
            "a".repeat(MAX_LSP_BODY_BYTES + 1)
        );
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = Cursor::new(framed.into_bytes());

        let err = read_transport_message(&mut reader).expect_err("oversized frame should fail");
        assert!(err.to_string().contains("LSP message too large"));
    }

    #[test]
    fn incremental_sync_reports_simple_deletions() {
        let base = Rope::from("hello world");
        let mut builder = DeltaBuilder::new(base.len());
        builder.delete(Interval::new(5, 6));
        let delta = builder.build();

        let changes = document_content_changes_from_delta(
            Some(&delta),
            |offset| {
                Ok(match offset {
                    5 => Position::new(0, 5),
                    6 => Position::new(0, 6),
                    _ => Position::new(0, offset as u32),
                })
            },
            String::from("helloworld"),
        )
        .expect("delete changes should succeed");

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].text, "");
        assert_eq!(changes[0].range, Some(Range::new(Position::new(0, 5), Position::new(0, 6))));
        assert_eq!(changes[0].range_length, Some(1));
    }

    #[test]
    fn incremental_sync_selection_replacement_falls_back_to_full_document() {
        let base = Rope::from("hello world");
        let mut builder = DeltaBuilder::new(base.len());
        builder.replace(Interval::new(0, 5), Rope::from("hi"));
        let delta = builder.build();

        let changes = document_content_changes_from_delta(
            Some(&delta),
            |_offset| Ok(Position::new(0, 0)),
            String::from("hi world"),
        )
        .expect("fallback changes should succeed");

        assert_eq!(changes.len(), 1);
        assert!(changes[0].range.is_none());
        assert_eq!(changes[0].text, "hi world");
    }

    #[test]
    fn incremental_sync_without_delta_falls_back_to_full_document() {
        let changes = document_content_changes_from_delta(
            None,
            |_offset| Ok(Position::new(0, 0)),
            String::from("full text"),
        )
        .expect("full document fallback should succeed");

        assert_eq!(changes.len(), 1);
        assert!(changes[0].range.is_none());
        assert_eq!(changes[0].text, "full text");
    }
}
