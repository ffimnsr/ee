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
        list_models: false,
        json: false,
        discover_models: false,
        surface: surface.map(String::from),
        model: model.map(String::from),
        critic_model: None,
        rubber_duck_mode: None,
        reasoning_effort: None,
        system_prompt: String::from(DEFAULT_SYSTEM_PROMPT),
        timeout_ms: Some(DEFAULT_TIMEOUT_MS),
        timeout_total_ms: None,
        timeout_headers_ms: None,
        timeout_chunk_ms: None,
        max_iterations: None,
        context_window: None,
        max_output_tokens: None,
        retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
        retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
        retry_max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
        checkpoint_dir: None,
        session_log_dir: None,
        session_log_durable: false,
        session_headers: true,
        compaction_mode: None,
        usage: false,
    }
}

fn dotenv(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

#[test]
fn explicit_args_resolve_the_route_and_keep_the_knobs() {
    let mut args = args(Some("go"), Some("kimi-k3"));
    args.timeout_ms = Some(5_000);
    args.max_iterations = Some(7);
    args.context_window = Some(32_768);
    args.retry_max_attempts = 4;
    args.system_prompt = String::from("custom");
    args.checkpoint_dir = Some(PathBuf::from("/tmp/ee-checkpoints"));

    let config = Config::from_args_and_dotenv(args, &dotenv(&[(API_KEY_ENV, "sk-secret")]))
        .expect("configured");

    assert_eq!(config.route.model_id, "kimi-k3");
    assert_eq!(config.route.surface, OpenCodeSurface::Go);
    assert_eq!(config.timeouts.total, Some(Duration::from_secs(5)));
    assert_eq!(config.max_iterations, Some(7));
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
    // No configured model resolves to the first active chat model on the
    // surface, deterministically.
    let default = crate::routes::chat_model_ids(OpenCodeSurface::Go)
        .first()
        .copied()
        .expect("the go surface has active chat models");
    let config = Config::from_args_and_dotenv(args(Some("go"), None), &BTreeMap::new())
        .expect("deterministic default model");
    assert_eq!(config.route.model_id, default);
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
    assert!(surface.required && model.required, "surface and model are mandatory inputs");
    // An installed agent must not pin a whole-turn timeout: the reference
    // defaults to unbounded total with phase budgets.
    let timeout = manifest.inputs.iter().find(|input| input.key == "timeout_ms").expect("timeout");
    assert!(timeout.default.is_none(), "a whole-turn timeout is opt-in, not a manifest default");
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
    text.extend(manifest.inputs.iter().flat_map(|input| {
        let mut lines = vec![input.key.clone(), input.label.clone(), input.config.env.clone()];
        for choice in &input.choices {
            lines.push(choice.value.clone());
            lines.push(choice.display().to_owned());
        }
        lines
    }));
    text
}

#[test]
fn setup_manifest_stays_under_the_host_byte_cap() {
    let encoded = serde_json::to_string(&setup_manifest()).expect("manifest serializes");

    assert!(
        encoded.len() < 16 * 1024,
        "setup manifest is {} bytes; the host cap is 16384",
        encoded.len()
    );
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

    let profile =
        route_profile(&route, "system", Timeouts::new(), RetryPolicy::none()).expect("profile");

    assert_eq!(profile.label, "OpenCode zen minimax-m3 (openai_chat_completions)");
    assert_eq!(profile.credential_var, API_KEY_ENV);
    assert_eq!(profile.endpoint.as_str(), "https://opencode.ai/zen/v1/chat/completions");
    assert_eq!(profile.model, "minimax-m3");
    assert_eq!(profile.system_prompt, "system");
    assert_eq!(profile.timeouts, Timeouts::new());
    assert_eq!(profile.retry, RetryPolicy::none());
    assert!(profile.extensions.is_none());
    assert_eq!(
        profile.image_opt_in_header.as_ref().map(reqwest::header::HeaderName::as_str),
        Some("typesafe-danger-zone-images"),
        "the OpenCode image lane needs its opt-in header, gated per request"
    );
}

#[test]
fn route_profile_advertises_store_only_where_the_catalog_does() {
    let responses = routes::resolve_route(OpenCodeSurface::Zen, "gpt-5.5").expect("routed");
    let chat = routes::resolve_route(OpenCodeSurface::Zen, "minimax-m3").expect("routed");

    let responses_profile =
        route_profile(&responses, "", Timeouts::new(), RetryPolicy::none()).expect("profile");
    let chat_profile =
        route_profile(&chat, "", Timeouts::new(), RetryPolicy::none()).expect("profile");

    assert!(responses_profile.store_responses, "Responses rows advertise server-side state");
    assert!(!chat_profile.store_responses, "a chat row never invites a continuation anchor");
    assert!(responses_profile.structured_output, "gpt-5.5 declares structured output");
    assert!(!chat_profile.structured_output, "minimax-m3 does not");
}

#[test]
fn every_catalog_route_has_a_trusted_profile() {
    let mut count = 0;
    for route in routes::catalog() {
        let profile = route_profile(&route, "", Timeouts::new(), RetryPolicy::none())
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
        (Some("zen"), Some("longcat-2.0")),
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
        "no model is configured and the selected surface has no enabled chat model: set \
         OPENCODE_MODEL to an exact documented model id"
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
         dialect, which ee-agent-opencode does not support yet: no request was attempted"
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
fn setup_manifest_offers_surface_scoped_model_selections() {
    let manifest = setup_manifest();

    let surface = manifest.inputs.iter().find(|input| input.key == "surface").expect("surface");
    assert_eq!(
        surface.choices.iter().map(|choice| choice.value.as_str()).collect::<Vec<_>>(),
        ["zen", "go"]
    );
    assert!(surface.choices[0].display().contains(routes::ZEN_API_ROOT));
    assert!(surface.choices[1].display().contains(routes::GO_API_ROOT));

    let model = manifest.inputs.iter().find(|input| input.key == "model").expect("model");
    assert!(model.remote_choices, "the host may search the full catalog on demand");
    let curated_ids = |surface: &str| {
        model
            .choices
            .iter()
            .filter(|choice| {
                choice
                    .when
                    .as_ref()
                    .is_some_and(|when| when.input == "surface" && when.value == surface)
            })
            .map(|choice| choice.value.as_str())
            .collect::<Vec<_>>()
    };

    for (surface, named) in [
        (OpenCodeSurface::Zen, ["deepseek-v4.1-flash", "glm-5.3-flash", "glm-5.3", "kimi-k3"]),
        (OpenCodeSurface::Go, ["deepseek-v4.1-flash", "glm-5.3-flash", "glm-5.3", "qwen3.8-flash"]),
    ] {
        let ids = curated_ids(surface.as_str());
        assert!(!ids.is_empty(), "{} offers curated model choices", surface.as_str());
        assert!(
            ids.len() <= 12,
            "{} keeps the default list short: {} ids",
            surface.as_str(),
            ids.len()
        );
        for id in &ids {
            assert!(
                routes::resolve_route(surface, id).is_ok(),
                "curated id `{id}` must resolve on {}",
                surface.as_str()
            );
        }
        for id in named {
            assert!(
                ids.contains(&id),
                "{} curated list keeps the requested `{id}`: {ids:?}",
                surface.as_str()
            );
        }
        let mut last = 0;
        for id in &ids {
            let cost = routes::resolve_route(surface, id)
                .expect("curated id routes")
                .cost()
                .map_or(0, |cost| cost.input_micros + 3 * cost.output_micros);
            assert!(cost >= last, "{surface:?} curated ids are ordered by usage allowance");
            last = cost;
        }
    }

    let critic = manifest.inputs.iter().find(|input| input.key == "critic_model").expect("critic");
    assert!(!critic.required, "the critic stays optional");
    assert!(critic.remote_choices, "the critic can search the full catalog too");
    assert_eq!(critic.choices.len(), model.choices.len(), "critic reuses the curated catalog");

    let approval_model = manifest
        .inputs
        .iter()
        .find(|input| input.key == "approval_classifier_model")
        .expect("approval classifier model");
    assert!(!approval_model.required, "the classifier model stays optional");
    assert!(!approval_model.remote_choices, "the classifier uses registered model slots");
    assert_eq!(
        approval_model.choices.iter().map(|choice| choice.value.as_str()).collect::<Vec<_>>(),
        ["default", "rubber_duck"]
    );

    let approval = manifest
        .inputs
        .iter()
        .find(|input| input.key == "approval_classifier")
        .expect("approval classifier");
    assert_eq!(approval.default.as_deref(), Some("off"));
    assert_eq!(
        approval.choices.iter().map(|choice| choice.value.as_str()).collect::<Vec<_>>(),
        ["off", "auto"]
    );

    let mode = manifest.inputs.iter().find(|input| input.key == "rubber_duck_mode").expect("mode");
    assert_eq!(
        mode.choices.iter().map(|choice| choice.value.as_str()).collect::<Vec<_>>(),
        ["off", "manual", "automatic"]
    );
    assert!(
        mode.advanced && !mode.required,
        "the built-in critic mode hides behind advanced setup so EE's critic is prompted first"
    );
    assert!(
        mode.label.contains("Built-in"),
        "the built-in critic is named separately from EE's rubber-duck critic: {}",
        mode.label
    );

    let timeout = manifest.inputs.iter().find(|input| input.key == "timeout_ms").expect("timeout");
    assert!(timeout.advanced, "low-level knobs stay behind the advanced gate");
}

#[test]
fn remote_choices_return_only_the_selected_surface() {
    for surface in [OpenCodeSurface::Zen, OpenCodeSurface::Go] {
        let model = remote_choices(surface, "model", None).expect("model accepts remote choices");
        let catalog = routes::chat_model_ids(surface);

        assert!(model.len() >= catalog.len(), "the full routable catalog is returned");
        assert!(
            model.iter().all(|choice| routes::resolve_route(surface, &choice.value).is_ok()),
            "every remote choice resolves on {}",
            surface.as_str()
        );
        assert!(
            model.iter().all(|choice| catalog.contains(&choice.value.as_str())),
            "remote choices stay on the requested surface"
        );
        let other = match surface {
            OpenCodeSurface::Zen => OpenCodeSurface::Go,
            OpenCodeSurface::Go => OpenCodeSurface::Zen,
        };
        let other_catalog = routes::chat_model_ids(other);
        assert!(
            !model.iter().any(|choice| {
                !catalog.contains(&choice.value.as_str())
                    && other_catalog.contains(&choice.value.as_str())
            }),
            "no {} id leaks into the {} list",
            other.as_str(),
            surface.as_str()
        );
        assert_eq!(
            remote_choices(surface, "critic_model", None).map(|choices| choices.len()),
            Some(model.len()),
            "the critic reuses the same catalog"
        );
        assert!(remote_choices(surface, "approval_classifier_model", None).is_none());
        assert!(remote_choices(surface, "surface", None).is_none());
    }
}

#[test]
fn setup_manifest_exposes_the_critic_mode_and_effort_knobs() {
    let manifest = setup_manifest();

    for (key, env) in [
        ("critic_model", CRITIC_MODEL_ENV),
        ("rubber_duck_mode", RUBBER_DUCK_MODE_ENV),
        ("approval_classifier", APPROVAL_CLASSIFIER_ENV),
        ("approval_classifier_model", APPROVAL_CLASSIFIER_MODEL_ENV),
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
    assert!(effort.choices.is_empty(), "effort choices follow the selected model");
    assert!(effort.remote_choices, "the host fetches choices for the selected model");
}

#[test]
fn reasoning_effort_choices_follow_the_selected_model() {
    let surface = OpenCodeSurface::Zen;
    let reasoning_model = routes::chat_model_ids(surface)
        .into_iter()
        .find(|model_id| {
            routes::resolve_route(surface, model_id).is_ok_and(|route| route.variants().len() > 1)
        })
        .expect("a reasoning model is in the zen catalog");
    let choices = remote_choices(surface, "reasoning_effort", Some(reasoning_model))
        .expect("effort accepts remote choices");
    let route = routes::resolve_route(surface, reasoning_model).expect("route resolves");
    let variants = route.variants();
    assert_eq!(
        choices.iter().map(|choice| choice.value.as_str()).collect::<Vec<_>>(),
        variants.iter().map(|variant| variant.id).collect::<Vec<_>>(),
        "the setup list equals the catalog variants"
    );
    assert!(!choices.is_empty(), "{reasoning_model} declares at least one selectable variant");

    let variant_suffixed = format!("{reasoning_model}#{}", variants[0].id);
    assert_eq!(
        remote_choices(surface, "reasoning_effort", Some(&variant_suffixed))
            .expect("a variant suffix still resolves")
            .len(),
        choices.len(),
        "the suffix selects a variant but does not change the list"
    );

    assert!(
        remote_choices(surface, "reasoning_effort", None).expect("missing model").is_empty(),
        "without a model there is nothing to scope to"
    );
    assert!(
        remote_choices(surface, "reasoning_effort", Some("not-a-model"))
            .expect("unknown model")
            .is_empty(),
        "unknown models offer no effort list"
    );
}

#[test]
fn args_parse_the_critic_mode_and_effort_selectors() {
    let parsed = Args::try_parse_from([
        "ee-agent-opencode",
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
    assert!(error.to_string().contains("expected \"none\", \"minimal\", \"low\""), "{error}");
    assert!(!error.to_string().contains("sk-"), "{error}");
}

#[test]
fn a_disabled_small_model_never_becomes_the_critic() {
    let file = crate::config_file::parse(&serde_json::json!({
        "small_model": "kimi-k3",
        "models": { "kimi-k3": { "disabled": true } }
    }))
    .expect("file parses");
    let config = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("gpt-5.5")),
        &BTreeMap::new(),
        file,
    )
    .expect("config resolves");
    assert!(config.critic_model.is_none(), "a disabled small model is skipped");
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

#[test]
fn variant_suffix_reaches_the_request_extensions() {
    let config = Config::from_args_and_dotenv(
        args(Some("zen"), Some("claude-opus-4-5#high")),
        &BTreeMap::new(),
    )
    .expect("variant route resolves");
    assert_eq!(
        config.route.variant.map(|variant| variant.id),
        Some("high"),
        "the catalog selects the budget variant"
    );

    let profile = config.profile().expect("profile builds");
    let thinking = &profile.extensions.as_ref().expect("extensions")["thinking"];
    assert_eq!(thinking["type"], "enabled");
    let budget = thinking["budget_tokens"].as_u64().expect("budget tokens");
    assert!(budget >= 1_024, "{budget} must clear the Anthropic floor");
    assert!(budget < config.route.limits().expect("limits").output);
}

#[test]
fn effort_shorthand_maps_to_the_matching_catalog_variant() {
    let mut with_effort = args(Some("zen"), Some("claude-opus-4-5"));
    with_effort.reasoning_effort = Some(String::from("max"));
    let config = Config::from_args_and_dotenv(with_effort, &BTreeMap::new()).expect("resolves");

    let profile = config.profile().expect("profile builds");
    let budget = profile.extensions.as_ref().expect("extensions")["thinking"]["budget_tokens"]
        .as_u64()
        .expect("budget tokens");
    assert_eq!(
        Some(budget),
        config.route.variants().iter().find(|v| v.id == "max").and_then(|v| v.budget_tokens)
    );
}

#[test]
fn effort_on_a_model_without_reasoning_support_fails_at_startup() {
    let mut with_effort = args(Some("zen"), Some("claude-3-5-haiku"));
    with_effort.reasoning_effort = Some(String::from("high"));

    let error = Config::from_args_and_dotenv(with_effort, &BTreeMap::new())
        .expect_err("a model without reasoning support rejects the variant");
    assert!(error.to_string().contains("no reasoning variant"), "{error}");
}

#[test]
fn catalog_limits_drive_context_and_output_bounds() {
    let config = Config::from_args_and_dotenv(args(Some("zen"), Some("gpt-5.5")), &BTreeMap::new())
        .expect("config resolves");
    let limits = config.route.limits().expect("catalog limits");
    assert_eq!(config.context_window, limits.context);

    let profile = config.profile().expect("profile builds");
    let fitted = profile.fitted_output_limit(1).expect("fitted limit");
    assert!(fitted > 8_192, "catalog output limit replaces the old constant: {fitted}");
    assert!(fitted <= limits.output);

    let mut overridden = args(Some("zen"), Some("gpt-5.5"));
    overridden.context_window = Some(32_768);
    overridden.max_output_tokens = Some(4_096);
    let config = Config::from_args_and_dotenv(overridden, &BTreeMap::new())
        .expect("explicit override validates");
    assert_eq!(config.context_window, 32_768);
    assert_eq!(config.max_output_tokens, Some(4_096));

    let mut too_large = args(Some("zen"), Some("gpt-5.5"));
    too_large.max_output_tokens = Some(limits.output + 1);
    let error = Config::from_args_and_dotenv(too_large, &BTreeMap::new())
        .expect_err("override above the catalog ceiling is rejected");
    assert!(matches!(error, ConfigError::OutputLimitExceeded { .. }), "{error:?}");
}

#[test]
fn legacy_routes_fall_back_to_the_configured_context_window() {
    let config =
        Config::from_args_and_dotenv(args(Some("go"), Some("qwen3.7-max")), &BTreeMap::new())
            .expect("legacy route resolves");
    assert!(config.route.limits().is_none());
    assert_eq!(config.context_window, DEFAULT_CONTEXT_WINDOW_TOKENS);
}

// ── Sampling and transport options ─────────────────────────────────

fn lookup(name: &str) -> Option<String> {
    match name {
        "OPENCODE_TEMPERATURE" => Some(String::from("0.3")),
        "OPENCODE_TOP_P" => Some(String::from("0.9")),
        "OPENCODE_TOP_K" => Some(String::from("32")),
        "OPENCODE_FREQUENCY_PENALTY" => Some(String::from("0.1")),
        "OPENCODE_PRESENCE_PENALTY" => Some(String::from("-0.1")),
        "OPENCODE_SEED" => Some(String::from("11")),
        "OPENCODE_STOP" => Some(String::from("END, STOP")),
        _ => None,
    }
}

#[test]
fn sampling_options_parse_from_the_environment() {
    let generation = super::options::generation_from(lookup).expect("valid values");
    assert_eq!(generation.temperature, Some(0.3));
    assert_eq!(generation.top_p, Some(0.9));
    assert_eq!(generation.top_k, Some(32));
    assert_eq!(generation.frequency_penalty, Some(0.1));
    assert_eq!(generation.presence_penalty, Some(-0.1));
    assert_eq!(generation.seed, Some(11));
    assert_eq!(generation.stop.as_deref(), Some(&[String::from("END"), String::from("STOP")][..]));
}

#[test]
fn an_unset_environment_produces_no_sampling_knobs() {
    let generation = super::options::generation_from(|_| None).expect("empty is valid");
    assert!(generation.is_empty());
}

#[test]
fn malformed_sampling_values_fail_at_startup() {
    let error = super::options::generation_from(|name| {
        (name == "OPENCODE_TEMPERATURE").then(|| String::from("hot"))
    })
    .expect_err("rejected");
    assert!(error.to_string().contains("OPENCODE_TEMPERATURE"), "{error}");

    let error = super::options::generation_from(|name| {
        (name == "OPENCODE_TEMPERATURE").then(|| String::from("9.5"))
    })
    .expect_err("out of range");
    assert!(error.to_string().contains("temperature"), "{error}");

    let error = super::options::generation_from(|name| {
        (name == "OPENCODE_STOP").then(|| String::from("A,,B"))
    })
    .expect_err("empty entry");
    assert!(error.to_string().contains("OPENCODE_STOP"), "{error}");
}

#[test]
fn a_model_without_a_knob_rejects_the_configuration() {
    let sampling =
        ee_agent_orchestrator::SamplingSupport { temperature: false, ..Default::default() };
    let generation =
        ee_agent_orchestrator::GenerationOptions { temperature: Some(0.4), ..Default::default() };
    let error = super::options::validate_generation("gpt-5.5", sampling, &generation)
        .expect_err("rejected");
    let text = error.to_string();
    assert!(text.contains("gpt-5.5"), "{text}");
    assert!(text.contains("temperature"), "{text}");
}

#[test]
fn phase_budgets_use_the_alias_and_the_reference_defaults() {
    let timeouts =
        super::options::timeouts_from(|_| None, Some(42_000), None, Some(1_500)).expect("valid");
    assert_eq!(timeouts.total, Some(Duration::from_millis(42_000)));
    assert_eq!(timeouts.headers, ee_chat_completions::Timeouts::DEFAULT_HEADERS);
    assert_eq!(timeouts.chunk, Duration::from_millis(1_500));
}

#[test]
fn config_file_disabled_model_fails_before_routing_by_flag() {
    let file = crate::config_file::parse(&serde_json::json!({
        "models": { "gpt-5.5": { "disabled": true } }
    }))
    .expect("file parses");
    let error = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("gpt-5.5")),
        &BTreeMap::new(),
        file,
    )
    .expect_err("disabled model is rejected");
    assert!(matches!(error, ConfigError::DisabledModel { .. }), "{error}");
    assert!(error.to_string().contains("disabled"), "{error}");
}

#[test]
fn config_file_default_model_selects_when_no_selector_is_given() {
    let file = crate::config_file::parse(&serde_json::json!({
        "default_surface": "zen",
        "default_model": "gpt-5.5"
    }))
    .expect("file parses");
    let config = Config::from_args_and_dotenv_with_file(args(None, None), &BTreeMap::new(), file)
        .expect("default model resolves");
    assert_eq!(config.route.model_id, "gpt-5.5");
    assert_eq!(config.route.surface, OpenCodeSurface::Zen);
}

#[test]
fn config_file_variant_fills_when_the_selector_has_none() {
    let file = crate::config_file::parse(&serde_json::json!({
        "models": { "gpt-5.5": { "variant": "high" } }
    }))
    .expect("file parses");
    let config = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("gpt-5.5")),
        &BTreeMap::new(),
        file,
    )
    .expect("variant resolves");
    assert_eq!(config.route.variant.map(|variant| variant.id), Some("high"));
}

#[test]
fn native_compaction_on_an_unsupported_route_fails_at_startup() {
    let error = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("claude-sonnet-5")),
        &dotenv(&[(COMPACTION_MODE_ENV, "native")]),
        crate::config_file::FileConfig::default(),
    )
    .expect_err("unsupported route is rejected");
    assert!(matches!(error, ConfigError::UnsupportedCompaction { .. }), "{error}");
    assert!(error.to_string().contains("claude-sonnet-5"), "{error}");
}

#[test]
fn auto_compaction_stays_available_on_a_supported_route() {
    let config = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("gpt-5.5")),
        &dotenv(&[(COMPACTION_MODE_ENV, "auto")]),
        crate::config_file::FileConfig::default(),
    )
    .expect("auto mode resolves");
    assert_eq!(config.compaction_mode, ee_agent_orchestrator::CompactionMode::Auto);
    assert!(config.profile().expect("profile").compaction);
}

#[test]
fn free_models_are_refused_when_the_switch_is_on() {
    let error = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("beam-free")),
        &dotenv(&[(DISABLE_FREE_MODELS_ENV, "1")]),
        crate::config_file::FileConfig::default(),
    )
    .expect_err("free model is refused");
    assert!(matches!(error, ConfigError::FreeModelDisabled { .. }), "{error}");
}

#[test]
fn config_file_headers_reach_the_profile_but_credentials_do_not() {
    let file = crate::config_file::parse(&serde_json::json!({
        "provider": { "headers": { "x-request-source": "ee" } }
    }))
    .expect("file parses");
    let config = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("gpt-5.5")),
        &BTreeMap::new(),
        file,
    )
    .expect("overlay resolves");
    let profile = config.profile().expect("profile");
    assert!(
        profile
            .headers
            .iter()
            .any(|(name, value)| name.as_str() == "x-request-source" && value == "ee")
    );

    let file = crate::config_file::parse(&serde_json::json!({
        "provider": { "headers": { "authorization": "Bearer override" } }
    }))
    .expect("file parses");
    let config = Config::from_args_and_dotenv_with_file(
        args(Some("zen"), Some("gpt-5.5")),
        &BTreeMap::new(),
        file,
    )
    .expect("config resolves");
    assert!(config.profile().is_err(), "credential headers cannot be overridden");
}

#[test]
fn catalog_rows_drive_the_cache_directive_per_dialect() {
    let key = dotenv(&[(API_KEY_ENV, "sk-secret")]);

    let messages = Config::from_args_and_dotenv(args(Some("zen"), Some("claude-sonnet-5")), &key)
        .expect("messages route");
    let directive = messages.cache_directive();
    assert!(directive.places_markers(), "explicit-cache row places markers");
    assert_eq!(directive.format, ee_chat_completions::CacheFormat::Anthropic);
    assert!(directive.support.explicit);
    assert_eq!(directive.support.max_breakpoints, 4);

    let responses =
        Config::from_args_and_dotenv(args(Some("zen"), Some("gpt-5.5")), &key).expect("responses");
    let directive = responses.cache_directive();
    assert!(!directive.places_markers(), "implicit-only row carries no markers");
    assert!(directive.support.implicit);
}
