//! CLI arguments and runtime configuration for the OpenRouter agent.
//!
//! Every knob keeps its historical `OPENROUTER_*` environment variable and
//! default.  Values that are neither CLI flags nor environment variables can
//! come from the local `.env` file via [`Config::from_args_and_dotenv`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, builder::BoolishValueParser};

/// Default OpenRouter chat-completions endpoint.
pub const DEFAULT_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// Default OpenRouter model id.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
/// Default system prompt sent with each session history.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an agent running inside ee editor. Answer concisely and help with software engineering tasks. Use available tools for workspace file reads; never print tool-call syntax as prose.";
/// Default minimum stored messages before `/compact` triggers a model summary.
pub const DEFAULT_COMPACT_MIN_MESSAGES: usize = 16;
/// Default stored messages kept verbatim at the tail after `/compact`.
pub const DEFAULT_COMPACT_RETAINED_TAIL: usize = 8;
/// Default maximum serialized bytes of history included in one compaction
/// request (system prompt and compaction prompt included).
pub const DEFAULT_COMPACT_MAX_INPUT_BYTES: usize = 64 * 1024;
/// Default model context-window size in tokens, used for the ACP
/// `usage_update` context-window denominator.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
/// Default percentage of the context window that triggers automatic session
/// compaction after OpenRouter reports a near-limit request.
pub const DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT: u8 = 80;
/// Default maximum transient/429 retries per model call.
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 2;
/// Default initial retry backoff: 500 ms.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 500;
/// Default retry backoff cap: 30 seconds.
pub const DEFAULT_RETRY_MAX_DELAY_MS: u64 = 30_000;

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let value = value.parse::<usize>().map_err(|_| String::from("must be a positive integer"))?;
    if value == 0 {
        return Err(String::from("must be at least 1"));
    }
    Ok(value)
}

fn parse_percentage(value: &str) -> Result<u8, String> {
    let value =
        value.parse::<u8>().map_err(|_| String::from("must be an integer from 0 through 100"))?;
    if value > 100 {
        return Err(String::from("must be an integer from 0 through 100"));
    }
    Ok(value)
}

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
    /// Minimum stored messages before `/compact` triggers a model summary.
    #[arg(long, env = "OPENROUTER_COMPACT_MIN_MESSAGES", default_value_t = DEFAULT_COMPACT_MIN_MESSAGES)]
    compact_min_messages: usize,
    /// Stored messages kept verbatim at the tail after `/compact`.
    #[arg(long, env = "OPENROUTER_COMPACT_RETAINED_TAIL", default_value_t = DEFAULT_COMPACT_RETAINED_TAIL)]
    compact_retained_tail: usize,
    /// Maximum serialized bytes of history included in one compaction request.
    #[arg(long, env = "OPENROUTER_COMPACT_MAX_INPUT_BYTES", default_value_t = DEFAULT_COMPACT_MAX_INPUT_BYTES)]
    compact_max_input_bytes: usize,
    /// Model context-window size in tokens (reported as `usage_update.size`).
    #[arg(long, env = "OPENROUTER_CONTEXT_WINDOW", default_value_t = DEFAULT_CONTEXT_WINDOW_TOKENS)]
    context_window: u64,
    /// Percentage of the context window that triggers automatic compaction after
    /// a reported near-limit request. Zero disables automatic compaction.
    #[arg(
        long,
        env = "OPENROUTER_AUTO_COMPACT_THRESHOLD_PERCENT",
        default_value_t = DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT,
        value_parser = parse_percentage,
    )]
    auto_compact_threshold_percent: u8,
    /// Maximum model–tool iterations and model calls per orchestrated turn.
    #[arg(
        long,
        env = "OPENROUTER_MAX_ITERATIONS",
        default_value_t = ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
        value_parser = parse_positive_usize,
    )]
    max_iterations: usize,
    /// Run in orchestrated mode: ee-agent-orchestrator owns the model–tool
    /// loop and OpenRouter acts as the model adapter instead of the simple
    /// provider mode. Default after parity; opt out for fallback diagnostics.
    #[arg(
        long,
        env = "OPENROUTER_ORCHESTRATED",
        default_value_t = true,
        action = ArgAction::Set,
        value_parser = BoolishValueParser::new(),
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
    )]
    orchestrated: bool,
    /// Maximum transient/429 retries per model call before failing the turn.
    #[arg(long, env = "OPENROUTER_RETRY_MAX_ATTEMPTS", default_value_t = DEFAULT_RETRY_MAX_ATTEMPTS)]
    retry_max_attempts: u32,
    /// Initial retry backoff in milliseconds (doubles per attempt, capped).
    #[arg(long, env = "OPENROUTER_RETRY_BASE_DELAY_MS", default_value_t = DEFAULT_RETRY_BASE_DELAY_MS)]
    retry_base_delay_ms: u64,
    /// Retry backoff cap in milliseconds (and cap for server Retry-After hints).
    #[arg(long, env = "OPENROUTER_RETRY_MAX_DELAY_MS", default_value_t = DEFAULT_RETRY_MAX_DELAY_MS)]
    retry_max_delay_ms: u64,
    /// Directory for durable turn checkpoints (recovery); unset keeps
    /// checkpoints in memory only.
    #[arg(long, env = "EE_CHECKPOINT_DIR")]
    checkpoint_dir: Option<PathBuf>,
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
    /// Minimum stored messages before `/compact` triggers a model summary.
    pub compact_min_messages: usize,
    /// Stored messages kept verbatim at the tail after `/compact`.
    pub compact_retained_tail: usize,
    /// Maximum serialized bytes of history included in one compaction
    /// request (system prompt and compaction prompt included).
    pub compact_max_input_bytes: usize,
    /// Model context-window size in tokens; the ACP `usage_update.size`.
    pub context_window: u64,
    /// Percentage of the context window that triggers automatic compaction after
    /// a reported near-limit request. Zero disables automatic compaction.
    pub auto_compact_threshold_percent: u8,
    /// Shared maximum for model–tool iterations and model calls per turn.
    pub max_iterations: usize,
    /// Maximum transient/429 retries per model call before failing the turn.
    pub retry_max_attempts: u32,
    /// Initial retry backoff (doubles per attempt, capped at
    /// `retry_max_delay_ms`).
    pub retry_base_delay: Duration,
    /// Retry backoff cap and cap for server Retry-After hints.
    pub retry_max_delay: Duration,
    /// Durable checkpoint directory for orchestrator recovery.
    pub checkpoint_dir: Option<PathBuf>,
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
            compact_min_messages: args.compact_min_messages,
            compact_retained_tail: args.compact_retained_tail,
            compact_max_input_bytes: args.compact_max_input_bytes,
            context_window: args.context_window,
            auto_compact_threshold_percent: args.auto_compact_threshold_percent,
            max_iterations: args.max_iterations,
            retry_max_attempts: args.retry_max_attempts,
            retry_base_delay: Duration::from_millis(args.retry_base_delay_ms),
            retry_max_delay: Duration::from_millis(args.retry_max_delay_ms),
            checkpoint_dir: args.checkpoint_dir,
        }
    }

    /// Returns the reported input-token threshold for automatic compaction.
    /// Zero-percent configuration disables the feature.
    #[must_use]
    pub fn auto_compact_threshold_tokens(&self) -> Option<u64> {
        (self.auto_compact_threshold_percent > 0).then(|| {
            self.context_window
                .saturating_mul(u64::from(self.auto_compact_threshold_percent))
                .saturating_div(100)
                .max(1)
        })
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
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let lock = ENV_LOCK.lock().expect("env lock poisoned");
            let previous = std::env::var(name).ok();
            // SAFETY: test holds a process-wide env mutex for this crate and
            // restores the value in Drop before releasing the lock.
            unsafe { std::env::set_var(name, value) };
            Self { _lock: lock, name, previous }
        }

        fn unset(name: &'static str) -> Self {
            let lock = ENV_LOCK.lock().expect("env lock poisoned");
            let previous = std::env::var(name).ok();
            // SAFETY: test holds a process-wide env mutex for this crate and
            // restores the value in Drop before releasing the lock.
            unsafe { std::env::remove_var(name) };
            Self { _lock: lock, name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: guard still holds the env mutex, and this restores the
            // exact prior state before any other guarded env mutation can run.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }

    fn args() -> Args {
        Args {
            model: String::from("test/model"),
            api_url: String::from(DEFAULT_API_URL),
            site_url: None,
            app_title: String::from("ee-test"),
            timeout_ms: 1_000,
            reasoning_effort: None,
            system_prompt: String::from("system"),
            orchestrated: true,
            compact_min_messages: DEFAULT_COMPACT_MIN_MESSAGES,
            compact_retained_tail: DEFAULT_COMPACT_RETAINED_TAIL,
            compact_max_input_bytes: DEFAULT_COMPACT_MAX_INPUT_BYTES,
            context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
            auto_compact_threshold_percent: DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT,
            max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
            retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            retry_max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
            checkpoint_dir: None,
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
        assert!(config.orchestrated, "defaults to orchestrated mode");
        assert_eq!(config.compact_min_messages, DEFAULT_COMPACT_MIN_MESSAGES);
        assert_eq!(config.compact_retained_tail, DEFAULT_COMPACT_RETAINED_TAIL);
        assert_eq!(config.compact_max_input_bytes, DEFAULT_COMPACT_MAX_INPUT_BYTES);
        assert_eq!(config.context_window, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(config.auto_compact_threshold_percent, DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
        assert_eq!(
            config.auto_compact_threshold_tokens(),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS * 80 / 100)
        );
        assert_eq!(
            config.max_iterations,
            ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS
        );
        assert_eq!(config.retry_max_attempts, DEFAULT_RETRY_MAX_ATTEMPTS);
        assert_eq!(config.retry_base_delay, Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS));
        assert_eq!(config.retry_max_delay, Duration::from_millis(DEFAULT_RETRY_MAX_DELAY_MS));
        assert!(config.checkpoint_dir.is_none());
        assert!(config.api_key.is_none());
    }

    #[test]
    fn cli_default_keeps_orchestrated_enabled() {
        let _env = EnvGuard::unset("OPENROUTER_ORCHESTRATED");
        let parsed = Args::try_parse_from(["ee-openrouter-agent"]).expect("default args parse");
        let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new());

        assert!(config.orchestrated, "OpenRouter defaults to orchestrated mode");
    }

    #[test]
    fn max_iterations_cli_and_env_override_share_one_limit() {
        let parsed = Args::try_parse_from(["ee-openrouter-agent", "--max-iterations", "64"])
            .expect("CLI iteration cap parses");
        let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new());
        assert_eq!(config.max_iterations, 64);

        let _env = EnvGuard::set("OPENROUTER_MAX_ITERATIONS", "32");
        let parsed = Args::try_parse_from(["ee-openrouter-agent"])
            .expect("environment iteration cap parses");
        let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new());
        assert_eq!(config.max_iterations, 32);
    }

    #[test]
    fn zero_max_iterations_is_rejected() {
        let error = Args::try_parse_from(["ee-openrouter-agent", "--max-iterations", "0"])
            .expect_err("zero iteration cap must fail");

        assert!(error.to_string().contains("must be at least 1"));
    }

    #[test]
    fn explicit_cli_false_disables_orchestrated_mode() {
        let parsed = Args::try_parse_from(["ee-openrouter-agent", "--orchestrated=false"])
            .expect("explicit false parses");
        let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new());

        assert!(!config.orchestrated, "explicit false remains fallback opt-out");
    }

    #[test]
    fn explicit_env_false_disables_orchestrated_mode() {
        let _env = EnvGuard::set("OPENROUTER_ORCHESTRATED", "0");
        let parsed = Args::try_parse_from(["ee-openrouter-agent"])
            .expect("env false parses when OPENROUTER_ORCHESTRATED=0");
        let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new());

        assert!(!config.orchestrated, "OPENROUTER_ORCHESTRATED=0 disables fallback");
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
