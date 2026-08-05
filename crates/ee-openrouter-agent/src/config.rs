//! CLI arguments and runtime configuration for the OpenRouter agent.
//!
//! Every knob keeps its historical `OPENROUTER_*` environment variable and
//! default.  Values that are neither CLI flags nor environment variables can
//! come from the local `.env` file via [`Config::from_args_and_dotenv`].

use std::collections::BTreeMap;
use std::time::Duration;

use clap::Parser;

/// Default OpenRouter chat-completions endpoint.
pub const DEFAULT_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// Default OpenRouter model id.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
/// Default system prompt sent with each session history.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an agent running inside ee editor. Answer concisely and help with software engineering tasks. Use available tools for workspace file reads; never print tool-call syntax as prose.";

/// Command-line arguments; every field also reads its `OPENROUTER_*`
/// environment variable.
#[derive(Debug, Parser)]
#[command(version, about = "ACP stdio bridge for OpenRouter chat completions")]
pub struct Args {
    /// OpenRouter model id, e.g. deepseek/deepseek-v4-flash-0731.
    #[arg(long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    model: String,
    /// Chat completions endpoint.
    #[arg(long, env = "OPENROUTER_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,
    /// Optional HTTP-Referer value recommended by OpenRouter.
    #[arg(long, env = "OPENROUTER_SITE_URL")]
    site_url: Option<String>,
    /// Optional X-Title value recommended by OpenRouter.
    #[arg(long, env = "OPENROUTER_APP_TITLE", default_value = "ee")]
    app_title: String,
    /// Request timeout in milliseconds.
    #[arg(long, env = "OPENROUTER_TIMEOUT_MS", default_value_t = 120_000)]
    timeout_ms: u64,
    /// Optional OpenRouter reasoning effort (`low`, `medium`, or `high`).
    #[arg(long, env = "OPENROUTER_REASONING_EFFORT")]
    reasoning_effort: Option<String>,
    /// System prompt sent with each session history.
    #[arg(long, env = "OPENROUTER_SYSTEM_PROMPT", default_value = DEFAULT_SYSTEM_PROMPT)]
    system_prompt: String,
    /// Run in orchestrated mode: ee-agent-orchestrator owns the model–tool
    /// loop and OpenRouter acts as the model adapter instead of the simple
    /// provider mode.  Off until orchestrated mode reaches parity.
    #[arg(long, env = "OPENROUTER_ORCHESTRATED", default_value_t = false)]
    orchestrated: bool,
}

/// Resolved agent configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// OpenRouter model id.
    pub model: String,
    /// Chat completions endpoint.
    pub api_url: String,
    /// OpenRouter API key; never logged, used only for the Authorization
    /// header.
    pub api_key: Option<String>,
    /// Optional HTTP-Referer value.
    pub site_url: Option<String>,
    /// X-Title value.
    pub app_title: String,
    /// Per-request HTTP timeout.
    pub timeout: Duration,
    /// System prompt sent with each session history.
    pub system_prompt: String,
    /// Optional reasoning effort passed as `reasoning.effort`.
    pub reasoning_effort: Option<String>,
    /// Whether to run through the orchestrator (model–tool loop owned by
    /// ee-agent-orchestrator) instead of the simple provider mode.
    pub orchestrated: bool,
}

impl Config {
    /// Resolves a configuration from parsed arguments plus `.env` fallbacks
    /// for the variables that are not CLI flags (`OPENROUTER_API_KEY`,
    /// `OPENROUTER_SITE_URL`).
    pub fn from_args_and_dotenv(args: Args, dotenv: &BTreeMap<String, String>) -> Self {
        Self {
            model: args.model,
            api_url: args.api_url,
            api_key: Self::env_or_dotenv("OPENROUTER_API_KEY", dotenv),
            site_url: args.site_url.or_else(|| Self::env_or_dotenv("OPENROUTER_SITE_URL", dotenv)),
            app_title: args.app_title,
            timeout: Duration::from_millis(args.timeout_ms),
            system_prompt: args.system_prompt,
            reasoning_effort: args.reasoning_effort,
            orchestrated: args.orchestrated,
        }
    }

    /// Looks a variable up in the process environment first, then in the
    /// parsed `.env` map; empty values count as unset.
    pub fn env_or_dotenv(name: &str, dotenv: &BTreeMap<String, String>) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| dotenv.get(name).cloned().filter(|value| !value.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            model: String::from("test/model"),
            api_url: String::from(DEFAULT_API_URL),
            site_url: None,
            app_title: String::from("ee-test"),
            timeout_ms: 1_000,
            reasoning_effort: None,
            system_prompt: String::from("system"),
            orchestrated: false,
        }
    }

    #[test]
    fn from_args_and_dotenv_keeps_all_args() {
        let mut parsed = args();
        parsed.timeout_ms = 42_000;
        let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new());

        assert_eq!(config.model, "test/model");
        assert_eq!(config.api_url, DEFAULT_API_URL);
        assert_eq!(config.app_title, "ee-test");
        assert_eq!(config.timeout, Duration::from_millis(42_000));
        assert_eq!(config.system_prompt, "system");
        assert!(config.reasoning_effort.is_none());
        assert!(!config.orchestrated, "defaults to simple provider mode");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn from_args_and_dotenv_falls_back_to_dotenv_values() {
        let dotenv = BTreeMap::from([
            (String::from("OPENROUTER_API_KEY"), String::from("sk-dotenv")),
            (String::from("OPENROUTER_SITE_URL"), String::from("https://dotenv.test")),
        ]);

        let config = Config::from_args_and_dotenv(args(), &dotenv);

        // The process environment may or may not export these variables, so
        // the assertion only holds when the environment leaves them unset.
        if std::env::var("OPENROUTER_API_KEY").ok().filter(|v| !v.is_empty()).is_none() {
            assert_eq!(config.api_key.as_deref(), Some("sk-dotenv"));
        }
        if std::env::var("OPENROUTER_SITE_URL").ok().filter(|v| !v.is_empty()).is_none() {
            assert_eq!(config.site_url.as_deref(), Some("https://dotenv.test"));
        }
    }

    #[test]
    fn dotenv_fallback_ignores_empty_values() {
        let dotenv = BTreeMap::from([(String::from("OPENROUTER_API_KEY"), String::new())]);
        let config = Config::from_args_and_dotenv(args(), &dotenv);
        assert!(config.api_key.is_none());
    }
}
