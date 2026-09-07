//! Stdio proxy transport.
use super::proxy_calls::read_bounded_line;
use super::socket_backend::SocketProxyBackend;
use super::*;

pub(super) struct StdioProxyTransport<R = tokio::io::Stdin, W = tokio::io::Stdout> {
    pub(super) reader: tokio::io::BufReader<R>,
    pub(super) writer: Arc<tokio::sync::Mutex<W>>,
}

impl StdioProxyTransport {
    pub(super) fn stdio() -> Self {
        Self::new(tokio::io::stdin(), tokio::io::stdout())
    }
}

impl<R: tokio::io::AsyncRead, W> StdioProxyTransport<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader: tokio::io::BufReader::new(reader),
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
        }
    }
}

impl<R, W> rmcp::transport::Transport<rmcp::service::RoleServer> for StdioProxyTransport<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(&item).unwrap_or_else(|_| "{}".to_string());
        let writer = Arc::clone(&self.writer);
        async move {
            let mut writer = writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        }
    }

    async fn receive(
        &mut self,
    ) -> Option<rmcp::service::RxJsonRpcMessage<rmcp::service::RoleServer>> {
        let line = read_bounded_line(&mut self.reader, PROXY_MAX_FRAME_BYTES).await.ok()?;
        let line = line?;
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(&line).ok()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Runs the proxy subprocess: connects to the editor's proxy socket, then
/// serves the [`ee_mcp::EeMcpProxy`] surface over this process's stdio.
///
/// The editor's listener verifies the token; the agent (which spawned this
/// process) speaks MCP 2026-07-28 over stdin/stdout.
pub(crate) fn run_proxy_stdio(socket: PathBuf, token: String) -> std::io::Result<()> {
    run_proxy_stdio_with_transport(socket, token, StdioProxyTransport::stdio())
}

pub(super) fn run_proxy_stdio_with_transport<T>(
    socket: PathBuf,
    token: String,
    transport: T,
) -> std::io::Result<()>
where
    T: rmcp::transport::Transport<rmcp::service::RoleServer> + Send + 'static,
{
    let backend = SocketProxyBackend::connect(&socket, &token)?;
    let proxy = ee_mcp::EeMcpProxy::new(Arc::new(backend));
    let runtime = TokioBuilder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    runtime.block_on(async move {
        let running = rmcp::serve_server(proxy, transport)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let _ = running.waiting().await;
        Ok(())
    })
}

/// Test-only raw stdio runner using the same MCP server and newline framing as
/// the subprocess entry. Duplex avoids spawning an external binary.
#[cfg(test)]
pub(crate) fn run_proxy_stdio_with_duplex(
    socket: PathBuf,
    token: String,
    stream: tokio::io::DuplexStream,
) -> std::io::Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    run_proxy_stdio_with_transport(socket, token, StdioProxyTransport::new(reader, writer))
}

// ── App integration ──────────────────────────────────────────────────────────
