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

//! Utility functions meant for converting types from LSP to Core format
//! and vice-versa

use std::fs;

use crate::types::{LanguageResponseError, LspCodeAction, PendingCompletionItem};
use lsp_types::*;
use url::Url;
use xi_core_lib::plugin_rpc::{CompletionSuggestion, NavigationTarget, SymbolItem};
use xi_plugin_lib::{
    Cache, Diagnostic as CoreDiagnostic, DiagnosticSeverity as CoreDiagnosticSeverity,
    Error as PluginLibError, Hover as CoreHover, Range as CoreRange, View,
};

pub(crate) fn marked_string_to_string(marked_string: &MarkedString) -> String {
    match *marked_string {
        MarkedString::String(ref text) => text.to_owned(),
        MarkedString::LanguageString(ref d) => format!("```{}\n{}\n```", d.language, d.value),
    }
}

pub(crate) fn markdown_from_hover_contents(
    hover_contents: HoverContents,
) -> Result<String, LanguageResponseError> {
    let res = match hover_contents {
        HoverContents::Scalar(content) => marked_string_to_string(&content),
        HoverContents::Array(content) => {
            let res: Vec<String> = content.iter().map(marked_string_to_string).collect();
            res.join("\n")
        }
        HoverContents::Markup(content) => content.value,
    };
    if res.is_empty() { Err(LanguageResponseError::FallbackResponse) } else { Ok(res) }
}

/// Counts the number of utf-16 code units in the given string.
pub(crate) fn count_utf16(s: &str) -> usize {
    let mut utf16_count = 0;
    for &b in s.as_bytes() {
        if (b as i8) >= -0x40 {
            utf16_count += 1;
        }
        if b >= 0xf0 {
            utf16_count += 1;
        }
    }
    utf16_count
}

/// Get LSP Style Utf-16 based position given the xi-core style utf-8 offset
pub(crate) fn get_position_of_offset<C: Cache>(
    view: &mut View<C>,
    offset: usize,
) -> Result<Position, PluginLibError> {
    let line_num = view.line_of_offset(offset)?;
    let line_offset = view.offset_of_line(line_num)?;

    let char_offset = count_utf16(&(view.get_line(line_num)?[0..(offset - line_offset)]));

    Ok(Position {
        line: u32::try_from(line_num).expect("line number should fit in u32"),
        character: u32::try_from(char_offset).expect("character offset should fit in u32"),
    })
}

pub(crate) fn offset_of_position<C: Cache>(
    view: &mut View<C>,
    position: Position,
) -> Result<usize, PluginLibError> {
    let line_offset = view.offset_of_line(position.line as usize);

    let mut cur_len_utf16 = 0;
    let mut cur_len_utf8 = 0;

    for u in view.get_line(position.line as usize)?.chars() {
        if cur_len_utf16 >= (position.character as usize) {
            break;
        }
        cur_len_utf16 += u.len_utf16();
        cur_len_utf8 += u.len_utf8();
    }

    Ok(cur_len_utf8 + line_offset?)
}

pub(crate) fn offset_of_position_in_document(
    text: &str,
    position: Position,
) -> Result<usize, LanguageResponseError> {
    let target_line = usize::try_from(position.line)
        .map_err(|_| LanguageResponseError::Transport(String::from("line index overflow")))?;
    let target_character = usize::try_from(position.character)
        .map_err(|_| LanguageResponseError::Transport(String::from("character index overflow")))?;

    let mut offset = 0usize;
    let mut lines = text.split_inclusive('\n');

    for _ in 0..target_line {
        let Some(line_text) = lines.next() else {
            return Err(LanguageResponseError::Transport(format!(
                "line {} out of bounds for diagnostics document",
                position.line
            )));
        };
        offset += line_text.len();
    }

    let line_text = lines.next().unwrap_or("");
    let line_without_newline = line_text.strip_suffix('\n').unwrap_or(line_text);

    let mut utf16_units = 0usize;
    let mut utf8_units = 0usize;
    for ch in line_without_newline.chars() {
        if utf16_units >= target_character {
            break;
        }
        utf16_units += ch.len_utf16();
        utf8_units += ch.len_utf8();
    }

    if utf16_units < target_character {
        return Err(LanguageResponseError::Transport(format!(
            "character {} out of bounds for diagnostics line {}",
            position.character, position.line
        )));
    }

    Ok(offset + utf8_units)
}

fn byte_column_of_position_in_document(
    text: &str,
    position: Position,
) -> Result<usize, LanguageResponseError> {
    let line_start =
        offset_of_position_in_document(text, Position { line: position.line, character: 0 })?;
    let position_offset = offset_of_position_in_document(text, position)?;
    Ok(position_offset.saturating_sub(line_start))
}

fn document_text_for_uri(
    current_document_uri: &Uri,
    current_document_text: &str,
    uri: &Uri,
) -> Result<String, LanguageResponseError> {
    if current_document_uri == uri {
        return Ok(current_document_text.to_string());
    }

    let path = Url::parse(uri.as_str())
        .map_err(|err| LanguageResponseError::Transport(format!("invalid URI {:?}: {err}", uri)))?
        .to_file_path()
        .map_err(|_| {
            LanguageResponseError::Transport(format!(
                "non-file URI in navigation response: {:?}",
                uri
            ))
        })?;
    fs::read_to_string(&path).map_err(|err| {
        LanguageResponseError::Transport(format!(
            "failed to read navigation target {}: {err}",
            path.display()
        ))
    })
}

fn navigation_target_from_uri_and_range(
    current_document_uri: &Uri,
    current_document_text: &str,
    uri: &Uri,
    range: Range,
) -> Result<NavigationTarget, LanguageResponseError> {
    let text = document_text_for_uri(current_document_uri, current_document_text, uri)?;
    let path = Url::parse(uri.as_str())
        .map_err(|err| LanguageResponseError::Transport(format!("invalid URI {:?}: {err}", uri)))?
        .to_file_path()
        .map_err(|_| {
            LanguageResponseError::Transport(format!(
                "non-file URI in navigation response: {:?}",
                uri
            ))
        })?;
    Ok(NavigationTarget {
        path: path.to_string_lossy().to_string(),
        line: usize::try_from(range.start.line)
            .map_err(|_| LanguageResponseError::Transport(String::from("line index overflow")))?,
        column: byte_column_of_position_in_document(&text, range.start)?,
        end_line: usize::try_from(range.end.line)
            .map_err(|_| LanguageResponseError::Transport(String::from("line index overflow")))?,
        end_column: byte_column_of_position_in_document(&text, range.end)?,
    })
}

pub(crate) fn completion_items_from_response(response: CompletionResponse) -> Vec<CompletionItem> {
    match response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    }
}

pub(crate) fn completion_suggestions_from_items(
    items: &[CompletionItem],
) -> Vec<CompletionSuggestion> {
    items
        .iter()
        .map(|item| CompletionSuggestion {
            label: item.label.clone(),
            detail: item.detail.clone(),
            insert_text: item.insert_text.clone().or_else(|| match &item.text_edit {
                Some(CompletionTextEdit::Edit(edit)) => Some(edit.new_text.clone()),
                Some(CompletionTextEdit::InsertAndReplace(edit)) => Some(edit.new_text.clone()),
                None => None,
            }),
        })
        .collect()
}

pub(crate) fn pending_completions_from_response(
    response: CompletionResponse,
) -> Vec<PendingCompletionItem> {
    let items = completion_items_from_response(response);
    let suggestions = completion_suggestions_from_items(&items);
    items
        .into_iter()
        .zip(suggestions)
        .map(|(item, suggestion)| PendingCompletionItem { suggestion, item })
        .collect()
}

pub(crate) fn completion_text_edits<C: Cache>(
    view: &mut View<C>,
    item: &CompletionItem,
) -> Result<Vec<TextEdit>, LanguageResponseError> {
    let mut edits = Vec::new();

    if let Some(text_edit) = item.text_edit.clone() {
        let text_edit = match text_edit {
            CompletionTextEdit::Edit(edit) => edit,
            CompletionTextEdit::InsertAndReplace(edit) => TextEdit {
                range: edit.insert,
                new_text: edit.new_text,
            },
        };
        edits.push(text_edit);
    }

    if let Some(additional_text_edits) = &item.additional_text_edits {
        edits.extend(additional_text_edits.iter().cloned());
    }

    if edits.is_empty() {
        let selection = view
            .get_selections()
            .map_err(LanguageResponseError::from)?
            .into_iter()
            .next()
            .unwrap_or(xi_plugin_lib::SelectionRange { start: 0, end: 0 });
        let start = selection.start.min(selection.end);
        let end = selection.start.max(selection.end);
        let new_text = item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone());
        edits.push(TextEdit {
            range: Range {
                start: get_position_of_offset(view, start).map_err(LanguageResponseError::from)?,
                end: get_position_of_offset(view, end).map_err(LanguageResponseError::from)?,
            },
            new_text,
        });
    }

    Ok(edits)
}

pub(crate) fn navigation_targets_from_definition_response(
    current_document_uri: &Uri,
    current_document_text: &str,
    response: GotoDefinitionResponse,
) -> Result<Vec<NavigationTarget>, LanguageResponseError> {
    match response {
        GotoDefinitionResponse::Scalar(location) => Ok(vec![navigation_target_from_uri_and_range(
            current_document_uri,
            current_document_text,
            &location.uri,
            location.range,
        )?]),
        GotoDefinitionResponse::Array(locations) => locations
            .into_iter()
            .map(|location| {
                navigation_target_from_uri_and_range(
                    current_document_uri,
                    current_document_text,
                    &location.uri,
                    location.range,
                )
            })
            .collect(),
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| {
                navigation_target_from_uri_and_range(
                    current_document_uri,
                    current_document_text,
                    &link.target_uri,
                    link.target_selection_range,
                )
            })
            .collect(),
    }
}

pub(crate) fn navigation_targets_from_references(
    current_document_uri: &Uri,
    current_document_text: &str,
    locations: Vec<Location>,
) -> Result<Vec<NavigationTarget>, LanguageResponseError> {
    locations
        .into_iter()
        .map(|location| {
            navigation_target_from_uri_and_range(
                current_document_uri,
                current_document_text,
                &location.uri,
                location.range,
            )
        })
        .collect()
}

pub(crate) fn extract_document_edits_for_uri(
    edit: WorkspaceEdit,
    document_uri: &Uri,
) -> Result<Vec<TextEdit>, LanguageResponseError> {
    let mut edits = Vec::new();

    if let Some(changes) = edit.changes {
        if let Some(mut document_edits) = changes.get(document_uri).cloned() {
            edits.append(&mut document_edits);
        }
    }

    if let Some(document_changes) = edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(documents) => {
                for document_edit in documents {
                    if document_edit.text_document.uri == *document_uri {
                        for edit in document_edit.edits {
                            match edit {
                                OneOf::Left(edit) => edits.push(edit),
                                OneOf::Right(_) => {
                                    return Err(LanguageResponseError::Transport(String::from(
                                        "annotated text edits are not supported",
                                    )));
                                }
                            }
                        }
                    }
                }
            }
            DocumentChanges::Operations(_) => {
                return Err(LanguageResponseError::Transport(String::from(
                    "resource operations in workspace edits are not supported",
                )));
            }
        }
    }

    Ok(edits)
}

pub(crate) fn workspace_edit_changes_only_document(
    edit: &WorkspaceEdit,
    document_uri: &Uri,
) -> Result<bool, LanguageResponseError> {
    if let Some(changes) = &edit.changes
        && changes.keys().any(|uri| uri != document_uri)
    {
        return Ok(false);
    }

    if let Some(document_changes) = &edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(documents) => {
                if documents.iter().any(|doc| doc.text_document.uri != *document_uri) {
                    return Ok(false);
                }
                for document_edit in documents {
                    if document_edit.edits.iter().any(|edit| matches!(edit, OneOf::Right(_))) {
                        return Err(LanguageResponseError::Transport(String::from(
                            "annotated text edits are not supported",
                        )));
                    }
                }
            }
            DocumentChanges::Operations(_) => {
                return Err(LanguageResponseError::Transport(String::from(
                    "resource operations in workspace edits are not supported",
                )));
            }
        }
    }

    Ok(true)
}

pub(crate) fn code_actions_from_response(
    response: CodeActionResponse,
    document_uri: &Uri,
) -> Result<Vec<LspCodeAction>, LanguageResponseError> {
    response
        .into_iter()
        .map(|action| match action {
            CodeActionOrCommand::Command(command) => Ok(LspCodeAction {
                title: command.title.clone(),
                edits: Vec::new(),
                command: Some(command),
            }),
            CodeActionOrCommand::CodeAction(action) => Ok(LspCodeAction {
                title: action.title,
                edits: action
                    .edit
                    .map(|edit| extract_document_edits_for_uri(edit, document_uri))
                    .transpose()?
                    .unwrap_or_default(),
                command: action.command,
            }),
        })
        .collect()
}

pub(crate) fn core_range_from_range<C: Cache>(
    view: &mut View<C>,
    range: Range,
) -> Result<CoreRange, PluginLibError> {
    Ok(CoreRange {
        start: offset_of_position(view, range.start)?,
        end: offset_of_position(view, range.end)?,
    })
}

pub(crate) fn core_hover_from_hover<C: Cache>(
    view: &mut View<C>,
    hover: Hover,
) -> Result<CoreHover, LanguageResponseError> {
    Ok(CoreHover {
        content: markdown_from_hover_contents(hover.contents)?,
        range: match hover.range {
            Some(range) => Some(core_range_from_range(view, range)?),
            None => None,
        },
    })
}

fn core_diagnostic_severity_from_lsp(
    severity: Option<DiagnosticSeverity>,
) -> CoreDiagnosticSeverity {
    match severity.unwrap_or(DiagnosticSeverity::ERROR) {
        DiagnosticSeverity::ERROR => CoreDiagnosticSeverity::Error,
        DiagnosticSeverity::WARNING => CoreDiagnosticSeverity::Warning,
        DiagnosticSeverity::INFORMATION => CoreDiagnosticSeverity::Information,
        DiagnosticSeverity::HINT => CoreDiagnosticSeverity::Hint,
        _ => CoreDiagnosticSeverity::Error,
    }
}

pub(crate) fn core_diagnostic_from_lsp_document(
    text: &str,
    diagnostic: Diagnostic,
) -> Result<CoreDiagnostic, LanguageResponseError> {
    Ok(CoreDiagnostic {
        range: CoreRange {
            start: offset_of_position_in_document(text, diagnostic.range.start)?,
            end: offset_of_position_in_document(text, diagnostic.range.end)?,
        },
        severity: core_diagnostic_severity_from_lsp(diagnostic.severity),
        message: diagnostic.message,
        source: diagnostic.source,
        code: diagnostic.code.map(|code| match code {
            NumberOrString::String(value) => value,
            NumberOrString::Number(value) => value.to_string(),
        }),
    })
}

/// Convert an LSP `SymbolKind` number to a short human-readable string.
fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum_member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type_param",
        _ => "symbol",
    }
}

/// Convert a flat list of `DocumentSymbol` (from `textDocument/documentSymbol`)
/// to `SymbolItem`s, resolving locations against `document_uri`.
pub(crate) fn symbol_items_from_document_symbols(
    document_uri: &Uri,
    symbols: Vec<DocumentSymbol>,
    file_path: &str,
) -> Vec<SymbolItem> {
    let _ = document_uri;
    let mut result = Vec::new();
    flatten_document_symbols(symbols, file_path, &mut result);
    result
}

fn flatten_document_symbols(symbols: Vec<DocumentSymbol>, file_path: &str, out: &mut Vec<SymbolItem>) {
    for sym in symbols {
        out.push(SymbolItem {
            name: sym.name,
            kind: symbol_kind_name(sym.kind).to_owned(),
            path: file_path.to_owned(),
            line: sym.range.start.line as usize,
            column: sym.range.start.character as usize,
        });
        if let Some(children) = sym.children {
            flatten_document_symbols(children, file_path, out);
        }
    }
}

/// Convert `SymbolInformation` items (from `workspace/symbol`) to `SymbolItem`s.
pub(crate) fn symbol_items_from_workspace_symbols(
    symbols: Vec<SymbolInformation>,
) -> Vec<SymbolItem> {
    symbols
        .into_iter()
        .filter_map(|sym| {
            let path = Url::parse(sym.location.uri.as_str())
                .ok()?
                .to_file_path()
                .ok()?;
            Some(SymbolItem {
                name: sym.name,
                kind: symbol_kind_name(sym.kind).to_owned(),
                path: path.to_string_lossy().to_string(),
                line: sym.location.range.start.line as usize,
                column: sym.location.range.start.character as usize,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_targets_convert_utf16_columns_to_utf8_columns() {
        let uri: Uri = "file:///tmp/example.rs".parse().expect("uri should parse");
        let text = "let 😀value = 1;\n";
        let response = GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: Range::new(Position::new(0, 6), Position::new(0, 11)),
        });

        let targets = navigation_targets_from_definition_response(&uri, text, response)
            .expect("definition response should convert");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, "/tmp/example.rs");
        assert_eq!(targets[0].line, 0);
        assert_eq!(targets[0].column, 8);
    }

    #[test]
    fn completion_response_flattens_completion_list() {
        let items = completion_suggestions_from_items(&completion_items_from_response(
            CompletionResponse::List(CompletionList {
                is_incomplete: false,
                items: vec![CompletionItem {
                    label: String::from("println!"),
                    detail: Some(String::from("macro")),
                    insert_text: Some(String::from("println!($0)")),
                    ..CompletionItem::default()
                }],
            }),
        ));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "println!");
        assert_eq!(items[0].detail.as_deref(), Some("macro"));
        assert_eq!(items[0].insert_text.as_deref(), Some("println!($0)"));
    }

    #[test]
    fn code_actions_extract_document_edits_for_current_uri() {
        let uri: Uri = "file:///tmp/example.rs".parse().expect("uri should parse");

        let actions = code_actions_from_response(
            vec![CodeActionOrCommand::CodeAction(CodeAction {
                title: String::from("Fix let"),
                edit: Some(WorkspaceEdit {
                    changes: None,
                    document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                        text_document: OptionalVersionedTextDocumentIdentifier {
                            uri: uri.clone(),
                            version: None,
                        },
                        edits: vec![OneOf::Left(TextEdit {
                            range: Range::new(Position::new(0, 0), Position::new(0, 3)),
                            new_text: String::from("let"),
                        })],
                    }])),
                    change_annotations: None,
                }),
                ..CodeAction::default()
            })],
            &uri,
        )
        .expect("code actions should convert");

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].title, "Fix let");
        assert_eq!(actions[0].edits.len(), 1);
        assert_eq!(actions[0].edits[0].new_text, "let");
    }
}
