//! Thin stdio entry point for `ee-agent-opencode`.
//!
//! The binary resolves one explicit `OPENCODE_SURFACE` + `OPENCODE_MODEL` route,
//! builds only that route's dialect codec, and hands the resulting orchestrator
//! provider to `ee-agent-acp-server`, which owns the ACP protocol loop. There is
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
use ee_agent_acp_server::{AcpAgentServer, AcpAgentServerConfig, env_or_dotenv, load_dotenv};
use ee_agent_opencode::config::{self, Args, Config};
use ee_agent_opencode::critic::opencode_multi_model_provider;
use ee_agent_opencode::discovery::{self, HttpMetadataFetcher};
use ee_agent_opencode::routes::{self, OpenCodeRoute, OpenCodeSurface};
use ee_agent_opencode::usage::{self, UsageProbe};
use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Setup-only flag, handled before clap so it never joins the normal ACP
    // argument set: the host re-invokes this binary per input to fetch choices.
    if let Some(key) = choices_request_key(std::env::args().skip(1)) {
        print_choices_request(&key);
        return;
    }
    let args = Args::parse();
    if args.ee_config {
        print_manifest();
        return;
    }
    let result = if args.print_catalog {
        print_catalog()
    } else if args.list_models {
        print_models(args.json)
    } else if args.print_route {
        print_route()
    } else if args.discover_models {
        print_discovered_models(args).await
    } else if args.usage {
        print_usage().await
    } else {
        run(args).await
    };
    if let Err(error) = result {
        eprintln!("ee-agent-opencode: {error}");
        std::process::exit(1);
    }
}

/// Prints the setup manifest and exits without loading credentials or starting
/// the agent.
fn print_manifest() {
    match serde_json::to_string(&config::setup_manifest()) {
        Ok(manifest) => println!("{manifest}"),
        Err(error) => {
            eprintln!("ee-agent-opencode: failed to serialize setup manifest: {error}");
            std::process::exit(1);
        }
    }
}

/// Extracts the input key from a `--ee-config-choices` request.
fn choices_request_key<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if let Some(key) = arg.strip_prefix("--ee-config-choices=") {
            return Some(key.to_owned());
        }
        if arg == "--ee-config-choices" {
            return args.next();
        }
    }
    None
}

/// Prints the full choice list for one input, scoped by the values the host
/// already collected (passed back as `OPENCODE_SURFACE` / `OPENCODE_MODEL`).
fn print_choices_request(key: &str) {
    let surface = std::env::var(config::SURFACE_ENV)
        .ok()
        .and_then(|value| OpenCodeSurface::parse(value.trim()).ok());
    let Some(surface) = surface else {
        eprintln!(
            "ee-agent-opencode: --ee-config-choices needs {} to be \"zen\" or \"go\"",
            config::SURFACE_ENV
        );
        std::process::exit(1);
    };
    let model = std::env::var(config::MODEL_ENV).ok();
    let Some(choices) = config::remote_choices(surface, key, model.as_deref()) else {
        eprintln!("ee-agent-opencode: `{key}` does not accept remote choices");
        std::process::exit(1);
    };
    match serde_json::to_string(&choices) {
        Ok(encoded) => println!("{encoded}"),
        Err(error) => {
            eprintln!("ee-agent-opencode: failed to serialize choices: {error}");
            std::process::exit(1);
        }
    }
}

/// Prints every documented routable `(surface, model)` entry.
fn print_catalog() -> Result<(), String> {
    let catalog: Vec<Value> = routes::catalog().map(|route| route_json(&route)).collect();
    print_json(&Value::Array(catalog))
}

/// Prints the catalog as JSON or a stable tab-separated table.
///
/// Listing is strictly local and display-only: it reads the generated table,
/// never the process environment, a credential, or the network, and its output
/// can never select an endpoint or a route.
fn print_models(json: bool) -> Result<(), String> {
    let routes = sorted_catalog();
    if json {
        let rows: Vec<Value> = routes.iter().map(model_json).collect();
        return print_json(&Value::Array(rows));
    }
    for route in &routes {
        println!("{}", model_line(route));
    }
    Ok(())
}

/// Stable, sorted listing order: surface first, then catalog id.
///
/// Legacy rows keep their place in the same order instead of being appended
/// unsorted, so `--list-models` output is reproducible across releases.
fn sorted_catalog() -> Vec<OpenCodeRoute> {
    let mut routes: Vec<OpenCodeRoute> = routes::catalog().collect();
    routes.sort_by(|left, right| {
        left.surface.as_str().cmp(right.surface.as_str()).then(left.model_id.cmp(right.model_id))
    });
    routes
}

/// One listing row: id, kind, protocol, limits, modalities, reasoning, cost.
fn model_json(route: &OpenCodeRoute) -> Value {
    let entry = route.entry;
    let limits = route.limits();
    let cost = route.cost();
    json!({
        "id": route.model_id,
        "surface": route.surface.as_str(),
        "kind": entry.kind.as_str(),
        "protocol": route.dialect.as_str(),
        "endpoint": route.endpoint,
        "status": if entry.legacy { "legacy" } else if entry.status.is_empty() { "active" } else { entry.status },
        "family": entry.family,
        "limits": limits.map(|limits| json!({ "context": limits.context, "output": limits.output })),
        "modalities": {
            "input": entry.modalities.input,
            "output": entry.modalities.output,
        },
        "reasoning": {
            "supported": entry.reasoning.supported,
            "toggle": entry.reasoning.toggle,
            "efforts": entry.reasoning.efforts,
            "variants": route.variants().iter().map(|variant| variant.id).collect::<Vec<_>>(),
        },
        "cost_micros": cost.map(|cost| json!({
            "input": cost.input_micros,
            "output": cost.output_micros,
            "cache_read": cost.cache_read_micros,
            "cache_write": cost.cache_write_micros,
        })),
        "tools": {
            "supported": entry.tools_supported,
            "strict": entry.tools_strict,
            "freeform": entry.freeform,
        },
        "structured_output": entry.structured_output,
        "legacy": entry.legacy,
    })
}

/// One tab-separated listing line.
fn model_line(route: &OpenCodeRoute) -> String {
    let entry = route.entry;
    let limits = route
        .limits()
        .map(|limits| format!("ctx={} out={}", limits.context, limits.output))
        .unwrap_or_else(|| String::from("ctx=? out=?"));
    let variants = route.variants().iter().map(|variant| variant.id).collect::<Vec<_>>().join(",");
    let reasoning = if variants.is_empty() {
        String::from("reasoning=none")
    } else {
        format!("reasoning={variants}")
    };
    let cost = route.cost().map_or_else(
        || String::from("cost=unknown"),
        |cost| {
            format!(
                "in={} out={}",
                ee_agent_orchestrator::format_price_micros(cost.input_micros),
                ee_agent_orchestrator::format_price_micros(cost.output_micros)
            )
        },
    );
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        route.surface.as_str(),
        route.model_id,
        route.dialect.as_str(),
        entry.kind.as_str(),
        limits,
        reasoning,
        cost,
        if entry.legacy {
            "status=legacy"
        } else if entry.status.is_empty() {
            "status=active"
        } else {
            entry.status
        }
    )
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

/// Queries workspace budgets and the Go allowance. Reachable only through
/// `--usage`: no startup path performs this request.
async fn print_usage() -> Result<(), String> {
    let dotenv = read_dotenv();
    let probe = UsageProbe::resolve(&mut |name| env_or_dotenv(name, &dotenv))?;
    let report = usage::fetch_usage(&probe).await?;
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
        eprintln!("ee-agent-opencode: warning: {note}");
    }
    let (provider, critic_warning) =
        opencode_multi_model_provider(&config, default_session_state_dir()?)?;
    if let Some(warning) = critic_warning {
        eprintln!("ee-agent-opencode: warning: {warning}");
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
            eprintln!("ee-agent-opencode: warning: failed to read .env: {error}");
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
    let limits = route.limits();
    let reasoning = route.reasoning();
    let cost = route.cost();
    json!({
        "surface": route.surface.as_str(),
        "model_id": route.model_id,
        "api_model_id": route.api_model_id(),
        "dialect": route.dialect.as_str(),
        "endpoint": route.endpoint,
        "legacy": route.is_legacy(),
        "limits": limits.map(|limits| json!({
            "context": limits.context,
            "output": limits.output,
        })),
        "reasoning": {
            "supported": reasoning.supported,
            "toggle": reasoning.toggle,
            "efforts": reasoning.efforts,
            "budget_min": reasoning.budget_min,
            "budget_max": reasoning.budget_max,
        },
        "cost_micros": cost.map(|cost| json!({
            "input": cost.input_micros,
            "output": cost.output_micros,
            "cache_read": cost.cache_read_micros,
            "cache_write": cost.cache_write_micros,
        })),
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
    use ee_agent_opencode::routes::OpenCodeSurface;

    #[test]
    fn list_models_rows_carry_limits_cost_variants_and_status() {
        let route = routes::resolve_route(OpenCodeSurface::Zen, "gpt-5.5").expect("route");
        let row = model_json(&route);

        assert_eq!(row["id"], json!("gpt-5.5"));
        assert_eq!(row["kind"], json!("chat"));
        assert_eq!(row["status"], json!("active"));
        assert!(row["limits"]["context"].as_u64().is_some_and(|value| value > 0));
        assert!(row["cost_micros"]["input"].as_u64().is_some());
        assert!(
            row["reasoning"]["variants"]
                .as_array()
                .expect("variants")
                .iter()
                .any(|variant| variant == "high"),
            "catalog variants are listed"
        );

        let line = model_line(&route);
        assert!(line.starts_with("zen\tgpt-5.5\t"), "{line}");
        assert!(line.contains("ctx="), "{line}");
    }

    #[test]
    fn list_models_is_sorted_and_needs_no_credential() {
        print_models(true).expect("json listing prints");
        print_models(false).expect("text listing prints");

        let routes = sorted_catalog();
        let mut last_surface: Option<OpenCodeSurface> = None;
        let mut last_id = "";
        for route in &routes {
            if last_surface != Some(route.surface) {
                last_surface = Some(route.surface);
                last_id = "";
            }
            assert!(
                last_id <= route.model_id,
                "ids are sorted within {}: {last_id} > {}",
                route.surface.as_str(),
                route.model_id
            );
            last_id = route.model_id;
        }
    }

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
        assert_eq!(value["api_model_id"], json!("gpt-5.5"));
        assert_eq!(value["legacy"], json!(false));
        assert!(value["limits"]["context"].as_u64().is_some_and(|value| value > 0));
        assert!(value["limits"]["output"].as_u64().is_some_and(|value| value > 0));
        assert_eq!(value["reasoning"]["supported"], json!(true));
        assert!(value["cost_micros"]["input"].as_u64().is_some());
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(9));
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
    fn choices_request_flag_accepts_both_spellings_and_ignores_other_args() {
        fn args(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| String::from(*value)).collect()
        }

        assert_eq!(choices_request_key(args(&["--ee-config-choices=model"])), Some("model".into()));
        assert_eq!(
            choices_request_key(args(&["--ee-config-choices", "critic_model"])),
            Some("critic_model".into())
        );
        assert_eq!(choices_request_key(args(&["--list-models", "--json"])), None);
        assert_eq!(choices_request_key(args(&["--ee-config-choices"])), None);
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
