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

//! Implementation of Language Server Plugin

pub(crate) use std::collections::{BTreeMap, HashMap, HashSet};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::{Arc, Mutex, mpsc};
pub(crate) use std::time::{Duration, Instant};

pub(crate) use jsonrpc_lite::Params;
pub(crate) use log::{debug, error, trace};
pub(crate) use serde_json::{Map, Value, json};
pub(crate) use tokio_util::sync::CancellationToken;

pub(crate) use xi_plugin_lib::{ChunkCache, CoreProxy, Plugin, View};
pub(crate) use xi_rope::rope::RopeDelta;

pub(crate) use crate::conversion_utils::*;
pub(crate) use crate::document_symbols::DocumentSymbolContext;
pub(crate) use crate::language_server_client::{LanguageServerClient, OpenDocumentState};
pub(crate) use crate::result_queue::ResultQueue;
pub(crate) use crate::types::{
    Config, Error, LanguageResponseError, LspCodeAction, LspResponse, PendingCompletionItem,
};
pub(crate) use crate::utils::*;
pub(crate) use lsp_types::*;
pub(crate) use xi_core_lib::{ConfigTable, LanguageId, ViewId};

#[derive(Clone)]
struct ViewServerRoute {
    server_id: String,
    ls_identifier: String,
    workspace_root: Option<Uri>,
}

#[derive(Clone)]
pub struct ViewInfo {
    version: i32,
    language_id: String,
    routes: Vec<ViewServerRoute>,
    path: PathBuf,
}

struct ClientRestartGroup {
    server_id: String,
    workspace_root: Option<Uri>,
    documents: Vec<(ViewId, OpenDocumentState)>,
}

/// Represents the state of the Language Server Plugin
pub struct LspPlugin {
    pub config: Config,
    view_info: HashMap<ViewId, ViewInfo>,
    core: Option<CoreProxy>,
    result_queue: ResultQueue,
    pending_code_actions: HashMap<ViewId, Vec<LspCodeAction>>,
    pending_completions: HashMap<ViewId, Vec<PendingCompletionItem>>,
    language_server_clients: HashMap<String, Arc<Mutex<LanguageServerClient>>>,
    disabled_views: HashMap<ViewId, String>,
    inactive_views: HashMap<ViewId, String>,
    route_views: HashMap<ViewId, String>,
    /// Tree-sitter symbol fallbacks that returned empty on a cold backend;
    /// the request is retried on the idle loop instead of surfacing a
    /// misleading empty picker.
    pending_symbol_retries: HashMap<ViewId, u32>,
}

/// Bound on one-shot tree-sitter symbol retries after a cold-start empty
/// result (grammar/query loading is lazy per plugin process).
const MAX_TREE_SITTER_SYMBOL_RETRIES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LanguageMatch {
    Enabled(String),
    Disabled(String),
}
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentToolRange {
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentToolDocumentSymbol {
    name: String,
    kind: String,
    range: AgentToolRange,
    selection_range: AgentToolRange,
    container_path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentToolReference {
    path: String,
    range: AgentToolRange,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentToolCodeActionEdit {
    range: AgentToolRange,
    new_text: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentToolCodeAction {
    title: String,
    kind: Option<String>,
    has_command: bool,
    edits: Vec<AgentToolCodeActionEdit>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentToolPlannedFileEdit {
    path: String,
    edits: Vec<AgentToolCodeActionEdit>,
}

fn agent_tool_range(range: Range) -> AgentToolRange {
    AgentToolRange {
        start_line: range.start.line.saturating_add(1),
        start_character: range.start.character.saturating_add(1),
        end_line: range.end.line.saturating_add(1),
        end_character: range.end.character.saturating_add(1),
    }
}

fn agent_symbol_kind_name(kind: SymbolKind) -> String {
    format!("{kind:?}").to_ascii_lowercase()
}

fn uri_to_file_path(uri: &Uri) -> Result<String, LanguageResponseError> {
    url::Url::parse(uri.as_str())
        .map_err(|err| LanguageResponseError::Transport(format!("invalid URI {:?}: {err}", uri)))?
        .to_file_path()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|_| LanguageResponseError::Transport(format!("URI is not a file path: {:?}", uri)))
}

fn flatten_agent_document_symbols(
    symbols: Vec<lsp_types::DocumentSymbol>,
    container_path: &str,
    out: &mut Vec<AgentToolDocumentSymbol>,
) {
    for symbol in symbols {
        out.push(AgentToolDocumentSymbol {
            name: symbol.name,
            kind: agent_symbol_kind_name(symbol.kind),
            range: agent_tool_range(symbol.range),
            selection_range: agent_tool_range(symbol.selection_range),
            container_path: container_path.to_owned(),
        });
        if let Some(children) = symbol.children {
            flatten_agent_document_symbols(children, container_path, out);
        }
    }
}

fn agent_workspace_edit_files(
    edit: WorkspaceEdit,
) -> Result<Vec<AgentToolPlannedFileEdit>, LanguageResponseError> {
    let mut files: BTreeMap<String, Vec<AgentToolCodeActionEdit>> = BTreeMap::new();

    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            let path = uri_to_file_path(&uri)?;
            let mapped = edits
                .into_iter()
                .map(|edit| AgentToolCodeActionEdit {
                    range: agent_tool_range(edit.range),
                    new_text: edit.new_text,
                })
                .collect::<Vec<_>>();
            files.entry(path).or_default().extend(mapped);
        }
    }

    if let Some(document_changes) = edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(documents) => {
                for document in documents {
                    let path = uri_to_file_path(&document.text_document.uri)?;
                    let edits = document
                        .edits
                        .into_iter()
                        .map(|edit| match edit {
                            OneOf::Left(edit) => Ok(AgentToolCodeActionEdit {
                                range: agent_tool_range(edit.range),
                                new_text: edit.new_text,
                            }),
                            OneOf::Right(_) => Err(LanguageResponseError::Transport(String::from(
                                "annotated text edits are not supported",
                            ))),
                        })
                        .collect::<Result<Vec<_>, LanguageResponseError>>()?;
                    files.entry(path).or_default().extend(edits);
                }
            }
            DocumentChanges::Operations(_) => {
                return Err(LanguageResponseError::Transport(String::from(
                    "resource operations in workspace edits are not supported",
                )));
            }
        }
    }

    Ok(files.into_iter().map(|(path, edits)| AgentToolPlannedFileEdit { path, edits }).collect())
}
fn merge_json_object(target: &mut Map<String, Value>, update: &ConfigTable) {
    for (key, value) in update {
        match (target.get_mut(key), value) {
            (Some(Value::Object(target_map)), Value::Object(update_map)) => {
                merge_json_object(target_map, update_map);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

mod agent_requests;
mod config;
mod documents;
mod lsp_requests;
mod plugin_impl;

#[cfg(test)]
mod tests;
