//! Thin stdio entry point: parse arguments, load `.env`, build the provider
//! (simple or orchestrated), and let the framework own the ACP protocol loop.

use clap::Parser;
use ee_agent_acp_server::{AcpAgentServer, AcpAgentServerConfig};
use ee_agent_openrouter::config::{Args, Config, setup_manifest};
use ee_agent_openrouter::dotenv::load_dotenv;
use ee_agent_openrouter::orchestrated::openrouter_multi_model_provider;
use ee_agent_openrouter::provider::OpenRouterProvider;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
        match serde_json::to_string(&setup_manifest()) {
            Ok(manifest) => println!("{manifest}"),
            Err(error) => {
                eprintln!("ee-agent-openrouter: failed to serialize setup manifest: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.list_models {
        if let Err(error) = print_models(args.json) {
            eprintln!("ee-agent-openrouter: {error}");
            std::process::exit(1);
        }
        return;
    }

    let dotenv = match load_dotenv(Path::new(".env")) {
        Ok(values) => values,
        Err(error) => {
            eprintln!("ee-agent-openrouter: warning: failed to read .env: {error}");
            BTreeMap::new()
        }
    };
    let config = match Config::from_args_and_dotenv(args, &dotenv) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("ee-agent-openrouter: {error}");
            std::process::exit(1);
        }
    };
    let server_config = AcpAgentServerConfig::default();
    let result = match provider_mode(&config) {
        ProviderMode::Orchestrated => run_orchestrated(config, server_config).await,
        ProviderMode::Simple => run_simple(config, server_config).await,
    };
    if let Err(error) = result {
        eprintln!("ee-agent-openrouter: {error}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderMode {
    Simple,
    Orchestrated,
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

/// Prints the full choice list for one input accepted by
/// [`ee_agent_openrouter::config::remote_choices`].
fn print_choices_request(key: &str) {
    // The host passes the values collected so far as process environment, so
    // the model-scoped effort list can read the already selected model.
    let model = std::env::var("OPENROUTER_MODEL").ok();
    let Some(choices) = ee_agent_openrouter::config::remote_choices(key, model.as_deref()) else {
        eprintln!("ee-agent-openrouter: `{key}` does not accept remote choices");
        std::process::exit(1);
    };
    match serde_json::to_string(&choices) {
        Ok(encoded) => println!("{encoded}"),
        Err(error) => {
            eprintln!("ee-agent-openrouter: failed to serialize choices: {error}");
            std::process::exit(1);
        }
    }
}

/// Prints the generated OpenRouter catalog as JSON or a stable table.
///
/// Listing is strictly local and display-only: it reads the committed table,
/// never the process environment, a credential, or the network, and its output
/// can never select an endpoint or a route.
fn print_models(json: bool) -> Result<(), String> {
    let mut models: Vec<&'static ee_agent_openrouter::catalog::CatalogModel> =
        ee_agent_openrouter::catalog::models().collect();
    models.sort_by_key(|entry| entry.id);
    if json {
        let rows: Vec<serde_json::Value> = models.iter().map(|entry| model_json(entry)).collect();
        let rendered = serde_json::to_string_pretty(&serde_json::Value::Array(rows))
            .map_err(|error| format!("failed to serialize output: {error}"))?;
        println!("{rendered}");
        return Ok(());
    }
    for entry in &models {
        println!("{}", model_line(entry));
    }
    Ok(())
}

/// One listing row for the machine-readable form.
fn model_json(entry: &ee_agent_openrouter::catalog::CatalogModel) -> serde_json::Value {
    use ee_agent_orchestrator::ReasoningProtocol;
    let variants =
        |protocol| entry.variants(protocol).iter().map(|variant| variant.id).collect::<Vec<_>>();
    serde_json::json!({
        "id": entry.id,
        "kind": entry.kind.as_str(),
        "status": if entry.status.is_empty() { "active" } else { entry.status },
        "family": entry.family,
        "base_url": entry.base_url,
        "routes": {
            "chat": entry.routes.chat,
            "responses": entry.routes.responses,
            "messages": entry.routes.messages,
            "images": entry.routes.images,
            "systemone": entry.routes.systemone,
        },
        "limits": entry.limits.map(|limits| serde_json::json!({
            "context": limits.context,
            "output": limits.output,
        })),
        "modalities": {
            "input": entry.modalities.input,
            "output": entry.modalities.output,
        },
        "reasoning": {
            "supported": entry.reasoning.supported,
            "toggle": entry.reasoning.toggle,
            "efforts": entry.reasoning.efforts,
            "variants": {
                "chat": variants(ReasoningProtocol::OpenAiChat),
                "responses": variants(ReasoningProtocol::OpenAiResponses),
            },
        },
        "cost_micros": entry.cost.map(|cost| serde_json::json!({
            "input": cost.input_micros,
            "output": cost.output_micros,
            "cache_read": cost.cache_read_micros,
            "cache_write": cost.cache_write_micros,
            "tiers": cost.tiers.len(),
        })),
        "tools": {
            "supported": entry.tools_supported,
            "strict": entry.tools_strict,
            "freeform": entry.freeform,
        },
        "structured_output": entry.structured_output,
    })
}

/// One tab-separated listing line.
fn model_line(entry: &ee_agent_openrouter::catalog::CatalogModel) -> String {
    use ee_agent_orchestrator::{ReasoningProtocol, format_price_micros};
    let limits = entry
        .limits
        .map(|limits| format!("ctx={} out={}", limits.context, limits.output))
        .unwrap_or_else(|| String::from("ctx=? out=?"));
    let variants = entry
        .variants(ReasoningProtocol::OpenAiChat)
        .iter()
        .map(|variant| variant.id)
        .collect::<Vec<_>>()
        .join(",");
    let reasoning = if variants.is_empty() {
        String::from("reasoning=none")
    } else {
        format!("reasoning={variants}")
    };
    let cost = entry.cost.map_or_else(
        || String::from("cost=unknown"),
        |cost| {
            format!(
                "in={} out={}",
                format_price_micros(cost.input_micros),
                format_price_micros(cost.output_micros)
            )
        },
    );
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        entry.id,
        entry.kind.as_str(),
        limits,
        reasoning,
        cost,
        if entry.status.is_empty() { "status=active" } else { entry.status },
        if entry.routes.responses { "routes=chat,responses" } else { "routes=chat" }
    )
}

fn provider_mode(config: &Config) -> ProviderMode {
    if config.orchestrated { ProviderMode::Orchestrated } else { ProviderMode::Simple }
}

/// Simple provider mode: OpenRouter owns session history and the bounded
/// tool loop, the framework owns the protocol.
async fn run_simple(config: Config, server_config: AcpAgentServerConfig) -> Result<(), String> {
    let provider = OpenRouterProvider::new(config)?;
    AcpAgentServer::new(provider, server_config)
        .run_stdio()
        .await
        .map_err(|error| error.to_string())
}

/// Orchestrated mode: the orchestrator owns the model–tool loop, task graph,
/// memory, budgets, and policy; OpenRouter only answers model calls.
async fn run_orchestrated(
    config: Config,
    server_config: AcpAgentServerConfig,
) -> Result<(), String> {
    let (provider, critic_warning) =
        openrouter_multi_model_provider(&config, default_session_state_dir()?)?;
    if let Some(warning) = critic_warning {
        eprintln!("ee-agent-openrouter: warning: {warning}");
    }
    AcpAgentServer::new(provider, server_config)
        .run_stdio()
        .await
        .map_err(|error| error.to_string())
}

/// Per-user durable storage for normal ACP sessions. Kept outside workspaces
/// so conversation snapshots never alter project files or repository state.
fn default_session_state_dir() -> Result<PathBuf, String> {
    dirs::data_local_dir().map(|directory| directory.join("ee").join("agent-sessions")).ok_or_else(
        || "could not determine local data directory for agent session state".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use ee_agent_openrouter::config::DEFAULT_CONTEXT_WINDOW_TOKENS;
    use ee_agent_openrouter::config::{DEFAULT_API_URL, DEFAULT_RESPONSES_URL, OpenRouterApiKind};
    use ee_agent_openrouter::orchestrated::openrouter_orchestrator_config;

    fn config(orchestrated: bool) -> Config {
        Config {
            model: String::from("test/model"),
            model_family: None,
            rubber_duck_model: None,
            rubber_duck_model_family: None,
            rubber_duck: ee_agent_orchestrator::RubberDuckConfig::default(),
            api_url: String::from(DEFAULT_API_URL),
            api_kind: OpenRouterApiKind::Chat,
            responses_url: String::from(DEFAULT_RESPONSES_URL),
            api_key: Some(String::from("sk-test")),
            site_url: None,
            app_title: String::from("ee-test"),
            timeouts: ee_chat_completions::Timeouts::new().with_total(Some(Duration::from_secs(1))),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated,
            compact_min_messages: 4,
            compact_retained_tail: 2,
            compact_max_input_bytes: 65_536,
            context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
            auto_compact_threshold_percent: 80,
            max_iterations: Some(16),
            retry_max_attempts: 0,
            retry_base_delay: Duration::from_millis(1),
            retry_max_delay: Duration::from_millis(10),
            checkpoint_dir: None,
            session_log_dir: None,
            session_log_durable: false,
            session_headers: true,
            compaction_mode: ee_agent_orchestrator::CompactionMode::Local,
            file: Default::default(),
        }
    }

    #[test]
    fn choices_request_flag_accepts_both_spellings_and_ignores_other_args() {
        fn args(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| String::from(*value)).collect()
        }

        assert_eq!(
            choices_request_key(args(&["--ee-config-choices=model"])),
            Some(String::from("model"))
        );
        assert_eq!(
            choices_request_key(args(&["--ee-config-choices", "rubber_duck_model"])),
            Some(String::from("rubber_duck_model"))
        );
        assert_eq!(choices_request_key(args(&["--list-models", "--json"])), None);
        assert_eq!(choices_request_key(args(&["--ee-config-choices"])), None);
    }

    #[test]
    fn list_models_reports_catalog_rows_with_limits_cost_and_variants() {
        print_models(false).expect("text listing prints");

        let entry =
            ee_agent_openrouter::catalog::find("deepseek/deepseek-v4-flash").expect("catalog row");
        let row = model_json(entry);
        assert_eq!(row["kind"], "chat");
        assert!(row["limits"]["context"].as_u64().is_some_and(|value| value > 0));
        assert!(row["cost_micros"]["input"].as_u64().is_some());
        assert!(row["reasoning"]["variants"]["chat"].is_array());
        assert_eq!(row["routes"]["chat"], true);

        let line = model_line(entry);
        assert!(line.contains("deepseek/deepseek-v4-flash"), "{line}");
        assert!(line.contains("ctx="), "{line}");
    }

    #[test]
    fn list_models_needs_no_credential_or_network() {
        // No OPENROUTER_API_KEY is set in the test process; listing must still
        // succeed because it reads only the committed catalog.
        print_models(true).expect("json listing prints");
    }

    #[test]
    fn default_config_selects_orchestrator_provider_path() {
        let config = config(true);

        assert_eq!(provider_mode(&config), ProviderMode::Orchestrated);
    }

    #[test]
    fn shared_iteration_cap_applies_to_loop_and_model_call_budgets() {
        let mut config = config(true);
        config.max_iterations = Some(64);

        let orchestrator =
            openrouter_orchestrator_config(&config, PathBuf::from("/tmp/ee-agent-sessions"))
                .orchestrator;

        assert_eq!(orchestrator.max_loop_iterations, Some(64));
        assert_eq!(orchestrator.max_model_calls, Some(64));
    }

    #[test]
    fn rubber_duck_policy_reaches_production_orchestrator_config() {
        let mut config = config(true);
        config.rubber_duck.mode = ee_agent_orchestrator::RubberDuckMode::Automatic;
        config.rubber_duck.max_calls = 3;
        config.rubber_duck.timeout = Duration::from_secs(5);

        let orchestrator =
            openrouter_orchestrator_config(&config, PathBuf::from("/tmp/ee-agent-sessions"))
                .orchestrator;

        assert_eq!(orchestrator.rubber_duck, config.rubber_duck);
        assert_eq!(
            orchestrator.rubber_duck_triggers.mode,
            ee_agent_orchestrator::RubberDuckTriggerMode::Automatic
        );
    }

    #[test]
    fn opt_out_config_selects_simple_openrouter_provider_path() {
        let config = config(false);

        assert_eq!(provider_mode(&config), ProviderMode::Simple);
    }
}
