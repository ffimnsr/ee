//! Interactive `[mcp.servers]` setup: add or remove servers across the
//! workspace/user config layers.
//!
//! Values flagged as secrets are stored in the host-bound encrypted secrets
//! store; config carries only their `secret://` references. Workspace-layer
//! references resolve only for a trusted workspace (EOF-safe: user-layer
//! values always resolve).

use std::collections::BTreeMap;

use crate::config::{self, ConfigScope, McpServerToml, McpTransportToml};
use crate::secrets::{SecretName, SecretReference, SecretStore};
use crate::setup_prompt::{
    confirm, prompt_line, prompt_value, read_hidden_value, validate_env_name, validate_header_name,
    validate_server_name,
};
use zeroize::Zeroizing;

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
    let (server, stored_secrets) = match transport {
        McpTransportToml::Stdio => {
            let command = prompt_value("Command", None, true)?
                .ok_or_else(|| String::from("missing command"))?;
            let args = prompt_value("Args (space-separated, empty to skip)", Some(""), false)?
                .unwrap_or_default();
            let args = args.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
            let env = prompt_pairs("Env var name", validate_env_name, "environment variable")?;
            let (env, stored) = store_secret_values(&id, "env", env)?;
            (
                McpServerToml {
                    transport: McpTransportToml::Stdio,
                    command: Some(command),
                    args: Some(args),
                    env,
                    cwd: None,
                    url: None,
                    headers: BTreeMap::new(),
                    timeout_ms: None,
                },
                stored,
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
            let (headers, stored) = store_secret_values(&id, "headers", headers)?;
            (
                McpServerToml {
                    transport: McpTransportToml::StreamableHttp,
                    command: None,
                    args: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    url: Some(url),
                    headers,
                    timeout_ms: Some(timeout_ms),
                },
                stored,
            )
        }
    };

    let written = config::write_mcp_server(scope, &id, &server)?;
    println!("Added mcp server `{id}` to {}.", written.display());
    if stored_secrets != 0 && scope == ConfigScope::Local {
        println!(
            "Stored {stored_secrets} secret value(s) in the encrypted store; workspace config \
             carries only `secret://` references, which resolve after the workspace is trusted."
        );
    }
    Ok(())
}

/// Stores secret-flagged values in the encrypted secrets store and returns
/// the config values (references for secrets, literals otherwise) plus the
/// number of stored secrets. Fails before any config write on store errors.
fn store_secret_values(
    server_id: &str,
    table: &str,
    prompted: BTreeMap<String, PromptedValue>,
) -> Result<(BTreeMap<String, String>, usize), String> {
    let store = if prompted.values().any(|value| value.secret) {
        Some(
            SecretStore::default()
                .map_err(|error| format!("cannot open encrypted secrets store: {error}"))?,
        )
    } else {
        None
    };
    let mut values = BTreeMap::new();
    let mut stored = 0;
    for (name, value) in prompted {
        if !value.secret {
            values.insert(name, value.value);
            continue;
        }
        let secret_name = SecretName::new(&format!("mcp.{server_id}.{table}.{name}"))
            .map_err(|error| format!("cannot store `{name}` as a secret: {error}"))?;
        let store = store.as_ref().ok_or_else(|| String::from("secrets store unavailable"))?;
        store
            .set(&secret_name, &Zeroizing::new(value.value))
            .map_err(|error| format!("cannot store secret `{name}`: {error}"))?;
        values.insert(name, SecretReference::from_name(secret_name).to_string());
        stored += 1;
    }
    Ok((values, stored))
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
    report_orphaned_secrets(id);
    Ok(())
}

/// Prints the vault entries this wizard created for `server_id` when no config
/// layer defines the server anymore. Never deletes: removing stored values
/// stays an explicit `ee do secrets delete <name>`. Best-effort: store or
/// config errors stay silent because the config removal already succeeded.
fn report_orphaned_secrets(server_id: &str) {
    let still_configured = [ConfigScope::Local, ConfigScope::Global].iter().any(|scope| {
        config::list_mcp_servers(*scope)
            .map(|servers| servers.iter().any(|(id, _)| id == server_id))
            // On a config read error stay quiet instead of claiming orphaned.
            .unwrap_or(true)
    });
    if still_configured {
        return;
    }
    let Ok(store) = SecretStore::default() else {
        return;
    };
    let names = orphaned_secret_names(&store, server_id);
    if names.is_empty() {
        return;
    }
    println!("Stored secret(s) for `{server_id}` are no longer referenced by any config layer:");
    for name in names {
        println!("  ee do secrets delete {name}");
    }
}

/// Names in `mcp.<server_id>.` namespace, sorted. Server ids cannot contain
/// `.`, so the trailing dot makes the namespace exact.
fn orphaned_secret_names(store: &SecretStore, server_id: &str) -> Vec<String> {
    let prefix = format!("mcp.{server_id}.");
    let Ok(names) = store.list() else {
        return Vec::new();
    };
    let mut orphaned = names
        .into_iter()
        .map(|name| name.to_string())
        .filter(|name| name.starts_with(&prefix))
        .collect::<Vec<_>>();
    orphaned.sort();
    orphaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::test_support::{StoredKeychain, test_binding};

    #[test]
    fn prompted_env_names_use_env_rule_and_headers_use_token_rule() {
        assert!(validate_env_name("API_KEY"));
        assert!(!validate_header_name("Bearer: abc"), "colon is not a header token");
        assert!(validate_header_name("X-API-Key"));
    }

    #[test]
    fn orphaned_secret_names_match_only_the_removed_server_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let store = SecretStore::new(
            Box::new(StoredKeychain::new()),
            test_binding(),
            temp.path().join("vault.json"),
        );
        for name in [
            "mcp.github.env.GITHUB_TOKEN",
            "mcp.github.headers.Authorization",
            "mcp.github-tools.env.TOKEN",
            "mcp.other.env.TOKEN",
            "agent.github.KEY",
        ] {
            store.set(&SecretName::new(name).unwrap(), &Zeroizing::new(String::from("v"))).unwrap();
        }

        assert_eq!(
            orphaned_secret_names(&store, "github"),
            ["mcp.github.env.GITHUB_TOKEN", "mcp.github.headers.Authorization"]
        );
        assert!(orphaned_secret_names(&store, "absent").is_empty());
    }
}
