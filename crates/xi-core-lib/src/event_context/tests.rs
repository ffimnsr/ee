use super::*;
use crate::config::ConfigManager;
use crate::core::dummy_weak_core;
use crate::line_offset::LineOffset;
use crate::object::SyntaxNavigationTarget;
use crate::plugin_rpc::PluginRequest;
use crate::plugins::rpc::{
    CodeActionRequest, Diagnostic, DiagnosticSeverity, FormatDocumentRequest,
    GetDiagnosticsResponse, GetSelectionsResponse, SelectionRange,
};
use crate::rpc::SelectionModifier;
use crate::selection::SelRegion;
use crate::tabs::BufferId;
use crate::text_store::DocumentMode;
use crate::text_store::{LineLookup, LogicalLine, TextStore};
use crate::vlf::store::VlfStore;
use serde_json::{Value, json};
use std::io::Write;
use std::mem;
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;
use xi_rope::Interval;
use xi_rpc::{Callback, Error as RpcError, Peer, RemoteError, RequestId};

#[derive(Clone, Default)]
struct RecordingPeer {
    notifications: Arc<Mutex<Vec<(String, Value)>>>,
}

impl RecordingPeer {
    fn take_notifications(&self) -> Vec<(String, Value)> {
        let mut notifications = self.notifications.lock().expect("recording peer poisoned");
        mem::take(&mut *notifications)
    }
}

impl Peer for RecordingPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(self.clone())
    }

    fn send_rpc_notification(&self, method: &str, params: &Value) {
        self.notifications
            .lock()
            .expect("recording peer poisoned")
            .push((method.to_owned(), params.clone()));
    }

    fn send_rpc_request_async(
        &self,
        _method: &str,
        _params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        f.call(Ok(Value::Null));
        RequestId::Number(0)
    }

    fn send_rpc_request(&self, _method: &str, _params: &Value) -> Result<Value, RpcError> {
        Ok(Value::Null)
    }

    fn send_rpc_request_timeout(
        &self,
        _method: &str,
        _params: &Value,
        _timeout: std::time::Duration,
    ) -> Result<Value, RpcError> {
        Ok(Value::Null)
    }

    fn cancel_rpc_request(&self, _id: RequestId) -> bool {
        false
    }

    fn request_is_pending(&self) -> bool {
        false
    }

    fn schedule_idle(&self, _token: usize) {}

    fn schedule_timer(&self, _time: std::time::Instant, _token: usize) {}

    fn cancel_timer(&self, _token: usize) -> bool {
        false
    }

    fn request_shutdown(&self) {}
}

struct ContextHarness {
    view: RefCell<View>,
    editor: RefCell<Editor>,
    client: Client,
    peer: RecordingPeer,
    core_ref: WeakXiCore,
    kill_ring: RefCell<Rope>,
    width_cache: RefCell<WidthCache>,
    config_manager: ConfigManager,
}

impl ContextHarness {
    fn new<S: AsRef<str>>(s: S) -> Self {
        let view_id = ViewId(1);
        let buffer_id = BufferId(2);
        let mut config_manager = ConfigManager::new(None, None);
        let config = config_manager.add_buffer(buffer_id, None);
        let view = RefCell::new(View::new(view_id, buffer_id));
        let editor = RefCell::new(Editor::with_text(s));
        let peer = RecordingPeer::default();
        let client = Client::new(Box::new(peer.clone()));
        let core_ref = dummy_weak_core();
        let kill_ring = RefCell::new(Rope::from(""));
        let width_cache = RefCell::new(WidthCache::new());
        let harness = ContextHarness {
            view,
            editor,
            client,
            peer,
            core_ref,
            kill_ring,
            width_cache,
            config_manager,
        };
        harness.make_context().view_init();
        harness.make_context().finish_init(&config);
        harness
    }

    fn debug_render(&self) -> String {
        let b = self.editor.borrow();
        let mut text: String = b.get_buffer().into();
        let v = self.view.borrow();
        for sel in v.sel_regions().iter().rev() {
            if sel.end == sel.start {
                text.insert(sel.end, '|');
            } else if sel.end > sel.start {
                text.insert_str(sel.end, "|]");
                text.insert(sel.start, '[');
            } else {
                text.insert(sel.start, ']');
                text.insert_str(sel.end, "[|");
            }
        }
        text
    }

    fn take_notifications(&self) -> Vec<(String, Value)> {
        self.peer.take_notifications()
    }

    fn make_context(&self) -> EventContext<'_> {
        let view_id = ViewId(1);
        let buffer_id = self.view.borrow().get_buffer_id();
        let config = self.config_manager.get_buffer_config(buffer_id);
        let language = self.config_manager.get_buffer_language(buffer_id);
        EventContext {
            view_id,
            buffer_id,
            view: &self.view,
            editor: &self.editor,
            config: &config.items,
            language,
            info: None,
            siblings: Vec::new(),
            plugins: Vec::new(),
            client: &self.client,
            kill_ring: &self.kill_ring,
            width_cache: &self.width_cache,
            weak_core: &self.core_ref,
        }
    }
}

fn vlf_store_from(content: &[u8], page_size: u64) -> (crate::vlf::store::VlfStore, NamedTempFile) {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(content).unwrap();
    file.flush().unwrap();
    let store =
        crate::vlf::store::VlfStore::open_with_config(file.path(), page_size, 1024 * 1024).unwrap();
    (store, file)
}

// ── Tests ──

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

#[test]
fn test_gestures() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveDown);
    ctx.do_edit(EditNotification::MoveDown);
    ctx.do_edit(EditNotification::MoveToEndOfParagraph);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that has three\n\
        lines.|"
    );

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    ctx.do_edit(EditNotification::MoveToEndOfParagraphAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        [this is a string|]\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveToEndOfParagraph);
    ctx.do_edit(EditNotification::MoveToBeginningOfParagraphAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        [|this is a string]\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        |this is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that |has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveToRightEndOfLineAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: MultiWordSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        [lines|]."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        li|nes."
    );

    ctx.do_edit(EditNotification::MoveToLeftEndOfLine);
    assert_eq!(
        harness.debug_render(),
        "\
        |this is a string\n\
        |that has three\n\
        |lines."
    );

    ctx.do_edit(EditNotification::MoveWordRight);
    assert_eq!(
        harness.debug_render(),
        "\
        this| is a string\n\
        that| has three\n\
        lines|."
    );

    ctx.do_edit(EditNotification::MoveToLeftEndOfLineAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        [|this] is a string\n\
        [|that] has three\n\
        [|lines]."
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::MoveToRightEndOfLine);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string|\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: MultiLineSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string|\n\
        that has three\n\
        [lines.|]"
    );

    ctx.do_edit(EditNotification::SelectAll);
    assert_eq!(
        harness.debug_render(),
        "\
        [this is a string\n\
        that has three\n\
        lines.|]"
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::AddSelectionAbove);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that h|as three\n\
        lines.|"
    );

    ctx.do_edit(EditNotification::MoveRight);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that ha|s three\n\
        lines.|"
    );

    ctx.do_edit(EditNotification::MoveLeft);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that h|as three\n\
        lines|."
    );
}

#[test]
fn goto_line_out_of_bounds_alerts_instead_of_panicking() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("hello\nworld\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::GotoLine { line: 99 });

    let notifications = harness.take_notifications();
    let alert = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected alert notification");
    assert_eq!(alert.1["msg"], json!("goto_line: line 99 beyond last line 3"));
    assert_eq!(harness.debug_render(), "|hello\nworld\n");
}

#[test]
fn gesture_out_of_bounds_alerts_instead_of_panicking() {
    use crate::rpc::{EditNotification, GestureType::PointSelect};

    let harness = ContextHarness::new("hello\nworld\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 99, col: 0, ty: PointSelect });

    let notifications = harness.take_notifications();
    let alert = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected alert notification");
    assert_eq!(alert.1["msg"], json!("gesture: line 99 beyond last line 3"));
    assert_eq!(harness.debug_render(), "|hello\nworld\n");
}

#[test]
fn toggle_line_comment_edits_current_line() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("fn main() {}\n");
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::ToggleLineComment);

    assert_eq!(harness.debug_render(), "// |fn main() {}\n");
}

#[test]
fn toggle_block_comment_edits_current_line_when_language_has_no_line_comment() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("div { color: red; }\n");
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("CSS");

    ctx.do_edit(EditNotification::ToggleBlockComment);

    assert_eq!(harness.debug_render(), "/* |div { color: red; } */\n");
}

#[test]
fn reindent_unsupported_language_avoids_background_task_and_panics() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("<div>\n<span>hi</span>\n</div>\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("HTML");

    ctx.do_edit(EditNotification::Reindent);

    assert!(!harness.editor.borrow().whole_scan_task.is_in_progress());
    let notifications = harness.take_notifications();
    assert!(notifications.iter().all(|(method, _)| method != "alert"));
    assert_eq!(harness.debug_render(), "|<div>\n<span>hi</span>\n</div>\n");
}

#[test]
fn goto_column_uses_display_width_and_can_extend_selection() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("日本x");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::GotoColumn { display_col: 2, modify_selection: false });
    assert_eq!(harness.debug_render(), "日|本x");

    ctx.do_edit(EditNotification::GotoColumn { display_col: 0, modify_selection: false });
    ctx.do_edit(EditNotification::GotoColumn { display_col: 2, modify_selection: true });
    assert_eq!(harness.debug_render(), "[日|]本x");
}

#[test]
fn goto_column_uses_logical_column_even_when_view_is_wrapped() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("abcdef");
    {
        let text = harness.editor.borrow().get_buffer().clone();
        harness.view.borrow_mut().debug_force_rewrap_cols(&text, 2);
    }

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::GotoColumn { display_col: 4, modify_selection: false });

    assert_eq!(harness.debug_render(), "abcd|ef");
}

#[test]
fn goto_next_paragraph_moves_to_next_nonblank_block() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta\n\ncharlie\n\ndelta\n");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::GotoNextParagraph);
    assert_eq!(harness.debug_render(), "alpha\nbeta\n\n|charlie\n\ndelta\n");

    ctx.do_edit(EditNotification::GotoNextParagraph);
    assert_eq!(harness.debug_render(), "alpha\nbeta\n\ncharlie\n\n|delta\n");
}

#[test]
fn goto_prev_paragraph_moves_to_previous_nonblank_block() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\n\nbeta\ngamma\n\ndelta\n");
    {
        let text = harness.editor.borrow().get_buffer().clone();
        harness.view.borrow_mut().set_selection(
            &text,
            crate::selection::Selection::new_simple(SelRegion::caret(
                crate::line_offset::LogicalLines.offset_of_line(&text, 5),
            )),
        );
    }
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::GotoPrevParagraph);
    assert_eq!(harness.debug_render(), "alpha\n\n|beta\ngamma\n\ndelta\n");

    ctx.do_edit(EditNotification::GotoPrevParagraph);
    assert_eq!(harness.debug_render(), "|alpha\n\nbeta\ngamma\n\ndelta\n");
}

#[test]
fn add_newline_commands_insert_blank_lines_around_current_line() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::AddNewlineBelow);
    assert_eq!(harness.debug_render(), "alpha\n|\nbeta");

    let harness = ContextHarness::new("alpha\nbeta");
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::MoveDown);
    ctx.do_edit(EditNotification::AddNewlineAbove);
    assert_eq!(harness.debug_render(), "alpha\n|\nbeta");
}

#[test]
fn join_selections_joins_current_and_next_line() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("abc\n    def\nxyz");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::JoinSelections { select_space: false });

    assert_eq!(harness.debug_render(), "abc def|xyz");
}

#[test]
fn join_selections_space_selects_inserted_space() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("abc\n    def\nxyz");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::JoinSelections { select_space: true });

    assert_eq!(harness.debug_render(), "abc[ |]defxyz");
}

#[test]
fn join_selections_handles_multiple_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("aa\n  bb\ncc\n  dd\nend");
    {
        let text = harness.editor.borrow().get_buffer().clone();
        let mut selection = crate::selection::Selection::new();
        selection.add_region(SelRegion::new(text.offset_of_line(0), text.offset_of_line(2)));
        selection.add_region(SelRegion::new(text.offset_of_line(2), text.offset_of_line(4)));
        harness.view.borrow_mut().set_selection(&text, selection);
    }

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::JoinSelections { select_space: true });

    assert_eq!(harness.debug_render(), "aa[ |]bbcc[ |]ddend");
}

#[test]
fn preview_filter_selections_keeps_matching_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta alps");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
            SelectionRange { start: 11, end: 15 },
        ],
    });

    let filtered =
        ctx.preview_filter_selections("^a", false).expect("filter preview should succeed");

    assert_eq!(
        filtered,
        vec![SelectionRange { start: 0, end: 5 }, SelectionRange { start: 11, end: 15 }]
    );
}

#[test]
fn preview_filter_selections_removes_matching_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta alps");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
            SelectionRange { start: 11, end: 15 },
        ],
    });

    let filtered =
        ctx.preview_filter_selections("^a", true).expect("filter preview should succeed");

    assert_eq!(filtered, vec![SelectionRange { start: 6, end: 10 }]);
}

#[test]
fn set_selections_replaces_current_selection_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta alps");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![SelectionRange { start: 6, end: 10 }],
    });

    assert_eq!(harness.debug_render(), "alpha [beta|] alps");
}

#[test]
fn extend_line_below_expands_to_next_line_start() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 2, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::ExtendLineBelow { count: 1 });

    assert_eq!(harness.debug_render(), "[alpha\n|]beta\ngamma");
}

#[test]
fn extend_line_above_selects_current_line_then_previous_line() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 2, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::ExtendLineAbove);
    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");

    ctx.do_edit(EditNotification::ExtendLineAbove);
    assert_eq!(harness.debug_render(), "[alpha\nbeta\n|]gamma");
}

#[test]
fn select_line_commands_adjust_active_edge_from_anchor() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 2, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::SelectLineBelow);
    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");

    ctx.do_edit(EditNotification::SelectLineBelow);
    assert_eq!(harness.debug_render(), "alpha\n[beta\ngamma|]");

    ctx.do_edit(EditNotification::SelectLineAbove);
    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");

    ctx.do_edit(EditNotification::SelectLineAbove);
    assert_eq!(harness.debug_render(), "[|alpha\nbeta\n]gamma");
}

#[test]
fn extend_to_line_bounds_selects_entire_lines() {
    use crate::rpc::{EditNotification, GestureType, SelectionGranularity};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 1, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::Gesture {
        line: 1,
        col: 2,
        ty: GestureType::SelectExtend { granularity: SelectionGranularity::Point },
    });
    ctx.do_edit(EditNotification::ExtendToLineBounds);

    assert_eq!(harness.debug_render(), "[alpha\nbeta\n|]gamma");
}

#[test]
fn move_word_start_uses_backend_vim_semantics() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveWordStart {
        forward: true,
        long_word: false,
        modify_selection: false,
    });
    assert_eq!(harness.debug_render(), "alpha |beta");

    ctx.do_edit(EditNotification::MoveWordStart {
        forward: false,
        long_word: false,
        modify_selection: false,
    });
    assert_eq!(harness.debug_render(), "|alpha beta");
}

#[test]
fn move_word_end_extends_selection_when_requested() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveWordEnd { long_word: false, modify_selection: true });

    assert_eq!(harness.debug_render(), "[alph|]a beta");
}

#[test]
fn find_char_moves_with_inclusive_and_exclusive_variants() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("abcabc");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::FindChar {
        target: 'b',
        forward: true,
        inclusive: true,
        modify_selection: false,
    });
    assert_eq!(harness.debug_render(), "a|bcabc");

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 6, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::FindChar {
        target: 'b',
        forward: false,
        inclusive: false,
        modify_selection: true,
    });
    assert_eq!(harness.debug_render(), "abcab[|c]");
}

#[test]
fn move_to_matching_bracket_handles_nested_multiline_pairs() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("fn main() {\n    (alpha + [beta])\n}\n");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 10, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::MoveToMatchingBracket { modify_selection: false });
    assert_eq!(harness.debug_render(), "fn main() {\n    (alpha + [beta])\n|}\n");

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 4, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::MoveToMatchingBracket { modify_selection: true });
    assert_eq!(harness.debug_render(), "fn main() {\n    [(alpha + [beta]|])\n}\n");
}

#[test]
fn preview_select_chars_respects_multibyte_boundaries() {
    let harness = ContextHarness::new("aéb");
    let mut ctx = harness.make_context();

    let selection = ctx.preview_select_chars(2);

    assert_eq!(selection, vec![SelectionRange { start: 0, end: 3 }]);
}

#[test]
fn preview_selected_text_uses_backend_selection_truth() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![SelectionRange { start: 1, end: 8 }],
    });

    assert_eq!(ctx.preview_selected_text(false), "lpha\nbe");
    assert_eq!(ctx.preview_selected_text(true), "alpha\nbeta\n");
}

#[test]
fn preview_selected_text_uses_text_store_for_constrained_normal() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta");
    harness.editor.borrow_mut().set_document_mode(DocumentMode::ConstrainedNormal);
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![SelectionRange { start: 1, end: 8 }],
    });

    assert_eq!(ctx.preview_selected_text(false), "lpha\nbe");
    assert_eq!(ctx.preview_selected_text(true), "alpha\nbeta\n");
}

#[test]
fn preview_block_text_respects_requested_rectangle() {
    let harness = ContextHarness::new("abcd\nefgh\nijk");
    let mut ctx = harness.make_context();

    assert_eq!(ctx.preview_block_text(0, 2, 1, 3), "bc\nfg\njk\n");
}

#[test]
fn shrink_to_line_bounds_drops_partial_outer_lines() {
    use crate::rpc::{EditNotification, GestureType, SelectionGranularity};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 1, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::Gesture {
        line: 2,
        col: 2,
        ty: GestureType::SelectExtend { granularity: SelectionGranularity::Point },
    });
    ctx.do_edit(EditNotification::ShrinkToLineBounds);

    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");
}

#[test]
fn delete_combining_enclosing_keycaps_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "1\u{E0101}\u{20E3}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 8, ty: PointSelect });

    assert_eq!(harness.debug_render(), "1\u{E0101}\u{20E3}|");

    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // multiple COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{20E3}".into() });
    assert_eq!(harness.debug_render(), "1\u{20E3}\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{20E3}".into() });
    assert_eq!(harness.debug_render(), "\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{20E3}\u{20E3}".into() });
    assert_eq!(harness.debug_render(), "\u{20E3}\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_variation_selector_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{FE0F}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 3, ty: PointSelect });

    assert_eq!(harness.debug_render(), "\u{FE0F}|");

    // Isolated variation selector
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple variation selectors
    ctx.do_edit(EditNotification::Insert { chars: "\u{FE0F}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "\u{FE0F}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{FE0F}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "\u{FE0F}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "\u{E0100}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "\u{E0100}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Multiple variation selectors
    ctx.do_edit(EditNotification::Insert { chars: "#\u{FE0F}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "#\u{FE0F}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "#\u{FE0F}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "#\u{FE0F}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "#\u{E0100}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "#\u{E0100}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "#\u{E0100}\u{E0100}".into() });
    assert_eq!(harness.debug_render(), "#\u{E0100}\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "#\u{E0100}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_emoji_zwj_sequence_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{1F441}\u{200D}\u{1F5E8}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{1F5E8}|");

    // U+200D is ZERO WIDTH JOINER.
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{1F5E8}\u{FE0E}".into() });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{1F5E8}\u{FE0E}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F373}".into() });
    assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}\u{1F373}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F487}\u{200D}\u{2640}".into() });
    assert_eq!(harness.debug_render(), "\u{1F487}\u{200D}\u{2640}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{1F487}\u{200D}\u{2640}\u{FE0F}".into() });
    assert_eq!(harness.debug_render(), "\u{1F487}\u{200D}\u{2640}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert {
        chars: "\u{1F468}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F48B}\u{200D}\u{1F468}".into(),
    });
    assert_eq!(
        harness.debug_render(),
        "\u{1F468}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F48B}\u{200D}\u{1F468}|"
    );
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier can be appended to the first emoji.
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{1F3FB}\u{200D}\u{1F4BC}".into() });
    assert_eq!(harness.debug_render(), "\u{1F469}\u{1F3FB}\u{200D}\u{1F4BC}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // End with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}".into() });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F5E8}".into() });
    assert_eq!(harness.debug_render(), "\u{200D}\u{1F5E8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    ctx.do_edit(EditNotification::Insert { chars: "\u{FE0E}\u{200D}\u{1F5E8}".into() });
    assert_eq!(harness.debug_render(), "\u{FE0E}\u{200D}\u{1F5E8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0E}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{FE0E}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Multiple ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{200D}\u{1F5E8}".into() });
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{200D}\u{1F5E8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}".into() });
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{200D}".into() });
    assert_eq!(harness.debug_render(), "\u{200D}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_flags_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{1F1FA}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 4, ty: PointSelect });

    // Isolated regional indicator symbol
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Odd numbered regional indicator symbols
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{1F1F8}\u{1F1FA}".into() });
    assert_eq!(harness.debug_render(), "\u{1F1FA}\u{1F1F8}\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}\u{1F1F8}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Incomplete sequence. (no tag_term: U+E007E)
    ctx.do_edit(EditNotification::Insert { chars: "a\u{1F3F4}\u{E0067}b".into() });
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E0067}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // No tag_base
    ctx.do_edit(EditNotification::Insert { chars: "\u{E0067}\u{E007F}b".into() });
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E007F}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // Isolated tag chars
    ctx.do_edit(EditNotification::Insert { chars: "\u{E0067}\u{E0067}b".into() });
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E0067}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E0067}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // Isolated tab term.
    ctx.do_edit(EditNotification::Insert { chars: "\u{E007F}\u{E007F}b".into() });
    assert_eq!(harness.debug_render(), "a\u{E007F}\u{E007F}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E007F}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");

    // Immediate tag_term after tag_base
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}b".into() });
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}b|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "a|");
}

#[test]
fn delete_emoji_modifier_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\u{1F466}\u{1F3FB}";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 8, ty: PointSelect });

    // U+1F3FB is EMOJI MODIFIER FITZPATRICK TYPE-1-2.
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F3FB}".into() });
    assert_eq!(harness.debug_render(), "\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Isolated multiple emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F3FB}\u{1F3FB}".into() });
    assert_eq!(harness.debug_render(), "\u{1F3FB}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Multiple emoji modifiers
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_mixed_edge_cases_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 7, ty: PointSelect });

    // COMBINING ENCLOSING KEYCAP + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
    // COMBINING ENCLOSING KEYCAP + ending with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // COMBINING ENCLOSING KEYCAP + ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{200D}\u{1F5E8}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F441}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // COMBINING ENCLOSING KEYCAP + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // COMBINING ENCLOSING KEYCAP + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "1\u{20E3}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + COMBINING ENCLOSING KEYCAP
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{20E3}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1f466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + end with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert {
        chars: "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F469}".into(),
    });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Variation selector + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + variation selector
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{FE0F}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start withj ZERO WIDTH JOINER + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + Regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + end with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{200D}\u{1F469}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Start with ZERO WIDTH JOINER + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // ZERO WIDTH JOINER + emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F469}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + end with ZERO WIDTH JOINER
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{200D}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Regional indicator symbol + Emoji modifier
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{1F3FB}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1FA}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // Emoji modifier + regional indicator symbol
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{1F1FA}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");

    // RIS + LF
    ctx.do_edit(EditNotification::Insert { chars: "\u{1F1E6}\u{000A}".into() });
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "\u{1F1E6}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn delete_variation_selector_with_combining_mark_uses_grapheme_boundary() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("e\u{0301}\u{FE0F}");
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 6, ty: PointSelect });

    assert_eq!(harness.debug_render(), "e\u{0301}\u{FE0F}|");
    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn edit_type_to_string_matches_wire_names() {
    assert_eq!(edit_type_to_string(EditType::InsertChars), "insert");
    assert_eq!(edit_type_to_string(EditType::InsertNewline), "newline");
    assert_eq!(edit_type_to_string(EditType::Other), "other");
}

#[test]
fn delete_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });

    ctx.do_edit(EditNotification::MoveRight);
    assert_eq!(
        harness.debug_render(),
        "\
        t|his is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(
        harness.debug_render(),
        "\
        |his is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteForward);
    assert_eq!(
        harness.debug_render(),
        "\
        |is is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveWordRight);
    ctx.do_edit(EditNotification::DeleteWordForward);
    assert_eq!(
        harness.debug_render(),
        "\
        is| a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteWordBackward);
    assert_eq!(
        harness.debug_render(),
        "| \
        a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveToRightEndOfLine);
    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    assert_eq!(
        harness.debug_render(),
        "\
        |\nthat has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteToEndOfParagraph);
    ctx.do_edit(EditNotification::DeleteToEndOfParagraph);
    assert_eq!(
        harness.debug_render(),
        "\
        |\nlines."
    );
}

#[test]
fn multiline_indentation_test() {
    use crate::rpc::{EditNotification, GestureType::*};
    let initial_text = "\
    this is a string\n\
    that has three\n\
    lines.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that has three\n\
    lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that |has three\n\
    lines."
    );

    // Simple multi line indent/outdent test
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    this |is a string\n    \
    that |has three\n\
    lines."
    );

    ctx.do_edit(EditNotification::Outdent);
    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that |has three\n\
    lines."
    );

    // Different position indent/outdent test
    // Shouldn't change cursor position
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 10, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that has t|hree\n\
    lines."
    );

    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    this |is a string\n    \
    that has t|hree\n\
    lines."
    );

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that has t|hree\n\
    lines."
    );

    // Multi line selection test
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 10, ty: ToggleSel });
    ctx.do_edit(EditNotification::MoveToEndOfDocumentAndModifySelection);
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    this [is a string\n    \
    that has three\n    \
    lines.|]"
    );

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    this [is a string\n\
    that has three\n\
    lines.|]"
    );

    // Multi cursor different line indent test
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    ctx.do_edit(EditNotification::Gesture { line: 2, col: 0, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
    |this is a string\n\
    that has three\n\
    |lines."
    );

    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    |this is a string\n\
    that has three\n    \
    |lines."
    );

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    |this is a string\n\
    that has three\n\
    |lines."
    );
}

#[test]
fn simple_indentation_test() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();
    // Single indent and outdent test
    ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(harness.debug_render(), "    hello|");
    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(harness.debug_render(), "hello|");

    // Test when outdenting with less than 4 spaces
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    ctx.do_edit(EditNotification::Insert { chars: "  ".into() });
    assert_eq!(harness.debug_render(), "  |hello");
    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(harness.debug_render(), "|hello");

    // Non-selection one line indent and outdent test
    ctx.do_edit(EditNotification::MoveToEndOfDocument);
    ctx.do_edit(EditNotification::Indent);
    ctx.do_edit(EditNotification::InsertNewline);
    ctx.do_edit(EditNotification::Insert { chars: "world".into() });
    assert_eq!(harness.debug_render(), "    hello\n    world|");

    ctx.do_edit(EditNotification::MoveWordLeft);
    ctx.do_edit(EditNotification::MoveToBeginningOfDocumentAndModifySelection);
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(harness.debug_render(), "    [|    hello\n        ]world");

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(harness.debug_render(), "[|    hello\n    ]world");

    ctx.do_edit(EditNotification::SelectAll);
    ctx.do_edit(EditNotification::DeleteBackward);
    ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
    ctx.do_edit(EditNotification::SelectAll);
    ctx.do_edit(EditNotification::InsertTab);
    assert_eq!(harness.debug_render(), "    |");
}

#[test]
fn number_change_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();
    // Single indent and outdent test
    ctx.do_edit(EditNotification::Insert { chars: "1234".into() });
    ctx.do_edit(EditNotification::IncreaseNumber);
    assert_eq!(harness.debug_render(), "1235|");

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 2, ty: PointSelect });
    ctx.do_edit(EditNotification::IncreaseNumber);
    assert_eq!(harness.debug_render(), "1236|");

    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    ctx.do_edit(EditNotification::Insert { chars: "-42".into() });
    ctx.do_edit(EditNotification::IncreaseNumber);
    assert_eq!(harness.debug_render(), "-41|");

    // Cursor is on the 3
    ctx.do_edit(EditNotification::MoveToEndOfDocument);
    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    ctx.do_edit(EditNotification::Insert { chars: "this is a 336 text example".into() });
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
    ctx.do_edit(EditNotification::DecreaseNumber);
    assert_eq!(harness.debug_render(), "this is a 335| text example");

    // Cursor is on of the 3
    ctx.do_edit(EditNotification::MoveToEndOfDocument);
    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    ctx.do_edit(EditNotification::Insert { chars: "this is a -336 text example".into() });
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
    ctx.do_edit(EditNotification::DecreaseNumber);
    assert_eq!(harness.debug_render(), "this is a -337| text example");
}

#[test]
fn test_exact_position() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\
        this is a string\n\
        that has three\n\
        \n\
        lines.\n\
        And lines with very different length.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: PointSelect });
    ctx.do_edit(EditNotification::AddSelectionAbove);
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that |has three\n\
        \n\
        lines.\n\
        And lines with very different length."
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
    ctx.do_edit(EditNotification::AddSelectionBelow);
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that |has three\n\
        \n\
        lines.\n\
        And lines with very different length."
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::Gesture { line: 4, col: 10, ty: PointSelect });
    ctx.do_edit(EditNotification::AddSelectionAbove);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that has t|hree\n\
        \n\
        lines.\n\
        And lines |with very different length."
    );
}

#[test]
fn test_illegal_plugin_edit() {
    use crate::plugins::PluginPid;
    use crate::plugins::rpc::{PluginEdit, PluginNotification};
    use xi_rope::DeltaBuilder;

    let text = "text";
    let harness = ContextHarness::new(text);
    let mut ctx = harness.make_context();
    let rev_token = ctx.editor.borrow().get_head_rev_token();

    let iv = Interval::new(1, 1);
    let mut builder = DeltaBuilder::new(0); // wrong length
    builder.replace(iv, "1".into());

    let edit_one = PluginEdit {
        rev: rev_token,
        delta: builder.build(),
        priority: 55,
        after_cursor: false,
        undo_group: None,
        author: "plugin_one".into(),
    };

    ctx.do_plugin_cmd(PluginPid(1), PluginNotification::Edit { edit: edit_one });
    let new_rev_token = ctx.editor.borrow().get_head_rev_token();
    assert_eq!(rev_token, new_rev_token);
}

#[test]
fn empty_transpose() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Transpose);

    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn eol_multicursor_transpose() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("word\n");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 4, ty: PointSelect }); // end of first line
    ctx.do_edit(EditNotification::AddSelectionBelow); // add cursor below that, at eof
    ctx.do_edit(EditNotification::Transpose);

    assert_eq!(harness.debug_render(), "wor\nd|");
}

// ── VLF viewport tests ──

fn vlf_harness(content: &[u8]) -> (ContextHarness, NamedTempFile) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    (harness, f)
}

#[test]
fn vlf_viewport_sends_correct_lines_for_scanned_file() {
    use crate::rpc::EditNotification;

    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\ndelta\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 1, generation: 1 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");

    assert_eq!(params["generation"], 1u64);
    assert_eq!(params["line_start"], 0u64);
    let lines = params["lines"].as_array().expect("lines must be array");
    assert_eq!(lines.len(), 2, "should return exactly the requested line count");
    assert_eq!(lines[0].as_str(), Some("alpha"));
    assert_eq!(lines[1].as_str(), Some("beta"));
}

#[test]
fn vlf_viewport_sends_full_crlf_line_range() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\r\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.editor.borrow_mut().enable_vlf_editing();
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 100, line_end: 140, generation: 12 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["line_start"], 100u64);
    assert_eq!(lines.len(), 41, "CRLF viewport should return full requested range");
    assert_eq!(lines.first().and_then(|line| line.as_str()), Some("line 100\r"));
    assert_eq!(lines.last().and_then(|line| line.as_str()), Some("line 140\r"));
}

#[test]
fn vlf_selected_text_reads_from_text_store() {
    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness
        .view
        .borrow_mut()
        .set_selection(&Rope::from("alpha\nbeta\ngamma\n"), SelRegion::new(1, 8));
    let mut ctx = harness.make_context();

    assert_eq!(ctx.preview_selected_text(false), "lpha\nbe");
    assert_eq!(ctx.preview_selected_text(true), "alpha\nbeta\n");
}

#[test]
fn vlf_viewport_uses_prefix_fallback_for_pending_index() {
    use crate::rpc::EditNotification;

    let mut f = NamedTempFile::new().unwrap();
    let content =
        (0..200).map(|i| format!("line {i} {}\n", "x".repeat(128 * 1024))).collect::<String>();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 20, line_end: 25, generation: 42 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification even for pending index");
    assert_eq!(params["generation"], 42u64);
    let lines = params["lines"].as_array().unwrap();
    assert_eq!(params["line_start"], 20u64);
    assert_eq!(lines.len(), 6, "prefix fallback should satisfy near-top viewport");
    assert!(
        lines
            .first()
            .and_then(|line| line.as_str())
            .is_some_and(|line| line.starts_with("line 20 "))
    );
    assert!(
        lines
            .last()
            .and_then(|line| line.as_str())
            .is_some_and(|line| line.starts_with("line 25 "))
    );
}

#[test]
fn vlf_viewport_estimates_unknown_line_count_from_decoded_chunk() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 2, generation: 7 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");

    let approximate = params["approximate_line_count"].as_u64().unwrap();
    assert!(approximate > 103, "estimate should not crawl by line_end + 100, got {approximate}");
    assert!(!params["line_count_exact"].as_bool().unwrap());
}

#[test]
fn vlf_viewport_near_approx_end_returns_tail_lines() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 190, line_end: 210, generation: 8 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let response_line_start = params["line_start"].as_u64().unwrap();
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(response_line_start, 190);
    assert_eq!(lines.first().and_then(|line| line.as_str()), Some("line 190"));
    assert!(lines.iter().any(|line| line.as_str() == Some("line 199")));
}

#[test]
fn vlf_viewport_tail_sentinel_returns_file_tail_without_index() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: u64::MAX, line_end: 4, generation: 9 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["generation"], 9u64);
    assert_eq!(params["approximate_line_count"], 201u64);
    assert!(params["line_count_exact"].as_bool().unwrap());
    assert_eq!(params["line_start"], 196u64);
    assert!(lines.iter().any(|line| line.as_str() == Some("line 199")));
}

#[test]
fn vlf_viewport_tail_sentinel_returns_tail_without_exact_line_count_scan() {
    use crate::rpc::EditNotification;

    let line = format!("{}\n", "x".repeat(1023));
    let content = line.repeat(33 * 1024);
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport {
        line_start: u64::MAX,
        line_end: 4,
        generation: 10,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");

    assert_eq!(params["generation"], 10u64);
    assert!(!params["line_count_exact"].as_bool().unwrap());
    let lines = params["lines"].as_array().expect("lines must be array");
    assert_eq!(lines.len(), 5);
    assert!(params["approximate_line_count"].as_u64().unwrap() >= 5);
}

#[test]
fn vlf_viewport_tail_sentinel_can_request_full_viewport_without_index() {
    use crate::rpc::EditNotification;

    let line = format!("{}\n", "x".repeat(1023));
    let content = line.repeat(33 * 1024);
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport {
        line_start: u64::MAX,
        line_end: 20,
        generation: 11,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["generation"], 11u64);
    assert!(!params["line_count_exact"].as_bool().unwrap());
    assert_eq!(lines.len(), 21);
}

#[test]
fn vlf_viewport_approximate_anchor_returns_lines_without_exact_index() {
    use crate::rpc::EditNotification;

    let content = (0..40_000).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();
    store.scan_page_at(0).unwrap();
    assert!(matches!(store.line_to_byte(LogicalLine(20_000)), LineLookup::Approximate(_)));

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport {
        line_start: 20_000,
        line_end: 20_010,
        generation: 12,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["generation"], 12u64);
    assert_eq!(params["line_start"], 20_000u64);
    assert!(!params["line_count_exact"].as_bool().unwrap());
    assert!(!lines.is_empty());
}

#[test]
fn vlf_viewport_ignored_for_normal_buffer() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("hello\nworld\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 1, generation: 1 });

    let notifications = harness.take_notifications();
    assert!(
        !notifications.iter().any(|(m, _)| m == "vlf_chunks"),
        "vlf_chunks must not be sent for normal (non-VLF) buffers: {notifications:?}"
    );
}

#[test]
fn vlf_find_emits_search_status_with_ranges() {
    use crate::rpc::EditNotification;

    let (harness, _f) = vlf_harness(b"alpha\nbeta needle\ngamma needle\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Find {
        chars: String::from("needle"),
        case_sensitive: true,
        regex: false,
        whole_words: false,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_search_status")
        .expect("expected vlf_search_status notification");

    assert_eq!(params["query"], "needle");
    assert_eq!(params["stored_match_count"], 2u64);
    assert_eq!(params["complete"], true);
    let ranges = params["ranges"].as_array().expect("ranges array");
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0]["line"], 1u64);
    assert_eq!(ranges[0]["start_col"], 5u64);
    assert_eq!(ranges[0]["end_col"], 11u64);
}

#[test]
fn render_if_needed_skips_vlf_placeholder_rope_selection_offsets() {
    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness.view.borrow_mut().set_vlf_selection(SelRegion::caret(10));
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.render_if_needed();

    let notifications = harness.take_notifications();
    assert!(
        !notifications.iter().any(|(method, _)| method == "update"),
        "VLF render path must not try to render placeholder rope state"
    );
}

#[test]
fn vlf_replace_range_notification_updates_text_then_search_and_viewport() {
    use crate::rpc::EditNotification;
    use crate::text_store::{ByteRange, TextChunkResult};

    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness.editor.borrow_mut().enable_vlf_editing();
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfReplaceRange {
        start_line: 1,
        start_col: 4,
        end_line: 1,
        end_col: 4,
        text: String::from(" needle"),
    });

    {
        let editor = harness.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("expected VLF store");
        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    let notifications = harness.take_notifications();
    let (_, scroll) = notifications
        .iter()
        .find(|(method, _)| method == "scroll_to")
        .expect("expected scroll_to after VLF edit");
    assert_eq!(scroll["line"], 1u64);
    assert_eq!(scroll["col"], 11u64);

    ctx.do_edit(EditNotification::Find {
        chars: String::from("needle"),
        case_sensitive: true,
        regex: false,
        whole_words: false,
    });

    let notifications = harness.take_notifications();
    let (_, status) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_search_status")
        .expect("expected vlf_search_status after edit");
    assert_eq!(status["stored_match_count"], 1u64);
    assert_eq!(status["ranges"][0]["line"], 1u64);
    assert_eq!(status["ranges"][0]["start_col"], 5u64);
    assert_eq!(status["ranges"][0]["end_col"], 11u64);

    ctx.do_edit(EditNotification::VlfViewport { line_start: 1, line_end: 1, generation: 77 });

    let notifications = harness.take_notifications();
    let (_, viewport) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_chunks")
        .expect("expected vlf_chunks after edit");
    assert_eq!(viewport["generation"], 77u64);
    let lines = viewport["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].as_str(), Some("beta needle"));
}

#[test]
fn vlf_find_next_scrolls_to_first_known_match() {
    use crate::rpc::EditNotification;

    let (harness, _f) = vlf_harness(b"alpha\nbeta needle\ngamma needle\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Find {
        chars: String::from("needle"),
        case_sensitive: true,
        regex: false,
        whole_words: false,
    });
    harness.take_notifications();

    ctx.do_edit(EditNotification::FindNext {
        wrap_around: true,
        allow_same: false,
        modify_selection: SelectionModifier::Set,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "scroll_to")
        .expect("expected scroll_to notification");

    assert_eq!(params["line"], 1u64);
    assert_eq!(params["col"], 5u64);
}

#[test]
fn vlf_syntax_selection_uses_visible_range_parse() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let source = b"fn main() { foo(bar); }\n";
    let (harness, _f) = vlf_harness(source);
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 0, generation: 1 });
    harness.take_notifications();

    let source = String::from_utf8(source.to_vec()).unwrap();
    {
        let editor = ctx.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("vlf store");
        let window_range = ctx.current_vlf_semantic_range(store).expect("semantic window");
        let window_text = ctx
            .current_vlf_semantic_window_text(store, window_range, "rust", None)
            .expect("semantic window text");
        assert_eq!(window_text, source);
        assert_eq!(window_range.start.0 as usize, 0);
        assert_eq!(window_range.end.0 as usize, source.len());
    }
    let start = source.find("bar").unwrap();
    let end = start + 3;
    harness.view.borrow_mut().set_vlf_selection(SelRegion::new(start, end));

    ctx.do_syntax_selection(crate::object::SyntaxSelectionAction::Expand);

    let notifications = harness.take_notifications();
    assert!(
        notifications.iter().all(|(method, _)| method != "alert"),
        "VLF semantic selection should not alert when current node is inside parsed window: {notifications:?}"
    );

    let selection = harness.view.borrow().sel_regions()[0];
    assert!(selection.min() <= start);
    assert!(selection.max() >= end);
    assert!(selection.min() < start || selection.max() > end);
}

#[test]
fn vlf_syntax_navigation_stays_bounded_to_visible_range() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() {}\nfn beta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 0, generation: 1 });
    harness.take_notifications();

    {
        let editor = ctx.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("vlf store");
        let window_range = ctx.current_vlf_semantic_range(store).expect("semantic window");
        let window_text = ctx
            .current_vlf_semantic_window_text(store, window_range, "rust", None)
            .expect("semantic window text");
        assert_eq!(window_text, "fn alpha() {}\n");
        assert_eq!(window_range.start.0 as usize, 0);
        assert_eq!(window_range.end.0 as usize, "fn alpha() {}\n".len());
    }

    ctx.do_syntax_navigation(crate::object::SyntaxNavigationAction::new(
        SyntaxNavigationTarget::Function,
        true,
    ));

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected alert notification");
    assert_eq!(params["msg"].as_str(), Some("goto_next_function: outside current parsed range"));
}

#[test]
fn vlf_syntax_navigation_uses_visible_range_parse() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() {}\nfn beta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 2, generation: 1 });
    harness.take_notifications();

    ctx.do_syntax_navigation(crate::object::SyntaxNavigationAction::new(
        SyntaxNavigationTarget::Function,
        true,
    ));

    let selection = harness.view.borrow().sel_regions()[0];
    assert!(selection.is_caret());
    assert_eq!(selection.min(), "fn alpha() {}\n".len());
    assert!(
        harness.take_notifications().iter().all(|(method, _)| method != "alert"),
        "VLF semantic navigation should not alert when target is inside parsed window"
    );
}

#[test]
fn vlf_syntax_commands_reuse_visible_parse_cache() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() { beta(gamma); }\nfn delta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 1, generation: 1 });
    harness.take_notifications();

    let start = "fn alpha() { beta(".len();
    let end = start + "gamma".len();
    harness.view.borrow_mut().set_vlf_selection(SelRegion::new(start, end));

    ctx.do_syntax_selection(crate::object::SyntaxSelectionAction::Expand);
    assert_eq!(harness.view.borrow().semantic_parse_cache_parse_count(), 1);

    ctx.do_syntax_navigation(crate::object::SyntaxNavigationAction::new(
        SyntaxNavigationTarget::Function,
        true,
    ));
    assert_eq!(harness.view.borrow().semantic_parse_cache_parse_count(), 1);
    assert!(
        harness.take_notifications().iter().all(|(method, _)| method != "alert"),
        "reused VLF semantic parse should keep commands functional"
    );
}
