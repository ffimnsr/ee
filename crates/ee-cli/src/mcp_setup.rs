//! Interactive `[mcp.servers]` setup: add or remove servers across the
//! workspace/user config layers.
//!
//! Secret env/header values never enter the workspace config layer: they are
//! written to the user config layer as split-layer patches (transport plus the
//! secret tables) that the workspace server definition completes at merge.

use std::collections::BTreeMap;

use crate::config::{self, ConfigScope, McpServerToml, McpTransportToml};
use crate::setup_prompt::{
    confirm, prompt_line, prompt_value, read_hidden_value, validate_env_name, validate_header_name,
    validate_server_name,
};

/// One prompted key/value pair with its secret flag.
struct PromptedValue {
    value: String,
    secret: bool,
}

pub(crate) fn run(user: bool) -> Result<(), String> {
    let scope = if user { ConfigScope::Global } else { ConfigScope::Local };
    let path = config::mcp_config_path(scope)?;
    loop {
        let configured = config::list_mcp_servers(scope)?;
        println!("MCP servers in {} ({}):", path.display(), scope_label(scope));
        if configured.is_empty() {
            println!("  (none)");
        } else {
            for (index, (id, transport)) in configured.iter().enumerate() {
                println!("  {}) {} ({transport})", index + 1, id);
            }
        }
        println!("Options:");
        println!("  1) Add MCP server");
        println!("  2) Remove MCP server");
        println!("  3) Done");
        match loop {
            let selected = prompt_line("Choose [1-3]: ")?;
            match selected.trim() {
                "1" | "2" | "3" => break selected.trim().to_owned(),
                _ => eprintln!("Enter 1, 2, or 3."),
            }
        }
        .as_str()
        {
            "1" => add_server(scope)?,
            "2" => remove_server(scope, &path, &configured)?,
            _ => return Ok(()),
        }
    }
}

fn scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Global => "user config",
        ConfigScope::Local => "workspace config",
    }
}

fn add_server(scope: ConfigScope) -> Result<(), String> {
    let id = loop {
        let value =
            prompt_value("Server name", None, true)?.ok_or_else(|| String::from("missing name"))?;
        if validate_server_name(&value) {
            break value;
        }
        eprintln!("Server name must use only letters, digits, `-`, or `_` (max 64).");
    };
    println!("Transport:");
    println!("  1) stdio");
    println!("  2) streamable_http");
    let transport = loop {
        match prompt_line("Select transport [1-2]: ")?.trim() {
            "1" => break McpTransportToml::Stdio,
            "2" => break McpTransportToml::StreamableHttp,
            _ => eprintln!("Enter 1 or 2."),
        }
    };
    let (server, secret_env, secret_headers) = match transport {
        McpTransportToml::Stdio => {
            let command = prompt_value("Command", None, true)?
                .ok_or_else(|| String::from("missing command"))?;
            let args = prompt_value("Args (space-separated, empty to skip)", Some(""), false)?
                .unwrap_or_default();
            let args = args.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
            let env = prompt_pairs("Env var name", validate_env_name, "environment variable")?;
            let mut secret_env = BTreeMap::new();
            let mut public_env = BTreeMap::new();
            for (name, value) in env {
                if value.secret {
                    secret_env.insert(name, value.value);
                } else {
                    public_env.insert(name, value.value);
                }
            }
            (
                McpServerToml {
                    transport: McpTransportToml::Stdio,
                    command: Some(command),
                    args: Some(args),
                    env: public_env,
                    cwd: None,
                    url: None,
                    headers: BTreeMap::new(),
                    timeout_ms: None,
                },
                secret_env,
                BTreeMap::new(),
            )
        }
        McpTransportToml::StreamableHttp => {
            let url =
                prompt_value("URL", None, true)?.ok_or_else(|| String::from("missing URL"))?;
            let timeout_ms = prompt_value("Timeout (ms)", Some("30000"), false)?
                .unwrap_or_else(|| String::from("30000"))
                .parse::<u64>()
                .map_err(|_| String::from("timeout must be a number of milliseconds"))?;
            let headers = prompt_pairs("Header name", validate_header_name, "header")?;
            let mut secret_headers = BTreeMap::new();
            let mut public_headers = BTreeMap::new();
            for (name, value) in headers {
                if value.secret {
                    secret_headers.insert(name, value.value);
                } else {
                    public_headers.insert(name, value.value);
                }
            }
            (
                McpServerToml {
                    transport: McpTransportToml::StreamableHttp,
                    command: None,
                    args: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    url: Some(url),
                    headers: public_headers,
                    timeout_ms: Some(timeout_ms),
                },
                BTreeMap::new(),
                secret_headers,
            )
        }
    };

    let written = config::write_mcp_server(scope, &id, &server)?;
    println!("Added mcp server `{id}` to {}.", written.display());
    if scope == ConfigScope::Local && (!secret_env.is_empty() || !secret_headers.is_empty()) {
        let user_path =
            config::write_mcp_server_user_partial(&id, transport, &secret_env, &secret_headers)?;
        println!(
            "Stored mcp secrets for `{id}` in {} (user config; workspace config stays clean).",
            user_path.display()
        );
    }
    Ok(())
}

/// Prompts for repeated `name → value` pairs; an empty name finishes.
fn prompt_pairs(
    name_label: &str,
    validate_name: fn(&str) -> bool,
    kind: &str,
) -> Result<BTreeMap<String, PromptedValue>, String> {
    let mut pairs = BTreeMap::new();
    loop {
        let name = prompt_value(name_label, Some(""), false)?.unwrap_or_default();
        if name.is_empty() {
            return Ok(pairs);
        }
        if !validate_name(&name) {
            eprintln!("Invalid {kind} name `{name}`.");
            continue;
        }
        let secret = confirm("Secret? [y/N]: ")?;
        let value = if secret {
            read_hidden_value(true)?.ok_or_else(|| format!("missing {kind} value"))?.to_string()
        } else {
            prompt_value("Value", None, true)?.ok_or_else(|| format!("missing {kind} value"))?
        };
        pairs.insert(name, PromptedValue { value, secret });
    }
}

fn remove_server(
    scope: ConfigScope,
    path: &std::path::Path,
    configured: &[(String, String)],
) -> Result<(), String> {
    if configured.is_empty() {
        eprintln!("No MCP servers configured in this scope.");
        return Ok(());
    }
    for (index, (id, transport)) in configured.iter().enumerate() {
        println!("  {}) {} ({transport})", index + 1, id);
    }
    let selected = loop {
        let value = prompt_line(&format!("Select server [1-{}]: ", configured.len()))?;
        let index = value
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .filter(|index| *index < configured.len());
        if let Some(index) = index {
            break index;
        }
        eprintln!("Enter a number from 1 through {}.", configured.len());
    };
    let id = &configured[selected].0;
    if !confirm(&format!("Remove mcp server `{id}` from {}? [y/N]: ", path.display()))? {
        return Ok(());
    }
    let removed = config::remove_mcp_server(scope, id)?;
    println!("Removed mcp server `{id}` from {}.", removed.display());
    if scope == ConfigScope::Local
        && config::list_mcp_servers(ConfigScope::Global)?.iter().any(|(other, _)| other == id)
    {
        let user_path = config::remove_mcp_server(ConfigScope::Global, id)?;
        println!("Removed user-layer mcp entries for `{id}` from {}.", user_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompted_env_names_use_env_rule_and_headers_use_token_rule() {
        assert!(validate_env_name("API_KEY"));
        assert!(!validate_header_name("Bearer: abc"), "colon is not a header token");
        assert!(validate_header_name("X-API-Key"));
    }
}
