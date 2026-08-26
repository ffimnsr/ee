//! Thin stdio entry point: parse arguments, load `.env`, build the provider
//! (simple or orchestrated), and let the framework own the ACP protocol loop.

use clap::Parser;
use ee_acp_agent_server::{AcpAgentServer, AcpAgentServerConfig};
use ee_openrouter_agent::config::{Args, Config, setup_manifest};
use ee_openrouter_agent::dotenv::load_dotenv;
use ee_openrouter_agent::orchestrated::{OpenRouterModelAdapter, openrouter_orchestrated_provider};
use ee_openrouter_agent::provider::OpenRouterProvider;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    if args.ee_config {
        match serde_json::to_string(&setup_manifest()) {
            Ok(manifest) => println!("{manifest}"),
            Err(error) => {
                eprintln!("ee-openrouter-agent: failed to serialize setup manifest: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    let dotenv = match load_dotenv(Path::new(".env")) {
        Ok(values) => values,
        Err(error) => {
            eprintln!("ee-openrouter-agent: warning: failed to read .env: {error}");
            BTreeMap::new()
        }
    };
    let config = Config::from_args_and_dotenv(args, &dotenv);
    let server_config = AcpAgentServerConfig::default();
    let result = match provider_mode(&config) {
        ProviderMode::Orchestrated => run_orchestrated(config, server_config).await,
        ProviderMode::Simple => run_simple(config, server_config).await,
    };
    if let Err(error) = result {
        eprintln!("ee-openrouter-agent: {error}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderMode {
    Simple,
    Orchestrated,
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
    let adapter = OpenRouterModelAdapter::new(config.clone())?;
    let provider = openrouter_orchestrated_provider(&config, default_session_state_dir()?, adapter);
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
    use ee_openrouter_agent::config::DEFAULT_API_URL;
    use ee_openrouter_agent::config::DEFAULT_CONTEXT_WINDOW_TOKENS;
    use ee_openrouter_agent::orchestrated::openrouter_orchestrator_config;

    fn config(orchestrated: bool) -> Config {
        Config {
            model: String::from("test/model"),
            api_url: String::from(DEFAULT_API_URL),
            api_key: Some(String::from("sk-test")),
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: Duration::from_secs(1),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated,
            compact_min_messages: 4,
            compact_retained_tail: 2,
            compact_max_input_bytes: 65_536,
            context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
            auto_compact_threshold_percent: 80,
            max_iterations: 16,
            retry_max_attempts: 0,
            retry_base_delay: Duration::from_millis(1),
            retry_max_delay: Duration::from_millis(10),
            checkpoint_dir: None,
        }
    }

    #[test]
    fn default_config_selects_orchestrator_provider_path() {
        let config = config(true);

        assert_eq!(provider_mode(&config), ProviderMode::Orchestrated);
    }

    #[test]
    fn shared_iteration_cap_applies_to_loop_and_model_call_budgets() {
        let mut config = config(true);
        config.max_iterations = 64;

        let orchestrator =
            openrouter_orchestrator_config(&config, PathBuf::from("/tmp/ee-agent-sessions"))
                .orchestrator;

        assert_eq!(orchestrator.max_loop_iterations, 64);
        assert_eq!(orchestrator.max_model_calls, 64);
    }

    #[test]
    fn opt_out_config_selects_simple_openrouter_provider_path() {
        let config = config(false);

        assert_eq!(provider_mode(&config), ProviderMode::Simple);
    }
}
