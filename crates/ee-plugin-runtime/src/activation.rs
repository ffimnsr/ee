//! Activation conditions and scope lifetimes.
//!
//! Activation is a pure evaluation over the manifest so the manager can defer
//! plugins without spawning anything. `command` scopes are ephemeral: they get
//! one invocation and no update stream.

use ee_plugin_api::manifest::{Activation, ScopeTarget};

/// Lifetime class for a declared scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeLifetime {
    /// Lives until the host/workspace/session closes.
    Persistent,
    /// Activated when a buffer, view, or turn is created.
    Deferred,
    /// Activated for one command invocation only; no update stream.
    Ephemeral,
}

/// Map a manifest scope to its lifetime.
pub fn scope_lifetime(scope: ScopeTarget) -> ScopeLifetime {
    match scope {
        ScopeTarget::Host | ScopeTarget::Workspace | ScopeTarget::Session => {
            ScopeLifetime::Persistent
        }
        ScopeTarget::Buffer | ScopeTarget::View | ScopeTarget::Turn => ScopeLifetime::Deferred,
        ScopeTarget::Command => ScopeLifetime::Ephemeral,
    }
}

/// Trigger being evaluated.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActivationTrigger<'a> {
    pub startup: bool,
    pub language: Option<&'a str>,
    pub command: Option<&'a str>,
    pub event: Option<&'a str>,
}

impl<'a> ActivationTrigger<'a> {
    pub fn startup() -> Self {
        Self { startup: true, ..Self::default() }
    }

    pub fn language(language: &'a str) -> Self {
        Self { language: Some(language), ..Self::default() }
    }

    pub fn command(command: &'a str) -> Self {
        Self { command: Some(command), ..Self::default() }
    }

    pub fn event(event: &'a str) -> Self {
        Self { event: Some(event), ..Self::default() }
    }
}

/// Whether a plugin activates for the given trigger.
pub fn should_activate(activation: &Activation, trigger: &ActivationTrigger<'_>) -> bool {
    if trigger.startup && activates_at_startup(activation) {
        return true;
    }
    if let Some(language) = trigger.language
        && activation.on_language.iter().any(|value| value == language)
    {
        return true;
    }
    if let Some(command) = trigger.command
        && activation.on_command.iter().any(|value| value == command)
    {
        return true;
    }
    if let Some(event) = trigger.event
        && activation.on_event.iter().any(|value| value == event)
    {
        return true;
    }
    false
}

/// Whether the plugin should be activated during `PluginManager::start`.
///
/// A manifest with no activation conditions at all is treated as startup-active
/// so simple plugins do not need boilerplate.
pub fn activates_at_startup(activation: &Activation) -> bool {
    if activation.on_startup {
        return true;
    }
    if activation.scopes.iter().any(|scope| scope_lifetime(*scope) == ScopeLifetime::Persistent) {
        return true;
    }
    activation.on_language.is_empty()
        && activation.on_command.is_empty()
        && activation.on_event.is_empty()
        && activation.scopes.is_empty()
}

/// Whether the plugin must be treated as command-scoped.
pub fn is_ephemeral(activation: &Activation) -> bool {
    !activation.on_command.is_empty() || activation.scopes.contains(&ScopeTarget::Command)
}

/// Whether every declared scope is persistent.
pub fn has_only_persistent_scopes(activation: &Activation) -> bool {
    !activation.scopes.is_empty()
        && activation.scopes.iter().all(|scope| scope_lifetime(*scope) == ScopeLifetime::Persistent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activation() -> Activation {
        Activation {
            on_startup: false,
            on_language: vec!["rust".to_owned()],
            on_command: vec!["outline.toggle".to_owned()],
            on_event: vec!["editor.document.changed".to_owned()],
            scopes: vec![ScopeTarget::Host],
        }
    }

    #[test]
    fn startup_requires_startup_flag_or_persistent_scope() {
        let mut manifest = activation();
        manifest.scopes.clear();
        assert!(!activates_at_startup(&manifest));
        manifest.on_startup = true;
        assert!(activates_at_startup(&manifest));

        manifest.on_startup = false;
        manifest.scopes = vec![ScopeTarget::Workspace];
        assert!(activates_at_startup(&manifest));

        manifest.scopes = vec![ScopeTarget::Buffer];
        assert!(!activates_at_startup(&manifest));
    }

    #[test]
    fn empty_activation_defaults_to_startup() {
        assert!(activates_at_startup(&Activation::default()));
    }

    #[test]
    fn language_command_and_event_triggers_match_exactly() {
        let manifest = activation();
        assert!(should_activate(&manifest, &ActivationTrigger::language("rust")));
        assert!(!should_activate(&manifest, &ActivationTrigger::language("python")));
        assert!(should_activate(&manifest, &ActivationTrigger::command("outline.toggle")));
        assert!(!should_activate(&manifest, &ActivationTrigger::command("other")));
        assert!(should_activate(&manifest, &ActivationTrigger::event("editor.document.changed")));
        assert!(!should_activate(&manifest, &ActivationTrigger::event("other")));
    }

    #[test]
    fn scope_lifetimes_are_stable() {
        assert_eq!(scope_lifetime(ScopeTarget::Host), ScopeLifetime::Persistent);
        assert_eq!(scope_lifetime(ScopeTarget::Workspace), ScopeLifetime::Persistent);
        assert_eq!(scope_lifetime(ScopeTarget::Session), ScopeLifetime::Persistent);
        assert_eq!(scope_lifetime(ScopeTarget::Buffer), ScopeLifetime::Deferred);
        assert_eq!(scope_lifetime(ScopeTarget::View), ScopeLifetime::Deferred);
        assert_eq!(scope_lifetime(ScopeTarget::Turn), ScopeLifetime::Deferred);
        assert_eq!(scope_lifetime(ScopeTarget::Command), ScopeLifetime::Ephemeral);
    }

    #[test]
    fn command_scope_and_trigger_are_ephemeral() {
        let mut manifest = activation();
        assert!(is_ephemeral(&manifest));
        manifest.on_command.clear();
        assert!(!is_ephemeral(&manifest));
        manifest.scopes = vec![ScopeTarget::Command];
        assert!(is_ephemeral(&manifest));
    }

    #[test]
    fn persistent_scope_check_ignores_empty_lists() {
        let manifest = Activation { scopes: vec![ScopeTarget::Host], ..Default::default() };
        assert!(has_only_persistent_scopes(&manifest));
        assert!(!has_only_persistent_scopes(&Activation::default()));
        let mixed = Activation {
            scopes: vec![ScopeTarget::Host, ScopeTarget::Buffer],
            ..Default::default()
        };
        assert!(!has_only_persistent_scopes(&mixed));
    }
}
