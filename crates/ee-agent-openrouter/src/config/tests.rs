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
        ee_config: false,
        list_models: false,
        json: false,
        model: Some(String::from("test/model")),
        model_family: None,
        rubber_duck_model: None,
        rubber_duck_model_family: None,
        rubber_duck_mode: RubberDuckMode::Manual,
        rubber_duck_max_calls: DEFAULT_RUBBER_DUCK_MAX_CALLS,
        rubber_duck_context_bytes: DEFAULT_RUBBER_DUCK_CONTEXT_BYTES,
        rubber_duck_output_bytes: DEFAULT_RUBBER_DUCK_OUTPUT_BYTES,
        rubber_duck_timeout_ms: u64::try_from(DEFAULT_RUBBER_DUCK_TIMEOUT.as_millis()).unwrap(),
        api_url: Some(String::from(DEFAULT_API_URL)),
        api_kind: OpenRouterApiKind::Chat,
        responses_url: Some(String::from(DEFAULT_RESPONSES_URL)),
        site_url: None,
        app_title: String::from("ee-test"),
        timeout_ms: Some(1_000),
        timeout_total_ms: None,
        timeout_headers_ms: None,
        timeout_chunk_ms: None,
        reasoning_effort: None,
        system_prompt: String::from("system"),
        orchestrated: true,
        compact_min_messages: DEFAULT_COMPACT_MIN_MESSAGES,
        compact_retained_tail: DEFAULT_COMPACT_RETAINED_TAIL,
        compact_max_input_bytes: DEFAULT_COMPACT_MAX_INPUT_BYTES,
        context_window: None,
        auto_compact_threshold_percent: DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT,
        max_iterations: None,
        retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
        retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
        retry_max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
        checkpoint_dir: None,
        session_log_dir: None,
        session_log_durable: false,
        session_headers: true,
        compaction_mode: None,
    }
}

#[test]
fn catalog_context_window_fills_when_unset_and_an_explicit_value_wins() {
    let mut parsed = args();
    parsed.model = Some(String::from("deepseek/deepseek-v4-flash"));
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.context_window, 1_048_576, "catalog context replaces the default");

    let mut parsed = args();
    parsed.model = Some(String::from("deepseek/deepseek-v4-flash"));
    parsed.context_window = Some(32_768);
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.context_window, 32_768, "an explicit window wins");

    let config = Config::from_args_and_dotenv(args(), &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.context_window, DEFAULT_CONTEXT_WINDOW_TOKENS);
}

#[test]
fn model_id_variant_suffix_is_split_and_the_base_id_resolves() {
    let mut parsed = args();
    parsed.model = Some(String::from("deepseek/deepseek-v4-flash#high"));
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");

    assert_eq!(config.base_model(), "deepseek/deepseek-v4-flash");
    assert_eq!(config.variant_id(), Some("high"));
    assert_eq!(config.catalog_entry().map(|entry| entry.id), Some("deepseek/deepseek-v4-flash"));

    assert_eq!(config.base_model(), "deepseek/deepseek-v4-flash");
    let mut plain = args();
    plain.model = Some(String::from(" test/model "));
    let config = Config::from_args_and_dotenv(plain, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.base_model(), "test/model");
    assert_eq!(config.variant_id(), None);
    assert!(config.catalog_entry().is_none());
}

#[test]
fn an_empty_variant_selector_is_rejected() {
    let mut parsed = args();
    parsed.model = Some(String::from("deepseek/deepseek-v4-flash#"));
    let error =
        Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect_err("empty variant rejected");
    assert!(matches!(error, ConfigError::MissingVariant { .. }), "{error}");
}

#[test]
fn an_unconfigured_model_resolves_to_the_first_catalog_chat_model() {
    let ids = crate::catalog::chat_model_ids();
    let expected = ids.first().copied().expect("the catalog has chat models");
    let mut parsed = args();
    parsed.model = None;
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.base_model(), expected, "deterministic catalog default");
}

#[test]
fn a_disabled_small_model_never_becomes_the_critic() {
    let mut file = crate::config_file::FileConfig {
        small_model: Some(String::from("anthropic/claude-haiku-4.5")),
        ..crate::config_file::FileConfig::default()
    };
    file.models.insert(
        String::from("anthropic/claude-haiku-4.5"),
        crate::config_file::ModelEntry {
            disabled: true,
            ..crate::config_file::ModelEntry::default()
        },
    );
    let config = Config::from_args_and_dotenv_with_file(args(), &BTreeMap::new(), file)
        .expect("config resolves");
    assert!(config.rubber_duck_model.is_none(), "a disabled small model is skipped");
    assert!(config.rubber_duck.backend.is_none());
}

#[test]
fn api_kind_parses_the_documented_routes() {
    assert_eq!(OpenRouterApiKind::parse("chat"), Ok(OpenRouterApiKind::Chat));
    assert_eq!(OpenRouterApiKind::parse("RESPONSES"), Ok(OpenRouterApiKind::Responses));
    assert_eq!(OpenRouterApiKind::parse("chat-completions"), Ok(OpenRouterApiKind::Chat));
    assert!(OpenRouterApiKind::parse("grpc").is_err());
    assert_eq!(OpenRouterApiKind::Responses.as_str(), "responses");
}

#[test]
fn from_args_and_dotenv_keeps_all_args() {
    let mut parsed = args();
    parsed.timeout_ms = Some(42_000);
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");

    assert_eq!(config.model, "test/model");
    assert_eq!(config.rubber_duck, RubberDuckConfig::default());
    assert_eq!(config.api_url, DEFAULT_API_URL);
    assert_eq!(config.app_title, "ee-test");
    assert_eq!(config.timeouts.total, Some(Duration::from_millis(42_000)));
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
        Some(DEFAULT_CONTEXT_WINDOW_TOKENS * 80 / 100 - DEFAULT_CONTEXT_WINDOW_TOKENS / 10)
    );
    assert_eq!(config.max_iterations, None);
    assert_eq!(config.retry_max_attempts, DEFAULT_RETRY_MAX_ATTEMPTS);
    assert_eq!(config.retry_base_delay, Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS));
    assert_eq!(config.retry_max_delay, Duration::from_millis(DEFAULT_RETRY_MAX_DELAY_MS));
    assert!(config.checkpoint_dir.is_none());
    assert!(config.api_key.is_none());
}

#[test]
fn setup_manifest_advertises_required_values_and_defaults() {
    let manifest = setup_manifest();

    assert_eq!(manifest.schema_version, SETUP_MANIFEST_SCHEMA_VERSION);
    assert_eq!(manifest.agent.id, "openrouter");
    assert_eq!(manifest.agent.display_name, "OpenRouter");
    assert_eq!(manifest.env_vars.len(), 1);
    assert_eq!(manifest.env_vars[0].name, "OPENROUTER_API_KEY");
    assert!(manifest.env_vars[0].required);
    assert!(manifest.env_vars[0].secret);
    assert_eq!(manifest.inputs[0].key, "model");
    assert_eq!(manifest.inputs[0].default.as_deref(), Some(DEFAULT_MODEL));
    assert_eq!(manifest.inputs[0].config.env, "OPENROUTER_MODEL");
    assert_eq!(manifest.inputs.len(), 11);
    assert_eq!(manifest.inputs[1].key, "model_family");
    assert_eq!(manifest.inputs[1].config.env, "OPENROUTER_MODEL_FAMILY");
    assert_eq!(manifest.inputs[2].key, "rubber_duck_model");
    assert_eq!(manifest.inputs[2].config.env, "OPENROUTER_RUBBER_DUCK_MODEL");
    assert_eq!(manifest.inputs[3].key, "rubber_duck_model_family");
    assert_eq!(manifest.inputs[3].config.env, "OPENROUTER_RUBBER_DUCK_MODEL_FAMILY");
    assert_eq!(manifest.inputs[4].key, "rubber_duck_mode");
    assert_eq!(manifest.inputs[4].config.env, "OPENROUTER_RUBBER_DUCK_MODE");
    assert_eq!(manifest.inputs[9].key, "max_iterations");
    assert_eq!(manifest.inputs[9].default, None);
    assert_eq!(manifest.inputs[9].config.env, "OPENROUTER_MAX_ITERATIONS");
    assert_eq!(manifest.inputs[10].key, "session_log_dir");
    assert_eq!(manifest.inputs[10].config.env, "EE_SESSION_LOG_DIR");
}

#[test]
fn model_family_and_rubber_duck_cli_options_parse() {
    let parsed = Args::try_parse_from([
        "ee-agent-openrouter",
        "--model-family",
        "anthropic",
        "--rubber-duck-model",
        "openai/gpt-5",
        "--rubber-duck-model-family",
        "openai",
        "--rubber-duck-mode",
        "automatic",
        "--rubber-duck-max-calls",
        "3",
        "--rubber-duck-context-bytes",
        "4096",
        "--rubber-duck-output-bytes",
        "2048",
        "--rubber-duck-timeout-ms",
        "5000",
    ])
    .expect("model metadata parses");
    assert_eq!(parsed.model_family.as_deref(), Some("anthropic"));
    assert_eq!(parsed.rubber_duck_model.as_deref(), Some("openai/gpt-5"));
    assert_eq!(parsed.rubber_duck_model_family.as_deref(), Some("openai"));
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.rubber_duck.mode, RubberDuckMode::Automatic);
    assert_eq!(config.rubber_duck.max_calls, 3);
    assert_eq!(config.rubber_duck.max_context_bytes, 4096);
    assert_eq!(config.rubber_duck.max_output_bytes, 2048);
    assert_eq!(config.rubber_duck.timeout, Duration::from_secs(5));
    assert!(matches!(
        config.rubber_duck.backend,
        Some(RubberDuckBackend::InternalModel { ref model_id })
            if model_id == ee_agent_orchestrator::RUBBER_DUCK_ROLE
    ));
}

#[test]
fn ee_config_flag_parses() {
    let parsed = Args::try_parse_from(["ee-agent-openrouter", "--ee-config"])
        .expect("setup manifest flag parses");

    assert!(parsed.ee_config);
}

#[test]
fn cli_default_keeps_orchestrated_enabled() {
    let _env = EnvGuard::unset("OPENROUTER_ORCHESTRATED");
    let parsed = Args::try_parse_from(["ee-agent-openrouter"]).expect("default args parse");
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");

    assert!(config.orchestrated, "OpenRouter defaults to orchestrated mode");
}

#[test]
fn max_iterations_cli_and_env_override_share_one_limit() {
    let parsed = Args::try_parse_from(["ee-agent-openrouter", "--max-iterations", "64"])
        .expect("CLI iteration cap parses");
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.max_iterations, Some(64));

    let _env = EnvGuard::set("OPENROUTER_MAX_ITERATIONS", "32");
    let parsed =
        Args::try_parse_from(["ee-agent-openrouter"]).expect("environment iteration cap parses");
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");
    assert_eq!(config.max_iterations, Some(32));
}

#[test]
fn zero_max_iterations_is_rejected() {
    let error = Args::try_parse_from(["ee-agent-openrouter", "--max-iterations", "0"])
        .expect_err("zero iteration cap must fail");

    assert!(error.to_string().contains("must be at least 1"));
}

#[test]
fn explicit_cli_false_disables_orchestrated_mode() {
    let parsed = Args::try_parse_from(["ee-agent-openrouter", "--orchestrated=false"])
        .expect("explicit false parses");
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");

    assert!(!config.orchestrated, "explicit false remains fallback opt-out");
}

#[test]
fn explicit_env_false_disables_orchestrated_mode() {
    let _env = EnvGuard::set("OPENROUTER_ORCHESTRATED", "0");
    let parsed = Args::try_parse_from(["ee-agent-openrouter"])
        .expect("env false parses when OPENROUTER_ORCHESTRATED=0");
    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");

    assert!(!config.orchestrated, "OPENROUTER_ORCHESTRATED=0 disables fallback");
}

#[test]
fn from_args_and_dotenv_falls_back_to_dotenv_values() {
    let dotenv = BTreeMap::from([
        (String::from("OPENROUTER_API_KEY"), String::from("sk-dotenv")),
        (String::from("OPENROUTER_SITE_URL"), String::from("https://dotenv.test")),
    ]);

    let config = Config::from_args_and_dotenv(args(), &dotenv).expect("config resolves");

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
    let config = Config::from_args_and_dotenv(args(), &dotenv).expect("config resolves");
    assert!(config.api_key.is_none());
}

#[test]
fn config_file_default_model_and_small_model_apply_when_unset() {
    let file = crate::config_file::parse(&serde_json::json!({
        "default_model": "deepseek/deepseek-v4-flash",
        "small_model": "deepseek/deepseek-v4-flash"
    }))
    .expect("file parses");
    let mut plain = args();
    plain.model = None;
    let config = Config::from_args_and_dotenv_with_file(plain, &BTreeMap::new(), file)
        .expect("default model resolves");
    assert_eq!(config.model, "deepseek/deepseek-v4-flash");
    assert_eq!(config.rubber_duck_model.as_deref(), Some("deepseek/deepseek-v4-flash"));
}

#[test]
fn config_file_disabled_model_cannot_be_selected_by_flag() {
    let file = crate::config_file::parse(&serde_json::json!({
        "models": { "deepseek/deepseek-v4-flash": { "disabled": true } }
    }))
    .expect("file parses");
    let mut parsed = args();
    parsed.model = Some(String::from("deepseek/deepseek-v4-flash"));
    let error = Config::from_args_and_dotenv_with_file(parsed, &BTreeMap::new(), file)
        .expect_err("disabled model is rejected");
    assert!(matches!(error, ConfigError::DisabledModel { .. }), "{error}");
}

#[test]
fn native_compaction_is_refused_because_no_route_advertises_it() {
    let mut parsed = args();
    parsed.compaction_mode = Some(String::from("native"));
    let error = Config::from_args_and_dotenv_with_file(
        parsed,
        &BTreeMap::new(),
        crate::config_file::FileConfig::default(),
    )
    .expect_err("unsupported compaction is rejected");
    assert!(matches!(error, ConfigError::UnsupportedCompaction { .. }), "{error}");
    assert!(error.to_string().contains("chat-completions"), "{error}");
}

#[test]
fn config_file_body_overlay_reaches_the_profile() {
    let file = crate::config_file::parse(&serde_json::json!({
        "provider": { "body": { "provider": { "sort": "throughput" } } }
    }))
    .expect("file parses");
    let config = Config::from_args_and_dotenv_with_file(args(), &BTreeMap::new(), file)
        .expect("config resolves");
    let profile =
        crate::openrouter::openrouter_profile(&config, &config.model).expect("profile builds");
    let extensions = profile.extensions.expect("extensions");
    assert_eq!(extensions["provider"]["sort"], "throughput");
}
