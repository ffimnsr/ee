use super::*;
use crate::model::ModelRole;

fn image(data: &str, mime: &str) -> ModelContent {
    ModelContent::Image { data: data.into(), mime_type: mime.into(), uri: None }
}

#[test]
fn catalog_cost_prices_uncached_output_and_cache_reads() {
    let cost = ModelCost {
        input_micros: 3_000_000,
        output_micros: 15_000_000,
        cache_read_micros: Some(300_000),
        cache_write_micros: Some(3_750_000),
        tiers: &[],
    };
    let tokens =
        TokenCounts { input: 10_000, output: 1_000, cached_input: 4_000, cache_write: 2_000 };
    // 4k uncached * $3/M + 4k cached * $0.3/M + 2k write * $3.75/M + 1k * $15/M
    assert_eq!(cost.cost_micros(tokens), 12_000 + 1_200 + 7_500 + 15_000);
}

#[test]
fn catalog_cost_prefers_the_matching_context_tier() {
    let cost = ModelCost {
        input_micros: 3_000_000,
        output_micros: 15_000_000,
        cache_read_micros: None,
        cache_write_micros: None,
        tiers: &[CostTier {
            input_context_above: 200_000,
            input_micros: 6_000_000,
            output_micros: 22_500_000,
            cache_read_micros: Some(600_000),
            cache_write_micros: None,
        }],
    };
    let below =
        cost.cost_micros(TokenCounts { input: 100_000, output: 1_000, ..TokenCounts::default() });
    let above =
        cost.cost_micros(TokenCounts { input: 300_000, output: 1_000, ..TokenCounts::default() });
    assert_eq!(below, 300_000 + 15_000);
    assert_eq!(above, 1_800_000 + 22_500);
}

#[test]
fn zero_price_is_a_known_price_not_unknown() {
    let cost = ModelCost {
        input_micros: 0,
        output_micros: 0,
        cache_read_micros: None,
        cache_write_micros: None,
        tiers: &[],
    };
    assert_eq!(
        cost.cost_micros(TokenCounts { input: 1_000, output: 1_000, ..TokenCounts::default() }),
        0
    );
}

#[test]
fn catalog_cost_fills_usage_only_when_the_provider_did_not_report_one() {
    let cost = Some(ModelCost {
        input_micros: 1_000_000,
        output_micros: 1_000_000,
        cache_read_micros: None,
        cache_write_micros: None,
        tiers: &[],
    });
    let mut response = ModelResponse::new();
    response.usage = ModelUsage::new().with_input_tokens(100).with_output_tokens(50);

    apply_catalog_cost(&mut response, cost);
    assert_eq!(response.usage.cost_micros, Some(150));

    response.usage.cost_micros = Some(7);
    apply_catalog_cost(&mut response, cost);
    assert_eq!(response.usage.cost_micros, Some(7), "provider-reported cost wins");
}

#[test]
fn unknown_prices_stay_unknown() {
    let mut response = ModelResponse::new();
    response.usage = ModelUsage::new().with_input_tokens(100);
    apply_catalog_cost(&mut response, None);
    assert_eq!(response.usage.cost_micros, None);
}

#[test]
fn chat_variants_use_declared_efforts_and_disable_through_none() {
    let support = ReasoningSupport {
        supported: true,
        toggle: true,
        efforts: &["none", "high", "max"],
        budget_min: None,
        budget_max: None,
    };
    let variants = reasoning_variants(support, ReasoningProtocol::OpenAiChat, None);
    let ids: Vec<_> = variants.iter().map(|variant| variant.id).collect();
    assert_eq!(ids, ["none", "high", "max"]);
    assert_eq!(find_variant(&variants, "high").expect("high").effort, Some("high"));
}

#[test]
fn chat_variants_fall_back_to_protocol_defaults_when_values_are_absent() {
    let support = ReasoningSupport {
        supported: true,
        toggle: false,
        efforts: &[],
        budget_min: None,
        budget_max: None,
    };
    let variants = reasoning_variants(support, ReasoningProtocol::OpenAiChat, None);
    assert_eq!(variant_ids(&variants), "low, medium, high");
}

#[test]
fn messages_variants_are_budgets_bounded_by_the_anthropic_ceiling() {
    let support = ReasoningSupport {
        supported: true,
        toggle: false,
        efforts: &[],
        budget_min: Some(1_024),
        budget_max: None,
    };
    let variants = reasoning_variants(support, ReasoningProtocol::AnthropicMessages, Some(64_000));
    assert_eq!(variant_ids(&variants), "high, max");
    assert_eq!(find_variant(&variants, "max").expect("max").budget_tokens, Some(31_999));
    assert_eq!(find_variant(&variants, "high").expect("high").budget_tokens, Some(16_000));
}

#[test]
fn toggle_only_messages_models_get_a_manual_thinking_budget() {
    let support = ReasoningSupport {
        supported: true,
        toggle: true,
        efforts: &["low", "high"],
        budget_min: None,
        budget_max: None,
    };
    let variants = reasoning_variants(support, ReasoningProtocol::AnthropicMessages, None);
    assert_eq!(variant_ids(&variants), "none, thinking");
    assert!(find_variant(&variants, "none").expect("none").disabled);
    assert_eq!(find_variant(&variants, "thinking").expect("thinking").budget_tokens, Some(16_000));
}

#[test]
fn models_without_reasoning_support_expose_no_variants() {
    let variants =
        reasoning_variants(ReasoningSupport::UNSUPPORTED, ReasoningProtocol::OpenAiChat, None);
    assert!(variants.is_empty());
    assert_eq!(variant_ids(&variants), "none");
}

#[test]
fn modalities_accept_exact_types_and_whole_families() {
    let declared =
        Modalities { input: &["text", "image/png", "application/pdf"], output: &["text"] };
    assert!(declared.accepts("text"));
    assert!(declared.accepts("image/png"));
    assert!(!declared.accepts("image/webp"));
    assert!(declared.accepts("application/pdf"));

    let family = Modalities { input: &["text", "image"], output: &["text"] };
    assert!(family.accepts("image/webp"));
    assert!(Modalities::UNKNOWN.accepts("video/mp4"));
}

#[test]
fn undeclared_modalities_become_a_named_placeholder() {
    let modalities = Modalities { input: &["text", "image/png"], output: &["text"] };
    let transcript =
        vec![ModelMessage::new(ModelRole::User).with_content(vec![image("AAAA", "image/webp")])];
    let sanitized = sanitize_modalities(modalities, "vendor/model", &transcript).expect("replaced");
    let ModelContent::Text(placeholder) = &sanitized[0].content[0] else {
        panic!("placeholder expected");
    };
    assert!(placeholder.contains("image/webp"), "{placeholder}");
    assert!(placeholder.contains("3-byte"), "{placeholder}");
    assert!(placeholder.contains("vendor/model"), "{placeholder}");

    let accepted =
        vec![ModelMessage::new(ModelRole::User).with_content(vec![image("AAAA", "image/png")])];
    assert!(sanitize_modalities(modalities, "vendor/model", &accepted).is_none());

    let audio = vec![ModelMessage::new(ModelRole::User).with_content(vec![ModelContent::Audio {
        data: String::from("AAAAAAAA"),
        mime_type: String::from("audio/wav"),
    }])];
    let sanitized = sanitize_modalities(modalities, "vendor/model", &audio).expect("replaced");
    let ModelContent::Text(placeholder) = &sanitized[0].content[0] else {
        panic!("audio placeholder expected");
    };
    assert!(placeholder.contains("audio/wav"), "{placeholder}");
    assert!(placeholder.contains("6-byte"), "{placeholder}");
}

#[test]
fn guard_transcript_replaces_undeclared_media_and_bounds_the_rest() {
    let modalities = Modalities { input: &["text", "image/png"], output: &["text"] };
    let transcript =
        vec![ModelMessage::new(ModelRole::User).with_content(vec![image("AAAA", "audio/wav")])];
    let guarded = guard_transcript(modalities, "vendor/model", &transcript).expect("rewritten");
    assert!(
        matches!(&guarded[0].content[0], ModelContent::Text(text) if text.contains("audio/wav"))
    );
}

#[test]
fn media_budget_counts_decoded_image_bytes() {
    let transcript =
        vec![ModelMessage::new(ModelRole::User).with_content(vec![image("AAAAAAAA", "image/png")])];
    assert_eq!(media_bytes(&transcript), 6);
}

#[test]
fn oversized_media_is_offloaded_largest_first_with_a_stable_placeholder() {
    let transcript = vec![
        ModelMessage::new(ModelRole::User).with_content(vec![
            image("AAAAAAAA", "image/png"),
            image("AAAAAAAAAAAAAAAA", "image/webp"),
        ]),
        ModelMessage::text(ModelRole::Assistant, "kept"),
    ];
    let bounded = bounded_media_with(&transcript, 10, 12).expect("offloads");
    // Dropping the 12-byte image leaves 6 bytes, under the 12-byte target.
    assert!(matches!(&bounded[0].content[1], ModelContent::Text(_)), "largest replaced");
    assert!(matches!(&bounded[0].content[0], ModelContent::Image { .. }), "smaller kept");
    assert!(matches!(&bounded[1].content[0], ModelContent::Text(_)), "text untouched");

    let first = bounded_media_with(&transcript, 10, 12).expect("offloads");
    let second = bounded_media_with(&transcript, 10, 12).expect("offloads");
    assert_eq!(first, second, "placeholder substitution is deterministic");
}

#[test]
fn transcripts_under_the_trigger_are_not_rewritten() {
    let transcript =
        vec![ModelMessage::new(ModelRole::User).with_content(vec![image("AAAAAAAA", "image/png")])];
    assert!(bounded_media_with(&transcript, 64, 32).is_none());
}

#[test]
fn model_kind_round_trips_catalog_type_values() {
    assert_eq!(ModelKind::parse("chat"), Some(ModelKind::Chat));
    assert_eq!(ModelKind::parse("image"), Some(ModelKind::Image));
    assert_eq!(ModelKind::parse("decision"), Some(ModelKind::Decision));
    assert_eq!(ModelKind::parse("unknown"), None);
    assert_eq!(ModelKind::Chat.as_str(), "chat");
}
