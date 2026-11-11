//! Deterministic prompt section rendering.
//!
//! Sections are sorted by `(order, registration)`, empty text is dropped,
//! interpolation is strict, and `complete` replaces the whole prompt.

use std::collections::{BTreeMap, BTreeSet};

use crate::prompt::error::PromptError;
use crate::prompt::sections::Slot;

/// One contributed prompt section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    /// Slot name that owns the order.
    pub slot: &'static str,
    /// Numeric order copied from the slot.
    pub order: i32,
    /// Registration identity used for duplicate detection and tie-breaking.
    pub registration: String,
    /// Section body.
    pub text: String,
    /// When true this section replaces the whole prompt.
    pub complete: bool,
    /// When false, `{{...}}` is preserved literally.
    pub interpolate: bool,
}

impl Section {
    /// Creates a section for `slot` with an explicit registration name.
    pub(crate) fn new(
        slot: &'static Slot,
        registration: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Self, PromptError> {
        let registration = registration.into();
        if registration.trim().is_empty() {
            return Err(PromptError::EmptyRegistration);
        }
        debug_assert!(
            crate::prompt::sections::stable_slot(slot.name).is_some()
                || crate::prompt::sections::runtime_slot(slot.name).is_some(),
            "prompt slot {} is not registered in either plane",
            slot.name
        );
        Ok(Self {
            slot: slot.name,
            order: slot.order,
            registration,
            text: text.into(),
            complete: false,
            interpolate: true,
        })
    }

    /// Creates a section using the slot's first registration name.
    pub(crate) fn for_slot(
        slot: &'static Slot,
        text: impl Into<String>,
    ) -> Result<Self, PromptError> {
        Self::new(slot, slot.default_registration(), text)
    }

    /// Marks this section as a complete prompt.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_complete(mut self, complete: bool) -> Self {
        self.complete = complete;
        self
    }

    /// Enables or disables `{{variable}}` interpolation.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_interpolation(mut self, interpolate: bool) -> Self {
        self.interpolate = interpolate;
        self
    }
}

/// Renders sections into one prompt string.
pub(crate) fn render_sections(
    sections: &[Section],
    variables: &BTreeMap<String, String>,
    byte_budget: Option<usize>,
) -> Result<String, PromptError> {
    let mut registrations = BTreeSet::new();
    for section in sections {
        if !registrations.insert(section.registration.as_str()) {
            return Err(PromptError::DuplicateRegistration {
                registration: section.registration.clone(),
            });
        }
    }

    let mut effective: Vec<&Section> =
        sections.iter().filter(|section| !section.text.trim().is_empty()).collect();
    let complete: Vec<&&Section> = effective.iter().filter(|section| section.complete).collect();
    if complete.len() > 1 {
        return Err(PromptError::MultipleCompleteSections { count: complete.len() });
    }

    let rendered = if let Some(section) = complete.first() {
        render_section(section, variables)?
    } else {
        effective.sort_by(|left, right| {
            left.order.cmp(&right.order).then_with(|| left.registration.cmp(&right.registration))
        });
        let mut parts = Vec::with_capacity(effective.len());
        for section in effective {
            parts.push(render_section(section, variables)?);
        }
        parts.join("\n\n")
    };

    if let Some(max) = byte_budget
        && rendered.len() > max
    {
        return Err(PromptError::ByteBudgetExceeded { used: rendered.len(), max });
    }
    Ok(rendered)
}

fn render_section(
    section: &Section,
    variables: &BTreeMap<String, String>,
) -> Result<String, PromptError> {
    if section.interpolate {
        interpolate(&section.text, section.slot, variables)
    } else {
        Ok(section.text.clone())
    }
}

/// Replaces every `{{name}}` with its supplied value.
///
/// Unknown, empty, malformed, or unclosed expressions fail assembly. Single
/// braces are ordinary text, so JSON snippets and code fences pass through.
fn interpolate(
    text: &str,
    section: &'static str,
    variables: &BTreeMap<String, String>,
) -> Result<String, PromptError> {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            return Err(PromptError::MalformedVariable { section: section.to_string() });
        };
        let name = &after_open[..end];
        if !valid_variable_name(name) {
            return Err(PromptError::MalformedVariable { section: section.to_string() });
        }
        match variables.get(name) {
            Some(value) if !value.is_empty() => output.push_str(value),
            Some(_) => {
                return Err(PromptError::UndefinedVariable {
                    section: section.to_string(),
                    variable: name.to_string(),
                });
            }
            None => {
                return Err(PromptError::UnknownVariable {
                    section: section.to_string(),
                    variable: name.to_string(),
                });
            }
        }
        rest = &after_open[end + 2..];
    }
    output.push_str(rest);
    Ok(output)
}

fn valid_variable_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::sections::{HARNESS_IDENTITY, PLAN_POLICY, TOOL_READ, TOOL_WRITE};

    fn section(slot: &'static Slot, text: &str) -> Section {
        Section::for_slot(slot, text).expect("valid section")
    }

    fn variables() -> BTreeMap<String, String> {
        BTreeMap::from([("provider".to_string(), "test".to_string())])
    }

    #[test]
    fn sections_sort_by_order_then_registration() {
        let sections = [
            section(&TOOL_READ, "read"),
            section(&HARNESS_IDENTITY, "identity"),
            Section::new(&TOOL_WRITE, "tool:write-a", "write-a").expect("valid"),
            Section::new(&TOOL_WRITE, "tool:write-b", "write-b").expect("valid"),
        ];
        let rendered = render_sections(&sections, &variables(), None).expect("renders");
        assert_eq!(rendered, "identity\n\nread\n\nwrite-a\n\nwrite-b");
    }

    #[test]
    fn empty_and_whitespace_sections_drop() {
        let sections = [section(&HARNESS_IDENTITY, "identity"), section(&PLAN_POLICY, "  \n ")];
        let rendered = render_sections(&sections, &variables(), None).expect("renders");
        assert_eq!(rendered, "identity");
    }

    #[test]
    fn duplicate_registration_fails_even_when_empty() {
        let sections = [
            section(&TOOL_READ, "read"),
            Section::new(&TOOL_WRITE, "tool:read", "").expect("valid"),
        ];
        let error = render_sections(&sections, &variables(), None).expect_err("duplicate");
        assert_eq!(error, PromptError::DuplicateRegistration { registration: "tool:read".into() });
    }

    #[test]
    fn interpolation_is_strict() {
        let sections = [section(&HARNESS_IDENTITY, "powered by {{provider}}")];
        assert_eq!(
            render_sections(&sections, &variables(), None).expect("renders"),
            "powered by test"
        );

        let unknown = [section(&HARNESS_IDENTITY, "powered by {{model}}")];
        assert_eq!(
            render_sections(&unknown, &variables(), None).expect_err("unknown"),
            PromptError::UnknownVariable {
                section: "HARNESS_IDENTITY".into(),
                variable: "model".into(),
            }
        );

        let undefined = [section(&HARNESS_IDENTITY, "powered by {{provider}}")];
        let empty = BTreeMap::from([("provider".to_string(), String::new())]);
        assert_eq!(
            render_sections(&undefined, &empty, None).expect_err("undefined"),
            PromptError::UndefinedVariable {
                section: "HARNESS_IDENTITY".into(),
                variable: "provider".into(),
            }
        );

        for malformed in ["{{provider", "{{}}", "{{bad name}}", "{{a{{b}}"] {
            let sections = [section(&HARNESS_IDENTITY, malformed)];
            assert!(
                matches!(
                    render_sections(&sections, &variables(), None),
                    Err(PromptError::MalformedVariable { .. })
                ),
                "{malformed} must fail"
            );
        }
    }

    #[test]
    fn non_interpolating_section_preserves_braces() {
        let sections =
            [section(&HARNESS_IDENTITY, "literal {{provider}}").with_interpolation(false)];
        assert_eq!(
            render_sections(&sections, &variables(), None).expect("renders"),
            "literal {{provider}}"
        );
    }

    #[test]
    fn complete_section_replaces_the_prompt() {
        let sections = [
            section(&HARNESS_IDENTITY, "identity"),
            section(&PLAN_POLICY, "plan").with_complete(true),
        ];
        assert_eq!(render_sections(&sections, &variables(), None).expect("renders"), "plan");
    }

    #[test]
    fn more_than_one_complete_section_fails() {
        let sections = [
            section(&HARNESS_IDENTITY, "identity").with_complete(true),
            section(&PLAN_POLICY, "plan").with_complete(true),
        ];
        assert_eq!(
            render_sections(&sections, &variables(), None).expect_err("complete collision"),
            PromptError::MultipleCompleteSections { count: 2 }
        );
    }

    #[test]
    fn empty_complete_section_does_not_count() {
        let sections = [
            section(&HARNESS_IDENTITY, "").with_complete(true),
            section(&PLAN_POLICY, "plan").with_complete(true),
        ];
        assert_eq!(render_sections(&sections, &variables(), None).expect("renders"), "plan");
    }

    #[test]
    fn byte_budget_is_enforced_after_rendering() {
        let sections = [section(&HARNESS_IDENTITY, "identity")];
        assert_eq!(
            render_sections(&sections, &variables(), Some(4)).expect_err("over budget"),
            PromptError::ByteBudgetExceeded { used: 8, max: 4 }
        );
        assert_eq!(
            render_sections(&sections, &variables(), Some(8)).expect("exact budget"),
            "identity"
        );
    }

    #[test]
    fn empty_registration_is_rejected() {
        assert_eq!(
            Section::new(&HARNESS_IDENTITY, " ", "identity").expect_err("empty"),
            PromptError::EmptyRegistration
        );
    }
}
