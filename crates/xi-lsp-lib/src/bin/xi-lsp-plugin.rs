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
use std::path::PathBuf;

use xi_core_lib::user_state_dir;
use xi_lsp_lib::{Config, LspPlugin, start_mainloop};

/// Resolve the plugin log file path.
///
/// Centralize logs in the user state dir (`<state base>/ee/xi-lsp-plugin.log`)
/// so opening the editor on a project does not drop a log file into the
/// workspace. `EE_PLUGIN_LOG` overrides the location, matching the `ee`
/// editor's own log overrides. Falls back to the current directory when no
/// user state dir is available.
fn plugin_log_path() -> PathBuf {
    plugin_log_path_with(
        std::env::var_os("EE_PLUGIN_LOG").filter(|value| !value.is_empty()).map(PathBuf::from),
        user_state_dir(),
    )
}

fn plugin_log_path_with(log_override: Option<PathBuf>, state_dir: Option<PathBuf>) -> PathBuf {
    if let Some(path) = log_override {
        return path;
    }
    if let Some(state_dir) = state_dir {
        return state_dir.join("xi-lsp-plugin.log");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("xi-lsp-plugin.log")
}

fn init_logger() -> Result<(), fern::InitError> {
    let level_filter = match std::env::var("XI_LOG") {
        Ok(level) => match level.to_lowercase().as_ref() {
            "trace" => log::LevelFilter::Trace,
            "debug" => log::LevelFilter::Debug,
            _ => log::LevelFilter::Info,
        },
        // Default to info
        Err(_) => log::LevelFilter::Info,
    };

    let log_path = plugin_log_path();
    // Create the parent directory unless the override is a bare file name
    // (e.g. `EE_PLUGIN_LOG=foo.log`), whose parent is empty.
    if let Some(parent) = log_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(level_filter)
        .chain(fern::log_file(log_path)?)
        .apply()
        .map_err(|e| e.into())
}

fn main() {
    // The specified language server must be in PATH. XCode does not use
    // the PATH variable of your shell. See the answers below to modify PATH to
    // have language servers in PATH while running from XCode.
    // https://stackoverflow.com/a/17394454 and https://stackoverflow.com/a/43043687
    if let Err(err) = init_logger() {
        eprintln!("Failed to start logger for LSP Plugin: {err}");
        std::process::exit(1);
    }
    let config: Config = Config::bundled();
    let mut plugin = LspPlugin::new(config);

    if let Err(err) = start_mainloop(&mut plugin) {
        eprintln!("LSP plugin mainloop failed: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::plugin_log_path_with;

    #[test]
    fn plugin_log_path_honors_env_override() {
        let custom = PathBuf::from("/tmp/custom-plugin.log");
        let state_dir = PathBuf::from("/tmp/state");
        assert_eq!(plugin_log_path_with(Some(custom.clone()), Some(state_dir)), custom);
    }

    #[test]
    fn plugin_log_path_uses_state_dir_when_unset() {
        let state_dir = PathBuf::from("/tmp/state");
        assert_eq!(
            plugin_log_path_with(None, Some(state_dir.clone())),
            state_dir.join("xi-lsp-plugin.log")
        );
    }

    #[test]
    fn plugin_log_path_falls_back_to_current_dir_without_state_dir() {
        let path = plugin_log_path_with(None, None);
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(path, cwd.join("xi-lsp-plugin.log"));
    }
}
