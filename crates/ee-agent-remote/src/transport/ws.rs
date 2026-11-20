//! WebSocket + HTTP surface for the remote protocol.
//!
//! One listener serves three things:
//!
//! - `GET /healthz` and `GET /readyz` (no Origin header allowed);
//! - `GET /ws` (bearer-token authenticated JSON-RPC over WebSocket);
//! - optional static assets for the WASM web client (R11).
//!
//! The listener binds loopback by default. Plain `ws://` is loopback/tunnel
//! only: non-loopback binds require an explicit flag and a token file, and the
//! guidance points at an SSH tunnel or a TLS reverse proxy.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::error::RemoteError;
use crate::host::RemoteHost;

use super::handler::ConnectionHandler;

/// Largest accepted HTTP request head.
pub const MAX_HTTP_HEAD_BYTES: usize = 16 * 1024;
/// Deadline for receiving one HTTP request head.
pub const HTTP_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Recommended default loopback bind.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

/// WebSocket/HTTP listener options.
#[derive(Debug, Clone)]
pub struct WsServeOptions {
    /// Bind address.
    pub bind: SocketAddr,
    /// Token file (0600) read once per connection.
    pub token_file: PathBuf,
    /// Static asset root for the web client.
    pub static_dir: Option<PathBuf>,
    /// Idle connection timeout; in-flight turns are never interrupted.
    pub idle_timeout: Duration,
    /// Explicit opt-in for non-loopback binds.
    pub allow_non_loopback: bool,
}

impl WsServeOptions {
    /// Builds loopback-default options.
    #[must_use]
    pub fn new(bind: SocketAddr, token_file: PathBuf) -> Self {
        Self {
            bind,
            token_file,
            static_dir: None,
            idle_timeout: crate::config::DEFAULT_IDLE_TIMEOUT,
            allow_non_loopback: false,
        }
    }
}

/// A bound listener; tests use [`Self::local_addr`] before serving.
pub struct WsServer {
    listener: TcpListener,
    options: WsServeOptions,
}

impl WsServer {
    /// Validates options and binds the listener.
    ///
    /// # Errors
    ///
    /// Returns `invalid_params` for a non-loopback bind without the explicit
    /// flag or an insecure/absent token file.
    pub async fn bind(options: WsServeOptions) -> Result<Self, RemoteError> {
        validate_bind(options.bind, options.allow_non_loopback)?;
        validate_token_file(&options.token_file)?;
        let listener = TcpListener::bind(options.bind).await.map_err(|error| {
            RemoteError::internal(format!("cannot bind {}: {error}", options.bind))
        })?;
        Ok(Self { listener, options })
    }

    /// The actual bound address (useful for port 0).
    ///
    /// # Errors
    ///
    /// Returns the socket-layer error.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts connections until the listener fails.
    ///
    /// # Errors
    ///
    /// Returns a typed error only for listener-level failures; per-connection
    /// failures are logged and the loop continues.
    pub async fn serve(self, host: RemoteHost) -> Result<(), RemoteError> {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "websocket accept failed");
                    continue;
                }
            };
            let host = host.clone();
            let options = self.options.clone();
            tokio::spawn(async move {
                if let Err(error) = handle_connection(host, stream, options).await {
                    tracing::debug!(%error, %peer, "websocket connection ended");
                }
            });
        }
    }
}

/// Binds and serves the WebSocket/HTTP surface.
///
/// # Errors
///
/// Same as [`WsServer::bind`].
pub async fn serve_ws(host: RemoteHost, options: WsServeOptions) -> Result<(), RemoteError> {
    WsServer::bind(options).await?.serve(host).await
}

fn validate_bind(bind: SocketAddr, allow_non_loopback: bool) -> Result<(), RemoteError> {
    if bind.ip().is_loopback() || allow_non_loopback {
        return Ok(());
    }
    Err(RemoteError::invalid_params(format!(
        "refusing to bind non-loopback address {bind}: plain ws:// is loopback-only. \
         Re-run with --allow-remote and use a secure path instead: \
         `ssh -L 8787:{bind} user@host` for a tunnel, or a reverse proxy that terminates TLS \
         (for example Caddy `reverse_proxy 127.0.0.1:8787`)."
    )))
}

fn validate_token_file(path: &Path) -> Result<(), RemoteError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        RemoteError::invalid_params(format!(
            "token file {} is not readable: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(RemoteError::invalid_params(format!(
            "token file {} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(RemoteError::invalid_params(format!(
                "token file {} must be owner-only (0600), found {:o}",
                path.display(),
                mode
            )));
        }
    }
    Ok(())
}

fn read_token(path: &Path) -> Result<String, RemoteError> {
    let token = std::fs::read_to_string(path).map_err(|error| {
        RemoteError::internal(format!("cannot read token file {}: {error}", path.display()))
    })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(RemoteError::unauthorized());
    }
    Ok(token)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = 0_u8;
    let length = left.len().max(right.len());
    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= a ^ b;
    }
    difference == 0 && left.len() == right.len()
}

struct HttpRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, RemoteError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    let deadline = Instant::now() + HTTP_HEAD_TIMEOUT;
    loop {
        if let Some(position) = find_head_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..position]).to_string();
            return parse_head(&head);
        }
        if buffer.len() >= MAX_HTTP_HEAD_BYTES {
            return Err(RemoteError::invalid_request("HTTP head too large"));
        }
        let read = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            stream.read(&mut chunk),
        )
        .await
        .map_err(|_| RemoteError::timeout("http/head", HTTP_HEAD_TIMEOUT.as_millis() as u64))?
        .map_err(|error| RemoteError::internal(format!("http read: {error}")))?;
        if read == 0 {
            return Err(RemoteError::invalid_request("connection closed before headers"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_head(head: &str) -> Result<HttpRequest, RemoteError> {
    let mut lines = head.split("\r\n");
    let request_line =
        lines.next().ok_or_else(|| RemoteError::invalid_request("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        return Err(RemoteError::invalid_request("malformed request line"));
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok(HttpRequest { method, target, headers })
}

fn target_path(target: &str) -> (&str, Option<&str>) {
    match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    }
}

fn query_token(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| pair.strip_prefix("token="))
}

fn bearer_token(headers: &BTreeMap<String, String>) -> Option<&str> {
    let value = headers.get("authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") { Some(token.trim()) } else { None }
}

async fn handle_connection(
    host: RemoteHost,
    mut stream: TcpStream,
    options: WsServeOptions,
) -> Result<(), RemoteError> {
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            let _ = write_json_response(&mut stream, 400, "Bad Request", &error_json(&error)).await;
            return Ok(());
        }
    };
    let origin = request.headers.contains_key("origin");
    let (path, query) = target_path(&request.target);

    match (request.method.as_str(), path) {
        ("GET", "/healthz") => {
            if origin {
                write_plain_response(&mut stream, 403, "Forbidden", "origin rejected").await
            } else {
                write_json_response(&mut stream, 200, "OK", &serde_json::json!({ "status": "ok" }))
                    .await
            }
        }
        ("GET", "/readyz") => {
            if origin {
                return write_plain_response(&mut stream, 403, "Forbidden", "origin rejected")
                    .await;
            }
            let ready = !host.manager().agent_ids().is_empty();
            let body = serde_json::json!({ "ready": ready });
            if ready {
                write_json_response(&mut stream, 200, "OK", &body).await
            } else {
                write_json_response(&mut stream, 503, "Service Unavailable", &body).await
            }
        }
        ("GET", "/ws") if is_websocket_upgrade(&request.headers) => {
            let expected = read_token(&options.token_file)?;
            let presented = bearer_token(&request.headers)
                .or_else(|| query.and_then(query_token))
                .unwrap_or_default();
            if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                return write_plain_response(&mut stream, 401, "Unauthorized", "invalid token")
                    .await;
            }
            upgrade(host, stream, request, options).await
        }
        ("GET", _) if options.static_dir.is_some() => {
            let root = options.static_dir.as_deref().expect("checked");
            serve_static(&mut stream, root, path).await
        }
        _ => write_plain_response(&mut stream, 404, "Not Found", "not found").await,
    }
}

fn is_websocket_upgrade(headers: &BTreeMap<String, String>) -> bool {
    headers.get("upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && headers
            .get("connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
        && headers.contains_key("sec-websocket-key")
}

async fn upgrade(
    host: RemoteHost,
    mut stream: TcpStream,
    request: HttpRequest,
    options: WsServeOptions,
) -> Result<(), RemoteError> {
    let key = request
        .headers
        .get("sec-websocket-key")
        .cloned()
        .ok_or_else(|| RemoteError::invalid_request("missing Sec-WebSocket-Key"))?;
    let accept = derive_accept_key(key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| RemoteError::internal(format!("ws handshake write: {error}")))?;
    stream
        .flush()
        .await
        .map_err(|error| RemoteError::internal(format!("ws handshake flush: {error}")))?;

    let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    drive_websocket(host, ws, options).await
}

async fn drive_websocket<S>(
    host: RemoteHost,
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    options: WsServeOptions,
) -> Result<(), RemoteError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let queue_capacity = host.config().max_outbound_queue.max(1);
    let (writer_tx, mut writer_rx) = mpsc::channel::<Value>(queue_capacity);
    let subscription = host.bus().subscribe();
    let overloaded = subscription.overloaded.clone();
    let mut forwarder = subscription;
    let mut handler = ConnectionHandler::new(host, writer_tx.clone(), overloaded);
    let mut idle_deadline = Instant::now() + options.idle_timeout;

    loop {
        tokio::select! {
            Some(frame) = writer_rx.recv() => {
                let text = serde_json::to_string(&frame).unwrap_or_default();
                if ws.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Some(frame) = forwarder.receiver.recv() => {
                if writer_tx.try_send(frame).is_err() {
                    // The outbound queue is full: the subscription is already
                    // marked overloaded by the bus, so stop stacking frames.
                    continue;
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(idle_deadline)) => {
                let _ = ws.close(None).await;
                break;
            }
            message = ws.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        idle_deadline = Instant::now() + options.idle_timeout;
                        handler.handle_text(text.as_str()).await;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        idle_deadline = Instant::now() + options.idle_timeout;
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        idle_deadline = Instant::now() + options.idle_timeout;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => break,
                    Some(Err(error)) => {
                        tracing::debug!(%error, "websocket read error");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn write_plain_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), RemoteError> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| RemoteError::internal(format!("http write: {error}")))
}

async fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &Value,
) -> Result<(), RemoteError> {
    let body = serde_json::to_string(body).unwrap_or_else(|_| String::from("{}"));
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| RemoteError::internal(format!("http write: {error}")))
}

async fn serve_static(
    stream: &mut TcpStream,
    root: &Path,
    target: &str,
) -> Result<(), RemoteError> {
    let Some(path) = resolve_static(root, target) else {
        return write_plain_response(stream, 404, "Not Found", "not found").await;
    };
    let bytes = std::fs::read(&path)
        .map_err(|error| RemoteError::internal(format!("static read: {error}")))?;
    let content_type = content_type(&path);
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|error| RemoteError::internal(format!("static write: {error}")))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| RemoteError::internal(format!("static write: {error}")))
}

fn resolve_static(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target_path(target).0;
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() { "index.html" } else { relative };
    if relative.split('/').any(|component| component == ".." || component.is_empty()) {
        return None;
    }
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let candidate = canonical_root.join(relative);
    let canonical = std::fs::canonicalize(candidate).ok()?;
    if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
        return None;
    }
    Some(canonical)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()).unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn error_json(error: &RemoteError) -> Value {
    serde_json::json!({ "code": error.code().as_str(), "message": error.message() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn non_loopback_binds_require_the_explicit_flag() {
        let addr: SocketAddr = "0.0.0.0:8787".parse().expect("addr");
        let error = validate_bind(addr, false).expect_err("non-loopback");
        assert!(error.message().contains("--allow-remote"));
        assert!(error.message().contains("ssh -L"));
        assert!(validate_bind(addr, true).is_ok());
    }

    #[test]
    fn token_files_must_be_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "secret-token").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            assert!(validate_token_file(&path).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        assert!(validate_token_file(&path).is_ok());
        assert_eq!(read_token(&path).expect("token"), "secret-token");
    }

    #[test]
    fn query_tokens_are_extracted() {
        assert_eq!(query_token("a=1&token=abc"), Some("abc"));
        assert_eq!(query_token("a=1"), None);
    }

    #[test]
    fn static_paths_cannot_escape_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "ok").expect("write");
        assert!(resolve_static(dir.path(), "/").is_some());
        assert!(resolve_static(dir.path(), "/../etc/passwd").is_none());
        assert!(resolve_static(dir.path(), "/missing.js").is_none());
    }
}
