//! Service behavior tests against the in-memory editor.

use std::sync::Arc;

use ee_plugin_api::context::ContributionSink;
use ee_plugin_api::effect::{ScrollRequest, SelectionRequest};
use ee_plugin_api::ids::CommandId;
use ee_plugin_api::model::{
    BufferId, EditRequest, Range, SelectionRange, SymbolItem, TextEdit, ViewId,
};
use ee_plugin_api::service::Service;

use super::{RecordingNotifier, editor, host, single_host};
use crate::plugin_runtime::core_api::{FoldRange, SyntaxScope};
use crate::plugin_runtime::services::document::{DocumentEditService, DocumentService, MAX_EDITS};
use crate::plugin_runtime::services::selection::SelectionService;
use crate::plugin_runtime::services::syntax::SyntaxService;
use crate::plugin_runtime::services::views::ViewsService;

fn document_services()
-> (Arc<super::super::test_support::MemoryEditor>, DocumentService, DocumentEditService) {
    let editor = editor();
    (Arc::clone(&editor), DocumentService::new(editor.clone()), DocumentEditService::new(editor))
}

fn insert(view: ViewId, rev: u64, text: &str) -> EditRequest {
    EditRequest {
        view_id: view,
        rev,
        edits: vec![TextEdit { range: Range { start: 0, end: 0 }, new_text: text.to_owned() }],
        priority: 0,
        after_cursor: false,
        undo_group: Some(3),
        author: "test".to_owned(),
    }
}

#[test]
fn document_reads_are_bounded() {
    let (_editor, document, _edit) = document_services();
    let snapshot = document.snapshot(BufferId(1)).unwrap();
    assert_eq!(snapshot.len, 5);
    assert_eq!(document.line_count(BufferId(1)).unwrap(), 1);
    assert_eq!(document.text_range(BufferId(1), Range { start: 1, end: 4 }).unwrap(), "ell");
    assert_eq!(document.document_text(BufferId(1), 3).unwrap(), "hel");
    let error = document.text_range(BufferId(1), Range { start: 0, end: 99 }).unwrap_err();
    assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Schema);
}

#[test]
fn edit_applies_with_undo_group_and_revision() {
    let (editor, _document, edit) = document_services();
    let ack = edit.apply(insert(ViewId(1), 0, "X")).unwrap();
    assert_eq!(ack.rev, 1);
    let last = editor.last_edit().unwrap();
    assert_eq!(last.undo_group, Some(3));
    assert_eq!(document_text(&editor), "Xhello");
}

fn document_text(editor: &super::super::test_support::MemoryEditor) -> String {
    use crate::plugin_runtime::core_api::EditorAccess;
    editor.read_document(BufferId(1), usize::MAX).unwrap()
}

#[test]
fn stale_revision_is_rejected() {
    let (_editor, _document, edit) = document_services();
    let error = edit.apply(insert(ViewId(1), 99, "x")).unwrap_err();
    assert!(error.to_string().contains("stale revision"));
}

#[test]
fn oversized_edit_batch_is_rejected() {
    let (_editor, _document, edit) = document_services();
    let mut request = insert(ViewId(1), 0, "x");
    request.edits = (0..=MAX_EDITS)
        .map(|index| TextEdit {
            range: Range { start: index, end: index },
            new_text: String::new(),
        })
        .collect();
    let error = edit.apply(request).unwrap_err();
    assert!(error.to_string().contains("edit count"));
}

#[test]
fn vlf_mutation_is_rejected() {
    let (editor, _document, edit) = document_services();
    editor.set_vlf(ViewId(1), true);
    let error = edit.apply(insert(ViewId(1), 0, "x")).unwrap_err();
    assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Permission);
}

#[test]
fn views_expose_siblings_and_scroll() {
    let editor = editor();
    editor.add_view(BufferId(1), ViewId(2));
    let views = ViewsService::new(editor.clone());
    assert_eq!(views.views_for_buffer(BufferId(1)), vec![ViewId(1), ViewId(2)]);
    assert_eq!(views.active_view(), Some(ViewId(1)));
    assert_eq!(views.viewport(ViewId(1)).unwrap().height, 10);
    views.scroll_to(ScrollRequest { view_id: ViewId(1), line: 4, column: 0 }).unwrap();
    assert_eq!(views.viewport(ViewId(1)).unwrap().first_line, 4);
    views.reveal(ViewId(2), 2, 0).unwrap();
    assert_eq!(views.viewport(ViewId(2)).unwrap().first_line, 2);
}

#[test]
fn selection_is_clamped_through_existing_view_api() {
    let editor = editor();
    let selection = SelectionService::new(editor.clone());
    selection
        .set(SelectionRequest {
            view_id: ViewId(1),
            rev: 0,
            selections: vec![SelectionRange { start: 1, end: 99 }],
        })
        .unwrap();
    assert_eq!(editor.selection_for(ViewId(1)), vec![SelectionRange { start: 1, end: 5 }]);
    assert!(selection.get(ViewId(1)).unwrap().len() == 1);
    let error = selection
        .set(SelectionRequest {
            view_id: ViewId(1),
            rev: 0,
            selections: vec![SelectionRange { start: 4, end: 2 }],
        })
        .unwrap_err();
    assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Schema);
}

#[test]
fn syntax_queries_are_capped_and_typed() {
    let editor = editor();
    editor.set_folds(vec![FoldRange { header_line: 0, body_start: 1, body_end: 2 }]);
    editor.set_symbols(vec![SymbolItem {
        name: "main".to_owned(),
        kind: "function".to_owned(),
        path: "src/main.rs".to_owned(),
        line: 1,
        column: 0,
        children: Vec::new(),
    }]);
    editor.set_scopes(vec![SyntaxScope { start: 0, end: 5, scope: "syntax.keyword".to_owned() }]);
    editor.add_grammar("rust");
    let syntax = SyntaxService::new(editor);
    assert_eq!(syntax.folds(ViewId(1)).unwrap()[0].header_line, 0);
    assert_eq!(syntax.symbols(ViewId(1)).unwrap()[0].name, "main");
    assert_eq!(syntax.scopes(ViewId(1)).unwrap()[0].scope, "syntax.keyword");
    assert!(syntax.grammar_available("rust"));
    assert!(!syntax.grammar_available("cobol"));
    assert_eq!(SyntaxService::ID, "editor.syntax@1");
}

#[test]
fn language_contributions_are_published_and_reload_notified() {
    let (_dir, host, notifier) = single_host();
    host.start().unwrap();
    let _ = host
        .contributions()
        .contribute(ee_plugin_api::contribution::Contribution::Language(
            ee_plugin_api::contribution::LanguageContribution {
                name: "demo".to_owned(),
                extensions: vec!["demo".to_owned()],
                filenames: Vec::new(),
                globs: Vec::new(),
                first_line_match: None,
                scope: "source.demo".to_owned(),
                default_config: None,
            },
        ))
        .unwrap();
    assert_eq!(host.language_contributions().len(), 1);
    let _ = host.contributions().contribute(ee_plugin_api::contribution::Contribution::LspServer(
        ee_plugin_api::contribution::LspServerContribution {
            id: "demo-lsp".to_owned(),
            language: "demo".to_owned(),
            command: "demo-lsp".to_owned(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            initialization_options: None,
        },
    ));
    assert_eq!(host.lsp_server_contributions().len(), 1);
    let before = notifier.batches.lock().unwrap().len();
    host.reload_paths(&[]).unwrap();
    assert!(notifier.batches.lock().unwrap().len() > before);
    assert!(host.is_active("fake"));
}

#[test]
fn blocked_and_disabled_states_are_reported() {
    let notifier = Arc::new(RecordingNotifier::default());
    let (_dir, host) = host(
        vec![("blocked".to_owned(), false, Arc::new(std::sync::Mutex::new(Vec::new())))],
        editor(),
        notifier.clone(),
    );
    host.start().unwrap();
    assert!(host.status().plugins.iter().any(|plugin| plugin.name == "blocked"));
    let statuses = notifier.statuses.lock().unwrap();
    assert!(!statuses.is_empty());
}

#[test]
fn command_service_is_registered() {
    let (_dir, host, _notifier) = single_host();
    host.start().unwrap();
    let services = host.services();
    let registry = services.read().unwrap();
    assert!(registry.contains(&"editor.commands@1".parse().unwrap()));
    assert!(host.commands().contains(&CommandId::new("fake.run").unwrap()));
}
