//! Transport abstraction over newline-delimited JSON-RPC frames.
//!
//! [`AcpTransport`] is the byte boundary between the framework and the
//! outside world.  [`StdioTransport`] speaks newline-delimited JSON-RPC over
//! stdin/stdout with a hard `max_frame_bytes` cap enforced *before*
//! parsing; EOF is a clean shutdown.  `MemoryTransport` (feature
//! `test-utils`) is an in-process queue transport so tests never need to
//! spawn real subprocesses.
//!
//! Note on the frame type: the SDK's `JsonRpcMessage` is a *trait* for typed
//! messages, not a wire struct, so it cannot be returned by value.  The
//! transport boundary therefore uses the SDK's raw wire envelope
//! ([`JsonRpcFrame`], backed by `agent_client_protocol::RawJsonRpcMessage`)
//! via the `ee_agent_protocol` re-export.

use std::future::Future;

use ee_agent_protocol::RawJsonRpcMessage;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::AcpServerError;
use crate::validate::validate_frame_len;

/// One JSON-RPC wire frame exchanged over a transport: a request, a
/// notification, or a response.  SDK-backed, never an ee-owned wire struct.
pub type JsonRpcFrame = RawJsonRpcMessage;

/// A transport that exchanges newline-delimited JSON-RPC frames.
///
/// `read_message` returns `Ok(None)` on a clean EOF (transport closed) and
/// `Err` on I/O, oversized, or malformed frames.  `write_message` emits one
/// frame per line and flushes before returning.
///
/// The trait is `Send + 'static` so transports can move across tasks, and
/// both methods return `Send` futures so later phases can spawn dispatch
/// tasks on generic transports.
pub trait AcpTransport: Send + 'static {
    /// Reads the next frame, or `Ok(None)` when the transport hit a clean
    /// EOF.
    fn read_message(
        &mut self,
    ) -> impl Future<Output = Result<Option<JsonRpcFrame>, AcpServerError>> + Send;

    /// Writes one frame as a single newline-terminated line.
    fn write_message(
        &mut self,
        frame: JsonRpcFrame,
    ) -> impl Future<Output = Result<(), AcpServerError>> + Send;
}

/// Newline-delimited JSON-RPC codec with a hard frame-size cap.
///
/// Generic over the underlying reader/writer so unit tests can drive it
/// with in-memory buffers.
struct LineFrameCodec<R, W> {
    reader: BufReader<R>,
    writer: W,
    max_frame_bytes: usize,
}

impl<R, W> LineFrameCodec<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    fn new(max_frame_bytes: usize, reader: R, writer: W) -> Self {
        Self { reader: BufReader::new(reader), writer, max_frame_bytes }
    }

    /// Reads one bounded, newline-terminated frame.
    ///
    /// Returns `Ok(None)` on a clean EOF.  A final frame without a trailing
    /// newline is still honored; blank lines are skipped.  Frames longer
    /// than `max_frame_bytes` fail closed before any parsing.
    async fn read_frame(&mut self) -> Result<Option<JsonRpcFrame>, AcpServerError> {
        loop {
            let mut buffer = Vec::new();
            loop {
                let read =
                    self.reader.read_until(b'\n', &mut buffer).await.map_err(AcpServerError::Io)?;
                if read == 0 {
                    break; // EOF (clean, or with a partial final frame in `buffer`)
                }
                if buffer.len() > self.max_frame_bytes {
                    tracing::warn!(
                        frame_bytes = buffer.len(),
                        max = self.max_frame_bytes,
                        "dropping oversized JSON-RPC frame"
                    );
                }
                // Fail closed before any parsing; also covers a partial line
                // that already exceeded the cap before its newline arrived.
                validate_frame_len(buffer.len(), self.max_frame_bytes)?;
                if buffer.last() == Some(&b'\n') {
                    break;
                }
            }
            if buffer.is_empty() {
                return Ok(None); // clean EOF
            }
            while buffer.last().is_some_and(|byte| matches!(byte, b'\n' | b'\r')) {
                buffer.pop();
            }
            if buffer.iter().all(u8::is_ascii_whitespace) {
                continue; // blank line: keep reading
            }

            let value: serde_json::Value =
                serde_json::from_slice(&buffer).map_err(|source| AcpServerError::JsonParse {
                    raw: String::from_utf8_lossy(&buffer).into_owned(),
                    source,
                })?;
            let frame = serde_json::from_value(value).map_err(|source| {
                AcpServerError::Protocol(format!("not a valid JSON-RPC message: {source}"))
            })?;
            return Ok(Some(frame));
        }
    }

    /// Writes one frame as a single line, then flushes.
    async fn write_frame(&mut self, frame: &JsonRpcFrame) -> Result<(), AcpServerError> {
        let line = serde_json::to_string(frame).map_err(|source| {
            AcpServerError::Protocol(format!("failed to serialize JSON-RPC frame: {source}"))
        })?;
        if line.len() > self.max_frame_bytes {
            validate_frame_len(line.len(), self.max_frame_bytes)?;
        }
        self.writer.write_all(line.as_bytes()).await.map_err(AcpServerError::Io)?;
        self.writer.write_all(b"\n").await.map_err(AcpServerError::Io)?;
        self.writer.flush().await.map_err(AcpServerError::Io)
    }
}

/// [`AcpTransport`] over stdin/stdout.
///
/// Reads newline-delimited JSON-RPC messages from stdin and writes one
/// JSON-RPC message per line to stdout, flushing after every message.
pub struct StdioTransport {
    codec: LineFrameCodec<tokio::io::Stdin, tokio::io::Stdout>,
}

impl StdioTransport {
    /// Creates a stdio transport enforcing the given frame-size cap.
    #[must_use]
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            codec: LineFrameCodec::new(max_frame_bytes, tokio::io::stdin(), tokio::io::stdout()),
        }
    }
}

impl AcpTransport for StdioTransport {
    async fn read_message(&mut self) -> Result<Option<JsonRpcFrame>, AcpServerError> {
        self.codec.read_frame().await
    }

    async fn write_message(&mut self, frame: JsonRpcFrame) -> Result<(), AcpServerError> {
        self.codec.write_frame(&frame).await
    }
}

/// In-process queue transport for tests (feature `test-utils`).
///
/// Inbound messages are fed through [`MemoryTransportHandle::send`];
/// outbound messages are captured in order and read back with
/// [`MemoryTransport::outbound`] / [`MemoryTransport::take_outbound`].
/// Closing (or dropping) the handle injects EOF: `read_message` then
/// returns `Ok(None)` once queued messages are drained.
#[cfg(feature = "test-utils")]
pub struct MemoryTransport {
    inbound: futures::channel::mpsc::UnboundedReceiver<JsonRpcFrame>,
    outbound: std::sync::Arc<std::sync::Mutex<Vec<JsonRpcFrame>>>,
}

/// Clonable handle that feeds messages into a [`MemoryTransport`] and reads
/// back what the transport writes out.
#[cfg(feature = "test-utils")]
#[derive(Clone)]
pub struct MemoryTransportHandle {
    inbound: futures::channel::mpsc::UnboundedSender<JsonRpcFrame>,
    outbound: std::sync::Arc<std::sync::Mutex<Vec<JsonRpcFrame>>>,
}

#[cfg(feature = "test-utils")]
impl MemoryTransport {
    /// Creates a transport paired with a handle used to inject messages and
    /// inspect outbound messages while the transport runs inside a server
    /// task.
    #[must_use]
    pub fn new() -> (Self, MemoryTransportHandle) {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let outbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self { inbound: receiver, outbound: outbound.clone() },
            MemoryTransportHandle { inbound: sender, outbound },
        )
    }

    /// Snapshot of all outbound messages written so far, in order.
    #[must_use]
    pub fn outbound(&self) -> Vec<JsonRpcFrame> {
        self.outbound.lock().expect("memory transport outbound lock poisoned").clone()
    }

    /// Takes all outbound messages written so far, clearing the capture.
    #[must_use]
    pub fn take_outbound(&self) -> Vec<JsonRpcFrame> {
        std::mem::take(&mut *self.outbound.lock().expect("memory transport outbound lock poisoned"))
    }
}

#[cfg(feature = "test-utils")]
impl MemoryTransportHandle {
    /// Queues one inbound message; returns `false` if the transport side is
    /// already closed.
    pub fn send(&self, frame: JsonRpcFrame) -> bool {
        self.inbound.unbounded_send(frame).is_ok()
    }

    /// Injects EOF: the transport's `read_message` returns `Ok(None)` after
    /// queued messages are drained.  Dropping the last handle has the same
    /// effect.
    pub fn close(&mut self) {
        self.inbound.close_channel();
    }

    /// Snapshot of all outbound messages written so far, in order.
    #[must_use]
    pub fn outbound(&self) -> Vec<JsonRpcFrame> {
        self.outbound.lock().expect("memory transport outbound lock poisoned").clone()
    }

    /// Takes all outbound messages written so far, clearing the capture.
    #[must_use]
    pub fn take_outbound(&self) -> Vec<JsonRpcFrame> {
        std::mem::take(&mut *self.outbound.lock().expect("memory transport outbound lock poisoned"))
    }
}

#[cfg(feature = "test-utils")]
impl AcpTransport for MemoryTransport {
    async fn read_message(&mut self) -> Result<Option<JsonRpcFrame>, AcpServerError> {
        use futures::StreamExt;
        Ok(self.inbound.next().await)
    }

    async fn write_message(&mut self, frame: JsonRpcFrame) -> Result<(), AcpServerError> {
        self.outbound.lock().expect("memory transport outbound lock poisoned").push(frame);
        Ok(())
    }
}

#[cfg(feature = "test-utils")]
impl std::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTransport").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::RequestId;

    fn parse_frame(json: &str) -> JsonRpcFrame {
        serde_json::from_str(json).expect("test frame parses")
    }

    fn frame_json(frame: &JsonRpcFrame) -> String {
        serde_json::to_string(frame).expect("test frame serializes")
    }

    /// `RawJsonRpcMessage` carries no `PartialEq`, so compare frames by
    /// their serialized wire form.
    fn assert_frame_eq(actual: Option<JsonRpcFrame>, expected: Option<JsonRpcFrame>) {
        assert_eq!(actual.as_ref().map(frame_json), expected.as_ref().map(frame_json),);
    }

    fn assert_frames_eq(actual: Vec<JsonRpcFrame>, expected: Vec<JsonRpcFrame>) {
        assert_eq!(
            actual.iter().map(frame_json).collect::<Vec<_>>(),
            expected.iter().map(frame_json).collect::<Vec<_>>(),
        );
    }

    fn codec<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        max_frame_bytes: usize,
        reader: R,
        writer: W,
    ) -> LineFrameCodec<R, W> {
        LineFrameCodec::new(max_frame_bytes, reader, writer)
    }

    const INITIALIZE: &str =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;

    #[tokio::test]
    async fn valid_request_frame_parses() {
        let mut codec = codec(1024, INITIALIZE.as_bytes(), Vec::new());
        let frame = codec.read_frame().await.expect("frame reads");
        let JsonRpcFrame::Request(request) = frame.expect("one frame") else {
            panic!("expected request frame");
        };
        assert_eq!(request.id, RequestId::Number(1));
        assert_eq!(request.method.as_ref(), "initialize");
    }

    #[tokio::test]
    async fn valid_response_frame_parses() {
        let input = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        let frame = codec.read_frame().await.expect("frame reads").expect("one frame");
        assert!(matches!(frame, JsonRpcFrame::Response(_)));
    }

    #[tokio::test]
    async fn valid_notification_frame_parses() {
        let input = r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#;
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        let frame = codec.read_frame().await.expect("frame reads").expect("one frame");
        assert!(matches!(frame, JsonRpcFrame::Notification(_)));
    }

    #[tokio::test]
    async fn multiple_frames_parse_in_order() {
        let input = format!(
            "{INITIALIZE}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"workspaceRoots\":[]}}}}\n"
        );
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        let first = codec.read_frame().await.expect("first frame reads").expect("first frame");
        let second = codec.read_frame().await.expect("second frame reads").expect("second frame");
        assert!(matches!(first, JsonRpcFrame::Request(_)));
        assert!(matches!(second, JsonRpcFrame::Request(_)));
        assert!(codec.read_frame().await.expect("EOF is not an error").is_none());
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let mut codec = codec(1024, b"{\"jsonrpc\":\"2.0\"\n".as_slice(), Vec::new());
        match codec.read_frame().await {
            Err(AcpServerError::JsonParse { .. }) => {}
            other => panic!("expected JsonParse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn valid_json_that_is_not_jsonrpc_is_a_protocol_error() {
        let mut codec = codec(1024, b"{\"foo\":1}\n".as_slice(), Vec::new());
        match codec.read_frame().await {
            Err(AcpServerError::Protocol(message)) => {
                assert!(message.contains("not a valid JSON-RPC message"));
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_frames_are_rejected() {
        let input = format!("[{INITIALIZE}]\n");
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        match codec.read_frame().await {
            Err(AcpServerError::Protocol(_)) => {}
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_parsing() {
        let payload = "x".repeat(1025);
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":\"{payload}\"}}\n"
        );
        let mut codec = codec(1024, line.as_bytes(), Vec::new());
        match codec.read_frame().await {
            Err(AcpServerError::Protocol(message)) => {
                assert!(message.contains("1024 byte cap"), "{message}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eof_is_clean_shutdown() {
        let mut codec = codec(1024, b"".as_slice(), Vec::new());
        assert!(codec.read_frame().await.expect("clean EOF").is_none());
    }

    #[tokio::test]
    async fn eof_after_frames_is_clean_shutdown() {
        let input = format!("{INITIALIZE}\n");
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        assert!(codec.read_frame().await.expect("frame reads").is_some());
        assert!(codec.read_frame().await.expect("clean EOF").is_none());
    }

    #[tokio::test]
    async fn final_frame_without_newline_is_honored() {
        let mut codec = codec(1024, INITIALIZE.as_bytes(), Vec::new());
        let frame = codec.read_frame().await.expect("frame reads").expect("one frame");
        assert!(matches!(frame, JsonRpcFrame::Request(_)));
        assert!(codec.read_frame().await.expect("clean EOF").is_none());
    }

    #[tokio::test]
    async fn blank_lines_are_skipped() {
        let input = format!("\n\n{INITIALIZE}\n\n");
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        let frame = codec.read_frame().await.expect("frame reads").expect("one frame");
        assert!(matches!(frame, JsonRpcFrame::Request(_)));
        assert!(codec.read_frame().await.expect("clean EOF").is_none());
    }

    #[tokio::test]
    async fn crlf_line_endings_are_tolerated() {
        let input = format!("{INITIALIZE}\r\n");
        let mut codec = codec(1024, input.as_bytes(), Vec::new());
        let frame = codec.read_frame().await.expect("frame reads").expect("one frame");
        assert!(matches!(frame, JsonRpcFrame::Request(_)));
    }

    #[tokio::test]
    async fn write_emits_one_line_per_frame() {
        let frame = parse_frame(INITIALIZE);
        let mut codec = codec(1024, b"".as_slice(), Vec::new());
        codec.write_frame(&frame).await.expect("frame writes");

        let output = codec.writer;
        assert!(output.ends_with(b"\n"), "must end with one newline");
        let line = &output[..output.len() - 1];
        assert!(!line.contains(&b'\n'), "must be a single line, got {output:?}");
        let roundtrip: JsonRpcFrame = serde_json::from_slice(line).expect("line parses back");
        let JsonRpcFrame::Request(written) = roundtrip else {
            panic!("expected request frame");
        };
        let JsonRpcFrame::Request(expected) = frame else {
            unreachable!();
        };
        assert_eq!(written.id, expected.id);
        assert_eq!(written.method, expected.method);
    }

    #[tokio::test]
    async fn write_rejects_oversized_frames() {
        let frame = parse_frame(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"pad\":\"{}\"}}}}",
            "x".repeat(1025)
        ));
        let mut codec = codec(1024, b"".as_slice(), Vec::new());
        match codec.write_frame(&frame).await {
            Err(AcpServerError::Protocol(message)) => {
                assert!(message.contains("1024 byte cap"), "{message}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[cfg(feature = "test-utils")]
    mod memory_transport {
        use super::*;

        #[tokio::test]
        async fn inbound_messages_arrive_in_order() {
            let (mut transport, handle) = MemoryTransport::new();
            let first = parse_frame(INITIALIZE);
            let second =
                parse_frame(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#);
            assert!(handle.send(first.clone()));
            assert!(handle.send(second.clone()));

            assert_frame_eq(transport.read_message().await.expect("reads"), Some(first));
            assert_frame_eq(transport.read_message().await.expect("reads"), Some(second));
        }

        #[tokio::test]
        async fn outbound_messages_are_captured_in_order() {
            let (mut transport, _handle) = MemoryTransport::new();
            let first = parse_frame(INITIALIZE);
            let second = parse_frame(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
            transport.write_message(first.clone()).await.expect("writes");
            transport.write_message(second.clone()).await.expect("writes");

            assert_frames_eq(transport.outbound(), vec![first.clone(), second.clone()]);
            assert_frames_eq(transport.take_outbound(), vec![first, second]);
            assert!(transport.outbound().is_empty(), "take clears the capture");
        }

        #[tokio::test]
        async fn closing_the_handle_injects_eof() {
            let (mut transport, mut handle) = MemoryTransport::new();
            let frame = parse_frame(INITIALIZE);
            handle.send(frame.clone());
            handle.close();

            assert_frame_eq(transport.read_message().await.expect("reads"), Some(frame));
            assert!(transport.read_message().await.expect("clean EOF").is_none());
        }

        #[tokio::test]
        async fn dropping_the_handle_injects_eof() {
            let (mut transport, handle) = MemoryTransport::new();
            drop(handle);
            assert!(transport.read_message().await.expect("clean EOF").is_none());
        }

        #[tokio::test]
        async fn sending_after_close_fails() {
            let (mut transport, mut handle) = MemoryTransport::new();
            handle.close();
            assert!(!handle.send(parse_frame(INITIALIZE)));
            assert!(transport.read_message().await.expect("clean EOF").is_none());
        }

        #[tokio::test]
        async fn transports_work_through_the_trait() {
            async fn roundtrip<T: AcpTransport>(
                transport: &mut T,
            ) -> Result<Option<JsonRpcFrame>, AcpServerError> {
                transport.read_message().await
            }

            let (mut transport, handle) = MemoryTransport::new();
            let frame = parse_frame(INITIALIZE);
            handle.send(frame.clone());
            assert_frame_eq(roundtrip(&mut transport).await.expect("reads"), Some(frame));
        }
    }
}
