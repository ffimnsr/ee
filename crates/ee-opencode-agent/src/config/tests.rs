use super::*;

/// Lookup over explicit values, so tests never mutate the process
/// environment.
fn env_lookup(surface: Option<&str>, model: Option<&str>) -> impl Fn(&str) -> Option<String> {
    let surface = surface.map(String::from);
    let model = model.map(String::from);
    move |name: &str| match name {
        SURFACE_ENV => surface.clone(),
        MODEL_ENV => model.clone(),
        _ => None,
    }
}

/// Same lookup, but reads of the API key fail loudly: routing must never
/// need a credential.
fn route_lookup(surface: Option<&str>, model: Option<&str>) -> impl Fn(&str) -> Option<String> {
    let inner = env_lookup(surface, model);
    move |name: &str| {
        assert_ne!(name, API_KEY_ENV, "route resolution must not read {API_KEY_ENV}");
        inner(name)
    }
}

fn args(surface: Option<&str>, model: Option<&str>) -> Args {
    Args {
        ee_config: false,
        print_route: false,
        print_catalog: false,
        discover_models: false,
        surface: surface.map(String::from),
        model: model.map(String::from),
        critic_model: None,
        rubber_duck_mode: None,
        reasoning_effort: None,
        system_prompt: String::from(DEFAULT_SYSTEM_PROMPT),
        timeout_ms: DEFAULT_TIMEOUT_MS,
        max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
        context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
        retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
        retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
        retry_max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
        checkpoint_dir: None,
    }
}

fn dotenv(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

#[test]
fn explicit_args_resolve_the_route_and_keep_the_knobs() {
    let mut args = args(Some("go"), Some("kimi-k3"));
    args.timeout_ms = 5_000;
    args.max_iterations = 7;
    args.context_window = 32_768;
    args.retry_max_attempts = 4;
    args.system_prompt = String::from("custom");
    args.checkpoint_dir = Some(PathBuf::from("/tmp/ee-checkpoints"));

    let config = Config::from_args_and_dotenv(args, &dotenv(&[(API_KEY_ENV, "sk-secret")]))
        .expect("configured");

    assert_eq!(config.route.model_id, "kimi-k3");
    assert_eq!(config.route.surface, OpenCodeSurface::Go);
    assert_eq!(config.timeout, Duration::from_secs(5));
    assert_eq!(config.max_iterations, 7);
    assert_eq!(config.context_window, 32_768);
    assert_eq!(config.retry.max_attempts, 4);
    assert_eq!(config.system_prompt, "custom");
    assert_eq!(config.checkpoint_dir, Some(PathBuf::from("/tmp/ee-checkpoints")));
    assert!(config.has_api_key());
}

#[test]
fn dotenv_supplies_selectors_and_key_when_args_do_not() {
    let config = Config::from_args_and_dotenv(
        args(None, None),
        &dotenv(&[(SURFACE_ENV, " zen "), (MODEL_ENV, "gpt-5.5"), (API_KEY_ENV, "sk-from-file")]),
    )
    .expect("configured from dotenv");

    assert_eq!(config.route.surface, OpenCodeSurface::Zen);
    assert_eq!(config.route.model_id, "gpt-5.5");
    assert_eq!(config.api_key.as_deref(), Some("sk-from-file"));
}

#[test]
fn args_win_over_dotenv_and_empty_values_count_as_unset() {
    let config = Config::from_args_and_dotenv(
        args(Some("go"), Some("kimi-k3")),
        &dotenv(&[(SURFACE_ENV, "zen"), (MODEL_ENV, "gpt-5.5"), (API_KEY_ENV, "")]),
    )
    .expect("configured");

    assert_eq!(config.route.surface, OpenCodeSurface::Go);
    assert_eq!(config.route.model_id, "kimi-k3");
    assert_eq!(config.api_key, None, "an empty key is unset, never a blank credential");
    assert!(!config.has_api_key());
}

#[test]
fn missing_and_unknown_selectors_fail_before_any_request() {
    assert_eq!(
        Config::from_args_and_dotenv(args(None, Some("kimi-k3")), &BTreeMap::new())
            .expect_err("missing surface"),
        ConfigError::MissingSurface
    );
    assert_eq!(
        Config::from_args_and_dotenv(args(Some("go"), None), &BTreeMap::new())
            .expect_err("missing model"),
        ConfigError::MissingModel
    );
    assert_eq!(
        Config::from_args_and_dotenv(
            args(Some("zen"), Some("kimi-k4")),
            &dotenv(&[(API_KEY_ENV, "sk-secret")]),
        )
        .expect_err("unknown model"),
        ConfigError::UnknownModel {
            surface: OpenCodeSurface::Zen,
            model_id: String::from("kimi-k4"),
        }
    );
}

#[test]
fn config_debug_redacts_the_api_key() {
    let config = Config::from_args_and_dotenv(
        args(Some("zen"), Some("gpt-5.5")),
        &dotenv(&[(API_KEY_ENV, "sk-secret-value")]),
    )
    .expect("configured");

    let debug = format!("{config:?}");

    assert!(debug.contains("[redacted]"), "{debug}");
    assert!(!debug.contains("sk-secret-value"), "{debug}");
    assert!(!debug.contains("Bearer"), "{debug}");
}

#[test]
fn setup_manifest_requires_explicit_selectors_and_a_secret_key() {
    let manifest = setup_manifest();

    assert_eq!(manifest.agent.id, "opencode");
    assert_eq!(manifest.agent.display_name, "OpenCode");
    let key = manifest.env_vars.iter().find(|var| var.name == API_KEY_ENV).expect("key var");
    assert!(key.required && key.secret, "the API key must be required and secret");
    let surface = manifest.inputs.iter().find(|input| input.key == "surface").expect("surface");
    let model = manifest.inputs.iter().find(|input| input.key == "model").expect("model");
    assert_eq!(surface.config.env, SURFACE_ENV);
    assert_eq!(model.config.env, MODEL_ENV);
    assert!(surface.default.is_none(), "no surface is chosen by default");
    assert!(model.default.is_none(), "no model is chosen by default");
    let serialized = serde_json::to_string(&manifest).expect("manifest serializes");
    assert!(!serialized.contains("sk-"), "the manifest never carries a key");
}

/// Every string the setup path can print for this agent.
fn manifest_text() -> Vec<String> {
    let manifest = setup_manifest();
    let mut text = vec![manifest.agent.id.clone(), manifest.agent.display_name.clone()];
    text.extend(
        manifest.env_vars.iter().flat_map(|env| vec![env.name.clone(), env.description.clone()]),
    );
    text.extend(
        manifest.inputs.iter().flat_map(|input| {
            vec![input.key.clone(), input.label.clone(), input.config.env.clone()]
        }),
    );
    text
}

#[test]
fn setup_manifest_describes_surfaces_with_their_distinct_endpoint_roots() {
    let text = manifest_text().join("\n");

    assert!(text.contains(routes::ZEN_API_ROOT), "zen root is shown to the user: {text}");
    assert!(text.contains(routes::GO_API_ROOT), "go root is shown to the user: {text}");
    assert!(text.contains("zen") && text.contains("go"));
    assert!(
        text.contains("surface"),
        "the surface choice is named as its own input, not implied by the provider"
    );
}

#[test]
fn setup_manifest_keeps_billing_and_account_claims_provider_owned() {
    let text = manifest_text();
    let claims = ["billing", "balances", "usage limits", "account management"];

    for claim in claims {
        let mentions = text.iter().filter(|line| line.to_ascii_lowercase().contains(claim));
        for line in mentions {
            assert!(
                line.contains("OpenCode"),
                "`{claim}` must be attributed to OpenCode, never claimed by ee: {line}"
            );
            assert!(
                line.contains("stay with OpenCode"),
                "`{claim}` must state that the provider owns it: {line}"
            );
        }
    }
}

#[test]
fn setup_manifest_asks_for_bare_model_ids_not_tui_aliases() {
    let text = manifest_text().join("\n");

    assert!(text.contains("not an OpenCode TUI alias"), "{text}");
    assert!(
        text.contains("kimi-k3") && text.contains("gpt-5.5"),
        "both surfaces get a documented representative id: {text}"
    );
    for (surface, example) in [(OpenCodeSurface::Zen, "gpt-5.5"), (OpenCodeSurface::Go, "kimi-k3")]
    {
        assert!(
            routes::resolve_route(surface, example).is_ok(),
            "documented example `{example}` must resolve on OpenCode {}",
            surface.as_str()
        );
    }
}

#[test]
fn route_profile_uses_the_catalog_endpoint_and_names_the_route() {
    let route = routes::resolve_route(OpenCodeSurface::Zen, "minimax-m3").expect("routed");

    let profile = route_profile(&route, "system", Duration::from_secs(5), RetryPolicy::none())
        .expect("profile");

    assert_eq!(profile.label, "OpenCode zen minimax-m3 (openai_chat_completions)");
    assert_eq!(profile.credential_var, API_KEY_ENV);
    assert_eq!(profile.endpoint.as_str(), "https://opencode.ai/zen/v1/chat/completions");
    assert_eq!(profile.model, "minimax-m3");
    assert_eq!(profile.system_prompt, "system");
    assert_eq!(profile.timeout, Duration::from_secs(5));
    assert_eq!(profile.retry, RetryPolicy::none());
    assert!(profile.extensions.is_none());
}

#[test]
fn every_catalog_route_has_a_trusted_profile() {
    let mut count = 0;
    for route in routes::catalog() {
        let profile = route_profile(&route, "", Duration::from_secs(1), RetryPolicy::none())
            .unwrap_or_else(|error| {
                panic!("{} {}: {error}", route.surface.as_str(), route.model_id)
            });
        assert!(profile.endpoint.as_str().starts_with("https://opencode.ai/zen/"));
        assert_eq!(profile.model, route.model_id);
        count += 1;
    }
    assert!(count > 90, "the catalog shrank unexpectedly: {count} routes");
}

#[test]
fn token_source_reports_the_missing_key_without_exposing_a_key() {
    let missing = Config::from_args_and_dotenv(args(Some("go"), Some("kimi-k3")), &BTreeMap::new())
        .expect("configured");
    assert_eq!(missing.token_source()().expect_err("no key"), MISSING_API_KEY);

    let present = Config::from_args_and_dotenv(
        args(Some("go"), Some("kimi-k3")),
        &dotenv(&[(API_KEY_ENV, "sk-secret")]),
    )
    .expect("configured");
    let token = present.token_source()().expect("token");
    assert_eq!(token.expose(), "sk-secret");
    assert!(!format!("{token:?}").contains("sk-secret"));
}

#[test]
fn zero_valued_knobs_are_rejected_by_the_flag_parsers() {
    assert!(parse_positive_u64("0").is_err());
    assert!(parse_positive_usize("0").is_err());
    assert!(parse_positive_u64("abc").is_err());
    assert_eq!(parse_positive_u64("1500").expect("parses"), 1500);
    assert_eq!(parse_positive_usize("3").expect("parses"), 3);
}

#[test]
fn explicit_surface_and_model_resolve_exact_route() {
    let route =
        resolve_route_from(route_lookup(Some("go"), Some("kimi-k3"))).expect("documented go route");

    assert_eq!(route.surface, OpenCodeSurface::Go);
    assert_eq!(route.model_id, "kimi-k3");
    assert_eq!(route.endpoint, "https://opencode.ai/zen/go/v1/chat/completions");
}

#[test]
fn surrounding_whitespace_is_tolerated_and_trimmed() {
    let route = resolve_route_from(route_lookup(Some(" zen "), Some(" gpt-5.5 ")))
        .expect("trimmed selectors resolve");

    assert_eq!(route.surface, OpenCodeSurface::Zen);
    assert_eq!(route.model_id, "gpt-5.5");
}

#[test]
fn missing_or_blank_selectors_fail_closed() {
    assert_eq!(
        resolve_route_from(route_lookup(None, Some("gpt-5.5"))).unwrap_err(),
        ConfigError::MissingSurface
    );
    assert_eq!(
        resolve_route_from(route_lookup(Some("go"), None)).unwrap_err(),
        ConfigError::MissingModel
    );
    assert_eq!(
        resolve_route_from(route_lookup(Some("   "), Some("gpt-5.5"))).unwrap_err(),
        ConfigError::MissingSurface
    );
    assert_eq!(
        resolve_route_from(route_lookup(Some("go"), Some(""))).unwrap_err(),
        ConfigError::MissingModel
    );
}

#[test]
fn malformed_surface_reports_the_rejected_value() {
    assert_eq!(
        resolve_route_from(route_lookup(Some("ZEN"), Some("gpt-5.5"))).unwrap_err(),
        ConfigError::UnsupportedSurface { value: String::from("ZEN") }
    );
}

#[test]
fn route_resolution_never_reads_the_api_key() {
    let route = resolve_route_from(route_lookup(Some("zen"), Some("gpt-5.5")))
        .expect("documented zen route");
    assert_eq!(route.endpoint, "https://opencode.ai/zen/v1/responses");

    for (surface, model) in [
        (Some("zen"), Some("gpt-5-codex")),
        (Some("zen"), Some("gemini-3.8-flash")),
        (Some("zen"), Some("qwen3.8-max")),
        (Some("zen"), Some("opencode/gpt-5.5")),
        (Some("zen"), Some("codex-mini-latest")),
        (Some("go"), Some("kimi-k3.5")),
        (None, Some("gpt-5.5")),
        (Some("go"), None),
    ] {
        assert!(resolve_route_from(route_lookup(surface, model)).is_err());
    }
}

#[test]
fn config_error_messages_are_deterministic() {
    assert_eq!(
        ConfigError::MissingSurface.to_string(),
        "OPENCODE_SURFACE is required and must be \"zen\" or \"go\": no OpenCode surface is \
         selected by default"
    );
    assert_eq!(
        ConfigError::MissingModel.to_string(),
        "OPENCODE_MODEL is required: set an exact documented model id for the selected surface"
    );
    assert_eq!(
        ConfigError::UnsupportedSurface { value: String::from("opencode") }.to_string(),
        "unsupported OpenCode surface \"opencode\": expected \"zen\" or \"go\""
    );
    assert_eq!(
        ConfigError::UnknownModel {
            surface: OpenCodeSurface::Zen,
            model_id: String::from("codex-mini-latest"),
        }
        .to_string(),
        "model \"codex-mini-latest\" is not a documented OpenCode zen model: pass an exact \
         model id from the current OpenCode zen model list"
    );
    assert_eq!(
        ConfigError::WrongSurface {
            surface: OpenCodeSurface::Zen,
            model_id: String::from("qwen3.8-max"),
            documented_surface: OpenCodeSurface::Go,
        }
        .to_string(),
        "model \"qwen3.8-max\" is documented for OpenCode go, not zen: set OPENCODE_SURFACE=go \
         or pick a zen model id"
    );
    assert_eq!(
        ConfigError::RetiredModel {
            surface: OpenCodeSurface::Zen,
            model_id: String::from("gpt-5-codex"),
            deprecated_on: "2026-07-23",
        }
        .to_string(),
        "model \"gpt-5-codex\" was deprecated on OpenCode zen on 2026-07-23 and is not routed: \
         pick a current model id"
    );
    assert_eq!(
        ConfigError::UnsupportedModelDialect {
            surface: OpenCodeSurface::Zen,
            model_id: String::from("gemini-3.8-flash"),
            dialect: routes::UnsupportedDialect::GoogleGenerativeLanguage,
        }
        .to_string(),
        "model \"gemini-3.8-flash\" on OpenCode zen uses the Google Generative Language \
         dialect, which ee-opencode-agent does not support yet: no request was attempted"
    );
    assert_eq!(
        ConfigError::TuiAliasModel {
            model_id: String::from("opencode-go/kimi-k3"),
            bare_model_id: String::from("kimi-k3"),
        }
        .to_string(),
        "model id \"opencode-go/kimi-k3\" is an OpenCode TUI alias: pass the documented model \
         id \"kimi-k3\" instead"
    );
    assert_eq!(
        ConfigError::UntrustedRouteEndpoint {
            endpoint: "http://gateway.example/v1",
            detail: String::from("plain http"),
        }
        .to_string(),
        "route endpoint \"http://gateway.example/v1\" is not a credential-safe origin: plain http"
    );
}

#[test]
fn setup_manifest_exposes_the_critic_mode_and_effort_knobs() {
    let manifest = setup_manifest();

    for (key, env) in [
        ("critic_model", CRITIC_MODEL_ENV),
        ("rubber_duck_mode", RUBBER_DUCK_MODE_ENV),
        ("reasoning_effort", REASONING_EFFORT_ENV),
    ] {
        let input = manifest
            .inputs
            .iter()
            .find(|input| input.key == key)
            .unwrap_or_else(|| panic!("manifest declares `{key}`"));
        assert_eq!(input.config.env, env, "{key} maps to {env}");
        assert!(!input.label.trim().is_empty(), "{key} has a label");
    }

    let critic = manifest.inputs.iter().find(|input| input.key == "critic_model").expect("critic");
    assert!(critic.default.is_none(), "the critic stays optional and explicit");
    assert!(critic.label.contains("same surface"), "{}", critic.label);
    assert!(critic.label.contains("different vendor"), "{}", critic.label);

    let mode = manifest.inputs.iter().find(|input| input.key == "rubber_duck_mode").expect("mode");
    assert_eq!(mode.default.as_deref(), Some("manual"), "manual is the documented default");

    let effort =
        manifest.inputs.iter().find(|input| input.key == "reasoning_effort").expect("effort");
    assert!(effort.default.is_none(), "no effort is sent unless it is configured explicitly");
}

#[test]
fn args_parse_the_critic_mode_and_effort_selectors() {
    let parsed = Args::try_parse_from([
        "ee-opencode-agent",
        "--surface",
        "zen",
        "--model",
        "gpt-5.5",
        "--critic-model",
        "kimi-k3",
        "--rubber-duck-mode",
        "AUTOMATIC",
        "--reasoning-effort",
        "High",
    ])
    .expect("arguments parse");

    let config = Config::from_args_and_dotenv(parsed, &BTreeMap::new()).expect("config resolves");

    assert_eq!(config.critic_model.as_deref(), Some("kimi-k3"));
    assert_eq!(config.rubber_duck.mode, RubberDuckMode::Automatic);
    assert_eq!(config.reasoning_effort, Some(ReasoningEffort::High));
}

#[test]
fn unsupported_mode_and_effort_values_fail_before_any_request() {
    let mut with_mode = args(Some("zen"), Some("gpt-5.5"));
    with_mode.rubber_duck_mode = Some(String::from("sometimes"));
    let error = Config::from_args_and_dotenv(with_mode, &BTreeMap::new())
        .expect_err("unsupported mode is rejected at startup");
    assert!(error.to_string().contains("accepts \"off\", \"manual\", or \"automatic\""), "{error}");

    let mut with_effort = args(Some("zen"), Some("gpt-5.5"));
    with_effort.reasoning_effort = Some(String::from("ultra"));
    let error = Config::from_args_and_dotenv(with_effort, &BTreeMap::new())
        .expect_err("unsupported effort is rejected at startup");
    assert!(error.to_string().contains("expected \"low\", \"medium\", or \"high\""), "{error}");
    assert!(!error.to_string().contains("sk-"), "{error}");
}

#[test]
fn blank_critic_and_effort_values_count_as_unset() {
    let config = Config::from_args_and_dotenv(
        args(Some("zen"), Some("gpt-5.5")),
        &dotenv(&[(CRITIC_MODEL_ENV, "   "), (REASONING_EFFORT_ENV, "  ")]),
    )
    .expect("config resolves");

    assert!(config.critic_model.is_none(), "an empty critic is not a critic");
    assert!(config.reasoning_effort.is_none(), "an empty effort sends nothing");
    assert_eq!(config.reasoning_effort_note(), None);
}
