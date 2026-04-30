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

//! The main library for xi-core.

#![allow(
    clippy::boxed_local,
    clippy::cast_lossless,
    clippy::collapsible_if,
    clippy::let_and_return,
    clippy::map_entry,
    clippy::match_as_ref,
    clippy::match_bool,
    clippy::needless_pass_by_value,
    clippy::new_without_default,
    clippy::or_fun_call,
    clippy::ptr_arg,
    clippy::too_many_arguments,
    clippy::unreadable_literal,
    clippy::get_unwrap
)]

pub mod annotations;
pub mod backspace;
pub mod client;
pub mod config;
pub mod core;
pub mod edit_ops;
pub mod edit_types;
pub mod editor;
pub mod event_context;
pub mod file;
pub mod find;
pub mod index_set;
pub mod layers;
pub mod line_cache_shadow;
pub mod line_ending;
pub mod line_offset;
pub mod linewrap;
pub mod movement;
pub mod plugins;
pub mod recorder;
pub mod selection;
pub mod styles;
pub mod syntax;
pub mod tabs;
pub mod view;
#[cfg(feature = "notify")]
pub mod watcher;
pub mod whitespace;
pub mod width_cache;
pub mod word_boundaries;

pub mod rpc;

pub use crate::config::{BufferItems as BufferConfig, Table as ConfigTable};
pub use crate::core::{WeakXiCore, XiCore};
pub use crate::editor::EditType;
pub use crate::plugins::manifest as plugin_manifest;
pub use crate::plugins::rpc as plugin_rpc;
pub use crate::plugins::PluginPid;
pub use crate::syntax::{LanguageDefinition, LanguageId};
pub use crate::tabs::test_helpers;
pub use crate::tabs::{BufferId, BufferIdentifier, ViewId};
