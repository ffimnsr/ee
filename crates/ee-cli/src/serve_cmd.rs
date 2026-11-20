//! `ee do agent serve`: headless remote-control host.
//!
//! This entrypoint never builds the editor: it resolves the same merged config
//! layers and `secret://` references as the editor, builds the shared launch
//! configs, and serves the pinned remote JSON-RPC protocol over stdio or WebSocket.

use std::io;
use std::path::PathBuf;

use ee_agent_host::AgentConnectionOptions;
use ee_agent_remote::{RemoteHost, RemoteHostConfig, RemotePermissionMode};

/// Resolved `ee do agent serve` options.
#[derive(Debug, Clone)]
pub(crate) struct ServeOptions {
    pub(crate) stdio: bool,
    pub(crate) listen: Option<std::net::SocketAddr>,
    pub(crate) token_file: Option<PathBuf>,
    pub(crate) static_dir: Option<PathBuf>,
    pub(crate) allow_remote: bool,
    pub(crate) allow_roots: Vec<PathBuf>,
    pub(crate) permission_mode: Option<String>,
    pub(crate) audit_log: Option<PathBuf>,
    pub(crate) session_log_dir: Option<PathBuf>,
}

/// Runs the serve command until the client closes or the listener fails.
///
/// # Errors
///
/// Returns an I/O error for invalid roots, missing agents, a missing token
/// file for `--listen`, runtime creation, or a transport failure.
pub(crate) fn run(serve: ServeOptions) -> io::Result<()> {
    validate_transport(&serve)?;
    let (config, options) = build_host_inputs(
        serve.allow_roots,
        serve.permission_mode,
        serve.audit_log,
        serve.session_log_dir,
    )?;
    let listen = serve.listen;
    let token_file = serve.token_file;
    let static_dir = serve.static_dir;
    let allow_remote = serve.allow_remote;
    if listen.is_some() && token_file.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--listen requires --token-file (0600) so the socket is never unauthenticated",
        ));
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let (host, _handler) = RemoteHost::headless_with_options(config, options).map_err(to_io)?;
        host.start().map_err(to_io)?;
        let result = match listen {
            Some(bind) => {
                let mut ws_options = ee_agent_remote::transport::WsServeOptions::new(
                    bind,
                    token_file.expect("checked above"),
                );
                ws_options.static_dir = static_dir;
                ws_options.idle_timeout = host.config().idle_timeout;
                ws_options.allow_non_loopback = allow_remote;
                let server =
                    ee_agent_remote::transport::WsServer::bind(ws_options).await.map_err(to_io)?;
                let addr = server.local_addr().map_err(io::Error::other)?;
                eprintln!(
                    "ee: agent serve listening on ws://{addr}/ws ({} agent(s), permission mode {})",
                    host.manager().agent_ids().len(),
                    host.config().permission_mode.as_str()
                );
                server.serve(host.clone()).await.map_err(to_io)
            }
            None => {
                eprintln!(
                    "ee: agent serve ready on stdio ({} agent(s), permission mode {})",
                    host.manager().agent_ids().len(),
                    host.config().permission_mode.as_str()
                );
                ee_agent_remote::transport::serve_stdio(host.clone()).await.map_err(to_io)
            }
        };
        host.shutdown().await;
        result
    })
}

/// Builds the shared headless host inputs for every remote surface.
///
/// Reuses the editor's merged config layers, workspace trust policy, and
/// `secret://` resolution. No network access happens here: registry downloads
/// are a setup-time operation, so a headless first run fails with guidance
/// instead of silently fetching.
///
/// # Errors
///
/// Returns an I/O error for invalid roots, missing agents, or a broken config.
pub(crate) fn build_host_inputs(
    allow_roots: Vec<PathBuf>,
    permission_mode: Option<String>,
    audit_log: Option<PathBuf>,
    session_log_dir: Option<PathBuf>,
) -> io::Result<(RemoteHostConfig, AgentConnectionOptions)> {
    let settings = crate::config::load_config(None);
    let roots = resolve_roots(allow_roots)?;
    let mode = resolve_mode(
        permission_mode.as_deref().or(settings.agents.remote_permission_mode.as_deref()),
    );
    let permission_rules = crate::app::agent_pane::host_permission_rules(
        settings.agents.permissions.as_ref(),
        crate::config::xi_core_config_dir().as_deref(),
    );
    let workspace_policy = crate::workspace_trust::workspace_secret_policy(&roots[0]);
    let store = match crate::secrets::SecretStore::default() {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("ee: warning: secrets store unavailable: {error}");
            None
        }
    };
    let launches = crate::agent_launch::resolve_agent_launches(
        &settings.agents.servers,
        store.as_ref(),
        workspace_policy,
    );
    for (id, message) in &launches.failures {
        eprintln!("ee: warning: agent `{id}` launch aborted: {message}");
    }
    if launches.agents.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no agents configured: add [agents.servers.<id>] to ee config or run `ee do agent setup`",
        ));
    }

    let config = RemoteHostConfig {
        agents: launches.agents,
        permission_rules,
        permission_mode: mode,
        allowed_roots: roots,
        secret_values: launches.secret_values,
        default_agent: settings.agents.default_agent.clone(),
        audit_path: audit_log.or_else(default_audit_path),
        session_log_dir,
        ..RemoteHostConfig::default()
    };
    let options = AgentConnectionOptions {
        max_concurrent_prompts: settings.agents.max_concurrent_prompts,
        ..AgentConnectionOptions::default()
    };
    Ok((config, options))
}

/// WebSocket-only flags are rejected instead of silently ignored on
/// stdio, and the two transports are mutually exclusive.
fn validate_transport(serve: &ServeOptions) -> io::Result<()> {
    if serve.stdio && serve.listen.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "choose either --stdio or --listen, not both",
        ));
    }
    if serve.listen.is_none() {
        if serve.token_file.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--token-file requires --listen",
            ));
        }
        if serve.static_dir.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--static-dir requires --listen",
            ));
        }
        if serve.allow_remote {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--allow-remote requires --listen",
            ));
        }
    }
    Ok(())
}

fn resolve_roots(allow_roots: Vec<PathBuf>) -> io::Result<Vec<PathBuf>> {
    let roots = if allow_roots.is_empty() { vec![std::env::current_dir()?] } else { allow_roots };
    let mut canonical = Vec::with_capacity(roots.len());
    for root in roots {
        let root = std::fs::canonicalize(&root).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("invalid --allow-root {}: {error}", root.display()),
            )
        })?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--allow-root {} is not a directory", root.display()),
            ));
        }
        canonical.push(root);
    }
    Ok(canonical)
}

fn resolve_mode(value: Option<&str>) -> RemotePermissionMode {
    match value {
        Some(value) => match RemotePermissionMode::parse(value) {
            Ok(mode) => mode,
            Err(error) => {
                // Fail closed to `ask`: surface every approval rather than
                // widening authority on a typo.
                eprintln!("ee: warning: {error}; using ask");
                RemotePermissionMode::Ask
            }
        },
        None => RemotePermissionMode::default(),
    }
}

fn default_audit_path() -> Option<PathBuf> {
    dirs::state_dir().or_else(dirs::config_dir).map(|dir| dir.join("ee").join("remote-audit.jsonl"))
}

fn to_io(error: ee_agent_remote::RemoteError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_permission_modes_fall_back_to_ask() {
        assert_eq!(resolve_mode(Some("nonsense")), RemotePermissionMode::Ask);
        assert_eq!(resolve_mode(Some("deny")), RemotePermissionMode::Deny);
        assert_eq!(resolve_mode(None), RemotePermissionMode::Ask);
    }

    #[test]
    fn roots_are_canonicalized_and_missing_roots_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = resolve_roots(vec![dir.path().to_path_buf()]).expect("roots");
        assert_eq!(roots.len(), 1);
        assert!(roots[0].is_absolute());
        assert!(resolve_roots(vec![dir.path().join("missing")]).is_err());
    }

    fn options(stdio: bool, listen: Option<std::net::SocketAddr>) -> ServeOptions {
        ServeOptions {
            stdio,
            listen,
            token_file: None,
            static_dir: None,
            allow_remote: false,
            allow_roots: Vec::new(),
            permission_mode: None,
            audit_log: None,
            session_log_dir: None,
        }
    }

    #[test]
    fn transport_flags_are_validated_instead_of_ignored() {
        let addr: std::net::SocketAddr = "127.0.0.1:8787".parse().expect("addr");
        assert!(validate_transport(&options(false, None)).is_ok());
        assert!(validate_transport(&options(true, None)).is_ok());
        assert!(validate_transport(&options(true, Some(addr))).is_err());

        let mut token_only = options(false, None);
        token_only.token_file = Some(PathBuf::from("/tmp/token"));
        assert!(validate_transport(&token_only).is_err());

        let mut static_only = options(false, None);
        static_only.static_dir = Some(PathBuf::from("/tmp/dist"));
        assert!(validate_transport(&static_only).is_err());

        let mut remote_only = options(false, None);
        remote_only.allow_remote = true;
        assert!(validate_transport(&remote_only).is_err());
    }
}
