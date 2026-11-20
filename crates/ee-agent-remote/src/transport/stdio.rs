//! Newline-delimited JSON-RPC 2.0 over a byte stream.
//!
//! One connection owns one [`Subscription`](crate::events::Subscription);
//! approvals, notifications, and responses share the same bounded outbound
//! queue. Handshake, overload, and dispatch behavior live in
//! [`super::handler::ConnectionHandler`].

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::RemoteError;
use crate::host::RemoteHost;

use super::handler::ConnectionHandler;

/// Largest accepted client frame.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Serves the remote protocol over stdin/stdout until the client closes.
///
/// # Errors
///
/// Returns transport I/O errors; protocol errors are answered in-band.
pub async fn serve_stdio(host: RemoteHost) -> Result<(), RemoteError> {
    let reader = tokio::io::stdin();
    let writer = tokio::io::stdout();
    serve_stream(host, reader, writer).await
}

/// Serves the remote protocol over arbitrary streams (tests and embedding).
///
/// # Errors
///
/// Returns transport I/O errors; protocol errors are answered in-band.
pub async fn serve_stream<R, W>(host: RemoteHost, reader: R, writer: W) -> Result<(), RemoteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let queue_capacity = host.config().max_outbound_queue.max(1);
    let (writer_tx, writer_rx) = mpsc::channel::<Value>(queue_capacity);
    let writer_task = tokio::spawn(writer_loop(writer, writer_rx));

    let subscription = host.bus().subscribe();
    let overloaded = subscription.overloaded.clone();
    let forward_tx = writer_tx.clone();
    let forwarder = tokio::spawn(async move {
        let mut subscription = subscription;
        while let Some(frame) = subscription.receiver.recv().await {
            if forward_tx.send(frame).await.is_err() {
                break;
            }
        }
    });

    let result = read_loop(&host, reader, &writer_tx, overloaded).await;

    drop(writer_tx);
    forwarder.abort();
    let _ = writer_task.await;
    host.shutdown().await;
    result
}

async fn read_loop<R>(
    host: &RemoteHost,
    reader: R,
    writer_tx: &mpsc::Sender<Value>,
    overloaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), RemoteError>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    let mut handler = ConnectionHandler::new(host.clone(), writer_tx.clone(), overloaded);

    loop {
        buffer.clear();
        let read = match reader.read_until(b'\n', &mut buffer).await {
            Ok(read) => read,
            Err(error) => return Err(RemoteError::internal(format!("stdio read: {error}"))),
        };
        if read == 0 {
            return Ok(());
        }
        if buffer.len() > MAX_FRAME_BYTES {
            let error =
                RemoteError::parse_error(format!("frame exceeds the {MAX_FRAME_BYTES}-byte cap"));
            if let Err(error) = writer_tx.try_send(crate::protocol::response_error_null_id(&error))
            {
                tracing::warn!(%error, "dropping oversized-frame error");
            }
            continue;
        }
        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        handler.handle_text(text).await;
    }
}

pub(crate) async fn writer_loop<W>(writer: W, mut receiver: mpsc::Receiver<Value>)
where
    W: AsyncWrite + Unpin,
{
    let mut writer = tokio::io::BufWriter::new(writer);
    while let Some(frame) = receiver.recv().await {
        let mut line = match serde_json::to_vec(&frame) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "outbound frame could not be serialized");
                continue;
            }
        };
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.flush().await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
    use ee_agent_host::{AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    use crate::REMOTE_PROTOCOL_VERSION;
    use crate::config::RemoteHostConfig;

    use super::*;

    #[derive(Clone)]
    struct ScriptedFake(Arc<std::sync::Mutex<Option<FakeAgent>>>);

    impl FakeTransportFactory for ScriptedFake {
        fn build(&self) -> FakeAgentTransport {
            let (agent, transport) = FakeAgent::spawn(streaming_script());
            *self.0.lock().expect("fake handle poisoned") = Some(agent);
            transport
        }
    }

    fn streaming_script() -> FakeAgentScript {
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "s-1" }))
            .wait_for("session/prompt")
            .emit(wire::session_update("s-1", wire::agent_message_chunk("m-1", "hello")))
            .respond(json!({ "stopReason": "end_turn" }))
    }

    struct Harness {
        client: LineClient,
        server: tokio::task::JoinHandle<Result<(), RemoteError>>,
    }

    struct LineClient {
        writer: DuplexStream,
        reader: BufReader<DuplexStream>,
    }

    impl LineClient {
        async fn send(&mut self, value: Value) {
            let mut line = serde_json::to_vec(&value).expect("frame");
            line.push(b'\n');
            self.writer.write_all(&line).await.expect("write");
            self.writer.flush().await.expect("flush");
        }

        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
                .await
                .expect("timed out reading a frame")
                .expect("read");
            serde_json::from_str(&line).expect("json frame")
        }

        /// Reads until the response with `id` arrives, skipping notifications.
        async fn recv_response(&mut self, id: &Value) -> Value {
            loop {
                let frame = self.recv().await;
                if frame.get("method").is_none() && frame.get("id") == Some(id) {
                    return frame;
                }
            }
        }
    }

    fn spawn_harness() -> Harness {
        let fake = ScriptedFake(Arc::new(std::sync::Mutex::new(None)));
        let mut config = RemoteHostConfig {
            request_timeout: Duration::from_secs(5),
            ..RemoteHostConfig::default()
        };
        config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
        config.fake_transports.insert("fake".into(), Arc::new(fake));
        let host = RemoteHost::with_options(
            config,
            Arc::new(ee_agent_host::DenyAllHandler),
            AgentConnectionOptions {
                handshake_timeout: Duration::from_secs(5),
                request_timeout: Duration::from_secs(5),
                ..AgentConnectionOptions::default()
            },
        );
        host.start().expect("pump");
        let (client_writer, server_reader) = tokio::io::duplex(64 * 1024);
        let (server_writer, client_reader) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(serve_stream(host, server_reader, server_writer));
        Harness {
            client: LineClient { writer: client_writer, reader: BufReader::new(client_reader) },
            server,
        }
    }

    #[tokio::test]
    async fn initialize_then_turn_streams_over_stdio_with_exact_ids() {
        let mut harness = spawn_harness();
        harness
            .client
            .send(json!({
                "jsonrpc": "2.0",
                "id": "init-1",
                "method": "initialize",
                "params": { "protocolVersion": REMOTE_PROTOCOL_VERSION, "clientId": "hermes" }
            }))
            .await;
        let initialized = harness.client.recv_response(&json!("init-1")).await;
        assert_eq!(initialized["id"], "init-1");
        assert_eq!(initialized["result"]["protocolVersion"], REMOTE_PROTOCOL_VERSION);

        harness.client.send(json!({ "jsonrpc": "2.0", "method": "initialized" })).await;

        harness
            .client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/new",
                "params": { "agentId": "fake", "cwd": std::env::current_dir().expect("cwd").display().to_string() }
            }))
            .await;
        let session = harness.client.recv_response(&json!(2)).await;
        assert_eq!(
            session["result"]["sessionId"], "s-1",
            "unexpected session/new response: {session}"
        );

        harness
            .client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "turn/start",
                "params": { "sessionId": "s-1", "text": "hi" }
            }))
            .await;

        let mut saw_update = false;
        loop {
            let frame = harness.client.recv().await;
            if frame["method"] == crate::protocol::SESSION_UPDATE {
                saw_update = true;
            }
            if frame["method"] == crate::protocol::TURN_COMPLETED {
                assert_eq!(frame["params"]["status"], "completed");
                break;
            }
        }
        assert!(saw_update);

        drop(harness.client.writer);
        let served = tokio::time::timeout(Duration::from_secs(5), harness.server)
            .await
            .expect("server exit")
            .expect("join");
        assert!(served.is_ok());
    }

    #[tokio::test]
    async fn requests_before_initialized_answer_not_initialized() {
        let mut harness = spawn_harness();
        harness
            .client
            .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {} }))
            .await;
        let response = harness.client.recv_response(&json!(1)).await;
        assert_eq!(response["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn unknown_methods_and_batches_fail_with_typed_codes() {
        let mut harness = spawn_harness();
        harness
            .client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": REMOTE_PROTOCOL_VERSION }
            }))
            .await;
        let _ = harness.client.recv_response(&json!(1)).await;

        harness
            .client
            .send(json!({ "jsonrpc": "2.0", "id": 2, "method": "turn/steer", "params": {} }))
            .await;
        let response = harness.client.recv_response(&json!(2)).await;
        assert_eq!(response["error"]["code"], -32601);

        harness.client.send(json!([{ "jsonrpc": "2.0", "id": 3, "method": "initialize" }])).await;
        let response = harness.client.recv_response(&Value::Null).await;
        assert_eq!(response["error"]["code"], -32600);
    }
}
