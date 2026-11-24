//! Reasoning-effort session configuration.
//!
//! Providers that document a reasoning field declare the values their selected
//! model accepts (catalog-derived, never a generic low/medium/high list). The
//! adapter advertises them as an ACP `effort` select option; the CLI's existing
//! `/effort` alias then works without a new command.
//!
//! Kept out of `agent_impl.rs` so the provider trait surface stays readable and
//! the semantic validation is testable on its own.

use ee_agent_acp_server::{ProviderError, ProviderFuture, SetConfigOptionContext};
use ee_agent_protocol::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};

use super::OrchestratorProvider;

/// Config-option id the CLI's `/effort` alias resolves to.
pub const REASONING_EFFORT_CONFIG_ID: &str = "effort";

/// Value that restores the provider profile's startup configuration.
pub const DEFAULT_EFFORT_VALUE: &str = "default";

/// Catalog-derived reasoning-effort choices for one configured model.
///
/// Values are variant ids from the model's catalog row (`low`, `high`, `max`,
/// `none`, ...). `current` is the configured startup effort, shown as the
/// selected value; `default` is always offered so a runtime override can be
/// removed without restarting the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningEffortControl {
    values: Vec<String>,
    current: Option<String>,
}

impl ReasoningEffortControl {
    /// Builds a control from catalog variant ids, dropping blanks, duplicates,
    /// and the reserved `default` value.
    #[must_use]
    pub fn new(values: Vec<String>, current: Option<String>) -> Self {
        let mut deduplicated = Vec::with_capacity(values.len());
        for value in values {
            let value = value.trim().to_string();
            if value.is_empty() || value == DEFAULT_EFFORT_VALUE || deduplicated.contains(&value) {
                continue;
            }
            deduplicated.push(value);
        }
        let current = current.filter(|current| deduplicated.contains(current));
        Self { values: deduplicated, current }
    }

    /// Whether any selectable effort exists beyond the provider default.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Whether one value is a declared effort, ignoring the reserved default.
    #[must_use]
    pub fn accepts(&self, value: &str) -> bool {
        self.values.iter().any(|allowed| allowed == value)
    }

    /// Returns a copy whose advertised current value is `current`.
    #[must_use]
    pub fn with_current(&self, current: Option<String>) -> Self {
        let current = current.filter(|current| self.values.contains(current));
        Self { values: self.values.clone(), current }
    }

    /// The advertised ACP select option.
    #[must_use]
    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        let mut choices =
            vec![SessionConfigSelectOption::new(DEFAULT_EFFORT_VALUE, "default: provider default")];
        choices.extend(
            self.values
                .iter()
                .map(|value| SessionConfigSelectOption::new(value.clone(), value.clone())),
        );
        let current = self.current.clone().unwrap_or_else(|| String::from(DEFAULT_EFFORT_VALUE));
        vec![
            SessionConfigOption::select(
                REASONING_EFFORT_CONFIG_ID,
                "Reasoning effort",
                current,
                choices,
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ]
    }

    /// Maps one requested value to the runtime override.
    ///
    /// Returns `Ok(None)` for the provider default and an actionable error for
    /// a value the selected model does not declare.
    fn normalize(&self, value: &str) -> Result<Option<String>, ProviderError> {
        let value = value.trim();
        if value == DEFAULT_EFFORT_VALUE || value.is_empty() {
            return Ok(None);
        }
        if self.values.iter().any(|allowed| allowed == value) {
            return Ok(Some(value.to_string()));
        }
        Err(ProviderError::InvalidRequest(format!(
            "unsupported reasoning effort {value:?}; expected {DEFAULT_EFFORT_VALUE} or one of {}",
            self.values.join(", ")
        )))
    }
}

/// Advertised config options for one provider configuration.
#[must_use]
pub(super) fn config_options(
    control: Option<&ReasoningEffortControl>,
) -> Option<Vec<SessionConfigOption>> {
    control.filter(|control| !control.is_empty()).map(ReasoningEffortControl::config_options)
}

/// Advertised config options showing `current` as the selected value.
#[must_use]
pub(super) fn config_options_for(
    control: Option<&ReasoningEffortControl>,
    current: Option<String>,
) -> Option<Vec<SessionConfigOption>> {
    control
        .filter(|control| !control.is_empty())
        .map(|control| control.with_current(current).config_options())
}

/// Applies one `effort` change to the session's runtime and returns the updated
/// option list for the client.
pub(super) fn set_config_option(
    provider: &OrchestratorProvider,
    ctx: SetConfigOptionContext,
) -> ProviderFuture<Result<Vec<SessionConfigOption>, ProviderError>> {
    let control = provider.config.reasoning_effort.clone();
    let sessions = provider.sessions.clone();
    let session_store = provider.session_store.clone();
    let implementation_name = provider.config.implementation.name.clone();
    Box::pin(async move {
        let Some(control) = control else {
            return Err(ProviderError::InvalidRequest(
                "session/set_config_option: this provider advertises no reasoning effort".into(),
            ));
        };
        if ctx.config_id.0.as_ref() != REASONING_EFFORT_CONFIG_ID {
            return Err(ProviderError::InvalidRequest(format!(
                "unknown session config option {:?}; only `{REASONING_EFFORT_CONFIG_ID}` is supported",
                ctx.config_id.0
            )));
        }
        let Some(value) = ctx.value.as_value_id() else {
            return Err(ProviderError::InvalidRequest(
                "reasoning effort expects one of its advertised id values".into(),
            ));
        };
        let effort = control.normalize(value.0.as_ref())?;
        {
            let mut sessions = sessions.lock().expect("adapter sessions poisoned");
            let Some(session) = sessions.get_mut(&ctx.session_id.to_string()) else {
                return Err(ProviderError::InvalidRequest(format!(
                    "unknown orchestrator session: {}",
                    ctx.session_id
                )));
            };
            session.runtime.set_reasoning_effort(effort.clone());
        }
        super::conversation::persist_session_snapshot(
            &session_store,
            &implementation_name,
            &ctx.session_id,
            &sessions,
        )?;
        let updated = control.with_current(effort);
        Ok(updated.config_options())
    })
}

#[cfg(test)]
mod tests {
    use ee_agent_protocol::{SessionConfigKind, SessionConfigSelectOptions};

    use super::*;

    #[test]
    fn control_offers_default_and_catalog_values_once() {
        let control = ReasoningEffortControl::new(
            vec![
                String::from("low"),
                String::from(" high "),
                String::from("low"),
                String::from("default"),
                String::new(),
            ],
            Some(String::from("high")),
        );
        assert!(!control.is_empty());
        let options = control.config_options();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id.0.as_ref(), REASONING_EFFORT_CONFIG_ID);
        assert!(matches!(options[0].category, Some(SessionConfigOptionCategory::ThoughtLevel)));
        let SessionConfigKind::Select(select) = &options[0].kind else {
            panic!("effort is a select option");
        };
        assert_eq!(select.current_value.0.as_ref(), "high");
        let SessionConfigSelectOptions::Ungrouped(choices) = &select.options else {
            panic!("effort values are ungrouped");
        };
        assert_eq!(
            choices.iter().map(|choice| choice.value.0.as_ref()).collect::<Vec<_>>(),
            ["default", "low", "high"]
        );
    }

    #[test]
    fn empty_controls_are_not_advertised() {
        assert!(config_options(None).is_none());
        let empty = ReasoningEffortControl::new(Vec::new(), None);
        assert!(empty.is_empty());
        assert!(config_options(Some(&empty)).is_none());
    }

    #[test]
    fn normalize_accepts_default_and_declared_values_only() {
        let control =
            ReasoningEffortControl::new(vec![String::from("low"), String::from("high")], None);
        assert_eq!(control.normalize("default"), Ok(None));
        assert_eq!(control.normalize(" "), Ok(None));
        assert_eq!(control.normalize("high"), Ok(Some(String::from("high"))));
        let error = control.normalize("ultra").expect_err("unknown effort is rejected");
        assert!(error.to_string().contains("expected default or one of low, high"), "{error}");
    }
}
