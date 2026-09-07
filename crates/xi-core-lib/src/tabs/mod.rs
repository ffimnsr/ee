// Copyright 2016 The xi-editor Authors.
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

//! The main container for core state.
//!
//! All events from the frontend or from plugins are handled here first.
//!
//! This file is called 'tabs' for historical reasons, and should probably
//! be renamed.
//!
//! Ownership boundary: this module owns save command routing, kickoff,
//! alerts, and post-save UI/config updates.

pub(crate) use std::cell::{Cell, RefCell};
pub(crate) use std::collections::{BTreeMap, HashMap, HashSet};
pub(crate) use std::fmt;
pub(crate) use std::fs::OpenOptions;
pub(crate) use std::io::ErrorKind;
pub(crate) use std::mem;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::Arc;
pub(crate) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) use log::{debug, error, info, warn};
pub(crate) use serde::de::{self, Deserializer, Unexpected};
pub(crate) use serde::ser::Serializer;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::{Value, json};

pub(crate) use xi_rope::Rope;
pub(crate) use xi_rpc::{self, OptionExt, ReadError, RemoteError, RpcCtx, RpcPeer};

pub(crate) use crate::WeakXiCore;
pub(crate) use crate::client::Client;
pub(crate) use crate::config::{ConfigDomain, ConfigDomainExternal, ConfigManager, Table};
pub(crate) use crate::editor::Editor;
pub(crate) use crate::event_context::EventContext;
pub(crate) use crate::file::{FileManager, OpenResult, SampledIndentation, SampledLineEnding};
pub(crate) use crate::line_ending::LineEnding;
pub(crate) use crate::plugin_rpc::{PluginNotification, PluginRequest};
pub(crate) use crate::plugins::rpc::ClientPluginInfo;
pub(crate) use crate::plugins::rpc::SelectionRange;
pub(crate) use crate::plugins::{
    Plugin, PluginCatalog, PluginDescription, PluginPid, PluginTerminationReason,
    start_plugin_process,
};
pub(crate) use crate::rpc::{
    CoreNotification, CoreRequest, EditNotification, PluginNotification as CorePluginNotification,
};
pub(crate) use crate::runtime_loader::{
    merged_runtime_languages, reload_default_runtime_loader_languages,
};
pub(crate) use crate::syntax::LanguageId;
pub(crate) use crate::text_store::{DocumentMode, EditPermission, TextStore};
pub(crate) use crate::view::View;
pub(crate) use crate::whitespace::Indentation;
pub(crate) use crate::width_cache::WidthCache;

#[cfg(feature = "notify")]
pub(crate) use crate::watcher::{FileWatcher, WatchToken};
#[cfg(feature = "notify")]
pub(crate) use notify::Event;

mod plugin_lifecycle;

/// ViewIds are the primary means of routing messages between
/// xi-core and a client view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewId(pub(crate) usize);

/// BufferIds uniquely identify open buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct BufferId(pub(crate) usize);

pub type PluginId = crate::plugins::PluginPid;

fn save_complete_alert(path: &Path) -> String {
    format!("save complete: {}", path.display())
}

fn save_cancelled_alert(path: &Path) -> String {
    format!("save cancelled: {}", path.display())
}

fn save_failed_alert(error: &crate::file::FileError) -> String {
    format!("save failed: {}", error)
}

fn save_error_details(error: &crate::file::FileError, path: &Path) -> (String, bool) {
    match error {
        crate::file::FileError::Io(io_error, _) if io_error.kind() == ErrorKind::Interrupted => {
            (save_cancelled_alert(path), false)
        }
        crate::file::FileError::Io(io_error, _)
            if io_error.kind() == ErrorKind::PermissionDenied =>
        {
            (save_failed_alert(error), true)
        }
        _ => (save_failed_alert(error), false),
    }
}

fn save_error_alert(error: &crate::file::FileError, path: &Path) -> String {
    save_error_details(error, path).0
}

fn elevated_save_temp_path(target_path: &Path) -> Result<PathBuf, std::io::Error> {
    let stem = target_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("buffer");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let candidate = std::env::temp_dir().join(format!("ee-elevated-save-{stem}-{now}.tmp"));
    OpenOptions::new().create_new(true).write(true).open(&candidate)?;
    Ok(candidate)
}

// old-style names; will be deprecated
pub type BufferIdentifier = BufferId;

/// Totally arbitrary; we reserve this space for `ViewId`s
pub(crate) const RENDER_VIEW_IDLE_MASK: usize = 1 << 25;
pub(crate) const REWRAP_VIEW_IDLE_MASK: usize = 1 << 26;
pub(crate) const FIND_VIEW_IDLE_MASK: usize = 1 << 27;
/// Idle token mask for delivering async whole-document scan results.
pub(crate) const WHOLE_SCAN_IDLE_MASK: usize = 1 << 28;
/// Idle token mask for delivering async rope-save results.
pub(crate) const SAVE_VIEW_IDLE_MASK: usize = 1 << 29;

const NEW_VIEW_IDLE_TOKEN: usize = 1001;
const VERIFY_LINE_ENDINGS_IDLE_TOKEN: usize = 1003;

/// xi_rpc idle Token for watcher related idle scheduling.
pub(crate) const WATCH_IDLE_TOKEN: usize = 1002;

const PLUGIN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PLUGIN_RESTART_BASE_DELAY_MS: u64 = 250;
const PLUGIN_RESTART_MAX_DELAY_MS: u64 = 5_000;
const PLUGIN_STABLE_UPTIME: Duration = Duration::from_secs(30);

/// Token for file-change events in open files
#[cfg(feature = "notify")]
pub const OPEN_FILE_EVENT_TOKEN: WatchToken = WatchToken(1);

#[cfg(feature = "notify")]
const PLUGIN_EVENT_TOKEN: WatchToken = WatchToken(2);

#[allow(dead_code)]
pub struct CoreState {
    editors: BTreeMap<BufferId, RefCell<Editor>>,
    views: BTreeMap<ViewId, RefCell<View>>,
    file_manager: FileManager,
    /// A local pasteboard.
    kill_ring: RefCell<Rope>,
    width_cache: RefCell<WidthCache>,
    /// User and platform specific settings
    config_manager: ConfigManager,
    /// A weak reference to the main state container, stashed so that
    /// it can be passed to plugins.
    self_ref: Option<WeakXiCore>,
    /// Views which need to have setup finished.
    pending_views: Vec<(ViewId, Table)>,
    pending_line_ending_verifications: Vec<BufferId>,
    peer: Client,
    id_counter: Counter,
    plugins: PluginCatalog,
    launching_plugins: HashSet<String>,
    scheduled_plugin_restarts: HashSet<String>,
    stopping_plugins: HashMap<PluginId, StopReason>,
    plugin_restart_state: HashMap<String, PluginRestartState>,
    pending_plugin_commands: Vec<PendingPluginCommand>,
    running_plugins: Vec<Plugin>,
}

#[derive(Debug, Clone)]
struct PendingPluginCommand {
    plugin_name: String,
    view_id: ViewId,
    method: String,
    params: Value,
    shutdown_after_dispatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StopReason {
    Manual,
    Restart,
    SingleInvocation,
    ResourceLimit(PluginTerminationReason),
}

#[derive(Debug, Default, Clone)]
struct PluginRestartState {
    consecutive_failures: u32,
    last_start: Option<Instant>,
}

fn stderr_is_user_visible(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("panic")
        || lower.contains("panicked")
        || lower.contains("error")
        || lower.contains("failed")
}

/// test helpers
pub mod test_helpers {
    use super::{BufferId, ViewId};

    pub fn new_view_id(id: usize) -> ViewId {
        ViewId(id)
    }

    pub fn new_buffer_id(id: usize) -> BufferId {
        BufferId(id)
    }
}

/// A multi-view aware iterator over `EventContext`s. A view which appears
/// as a sibling will not appear again as a main view.
pub struct Iter<'a, I> {
    views: I,
    seen: HashSet<ViewId>,
    inner: &'a CoreState,
}

impl<'a, I> Iterator for Iter<'a, I>
where
    I: Iterator<Item = &'a ViewId>,
{
    type Item = EventContext<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let &mut Iter { ref mut views, ref mut seen, inner } = self;
        loop {
            let next_view = match views.next() {
                None => return None,
                Some(v) if seen.contains(v) => continue,
                Some(v) => v,
            };
            let context = inner.make_context(*next_view).unwrap();
            context.siblings.iter().for_each(|sibl| {
                let _ = seen.insert(sibl.borrow().get_view_id());
            });
            return Some(context);
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Counter(Cell<usize>);

impl Counter {
    pub(crate) fn next(&self) -> usize {
        let n = self.0.get();
        self.0.set(n + 1);
        n + 1
    }
}

// these two only exist so that we can use ViewIds as idle tokens
impl From<usize> for ViewId {
    fn from(src: usize) -> ViewId {
        ViewId(src)
    }
}

impl From<ViewId> for usize {
    fn from(src: ViewId) -> usize {
        src.0
    }
}

impl fmt::Display for ViewId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "view-id-{}", self.0)
    }
}

impl Serialize for ViewId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ViewId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.trim_start_matches("view-id-")
            .parse::<usize>()
            .map(ViewId)
            .map_err(|_| de::Error::invalid_value(Unexpected::Str(&s), &"view id"))
    }
}

impl fmt::Display for BufferId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "buffer-id-{}", self.0)
    }
}

impl BufferId {
    pub fn new(val: usize) -> Self {
        BufferId(val)
    }
}

mod construction;
mod contexts;
mod idle;
mod plugin_events;
mod plugins;
mod test_access;

#[cfg(test)]
mod tests;
