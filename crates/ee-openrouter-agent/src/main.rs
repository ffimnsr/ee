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
use ee_openrouter_agent::orchestrated::OpenRouterModelAdapter;
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
    let result = if config.orchestrated {
        run_orchestrated(config, server_config).await
    } else {
        run_simple(config, server_config).await
    };
    if let Err(error) = result {
        eprintln!("ee-openrouter-agent: {error}");
        std::process::exit(1);
    }
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
    let provider = OrchestratorProvider::new(provider_config, Arc::new(adapter));
    AcpAgentServer::new(provider, server_config)
        .run_stdio()
        .await
        .map_err(|error| error.to_string())
}
