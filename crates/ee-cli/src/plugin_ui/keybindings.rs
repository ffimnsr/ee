//! Plugin keybinding parsing and merge helpers.
//!
//! Plugin default keybindings are merged *below* the user keymap: a user
//! binding always wins, an explicit user unbind suppresses the plugin default,
//! and duplicate plugin defaults for the same key are rejected deterministically
//! by the registry.

use ee_plugin_api::contribution::KeybindingContribution;

use crate::keymap::{BindingKey, Mode, parse_binding_mode, parse_key_press_spec};

/// A validated plugin default binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedKeybinding {
    pub binding: BindingKey,
}

/// Parse a contribution key spec into a frontend binding.
pub fn parse_keybinding(contribution: &KeybindingContribution) -> Result<ParsedKeybinding, String> {
    let key = parse_key_press_spec(&contribution.key)?;
    let mode = when_to_mode(contribution.when.as_deref())?;
    Ok(ParsedKeybinding {
        binding: BindingKey { mode, key: key.key, modifiers: key.modifiers, prefix: None },
    })
}

/// Map the contribution `when` context to an editor mode.
///
/// `editor`/`global` are the common defaults used by plugins that do not know
/// about vim modes. Unknown contexts are rejected so a typo never silently
/// binds somewhere unexpected.
pub fn when_to_mode(when: Option<&str>) -> Result<Mode, String> {
    let Some(when) = when.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Mode::Normal);
    };
    match when {
        "editor" | "global" | "normal" => Ok(Mode::Normal),
        other => parse_binding_mode(other),
    }
}

/// Whether the user keymap explicitly unbinds `binding`.
#[must_use]
pub fn user_unbinds(settings: &crate::keymap::KeymapSettings, binding: &BindingKey) -> bool {
    settings
        .operations
        .iter()
        .any(|operation| matches!(operation, crate::keymap::KeymapOperation::Unbind(key) if key == binding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Action;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ee_plugin_api::ids::CommandId;

    fn contribution(key: &str, when: Option<&str>) -> KeybindingContribution {
        KeybindingContribution {
            command: CommandId::new("demo.run").unwrap(),
            key: key.to_owned(),
            when: when.map(str::to_owned),
        }
    }

    #[test]
    fn parse_and_format_round_trip_for_plugin_command_actions() {
        let parsed = parse_keybinding(&contribution("ctrl+shift+o", Some("normal"))).unwrap();
        assert_eq!(parsed.binding.mode, Mode::Normal);
        assert_eq!(parsed.binding.key, KeyCode::Char('o'));
        assert_eq!(parsed.binding.modifiers, KeyModifiers::CONTROL | KeyModifiers::SHIFT);

        let action = Action::PluginCommand(String::from("demo.run"));
        let spec = crate::keymap::format_action_spec(&action);
        assert_eq!(spec, "plugin_command:demo.run");
        assert_eq!(crate::keymap::parse_action_spec(&spec).unwrap(), action);
    }

    #[test]
    fn when_context_defaults_to_normal_and_accepts_modes() {
        assert_eq!(when_to_mode(None).unwrap(), Mode::Normal);
        assert_eq!(when_to_mode(Some("editor")).unwrap(), Mode::Normal);
        assert_eq!(when_to_mode(Some("insert")).unwrap(), Mode::Insert);
        assert!(when_to_mode(Some("nonsense")).is_err());
    }

    #[test]
    fn invalid_key_specs_are_rejected() {
        assert!(parse_keybinding(&contribution("not-a-key", None)).is_err());
    }
}
