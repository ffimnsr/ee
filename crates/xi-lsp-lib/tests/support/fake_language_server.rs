use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufReader, BufWriter, Write};

use lsp_server::{Message, Notification, Response};
use serde_json::{Value, json};

fn append_log(
    log: &mut dyn Write,
    kind: &str,
    method: &str,
    params: Option<Value>,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *log,
        &json!({
            "kind": kind,
            "method": method,
            "params": params,
        }),
    )?;
    writeln!(log)
}

fn main() -> io::Result<()> {
    let log_path = env::args().nth(1).expect("missing log path");
    let mut log = BufWriter::new(OpenOptions::new().create(true).append(true).open(log_path)?);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut current_uri = None;

    while let Some(message) = Message::read(&mut reader)? {
        match message {
            Message::Request(request) => {
                append_log(&mut log, "request", &request.method, Some(request.params.clone()))?;
                match request.method.as_str() {
                    "initialize" => {
                        let response = Response::new_ok(
                            request.id,
                            json!({
                                "capabilities": {
                                    "hoverProvider": true,
                                    "textDocumentSync": 2
                                }
                            }),
                        );
                        Message::Response(response).write(&mut writer)?;
                        writer.flush()?;
                    }
                    "textDocument/hover" => {
                        let response = Response::new_ok(
                            request.id,
                            json!({
                                "contents": {
                                    "kind": "markdown",
                                    "value": "fake hover"
                                }
                            }),
                        );
                        Message::Response(response).write(&mut writer)?;
                        writer.flush()?;
                    }
                    "shutdown" => {
                        Message::Response(Response::new_ok(request.id, Value::Null))
                            .write(&mut writer)?;
                        writer.flush()?;
                    }
                    _ => {
                        Message::Response(Response::new_ok(request.id, Value::Null))
                            .write(&mut writer)?;
                        writer.flush()?;
                    }
                }
            }
            Message::Notification(notification) => {
                append_log(
                    &mut log,
                    "notification",
                    &notification.method,
                    Some(notification.params.clone()),
                )?;
                match notification.method.as_str() {
                    "textDocument/didOpen" => {
                        current_uri = notification
                            .params
                            .get("textDocument")
                            .and_then(|document| document.get("uri"))
                            .cloned();
                        if let Some(uri) = current_uri.clone() {
                            let diagnostics = Notification::new(
                                String::from("textDocument/publishDiagnostics"),
                                json!({
                                    "uri": uri,
                                    "diagnostics": [{
                                        "range": {
                                            "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 2 }
                                        },
                                        "severity": 2,
                                        "message": "open diagnostic",
                                        "source": "fake-server"
                                    }]
                                }),
                            );
                            Message::Notification(diagnostics).write(&mut writer)?;
                            writer.flush()?;
                        }
                    }
                    "textDocument/didChange" => {
                        if let Some(uri) = current_uri.clone() {
                            let diagnostics = Notification::new(
                                String::from("textDocument/publishDiagnostics"),
                                json!({
                                    "uri": uri,
                                    "diagnostics": [{
                                        "range": {
                                            "start": { "line": 0, "character": 3 },
                                            "end": { "line": 0, "character": 9 }
                                        },
                                        "severity": 1,
                                        "message": "changed diagnostic",
                                        "source": "fake-server"
                                    }]
                                }),
                            );
                            Message::Notification(diagnostics).write(&mut writer)?;
                            writer.flush()?;
                        }
                    }
                    "exit" => break,
                    _ => {}
                }
            }
            Message::Response(response) => {
                append_log(&mut log, "response", "client-response", Some(json!(response)))?;
            }
        }
    }

    Ok(())
}
