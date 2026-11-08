//! Bounded, stable diagnostics for every loader phase.
//!
//! Diagnostics are the user-facing contract for `ee do plugins list/validate`
//! and for the editor status surface. Messages are truncated on a character
//! boundary so an untrusted plugin cannot flood logs with unbounded payloads.

use std::path::PathBuf;

use ee_plugin_api::error::{ErrorClass, PluginError};
use serde::{Deserialize, Serialize};

/// Maximum diagnostic message length in bytes.
pub const MAX_DIAGNOSTIC_MESSAGE: usize = 512;

/// Loader phase that produced a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticPhase {
    Discovery,
    Config,
    Resolve,
    Activation,
    Transport,
    Runtime,
    Shutdown,
}

impl DiagnosticPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Config => "config",
            Self::Resolve => "resolve",
            Self::Activation => "activation",
            Self::Transport => "transport",
            Self::Runtime => "runtime",
            Self::Shutdown => "shutdown",
        }
    }
}

/// One structured loader diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub class: ErrorClass,
    pub code: String,
    pub phase: DiagnosticPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PathBuf>,
    pub message: String,
}

impl Diagnostic {
    /// Build a diagnostic from explicit parts.
    pub fn new(
        class: ErrorClass,
        code: impl Into<String>,
        phase: DiagnosticPhase,
        message: impl Into<String>,
    ) -> Self {
        Self {
            class,
            code: code.into(),
            phase,
            plugin: None,
            origin: None,
            message: truncate_message(message.into()),
        }
    }

    /// Attach the plugin name.
    pub fn with_plugin(mut self, plugin: impl Into<String>) -> Self {
        self.plugin = Some(plugin.into());
        self
    }

    /// Attach the plugin origin path.
    pub fn with_origin(mut self, origin: impl Into<PathBuf>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    /// Build from a typed plugin error without retaining the error value.
    pub fn from_error(error: &PluginError, phase: DiagnosticPhase) -> Self {
        Self::new(error.class(), error.code(), phase, error.to_string())
    }

    /// Build from a typed plugin error and attach plugin/origin context.
    pub fn from_plugin_error(
        error: &PluginError,
        phase: DiagnosticPhase,
        plugin: impl Into<String>,
        origin: Option<PathBuf>,
    ) -> Self {
        let mut diagnostic = Self::from_error(error, phase).with_plugin(plugin);
        if let Some(origin) = origin {
            diagnostic.origin = Some(origin);
        }
        diagnostic
    }
}

fn truncate_message(mut message: String) -> String {
    if message.len() <= MAX_DIAGNOSTIC_MESSAGE {
        return message;
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str("...");
    message
}

#[cfg(test)]
mod tests {
    use ee_plugin_api::error::PluginError;
    use serde_json::json;

    use super::*;

    fn every_class() -> Vec<(PluginError, ErrorClass)> {
        vec![
            (
                PluginError::Manifest(ee_plugin_api::manifest::ManifestError::EmptyField {
                    field: "name",
                }),
                ErrorClass::Manifest,
            ),
            (PluginError::Schema { message: "bad".to_owned() }, ErrorClass::Schema),
            (PluginError::Dependency { message: "missing".to_owned() }, ErrorClass::Dependency),
            (
                PluginError::Permission { permission: "editor.fs.write".to_owned() },
                ErrorClass::Permission,
            ),
            (PluginError::Transport { message: "closed".to_owned() }, ErrorClass::Transport),
            (
                PluginError::Contribution(
                    ee_plugin_api::contribution::ContributionError::EmptyField { field: "title" },
                ),
                ErrorClass::Contribution,
            ),
            (PluginError::Config { message: "bad".to_owned() }, ErrorClass::Config),
            (PluginError::Resource { message: "cap".to_owned() }, ErrorClass::Resource),
            (PluginError::Runtime { message: "boom".to_owned() }, ErrorClass::Runtime),
            (PluginError::Io { message: "gone".to_owned() }, ErrorClass::Io),
        ]
    }

    #[test]
    fn every_class_maps_to_stable_json_snapshot() {
        for (error, expected) in every_class() {
            let diagnostic = Diagnostic::from_plugin_error(
                &error,
                DiagnosticPhase::Activation,
                "demo",
                Some(PathBuf::from("/plugins/demo")),
            );
            assert_eq!(diagnostic.class, expected);
            assert_eq!(diagnostic.code, error.code());
            let value = serde_json::to_value(&diagnostic).unwrap();
            assert_eq!(value["class"], json!(expected));
            assert_eq!(value["phase"], json!("activation"));
            assert_eq!(value["plugin"], json!("demo"));
        }
    }

    #[test]
    fn messages_are_bounded_on_char_boundary() {
        let diagnostic = Diagnostic::new(
            ErrorClass::Runtime,
            "plugin.runtime",
            DiagnosticPhase::Runtime,
            "é".repeat(MAX_DIAGNOSTIC_MESSAGE),
        );
        assert!(diagnostic.message.len() <= MAX_DIAGNOSTIC_MESSAGE + 3);
        assert!(diagnostic.message.ends_with("..."));
        assert!(diagnostic.message.is_char_boundary(diagnostic.message.len()));
    }
}
