//! Event-context tests: delete.
use super::*;

#[test]
fn delete_combining_enclosing_keycaps_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "1\u{E0101}\u{20E3}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 8, ty: PointSelect });

    assert_eq!(harness.debug_render(), "1\u{E0101}\u{20E3}|");

    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // multiple COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{20E3}".into() });
    assert_eq!(harness.debug_render(), "1\u{20E3}\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{20E3}".into() });
    assert_eq!(harness.debug_render(), "\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{20E3}\u{20E3}".into() });
    assert_eq!(harness.debug_render(), "\u{20E3}\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_variation_selector_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{FE0F}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 3, ty: PointSelect });

    assert_eq!(harness.debug_render(), "\u{FE0F}|");

    // Isolated variation selector
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple variation selectors
    ctx.do_edit(EditNotification::Insert { chars: "\u{FE0F}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "\u{FE0F}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{FE0F}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "\u{FE0F}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "\u{E0100}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "\u{E0100}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Multiple variation selectors
    ctx.do_edit(EditNotification::Insert { chars: "#\u{FE0F}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "#\u{FE0F}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "#\u{FE0F}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "#\u{FE0F}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "#\u{E0100}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "#\u{E0100}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "#\u{E0100}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "#\u{E0100}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_emoji_zwj_sequence_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{1F441}\u{200D}\u{1F5E8}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{1F5E8}|");

    // U+200D is ZERO WIDTH JOINER.
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{1F5E8}\u{FE0E}".into() });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{1F5E8}\u{FE0E}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F373}".into() });
    assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}\u{1F373}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F487}\u{200D}\u{2640}".into() });
    assert_eq!(harness.debug_render(), "\u{1F487}\u{200D}\u{2640}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F487}\u{200D}\u{2640}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "\u{1F487}\u{200D}\u{2640}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert {
        chars: "\u{1F468}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F48B}\u{200D}\u{1F468}".into(),
    });
    assert_eq!(
        harness.debug_render(),
        "\u{1F468}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F48B}\u{200D}\u{1F468}|"
    );
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier can be appended to the first emoji.
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{1F3FB}\u{200D}\u{1F4BC}".into() });
    assert_eq!(harness.debug_render(), "\u{1F469}\u{1F3FB}\u{200D}\u{1F4BC}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // End with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}".into() });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F5E8}".into() });
    assert_eq!(harness.debug_render(), "\u{200D}\u{1F5E8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{FE0E}\u{200D}\u{1F5E8}".into() });
    assert_eq!(harness.debug_render(), "\u{FE0E}\u{200D}\u{1F5E8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0E}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0E}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Multiple ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{200D}\u{1F5E8}".into() });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{200D}\u{1F5E8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}".into() });
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{200D}".into() });
    assert_eq!(harness.debug_render(), "\u{200D}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_flags_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{1F1FA}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 4, ty: PointSelect });

    // Isolated regional indicator symbol
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Odd numbered regional indicator symbols
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{1F1F8}\u{1F1FA}".into() });
    assert_eq!(harness.debug_render(), "\u{1F1FA}\u{1F1F8}\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}\u{1F1F8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Incomplete sequence. (no tag_term: U+E007E)
    ctx.do_edit(EditNotification::Insert { chars: "a\u{1F3F4}\u{E0067}b".into() });
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E0067}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // No tag_base
    ctx.do_edit(EditNotification::Insert { chars: "\u{E0067}\u{E007F}b".into() });
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E007F}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // Isolated tag chars
    ctx.do_edit(EditNotification::Insert { chars: "\u{E0067}\u{E0067}b".into() });
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E0067}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // Isolated tab term.
    ctx.do_edit(EditNotification::Insert { chars: "\u{E007F}\u{E007F}b".into() });
    assert_eq!(harness.debug_render(), "a\u{E007F}\u{E007F}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E007F}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // Immediate tag_term after tag_base
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}b".into() });
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");
}

#[test]
fn delete_emoji_modifier_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{1F466}\u{1F3FB}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 8, ty: PointSelect });

    // U+1F3FB is EMOJI MODIFIER FITZPATRICK TYPE-1-2.
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F3FB}".into() });
    assert_eq!(harness.debug_render(), "\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F3FB}\u{1F3FB}".into() });
    assert_eq!(harness.debug_render(), "\u{1F3FB}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Multiple emoji modifiers
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_mixed_edge_cases_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 7, ty: PointSelect });

    // COMBINING ENCLOSING KEYCAP + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
    // COMBINING ENCLOSING KEYCAP + ending with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // COMBINING ENCLOSING KEYCAP + ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{200D}\u{1F5E8}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // COMBINING ENCLOSING KEYCAP + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // COMBINING ENCLOSING KEYCAP + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1f466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + end with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert {
        chars: "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F469}".into(),
    });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start withj ZERO WIDTH JOINER + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + Regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + end with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{200D}\u{1F469}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + end with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + Emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // RIS + LF
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1E6}\u{000A}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1E6}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_variation_selector_with_combining_mark_uses_grapheme_boundary() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("e\u{0301}\u{FE0F}");
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 6, ty: PointSelect });

    assert_eq!(harness.debug_render(), "e\u{0301}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}
