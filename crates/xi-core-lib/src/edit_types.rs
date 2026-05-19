// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A bunch of boilerplate for converting the `EditNotification`s we receive
//! from the client into the events we use internally.
//!
//! This simplifies code elsewhere, and makes it easier to route events to
//! the editor or view as appropriate.

use log::warn;

use crate::movement::Movement;
use crate::object::{SyntaxNavigationAction, SyntaxNavigationTarget, SyntaxSelectionAction};
use crate::plugins::manifest::PluginCapability;
use crate::plugins::rpc::SelectionRange;
use crate::rpc::{
    EditNotification, FindQuery, GestureType, LineRange, LineReplacement, MouseAction, Position,
    SelectionGranularity, SelectionModifier,
};
use crate::view::Size;

/// Events that only modify view state
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum ViewEvent {
    Move(Movement),
    ModifySelection(Movement),
    SelectAll,
    MergeSelections,
    MergeConsecutiveSelections,
    Scroll(LineRange),
    AddSelectionAbove,
    AddSelectionBelow,
    Click(MouseAction),
    Drag(MouseAction),
    Gesture { line: u64, col: u64, ty: GestureType },
    GotoLine { line: u64 },
    Find { chars: String, case_sensitive: bool, regex: bool, whole_words: bool },
    MultiFind { queries: Vec<FindQuery> },
    FindNext { wrap_around: bool, allow_same: bool, modify_selection: SelectionModifier },
    FindPrevious { wrap_around: bool, allow_same: bool, modify_selection: SelectionModifier },
    FindAll,
    HighlightFind { visible: bool },
    SelectionForFind { case_sensitive: bool },
    Replace { chars: String, preserve_case: bool },
    SelectionForReplace,
    SelectRegex { chars: String, case_sensitive: bool },
    SelectionIntoLines,
    CollapseSelections,
    TrimSelections,
    FlipSelections,
    EnsureSelectionsForward,
    KeepPrimarySelection,
    RemovePrimarySelection,
    RotateSelectionsBackward,
    RotateSelectionsForward,
}

/// Events that modify the buffer
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum BufferEvent {
    Delete {
        movement: Movement,
        kill: bool,
    },
    Backspace,
    Transpose,
    Undo,
    Redo,
    Uppercase,
    Lowercase,
    Capitalize,
    Indent,
    Outdent,
    Insert(String),
    Paste(String),
    PasteRegister {
        chars: String,
        before: bool,
    },
    InsertNewline,
    InsertTab,
    Yank,
    ReplaceNext,
    ReplaceAll,
    DuplicateLine,
    IncreaseNumber,
    DecreaseNumber,
    AlignSelections,
    AlignIt {
        pattern: String,
        regex: bool,
        occurrence: i64,
        all: bool,
        format: String,
        range: Option<LineRange>,
    },
    ExpandTabs {
        range: Option<LineRange>,
    },
    ReflowLines {
        width: usize,
        range: Option<LineRange>,
    },
    SortLines {
        descending: bool,
        range: Option<LineRange>,
    },
    RotateSelectionContentsBackward,
    RotateSelectionContentsForward,
    ReverseSelectionContents,
}

/// An event that needs special handling
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum SpecialEvent {
    Resize(Size),
    RequestLines(LineRange),
    RequestHover {
        request_id: usize,
        position: Option<Position>,
    },
    DispatchPluginCommand {
        capability: PluginCapability,
        method: &'static str,
        params: serde_json::Value,
    },
    DeleteLineRange {
        start_line: usize,
        end_line: usize,
    },
    DeleteBlock {
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    },
    ReplayBlockInsert {
        start_line: usize,
        end_line: usize,
        column: usize,
        text: String,
        append: bool,
    },
    ReplaceLineRange {
        start_line: usize,
        end_line: usize,
        lines: Vec<String>,
    },
    ApplyLineReplacements {
        replacements: Vec<LineReplacement>,
    },
    SetSelections {
        selections: Vec<SelectionRange>,
    },
    GotoColumn {
        display_col: usize,
        modify_selection: bool,
    },
    AddNewlineAbove,
    AddNewlineBelow,
    JoinSelections {
        select_space: bool,
    },
    ExtendLineBelow {
        count: usize,
    },
    ExtendLineAbove,
    SelectLineAbove,
    SelectLineBelow,
    ExtendToLineBounds,
    ShrinkToLineBounds,
    MoveWordStart {
        forward: bool,
        long_word: bool,
        modify_selection: bool,
    },
    MoveWordEnd {
        long_word: bool,
        modify_selection: bool,
    },
    FindChar {
        target: char,
        forward: bool,
        inclusive: bool,
        modify_selection: bool,
    },
    MoveToMatchingBracket {
        modify_selection: bool,
    },
    CommitUndoCheckpoint,
    ToggleComment,
    ToggleLineComment,
    ToggleBlockComment,
    Reindent,
    NormalizeLineEndings {
        line_ending: String,
    },
    SyntaxSelection(SyntaxSelectionAction),
    SyntaxNavigation(SyntaxNavigationAction),
    GotoParagraph {
        forward: bool,
    },
    /// VLF viewport request; see [`EditNotification::VlfViewport`].
    VlfViewport {
        line_start: u64,
        line_end: u64,
        generation: u64,
    },
    VlfReplaceRange {
        start_line: u64,
        start_col: u64,
        end_line: u64,
        end_col: u64,
        text: String,
    },
}

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum EventDomain {
    View(ViewEvent),
    Buffer(BufferEvent),
    Special(SpecialEvent),
}

impl From<BufferEvent> for EventDomain {
    fn from(src: BufferEvent) -> EventDomain {
        EventDomain::Buffer(src)
    }
}

impl From<ViewEvent> for EventDomain {
    fn from(src: ViewEvent) -> EventDomain {
        EventDomain::View(src)
    }
}

impl From<SpecialEvent> for EventDomain {
    fn from(src: SpecialEvent) -> EventDomain {
        EventDomain::Special(src)
    }
}

#[rustfmt::skip]
impl From<EditNotification> for EventDomain {
    fn from(src: EditNotification) -> EventDomain {
        use self::EditNotification::*;
        match src {
            Insert { chars } =>
                BufferEvent::Insert(chars).into(),
            Paste { chars } =>
                BufferEvent::Paste(chars).into(),
            PasteRegister { chars, before } =>
                BufferEvent::PasteRegister { chars, before }.into(),
            DeleteForward =>
                BufferEvent::Delete {
                    movement: Movement::Right,
                    kill: false
                }.into(),
            DeleteBackward =>
                BufferEvent::Backspace.into(),
            DeleteWordForward =>
                BufferEvent::Delete {
                    movement: Movement::RightWord,
                    kill: false
                }.into(),
            DeleteWordBackward =>
                BufferEvent::Delete {
                    movement: Movement::LeftWord,
                    kill: false
                }.into(),
            DeleteToEndOfParagraph =>
                BufferEvent::Delete {
                    movement: Movement::EndOfParagraphKill,
                    kill: true
                }.into(),
            DeleteToBeginningOfLine =>
                BufferEvent::Delete {
                    movement: Movement::LeftOfLine,
                    kill: false
                }.into(),
            InsertNewline =>
                BufferEvent::InsertNewline.into(),
            InsertTab =>
                BufferEvent::InsertTab.into(),
            MoveUp =>
                ViewEvent::Move(Movement::Up).into(),
            MoveUpAndModifySelection =>
                ViewEvent::ModifySelection(Movement::Up).into(),
            MoveDown =>
                ViewEvent::Move(Movement::Down).into(),
            MoveDownAndModifySelection =>
                ViewEvent::ModifySelection(Movement::Down).into(),
            MoveLeft | MoveBackward =>
                ViewEvent::Move(Movement::Left).into(),
            MoveLeftAndModifySelection =>
                ViewEvent::ModifySelection(Movement::Left).into(),
            MoveRight | MoveForward  =>
                ViewEvent::Move(Movement::Right).into(),
            MoveRightAndModifySelection =>
                ViewEvent::ModifySelection(Movement::Right).into(),
            MoveWordLeft =>
                ViewEvent::Move(Movement::LeftWord).into(),
            MoveWordLeftAndModifySelection =>
                ViewEvent::ModifySelection(Movement::LeftWord).into(),
            MoveWordRight =>
                ViewEvent::Move(Movement::RightWord).into(),
            MoveWordRightAndModifySelection =>
                ViewEvent::ModifySelection(Movement::RightWord).into(),
            MoveToBeginningOfParagraph =>
                ViewEvent::Move(Movement::StartOfParagraph).into(),
            MoveToBeginningOfParagraphAndModifySelection =>
                ViewEvent::ModifySelection(Movement::StartOfParagraph).into(),
            MoveToEndOfParagraph =>
                ViewEvent::Move(Movement::EndOfParagraph).into(),
            MoveToEndOfParagraphAndModifySelection =>
                ViewEvent::ModifySelection(Movement::EndOfParagraph).into(),
            MoveToLeftEndOfLine =>
                ViewEvent::Move(Movement::LeftOfLine).into(),
            MoveToLeftEndOfLineAndModifySelection =>
                ViewEvent::ModifySelection(Movement::LeftOfLine).into(),
            MoveToRightEndOfLine =>
                ViewEvent::Move(Movement::RightOfLine).into(),
            MoveToRightEndOfLineAndModifySelection =>
                ViewEvent::ModifySelection(Movement::RightOfLine).into(),
            MoveToBeginningOfDocument =>
                ViewEvent::Move(Movement::StartOfDocument).into(),
            MoveToBeginningOfDocumentAndModifySelection =>
                ViewEvent::ModifySelection(Movement::StartOfDocument).into(),
            MoveToEndOfDocument =>
                ViewEvent::Move(Movement::EndOfDocument).into(),
            MoveToEndOfDocumentAndModifySelection =>
                ViewEvent::ModifySelection(Movement::EndOfDocument).into(),
            ScrollPageUp =>
                ViewEvent::Move(Movement::UpPage).into(),
            PageUpAndModifySelection =>
                ViewEvent::ModifySelection(Movement::UpPage).into(),
            ScrollPageDown =>
                ViewEvent::Move(Movement::DownPage).into(),
            PageDownAndModifySelection =>
                ViewEvent::ModifySelection(Movement::DownPage).into(),
            SelectAll => ViewEvent::SelectAll.into(),
            MergeSelections => ViewEvent::MergeSelections.into(),
            MergeConsecutiveSelections => ViewEvent::MergeConsecutiveSelections.into(),
            AddSelectionAbove => ViewEvent::AddSelectionAbove.into(),
            AddSelectionBelow => ViewEvent::AddSelectionBelow.into(),
            Scroll(range) => ViewEvent::Scroll(range).into(),
            Resize(size) => SpecialEvent::Resize(size).into(),
            GotoLine { line } => ViewEvent::GotoLine { line }.into(),
            RequestLines(range) => SpecialEvent::RequestLines(range).into(),
            Yank => BufferEvent::Yank.into(),
            Transpose => BufferEvent::Transpose.into(),
            Click(action) => ViewEvent::Click(action).into(),
            Drag(action) => ViewEvent::Drag(action).into(),
            Gesture { line, col,  ty } => {
                // Translate deprecated gesture types into the new format
                let new_ty = match ty {
                    GestureType::PointSelect => {
                        warn!("The point_select gesture is deprecated; use select instead");
                        GestureType::Select {granularity: SelectionGranularity::Point, multi: false}
                    }
                    GestureType::ToggleSel => {
                        warn!("The toggle_sel gesture is deprecated; use select instead");
                        GestureType::Select { granularity: SelectionGranularity::Point, multi: true}
                    }
                    GestureType::WordSelect => {
                        warn!("The word_select gesture is deprecated; use select instead");
                        GestureType::Select { granularity: SelectionGranularity::Word, multi: false}
                    }
                    GestureType::MultiWordSelect => {
                        warn!("The multi_word_select gesture is deprecated; use select instead");
                        GestureType::Select { granularity: SelectionGranularity::Word, multi: true}
                    }
                    GestureType::LineSelect => {
                        warn!("The line_select gesture is deprecated; use select instead");
                        GestureType::Select { granularity: SelectionGranularity::Line, multi: false}
                    }
                    GestureType::MultiLineSelect => {
                        warn!("The multi_line_select gesture is deprecated; use select instead");
                        GestureType::Select { granularity: SelectionGranularity::Line, multi: true}
                    }
                    GestureType::RangeSelect => {
                        warn!("The range_select gesture is deprecated; use select_extend instead");
                        GestureType::SelectExtend { granularity: SelectionGranularity::Point }
                    }
                    _ => ty
                };
                ViewEvent::Gesture { line, col, ty: new_ty }.into()
            },
            Undo => BufferEvent::Undo.into(),
            Redo => BufferEvent::Redo.into(),
            Find { chars, case_sensitive, regex, whole_words } =>
                ViewEvent::Find { chars, case_sensitive, regex, whole_words }.into(),
            MultiFind { queries } =>
                ViewEvent::MultiFind { queries }.into(),
            FindNext { wrap_around, allow_same, modify_selection } =>
                ViewEvent::FindNext { wrap_around, allow_same, modify_selection }.into(),
            FindPrevious { wrap_around, allow_same, modify_selection } =>
                ViewEvent::FindPrevious { wrap_around, allow_same, modify_selection }.into(),
            FindAll => ViewEvent::FindAll.into(),
            Uppercase => BufferEvent::Uppercase.into(),
            Lowercase => BufferEvent::Lowercase.into(),
            Capitalize => BufferEvent::Capitalize.into(),
            ToggleComment => SpecialEvent::ToggleComment.into(),
            ToggleLineComment => SpecialEvent::ToggleLineComment.into(),
            ToggleBlockComment => SpecialEvent::ToggleBlockComment.into(),
            Indent => BufferEvent::Indent.into(),
            Outdent => BufferEvent::Outdent.into(),
            Reindent => SpecialEvent::Reindent.into(),
            ExpandTabs { range } => BufferEvent::ExpandTabs { range }.into(),
            ReflowLines { width, range } => BufferEvent::ReflowLines { width, range }.into(),
            SortLines { descending, range } => BufferEvent::SortLines { descending, range }.into(),
            NormalizeLineEndings { line_ending } => {
                SpecialEvent::NormalizeLineEndings { line_ending }.into()
            }
            HighlightFind { visible } => ViewEvent::HighlightFind { visible }.into(),
            SelectionForFind { case_sensitive } =>
                ViewEvent::SelectionForFind { case_sensitive }.into(),
            Replace { chars, preserve_case } =>
                ViewEvent::Replace { chars, preserve_case }.into(),
            ReplaceNext => BufferEvent::ReplaceNext.into(),
            ReplaceAll => BufferEvent::ReplaceAll.into(),
            SelectionForReplace => ViewEvent::SelectionForReplace.into(),
            SelectRegex { chars, case_sensitive } => {
                ViewEvent::SelectRegex { chars, case_sensitive }.into()
            }
            TrimSelections => ViewEvent::TrimSelections.into(),
            AlignSelections => BufferEvent::AlignSelections.into(),
            AlignIt { pattern, regex, occurrence, all, format, range } => {
                BufferEvent::AlignIt { pattern, regex, occurrence, all, format, range }.into()
            }
            FlipSelections => ViewEvent::FlipSelections.into(),
            EnsureSelectionsForward => ViewEvent::EnsureSelectionsForward.into(),
            KeepPrimarySelection => ViewEvent::KeepPrimarySelection.into(),
            RemovePrimarySelection => ViewEvent::RemovePrimarySelection.into(),
            RotateSelectionsBackward => ViewEvent::RotateSelectionsBackward.into(),
            RotateSelectionsForward => ViewEvent::RotateSelectionsForward.into(),
            ExpandSelection => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::Expand).into()
            }
            ShrinkSelection => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::Shrink).into()
            }
            GotoNextFunction => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Function,
                    true,
                ))
                .into()
            }
            GotoPrevFunction => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Function,
                    false,
                ))
                .into()
            }
            GotoNextClass => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Class,
                    true,
                ))
                .into()
            }
            GotoPrevClass => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Class,
                    false,
                ))
                .into()
            }
            GotoNextParameter => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Parameter,
                    true,
                ))
                .into()
            }
            GotoPrevParameter => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Parameter,
                    false,
                ))
                .into()
            }
            GotoNextComment => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Comment,
                    true,
                ))
                .into()
            }
            GotoPrevComment => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Comment,
                    false,
                ))
                .into()
            }
            GotoNextTest => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Test,
                    true,
                ))
                .into()
            }
            GotoPrevTest => {
                SpecialEvent::SyntaxNavigation(SyntaxNavigationAction::new(
                    SyntaxNavigationTarget::Test,
                    false,
                ))
                .into()
            }
            GotoNextParagraph => SpecialEvent::GotoParagraph { forward: true }.into(),
            GotoPrevParagraph => SpecialEvent::GotoParagraph { forward: false }.into(),
            SelectPrevSibling => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::SelectPrevSibling).into()
            }
            SelectNextSibling => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::SelectNextSibling).into()
            }
            SelectAllSiblings => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::SelectAllSiblings).into()
            }
            SelectAllChildren => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::SelectAllChildren).into()
            }
            MoveParentNodeStart => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::MoveParentNodeStart).into()
            }
            MoveParentNodeEnd => {
                SpecialEvent::SyntaxSelection(SyntaxSelectionAction::MoveParentNodeEnd).into()
            }
            RequestCompletion { index } =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_completion",
                    params: serde_json::json!({ "index": index }),
                }.into(),
            RequestDeclaration =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_declaration",
                    params: serde_json::json!({}),
                }.into(),
            RequestDefinition =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_definition",
                    params: serde_json::json!({}),
                }.into(),
            RequestTypeDefinition =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_type_definition",
                    params: serde_json::json!({}),
                }.into(),
            RequestReferences =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_references",
                    params: serde_json::json!({}),
                }.into(),
            RequestImplementation =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_implementation",
                    params: serde_json::json!({}),
                }.into(),
            FormatDocument =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "format_document",
                    params: serde_json::json!({}),
                }.into(),
            RequestCodeActions { index } =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_code_actions",
                    params: serde_json::json!({ "index": index }),
                }.into(),
            RequestRename { new_name } =>
                SpecialEvent::DispatchPluginCommand {
                    capability: PluginCapability::Edit,
                    method: "request_rename",
                    params: serde_json::json!({ "new_name": new_name }),
                }.into(),
            RequestHover { request_id, position } =>
                SpecialEvent::RequestHover { request_id, position }.into(),
            DeleteLineRange { start_line, end_line } =>
                SpecialEvent::DeleteLineRange { start_line, end_line }.into(),
            DeleteBlock { start_line, end_line, left_col, right_col } =>
                SpecialEvent::DeleteBlock { start_line, end_line, left_col, right_col }.into(),
            ReplayBlockInsert { start_line, end_line, column, text, append } =>
                SpecialEvent::ReplayBlockInsert {
                    start_line,
                    end_line,
                    column,
                    text,
                    append,
                }.into(),
            ReplaceLineRange { start_line, end_line, lines } =>
                SpecialEvent::ReplaceLineRange { start_line, end_line, lines }.into(),
            ApplyLineReplacements { replacements } =>
                SpecialEvent::ApplyLineReplacements { replacements }.into(),
            SetSelections { selections } =>
                SpecialEvent::SetSelections { selections }.into(),
            SelectionIntoLines => ViewEvent::SelectionIntoLines.into(),
            GotoColumn { display_col, modify_selection } => {
                SpecialEvent::GotoColumn { display_col, modify_selection }.into()
            }
            AddNewlineAbove =>
                SpecialEvent::AddNewlineAbove.into(),
            AddNewlineBelow =>
                SpecialEvent::AddNewlineBelow.into(),
            JoinSelections { select_space } => {
                SpecialEvent::JoinSelections { select_space }.into()
            }
            ExtendLineBelow { count } => {
                SpecialEvent::ExtendLineBelow { count }.into()
            }
            ExtendLineAbove =>
                SpecialEvent::ExtendLineAbove.into(),
            SelectLineAbove =>
                SpecialEvent::SelectLineAbove.into(),
            SelectLineBelow =>
                SpecialEvent::SelectLineBelow.into(),
            ExtendToLineBounds =>
                SpecialEvent::ExtendToLineBounds.into(),
            ShrinkToLineBounds =>
                SpecialEvent::ShrinkToLineBounds.into(),
            MoveWordStart { forward, long_word, modify_selection } => {
                SpecialEvent::MoveWordStart { forward, long_word, modify_selection }.into()
            }
            MoveWordEnd { long_word, modify_selection } => {
                SpecialEvent::MoveWordEnd { long_word, modify_selection }.into()
            }
            FindChar { target, forward, inclusive, modify_selection } => {
                SpecialEvent::FindChar { target, forward, inclusive, modify_selection }.into()
            }
            CommitUndoCheckpoint => SpecialEvent::CommitUndoCheckpoint.into(),
            MoveToMatchingBracket { modify_selection } => {
                SpecialEvent::MoveToMatchingBracket { modify_selection }.into()
            }
            DuplicateLine => BufferEvent::DuplicateLine.into(),
            IncreaseNumber => BufferEvent::IncreaseNumber.into(),
            DecreaseNumber => BufferEvent::DecreaseNumber.into(),
            RotateSelectionContentsBackward => BufferEvent::RotateSelectionContentsBackward.into(),
            RotateSelectionContentsForward => BufferEvent::RotateSelectionContentsForward.into(),
            ReverseSelectionContents => BufferEvent::ReverseSelectionContents.into(),
            CollapseSelections => ViewEvent::CollapseSelections.into(),
            VlfViewport { line_start, line_end, generation } =>
                SpecialEvent::VlfViewport { line_start, line_end, generation }.into(),
            VlfReplaceRange { start_line, start_col, end_line, end_col, text } =>
                SpecialEvent::VlfReplaceRange {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                    text,
                }.into(),
        }
    }
}
