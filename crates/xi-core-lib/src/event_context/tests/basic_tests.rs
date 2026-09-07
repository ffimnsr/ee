//! Event-context tests: basic.
use super::*;

#[test]
fn vlf_exact_line_byte_refuses_approximate_line_anchor() {
    use crate::text_store::{LineLookup, LogicalLine};

    let (store, _file) = vlf_store_from(b"a\nb\nc\nd\ne", 4);
    store.scan_page_at(0).unwrap();

    assert!(matches!(store.line_to_byte(LogicalLine(4)), LineLookup::Approximate(_)));
    assert!(matches!(vlf_exact_line_byte(&store, 4), Err(LineLookup::Approximate(_))));
}

#[test]
fn vlf_tail_count_uses_exact_logical_line_count() {
    let (store, _file) = vlf_store_from(b"alpha\nbeta\ngamma", 8);
    assert_eq!(vlf_exact_logical_line_count(&store), Some(3));

    let (store, _file) = vlf_store_from(b"alpha\nbeta\n", 8);
    assert_eq!(vlf_exact_logical_line_count(&store), Some(3));
}

#[test]
fn smoke_test() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
    ctx.do_edit(EditNotification::Insert { chars: " ".into() });
    ctx.do_edit(EditNotification::Insert { chars: "world".into() });
    ctx.do_edit(EditNotification::Insert { chars: "!".into() });
    assert_eq!(harness.debug_render(), "hello world!|");
    ctx.do_edit(EditNotification::MoveWordLeft);
    ctx.do_edit(EditNotification::InsertNewline);
    assert_eq!(harness.debug_render(), "hello \n|world!");
    ctx.do_edit(EditNotification::MoveWordRightAndModifySelection);
    assert_eq!(harness.debug_render(), "hello \n[world|]!");
    ctx.do_edit(EditNotification::Insert { chars: "friends".into() });
    assert_eq!(harness.debug_render(), "hello \nfriends|!");
}

#[test]
fn language_changed_invalidates_view_for_syntax_refresh() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();
    crate::runtime_loader::with_default_runtime_loader_mut(|loader| {
        for kind in [
            crate::runtime_loader::RuntimeQueryKind::Highlights,
            crate::runtime_loader::RuntimeQueryKind::Injections,
        ] {
            let _ = loader.compile_query_kind("rust", kind);
        }
    });
    let harness = ContextHarness::new("let x = 1;\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.language_changed(&LanguageId::from("rust"));

    let notifications = harness.take_notifications();
    assert!(notifications.iter().any(|(method, _)| method == "language_changed"));

    let syntax_refresh = notifications.iter().any(|(method, params)| {
        method == "update"
            && params["update"]["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["lines"].as_array().is_some_and(|lines| {
                        lines.iter().any(|line| line.get("syntax_spans").is_some())
                    })
                })
            })
    });

    assert!(syntax_refresh, "language change should force a rendered syntax refresh");
}

#[test]
fn get_selections_returns_current_selection_ranges() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("hello world");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveToRightEndOfLineAndModifySelection);

    let response: GetSelectionsResponse = serde_json::from_value(
        ctx.do_plugin_cmd_sync(crate::plugins::PluginPid(9), PluginRequest::GetSelections)
            .expect("selection request should succeed"),
    )
    .expect("selection response should deserialize");

    assert_eq!(response.selections, vec![SelectionRange { start: 0, end: 11 }]);
}

#[test]
fn typed_plugin_requests_return_structured_results_or_errors() {
    let harness = ContextHarness::new("hello world");
    let mut ctx = harness.make_context();

    let diagnostics = ctx
        .do_plugin_cmd_sync(crate::plugins::PluginPid(9), PluginRequest::GetDiagnostics)
        .expect("diagnostics request should succeed");
    let format_err = ctx
        .do_plugin_cmd_sync(
            crate::plugins::PluginPid(9),
            PluginRequest::FormatDocument(FormatDocumentRequest { options: None }),
        )
        .expect_err("formatting should be unsupported");
    let code_actions_err = ctx
        .do_plugin_cmd_sync(
            crate::plugins::PluginPid(9),
            PluginRequest::GetCodeActions(CodeActionRequest {
                range: crate::plugins::rpc::Range { start: 0, end: 5 },
                diagnostics: Vec::new(),
            }),
        )
        .expect_err("code actions should be unsupported");

    let diagnostics: GetDiagnosticsResponse =
        serde_json::from_value(diagnostics).expect("diagnostics response should deserialize");

    assert!(diagnostics.diagnostics.is_empty());
    assert!(matches!(format_err, RemoteError::Custom { code: 501, .. }));
    assert!(matches!(code_actions_err, RemoteError::Custom { code: 501, .. }));
}

#[test]
fn plugin_diagnostics_round_trip_through_view_state() {
    use crate::plugins::rpc::PluginNotification;

    let harness = ContextHarness::new("hello world");
    let mut ctx = harness.make_context();

    ctx.do_plugin_cmd(
        crate::plugins::PluginPid(9),
        PluginNotification::UpdateDiagnostics {
            diagnostics: vec![Diagnostic {
                range: crate::plugins::rpc::Range { start: 1, end: 4 },
                severity: DiagnosticSeverity::Warning,
                message: String::from("warn"),
                source: Some(String::from("lsp")),
                code: Some(String::from("W1")),
            }],
        },
    );

    let diagnostics: GetDiagnosticsResponse = serde_json::from_value(
        ctx.do_plugin_cmd_sync(crate::plugins::PluginPid(9), PluginRequest::GetDiagnostics)
            .expect("diagnostics request should succeed"),
    )
    .expect("diagnostics response should deserialize");

    assert_eq!(diagnostics.diagnostics.len(), 1);
    assert_eq!(diagnostics.diagnostics[0].message, "warn");
    assert_eq!(diagnostics.diagnostics[0].severity, DiagnosticSeverity::Warning);
}
