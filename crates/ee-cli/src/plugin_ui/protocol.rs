//! Frontend capability negotiation and outbound plugin protocol methods.
//!
//! The frontend never invokes plugin callbacks. It sends a command id with JSON
//! arguments, or a bounded `ui_event` for a declarative panel/picker/overlay.

use std::io;

use serde_json::{Value, json};

use crate::backend::send_rpc_notification;
use crate::buffer::BufferManager;

/// Contribution kinds this terminal frontend can render.
pub const CONTRIBUTION_KINDS: &[&str] = &[
    "command",
    "ex_command",
    "keybinding",
    "status_item",
    "panel",
    "overlay",
    "picker",
    "menu_item",
    "decoration",
];

/// Declarative renderers this frontend implements.
pub const PANEL_RENDERERS: &[&str] = &["list", "text", "markdown"];

/// UI interactions this frontend can emit.
pub const UI_EVENTS: &[&str] = &["filter", "select", "activate", "close"];

/// Capability payload advertised in `client_started`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendCapabilities {
    pub frontend: &'static str,
    pub contribution_kinds: Vec<String>,
    pub panel_renderers: Vec<String>,
    pub dynamic_ui: bool,
    pub ui_events: Vec<String>,
}

impl Default for FrontendCapabilities {
    fn default() -> Self {
        Self::ee_cli()
    }
}

impl FrontendCapabilities {
    /// Capabilities for this `ee-cli` build.
    #[must_use]
    pub fn ee_cli() -> Self {
        Self {
            frontend: "ee-cli",
            contribution_kinds: CONTRIBUTION_KINDS.iter().map(|kind| (*kind).to_owned()).collect(),
            panel_renderers: PANEL_RENDERERS.iter().map(|kind| (*kind).to_owned()).collect(),
            dynamic_ui: true,
            ui_events: UI_EVENTS.iter().map(|kind| (*kind).to_owned()).collect(),
        }
    }

    #[must_use]
    pub fn supports_kind(&self, kind: &str) -> bool {
        self.contribution_kinds.iter().any(|candidate| candidate == kind)
    }

    #[must_use]
    pub fn supports_renderer(&self, renderer: &str) -> bool {
        self.panel_renderers.iter().any(|candidate| candidate == renderer)
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "frontend": self.frontend,
            "contribution_kinds": self.contribution_kinds,
            "panel_renderers": self.panel_renderers,
            "dynamic_ui": self.dynamic_ui,
            "ui_events": self.ui_events,
        })
    }
}

/// Bounded interaction kinds sent for declarative UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginUiEventKind {
    Filter,
    Select,
    Activate,
    Close,
}

impl PluginUiEventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Select => "select",
            Self::Activate => "activate",
            Self::Close => "close",
        }
    }
}

impl BufferManager {
    /// Send `command { id, args }` to the backend.
    pub(crate) fn send_plugin_command(&self, id: &str, args: Value) -> io::Result<()> {
        if id.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty plugin command id"));
        }
        send_rpc_notification(&self.tx, "command", json!({ "id": id, "args": args }))
    }

    /// Send one `ui_event` for a declarative contribution.
    pub(crate) fn send_plugin_ui_event(
        &self,
        id: &str,
        kind: PluginUiEventKind,
        value: Value,
    ) -> io::Result<()> {
        if id.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty plugin ui event id"));
        }
        send_rpc_notification(
            &self.tx,
            "ui_event",
            json!({
                "host": "editor",
                "id": id,
                "kind": kind.as_str(),
                "value": value,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ee_cli_advertises_declarative_capabilities() {
        let capabilities = FrontendCapabilities::ee_cli();
        assert!(capabilities.supports_kind("panel"));
        assert!(capabilities.supports_kind("picker"));
        assert!(!capabilities.supports_kind("agent_tool"));
        assert!(capabilities.supports_renderer("markdown"));
        assert!(!capabilities.supports_renderer("tree"));
        let json = capabilities.to_json();
        assert_eq!(json["frontend"], "ee-cli");
        assert_eq!(json["dynamic_ui"], true);
    }
}
