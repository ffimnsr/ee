//! Typed editor events.
//!
//! `NAME` strings are the v2 wire contract and match
//! `docs/upgrades/plugin-system-cordis.md` §7.1. In-process plugins register
//! typed listeners; native plugins receive the same event name with a JSON
//! payload built from these structs.

use ee_plugin_api::effect::{DecorationPatch, StatusItem};
use ee_plugin_api::event::Event;
use ee_plugin_api::ids::{CommandId, ContributionId};
use ee_plugin_api::model::{BufferId, DocumentDelta, LanguageId, SelectionRange, ViewId};
use serde::Serialize;
use serde_json::Value;

/// Emitted before a committed delta is applied by core.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentBeforeEdit {
    pub view_id: ViewId,
    pub rev: u64,
    pub edit_type: String,
}

impl Event for DocumentBeforeEdit {
    const NAME: &'static str = "editor.document.before_edit";
}

/// Emitted once per committed delta.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentChanged {
    pub view_id: ViewId,
    pub buffer_id: BufferId,
    pub rev: u64,
    pub new_len: usize,
    pub new_line_count: usize,
    pub undo_group: Option<usize>,
    pub author: String,
    pub edit_type: String,
    /// `None` when the delta exceeded the streaming cap; plugins must fetch a
    /// snapshot instead.
    pub delta: Option<DocumentDelta>,
}

impl Event for DocumentChanged {
    const NAME: &'static str = "editor.document.changed";
}

/// Emitted after a buffer snapshot has been written to disk.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentSaved {
    pub view_id: ViewId,
    pub buffer_id: BufferId,
    pub path: String,
    pub rev: u64,
}

impl Event for DocumentSaved {
    const NAME: &'static str = "editor.document.saved";
}

/// Emitted when a view finishes initialization.
#[derive(Debug, Clone, Serialize)]
pub struct ViewOpened {
    pub view_id: ViewId,
    pub buffer_id: BufferId,
    pub language: LanguageId,
    pub config: Value,
}

impl Event for ViewOpened {
    const NAME: &'static str = "editor.view.opened";
}

/// Emitted when a view closes.
#[derive(Debug, Clone, Serialize)]
pub struct ViewClosed {
    pub view_id: ViewId,
}

impl Event for ViewClosed {
    const NAME: &'static str = "editor.view.closed";
}

/// Emitted after selection truth changes for a view.
#[derive(Debug, Clone, Serialize)]
pub struct SelectionChanged {
    pub view_id: ViewId,
    pub selections: Vec<SelectionRange>,
}

impl Event for SelectionChanged {
    const NAME: &'static str = "editor.selection.changed";
}

/// Emitted when a view's language changes.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageChanged {
    pub view_id: ViewId,
    pub old: LanguageId,
    pub new: LanguageId,
}

impl Event for LanguageChanged {
    const NAME: &'static str = "editor.language.changed";
}

/// Emitted when configuration for a scope changes.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigChanged {
    pub scope: String,
    pub changes: Value,
}

impl Event for ConfigChanged {
    const NAME: &'static str = "editor.config.changed";
}

/// Waterfall event consulted before command dispatch.
#[derive(Debug, Clone, Serialize)]
pub struct CommandBefore {
    pub id: CommandId,
    pub view_id: ViewId,
    pub args: Value,
}

impl Event for CommandBefore {
    const NAME: &'static str = "editor.command.before";
}

/// Emitted after a command was invoked.
#[derive(Debug, Clone, Serialize)]
pub struct CommandAfter {
    pub id: CommandId,
    pub view_id: ViewId,
    pub effects: usize,
}

impl Event for CommandAfter {
    const NAME: &'static str = "editor.command.after";
}

/// Serial event asking plugins for decoration patches.
#[derive(Debug, Clone, Serialize)]
pub struct DecorationsCollect {
    pub view_id: ViewId,
    pub rev: u64,
}

impl Event for DecorationsCollect {
    const NAME: &'static str = "editor.decorations.collect";
}

/// Accumulator passed to `editor.decorations.collect` listeners.
#[derive(Debug, Default)]
pub struct DecorationCollector {
    pub patches: Vec<DecorationPatch>,
}

/// Serial event asking plugins for status items.
#[derive(Debug, Clone, Serialize)]
pub struct StatusCollect {
    pub view_id: ViewId,
}

impl Event for StatusCollect {
    const NAME: &'static str = "editor.status.collect";
}

/// Accumulator passed to `editor.status.collect` listeners.
#[derive(Debug, Default)]
pub struct StatusCollector {
    pub items: Vec<StatusItem>,
}

/// Serial event requesting dynamic UI content from a provider.
#[derive(Debug, Clone, Serialize)]
pub struct UiRender {
    pub view_id: ViewId,
    pub id: ContributionId,
    pub revision: u64,
}

impl Event for UiRender {
    const NAME: &'static str = "editor.ui.render";
}

/// Serial event collecting declarative UI contributions at activation/reload.
#[derive(Debug, Clone, Serialize)]
pub struct UiContributions {
    pub host: String,
}

impl Event for UiContributions {
    const NAME: &'static str = "editor.ui.contributions";
}

/// Emitted once before the host shuts plugins down.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Shutdown;

impl Event for Shutdown {
    const NAME: &'static str = "editor.shutdown";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_stable() {
        assert_eq!(DocumentBeforeEdit::NAME, "editor.document.before_edit");
        assert_eq!(DocumentChanged::NAME, "editor.document.changed");
        assert_eq!(DocumentSaved::NAME, "editor.document.saved");
        assert_eq!(ViewOpened::NAME, "editor.view.opened");
        assert_eq!(ViewClosed::NAME, "editor.view.closed");
        assert_eq!(SelectionChanged::NAME, "editor.selection.changed");
        assert_eq!(LanguageChanged::NAME, "editor.language.changed");
        assert_eq!(ConfigChanged::NAME, "editor.config.changed");
        assert_eq!(CommandBefore::NAME, "editor.command.before");
        assert_eq!(CommandAfter::NAME, "editor.command.after");
        assert_eq!(DecorationsCollect::NAME, "editor.decorations.collect");
        assert_eq!(StatusCollect::NAME, "editor.status.collect");
        assert_eq!(UiRender::NAME, "editor.ui.render");
        assert_eq!(UiContributions::NAME, "editor.ui.contributions");
        assert_eq!(Shutdown::NAME, "editor.shutdown");
    }

    #[test]
    fn document_changed_serializes_delta() {
        let event = DocumentChanged {
            view_id: ViewId(1),
            buffer_id: BufferId(2),
            rev: 3,
            new_len: 5,
            new_line_count: 2,
            undo_group: Some(1),
            author: "core".to_owned(),
            edit_type: "insert".to_owned(),
            delta: Some(DocumentDelta {
                ops: vec![ee_plugin_api::model::DeltaOp::Insert { offset: 0, text: "hi".into() }],
            }),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["view_id"], 1);
        assert_eq!(value["delta"]["ops"][0]["op"], "insert");
    }
}
