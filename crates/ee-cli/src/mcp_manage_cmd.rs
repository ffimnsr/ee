//! `ee do agent mcp add|list|rm`: non-interactive `[mcp.servers]`
//! management.
//!
//! Entries are written to one config scope at a time. Secret-looking values
//! must be `secret://` references; the command never stores raw credentials.

use std::collections::BTreeMap;
use std::io;

use crate::args::{AgentMcpAddArgs, AgentMcpCommands, AgentMcpListArgs, AgentMcpRemoveArgs};
use crate::config::{self, ConfigScope, McpServerSettings, McpServerToml, McpTransportToml};
use crate::setup_prompt::{validate_env_name, validate_header_name, validate_server_name};

pub(crate) fn run(command: AgentMcpCommands) -> io::Result<()> {
    match command {
        AgentMcpCommands::Add(args) => add(&args),
        AgentMcpCommands::List(args) => list(&args),
        AgentMcpCommands::Rm(args) => remove(&args),
    }
}

fn scope(user: bool) -> ConfigScope {
    if user { ConfigScope::Global } else { ConfigScope::Local }
}

fn scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Global => "user",
        ConfigScope::Local => "workspace",
    }
}

fn add(args: &AgentMcpAddArgs) -> io::Result<()> {
    let id = args.id.trim();
    if !validate_server_name(id) {
        return Err(invalid(format!(
            "mcp server id must be 1-64 characters of letters, digits, '-' or '_', got {id:?}"
        )));
    }
    let command = args.command.as_deref().map(str::trim).filter(|value| !value.is_empty());
    let url = args.url.as_deref().map(str::trim).filter(|value| !value.is_empty());
    let server = match (command, url) {
        (Some(command), None) => {
            if !args.headers.is_empty() || args.timeout_ms.is_some() {
                return Err(invalid(String::from(
                    "--header and --timeout-ms require --url, not --command",
                )));
            }
            McpServerToml {
                transport: McpTransportToml::Stdio,
                command: Some(command.to_owned()),
                args: Some(args.args.clone()),
                env: parse_pairs(&args.env, "environment variable", validate_env_name)?,
                cwd: args.cwd.clone(),
                url: None,
                headers: BTreeMap::new(),
                timeout_ms: None,
            }
        }
        (None, Some(url)) => {
            if !args.args.is_empty() || !args.env.is_empty() || args.cwd.is_some() {
                return Err(invalid(String::from(
                    "--arg, --env, and --cwd require --command, not --url",
                )));
            }
            McpServerToml {
                transport: McpTransportToml::StreamableHttp,
                command: None,
                args: None,
                env: BTreeMap::new(),
                cwd: None,
                url: Some(url.to_owned()),
                headers: parse_pairs(&args.headers, "header", validate_header_name)?,
                timeout_ms: args.timeout_ms,
            }
        }
        (None, None) => {
            return Err(invalid(String::from(
                "either --command (stdio) or --url (streamable_http) is required",
            )));
        }
        (Some(_), Some(_)) => {
            return Err(invalid(String::from("--command and --url are mutually exclusive")));
        }
    };
    config::validate_mcp_server_shape(id, &server).map_err(invalid)?;

    let scope = scope(args.user);
    let path = config::write_mcp_server(scope, id, &server).map_err(io::Error::other)?;
    println!("mcp server {id} written to {} ({})", path.display(), scope_label(scope));
    match id {
        "ee" => println!(
            "trust grants for the built-in server: ee do agent trust mcp grant --server ee"
        ),
        _ => println!(
            "agent-owned server: ee forwards this entry to agents; trust grants cover only the built-in ee server"
        ),
    }
    Ok(())
}

fn list(args: &AgentMcpListArgs) -> io::Result<()> {
    if args.user {
        let scope = ConfigScope::Global;
        let path = config::mcp_config_path(scope).map_err(io::Error::other)?;
        let servers = config::list_mcp_servers(scope).map_err(io::Error::other)?;
        println!("MCP servers in {} ({}):", path.display(), scope_label(scope));
        if servers.is_empty() {
            println!("  (none)");
            return Ok(());
        }
        for (id, transport) in servers {
            println!("  {id}  {transport}");
        }
        return Ok(());
    }

    let settings = config::load_config(None);
    if settings.mcp.servers.is_empty() {
        println!("no MCP servers configured");
        return Ok(());
    }
    println!("MCP servers (effective):");
    for (id, server) in &settings.mcp.servers {
        match server {
            McpServerSettings::Stdio { command, args, .. } => {
                println!("  {id}  stdio  {command}  ({} args)", args.len());
            }
            McpServerSettings::StreamableHttp { url, .. } => {
                println!("  {id}  streamable_http  {}", sanitized_url(url));
            }
        }
    }
    Ok(())
}

fn remove(args: &AgentMcpRemoveArgs) -> io::Result<()> {
    let id = args.id.trim();
    if !validate_server_name(id) {
        return Err(invalid(format!("invalid mcp server id {id:?}")));
    }
    let scope = scope(args.user);
    let path = config::mcp_config_path(scope).map_err(io::Error::other)?;
    let existed = config::list_mcp_servers(scope)
        .map_err(io::Error::other)?
        .iter()
        .any(|(entry, _)| entry == id);
    config::remove_mcp_server(scope, id).map_err(io::Error::other)?;
    if existed {
        println!("removed mcp server {id} from {} ({})", path.display(), scope_label(scope));
        if id == "ee" {
            println!(
                "pinned trust grants are separate; remove them with `ee do agent trust mcp revoke --server ee`"
            );
        }
    } else {
        println!("no mcp server {id} in {} ({})", path.display(), scope_label(scope));
    }
    Ok(())
}

fn parse_pairs(
    raw: &[String],
    kind: &str,
    validate_name: fn(&str) -> bool,
) -> io::Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    for pair in raw {
        let Some((name, value)) = pair.split_once('=') else {
            return Err(invalid(format!("{kind} must be KEY=VALUE, got {pair:?}")));
        };
        if !validate_name(name) {
            return Err(invalid(format!("invalid {kind} name {name:?}")));
        }
        if ee_agent_host::redact::is_secret_key(name) && !value.contains("secret://") {
            return Err(invalid(format!(
                "{kind} {name:?} looks secret; store it with `ee do secrets set <name>` and pass secret://<name>"
            )));
        }
        pairs.insert(name.to_owned(), value.to_owned());
    }
    Ok(pairs)
}

/// Removes query/fragment data, which may carry tokens, before printing.
fn sanitized_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut url) => {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => String::from("<invalid url>"),
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_like_env_values_require_references() {
        assert!(
            parse_pairs(
                &[String::from("API_KEY=plain")],
                "environment variable",
                validate_env_name
            )
            .is_err()
        );
        let pairs = parse_pairs(
            &[String::from("API_KEY=secret://mcp.demo.API_KEY")],
            "environment variable",
            validate_env_name,
        )
        .unwrap();
        assert_eq!(pairs.get("API_KEY").map(String::as_str), Some("secret://mcp.demo.API_KEY"));
    }

    #[test]
    fn headers_accept_embedded_reference_templates() {
        let pairs = parse_pairs(
            &[String::from("Authorization=Bearer secret://mcp-token")],
            "header",
            validate_header_name,
        )
        .unwrap();
        assert_eq!(
            pairs.get("Authorization").map(String::as_str),
            Some("Bearer secret://mcp-token")
        );
        assert!(
            parse_pairs(
                &[String::from("Authorization=Bearer plain")],
                "header",
                validate_header_name
            )
            .is_err()
        );
    }

    #[test]
    fn list_urls_drop_query_and_fragment() {
        assert_eq!(
            sanitized_url("https://example.test/mcp?token=leak#frag"),
            "https://example.test/mcp"
        );
        assert_eq!(sanitized_url("not a url"), "<invalid url>");
    }
}
