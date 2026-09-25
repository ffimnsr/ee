//! Thin stdio entry point for `ee-opencode-agent`.
//!
//! The binary resolves one explicit `OPENCODE_SURFACE` + `OPENCODE_MODEL` route,
//! builds only that route's dialect codec, and hands the resulting orchestrator
//! provider to `ee-acp-agent-server`, which owns the ACP protocol loop. There is
//! no second ACP loop, tool executor, or simple provider mode on this path, and
//! nothing is written to stdout except setup-manifest and diagnostic JSON, so the
//! stdio channel stays free for ACP frames.
//!
//! `--ee-config` prints the setup manifest without reading a credential or
//! contacting anything, and an unroutable model fails closed before any HTTP
//! client, request body, or `Authorization` header exists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Parser;
use ee_acp_agent_server::{AcpAgentServer, AcpAgentServerConfig, env_or_dotenv, load_dotenv};
use ee_opencode_agent::config::{self, Args, Config};
use ee_opencode_agent::critic::opencode_multi_model_provider;
use ee_opencode_agent::discovery::{self, HttpMetadataFetcher};
use ee_opencode_agent::routes::{self, OpenCodeRoute};
use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    if args.ee_config {
        print_manifest();
        return;
    }
    let result = if args.print_catalog {
        print_catalog()
    } else if args.print_route {
        print_route()
    } else if args.discover_models {
        print_discovered_models(args).await
    } else {
        run(args).await
    };
    if let Err(error) = result {
        eprintln!("ee-opencode-agent: {error}");
        std::process::exit(1);
    }
}

/// Prints the setup manifest and exits without loading credentials or starting
/// the agent.
fn print_manifest() {
    match serde_json::to_string(&config::setup_manifest()) {
        Ok(manifest) => println!("{manifest}"),
        Err(error) => {
            eprintln!("ee-opencode-agent: failed to serialize setup manifest: {error}");
            std::process::exit(1);
        }
    }
}

/// Prints every documented routable `(surface, model)` entry.
fn print_catalog() -> Result<(), String> {
    let catalog: Vec<Value> = routes::catalog().map(|route| route_json(&route)).collect();
    print_json(&Value::Array(catalog))
}

/// Prints the exact route for `OPENCODE_SURFACE` + `OPENCODE_MODEL`, reading the
/// process environment and `.env` the same way the agent does.
fn print_route() -> Result<(), String> {
    let dotenv = read_dotenv();
    let route = config::resolve_route_from(|name| env_or_dotenv(name, &dotenv))
        .map_err(|error| error.to_string())?;
    print_json(&route_json(&route))
}

/// Fetches the resolved surface's `/models` metadata.
///
/// This is an explicit local action: one request to the trusted surface root,
/// and the printed report is display-only — it never selects a route.
async fn print_discovered_models(args: Args) -> Result<(), String> {
    let config =
        Config::from_args_and_dotenv(args, &read_dotenv()).map_err(|error| error.to_string())?;
    if !config.has_api_key() {
        return Err(String::from(config::MISSING_API_KEY));
    }
    let fetcher = HttpMetadataFetcher::new(&config)?;
    let report = discovery::discover_models(&fetcher, config.route.surface).await?;
    print_json(&report.to_json())
}

/// Runs the production path: resolve config, build the routed adapter (plus the
/// configured critic), serve ACP over stdio.
async fn run(args: Args) -> Result<(), String> {
    let config =
        Config::from_args_and_dotenv(args, &read_dotenv()).map_err(|error| error.to_string())?;
    if !config.has_api_key() {
        return Err(String::from(config::MISSING_API_KEY));
    }
    if let Some(note) = config.reasoning_effort_note() {
        eprintln!("ee-opencode-agent: warning: {note}");
    }
    let (provider, critic_warning) =
        opencode_multi_model_provider(&config, default_session_state_dir()?)?;
    if let Some(warning) = critic_warning {
        eprintln!("ee-opencode-agent: warning: {warning}");
    }
    AcpAgentServer::new(provider, AcpAgentServerConfig::default())
        .run_stdio()
        .await
        .map_err(|error| error.to_string())
}

/// Reads `.env` next to the working directory without mutating the process
/// environment; a missing or unreadable file is not fatal.
fn read_dotenv() -> BTreeMap<String, String> {
    match load_dotenv(Path::new(".env")) {
        Ok(values) => values,
        Err(error) => {
            eprintln!("ee-opencode-agent: warning: failed to read .env: {error}");
            BTreeMap::new()
        }
    }
}

/// Per-user durable storage for normal ACP sessions. Kept outside workspaces so
/// conversation snapshots never alter project files or repository state.
fn default_session_state_dir() -> Result<PathBuf, String> {
    dirs::data_local_dir().map(|directory| directory.join("ee").join("agent-sessions")).ok_or_else(
        || "could not determine local data directory for agent session state".to_string(),
    )
}

/// One machine-readable route record; never carries a credential.
fn route_json(route: &OpenCodeRoute) -> Value {
    json!({
        "surface": route.surface.as_str(),
        "model_id": route.model_id,
        "dialect": route.dialect.as_str(),
        "endpoint": route.endpoint,
    })
}

fn print_json(value: &Value) -> Result<(), String> {
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|error| format!("failed to serialize output: {error}"))?;
    println!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_opencode_agent::routes::OpenCodeSurface;

    #[test]
    fn route_json_carries_exact_catalog_fields() {
        let route =
            routes::resolve_route(OpenCodeSurface::Zen, "gpt-5.5").expect("documented route");
        let value = route_json(&route);

        // Field-by-field so the assertion stays independent of JSON key order.
        assert_eq!(value["surface"], json!("zen"));
        assert_eq!(value["model_id"], json!("gpt-5.5"));
        assert_eq!(value["dialect"], json!("openai_responses"));
        assert_eq!(value["endpoint"], json!("https://opencode.ai/zen/v1/responses"));
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(4));
    }

    #[test]
    fn printing_the_catalog_succeeds_without_credentials_or_configuration() {
        print_catalog().expect("catalog prints");
    }

    #[test]
    fn setup_manifest_declares_the_secret_without_a_credential_value() {
        let manifest = config::setup_manifest();
        let encoded = serde_json::to_string(&manifest).expect("manifest encodes");

        assert_eq!(manifest.agent.id, "opencode");
        assert_eq!(manifest.agent.display_name, "OpenCode");
        assert_eq!(manifest.env_vars[0].name, config::API_KEY_ENV);
        assert!(manifest.env_vars[0].secret);
        assert!(manifest.env_vars[0].required);
        assert!(!encoded.contains("sk-"), "the manifest names the secret, never its value");
    }

    #[test]
    fn session_state_lives_under_the_ee_local_data_convention() {
        let directory = default_session_state_dir().expect("local data directory resolves");

        assert!(directory.ends_with("ee/agent-sessions"), "{}", directory.display());
    }

    #[tokio::test]
    async fn unroutable_model_fails_closed_before_any_request() {
        let args = Args::for_tests(Some("zen"), Some("not-a-documented-model"));

        let error = run(args).await.expect_err("unknown model cannot route");

        assert!(error.contains("is not a documented OpenCode zen model"), "{error}");
        assert!(!error.contains(config::API_KEY_ENV), "no credential path was reached: {error}");
    }

    #[tokio::test]
    async fn cross_surface_model_fails_closed_with_the_other_surface_named() {
        let args = Args::for_tests(Some("go"), Some("gpt-5.5"));

        let error = run(args).await.expect_err("cross-surface model cannot route");

        assert!(error.contains("documented for OpenCode zen, not go"), "{error}");
    }
}
