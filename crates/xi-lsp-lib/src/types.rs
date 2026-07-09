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

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Error as IOError;

use jsonrpc_lite::Error as JsonRpcError;
use lsp_types::{Command, CompletionItem, TextEdit, WorkspaceEdit};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xi_core_lib::plugin_rpc::{CompletionSuggestion, NavigationTarget, SymbolItem};
use xi_plugin_lib::Diagnostic as CoreDiagnostic;
use xi_plugin_lib::Error as PluginLibError;
use xi_rpc::RemoteErrorDetails;

use crate::language_server_client::LanguageServerClient;
use lsp_types::*;

pub trait Callable: Send {
    fn call(
        self: Box<Self>,
        client: &mut LanguageServerClient,
        result: Result<Value, JsonRpcError>,
    );
}

impl<F: Send + FnOnce(&mut LanguageServerClient, Result<Value, JsonRpcError>)> Callable for F {
    fn call(self: Box<F>, client: &mut LanguageServerClient, result: Result<Value, JsonRpcError>) {
        (*self)(client, result)
    }
}

pub type Callback = Box<dyn Callable>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Language Specific Configuration
pub struct LanguageConfig {
    pub language_name: String,
    pub start_command: String,
    pub start_arguments: Vec<String>,
    pub extensions: Vec<String>,
    #[serde(default)]
    pub filenames: Vec<String>,
    pub supports_single_file: bool,
    pub workspace_identifier: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub initialization_options: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisabledLanguageConfig {
    pub extensions: Vec<String>,
    #[serde(default)]
    pub filenames: Vec<String>,
}

/// Represents the config for the Language Plugin
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub language_config: HashMap<String, LanguageConfig>,
    #[serde(default)]
    pub disabled_language_config: HashMap<String, DisabledLanguageConfig>,
    #[serde(default)]
    pub language_servers: HashMap<String, Vec<String>>,
}

struct BundledRouting<'a> {
    extensions: &'a [&'a str],
    filenames: &'a [&'a str],
    supports_single_file: bool,
    workspace_identifier: Option<&'a str>,
}

fn bundled_language(
    id: &str,
    language_name: &str,
    start_command: &str,
    start_arguments: &[&str],
    routing: BundledRouting<'_>,
) -> (String, LanguageConfig) {
    (
        id.to_owned(),
        LanguageConfig {
            language_name: language_name.to_owned(),
            start_command: start_command.to_owned(),
            start_arguments: start_arguments.iter().map(|arg| (*arg).to_owned()).collect(),
            extensions: routing.extensions.iter().map(|ext| (*ext).to_owned()).collect(),
            filenames: routing.filenames.iter().map(|filename| (*filename).to_owned()).collect(),
            supports_single_file: routing.supports_single_file,
            workspace_identifier: routing.workspace_identifier.map(str::to_owned),
            env: BTreeMap::new(),
            initialization_options: None,
        },
    )
}

fn bundled_language_server(id: &str) -> (String, Vec<String>) {
    (id.to_owned(), vec![id.to_owned()])
}

impl Config {
    pub fn bundled() -> Self {
        Self {
            language_config: HashMap::from([
                bundled_language(
                    "bash",
                    "Bash",
                    "bash-language-server",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["sh", "bash"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "c",
                    "C",
                    "clangd",
                    &[],
                    BundledRouting {
                        extensions: &["c"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "cpp",
                    "C++",
                    "clangd",
                    &[],
                    BundledRouting {
                        extensions: &["cc", "cpp", "cxx", "hh", "hpp", "hxx"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "cmake",
                    "CMake",
                    "cmake-language-server",
                    &[],
                    BundledRouting {
                        extensions: &["cmake"],
                        filenames: &["CMakeLists.txt"],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "css",
                    "CSS",
                    "vscode-css-language-server",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["css", "scss"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "dockerfile",
                    "Dockerfile",
                    "docker-langserver",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &[],
                        filenames: &["Dockerfile", "Containerfile"],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "elixir",
                    "Elixir",
                    "elixir-ls",
                    &[],
                    BundledRouting {
                        extensions: &["ex", "exs", "heex"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("mix.exs"),
                    },
                ),
                bundled_language(
                    "gleam",
                    "Gleam",
                    "gleam",
                    &["lsp"],
                    BundledRouting {
                        extensions: &["gleam"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("gleam.toml"),
                    },
                ),
                bundled_language(
                    "go",
                    "Go",
                    "gopls",
                    &[],
                    BundledRouting {
                        extensions: &["go"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("go.mod"),
                    },
                ),
                bundled_language(
                    "html",
                    "HTML",
                    "vscode-html-language-server",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["html", "htm"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "javascript",
                    "JavaScript",
                    "typescript-language-server",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["js", "jsx", "mjs", "cjs"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("package.json"),
                    },
                ),
                bundled_language(
                    "json",
                    "Json",
                    "vscode-json-languageserver",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["json", "jsonc"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "java",
                    "Java",
                    "jdtls",
                    &[],
                    BundledRouting {
                        extensions: &["java"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "just",
                    "Just",
                    "just-lsp",
                    &[],
                    BundledRouting {
                        extensions: &[],
                        filenames: &["justfile", "Justfile"],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "kotlin",
                    "Kotlin",
                    "kotlin-language-server",
                    &[],
                    BundledRouting {
                        extensions: &["kt", "kts"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "lua",
                    "Lua",
                    "lua-language-server",
                    &[],
                    BundledRouting {
                        extensions: &["lua"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "markdown",
                    "Markdown",
                    "marksman",
                    &[],
                    BundledRouting {
                        extensions: &["md", "markdown"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "nix",
                    "Nix",
                    "nil",
                    &[],
                    BundledRouting {
                        extensions: &["nix"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "ocaml",
                    "OCaml",
                    "ocamllsp",
                    &[],
                    BundledRouting {
                        extensions: &["ml", "mli"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "php",
                    "PHP",
                    "intelephense",
                    &[],
                    BundledRouting {
                        extensions: &["php"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "python",
                    "Python",
                    "jedi-language-server",
                    &[],
                    BundledRouting {
                        extensions: &["py", "pyi"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "ruby",
                    "Ruby",
                    "ruby-lsp",
                    &[],
                    BundledRouting {
                        extensions: &["rb"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "scala",
                    "Scala",
                    "metals",
                    &[],
                    BundledRouting {
                        extensions: &["scala"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "rust",
                    "rust",
                    "rust-analyzer",
                    &[],
                    BundledRouting {
                        extensions: &["rs"],
                        filenames: &[],
                        supports_single_file: false,
                        workspace_identifier: Some("Cargo.toml"),
                    },
                ),
                bundled_language(
                    "toml",
                    "TOML",
                    "taplo",
                    &[],
                    BundledRouting {
                        extensions: &["toml"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "typescript",
                    "TypeScript",
                    "typescript-language-server",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["ts", "tsx", "mts", "cts"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("package.json"),
                    },
                ),
                bundled_language(
                    "svelte",
                    "Svelte",
                    "svelteserver",
                    &[],
                    BundledRouting {
                        extensions: &["svelte"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("package.json"),
                    },
                ),
                bundled_language(
                    "swift",
                    "Swift",
                    "sourcekit-lsp",
                    &[],
                    BundledRouting {
                        extensions: &["swift"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "vue",
                    "Vue",
                    "vue-language-server",
                    &[],
                    BundledRouting {
                        extensions: &["vue"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: Some("package.json"),
                    },
                ),
                bundled_language(
                    "yaml",
                    "Yaml",
                    "yaml-language-server",
                    &["--stdio"],
                    BundledRouting {
                        extensions: &["yaml", "yml"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
                bundled_language(
                    "zig",
                    "Zig",
                    "zls",
                    &[],
                    BundledRouting {
                        extensions: &["zig"],
                        filenames: &[],
                        supports_single_file: true,
                        workspace_identifier: None,
                    },
                ),
            ]),
            disabled_language_config: HashMap::new(),
            language_servers: HashMap::from([
                bundled_language_server("bash"),
                bundled_language_server("c"),
                bundled_language_server("cpp"),
                bundled_language_server("cmake"),
                bundled_language_server("css"),
                bundled_language_server("dockerfile"),
                bundled_language_server("elixir"),
                bundled_language_server("gleam"),
                bundled_language_server("go"),
                bundled_language_server("html"),
                bundled_language_server("java"),
                bundled_language_server("javascript"),
                bundled_language_server("just"),
                bundled_language_server("json"),
                bundled_language_server("kotlin"),
                bundled_language_server("lua"),
                bundled_language_server("markdown"),
                bundled_language_server("nix"),
                bundled_language_server("ocaml"),
                bundled_language_server("php"),
                bundled_language_server("python"),
                bundled_language_server("ruby"),
                bundled_language_server("rust"),
                bundled_language_server("scala"),
                bundled_language_server("svelte"),
                bundled_language_server("swift"),
                bundled_language_server("toml"),
                bundled_language_server("typescript"),
                bundled_language_server("vue"),
                bundled_language_server("yaml"),
                bundled_language_server("zig"),
            ]),
        }
    }
}

// TODO: Improve Error handling in module and add more types as necessary

/// Types to represent errors in the module.
#[derive(Debug)]
pub enum Error {
    PathError,
    FileUrlParseError,
    IOError(IOError),
    ServerStart { context: &'static str, message: String },
    Protocol(String),
    Serialization(String),
    LockPoisoned(&'static str),
}

impl From<IOError> for Error {
    fn from(err: IOError) -> Error {
        Error::IOError(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Serialization(err.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::PathError => write!(f, "path error"),
            Error::FileUrlParseError => write!(f, "file url parse error"),
            Error::IOError(err) => write!(f, "io error: {err}"),
            Error::ServerStart { context, message } => {
                write!(f, "server start failed during {context}: {message}")
            }
            Error::Protocol(message) => write!(f, "protocol error: {message}"),
            Error::Serialization(message) => write!(f, "serialization error: {message}"),
            Error::LockPoisoned(context) => write!(f, "lock poisoned: {context}"),
        }
    }
}

/// Possible Errors that can occur while handling Language Plugins
#[derive(Debug)]
pub enum LanguageResponseError {
    LanguageServerError(String),
    PluginLibError(PluginLibError),
    NullResponse,
    FallbackResponse,
    Transport(String),
}

impl From<PluginLibError> for LanguageResponseError {
    fn from(error: PluginLibError) -> Self {
        LanguageResponseError::PluginLibError(error)
    }
}

impl fmt::Display for LanguageResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanguageResponseError::NullResponse => write!(f, "null response from server"),
            LanguageResponseError::FallbackResponse => write!(f, "fallback response from server"),
            LanguageResponseError::LanguageServerError(_) => {
                write!(f, "language server error occured")
            }
            LanguageResponseError::PluginLibError(_) => write!(f, "Plugin Lib Error"),
            LanguageResponseError::Transport(_) => write!(f, "language server transport error"),
        }
    }
}

impl RemoteErrorDetails for LanguageResponseError {
    fn remote_error_code(&self) -> i64 {
        match self {
            LanguageResponseError::NullResponse => 0,
            LanguageResponseError::FallbackResponse => 1,
            LanguageResponseError::LanguageServerError(_) => 2,
            LanguageResponseError::PluginLibError(_) => 3,
            LanguageResponseError::Transport(_) => 4,
        }
    }

    fn remote_error_data(&self) -> Option<Value> {
        match self {
            LanguageResponseError::NullResponse | LanguageResponseError::FallbackResponse => None,
            LanguageResponseError::LanguageServerError(error)
            | LanguageResponseError::Transport(error) => Some(Value::String(error.clone())),
            LanguageResponseError::PluginLibError(error) => {
                Some(Value::String(format!("{:?}", error)))
            }
        }
    }
}

impl From<Error> for LanguageResponseError {
    fn from(error: Error) -> Self {
        LanguageResponseError::Transport(error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct LspCodeAction {
    pub title: String,
    pub edits: Vec<TextEdit>,
    pub command: Option<Command>,
}

#[derive(Debug, Clone)]
pub struct PendingCompletionItem {
    pub suggestion: CompletionSuggestion,
    pub item: CompletionItem,
}

#[derive(Debug)]
pub enum LspResponse {
    Hover(Result<Hover, LanguageResponseError>),
    Diagnostics(Result<Vec<CoreDiagnostic>, LanguageResponseError>),
    Completions(Result<Vec<PendingCompletionItem>, LanguageResponseError>),
    Locations { title: String, result: Result<Vec<NavigationTarget>, LanguageResponseError> },
    Symbols { title: String, result: Result<Vec<SymbolItem>, LanguageResponseError> },
    Formatting { title: String, result: Result<Vec<TextEdit>, LanguageResponseError> },
    CodeActions(Result<Vec<LspCodeAction>, LanguageResponseError>),
    Rename { title: String, result: Result<Option<WorkspaceEdit>, LanguageResponseError> },
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use xi_rpc::RemoteError;

    use super::{Config, LanguageResponseError};

    #[test]
    fn language_response_error_converts_into_remote_error() {
        let err: RemoteError = LanguageResponseError::Transport("connection reset".into()).into();

        assert_eq!(
            err,
            RemoteError::custom(
                4,
                "language server transport error",
                Some(Value::String("connection reset".into())),
            )
        );
    }

    #[test]
    fn bundled_config_preserves_current_defaults() {
        let config = Config::bundled();

        assert_eq!(
            config.language_config.keys().cloned().collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                String::from("bash"),
                String::from("c"),
                String::from("cpp"),
                String::from("cmake"),
                String::from("css"),
                String::from("dockerfile"),
                String::from("elixir"),
                String::from("gleam"),
                String::from("go"),
                String::from("html"),
                String::from("java"),
                String::from("javascript"),
                String::from("just"),
                String::from("json"),
                String::from("kotlin"),
                String::from("lua"),
                String::from("markdown"),
                String::from("nix"),
                String::from("ocaml"),
                String::from("php"),
                String::from("python"),
                String::from("ruby"),
                String::from("rust"),
                String::from("scala"),
                String::from("svelte"),
                String::from("swift"),
                String::from("toml"),
                String::from("typescript"),
                String::from("vue"),
                String::from("yaml"),
                String::from("zig"),
            ])
        );
        assert_eq!(config.language_config.len(), config.language_servers.len());

        let rust = config.language_config.get("rust").unwrap();
        assert_eq!(rust.language_name, "rust");
        assert_eq!(rust.start_command, "rust-analyzer");
        assert!(rust.start_arguments.is_empty());
        assert_eq!(rust.extensions, vec!["rs"]);
        assert!(!rust.supports_single_file);
        assert_eq!(rust.workspace_identifier.as_deref(), Some("Cargo.toml"));
        assert!(rust.env.is_empty());
        assert_eq!(rust.initialization_options, None);

        let javascript = config.language_config.get("javascript").unwrap();
        assert_eq!(javascript.language_name, "JavaScript");
        assert_eq!(javascript.start_command, "typescript-language-server");
        assert_eq!(javascript.start_arguments, vec!["--stdio"]);
        assert_eq!(javascript.extensions, vec!["js", "jsx", "mjs", "cjs"]);
        assert!(javascript.supports_single_file);
        assert_eq!(javascript.workspace_identifier.as_deref(), Some("package.json"));

        let cmake = config.language_config.get("cmake").unwrap();
        assert_eq!(cmake.language_name, "CMake");
        assert_eq!(cmake.start_command, "cmake-language-server");
        assert!(cmake.start_arguments.is_empty());
        assert_eq!(cmake.extensions, vec!["cmake"]);
        assert_eq!(cmake.filenames, vec!["CMakeLists.txt"]);
        assert!(cmake.supports_single_file);

        let dockerfile = config.language_config.get("dockerfile").unwrap();
        assert_eq!(dockerfile.language_name, "Dockerfile");
        assert_eq!(dockerfile.start_command, "docker-langserver");
        assert_eq!(dockerfile.start_arguments, vec!["--stdio"]);
        assert!(dockerfile.extensions.is_empty());
        assert_eq!(dockerfile.filenames, vec!["Dockerfile", "Containerfile"]);
        assert!(dockerfile.supports_single_file);

        let gleam = config.language_config.get("gleam").unwrap();
        assert_eq!(gleam.language_name, "Gleam");
        assert_eq!(gleam.start_command, "gleam");
        assert_eq!(gleam.start_arguments, vec!["lsp"]);
        assert_eq!(gleam.extensions, vec!["gleam"]);
        assert!(gleam.filenames.is_empty());
        assert!(gleam.supports_single_file);
        assert_eq!(gleam.workspace_identifier.as_deref(), Some("gleam.toml"));

        let json = config.language_config.get("json").unwrap();
        assert_eq!(json.language_name, "Json");
        assert_eq!(json.start_command, "vscode-json-languageserver");
        assert_eq!(json.start_arguments, vec!["--stdio"]);
        assert_eq!(json.extensions, vec!["json", "jsonc"]);
        assert!(json.supports_single_file);
        assert_eq!(json.workspace_identifier, None);
        assert!(json.env.is_empty());
        assert_eq!(json.initialization_options, None);

        let python = config.language_config.get("python").unwrap();
        assert_eq!(python.language_name, "Python");
        assert_eq!(python.start_command, "jedi-language-server");
        assert!(python.start_arguments.is_empty());
        assert_eq!(python.extensions, vec!["py", "pyi"]);
        assert!(python.supports_single_file);
        assert_eq!(python.workspace_identifier, None);

        let typescript = config.language_config.get("typescript").unwrap();
        assert_eq!(typescript.language_name, "TypeScript");
        assert_eq!(typescript.start_command, "typescript-language-server");
        assert_eq!(typescript.start_arguments, vec!["--stdio"]);
        assert_eq!(typescript.extensions, vec!["ts", "tsx", "mts", "cts"]);
        assert!(typescript.supports_single_file);
        assert_eq!(typescript.workspace_identifier.as_deref(), Some("package.json"));
        assert!(typescript.env.is_empty());
        assert_eq!(typescript.initialization_options, None);

        let vue = config.language_config.get("vue").unwrap();
        assert_eq!(vue.language_name, "Vue");
        assert_eq!(vue.start_command, "vue-language-server");
        assert!(vue.start_arguments.is_empty());
        assert_eq!(vue.extensions, vec!["vue"]);
        assert!(vue.filenames.is_empty());
        assert!(vue.supports_single_file);
        assert_eq!(vue.workspace_identifier.as_deref(), Some("package.json"));

        let just = config.language_config.get("just").unwrap();
        assert_eq!(just.language_name, "Just");
        assert_eq!(just.start_command, "just-lsp");
        assert!(just.start_arguments.is_empty());
        assert!(just.extensions.is_empty());
        assert_eq!(just.filenames, vec!["justfile", "Justfile"]);
        assert!(just.supports_single_file);

        let yaml = config.language_config.get("yaml").unwrap();
        assert_eq!(yaml.language_name, "Yaml");
        assert_eq!(yaml.start_command, "yaml-language-server");
        assert_eq!(yaml.start_arguments, vec!["--stdio"]);
        assert_eq!(yaml.extensions, vec!["yaml", "yml"]);
        assert!(yaml.supports_single_file);
        assert_eq!(yaml.workspace_identifier, None);
        assert!(yaml.env.is_empty());
        assert_eq!(yaml.initialization_options, None);

        let zig = config.language_config.get("zig").unwrap();
        assert_eq!(zig.language_name, "Zig");
        assert_eq!(zig.start_command, "zls");
        assert!(zig.start_arguments.is_empty());
        assert_eq!(zig.extensions, vec!["zig"]);
        assert!(zig.supports_single_file);
        assert_eq!(zig.workspace_identifier, None);
    }
}
