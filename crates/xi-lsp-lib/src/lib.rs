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
use xi_plugin_lib::Plugin;
use xi_plugin_lib::mainloop;

pub mod conversion_utils;
pub mod language_server_client;
pub mod lsp_plugin;
mod result_queue;
pub mod types;
mod utils;
pub use crate::lsp_plugin::LspPlugin;
pub use crate::result_queue::ResultQueue;
pub use crate::types::Config;
pub use crate::utils::{read_transport_message, shutdown_language_server, start_new_server};

pub fn start_mainloop<P: Plugin>(plugin: &mut P) -> Result<(), xi_rpc::ReadError> {
    mainloop(plugin)
}
