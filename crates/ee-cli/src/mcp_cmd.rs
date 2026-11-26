//! `ee do agent serve-mcp`: MCP stdio server for agent drivers.
//!
//! One process owns one host and its own agent connections. Shares config and
//! secret resolution with `ee do agent serve` through
//! [`crate::serve_cmd::build_host_inputs`].

use std::io;
use std::path::PathBuf;

use ee_agent_remote::RemoteHost;

/// Runs the MCP server until the driver closes stdin.
///
/// # Errors
///
/// Returns an I/O error for invalid roots, missing agents, runtime creation,
/// or a transport failure.
pub(crate) fn run(
    allow_roots: Vec<PathBuf>,
    permission_mode: Option<String>,
    audit_log: Option<PathBuf>,
    session_log_dir: Option<PathBuf>,
) -> io::Result<()> {
    let (config, options) = crate::serve_cmd::build_host_inputs(
        allow_roots,
        permission_mode,
        audit_log,
        session_log_dir,
    )?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let (host, _handler) = RemoteHost::headless_with_options(config, options).map_err(to_io)?;
        eprintln!(
            "ee: agent serve-mcp ready on stdio ({} agent(s), permission mode {})",
            host.manager().agent_ids().len(),
            host.config().permission_mode.as_str()
        );
        let result = ee_agent_remote::mcp::serve_stdio(host.clone()).await;
        host.shutdown().await;
        result.map_err(to_io)
    })
}

fn to_io(error: ee_agent_remote::RemoteError) -> io::Error {
    io::Error::other(error.to_string())
}
