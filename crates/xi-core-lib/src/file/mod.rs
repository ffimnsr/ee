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

//! File open/close/save machinery: open-policy application and buffer
//! metadata (`manager`), decode plus formatting analysis (`open`), save
//! execution (`save`), error mapping (`error`), and small filesystem helpers
//! (`support`).
//!
//! Split into submodules so every file stays below the 1K LOC threshold;
//! `lib.rs` keeps `pub mod file`, and the re-exports below preserve the
//! historical `crate::file::*` surface.

mod error;
mod manager;
mod open;
mod save;
mod support;

pub use error::FileError;
pub use manager::{FileInfo, FileManager};
pub use open::{
    CharacterEncoding, FileOpenAnalysis, OpenResult, SampledIndentation, SampledLineEnding,
};

pub(crate) use open::{UTF8_BOM, sampled_line_count_hint, try_load_file};

pub(crate) use save::{
    PreparedRopeSave, PreparedRopeSaveKind, PreparedVlfSave, PreparedVlfSaveKind, SaveOptions,
    execute_prepared_rope_save, execute_prepared_rope_save_with_progress,
    execute_prepared_vlf_save,
};

#[cfg(target_family = "unix")]
pub(crate) use support::{
    FileChangeCookie, change_cookie_from_metadata, get_change_cookie, get_permissions,
    permissions_from_metadata,
};
pub(crate) use support::{get_file_len, get_mod_time, mod_time_from_metadata, open_advisory_lock};

#[cfg(test)]
pub(crate) use support::with_large_alloc_tracking;
