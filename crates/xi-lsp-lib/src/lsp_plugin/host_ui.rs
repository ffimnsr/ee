//! Host UI bridge used by the LSP plugin.
//!
//! The legacy plugin proxy mutated plugin state directly. The v2 host only accepts
//! typed effects, so this bridge converts status and picker updates into
//! effects submitted on the plugin connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ee_plugin_api::contribution::StatusAlignment;
use ee_plugin_api::effect::{Effect, StatusItem, StatusItemKey};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ContributionId;
use ee_plugin_api::model::{CompletionSuggestion, NavigationTarget, SymbolItem, ViewId};
use ee_plugin_sdk::client::UiClient;
use serde_json::{Value, json};

const HOVER_OVERLAY: &str = "lsp.ui.hover";
const COMPLETION_PICKER: &str = "lsp.ui.completion";
const LOCATIONS_PICKER: &str = "lsp.ui.locations";
const SYMBOLS_PICKER: &str = "lsp.ui.symbols";
const WORKSPACE_SYMBOLS_PICKER: &str = "lsp.ui.workspace_symbols";
const CODE_ACTIONS_PICKER: &str = "lsp.ui.code_actions";

/// Typed UI bridge shared with language-server callbacks.
#[derive(Clone)]
pub struct HostUi {
    ui: UiClient,
    revision: Arc<AtomicU64>,
}

impl HostUi {
    pub fn new(caller: Arc<dyn ee_plugin_api::context::ServiceCaller>) -> Self {
        Self { ui: UiClient::new(caller), revision: Arc::new(AtomicU64::new(1)) }
    }

    fn next_revision(&self) -> u64 {
        let revision = self.revision.fetch_add(1, Ordering::SeqCst);
        revision.max(1)
    }

    pub fn alert(&self, message: impl Into<String>) {
        let _ = self.ui.show_overlay(
            contribution(HOVER_OVERLAY),
            self.next_revision(),
            true,
            json!({ "text": message.into() }),
        );
    }

    /// Idle scheduling is driven by the runtime tick loop.
    pub fn schedule_idle(&self, _view: ViewId) {}

    pub fn add_status_item(&self, _view: ViewId, key: &str, value: &str, alignment: &str) {
        let _ = self.ui.set_status(StatusItem {
            key: StatusItemKey::new(key),
            text: value.to_owned(),
            alignment: if alignment == "right" {
                StatusAlignment::Right
            } else {
                StatusAlignment::Left
            },
            priority: 0,
        });
    }

    pub fn update_status_item(&self, _view: ViewId, key: &str, value: &str) {
        self.add_status_item(ViewId(0), key, value, "left");
    }

    pub fn remove_status_item(&self, _view: ViewId, key: &str) {
        let _ = self.ui.remove_status(key);
    }

    pub fn show_completions(&self, _view: ViewId, items: &[CompletionSuggestion]) {
        let items = items
            .iter()
            .map(|item| {
                json!({
                    "label": item.label,
                    "detail": item.detail,
                    "insert_text": item.insert_text,
                })
            })
            .collect();
        let _ = self.ui.show_picker(contribution(COMPLETION_PICKER), self.next_revision(), items);
    }

    pub fn show_locations(&self, _view: ViewId, title: &str, locations: &[NavigationTarget]) {
        let items = locations
            .iter()
            .map(|location| {
                json!({
                    "title": format!("{}:{}:{}", title, location.line + 1, location.column + 1),
                    "path": location.path,
                    "line": location.line,
                    "column": location.column,
                })
            })
            .collect();
        let _ = self.ui.show_picker(contribution(LOCATIONS_PICKER), self.next_revision(), items);
    }

    pub fn show_symbols(&self, _view: ViewId, title: &str, symbols: &[SymbolItem]) {
        let picker =
            if title.contains("workspace") { WORKSPACE_SYMBOLS_PICKER } else { SYMBOLS_PICKER };
        let items = symbols.iter().map(symbol_item).collect();
        let _ = self.ui.show_picker(contribution(picker), self.next_revision(), items);
    }

    pub fn show_code_actions(&self, _view: ViewId, titles: &[String]) {
        let items = titles.iter().map(|title| json!({ "title": title })).collect();
        let _ = self.ui.show_picker(contribution(CODE_ACTIONS_PICKER), self.next_revision(), items);
    }

    pub fn set_diagnostics(
        &self,
        view: ViewId,
        rev: u64,
        diagnostics: &[ee_plugin_api::model::Diagnostic],
    ) {
        let spans = diagnostics
            .iter()
            .map(|diagnostic| ee_plugin_api::effect::DecorationSpan {
                start: diagnostic.range.start,
                end: diagnostic.range.end,
                style: ee_plugin_api::effect::StyleRole::new(match diagnostic.severity {
                    ee_plugin_api::model::DiagnosticSeverity::Error => "diagnostic.error",
                    ee_plugin_api::model::DiagnosticSeverity::Warning => "diagnostic.warning",
                    ee_plugin_api::model::DiagnosticSeverity::Information => {
                        "diagnostic.information"
                    }
                    ee_plugin_api::model::DiagnosticSeverity::Hint => "diagnostic.hint",
                }),
                text: Some(diagnostic.message.clone()),
            })
            .collect();
        let _ =
            self.ui.submit(vec![Effect::SetDecorations(ee_plugin_api::effect::DecorationPatch {
                view_id: view,
                rev,
                decoration_kind: "diagnostic".to_owned(),
                spans,
            })]);
    }

    pub fn emit_agent_tool_result(&self, kind: &str, payload: Value) {
        let _ = self.ui.submit(vec![Effect::EmitAgentToolResult(
            ee_plugin_api::effect::AgentToolResult {
                call_id: kind.to_owned(),
                is_error: false,
                content: payload,
            },
        )]);
    }

    pub fn show_agent_tool_result(&self, _view: ViewId, kind: &str, payload: Value) {
        self.emit_agent_tool_result(kind, payload);
    }

    pub fn submit_effects(&self, effects: Vec<Effect>) -> Result<(), PluginError> {
        self.ui.submit(effects)
    }
}

fn contribution(id: &str) -> ContributionId {
    ContributionId::new(id).expect("static LSP contribution id")
}

fn symbol_item(symbol: &SymbolItem) -> Value {
    json!({
        "name": symbol.name,
        "kind": symbol.kind,
        "path": symbol.path,
        "line": symbol.line,
        "column": symbol.column,
    })
}
