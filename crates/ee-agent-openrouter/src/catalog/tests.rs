use ee_agent_orchestrator::{ModelKind, ReasoningProtocol};

use super::*;

#[test]
fn generated_rows_are_sorted_unique_and_chat_rows_carry_limits() {
    let mut previous = "";
    for entry in models() {
        assert!(previous < entry.id, "{} must follow {previous}", entry.id);
        previous = entry.id;
        if entry.kind == ModelKind::Chat {
            let limits = entry.limits.unwrap_or_else(|| panic!("{entry} has no limits"));
            assert!(limits.context > 0, "{entry} context");
            assert!(limits.output > 0, "{entry} output");
        }
    }
}

#[test]
fn chat_rows_advertise_the_chat_route() {
    for entry in models().filter(|entry| entry.kind == ModelKind::Chat) {
        assert!(entry.routes.chat || entry.routes.responses, "{entry} advertises no text route");
    }
}

#[test]
fn representative_rows_carry_expected_metadata() {
    let deepseek = find("deepseek/deepseek-v4-flash").expect("deepseek row");
    assert_eq!(deepseek.kind, ModelKind::Chat);
    assert!(deepseek.limits.expect("limits").context >= 1_000_000);
    assert!(deepseek.reasoning.supported);
    assert!(deepseek.routes.chat);
    assert!(deepseek.cost.is_some());

    let claude = find("anthropic/claude-sonnet-4.5").expect("claude row");
    assert!(claude.modalities.accepts("application/pdf"));
    assert!(!claude.cost.expect("cost").tiers.is_empty(), "context-tier override retained");

    let image = find("tencent/hy-image-v3.5-preview").expect("image row");
    assert_eq!(image.kind, ModelKind::Image);
    assert!(!image.is_chat());
}

#[test]
fn provider_compaction_is_never_advertised() {
    // The reference OpenRouter provider exposes no `/compact` route, so no row
    // may claim the capability.
    for entry in models() {
        assert!(!entry.compaction, "{entry} must not advertise provider compaction");
    }
}

#[test]
fn capability_matrix_replaces_provider_heuristics() {
    let claude = find("anthropic/claude-sonnet-5").expect("claude row");
    assert!(claude.capabilities.cache.explicit);
    assert_eq!(claude.capabilities.cache.format, CacheFormatId::Anthropic);
    assert_eq!(claude.capabilities.cache.placement, CachePlacement::Auto);
    assert_eq!(claude.capabilities.cache.max_breakpoints, 4);
    assert!(claude.capabilities.tool_choices.contains(&"tool"));

    let gpt = find("openai/gpt-5.5").expect("gpt row");
    assert!(gpt.capabilities.cache.implicit);
    assert!(!gpt.capabilities.cache.explicit);
    assert!(gpt.routes.responses);
    assert!(gpt.capabilities.store);
    assert!(gpt.capabilities.prompt_cache_key);

    let qwen = find("qwen/qwen3-coder").expect("qwen row");
    assert!(qwen.capabilities.cache.explicit, "Qwen routes place breakpoints");
    assert_eq!(qwen.capabilities.cache.placement, CachePlacement::SystemAndTail);
    assert_eq!(qwen.capabilities.cache.format, CacheFormatId::Alibaba);
    assert_eq!(qwen.capabilities.cache.max_breakpoints, 4);
    assert!(qwen.capabilities.cache.targets.system);
    assert!(qwen.capabilities.cache.targets.messages);
    assert!(!qwen.capabilities.cache.targets.tools, "Qwen never marks tool definitions");
}

#[test]
fn every_chat_row_records_release_and_capabilities() {
    for entry in models().filter(|entry| entry.kind == ModelKind::Chat) {
        assert!(!entry.release_date.is_empty(), "{entry}");
        assert!(entry.release_date.len() >= 7, "{entry} release date is partial");
        assert!(!entry.capabilities.service_tiers.iter().any(|tier| tier.is_empty()), "{entry}");
    }
}

#[test]
fn unknown_models_have_no_catalog_row() {
    assert!(find("vendor/not-a-catalog-model").is_none());
}

#[test]
fn variants_follow_the_selected_protocol() {
    let deepseek = find("deepseek/deepseek-v4-flash").expect("deepseek row");
    let chat: Vec<_> =
        deepseek.variants(ReasoningProtocol::OpenAiChat).iter().map(|v| v.id).collect();
    assert_eq!(chat, ["none", "high", "xhigh"]);
    let responses: Vec<_> =
        deepseek.variants(ReasoningProtocol::OpenAiResponses).iter().map(|v| v.id).collect();
    assert_eq!(responses, chat);
}

#[test]
fn snapshot_digest_is_recorded() {
    assert_eq!(snapshot_sha256().len(), 64);
    assert!(snapshot_source().starts_with("https://"));
}

#[test]
fn catalog_contains_chat_image_and_decision_entries() {
    let kinds: Vec<_> = models().map(|entry| entry.kind).collect();
    assert!(kinds.contains(&ModelKind::Chat));
    assert!(kinds.contains(&ModelKind::Image));
    assert!(kinds.contains(&ModelKind::Decision));
}
