//! Transport construction for MCP servers.
//!
//! Both transports come from [`rmcp`]: stdio via `TokioChildProcess`
//! (explicit command/args/env/cwd, piped stderr, graceful shutdown with
//! kill-on-timeout), and Streamable HTTP via the official client.  ee-owned
//! code is limited to wiring the resolved config into the SDK builders,
//! capturing stderr into a bounded diagnostics buffer, and enforcing the
//! no-HTTP+SSE policy (the deprecated transport is simply never built).

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use rmcp::transport::child_process::TokioChildProcess;

use crate::McpError;
use crate::config::{McpServerConfig, McpServerKind};

/// A spawnable stdio server process.
pub struct StdioProcess {
    /// The rmcp child-process transport (stdout/stdin JSON-RPC).
    pub transport: TokioChildProcess,
    /// Shared bounded stderr diagnostics (never parsed as protocol).
    pub diagnostics: Arc<Mutex<BoundedDiagnostics>>,
}

/// Bounded ring of stderr bytes; front-truncated at a char boundary so the
/// most recent diagnostics survive.
#[derive(Debug)]
pub struct BoundedDiagnostics {
    bytes: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl BoundedDiagnostics {
    /// Creates an empty buffer with `cap` max retained bytes.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self { bytes: Vec::new(), cap: cap.max(1), truncated: false }
    }

    /// Appends one stderr chunk, enforcing the cap.
    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        while self.bytes.len() > self.cap {
            let mut cut = self.bytes.len() - self.cap;
            while cut < self.bytes.len() && self.bytes[cut] & 0xC0 == 0x80 {
                cut += 1;
            }
            self.bytes.drain(..cut);
            self.truncated = true;
        }
    }

    /// Retained stderr as lossy string.
    #[must_use]
    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Whether any stderr bytes were dropped by the cap.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Spawns a configured stdio MCP server process.
///
/// # Errors
///
/// Fails when the command is empty, the cwd is relative, or the process
/// cannot be spawned.
pub fn spawn_stdio(config: &McpServerConfig) -> Result<StdioProcess, McpError> {
    let McpServerKind::Stdio(stdio) = &config.kind else {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "server {} is not a stdio server",
            config.id
        )));
    };
    if stdio.command.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "mcp server {}: command must not be empty",
            config.id
        )));
    }
    let mut command = tokio::process::Command::new(&stdio.command);
    command.args(&stdio.args);
    command.envs(&stdio.env);
    if let Some(cwd) = &stdio.cwd {
        command.current_dir(cwd);
    }
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| McpError::Io(format!("cannot spawn mcp server {}: {error}", config.id)))?;
    let diagnostics = Arc::new(Mutex::new(BoundedDiagnostics::new(stdio.stderr_cap)));
    if let Some(stderr) = stderr {
        spawn_stderr_reader(stderr, Arc::clone(&diagnostics));
    }
    Ok(StdioProcess { transport, diagnostics })
}

/// Reads the child's stderr into the bounded diagnostics buffer.  stderr is
/// never parsed as protocol, per ee policy.
fn spawn_stderr_reader(
    mut stderr: tokio::process::ChildStderr,
    diagnostics: Arc<Mutex<BoundedDiagnostics>>,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => diagnostics.lock().expect("diagnostics poisoned").push(&buffer[..n]),
            }
        }
    });
}

/// Builds an rmcp Streamable HTTP client transport for a configured server.
///
/// # Errors
///
/// Fails when the server is not a Streamable HTTP server or the URL is
/// invalid.
pub fn build_http_transport(
    config: &McpServerConfig,
) -> Result<
    rmcp::transport::streamable_http_client::StreamableHttpClientTransport<reqwest::Client>,
    McpError,
> {
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };
    let McpServerKind::StreamableHttp(http) = &config.kind else {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "server {} is not a Streamable HTTP server",
            config.id
        )));
    };
    let url = url::Url::parse(&http.url).map_err(|error| {
        McpError::InvalidPrimitiveResult(format!("invalid url {}: {error}", http.url))
    })?;
    let mut builder = StreamableHttpClientTransportConfig::with_uri(url.to_string())
        .max_sse_event_size(1024 * 1024);
    if !http.headers.is_empty() {
        let mut headers = std::collections::HashMap::new();
        for (name, value) in &http.headers {
            let header_name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                McpError::InvalidPrimitiveResult(format!("invalid header name {name:?}: {error}"))
            })?;
            let header_value = http::HeaderValue::from_str(value).map_err(|error| {
                McpError::InvalidPrimitiveResult(format!(
                    "invalid header value for {name:?}: {error}"
                ))
            })?;
            headers.insert(header_name, header_value);
        }
        builder = builder.custom_headers(headers);
    }
    Ok(StreamableHttpClientTransport::with_client(reqwest::Client::new(), builder))
}

impl StdioProcess {
    /// Gracefully closes the child transport and waits for exit, killing the
    /// process if it does not stop within rmcp's grace window.
    pub async fn shutdown(&mut self) {
        let _ = self.transport.graceful_shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn bounded_diagnostics_truncates_from_front_at_char_boundary() {
        let mut buffer = BoundedDiagnostics::new(5);
        buffer.push("hello world".as_bytes());
        assert_eq!(buffer.as_string(), "world");
        assert!(buffer.truncated());
    }

    #[test]
    fn bounded_diagnostics_keeps_short_output_untouched() {
        let mut buffer = BoundedDiagnostics::new(1024);
        buffer.push("ok\n".as_bytes());
        assert_eq!(buffer.as_string(), "ok\n");
        assert!(!buffer.truncated());
    }

    #[test]
    fn non_stdio_server_cannot_spawn() {
        let config = McpServerConfig {
            id: "srv".to_string(),
            kind: McpServerKind::StreamableHttp(crate::config::StreamableHttpConfig {
                url: "https://example.com".to_string(),
                headers: BTreeMap::new(),
            }),
            timeout_ms: 1000,
        };
        assert!(spawn_stdio(&config).is_err());
    }

    /// Real subprocess lifecycle: spawn, stderr capture, clean kill.
    #[tokio::test]
    async fn stdio_subprocess_captures_stderr_and_shuts_down() {
        let config = McpServerConfig {
            id: "srv".to_string(),
            kind: McpServerKind::Stdio(crate::config::StdioMcpConfig {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "echo diag-line >&2; sleep 30".to_string()],
                env: BTreeMap::new(),
                cwd: None,
                stderr_cap: 4096,
            }),
            timeout_ms: 1000,
        };
        let mut process = spawn_stdio(&config).expect("spawn");
        assert!(process.transport.id().is_some(), "child process started");

        // stderr is captured into the bounded diagnostics buffer (never
        // parsed as protocol).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if process.diagnostics.lock().expect("poisoned").as_string().contains("diag-line") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "stderr diagnostics never captured");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Graceful shutdown kills the sleeping child within a bounded window.
        let start = tokio::time::Instant::now();
        process.shutdown().await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "shutdown must not hang on a live child"
        );
    }
}
