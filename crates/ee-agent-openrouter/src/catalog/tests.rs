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
