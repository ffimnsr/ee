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

//! A container for the state relevant to a single event.

mod dispatch;
mod editing;
mod helpers;
mod init;
mod movement;
mod plugins;
mod vlf;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::time::Duration;

use serde_json::Value;

use xi_rope::Rope;

use crate::WeakXiCore;
use crate::client::Client;
use crate::config::{BufferItems, Table};
use crate::editor::{EditType, Editor};
use crate::file::FileInfo;
use crate::plugins::Plugin;
use crate::syntax::LanguageId;
use crate::tabs::{BufferId, ViewId};
use crate::tree_sitter_support::VisibleSyntaxSpan;
use crate::view::View;
use crate::width_cache::WidthCache;

// ── Re-exports for sibling submodules ──

// Re-export free functions from helpers so `super::x` works from any submodule.
// Re-export free functions from helpers and vlf so sibling submodules
// can access them via `super::name`.
pub(crate) use helpers::*;
pub(crate) use vlf::*;

// Note: methods on EventContext (dispatch_command_to_plugins, render, etc.)
// are NOT re-exported here because methods can't be module-level items.
// They are accessible via `self.method()` from any submodule as long as
// their visibility is at least `pub(super)`.

// Maximum returned result from plugin get_data RPC.
pub const MAX_SIZE_LIMIT: usize = 1024 * 1024;

//TODO: tune this. a few ms can make a big difference. We may in the future
//want to make this tuneable at runtime, or to be configured by the client.
/// The render delay after an edit occurs; plugin updates received in this
/// window will be sent to the view along with the edit.
const RENDER_DELAY: Duration = Duration::from_millis(2);
const VLF_TAIL_EXACT_LINE_COUNT_MAX_BYTES: u64 = 32 * 1024 * 1024;
const VLF_PREFIX_PENDING_INDEX_FALLBACK_MAX_BYTES: u64 = 32 * 1024 * 1024;

struct VlfViewportResponse {
    line_start: u64,
    lines: Vec<String>,
    syntax_spans: Vec<Vec<VisibleSyntaxSpan>>,
    approximate_line_count: u64,
    line_count_exact: bool,
    index_progress: f64,
}

/// A collection of all the state relevant for handling a particular event.
///
/// This is created dynamically for each event that arrives to the core,
/// such as a user-initiated edit or style updates from a plugin.
pub struct EventContext<'a> {
    pub(crate) view_id: ViewId,
    pub(crate) buffer_id: BufferId,
    pub(crate) editor: &'a RefCell<Editor>,
    pub(crate) info: Option<&'a FileInfo>,
    pub(crate) config: &'a BufferItems,
    pub(crate) language: LanguageId,
    pub(crate) view: &'a RefCell<View>,
    pub(crate) siblings: Vec<&'a RefCell<View>>,
    pub(crate) plugins: Vec<&'a Plugin>,
    pub(crate) client: &'a Client,
    pub(crate) width_cache: &'a RefCell<WidthCache>,
    pub(crate) kill_ring: &'a RefCell<Rope>,
    pub(crate) weak_core: &'a WeakXiCore,
}

fn edit_type_to_string(edit_type: EditType) -> String {
    match edit_type {
        EditType::Other => "other",
        EditType::InsertChars => "insert",
        EditType::InsertNewline => "newline",
        EditType::Indent => "indent",
        EditType::Delete => "delete",
        EditType::Undo => "undo",
        EditType::Redo => "redo",
        EditType::Transpose => "transpose",
        EditType::Surround => "surround",
    }
    .to_string()
}

fn buffer_items_to_table(config: &BufferItems) -> Table {
    match serde_json::to_value(config) {
        Ok(Value::Object(table)) => table,
        Ok(other) => {
            log::error!("buffer config serialized to non-object value: {:?}", other);
            Table::new()
        }
        Err(err) => {
            log::error!("failed to serialize buffer config: {:?}", err);
            Table::new()
        }
    }
}
