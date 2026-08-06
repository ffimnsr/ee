//! Thin stdio entry point: parse arguments, load `.env`, build the provider
//! (simple or orchestrated), and let the framework own the ACP protocol loop.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use ee_acp_agent_server::{AcpAgentServer, AcpAgentServerConfig};
use ee_agent_orchestrator::{OrchestratorProvider, OrchestratorProviderConfig};
use ee_agent_protocol::Implementation;
use ee_openrouter_agent::config::{Args, Config};
use ee_openrouter_agent::dotenv::load_dotenv;
use ee_openrouter_agent::orchestrated::{OpenRouterModelAdapter, openrouter_orchestrated_policy};
use ee_openrouter_agent::provider::OpenRouterProvider;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
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
    let adapter = OpenRouterModelAdapter::new(config)?;
    // Keep the agent identity the same in both modes.
    let provider_config = OrchestratorProviderConfig {
        implementation: Implementation::new("ee-openrouter-agent", env!("CARGO_PKG_VERSION"))
            .title("OpenRouter"),
        ..OrchestratorProviderConfig::default()
    };
    let provider = OrchestratorProvider::with_policy(
        provider_config,
        Arc::new(adapter),
        openrouter_orchestrated_policy(),
    );
    AcpAgentServer::new(provider, server_config)
        .run_stdio()
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use ee_openrouter_agent::config::DEFAULT_API_URL;

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
        }
    }

    #[test]
    fn default_config_selects_orchestrator_provider_path() {
        let config = config(true);

        assert_eq!(provider_mode(&config), ProviderMode::Orchestrated);
    }

    #[test]
    fn opt_out_config_selects_simple_openrouter_provider_path() {
        let config = config(false);

        assert_eq!(provider_mode(&config), ProviderMode::Simple);
    }
}
