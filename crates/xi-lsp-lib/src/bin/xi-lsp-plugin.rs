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
use xi_lsp_lib::{Config, LspPlugin, start_mainloop};

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
        .chain(std::io::stderr())
        .chain(fern::log_file("xi-lsp-plugin.log")?)
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
